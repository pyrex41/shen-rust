//! AOT pipeline smoke test.
//!
//! Defines `fact`, `loop-sum`, `double` via tree-walker `defun`, then
//! installs the AOT-compiled versions over the top and verifies they
//! return the same results. Also measures relative throughput on a
//! hot-loop benchmark.

use std::time::Instant;

use shen_rust::aot::generated;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

fn run(interp: &mut Interp, src: &str) -> Value {
    let e =
        parse_one(src, &mut interp.symbols).unwrap_or_else(|err| panic!("parse {src:?}: {err}"));
    interp
        .eval(&e)
        .unwrap_or_else(|err| panic!("eval {src:?}: {err}"))
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

#[test]
fn aot_factorial_matches_treewalker() {
    let mut tw = fresh_with_defuns();
    let tw_result = run(&mut tw, "(fact 10)");

    let mut aot = fresh_with_defuns();
    generated::install(&mut aot);
    let aot_result = run(&mut aot, "(fact 10)");

    assert!(
        (tw_result.as_int() == Some(3628800)),
        "tree-walker fact(10) was {tw_result:?}"
    );
    assert!(
        (aot_result.as_int() == Some(3628800)),
        "AOT fact(10) was {aot_result:?}"
    );
}

#[test]
fn aot_loop_sum_matches_treewalker() {
    let mut tw = fresh_with_defuns();
    let tw_result = run(&mut tw, "(loop-sum 1000 0)");

    let mut aot = fresh_with_defuns();
    generated::install(&mut aot);
    let aot_result = run(&mut aot, "(loop-sum 1000 0)");

    assert!((tw_result.as_int() == Some(1000)));
    assert!((aot_result.as_int() == Some(1000)));
}

#[test]
fn aot_double_matches_treewalker() {
    let mut tw = fresh_with_defuns();
    let tw_result = run(&mut tw, "(double 21)");

    let mut aot = fresh_with_defuns();
    generated::install(&mut aot);
    let aot_result = run(&mut aot, "(double 21)");

    assert!((tw_result.as_int() == Some(42)));
    assert!((aot_result.as_int() == Some(42)));
}

/// Crude throughput comparison — not a rigorous benchmark, just a sanity
/// check that AOT isn't catastrophically slower. We run a tight 100k
/// iteration of `(loop-sum 50 0)` on each backend and report ratio.
///
/// The test asserts only the AOT result equals the tree-walker; the
/// timings are printed for inspection (`cargo test -- --nocapture`).
#[test]
fn aot_perf_smoke() {
    const RUNS: u32 = 5000;

    let mut tw = fresh_with_defuns();
    let parsed_tw = parse_one("(loop-sum 50 0)", &mut tw.symbols).unwrap();
    let t0 = Instant::now();
    let mut last_tw = Value::nil();
    for _ in 0..RUNS {
        last_tw = tw.eval(&parsed_tw).unwrap();
    }
    let tw_elapsed = t0.elapsed();

    let mut aot = fresh_with_defuns();
    generated::install(&mut aot);
    let parsed_aot = parse_one("(loop-sum 50 0)", &mut aot.symbols).unwrap();
    let t0 = Instant::now();
    let mut last_aot = Value::nil();
    for _ in 0..RUNS {
        last_aot = aot.eval(&parsed_aot).unwrap();
    }
    let aot_elapsed = t0.elapsed();

    let ratio = tw_elapsed.as_secs_f64() / aot_elapsed.as_secs_f64();
    eprintln!(
        "aot_perf_smoke: {RUNS} iterations of (loop-sum 50 0)\n  tree-walker: {tw_elapsed:?}\n  AOT:         {aot_elapsed:?}\n  speedup:     {ratio:.2}x"
    );

    assert!((last_tw.as_int() == Some(50)));
    assert!((last_aot.as_int() == Some(50)));
}
