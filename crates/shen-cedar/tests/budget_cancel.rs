//! Tests for the instruction-count / wall-clock cancellation budget
//! added to the evaluator (`Interp::set_budget` / `set_deadline`).
//!
//! The per-tick CPU budget the correlator needs (see otelcheck's
//! `design/deployment-topology.md`) requires `eval` to be cancelable
//! *mid-evaluation*, not just abortable between top-level calls.

use shen_cedar::error::ShenResult;
use shen_cedar::interp::eval::Interp;
use shen_cedar::kl::parser::parse_one;
use shen_cedar::value::Value;

/// Define a Shen-level countdown loop that recurses `n` times. It compiles
/// to the tree-walked `Lambda` path (the VM punts on jumps/recursion), so
/// the loop body burns steps through `eval_in`'s trampoline.
fn define_spin(interp: &mut Interp) {
    let src = "(defun spin (N) (if (= N 0) done (spin (- N 1))))";
    let e = parse_one(src, &mut interp.symbols).expect("parse defun");
    interp.eval(&e).expect("define spin");
}

fn eval(interp: &mut Interp, src: &str) -> ShenResult<Value> {
    let e = parse_one(src, &mut interp.symbols).expect("parse");
    interp.eval(&e)
}

#[test]
fn unbounded_eval_completes() {
    let mut interp = Interp::new();
    define_spin(&mut interp);
    let v = eval(&mut interp, "(spin 5000)").expect("should complete unbounded");
    assert!(matches!(v, Value::Sym(_)), "expected `done`, got {v:?}");
}

#[test]
fn budget_cancels_long_eval() {
    let mut interp = Interp::new();
    define_spin(&mut interp);
    interp.set_budget(1_000);
    let err = eval(&mut interp, "(spin 1000000)").expect_err("should be cancelled");
    assert!(err.is_cancelled(), "expected cancellation, got {err:?}");
    assert!(
        err.to_string().contains("budget"),
        "message should mention the budget: {err}"
    );
}

#[test]
fn clear_budget_restores_unbounded() {
    let mut interp = Interp::new();
    define_spin(&mut interp);
    interp.set_budget(10);
    assert!(eval(&mut interp, "(spin 1000000)").is_err());
    interp.clear_budget();
    let v = eval(&mut interp, "(spin 5000)").expect("should complete after clear");
    assert!(matches!(v, Value::Sym(_)));
}

#[test]
fn cancellation_propagates_past_trap_error() {
    // `trap-error` must NOT swallow a budget cancellation — the scheduler
    // has to see the abort. The handler returns `caught`; if cancellation
    // leaked into it we'd get `caught` instead of an error.
    let mut interp = Interp::new();
    define_spin(&mut interp);
    interp.set_budget(1_000);
    let err = eval(&mut interp, "(trap-error (spin 1000000) (lambda E caught))")
        .expect_err("cancellation must propagate past trap-error");
    assert!(err.is_cancelled(), "expected cancellation, got {err:?}");
}

#[test]
fn ordinary_errors_are_not_cancellations() {
    // A genuine Shen error has kind Normal and is_cancelled() == false.
    let mut interp = Interp::new();
    let err = eval(&mut interp, "(simple-error \"boom\")").expect_err("should error");
    assert!(
        !err.is_cancelled(),
        "ordinary error misclassified as cancelled"
    );
}
