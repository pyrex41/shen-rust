//! JIT differential oracle — stage J1 (`design/jit-j1-handoff.md` §3f).
//!
//! For a corpus of inputs to `shen.length-h` we assert the JIT'd result is
//! `shen_eq`-equal **and** `Ok`/`Err`-matching to the interpreted result. This
//! is the gate that lets the JIT path go live without trusting the hand-written
//! Cranelift IR blindly: any divergence — a missing tag-check, a wrong overflow
//! edge, a mishandled `tl` error — fails CI.
//!
//! **Reference**: the AOT-compiled `shen.length-h` (`aot::kernel::sys`), which
//! is *exactly* the implementation the JIT replaces, dispatched through the
//! same AOT direct-call table (so it recurses correctly and shares the call
//! contract — full arity, no currying). Two independent implementations of the
//! same KL function: the Rust AOT codegen and the Cranelift IR. (AOT≡tree-walk
//! equivalence is separately gated by kernel-tests / kernel-aot-audit, so AOT
//! is a sound stand-in for "the interpreter" here.)
//!
//! The JIT is reached through the *real* public tier-in: `SHEN_RUST_JIT` set →
//! `jit::install_jit` builds the engine and registers the shim in the direct
//! table → `rt::apply_direct("shen.length-h", ..)` dispatches to it, exactly
//! the path a booted kernel takes.
//!
//! The whole file is gated on the `jit` feature, so a default build compiles it
//! away to nothing.
#![cfg(feature = "jit")]

use shen_rust::aot::kernel::sys::aot_shen_x2e_length_x2d_h;
use shen_rust::aot::runtime as rt;
use shen_rust::interp::eval::Interp;
use shen_rust::jit;
use shen_rust::symbol::SymId;
use shen_rust::value::{shen_eq, Value};

/// Build an improper list `(e0 e1 ... ek . tail)` — a chain of conses ending in
/// `tail` rather than `nil`.
fn improper(elems: &[Value], tail: Value) -> Value {
    let mut acc = tail;
    for e in elems.iter().rev() {
        acc = Value::cons(*e, acc);
    }
    acc
}

#[test]
fn length_h_jit_matches_aot() {
    // Tier-in is env-gated; set it before building the JIT interpreter. This
    // test file is its own binary, so the var is isolated to this process.
    // SAFETY: single-threaded setup before any interpreter exists; edition 2021
    // `set_var` is safe.
    std::env::set_var("SHEN_RUST_JIT", "1");

    // Reference interpreter: register the AOT `shen.length-h` in the direct
    // table so its self-recursion resolves to itself (pure AOT, no JIT).
    let mut refi = Interp::new();
    refi.register_aot_direct("shen.length-h", aot_shen_x2e_length_x2d_h);

    // JIT interpreter: install the engine + shim through the public path. No
    // full kernel boot needed — `install_jit` registers the shim directly.
    let mut jiti = Interp::new();
    jit::install_jit(&mut jiti);
    assert!(jiti.jit_active(), "jit engine should be installed");

    // Build a varied corpus on the shared thread-local heap (both interpreters
    // run on this thread, so the same `Value`s are valid in each).
    let ints = |n: i64| (0..n).map(Value::int);
    let nested = Value::list([
        Value::list([Value::int(1), Value::int(2)]),
        Value::list([Value::int(3)]),
        Value::nil(),
    ]);

    let corpus: Vec<(&str, Vec<Value>)> = vec![
        // --- happy path: proper lists, accumulator starting at 0 -----------
        ("empty", vec![Value::nil(), Value::int(0)]),
        ("one", vec![Value::list(ints(1)), Value::int(0)]),
        ("three", vec![Value::list(ints(3)), Value::int(0)]),
        ("thousand", vec![Value::list(ints(1000)), Value::int(0)]),
        // deep — exercises the `return_call` TCO at constant stack
        ("deep_100k", vec![Value::list(ints(100_000)), Value::int(0)]),
        // nested elements: length counts only the spine
        ("nested", vec![nested, Value::int(0)]),
        // --- non-zero / non-canonical accumulators -------------------------
        ("acc_5", vec![Value::list(ints(2)), Value::int(5)]),
        ("acc_neg", vec![Value::list(ints(4)), Value::int(-3)]),
        // --- error paths: must Err identically -----------------------------
        // improper list: `tl` eventually hits a non-cons
        (
            "improper_int_tail",
            vec![
                improper(&[Value::int(1), Value::int(2)], Value::int(7)),
                Value::int(0),
            ],
        ),
        // first arg is a non-nil non-cons → immediate `tl` error
        ("first_arg_int", vec![Value::int(5), Value::int(0)]),
        ("first_arg_sym", vec![Value::sym(SymId(123)), Value::int(0)]),
        // non-number accumulator on a non-empty list → `add` error
        ("acc_string", vec![Value::list(ints(1)), Value::str("x")]),
        // --- arity guard (both reject under-/over-application identically) --
        ("arity_1", vec![Value::nil()]),
        ("arity_3", vec![Value::nil(), Value::int(0), Value::int(0)]),
    ];

    for (label, args) in &corpus {
        let want = rt::apply_direct(&mut refi, "shen.length-h", args);
        let got = rt::apply_direct(&mut jiti, "shen.length-h", args);

        match (&want, &got) {
            (Ok(a), Ok(b)) => assert!(
                shen_eq(a, b),
                "[{label}] JIT result {b:?} != reference {a:?}"
            ),
            (Err(_), Err(_)) => { /* both errored — Ok/Err status matches */ }
            (Ok(a), Err(e)) => panic!("[{label}] reference Ok({a:?}) but JIT Err({e:?})"),
            (Err(e), Ok(b)) => panic!("[{label}] reference Err({e:?}) but JIT Ok({b:?})"),
        }
    }
}
