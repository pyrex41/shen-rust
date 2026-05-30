//! Prototype B — **Shen's engine reasons ABOUT Cedar policies.**
//!
//! Cedar decides single requests; it does not, on its own, tell you that one
//! policy is dead (can never grant) or that a permit and a forbid overlap.
//! Here the *analysis* is written in Shen — the logic/proof language — and run
//! by the served Shen VM over the scopes extracted from a real Cedar
//! `PolicySet`. Rust does only the plumbing (introspect Cedar, marshal scopes
//! in, collect findings); the verification semantics live in Shen.
//!
//! Per (permit, forbid) pair the Shen `classify` predicate returns one of:
//!   * `shadowed` — the forbid's scope ⊇ the permit's  ⇒ the permit is DEAD
//!     (Cedar's forbid-wins semantics means it can never actually grant);
//!   * `overlap`  — the scopes intersect but neither subsumes  ⇒ partial
//!     conflict (forbid wins on the intersection);
//!   * `disjoint` — no interaction.
//!
//! Run: `cargo run -p shen-cedar-demos --bin verify`

use std::str::FromStr;

use cedar_policy::{
    ActionConstraint, Effect, Policy, PolicySet, PrincipalConstraint, ResourceConstraint,
};

use shen_cedar_demos::ShenHost;

/// The Shen "verifier": pure scope reasoning, loaded into the engine.
const VERIFIER: &[&str] = &[
    // Relation of one forbid-scope `f` to the matching permit-scope `p`.
    "(defun srel (f p) (if (= f p) eq (if (= f \"any\") fany (if (= p \"any\") pany dis))))",
    // Does the forbid cover this scope (equal, or forbid is unconstrained)?
    "(defun is-cover (r) (if (= r eq) true (if (= r fany) true false)))",
    // Can the two scopes ever intersect (anything but provably-disjoint)?
    "(defun is-inter (r) (if (= r dis) false true))",
    // Classify a (forbid, permit) pair from their principal/action/resource scopes.
    "(defun classify (fp fa fr pp pa pr) \
       (let rp (srel fp pp) (let ra (srel fa pa) (let rr (srel fr pr) \
         (if (and (is-cover rp) (and (is-cover ra) (is-cover rr))) shadowed \
           (if (and (is-inter rp) (and (is-inter ra) (is-inter rr))) overlap disjoint))))))",
];

/// Policies with deliberate interactions: a dead permit and a partial conflict.
const POLICIES: &str = r#"
// p0: interns are forbidden everything.
forbid(principal in Role::"Intern", action == Action::"Eval", resource);

// p1: ...but this permit tries to let them run pure code — DEAD (shadowed by p0).
permit(principal in Role::"Intern", action == Action::"Eval", resource == ShenCap::"pure");

// p2: forbid io for everyone on the io capability.
forbid(principal, action == Action::"Eval", resource == ShenCap::"io");

// p3: analysts may run anything — OVERLAPS p2 on the io capability (forbid wins there).
permit(principal in Role::"Analyst", action == Action::"Eval", resource);

// p4: admins may run pure code — disjoint from the forbids above.
permit(principal in Role::"Admin", action == Action::"Eval", resource == ShenCap::"pure");
"#;

struct Scope {
    id: String,
    principal: String,
    action: String,
    resource: String,
}

fn main() {
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(run)
        .expect("spawn");
    std::process::exit(handle.join().unwrap_or(2));
}

fn run() -> i32 {
    eprint!("booting served Shen VM… ");
    let mut host = match ShenHost::new() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("FAILED: {e}");
            return 1;
        }
    };
    if let Err(e) = host.define_all(VERIFIER) {
        eprintln!("load verifier: {e}");
        return 1;
    }
    eprintln!("ready.\n");

    let set = match PolicySet::from_str(POLICIES) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cedar parse: {e}");
            return 1;
        }
    };

    let mut permits = Vec::new();
    let mut forbids = Vec::new();
    for p in set.policies() {
        let sc = scope_of(p);
        println!(
            "{:<6} {:<7} principal={:<22} resource={}",
            sc.id,
            effect_name(p.effect()),
            sc.principal,
            sc.resource,
        );
        match p.effect() {
            Effect::Permit => permits.push(sc),
            Effect::Forbid => forbids.push(sc),
        }
    }

    println!("\nShen-computed policy interactions (forbid vs permit):");
    let mut findings = 0;
    for f in &forbids {
        for p in &permits {
            let verdict = host
                .call(
                    "classify",
                    vec![
                        host.string(&f.principal),
                        host.string(&f.action),
                        host.string(&f.resource),
                        host.string(&p.principal),
                        host.string(&p.action),
                        host.string(&p.resource),
                    ],
                )
                .map(|v| host.text(&v))
                .unwrap_or_else(|e| format!("<err {e}>"));
            match verdict.as_str() {
                "shadowed" => {
                    findings += 1;
                    println!(
                        "  ⚠ {} is DEAD — fully shadowed by forbid {} (can never grant)",
                        p.id, f.id
                    );
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
    println!(
        "\nShen reasoned over {} Cedar policies and flagged {} interaction(s).",
        permits.len() + forbids.len(),
        findings,
    );
    0
}

fn effect_name(e: Effect) -> &'static str {
    match e {
        Effect::Permit => "permit",
        Effect::Forbid => "forbid",
    }
}

/// Extract a policy's scope into the string form the Shen verifier consumes.
/// Unconstrained scopes become the sentinel `"any"`; constrained ones use
/// Cedar's canonical `Type::"id"` rendering (stable across policies, so string
/// equality in Shen is a sound scope-equality test for this model).
fn scope_of(p: &Policy) -> Scope {
    Scope {
        id: p.id().to_string(),
        principal: match p.principal_constraint() {
            PrincipalConstraint::Any => "any".into(),
            PrincipalConstraint::In(uid)
            | PrincipalConstraint::Eq(uid)
            | PrincipalConstraint::IsIn(_, uid) => uid.to_string(),
            PrincipalConstraint::Is(t) => t.to_string(),
        },
        action: match p.action_constraint() {
            ActionConstraint::Any => "any".into(),
            ActionConstraint::Eq(uid) => uid.to_string(),
            ActionConstraint::In(uids) => {
                uids.first().map(|u| u.to_string()).unwrap_or("any".into())
            }
        },
        resource: match p.resource_constraint() {
            ResourceConstraint::Any => "any".into(),
            ResourceConstraint::In(uid)
            | ResourceConstraint::Eq(uid)
            | ResourceConstraint::IsIn(_, uid) => uid.to_string(),
            ResourceConstraint::Is(t) => t.to_string(),
        },
    }
}
