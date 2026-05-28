//! Smoke tests for the KL evaluator — arithmetic, conditionals, closures,
//! recursion, partial application, and `trap-error`.

use shen_cedar::error::ShenResult;
use shen_cedar::interp::eval::Interp;
use shen_cedar::kl::parser::parse_one;
use shen_cedar::value::Value;

fn run(src: &str) -> ShenResult<Value> {
    let mut interp = Interp::new();
    let e = parse_one(src, &mut interp.symbols)?;
    interp.eval(&e)
}

fn run_in(interp: &mut Interp, src: &str) -> ShenResult<Value> {
    let e = parse_one(src, &mut interp.symbols)?;
    interp.eval(&e)
}

#[test]
fn arithmetic() {
    assert!(matches!(run("(+ 1 1)").unwrap(), Value::Int(2)));
    assert!(matches!(run("(- 10 3)").unwrap(), Value::Int(7)));
    assert!(matches!(run("(* 6 7)").unwrap(), Value::Int(42)));
    assert!(matches!(run("(+ 1 (* 2 3))").unwrap(), Value::Int(7)));
}

#[test]
fn comparisons() {
    assert!(matches!(run("(> 5 3)").unwrap(), Value::Bool(true)));
    assert!(matches!(run("(< 5 3)").unwrap(), Value::Bool(false)));
    assert!(matches!(run("(>= 5 5)").unwrap(), Value::Bool(true)));
}

#[test]
fn equality_across_int_float() {
    assert!(matches!(run("(= 1 1.0)").unwrap(), Value::Bool(true)));
    assert!(matches!(run("(= 1 2)").unwrap(), Value::Bool(false)));
}

#[test]
fn cons_hd_tl() {
    assert!(matches!(
        run("(hd (cons 1 (cons 2 ())))").unwrap(),
        Value::Int(1)
    ));
    // (tl (cons 1 ())) → ()
    assert!(matches!(run("(tl (cons 1 ()))").unwrap(), Value::Nil));
}

#[test]
fn let_binds() {
    assert!(matches!(run("(let X 5 (+ X 1))").unwrap(), Value::Int(6)));
    // Nested let with the same name — innermost wins.
    assert!(matches!(
        run("(let X 1 (let X 2 X))").unwrap(),
        Value::Int(2)
    ));
}

#[test]
fn if_branch() {
    assert!(matches!(run("(if true 1 2)").unwrap(), Value::Int(1)));
    assert!(matches!(run("(if false 1 2)").unwrap(), Value::Int(2)));
    assert!(matches!(run("(if (> 5 3) 1 2)").unwrap(), Value::Int(1)));
}

#[test]
fn cond_falls_through() {
    let v = run("(cond ((> 1 2) 100) ((= 3 3) 42) (true 0))").unwrap();
    assert!(matches!(v, Value::Int(42)));
}

#[test]
fn and_or_short_circuit() {
    assert!(matches!(
        run("(and true (> 5 3))").unwrap(),
        Value::Bool(true)
    ));
    assert!(matches!(
        run("(and false (simple-error \"should not run\"))").unwrap(),
        Value::Bool(false)
    ));
    assert!(matches!(run("(or false true)").unwrap(), Value::Bool(true)));
    assert!(matches!(
        run("(or true (simple-error \"should not run\"))").unwrap(),
        Value::Bool(true)
    ));
}

#[test]
fn lambda_and_apply() {
    let v = run("((lambda X (+ X 1)) 41)").unwrap();
    assert!(matches!(v, Value::Int(42)));
}

#[test]
fn defun_and_call() {
    let mut interp = Interp::new();
    run_in(&mut interp, "(defun double (X) (* X 2))").unwrap();
    let v = run_in(&mut interp, "(double 21)").unwrap();
    assert!(matches!(v, Value::Int(42)));
}

#[test]
fn recursive_factorial() {
    let mut interp = Interp::new();
    run_in(
        &mut interp,
        "(defun fact (N) (if (= N 0) 1 (* N (fact (- N 1)))))",
    )
    .unwrap();
    let v = run_in(&mut interp, "(fact 5)").unwrap();
    assert!(matches!(v, Value::Int(120)));
}

#[test]
fn tail_recursive_loop_does_not_overflow() {
    // A deep tail call. With the trampoline, this must succeed without
    // blowing the Rust stack.
    let mut interp = Interp::new();
    run_in(
        &mut interp,
        "(defun loop (N ACC) (if (= N 0) ACC (loop (- N 1) (+ ACC 1))))",
    )
    .unwrap();
    let v = run_in(&mut interp, "(loop 50000 0)").unwrap();
    assert!(matches!(v, Value::Int(50000)));
}

#[test]
fn partial_application() {
    let mut interp = Interp::new();
    // Curry add via partial application.
    run_in(&mut interp, "(defun add (X Y) (+ X Y))").unwrap();
    let v = run_in(&mut interp, "(((fn add) 10) 32)").unwrap();
    assert!(matches!(v, Value::Int(42)));
}

#[test]
fn trap_error_catches_simple_error() {
    let v = run("(trap-error (simple-error \"boom\") (lambda E (error-to-string E)))").unwrap();
    if let Value::Str(s) = v {
        assert_eq!(&*s, "boom");
    } else {
        panic!("expected string");
    }
}

#[test]
fn freeze_and_thaw() {
    let v = run("(thaw (freeze (+ 1 2)))").unwrap();
    assert!(matches!(v, Value::Int(3)));
}

#[test]
fn set_and_value() {
    let mut interp = Interp::new();
    run_in(&mut interp, "(set my-flag 99)").unwrap();
    let v = run_in(&mut interp, "(value my-flag)").unwrap();
    assert!(matches!(v, Value::Int(99)));
}

#[test]
fn absvector_round_trip() {
    let mut interp = Interp::new();
    run_in(&mut interp, "(set v (absvector 3))").unwrap();
    run_in(&mut interp, "(address-> (value v) 0 100)").unwrap();
    run_in(&mut interp, "(address-> (value v) 1 200)").unwrap();
    let v = run_in(&mut interp, "(<-address (value v) 1)").unwrap();
    assert!(matches!(v, Value::Int(200)));
}

#[test]
fn intern_and_back() {
    let v = run("(intern \"abc\")").unwrap();
    assert!(matches!(v, Value::Sym(_)));
}

#[test]
fn predicates() {
    assert!(matches!(run("(number? 1)").unwrap(), Value::Bool(true)));
    assert!(matches!(
        run("(number? \"x\")").unwrap(),
        Value::Bool(false)
    ));
    assert!(matches!(run("(string? \"x\")").unwrap(), Value::Bool(true)));
    assert!(matches!(
        run("(cons? (cons 1 ()))").unwrap(),
        Value::Bool(true)
    ));
    assert!(matches!(run("(cons? ())").unwrap(), Value::Bool(false)));
}
