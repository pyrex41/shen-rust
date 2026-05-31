//! Example 3 — **Cedar generated FROM a Shen spec** (hardened).
//!
//! Parallel to the shengen line (Shen spec → guard types): the source of truth
//! is `spec/authz.shen`, and Cedar is a build artifact. The served Shen VM
//! computes the transitive grant closure over the role-inheritance DAG; the
//! host renders Cedar permits, strict-validates them against the schema, writes
//! the artifact, and enforces it.
//!
//! Hardening over the original prototype:
//!   * the spec is the committed file `spec/authz.shen` (embedded via
//!     `include_str!`), not inline strings — editing it is how policy changes;
//!   * the generated policy set is strict-validated against `authz.cedarschema`
//!     (generation that produces an ill-typed policy fails the build);
//!   * the generated Cedar is written to disk as an artifact;
//!   * entities are schema-validated and the policy is enforced on live requests.
//!
//! Run: `cargo run -p shen-cedar-authz --example generate`

use std::str::FromStr;

use cedar_policy::{Authorizer, Context, Decision, EntityUid, PolicySet, Request};

use shen_cedar_authz::{entities, read_list, schema, validate_policies, ShenHost};

/// The committed Shen spec — the single source of truth.
const SPEC_SRC: &str = include_str!("../spec/authz.shen");

const ENTITIES_JSON: &str = r#"[
    { "uid": {"type":"User","id":"dana"}, "attrs": {}, "parents": [{"type":"Role","id":"Admin"}] },
    { "uid": {"type":"User","id":"erin"}, "attrs": {}, "parents": [{"type":"Role","id":"Lead"}] },
    { "uid": {"type":"Role","id":"Admin"},    "attrs": {}, "parents": [] },
    { "uid": {"type":"Role","id":"Lead"},     "attrs": {}, "parents": [] },
    { "uid": {"type":"Role","id":"Analyst"},  "attrs": {}, "parents": [] },
    { "uid": {"type":"Role","id":"Auditor"},  "attrs": {}, "parents": [] }
]"#;

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

    eprint!("booting served Shen VM… ");
    let mut host = match ShenHost::new() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("FAILED: {e}");
            return 1;
        }
    };
    if let Err(e) = host.load_source(SPEC_SRC) {
        eprintln!("load spec: {e}");
        return 1;
    }
    eprintln!("ready (spec: spec/authz.shen).\n");

    // Shen computes the transitive grant closure.
    let grants_v = match host.call("expand-all", vec![]) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("expand-all: {e}");
            return 1;
        }
    };

    let mut cedar = String::new();
    let mut seen: Vec<String> = Vec::new();
    for triple in read_list(&grants_v) {
        let parts = read_list(&triple);
        if parts.len() != 3 {
            continue;
        }
        let policy = format!(
            "permit(principal in Role::{:?}, action == Action::{:?}, resource == ShenCap::{:?});",
            host.text(&parts[0]),
            host.text(&parts[1]),
            host.text(&parts[2]),
        );
        if !seen.contains(&policy) {
            seen.push(policy.clone());
            cedar.push_str(&policy);
            cedar.push('\n');
        }
    }

    println!("=== Cedar generated from spec/authz.shen ===\n{cedar}");

    // Close the loop: Cedar parses + strict-validates what Shen produced.
    let set = match PolicySet::from_str(&cedar) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("generated Cedar failed to parse: {e}");
            return 1;
        }
    };
    let errs = validate_policies(&set, &schema);
    if !errs.is_empty() {
        eprintln!("generated policy FAILED schema validation:");
        for e in errs {
            eprintln!("  - {e}");
        }
        return 1;
    }
    println!(
        "Generated {} permits — Cedar-parsed + strict-validated ✓",
        seen.len()
    );

    // Write the build artifact.
    let out = std::env::temp_dir().join("shen-cedar-authz.generated.cedar");
    match std::fs::write(&out, &cedar) {
        Ok(()) => println!("Artifact written: {}\n", out.display()),
        Err(e) => eprintln!("(could not write artifact: {e})\n"),
    }

    // Enforce. Admin inherits Analyst(pure)+Auditor(logs); Lead inherits only
    // Analyst(pure) — so erin(Lead)·logs must be denied.
    let entities = match entities(ENTITIES_JSON, &schema) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
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
            Some(&schema),
        )
        .expect("request");
        let allowed = matches!(
            authz.is_authorized(&req, &set, &entities).decision(),
            Decision::Allow
        );
        ok &= allowed == expect_allow;
        let mark = if allowed == expect_allow {
            "✓"
        } else {
            "✗ UNEXPECTED"
        };
        println!(
            "  {label:<20} => {:<5} {mark}",
            if allowed { "ALLOW" } else { "DENY" }
        );
    }
    println!(
        "\nspec/authz.shen → Cedar (validated artifact) → enforced. Closure computed by the Shen engine.{}",
        if ok { "" } else { "  (a check did not match!)" }
    );
    if ok {
        0
    } else {
        1
    }
}
