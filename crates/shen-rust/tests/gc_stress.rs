//! GC Step 4 stress: many depth-0 collections under a tiny trigger floor on
//! a served-shaped workload (repeated `eval` requests against a long-lived
//! interpreter). This is the missed-root detector the one-shot kernel suite
//! cannot be: kernel-tests spends its wall inside a handful of activations,
//! so a whole run sees ~2 collections; here every iteration exits to depth 0
//! and the 4096-node floor forces collection every few iterations.
//!
//! Any uncovered root class — env tables, closure-cache constant pools,
//! result rooting at the funnel exit, conservative stack slots — manifests
//! as a wrong sum, a non-cons where a list should be, or (in debug builds) a
//! read of the 0xDEAD…-poisoned freed words.
//!
//! Lives in its own integration-test binary because it configures the GC
//! through the process-global `SHEN_RUST_GC` environment variable: nothing
//! else may run in this process. (On targets where the conservative scan is
//! unsupported the env var is refused, the heap stays grow-only, and this
//! test degrades to a plain correctness loop — still valid, just not a GC
//! test; the `collections > 0` assertion is gated accordingly.)

use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::{shen_eq, Value};

fn eval(interp: &mut Interp, src: &str) -> Value {
    let form = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    interp
        .eval(&form)
        .unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
}

#[test]
fn served_loop_with_aggressive_gc_is_correct_and_bounded() {
    // Tiny floor: collect whenever footprint grows ≥4096 nodes past the live
    // set. Must be set before the first Interp (checked in Interp::new).
    std::env::set_var("SHEN_RUST_GC", "4096");
    // Must match gc::stack::SCAN_SUPPORTED (incl. miri, where the env var is
    // refused and this test degrades to a plain correctness loop).
    let gc_active = cfg!(all(
        target_arch = "aarch64",
        any(target_os = "macos", target_os = "linux"),
        not(miri)
    ));

    let mut interp = Interp::new();
    eval(
        &mut interp,
        "(defun range (N ACC) (if (= N 0) ACC (range (- N 1) (cons N ACC))))",
    );
    eval(
        &mut interp,
        "(defun rev (XS ACC) (if (cons? XS) (rev (tl XS) (cons (hd XS) ACC)) ACC))",
    );
    eval(
        &mut interp,
        "(defun sum (XS ACC) (if (cons? XS) (sum (tl XS) (+ (hd XS) ACC)) ACC))",
    );
    // A long-lived global: every collection must keep it (env root) intact.
    eval(&mut interp, "(set keeper (range 100 ()))");

    for i in 0..2_000 {
        // Cons-heavy garbage, exercised through the full build/walk cycle.
        let v = eval(&mut interp, "(sum (rev (range 200 ()) ()) 0)");
        assert_eq!(
            v.as_int(),
            Some(20_100),
            "iteration {i}: list sum corrupted"
        );
        // Closure nodes (lambda creation + call) and blob nodes (strings).
        let w = eval(&mut interp, "((lambda X (+ X 1)) 41)");
        assert_eq!(
            w.as_int(),
            Some(42),
            "iteration {i}: lambda result corrupted"
        );
        let s = eval(&mut interp, "(cn \"abc\" (str 42))");
        assert!(
            shen_eq(&s, &Value::str("abc42".to_string())),
            "iteration {i}: string corrupted"
        );
    }

    // The global survived every collection with structure intact.
    let kept = eval(&mut interp, "(sum (value keeper) 0)");
    assert_eq!(kept.as_int(), Some(5_050), "long-lived global corrupted");

    let (collections, last_live, node_count) = interp.gc_stats();
    if gc_active {
        assert!(
            collections >= 10,
            "stress loop barely collected ({collections} collections) — \
             trigger or safepoint wiring regressed"
        );
        assert!(
            node_count < 1_000_000,
            "heap unbounded under collection: {node_count} nodes \
             (last_live {last_live})"
        );
    }
}
