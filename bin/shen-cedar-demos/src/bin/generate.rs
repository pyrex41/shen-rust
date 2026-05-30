//! Prototype C — **Cedar policies generated FROM a Shen spec.**
//!
//! Parallel to the project's shengen line (Shen specs → guard types for other
//! languages): here the source of truth is a Shen authorization spec, and the
//! compilation target is Cedar policy text. The spec states *base grants* and a
//! *role-inheritance* DAG; the served Shen VM computes the transitive grant
//! closure (a role gets its own grants plus every ancestor's), and Rust renders
//! the result to Cedar `permit` statements. The loop is then closed: the
//! generated policy is parsed back by Cedar and enforced on live requests.
//!
//! Run: `cargo run -p shen-cedar-demos --bin generate`

use std::str::FromStr;

use cedar_policy::{Authorizer, Context, Decision, Entities, EntityUid, PolicySet, Request};

use shen_cedar_demos::{read_list, ShenHost};

/// The Shen authorization spec + the closure that expands it. Editing THIS is
/// how you change the policy; Cedar below is generated, never hand-written.
const SPEC: &[&str] = &[
    // ── the spec (source of truth) ──────────────────────────────────────
    // base grants: [role action resource]
    "(defun base-grants () [[Analyst Eval pure] [Auditor Eval logs]])",
    // role inheritance edges: [role parent]
    "(defun role-parents () [[Admin Analyst] [Admin Auditor] [Lead Analyst]])",
    // roles to emit policy for
    "(defun all-roles () [Analyst Auditor Admin Lead])",
    // ── the closure (computation the spec language buys you) ─────────────
    "(defun appnd (a b) (if (= a []) b (cons (hd a) (appnd (tl a) b))))",
    "(defun mem (x xs) (if (= xs []) false (if (= x (hd xs)) true (mem x (tl xs)))))",
    "(defun parents-h (r es) (if (= es []) [] \
       (if (= (hd (hd es)) r) (cons (hd (tl (hd es))) (parents-h r (tl es))) \
         (parents-h r (tl es)))))",
    "(defun parents (r) (parents-h r (role-parents)))",
    "(defun anc-list (rs) (if (= rs []) [] (appnd (ancestors (hd rs)) (anc-list (tl rs)))))",
    "(defun ancestors (r) (cons r (anc-list (parents r))))",
    "(defun gfilter (anc gs r) (if (= gs []) [] \
       (if (mem (hd (hd gs)) anc) (cons (cons r (tl (hd gs))) (gfilter anc (tl gs) r)) \
         (gfilter anc (tl gs) r))))",
    "(defun grants-of (r) (gfilter (ancestors r) (base-grants) r))",
    "(defun expand-roles (rs) (if (= rs []) [] (appnd (grants-of (hd rs)) (expand-roles (tl rs)))))",
    "(defun expand-all () (expand-roles (all-roles)))",
];

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
    if let Err(e) = host.define_all(SPEC) {
        eprintln!("load spec: {e}");
        return 1;
    }
    eprintln!("ready.\n");

    // Shen computes the transitive grant closure.
    let grants_v = match host.call("expand-all", vec![]) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("expand-all: {e}");
            return 1;
        }
    };

    // Render each [role action resource] triple to a Cedar permit.
    let mut cedar = String::new();
    let mut seen = Vec::new();
    let mut count = 0;
    for triple in read_list(&grants_v) {
        let parts = read_list(&triple);
        if parts.len() != 3 {
            continue;
        }
        let role = host.text(&parts[0]);
        let action = host.text(&parts[1]);
        let resource = host.text(&parts[2]);
        let policy = format!(
            "permit(principal in Role::{:?}, action == Action::{:?}, resource == ShenCap::{:?});",
            role, action, resource
        );
        if seen.contains(&policy) {
            continue;
        }
        seen.push(policy.clone());
        cedar.push_str(&policy);
        cedar.push('\n');
        count += 1;
    }

    println!("=== Cedar policy generated from the Shen spec ===\n{cedar}");

    // Close the loop: Cedar parses what Shen produced.
    let set = match PolicySet::from_str(&cedar) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("generated Cedar failed to parse: {e}");
            return 1;
        }
    };
    println!("Cedar accepted the generated policy: {count} permits.\n");

    // ...and enforces it. Admin inherits Analyst(pure)+Auditor(logs); Lead
    // inherits only Analyst(pure), so Lead·logs must be denied.
    let entities = Entities::from_json_str(ENTITIES_JSON, None).expect("entities");
    let authz = Authorizer::new();
    let checks = [
        (
            r#"User::"dana""#,
            r#"ShenCap::"logs""#,
            "dana(Admin) · logs",
            true,
        ),
        (
            r#"User::"dana""#,
            r#"ShenCap::"pure""#,
            "dana(Admin) · pure",
            true,
        ),
        (
            r#"User::"erin""#,
            r#"ShenCap::"pure""#,
            "erin(Lead) · pure",
            true,
        ),
        (
            r#"User::"erin""#,
            r#"ShenCap::"logs""#,
            "erin(Lead) · logs",
            false,
        ),
    ];
    println!("Enforcing the generated policy on live requests:");
    let mut ok = true;
    for (principal, resource, label, expect_allow) in checks {
        let req = Request::new(
            EntityUid::from_str(principal).unwrap(),
            EntityUid::from_str(r#"Action::"Eval""#).unwrap(),
            EntityUid::from_str(resource).unwrap(),
            Context::empty(),
            None,
        )
        .expect("request");
        let allowed = matches!(
            authz.is_authorized(&req, &set, &entities).decision(),
            Decision::Allow
        );
        let mark = if allowed == expect_allow {
            "✓"
        } else {
            "✗ UNEXPECTED"
        };
        ok &= allowed == expect_allow;
        println!(
            "  {label:<20} => {:<5} {mark}",
            if allowed { "ALLOW" } else { "DENY" }
        );
    }
    println!(
        "\nShen spec → Cedar policy → enforced. Inheritance closure computed by the Shen engine.{}",
        if ok { "" } else { "  (a check did not match!)" }
    );
    if ok {
        0
    } else {
        1
    }
}

const ENTITIES_JSON: &str = r#"[
    { "uid": {"type":"User","id":"dana"}, "attrs": {}, "parents": [{"type":"Role","id":"Admin"}] },
    { "uid": {"type":"User","id":"erin"}, "attrs": {}, "parents": [{"type":"Role","id":"Lead"}] },
    { "uid": {"type":"Role","id":"Admin"},    "attrs": {}, "parents": [] },
    { "uid": {"type":"Role","id":"Lead"},     "attrs": {}, "parents": [] },
    { "uid": {"type":"Role","id":"Analyst"},  "attrs": {}, "parents": [] },
    { "uid": {"type":"Role","id":"Auditor"},  "attrs": {}, "parents": [] }
]"#;
