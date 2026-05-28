//! End-to-end test of the Cedar bridge from Shen.
//!
//! We construct a tiny policy + entities + request, ask Shen to authorize
//! it, and check the decision matches what the Cedar SDK would return.

use shen_cedar::error::ShenResult;
use shen_cedar::interp::eval::Interp;
use shen_cedar::kl::parser::parse_one;
use shen_cedar::value::{shen_eq, Value};

fn run(src: &str) -> ShenResult<Value> {
    let mut interp = Interp::new();
    let expr = parse_one(src, &mut interp.symbols)?;
    interp.eval(&expr)
}

#[test]
fn parse_policy_returns_foreign() {
    let v = run(r#"(cedar.parse-policy "permit(principal, action, resource);")"#).unwrap();
    assert!(matches!(v, Value::Foreign(_)));
}

#[test]
fn parse_policy_set_returns_foreign() {
    let v = run(r#"(cedar.parse-policy-set "permit(principal, action, resource);")"#).unwrap();
    assert!(matches!(v, Value::Foreign(_)));
}

#[test]
fn empty_policy_set_denies_by_default() {
    let mut interp = Interp::new();
    let mut go = |src: &str| {
        let e = parse_one(src, &mut interp.symbols)
            .unwrap_or_else(|err| panic!("parse {src:?}: {err}"));
        interp
            .eval(&e)
            .unwrap_or_else(|err| panic!("eval {src:?}: {err}"))
    };
    go(r#"(set pset (cedar.empty-policy-set))"#);
    go(r#"(set ents (cedar.empty-entities))"#);
    go(r#"(set principal (cedar.make-entity-uid "User" "alice"))"#);
    go(r#"(set action (cedar.make-entity-uid "Action" "read"))"#);
    go(r#"(set resource (cedar.make-entity-uid "Doc" "d1"))"#);
    go(r#"(set req (cedar.make-request (value principal) (value action) (value resource) ()))"#);
    let v = go(r#"(cedar.is-authorized (value pset) (value ents) (value req))"#);
    let deny = interp.intern("Deny");
    assert!(
        shen_eq(&v, &Value::Sym(deny)),
        "empty policy set should Deny, got {v:?}"
    );
}

#[test]
fn permit_all_policy_allows() {
    // Build a 1-policy set that permits everything, then authorize.
    let mut interp = Interp::new();
    let mut go = |src: &str| {
        let e = parse_one(src, &mut interp.symbols)
            .unwrap_or_else(|err| panic!("parse {src:?}: {err}"));
        interp
            .eval(&e)
            .unwrap_or_else(|err| panic!("eval {src:?}: {err}"))
    };
    go(r#"(set p (cedar.parse-policy "permit(principal, action, resource);"))"#);
    go(r#"(set pset0 (cedar.empty-policy-set))"#);
    go(r#"(set pset (cedar.policy-set-add (value pset0) (value p)))"#);
    go(r#"(set ents (cedar.empty-entities))"#);
    go(r#"(set principal (cedar.make-entity-uid "User" "alice"))"#);
    go(r#"(set action (cedar.make-entity-uid "Action" "read"))"#);
    go(r#"(set resource (cedar.make-entity-uid "Doc" "d1"))"#);
    go(r#"(set req (cedar.make-request (value principal) (value action) (value resource) ()))"#);
    let v = go(r#"(cedar.is-authorized (value pset) (value ents) (value req))"#);
    let allow = interp.intern("Allow");
    assert!(shen_eq(&v, &Value::Sym(allow)), "expected Allow, got {v:?}");
}

#[test]
fn forbid_all_policy_denies() {
    let mut interp = Interp::new();
    let mut go = |src: &str| {
        let e = parse_one(src, &mut interp.symbols)
            .unwrap_or_else(|err| panic!("parse {src:?}: {err}"));
        interp
            .eval(&e)
            .unwrap_or_else(|err| panic!("eval {src:?}: {err}"))
    };
    go(r#"(set p (cedar.parse-policy "forbid(principal, action, resource);"))"#);
    go(r#"(set pset0 (cedar.empty-policy-set))"#);
    go(r#"(set pset (cedar.policy-set-add (value pset0) (value p)))"#);
    go(r#"(set ents (cedar.empty-entities))"#);
    go(r#"(set principal (cedar.make-entity-uid "User" "alice"))"#);
    go(r#"(set action (cedar.make-entity-uid "Action" "read"))"#);
    go(r#"(set resource (cedar.make-entity-uid "Doc" "d1"))"#);
    go(r#"(set req (cedar.make-request (value principal) (value action) (value resource) ()))"#);
    let v = go(r#"(cedar.is-authorized (value pset) (value ents) (value req))"#);
    let deny = interp.intern("Deny");
    assert!(shen_eq(&v, &Value::Sym(deny)), "expected Deny, got {v:?}");
}

#[test]
fn policy_to_string_roundtrip() {
    let mut interp = Interp::new();
    let mut go = |src: &str| {
        let e = parse_one(src, &mut interp.symbols)
            .unwrap_or_else(|err| panic!("parse {src:?}: {err}"));
        interp
            .eval(&e)
            .unwrap_or_else(|err| panic!("eval {src:?}: {err}"))
    };
    go(r#"(set p (cedar.parse-policy "permit(principal, action, resource);"))"#);
    let v = go(r#"(cedar.policy->string (value p))"#);
    if let Value::Str(s) = v {
        // Cedar formatter may add whitespace/newlines, just check the keyword.
        assert!(s.contains("permit"), "expected 'permit' in {s:?}");
    } else {
        panic!("expected Str, got {v:?}");
    }
}
