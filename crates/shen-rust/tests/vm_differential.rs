//! Differential oracle: the bytecode VM must be observationally equivalent
//! to the tree-walker (A5 in `design/perf-handoff.md`).
//!
//! The tree-walker is the reference. For each program in the corpus we
//! install its `defun`s twice — once as tree-walked `ClosureKind::Lambda`,
//! once as VM `ClosureKind::Bytecode` — then evaluate the same entry
//! expression against each and assert the results agree (both the value,
//! by `shen_eq`, and the Ok/Err status). This is the gate that lets the VM
//! become the default executor (A3) without trusting it blindly: any
//! divergence fails CI.
//!
//! Programs here use only forms the VM compiler lowers. `trap-error` /
//! `thaw` inside a `defun` body are covered separately
//! (`uncompilable_bodies_are_rejected`) — those are the documented
//! tree-walker fallback in `do_defun`.

use std::rc::Rc;

use shen_rust::error::ShenResult;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::ast::KlExpr;
use shen_rust::kl::parser::parse_one;
use shen_rust::symbol::SymId;
use shen_rust::value::{shen_eq, Closure, ClosureKind, LambdaBody, Value};

#[derive(Clone, Copy)]
enum Engine {
    TreeWalk,
    Vm,
}

fn parse_defun(interp: &mut Interp, src: &str) -> (SymId, Vec<SymId>, KlExpr) {
    let form = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    let items = match form {
        KlExpr::App(items) => items,
        other => panic!("expected (defun ...), got {other:?}"),
    };
    assert_eq!(items.len(), 4, "defun must be (defun NAME (PARAMS) BODY)");
    let name = match &items[1] {
        KlExpr::Sym(s) => *s,
        other => panic!("defun name must be sym, got {other:?}"),
    };
    let params: Vec<SymId> = match &items[2] {
        KlExpr::Nil => Vec::new(),
        KlExpr::App(ps) => ps
            .iter()
            .map(|p| match p {
                KlExpr::Sym(s) => *s,
                other => panic!("param must be sym, got {other:?}"),
            })
            .collect(),
        other => panic!("param list malformed: {other:?}"),
    };
    (name, params, items[3].clone())
}

/// Install one `defun` under the chosen engine.
fn install(interp: &mut Interp, src: &str, engine: Engine) {
    let (name, params, body) = parse_defun(interp, src);
    let kind = match engine {
        Engine::TreeWalk => ClosureKind::Lambda(Rc::new(LambdaBody {
            captured: Vec::new(),
            params: params.clone(),
            body,
        })),
        Engine::Vm => {
            let bf = shen_rust::vm::compile_fn(interp, Some(name), &params, &body)
                .unwrap_or_else(|e| panic!("vm compile {src:?}: {e}"));
            ClosureKind::Bytecode(Rc::new(bf), Vec::new())
        }
    };
    let closure = Closure {
        name: Some(name),
        arity: params.len(),
        partial: Vec::new(),
        kind,
    };
    interp.env.set_fn(name, Value::closure(closure));
}

