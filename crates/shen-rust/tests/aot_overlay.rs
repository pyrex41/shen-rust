//! Overlay install API: the verified production path must install only
//! when the artifact matches the live world, and must leave the loaded
//! engine serving (no partial installs) on any mismatch.

use std::path::PathBuf;

use shen_rust::aot::generated;
use shen_rust::aot::overlay::{fnv64, kernel_digest, OverlayModule, OVERLAY_FORMAT};
use shen_rust::aot::runtime as rt;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

const LIVE_SRC: &str = "(defun fact ...) (defun loop-sum ...) (defun double ...)";

fn kernel_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.join("kernel").join("klambda")
}

fn run(interp: &mut Interp, src: &str) {
    let e = parse_one(src, &mut interp.symbols).unwrap();
    interp.eval(&e).unwrap();
}

fn fresh_with_defuns() -> Interp {
    let mut interp = Interp::new();
    run(
        &mut interp,
        "(defun fact (N) (if (= N 0) 1 (* N (fact (- N 1)))))",
    );
    run(
        &mut interp,
        "(defun loop-sum (N ACC) (if (= N 0) ACC (loop-sum (- N 1) (+ ACC 1))))",
    );
    run(&mut interp, "(defun double (X) (* X 2))");
    interp
}

fn module(source_fnv: u64, kernel_fnv: u64) -> OverlayModule {
    OverlayModule {
        label: "test-overlay",
        format: OVERLAY_FORMAT,
        source_fnv,
        kernel_fnv,
        compiled: &[("fact", 1), ("loop-sum", 2), ("double", 1)],
        install: generated::install,
    }
}

#[test]
fn if_match_installs_on_full_match() {
    let mut interp = fresh_with_defuns();
    let kd = kernel_digest(&kernel_dir());
    let m = module(fnv64(LIVE_SRC.as_bytes()), kd);
    assert!(interp.install_overlay_if_match(&m, LIVE_SRC, &kernel_dir()));
    let sym = interp.intern("double");
    assert!(
        interp.get_aot_direct(sym).is_some(),
        "direct slot populated"
    );
    let v = rt::apply_direct(&mut interp, "double", &[Value::int(21)]).unwrap();
    assert_eq!(v.as_int(), Some(42));
}

#[test]
fn if_match_refuses_on_source_drift() {
    let mut interp = fresh_with_defuns();
    let kd = kernel_digest(&kernel_dir());
    let m = module(fnv64(b"some other source"), kd);
    assert!(!interp.install_overlay_if_match(&m, LIVE_SRC, &kernel_dir()));
    let sym = interp.intern("double");
    assert!(interp.get_aot_direct(sym).is_none(), "nothing installed");
}

#[test]
fn if_match_refuses_on_kernel_drift() {
    let mut interp = fresh_with_defuns();
    let m = module(fnv64(LIVE_SRC.as_bytes()), 0xdead_beef);
    assert!(!interp.install_overlay_if_match(&m, LIVE_SRC, &kernel_dir()));
}

#[test]
fn install_overlay_refuses_when_names_not_loaded() {
    // The .shen load this overlay was generated from never happened:
    // all-or-nothing refusal, loaded engine keeps serving.
    let mut interp = Interp::new();
    let m = module(0, 0);
    let receipt = interp.install_overlay(&m, false).unwrap();
    assert!(!receipt.installed);
    assert_eq!(receipt.mismatches.len(), 3);
    assert!(interp.install_overlay(&m, true).is_err(), "strict errors");
}

#[test]
fn install_overlay_refuses_on_arity_mismatch() {
    let mut interp = fresh_with_defuns();
    // Redefine double with the wrong arity: the loaded source no longer
    // matches generation time.
    run(&mut interp, "(defun double (X Y) (* X Y))");
    let m = module(0, 0);
    let receipt = interp.install_overlay(&m, false).unwrap();
    assert!(!receipt.installed);
    assert_eq!(receipt.mismatches.len(), 1);
    assert!(receipt.mismatches[0].contains("double"));
    let sym = interp.intern("fact");
    assert!(
        interp.get_aot_direct(sym).is_none(),
        "all-or-nothing: peers not installed either"
    );
}
