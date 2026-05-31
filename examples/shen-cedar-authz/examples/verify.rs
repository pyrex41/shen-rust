//! Example 2 — **Shen reasons ABOUT Cedar policies** (hardened).
//!
//! Cedar answers single requests; it does not tell you a policy is dead or
//! that two policies conflict. Here the *analysis* is Shen-authored and runs
//! on the served VM over the scopes extracted from a real, schema-validated
//! `PolicySet`.
//!
//! Hardening over the original prototype:
//!   * the policy set is strict-validated against `authz.cedarschema` first;
//!   * **`in` is resolved against the role DAG**, not treated as string
//!     equality. The Shen verifier carries a `reaches` (membership-closure)
//!     predicate, so `forbid(principal in Role::"Staff")` correctly shadows
//!     `permit(principal in Role::"Analyst")` when `Analyst in Staff` — a
//!     finding the string-equality prototype could not make;
//!   * each static finding is cross-checked against the live Cedar authorizer.
//!
//! Run: `cargo run -p shen-cedar-authz --example verify`

use std::str::FromStr;

use cedar_policy::{
    ActionConstraint, Authorizer, Context, Decision, Effect, EntityUid, Policy, PolicySet,
    PrincipalConstraint, Request, ResourceConstraint,
};

use shen_cedar::value::Value;
use shen_cedar_authz::{entities, schema, validate_policies, ShenHost};

/// The Shen verifier: hierarchy-aware scope reasoning, loaded into the engine.
/// A scope is encoded `[kind id]` with kind ∈ {kany, kin, keq}; `es` is the
/// membership DAG as `[child parent]` edges.
const VERIFIER: &[&str] = &[
    "(defun parents-of (x es) (if (= es []) [] \
       (if (= (hd (hd es)) x) (cons (hd (tl (hd es))) (parents-of x (tl es))) \
         (parents-of x (tl es)))))",
    // a reaches b  ==  a is-in b  (a == b, or some parent of a reaches b)
    "(defun reaches (a b es) (if (= a b) true (reaches-list (parents-of a es) b es)))",
    "(defun reaches-list (ps b es) (if (= ps []) false \
       (if (reaches (hd ps) b es) true (reaches-list (tl ps) b es))))",
    // forbid-scope f COVERS permit-scope p  (f's entity-set ⊇ p's)
    "(defun s-covers (f p es) \
       (let fk (hd f) (let fi (hd (tl f)) (let pk (hd p) (let pi (hd (tl p)) \
         (if (= fk kany) true \
           (if (= fk kin) \
               (if (= pk kin) (reaches pi fi es) (if (= pk keq) (reaches pi fi es) false)) \
             (if (= pk keq) (= fi pi) false))))))))",
    // forbid-scope f INTERSECTS permit-scope p
    "(defun s-inter (f p es) \
       (let fk (hd f) (let fi (hd (tl f)) (let pk (hd p) (let pi (hd (tl p)) \
         (if (= fk kany) true \
           (if (= pk kany) true \
             (if (= fk kin) \
                 (if (= pk kin) (or (reaches fi pi es) (reaches pi fi es)) (reaches pi fi es)) \
               (if (= pk keq) (= fi pi) (reaches fi pi es))))))))))",
    "(defun classify (fp fr pp pr es) \
       (if (and (s-covers fp pp es) (s-covers fr pr es)) shadowed \
         (if (and (s-inter fp pp es) (s-inter fr pr es)) overlap disjoint)))",
];

const POLICIES: &str = r#"
// p0: staff are forbidden everything.
forbid(principal in Role::"Staff", action == Action::"Eval", resource);

// p1: analysts may run pure code — DEAD: Analyst in Staff, so p0 shadows it.
permit(principal in Role::"Analyst", action == Action::"Eval", resource == ShenCap::"pure");

// p2: nobody may use the io capability.
forbid(principal, action == Action::"Eval", resource == ShenCap::"io");

// p3: admins may run anything — OVERLAPS p2 on io (forbid wins there).
permit(principal in Role::"Admin", action == Action::"Eval", resource);

// p4: managers may run pure — disjoint from both forbids.
permit(principal in Role::"Manager", action == Action::"Eval", resource == ShenCap::"pure");
"#;

