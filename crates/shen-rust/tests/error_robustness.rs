//! Port-authored error-robustness suite (NOT the canonical kernel suite).
//!
//! Mirror of shen-go's `kl/error_robustness_test.go`: the error-CATCHABILITY
//! CONTRACT. Every documented kernel error path must
//!
//!   1. raise a Shen-catchable error (so `(trap-error ...)` handles it),
//!   2. surface a stable, informative message (so callers can match on it),
//!   3. leave the interpreter clean enough that the next eval succeeds.
//!
//! Tested on BOTH evaluation paths, following the install pattern of
//! `vm_differential.rs`: a trigger is run (a) at the tree-walked top level
//! and (b) inside a `defun` body installed as a `ClosureKind::Bytecode`
//! closure so it executes through the bytecode VM.
//!
//! Divergence from shen-go: shen-rust's error wording differs (idiomatic),
//! e.g. shen-go's "can't apply non function: overflow->str" vs shen-rust's
//! "undefined function: overflow->str". We pin shen-rust's ACTUAL messages
//! and assert on stable substrings rather than faking parity.

use std::rc::Rc;

use shen_rust::error::ShenResult;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::ast::KlExpr;
use shen_rust::kl::parser::parse_one;
use shen_rust::symbol::SymId;
use shen_rust::value::{Closure, ClosureKind, Value};

fn parse(interp: &mut Interp, src: &str) -> KlExpr {
    parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"))
}

fn eval(interp: &mut Interp, src: &str) -> ShenResult<Value> {
    let e = parse(interp, src);
    interp.eval(&e)
}

/// trap-error wrapper: evaluate `(trap-error TRIGGER (lambda E
/// (error-to-string E)))` at the tree-walked top level and return the
/// caught message (or panic if the trigger somehow did not raise).
fn caught_tree(interp: &mut Interp, trigger: &str) -> String {
    let src = format!("(trap-error {trigger} (lambda E (error-to-string E)))");
    let v = eval(interp, &src).unwrap_or_else(|e| panic!("trap-error itself failed: {e}"));
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| panic!("trigger {trigger:?} did not raise a catchable error; got {v:?}"))
}

// --- VM install plumbing (same shape as vm_differential.rs) ---------------

fn parse_defun(interp: &mut Interp, src: &str) -> (SymId, Vec<SymId>, KlExpr) {
    let form = parse(interp, src);
    let items = match form {
        KlExpr::App(items) => items,
        other => panic!("expected (defun ...), got {other:?}"),
    };
    assert_eq!(items.len(), 4, "defun must be (defun NAME (PARAMS) BODY)");
    let name = match &items[1] {
        KlExpr::Sym(s) => *s,
        other => panic!("defun name must be sym: {other:?}"),
    };
    let params: Vec<SymId> = match &items[2] {
        KlExpr::Nil => Vec::new(),
        KlExpr::App(ps) => ps
            .iter()
            .map(|p| match p {
                KlExpr::Sym(s) => *s,
                other => panic!("param must be sym: {other:?}"),
            })
            .collect(),
        other => panic!("param list malformed: {other:?}"),
    };
    (name, params, items[3].clone())
}

/// Install a 0-arg `defun` whose body is `trigger`, under the bytecode VM,
/// then return the message caught when it is invoked through `trap-error`.
///
/// Returns `None` if the VM compiler rejects the body — some forms
/// (`trap-error`, `thaw` inside a defun body) are the documented
/// tree-walker fallback in `do_defun` and never reach the VM. Callers skip
/// the VM assertion for those, exactly as shen-go skips with an empty
/// `wantVM`.
fn caught_vm(interp: &mut Interp, fn_name: &str, trigger: &str) -> Option<String> {
    let defun_src = format!("(defun {fn_name} () {trigger})");
    let (name, params, body) = parse_defun(interp, &defun_src);
    let bf = match shen_rust::vm::compile_fn(interp, Some(name), &params, &body) {
        Ok(bf) => bf,
        Err(_) => return None, // documented tree-walker fallback
    };
    let closure = Closure {
        name: Some(name),
        arity: params.len(),
        partial: Vec::new(),
        kind: ClosureKind::Bytecode(Rc::new(bf), Vec::new()),
    };
    interp.env.set_fn(name, Value::closure(closure));
    let src = format!("(trap-error ({fn_name}) (lambda E (error-to-string E)))");
    let v = eval(interp, &src).unwrap_or_else(|e| panic!("trap-error (vm) failed: {e}"));
    Some(
        v.as_str()
            .map(str::to_string)
            .unwrap_or_else(|| panic!("vm trigger {trigger:?} did not raise; got {v:?}")),
    )
}

