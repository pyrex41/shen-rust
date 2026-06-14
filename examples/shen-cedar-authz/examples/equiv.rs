//! Example 4 — **codegen-equivalence gate** (shen-derive, native).
//!
//! `generate.rs` renders Cedar from `spec/authz.shen` and strict-validates the
//! *shape* of the result against the schema. It never checks that the Cedar
//! PolicySet *decides the same way* as the Shen spec it came from. Schema
//! validation catches an ill-typed policy; it does not catch a closure bug, a
//! rendering bug, or a hand-edit that quietly drops a permit.
//!
//! This is the shen-derive move, done natively: the Shen spec is the **oracle**
//! (evaluated in the served VM, `expand-all`), the generated Cedar is the
//! **target**, and the gate enumerates the whole finite request space asserting
//! the two agree on every decision. No Go, no subprocess — the VM and the Cedar
//! `Authorizer` run in one Rust process, exactly as the other examples do.
//!
//! The harness itself is generic over two small traits:
//!
//!   * [`Oracle`] — the source-of-truth decision. Today the only impl is
//!     [`ShenSpecOracle`], backed by the served Shen VM's `expand-all` permit
//!     set. The trait is deliberately narrow (`allows` + `name`) so a *different*
//!     oracle — e.g. an out-of-process SBCL evaluation of the same spec — could
//!     be dropped in to cross-check the shen-rust VM itself.
//!   * [`Target`] — the artifact under test. Today the only impl is
//!     [`CedarTarget`], backed by the Cedar `Authorizer` over the generated
//!     `PolicySet` + entities. The trait is likewise narrow so a *different*
//!     target — e.g. a shengen→Rust compiled guard function — could be checked
//!     against the same oracle with no change to the gate loop.
//!
//! Those alternate impls are intentionally **not** provided here; the traits
//! just establish the seam.
//!
//! Pass `--inject-drift` to drop one generated permit before the check, to see
//! the gate flag the now-divergent requests (and exit non-zero).
//!
//! Run: `cargo run -p shen-cedar-authz --example equiv [-- --inject-drift]`

use std::collections::HashSet;
use std::str::FromStr;

use cedar_policy::{
    Authorizer, Context, Decision, Entities, EntityUid, PolicySet, Request, Schema,
};

use shen_cedar_authz::{entities, read_list, schema, validate_policies, ShenHost};

const SPEC_SRC: &str = include_str!("../spec/authz.shen");

/// The test fixture: one user per role, so every role's closure is exercised.
/// (user, role) — each user is a direct member of exactly that Role.
const USERS: &[(&str, &str)] = &[
    ("alice", "Analyst"),
    ("fred", "Auditor"),
    ("dana", "Admin"),
    ("erin", "Lead"),
];

/// Resources to probe. `io` is granted to no role — both engines must DENY it,
/// so it exercises agreement-on-false, not just agreement-on-true.
const RESOURCES: &[&str] = &["pure", "logs", "io"];

/// The source-of-truth decision the gate trusts.
///
/// `role`/`action`/`resource` are the spec-level coordinates of a request. The
/// only impl today is [`ShenSpecOracle`] (the served Shen VM); the trait exists
/// so a future SBCL evaluation of the same spec could be dropped in to
/// cross-check the shen-rust VM with no change to the gate loop.
trait Oracle {
    fn allows(&mut self, role: &str, action: &str, resource: &str) -> bool;
    fn name(&self) -> &str;
}

/// The artifact under test — the thing the oracle is checked against.
///
/// `user`/`action`/`resource` are the concrete request coordinates (a user, not
/// a role: membership is resolved inside the target). The only impl today is
/// [`CedarTarget`] (the generated Cedar `PolicySet`); the trait exists so a
/// future shengen→Rust compiled guard could be checked against the same oracle
/// with no change to the gate loop.
trait Target {
    fn allows(&mut self, user: &str, action: &str, resource: &str) -> bool;
    fn name(&self) -> &str;
}

/// [`Oracle`] backed by the served Shen VM's `expand-all` permit closure,
/// materialised as a set of `(role, action, resource)` triples.
struct ShenSpecOracle {
    name: String,
    permits: HashSet<(String, String, String)>,
}

impl Oracle for ShenSpecOracle {
    fn allows(&mut self, role: &str, action: &str, resource: &str) -> bool {
        self.permits
            .contains(&(role.to_string(), action.to_string(), resource.to_string()))
    }
    fn name(&self) -> &str {
        &self.name
    }
}

/// [`Target`] backed by the Cedar `Authorizer` over the generated `PolicySet`
/// and the fixture entities, schema-checked.
struct CedarTarget {
    name: String,
    authz: Authorizer,
    set: PolicySet,
    entities: Entities,
    schema: Schema,
}

impl Target for CedarTarget {
    fn allows(&mut self, user: &str, action: &str, resource: &str) -> bool {
        let req = Request::new(
            EntityUid::from_str(&format!("User::{user:?}")).unwrap(),
            EntityUid::from_str(&format!("Action::{action:?}")).unwrap(),
            EntityUid::from_str(&format!("ShenCap::{resource:?}")).unwrap(),
            Context::empty(),
            Some(&self.schema),
        )
        .expect("request");
        matches!(
            self.authz
                .is_authorized(&req, &self.set, &self.entities)
                .decision(),
            Decision::Allow
        )
    }
    fn name(&self) -> &str {
        &self.name
    }
}

