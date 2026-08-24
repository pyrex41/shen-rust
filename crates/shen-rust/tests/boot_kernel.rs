//! Phase 2 integration test: boot the full ShenOSKernel-41.2 and verify a
//! handful of expressions evaluate correctly via the kernel's own eval
//! pipeline (not raw `eval-kl`).
//!
//! Mirrors the "Verified expressions" list in
//! `shen-ocaml/STATUS.md`. If the kernel loads cleanly but an expression
//! diverges, narrow the failure to the specific primitive.

use std::path::PathBuf;

use shen_rust::interp::boot::boot_with_kernel;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

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
    // Just running boot is the test — it has to load every kernel file
    // (incl. the S41.2-refresh self-initialising declarations.kl) without
    // raising.
    let _ = fresh_booted();
}

#[test]
fn version_global_is_set() {
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(value *version*)");
    if let Some(s) = v.as_str() {
        assert_eq!(s, "41.2");
    } else {
        panic!("expected string, got {v:?}");
    }
}

#[test]
fn implementation_global() {
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(value *implementation*)");
    if let Some(s) = v.as_str() {
        assert_eq!(s, "shen-rust");
    } else {
        panic!("expected string, got {v:?}");
    }
}

#[test]
fn simple_arithmetic_post_boot() {
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(+ 1 1)");
    assert!((v.as_int() == Some(2)));
}

#[test]
fn let_binds_post_boot() {
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(let X 5 (+ X 1))");
    assert!((v.as_int() == Some(6)));
}

#[test]
fn cons_hd_post_boot() {
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(hd (cons 1 (cons 2 ())))");
    assert!((v.as_int() == Some(1)));
}

#[test]
fn shen_batteries_features_query_reports_sha_host() {
    let mut interp = fresh_booted();
    let value = eval(&mut interp, "(shen.x.features.current)");
    let pure = std::env::var("SHEN_X_SHA256").ok().as_deref() == Some("pure");
    if pure {
        assert!(value.is_nil(), "pure SHA mode must advertise no host feature: {value:?}");
    } else {
        let feature = value
            .head()
            .and_then(|v| v.as_sym())
            .map(|s| interp.resolve(s).to_string());
        assert_eq!(feature.as_deref(), Some("shen.x/sha256-host"));
        assert!(value.tail().is_some_and(|tail| tail.is_nil()));

        // AOT-generated Batteries code uses the direct table. Verify the
        // feature query is installed there as well as in the live closure
        // namespace.
        let sym = interp.intern("shen.x.features.current");
        let direct = interp.get_aot_direct(sym).expect("AOT feature query");
        let direct_value = direct(&mut interp, &[]).expect("feature query succeeds");
        assert_eq!(direct_value.head().and_then(|v| v.as_sym()).map(|s| interp.resolve(s).to_string()),
                   Some("shen.x/sha256-host".to_string()));
    }
}

#[test]
fn defun_and_call_post_boot() {
    let mut interp = fresh_booted();
    eval(&mut interp, "(defun double (X) (* X 2))");
    let v = eval(&mut interp, "(double 21)");
    assert!((v.as_int() == Some(42)));
}

#[test]
fn trap_error_post_boot() {
    let mut interp = fresh_booted();
    let v = eval(
        &mut interp,
        "(trap-error (simple-error \"boom\") (lambda E (error-to-string E)))",
    );
    if let Some(s) = v.as_str() {
        assert_eq!(s, "boom");
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
    assert!((v.as_int() == Some(2)));
}

#[test]
fn fn_lookup_post_metadata() {
    // After register_all_metadata, `(fn +)` should return the closure.
    let mut interp = fresh_booted();
    let v = eval(&mut interp, "(fn +)");
    assert!((v.is_closure()));
}
