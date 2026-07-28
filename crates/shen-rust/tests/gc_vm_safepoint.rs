//! GC mid-run VM safepoint stress (the script-workload extension of Step 4).
//!
//! `gc_stress.rs` exercises depth-0 collections on a served-shaped loop —
//! many small `eval`s, each returning to activation depth 0. This test is
//! the OTHER workload shape, the one that motivated the mid-run safepoint:
//! a script whose entire wall lives inside **one** toplevel evaluation
//! (urdr's replay/prng suites are exactly this), where the depth-0 funnel
//! safepoint is unreachable until the run is over and a grow-only heap
//! balloons to the run's total allocation volume.
//!
//! With the VM enabled, the dispatch loop's hazard-gated safepoint
//! (`vm::exec::maybe_collect_mid_run`) must fire *inside* the single
//! activation, sweep the cons garbage, and keep the heap bounded — without
//! corrupting the live lists, the long-lived global, closures, or strings
//! (payload-bearing node kinds are retained mid-run by design).
//!
//! Lives in its own integration-test binary: it configures the GC through
//! the process-global `SHEN_RUST_GC` env var and flips the process-global
//! VM switch, so nothing else may run in this process. (Where the
//! conservative scan is unsupported — non-aarch64, miri — the env var is
//! refused and this degrades to a plain correctness run; the collection
//! assertions are gated accordingly.)

use shen_rust::interp::eval::{enable_vm, Interp};
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

fn eval(interp: &mut Interp, src: &str) -> Value {
    let form = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    interp
        .eval(&form)
        .unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
}

#[test]
fn single_activation_vm_run_collects_and_stays_bounded() {
    // Small trigger floor so the single long run requests many collections.
    // Must be set before the first Interp (checked in Interp::new).
    std::env::set_var("SHEN_RUST_GC", "65536");
    // The mid-run safepoint lives in the VM dispatch loop; compile the
    // defuns below to bytecode. Must run before any closure is built.
    enable_vm();
    // Must match gc::stack::SCAN_SUPPORTED (incl. miri, where the env var
    // is refused and this test degrades to a plain correctness run).
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
    // One iteration of cons churn: build, reverse, sum, drop. ~800 nodes of
    // garbage per call.
    eval(
        &mut interp,
        "(defun churn (I ACC) (if (= I 0) ACC \
           (churn (- I 1) (+ ACC (sum (rev (range 400 ()) ()) 0)))))",
    );
    // A long-lived global (env root): every mid-run collection must keep it
    // — and its cons structure — intact.
    eval(&mut interp, "(set keeper (range 100 ()))");

    let (before_collections, _, _) = interp.gc_stats();

    // THE single activation: ~2.4M nodes of cons garbage inside one eval.
    // Grow-only (or a safepoint that never fires) means node_count ends
    // ≈2.4M; a working mid-run safepoint keeps it near the 64Ki floor.
    // sum(1..=400) = 80200; 3000 iterations → 240,600,000.
    let v = eval(&mut interp, "(churn 3000 0)");
    assert_eq!(v.as_int(), Some(240_600_000), "churn sum corrupted");

    let (mid_collections, _, node_count_inside) = interp.gc_stats();
    // The keeper list survived the mid-run collections with structure intact.
    let kept = eval(&mut interp, "(sum (value keeper) 0)");
    assert_eq!(kept.as_int(), Some(5_050), "long-lived global corrupted");
    // Closure + string nodes (retained kinds) still work end to end.
    let w = eval(&mut interp, "((lambda X (+ X 1)) 41)");
    assert_eq!(w.as_int(), Some(42), "lambda result corrupted");
    let s = eval(&mut interp, "(cn \"abc\" (str 42))");
    assert!(
        shen_rust::value::shen_eq(&s, &Value::str("abc42".to_string())),
        "string corrupted"
    );

    if gc_active {
        assert!(
            mid_collections - before_collections >= 5,
            "the single-activation run barely collected \
             ({before_collections} → {mid_collections}) — the VM mid-run \
             safepoint did not fire"
        );
        assert!(
            node_count_inside < 1_000_000,
            "heap unbounded across the single activation: \
             {node_count_inside} nodes (≈2.4M would mean grow-only)"
        );
    }
}
