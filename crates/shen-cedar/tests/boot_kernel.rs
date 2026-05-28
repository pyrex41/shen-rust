//! Phase 2 integration test: boot the full ShenOSKernel-41.1 and verify a
//! handful of expressions evaluate correctly via the kernel's own eval
//! pipeline (not raw `eval-kl`).
//!
//! Mirrors the "Verified expressions" list in
//! `shen-ocaml/STATUS.md`. If the kernel loads cleanly but an expression
//! diverges, narrow the failure to the specific primitive.

use std::path::PathBuf;

use shen_cedar::interp::boot::boot_with_kernel;
use shen_cedar::interp::eval::Interp;
use shen_cedar::kl::parser::parse_one;
use shen_cedar::value::Value;

fn kernel_klambda_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // workspace root
    p.push("kernel");
    p.push("klambda");
    p
}

fn fresh_booted() -> Interp {
    let mut interp = Interp::new();
    let dir = kernel_klambda_dir();
    boot_with_kernel(&mut interp, &dir).unwrap_or_else(|e| panic!("kernel boot failed: {e}"));
    interp
}

fn eval(interp: &mut Interp, src: &str) -> Value {
    let expr = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    interp
        .eval(&expr)
        .unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
}

#[test]
fn kernel_boots_clean() {
    // Just running boot is the test — it has to load all 21 files and
    // shen.initialise without raising.
    let _ = fresh_booted();
}

#[test]
fn version_global_is_set() {
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(value *version*)");
    if let Value::Str(s) = v {
        assert_eq!(&*s, "41.1");
    } else {
        panic!("expected string, got {v:?}");
    }
}

#[test]
fn implementation_global() {
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(value *implementation*)");
    if let Value::Str(s) = v {
        assert_eq!(&*s, "shen-cedar");
    } else {
        panic!("expected string, got {v:?}");
    }
}

#[test]
fn simple_arithmetic_post_boot() {
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(+ 1 1)");
    assert!(matches!(v, Value::Int(2)));
}

#[test]
fn let_binds_post_boot() {
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(let X 5 (+ X 1))");
    assert!(matches!(v, Value::Int(6)));
}

#[test]
fn cons_hd_post_boot() {
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(hd (cons 1 (cons 2 ())))");
    assert!(matches!(v, Value::Int(1)));
}

#[test]
fn defun_and_call_post_boot() {
    let mut interp = fresh_booted();
    eval(&mut interp, "(defun double (X) (* X 2))");
    let v = eval(&mut interp, "(double 21)");
    assert!(matches!(v, Value::Int(42)));
}

#[test]
fn trap_error_post_boot() {
    let mut interp = fresh_booted();
    let v = eval(
        &mut interp,
        "(trap-error (simple-error \"boom\") (lambda E (error-to-string E)))",
    );
    if let Value::Str(s) = v {
        assert_eq!(&*s, "boom");
    } else {
        panic!("expected string, got {v:?}");
    }
}

#[test]
fn kernel_eval_pipeline_runs() {
    // `eval` is the kernel's Shen-level evaluator
    // (`eval-kl (shen.shen->kl X)`). Going through it exercises
    // macro expansion + process-applications, which is the real bar.
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(eval (cons + (cons 1 (cons 1 ()))))");
    assert!(matches!(v, Value::Int(2)));
}

#[test]
fn fn_lookup_post_metadata() {
    // After register_all_metadata, `(fn +)` should return the closure.
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(fn +)");
    assert!(matches!(v, Value::Closure(_)));
}
