//! Example 1 — **Cedar gates the served Shen VM** (hardened).
//!
//! A policy-gated Shen evaluator: each request is (principal, action,
//! resource, shen-source). Cedar authorizes it; only on `Allow` does the
//! served Shen VM evaluate the source.
//!
//! Hardening over the original prototype:
//!   * the policy set is strict-validated against `authz.cedarschema` at
//!     startup (a type error in policies fails fast, before any request);
//!   * entities are validated against the schema (`Entities::from_json_str`
//!     with `Some(schema)`);
//!   * requests are built schema-checked (`Request::new(.., Some(&schema))`),
//!     so a malformed principal/action/resource is rejected by Cedar.
//!
//! Run: `cargo run -p shen-cedar-authz --example gate`

use std::str::FromStr;

use cedar_policy::{Authorizer, Context, Decision, EntityUid, PolicySet, Request};

use shen_cedar_authz::{entities, schema, validate_policies, ShenHost};

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

const ENTITIES_JSON: &str = r#"[
    { "uid": {"type":"User","id":"alice"}, "attrs": {}, "parents": [{"type":"Role","id":"Analyst"}] },
    { "uid": {"type":"User","id":"root"},  "attrs": {}, "parents": [{"type":"Role","id":"Admin"}] },
    { "uid": {"type":"Role","id":"Analyst"}, "attrs": {}, "parents": [] },
    { "uid": {"type":"Role","id":"Admin"},   "attrs": {}, "parents": [] }
]"#;

enum Outcome {
    Allowed(String),
    Denied,
    Error(String),
}

fn main() {
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(run)
        .expect("spawn");
    std::process::exit(handle.join().unwrap_or(2));
}

fn run() -> i32 {
    let schema = match schema() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let policies = match PolicySet::from_str(POLICIES) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cedar parse: {e}");
            return 1;
        }
    };
    // Hardening: strict-validate the policy set before serving anything.
    let errs = validate_policies(&policies, &schema);
    if !errs.is_empty() {
        eprintln!("policy validation FAILED:");
        for e in errs {
            eprintln!("  - {e}");
        }
        return 1;
    }
    eprintln!("policies: strict-validated against schema ✓");

    let entities = match entities(ENTITIES_JSON, &schema) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    eprintln!("entities: schema-validated ✓");

    eprint!("booting served Shen VM… ");
    let mut host = match ShenHost::new() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("FAILED: {e}");
            return 1;
        }
    };
    eprintln!("ready.\n");

    let authz = Authorizer::new();
    let requests: &[(&str, &str, &str, &str)] = &[
        (
            r#"User::"alice""#,
            r#"ShenCap::"pure""#,
            "(+ 2 3)",
            "analyst · pure arithmetic",
        ),
        (
            r#"User::"alice""#,
            r#"ShenCap::"pure""#,
            "(defun fac (n) (if (= n 0) 1 (* n (fac (- n 1)))))",
            "analyst · define recursive fn (VM-compiled)",
        ),
        (
            r#"User::"alice""#,
            r#"ShenCap::"pure""#,
            "(fac 12)",
            "analyst · call it (runs on the VM)",
        ),
        (
            r#"User::"alice""#,
            r#"ShenCap::"io""#,
            "(load \"harness.shen\")",
            "analyst · io — Cedar blocks before Shen runs",
        ),
        (
            r#"User::"root""#,
            r#"ShenCap::"io""#,
            "(+ 100 1)",
            "admin · io capability",
        ),
    ];

    println!("{:<46}  {:<8}  result", "request", "decision");
    println!("{}", "-".repeat(78));
    for (principal, cap, src, note) in requests {
        let outcome = authorized_eval(
            &mut host, &authz, &policies, &entities, &schema, principal, cap, src,
        );
        let who = principal.trim_start_matches("User::").trim_matches('"');
        let capname = cap.trim_start_matches("ShenCap::").trim_matches('"');
        let label = truncate(&format!("{who} · {capname} · {src}"), 46);
        match outcome {
            Outcome::Allowed(r) => println!("{label:<46}  {:<8}  => {r}", "ALLOW"),
            Outcome::Denied => println!("{label:<46}  {:<8}  (engine not invoked)", "DENY"),
            Outcome::Error(e) => println!("{label:<46}  {:<8}  {e}", "ERROR"),
        }
        eprintln!("    └─ {note}");
    }
    println!("\nCedar gates (schema-validated), the served Shen VM computes — one Rust process.");
    0
}

#[allow(clippy::too_many_arguments)]
fn authorized_eval(
    host: &mut ShenHost,
    authz: &Authorizer,
    policies: &PolicySet,
    entities: &cedar_policy::Entities,
    schema: &cedar_policy::Schema,
    principal: &str,
    capability: &str,
    shen_src: &str,
) -> Outcome {
    // Schema-checked request: Cedar rejects malformed principal/action/resource.
    let req = match (
        EntityUid::from_str(principal),
        EntityUid::from_str(r#"Action::"Eval""#),
        EntityUid::from_str(capability),
    ) {
        (Ok(p), Ok(a), Ok(r)) => match Request::new(p, a, r, Context::empty(), Some(schema)) {
            Ok(req) => req,
            Err(e) => return Outcome::Error(format!("invalid request: {e}")),
        },
        _ => return Outcome::Error("bad entity uid".into()),
    };

    match authz.is_authorized(&req, policies, entities).decision() {
        Decision::Deny => Outcome::Denied,
        Decision::Allow => match host.eval(shen_src) {
            Ok(v) => Outcome::Allowed(render(host, &v)),
            Err(e) => Outcome::Allowed(format!("<shen error: {e}>")),
        },
    }
}

fn render(host: &ShenHost, v: &shen_cedar::value::Value) -> String {
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
        return host.interp.resolve(s).to_string();
    }
    if v.is_vec() {
        let cells = v.vec_cells();
        if cells.len() == 2 {
            if let Some(s) = cells[1].as_str() {
                return s.to_string();
            }
        }
        return "<vector>".into();
    }
    if v.is_cons() {
        let mut out = String::from("(");
        let mut cur = *v;
        let mut first = true;
        while let (Some(h), Some(t)) = (cur.head(), cur.tail()) {
            if !first {
                out.push(' ');
            }
            out.push_str(&render(host, h));
            first = false;
            cur = *t;
        }
        out.push(')');
        return out;
    }
    "<value>".into()
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
