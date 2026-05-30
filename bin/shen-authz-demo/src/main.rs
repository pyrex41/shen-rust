//! Prototype: **Shen + Cedar, natively in one Rust process.**
//!
//! Answers "does this work for an app that combines shen + cedar natively w/
//! rust?" — yes. This binary links two engines:
//!
//!   * `shen-cedar` — the Shen language port, run in its **served / VM mode**
//!     (`enable_vm()`, the entrypoint shipped for long-running sessions where
//!     the bytecode VM's per-body win amortizes). It is the *compute* engine.
//!   * `cedar-policy` — AWS Cedar, the authorization-policy language. It is the
//!     *authorization gate*: every request is checked against a policy set +
//!     an entity (role) hierarchy before the Shen engine ever runs.
//!
//! The combination is a **policy-gated Shen evaluator**: a request is
//! `(principal, action, resource, shen-source)`. Cedar decides whether the
//! principal may perform the action on the resource; only on `Allow` does the
//! served Shen VM evaluate the source. This is the natural shape for a
//! served/multi-tenant Shen-as-a-service — the `--served` work is the host,
//! Cedar controls who may run what.
//!
//! Run: `cargo run -p shen-authz-demo`

use std::str::FromStr;

use cedar_policy::{Authorizer, Context, Decision, Entities, EntityUid, PolicySet, Request};

use shen_cedar::interp::boot::boot;
use shen_cedar::interp::eval::Interp;
use shen_cedar::kl::ast::KlExpr;
use shen_cedar::kl::parser::parse_one;
use shen_cedar::value::Value;

/// One engine, one policy decision-point, sharing a process.
struct PolicyGatedShen {
    interp: Interp,
    authorizer: Authorizer,
    policies: PolicySet,
    entities: Entities,
}

impl PolicyGatedShen {
    fn new() -> Result<Self, String> {
        // --- Shen engine: served / VM mode, booted once. -------------------
        // `enable_vm()` is the programmatic form of `shen-cedar --served`:
        // runtime closures compile to bytecode, which amortizes across the
        // many requests a served process handles (~2.3× warm; see
        // scripts/warm-bench.sh). Must run before boot / any closure build.
        shen_cedar::interp::eval::enable_vm();
        let mut interp = Interp::new();
        boot(&mut interp).map_err(|e| format!("shen boot: {e}"))?;

        // --- Cedar: policy set + entity (role) hierarchy. ------------------
        // Two roles. Analysts may evaluate only *pure* Shen capabilities;
        // Admins may evaluate anything (including the `io` capability that
        // covers `load` / filesystem access).
        let policies = PolicySet::from_str(POLICIES).map_err(|e| format!("cedar policies: {e}"))?;
        let entities = Entities::from_json_str(ENTITIES_JSON, None)
            .map_err(|e| format!("cedar entities: {e}"))?;

        Ok(Self {
            interp,
            authorizer: Authorizer::new(),
            policies,
            entities,
        })
    }