/// The documented error paths and a stable substring that must appear in
/// the caught message on shen-rust. (shen-go's exact strings differ — see
/// the module comment.)
fn cases() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // (subtest name, trigger, substring that must appear)
        ("apply_unbound_symbol", "(overflow->str)", "overflow->str"),
        ("apply_non_function_literal", "(42 1)", "42"),
        (
            "value_of_unbound_variable",
            "(value never-bound-xyz)",
            "never-bound-xyz",
        ),
        ("if_requires_boolean", "(if 42 1 2)", "boolean"),
        ("simple_error_explicit", r#"(simple-error "oops")"#, "oops"),
        ("hd_of_non_cons", "(hd 5)", "hd"),
        ("division_by_zero", "(/ 1 0)", "division"),
    ]
}

#[test]
fn tree_walker_errors_are_catchable() {
    for (name, trigger, needle) in cases() {
        // Fresh interpreter per case so a regression in one can't poison
        // the others.
        let mut i = Interp::new();
        let msg = caught_tree(&mut i, trigger);
        assert!(
            msg.contains(needle),
            "[{name}] tree-walker message {msg:?} missing {needle:?}"
        );
        // State is clean enough for the next eval to succeed.
        assert_eq!(
            eval(&mut i, "(+ 40 2)").unwrap().as_int(),
            Some(42),
            "[{name}] interpreter not clean after caught error"
        );
    }
}

#[test]
fn vm_errors_are_catchable() {
    let mut covered = 0;
    for (name, trigger, needle) in cases() {
        let mut i = Interp::new();
        let fn_name = format!("bad-fn-{name}");
        match caught_vm(&mut i, &fn_name, trigger) {
            Some(msg) => {
                covered += 1;
                assert!(
                    msg.contains(needle),
                    "[{name}] VM message {msg:?} missing {needle:?}"
                );
                assert_eq!(
                    eval(&mut i, "(+ 40 2)").unwrap().as_int(),
                    Some(42),
                    "[{name}] interpreter not clean after caught VM error"
                );
            }
            None => {
                // VM compiler rejected the body → tree-walker fallback path.
                // That is a legitimate documented outcome; the tree-walker
                // case above already covers catchability.
            }
        }
    }
    // The whole point is to exercise the VM error path — make sure at least
    // a few cases actually compiled to bytecode and raised through the VM.
    assert!(
        covered >= 4,
        "expected several VM-compiled error triggers, only {covered} reached the VM"
    );
}

/// shen-go's TestEvalSurvivesAdversarialSequence: several errors in a row on
/// the SAME interpreter, each caught, with a valid eval succeeding after.
#[test]
fn interpreter_survives_adversarial_sequence() {
    let mut i = Interp::new();
    let seq = [
        ("(overflow->str)", "overflow->str"),
        ("(value not-bound-1)", "not-bound-1"),
        ("(if 42 1 2)", "boolean"),
        (r#"(simple-error "boom")"#, "boom"),
        ("(42 1)", "42"),
    ];
    for (trigger, needle) in seq {
        let msg = caught_tree(&mut i, trigger);
        assert!(
            msg.contains(needle),
            "sequence: {trigger:?} message {msg:?} missing {needle:?}"
        );
    }
    // After the whole adversarial run, ordinary arithmetic still works.
    assert_eq!(eval(&mut i, "(+ 40 2)").unwrap().as_int(), Some(42));
}

/// A caught error must not leave the trigger's partially-evaluated state
/// behind: nested trap-errors and re-binding work afterward.
#[test]
fn nested_trap_error_state_is_clean() {
    let mut i = Interp::new();
    // Inner error caught, outer returns the recovered value unchanged.
    let v = eval(
        &mut i,
        r#"(trap-error
             (trap-error (hd 5) (lambda E (simple-error (error-to-string E))))
             (lambda E2 (error-to-string E2)))"#,
    )
    .unwrap();
    assert!(v.as_str().is_some_and(|s| s.contains("hd")));
    // A subsequent defun + call works.
    eval(&mut i, "(defun sq (x) (* x x))").unwrap();
    assert_eq!(eval(&mut i, "(sq 9)").unwrap().as_int(), Some(81));
}