fn main() {
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(run)
        .expect("spawn");
    std::process::exit(handle.join().unwrap_or(2));
}

fn run() -> i32 {
    let inject_drift = std::env::args().any(|a| a == "--inject-drift");

    let schema = match schema() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    // --- The oracle: evaluate the Shen spec in the served VM. ----------------
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
    eprintln!("ready (oracle: spec/authz.shen).");

    let grants_v = match host.call("expand-all", vec![]) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("expand-all: {e}");
            return 1;
        }
    };

    // The spec's permit closure as a set of (role, action, resource) triples,
    // plus the Cedar rendered from it (the target's source).
    let mut spec_permits: HashSet<(String, String, String)> = HashSet::new();
    let mut cedar = String::new();
    let mut rendered: Vec<String> = Vec::new();
    for triple in read_list(&grants_v) {
        let parts = read_list(&triple);
        if parts.len() != 3 {
            continue;
        }
        let (role, action, res) = (
            host.text(&parts[0]),
            host.text(&parts[1]),
            host.text(&parts[2]),
        );
        spec_permits.insert((role.clone(), action.clone(), res.clone()));
        let policy = format!(
            "permit(principal in Role::{role:?}, action == Action::{action:?}, resource == ShenCap::{res:?});"
        );
        if !rendered.contains(&policy) {
            // --- inject a codegen/hand-edit drift: drop Admin·logs ---------
            if inject_drift && role == "Admin" && res == "logs" {
                eprintln!("  (injected drift: dropping generated permit Admin·logs)");
                continue;
            }
            rendered.push(policy.clone());
            cedar.push_str(&policy);
            cedar.push('\n');
        }
    }

    let oracle = ShenSpecOracle {
        name: "spec/authz.shen".to_string(),
        permits: spec_permits,
    };

    // --- The target: parse + strict-validate the generated Cedar. -----------
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

    let entities_json = build_entities_json();
    let entities = match entities(&entities_json, &schema) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let target = CedarTarget {
        name: "Cedar PolicySet".to_string(),
        authz: Authorizer::new(),
        set,
        entities,
        schema: schema.clone(),
    };

    gate(oracle, target)
}

/// The gate: for every request in the finite space, the oracle and the target
/// must agree. Generic over [`Oracle`]/[`Target`] so the harness is not
/// hard-wired to (shen-rust VM × Cedar).
fn gate(mut oracle: impl Oracle, mut target: impl Target) -> i32 {
    println!("\n=== shen-derive: codegen-equivalence gate ===");
    println!("oracle = {}  (shen-rust VM: expand-all)", oracle.name());
    println!("target = {} generated from that spec", target.name());
    println!(
        "checking {} requests ({} users × {} resources, action Eval)\n",
        USERS.len() * RESOURCES.len(),
        USERS.len(),
        RESOURCES.len()
    );
    println!("  request               spec    cedar");
    println!("  ------------------------------------------");

    let mut divergences = 0;
    for (user, role) in USERS {
        for res in RESOURCES {
            let spec_allow = oracle.allows(role, "Eval", res);
            let cedar_allow = target.allows(user, "Eval", res);

            let agree = spec_allow == cedar_allow;
            if !agree {
                divergences += 1;
            }
            let mark = if agree { "✓" } else { "✗ DIVERGENCE" };
            println!(
                "  {:<20}  {:<6}  {:<6}  {mark}",
                format!("{user}({role})·{res}"),
                yn(spec_allow),
                yn(cedar_allow),
            );
        }
    }

    let n = USERS.len() * RESOURCES.len();
    println!();
    if divergences == 0 {
        println!("{n}/{n} agree — generated Cedar is equivalent to the spec ✓");
        0
    } else {
        println!(
            "{divergences}/{n} DIVERGE — generated Cedar does NOT decide like the spec ✗\n\
             the gate would fail the build: the Cedar drifted from spec/authz.shen."
        );
        1
    }
}

fn yn(b: bool) -> &'static str {
    if b {
        "ALLOW"
    } else {
        "DENY"
    }
}

/// Build the entities JSON for the fixture: each user is a member of its Role.
fn build_entities_json() -> String {
    let mut roles: Vec<&str> = USERS.iter().map(|(_, r)| *r).collect();
    roles.sort_unstable();
    roles.dedup();
    let users: Vec<String> = USERS
        .iter()
        .map(|(u, r)| {
            format!(
                r#"{{ "uid": {{"type":"User","id":"{u}"}}, "attrs": {{}}, "parents": [{{"type":"Role","id":"{r}"}}] }}"#
            )
        })
        .collect();
    let role_ents: Vec<String> = roles
        .iter()
        .map(|r| {
            format!(r#"{{ "uid": {{"type":"Role","id":"{r}"}}, "attrs": {{}}, "parents": [] }}"#)
        })
        .collect();
    format!("[{}]", [users, role_ents].concat().join(",\n"))
}