    /// The combined operation: Cedar authorizes, then (and only then) the
    /// served Shen VM evaluates.
    fn authorized_eval(&mut self, principal: &str, capability: &str, shen_src: &str) -> Outcome {
        let req = match (
            EntityUid::from_str(principal),
            EntityUid::from_str(r#"Action::"Eval""#),
            EntityUid::from_str(capability),
        ) {
            (Ok(p), Ok(a), Ok(r)) => match Request::new(p, a, r, Context::empty(), None) {
                Ok(req) => req,
                Err(e) => return Outcome::Error(format!("cedar request: {e}")),
            },
            _ => return Outcome::Error("bad entity uid".into()),
        };

        let resp = self
            .authorizer
            .is_authorized(&req, &self.policies, &self.entities);
        match resp.decision() {
            Decision::Deny => Outcome::Denied,
            Decision::Allow => match self.eval_shen(shen_src) {
                Ok(v) => Outcome::Allowed(self.render(&v)),
                Err(e) => Outcome::Allowed(format!("<shen error: {e}>")),
            },
        }
    }

    /// Evaluate a Shen source line through the kernel's own `eval` (macro
    /// expansion + process-applications), exactly like the REPL / served path.
    fn eval_shen(&mut self, src: &str) -> Result<Value, shen_cedar::error::ShenError> {
        let expr = parse_one(src, &mut self.interp.symbols)
            .map_err(|e| shen_cedar::error::ShenError::new(format!("parse: {e}")))?;
        let eval_sym = self.interp.intern("eval");
        if self.interp.env.get_fn(eval_sym).is_some() {
            let quoted = klexpr_to_value(&expr);
            let f = self.interp.env.get_fn(eval_sym).cloned().unwrap();
            self.interp.apply(f, vec![quoted])
        } else {
            self.interp.eval(&expr)
        }
    }

    fn render(&self, v: &Value) -> String {
        if v.is_nil() {
            return "()".into();
        }
        if let Some(b) = v.as_bool() {
            return b.to_string();
        }
        if let Some(n) = v.as_int() {
            return n.to_string();
        }
        if let Some(s) = v.as_str() {
            return format!("{s:?}");
        }
        if let Some(s) = v.as_sym() {
            return self.interp.resolve(s).to_string();
        }
        if v.is_cons() {
            let mut out = String::from("(");
            let mut cur = *v;
            let mut first = true;
            while let (Some(h), Some(t)) = (cur.head(), cur.tail()) {
                if !first {
                    out.push(' ');
                }
                out.push_str(&self.render(h));
                first = false;
                cur = *t;
            }
            out.push(')');
            return out;
        }
        if v.is_vec() {
            // Shen "print vectors": slot 1 holds the display string (e.g. the
            // `fac` a `defun` returns).
            let cells = v.vec_cells();
            if cells.len() == 2 {
                if let Some(s) = cells[1].as_str() {
                    return s.to_string();
                }
            }
            return "<vector>".into();
        }
        if v.is_closure() {
            return "<closure>".into();
        }
        "<value>".into()
    }
}

enum Outcome {
    Allowed(String),
    Denied,
    Error(String),
}

fn klexpr_to_value(e: &KlExpr) -> Value {
    match e {
        KlExpr::Nil => Value::nil(),
        KlExpr::Bool(b) => Value::bool(*b),
        KlExpr::Int(n) => Value::int(*n),
        KlExpr::Float(x) => Value::float(*x),
        KlExpr::Str(s) => Value::str(s.clone()),
        KlExpr::Sym(s) => Value::sym(*s),
        KlExpr::App(items) => Value::list(items.iter().map(klexpr_to_value)),
    }
}

/// Cedar policies. `Eval` on a `ShenCap` resource is the gated operation.
const POLICIES: &str = r#"
// Analysts may evaluate pure (side-effect-free) Shen capabilities.
permit(
    principal in Role::"Analyst",
    action == Action::"Eval",
    resource == ShenCap::"pure"
);

// Admins may evaluate any Shen capability (including io: load / filesystem).
permit(
    principal in Role::"Admin",
    action == Action::"Eval",
    resource
);
"#;

/// Cedar entity hierarchy: who belongs to which role.
const ENTITIES_JSON: &str = r#"[
    { "uid": {"type":"User","id":"alice"}, "attrs": {}, "parents": [{"type":"Role","id":"Analyst"}] },
    { "uid": {"type":"User","id":"root"},  "attrs": {}, "parents": [{"type":"Role","id":"Admin"}] },
    { "uid": {"type":"Role","id":"Analyst"}, "attrs": {}, "parents": [] },
    { "uid": {"type":"Role","id":"Admin"},   "attrs": {}, "parents": [] }
]"#;

fn main() {
    // Mirror the REPL's stack workaround — type-checked / recursive Shen code
    // recurses deep through non-tail frames.
    let handle = std::thread::Builder::new()
        .name("shen-authz-demo".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(run)
        .expect("spawn worker");
    std::process::exit(handle.join().unwrap_or(2));
}

fn run() -> i32 {
    eprint!("booting served Shen VM + Cedar authorizer… ");
    let mut svc = match PolicyGatedShen::new() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("FAILED\n  {e}");
            return 1;
        }
    };
    eprintln!("ready.\n");

    // (principal, capability-resource, shen source, what it demonstrates)
    let requests: &[(&str, &str, &str, &str)] = &[
        (
            r#"User::"alice""#,
            r#"ShenCap::"pure""#,
            "(+ 2 3)",
            "analyst runs pure arithmetic",
        ),
        (
            r#"User::"alice""#,
            r#"ShenCap::"pure""#,
            "(defun fac (n) (if (= n 0) 1 (* n (fac (- n 1)))))",
            "analyst defines a recursive fn (VM-compiled)",
        ),
        (
            r#"User::"alice""#,
            r#"ShenCap::"pure""#,
            "(fac 12)",
            "analyst calls it — runs on the bytecode VM",
        ),
        (
            r#"User::"alice""#,
            r#"ShenCap::"io""#,
            "(load \"harness.shen\")",
            "analyst attempts io — Cedar blocks BEFORE Shen runs",
        ),
        (
            r#"User::"root""#,
            r#"ShenCap::"io""#,
            "(+ 100 1)",
            "admin may use io capability",
        ),
    ];

    println!("{:<48}  {:<10}  result", "request", "decision");
    println!("{}", "-".repeat(78));
    for (principal, cap, src, note) in requests {
        let who = principal.trim_start_matches("User::").trim_matches('"');
        let capname = cap.trim_start_matches("ShenCap::").trim_matches('"');
        let label = format!("{who} · {capname} · {src}");
        let label = truncate(&label, 48);
        match svc.authorized_eval(principal, cap, src) {
            Outcome::Allowed(r) => println!("{label:<48}  {:<10}  => {r}", "ALLOW"),
            Outcome::Denied => println!("{label:<48}  {:<10}  (engine not invoked)", "DENY"),
            Outcome::Error(e) => println!("{label:<48}  {:<10}  {e}", "ERROR"),
        }
        eprintln!("    └─ {note}");
    }
    println!("\nBoth engines linked natively in one Rust process: Cedar gates, the served Shen VM computes.");
    0
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n - 1).collect();
        out.push('…');
        out
    }
}