/// Membership DAG (child `in` parent), in Cedar's canonical entity rendering.
const HIERARCHY: &[(&str, &str)] = &[
    (r#"Role::"Analyst""#, r#"Role::"Staff""#),
    (r#"Role::"Intern""#, r#"Role::"Staff""#),
];

/// Entities for the live cross-check: alice is an Analyst (∈ Staff).
const ENTITIES_JSON: &str = r#"[
    { "uid": {"type":"User","id":"alice"}, "attrs": {}, "parents": [{"type":"Role","id":"Analyst"}] },
    { "uid": {"type":"Role","id":"Analyst"}, "attrs": {}, "parents": [{"type":"Role","id":"Staff"}] },
    { "uid": {"type":"Role","id":"Staff"},   "attrs": {}, "parents": [] }
]"#;

struct Scope {
    id: String,
    principal: (String, String),
    resource: (String, String),
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
    let set = match PolicySet::from_str(POLICIES) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cedar parse: {e}");
            return 1;
        }
    };
    let errs = validate_policies(&set, &schema);
    if !errs.is_empty() {
        eprintln!("policy validation FAILED:");
        for e in errs {
            eprintln!("  - {e}");
        }
        return 1;
    }
    eprintln!("policies: strict-validated against schema ✓");

    eprint!("booting served Shen VM… ");
    let mut host = match ShenHost::new() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("FAILED: {e}");
            return 1;
        }
    };
    if let Err(e) = host.load_source(&VERIFIER.join("\n")) {
        eprintln!("load verifier: {e}");
        return 1;
    }
    eprintln!("ready.\n");

    // Build the membership DAG as Shen data once.
    let edges: Vec<Value> = HIERARCHY
        .iter()
        .map(|(c, p)| host.list([host.string(c), host.string(p)]))
        .collect();
    let edges = host.list(edges);

    let mut permits = Vec::new();
    let mut forbids = Vec::new();
    for p in set.policies() {
        let sc = scope_of(p);
        println!(
            "{:<6} {:<7} principal={:<26} resource={}",
            sc.id,
            effect_name(p.effect()),
            fmt_scope(&sc.principal),
            fmt_scope(&sc.resource),
        );
        match p.effect() {
            Effect::Permit => permits.push(sc),
            Effect::Forbid => forbids.push(sc),
        }
    }

    println!("\nShen-computed interactions (hierarchy-aware: `in` resolved over the role DAG):");
    let mut findings = 0;
    for f in &forbids {
        for p in &permits {
            let args = vec![
                scope_val(&mut host, &f.principal),
                scope_val(&mut host, &f.resource),
                scope_val(&mut host, &p.principal),
                scope_val(&mut host, &p.resource),
                edges,
            ];
            let verdict = host
                .call("classify", args)
                .map(|v| host.text(&v))
                .unwrap_or_else(|e| format!("<err {e}>"));
            match verdict.as_str() {
                "shadowed" => {
                    findings += 1;
                    let via = if needs_hierarchy(&f.principal, &p.principal) {
                        format!(
                            "  (via {} in {} — string-equality would miss this)",
                            tail(&p.principal.1),
                            tail(&f.principal.1)
                        )
                    } else {
                        String::new()
                    };
                    println!("  ⚠ {} is DEAD — shadowed by forbid {}{}", p.id, f.id, via);
                }
                "overlap" => {
                    findings += 1;
                    println!(
                        "  ⚠ {} OVERLAPS forbid {} — forbid wins on the intersection",
                        p.id, f.id
                    );
                }
                _ => {}
            }
        }
    }
    if findings == 0 {
        println!("  (none)");
    }

    // Cross-check the shadow against the LIVE Cedar authorizer: alice (Analyst
    // ∈ Staff) requesting `pure` must be DENIED despite p1's permit.
    let entities = match entities(ENTITIES_JSON, &schema) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let req = Request::new(
        EntityUid::from_str(r#"User::"alice""#).unwrap(),
        EntityUid::from_str(r#"Action::"Eval""#).unwrap(),
        EntityUid::from_str(r#"ShenCap::"pure""#).unwrap(),
        Context::empty(),
        Some(&schema),
    )
    .unwrap();
    let decision = Authorizer::new()
        .is_authorized(&req, &set, &entities)
        .decision();
    let confirmed = matches!(decision, Decision::Deny);
    println!(
        "\nCross-check (live Cedar): alice(Analyst∈Staff) · pure => {} {}",
        if confirmed { "DENY" } else { "ALLOW" },
        if confirmed {
            "✓ confirms p1 is dead"
        } else {
            "✗ static finding NOT confirmed"
        }
    );
    println!(
        "\nShen reasoned over {} policies, flagged {} interaction(s).",
        permits.len() + forbids.len(),
        findings
    );
    if confirmed {
        0
    } else {
        1
    }
}

fn scope_val(host: &mut ShenHost, (kind, id): &(String, String)) -> Value {
    let k = host.symbol(kind);
    let i = host.string(id);
    host.list([k, i])
}

fn needs_hierarchy(f: &(String, String), p: &(String, String)) -> bool {
    f.0 == "kin" && p.0 == "kin" && f.1 != p.1
}

fn fmt_scope((kind, id): &(String, String)) -> String {
    match kind.as_str() {
        "kany" => "any".into(),
        "kin" => format!("in {id}"),
        "keq" => format!("== {id}"),
        _ => id.clone(),
    }
}

fn tail(uid: &str) -> &str {
    uid.rsplit("::").next().unwrap_or(uid).trim_matches('"')
}

fn effect_name(e: Effect) -> &'static str {
    match e {
        Effect::Permit => "permit",
        Effect::Forbid => "forbid",
    }
}

fn scope_of(p: &Policy) -> Scope {
    Scope {
        id: p.id().to_string(),
        principal: match p.principal_constraint() {
            PrincipalConstraint::Any => ("kany".into(), String::new()),
            PrincipalConstraint::In(uid) | PrincipalConstraint::IsIn(_, uid) => {
                ("kin".into(), uid.to_string())
            }
            PrincipalConstraint::Eq(uid) => ("keq".into(), uid.to_string()),
            PrincipalConstraint::Is(_) => ("kany".into(), String::new()),
        },
        resource: match p.resource_constraint() {
            ResourceConstraint::Any => ("kany".into(), String::new()),
            ResourceConstraint::In(uid) | ResourceConstraint::IsIn(_, uid) => {
                ("kin".into(), uid.to_string())
            }
            ResourceConstraint::Eq(uid) => ("keq".into(), uid.to_string()),
            ResourceConstraint::Is(_) => ("kany".into(), String::new()),
        },
    }
}

// Action is uniform (`== Action::"Eval"`) across these policies; kept explicit
// so the example documents that scope reasoning covers principal+resource and
// treats the action dimension as already-matching.
#[allow(dead_code)]
fn action_str(p: &Policy) -> String {
    match p.action_constraint() {
        ActionConstraint::Any => "any".into(),
        ActionConstraint::Eq(uid) => uid.to_string(),
        ActionConstraint::In(uids) => uids.first().map(|u| u.to_string()).unwrap_or("any".into()),
    }
}
