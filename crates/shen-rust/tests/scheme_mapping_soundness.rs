//! Runtime soundness for Shen/Scheme-style mappings (Astra review).
//!
//! These cases are the ones 134/0 does not cover: trap-error that *uses* the
//! exception, operands that raise before the accessor, hash agreeing with `=`,
//! get miss messages, freeze captured by a lambda. Callees (hash/get/value)
//! are the native/AOT overrides; the enclosing form is tree-walked KL.

use std::path::PathBuf;

use shen_rust::error::ShenResult;
use shen_rust::interp::boot::boot_with_kernel;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

fn kernel_klambda_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("kernel");
    p.push("klambda");
    p
}

fn fresh_booted() -> Interp {
    let mut interp = Interp::new();
    boot_with_kernel(&mut interp, &kernel_klambda_dir())
        .unwrap_or_else(|e| panic!("kernel boot failed: {e}"));
    interp
}

fn eval_kl(interp: &mut Interp, src: &str) -> ShenResult<Value> {
    let e =
        parse_one(src, &mut interp.symbols).unwrap_or_else(|err| panic!("parse {src:?}: {err}"));
    interp.eval(&e)
}

fn ok(interp: &mut Interp, src: &str) -> Value {
    eval_kl(interp, src).unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
}

#[test]
fn trap_error_handler_receives_exception() {
    let mut i = fresh_booted();
    let v = ok(
        &mut i,
        r#"(trap-error (simple-error "boom") (lambda E (error-to-string E)))"#,
    );
    assert_eq!(v.as_str(), Some("boom"));
}

#[test]
fn trap_error_value_handler_uses_exn() {
    let mut i = fresh_booted();
    let v = ok(
        &mut i,
        r#"(trap-error (value definitely-unbound-xyzzy) (lambda E (error-to-string E)))"#,
    );
    let s = v.as_str().expect("string");
    assert!(
        s.contains("unbound") || s.contains("definitely-unbound-xyzzy"),
        "handler saw {s:?}"
    );
}

#[test]
fn trap_error_catches_operand_error() {
    let mut i = fresh_booted();
    let v = ok(
        &mut i,
        r#"(trap-error (<-vector (simple-error "boom") 1) (lambda E 42))"#,
    );
    assert_eq!(v.as_int(), Some(42));
}

#[test]
fn intern_true_is_bool_and_hashes_with_true() {
    let mut i = fresh_booted();
    assert_eq!(ok(&mut i, r#"(intern "true")"#).as_bool(), Some(true));
    assert_eq!(ok(&mut i, r#"(intern "false")"#).as_bool(), Some(false));
    assert_eq!(
        ok(&mut i, r#"(= true (intern "true"))"#).as_bool(),
        Some(true)
    );
    assert_eq!(
        ok(&mut i, r#"(hash true 1009)"#).as_int(),
        ok(&mut i, r#"(hash (intern "true") 1009)"#).as_int()
    );
    assert_eq!(
        ok(&mut i, "(hash 1 1009)").as_int(),
        ok(&mut i, "(hash 1.0 1009)").as_int()
    );
}

#[test]
fn get_missing_key_has_no_attributes() {
    let mut i = fresh_booted();
    let err = eval_kl(
        &mut i,
        "(get scheme-mapping-absent-key scheme-mapping-absent-prop (value *property-vector*))",
    )
    .expect_err("missing get must error");
    let s = err.to_string();
    assert!(
        s.contains("has no attributes") || s.contains("not found"),
        "unexpected get miss: {s}"
    );
}

#[test]
fn freeze_captured_by_lambda_still_thaws() {
    let mut i = fresh_booted();
    let v = ok(&mut i, "(let G (freeze 7) ((lambda Y (thaw G)) 0))");
    assert_eq!(v.as_int(), Some(7));
}

#[test]
fn put_under_true_get_under_intern_true() {
    let mut i = fresh_booted();
    ok(
        &mut i,
        r#"(put true scheme-mapping-prop 99 (value *property-vector*))"#,
    );
    assert_eq!(
        ok(
            &mut i,
            r#"(get (intern "true") scheme-mapping-prop (value *property-vector*))"#
        )
        .as_int(),
        Some(99)
    );
}