/// Install `defuns` under `engine` in a fresh interpreter, then evaluate
/// `entry`. The entry expression itself is tree-walked either way (that's
/// the realistic top level); only the installed functions differ.
fn run_program(defuns: &[&str], entry: &str, engine: Engine) -> ShenResult<Value> {
    let mut interp = Interp::new();
    for d in defuns {
        install(&mut interp, d, engine);
    }
    let expr =
        parse_one(entry, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {entry:?}: {e}"));
    interp.eval(&expr)
}

/// Assert the VM and tree-walker agree on `(defuns, entry)`.
fn assert_equiv(defuns: &[&str], entry: &str) {
    let tw = run_program(defuns, entry, Engine::TreeWalk);
    let vm = run_program(defuns, entry, Engine::Vm);
    match (&tw, &vm) {
        (Ok(a), Ok(b)) => assert!(
            shen_eq(a, b),
            "value divergence on {entry:?}: tree={a:?} vm={b:?}"
        ),
        (Err(_), Err(_)) => { /* both error — agree on failure */ }
        _ => panic!("status divergence on {entry:?}: tree={tw:?} vm={vm:?}"),
    }
}

#[test]
fn arithmetic_and_nesting() {
    assert_equiv(&["(defun f (x) (+ (* x x) (- x 1)))"], "(f 7)");
    assert_equiv(&["(defun g (a b) (/ (+ a b) 2))"], "(g 10 4)");
}

#[test]
fn non_tail_recursion_fib() {
    let d = &["(defun fib (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))"];
    for n in [0, 1, 2, 5, 10, 15] {
        assert_equiv(d, &format!("(fib {n})"));
    }
}

#[test]
fn self_tail_call_loop() {
    let d = &[
        "(defun loop (n acc) (if (= n 0) acc (loop (- n 1) (+ acc n))))",
        "(defun sumto (n) (loop n 0))",
    ];
    for n in [0, 1, 100, 5000] {
        assert_equiv(d, &format!("(sumto {n})"));
    }
}

#[test]
fn cross_function_tail_calls() {
    // Mutual tail recursion — exercises Op::TailCall on both engines.
    let d = &[
        "(defun even? (n) (if (= n 0) true (odd? (- n 1))))",
        "(defun odd? (n) (if (= n 0) false (even? (- n 1))))",
    ];
    for n in [0, 1, 2, 7, 100, 1001] {
        assert_equiv(d, &format!("(even? {n})"));
    }
}

#[test]
fn let_shadowing() {
    assert_equiv(
        &["(defun s (x) (let x (* x 10) (let x (+ x 1) x)))"],
        "(s 5)",
    );
}

#[test]
fn cond_cascade() {
    let d = &["(defun grade (s) (cond ((>= s 90) 4) ((>= s 80) 3) ((>= s 70) 2) (true 0)))"];
    for s in [95, 85, 72, 50] {
        assert_equiv(d, &format!("(grade {s})"));
    }
}

#[test]
fn and_or_short_circuit() {
    let d = &["(defun p (x) (or (and (> x 0) (< x 10)) (= x 100)))"];
    for x in [-1, 5, 50, 100] {
        assert_equiv(d, &format!("(p {x})"));
    }
}

#[test]
fn cons_hd_tl_walk() {
    let d = &[
        "(defun build (n acc) (if (= n 0) acc (build (- n 1) (cons n acc))))",
        "(defun walk (xs acc) (if (cons? xs) (walk (tl xs) (+ acc (hd xs))) acc))",
        "(defun s (n) (walk (build n ()) 0))",
    ];
    for n in [0, 1, 50, 500] {
        assert_equiv(d, &format!("(s {n})"));
    }
}

#[test]
fn closures_capture_upvals() {
    // make-adder returns a closure capturing y; both engines must produce
    // an equivalent callable that the (tree-walked) entry applies.
    assert_equiv(
        &["(defun make-adder (y) (lambda x (+ x y)))"],
        "((make-adder 10) 5)",
    );
    // Two-level capture.
    assert_equiv(
        &["(defun mk (a) (lambda b (lambda c (+ a (+ b c)))))"],
        "(((mk 1) 2) 3)",
    );
}

#[test]
fn freeze_then_thaw() {
    // `freeze` is compiled inside the defun body; `thaw` lives in the
    // tree-walked entry expression.
    assert_equiv(
        &["(defun delayed (x) (freeze (* x x)))"],
        "(thaw (delayed 9))",
    );
}

#[test]
fn partial_and_over_application() {
    // Curried application drives the partial-app path (Call falls back to
    // interp.apply, which must dispatch the Bytecode closure identically).
    let d = &["(defun add3 (a b c) (+ a (+ b c)))"];
    assert_equiv(d, "(add3 1 2 3)");
    assert_equiv(d, "(((add3 1) 2) 3)");
    assert_equiv(d, "((add3 1 2) 3)");
}

#[test]
fn error_cases_agree() {
    // hd of a non-cons must error on BOTH engines (status divergence would
    // fail). Result is Err either way.
    assert_equiv(&["(defun bad (x) (hd x))"], "(bad 5)");
    // Division by zero.
    assert_equiv(&["(defun d (x) (/ x 0))"], "(d 10)");
}

#[test]
fn boolean_symbol_cross_equate() {
    // A predicate result compared against the literal `true` must behave
    // the same; this is the Bool/Sym cross-equate path in shen_eq.
    assert_equiv(&["(defun q (x) (= true (cons? x)))"], "(q (cons 1 ()))");
}

#[test]
fn uncompilable_bodies_are_rejected() {
    // `trap-error` and `thaw` in a *defun body* are not lowered by the VM
    // compiler — `do_defun` falls back to the tree-walker for these. Assert
    // the compiler rejects them (so the fallback is genuinely exercised),
    // rather than silently miscompiling.
    let mut interp = Interp::new();
    for src in [
        "(defun safe (x) (trap-error (hd x) (lambda e 0)))",
        "(defun force (f) (thaw f))",
    ] {
        let (name, params, body) = parse_defun(&mut interp, src);
        let r = shen_rust::vm::compile_fn(&interp, Some(name), &params, &body);
        assert!(
            r.is_err(),
            "expected VM compiler to reject {src:?} (forcing tree-walker fallback), but it compiled"
        );
    }
}
