//! JIT Win A differential oracle — the named self-tail `return_call` edge.
//!
//! W1 makes a SELF-call in TRUE TAIL position of a NAMED top-level defun emit a
//! native `return_call` to the body's own `FuncId` (mirroring
//! `define_length_h_tail`), instead of the `rtj_apply_named` FFI hop. This test
//! drives `shen-w1-sumto` through BOTH a tree-walked reference and the
//! JIT-installed subject across a corpus and asserts `shen_eq` + `Ok`/`Err`
//! parity, plus a positive ran-signal that the fn actually compiled to native
//! code (not silently bailed).
//!
//! Run: `cargo test --features jit --test jit_winA_differential`.
#![cfg(feature = "jit")]

use shen_rust::aot::runtime as rt;
use shen_rust::interp::eval::Interp;
use shen_rust::jit;
use shen_rust::kl::parser::parse_one;
use shen_rust::symbol::SymId;
use shen_rust::value::{shen_eq, Value};

/// `FIXNUM_MAX` (`(1 << 60) - 1`), mirrored from `value.rs`.
const FIXNUM_MAX: i64 = (1 << 60) - 1;

fn body() {
    // Tier-in is env-gated; set it FIRST (install_jit early-returns if unset).
    // SAFETY: single-threaded setup before any interpreter exists.
    std::env::set_var("SHEN_RUST_JIT", "1");

    // Reference: tree-walked defun, NO install_jit.
    let mut refi = Interp::new();
    let dref = parse_one(
        "(defun shen-w1-sumto (N ACC) (if (= N 0) ACC (shen-w1-sumto (- N 1) (+ ACC N))))",
        &mut refi.symbols,
    )
    .unwrap();
    refi.eval(&dref).unwrap();

    // Subject: install_jit defines the defun AND overrides the direct slot with
    // jit_shim_w1_sumto (the native return_call body).
    let mut jiti = Interp::new();
    jit::install_jit(&mut jiti);
    assert!(jiti.jit_active(), "jit engine should be installed");
    assert!(
        jiti.jit_named_compiled("shen-w1-sumto"),
        "W1 fn must compile to native (not bail)"
    );

    // Corpus: args = [Value::int(n), Value::int(acc0)] unless noted.
    let corpus: Vec<(&str, Vec<Value>)> = vec![
        ("base", vec![Value::int(0), Value::int(0)]),
        ("tiny", vec![Value::int(5), Value::int(0)]),
        ("mid", vec![Value::int(100), Value::int(0)]),
        ("k1000", vec![Value::int(1000), Value::int(0)]),
        // LOAD-BEARING TCO: a non-return_call build stack-overflows here.
        ("deep100k", vec![Value::int(100_000), Value::int(0)]),
        // fixnum→float promotion: first few (+ ACC N) cross 2^60-1.
        ("overflow", vec![Value::int(10), Value::int(FIXNUM_MAX - 5)]),
        // (= N 0)/(- N 1) error via rtj_* ⇒ errflag ⇒ Err on both.
        ("err_nonnum", vec![Value::sym(SymId(123)), Value::int(0)]),
    ];

    for (label, args) in &corpus {
        let want = rt::apply_direct(&mut refi, "shen-w1-sumto", args);
        let got = rt::apply_direct(&mut jiti, "shen-w1-sumto", args);
        match (&want, &got) {
            (Ok(a), Ok(b)) => assert!(shen_eq(a, b), "{label}: jit {b:?} != tree {a:?}"),
            (Err(_), Err(_)) => {}
            _ => panic!("{label}: ok/err mismatch want={want:?} got={got:?}"),
        }
    }

    // Spot-check the expected exact values (non-vacuity of the corpus).
    let mid = rt::apply_direct(
        &mut jiti,
        "shen-w1-sumto",
        &[Value::int(100), Value::int(0)],
    )
    .unwrap();
    assert!(shen_eq(&mid, &Value::int(5050)), "mid = {mid:?}");

    // The shim is strict on arity (the tree-walked reference curries under-
    // application, so this is asserted on the JIT shim directly, not diffed).
    assert!(
        rt::apply_direct(&mut jiti, "shen-w1-sumto", &[Value::int(3)]).is_err(),
        "shim must reject 1 arg"
    );
    assert!(
        rt::apply_direct(
            &mut jiti,
            "shen-w1-sumto",
            &[Value::int(3), Value::int(0), Value::int(0)]
        )
        .is_err(),
        "shim must reject 3 args"
    );
}

#[test]
fn w1_sumto_jit_matches_tree() {
    // The tree-walked reference recurses deeply on deep100k; the JIT side is
    // constant-stack. Run on a big stack so the reference doesn't overflow.
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(body)
        .unwrap()
        .join()
        .unwrap();
}
