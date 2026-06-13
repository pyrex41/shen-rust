//! Port-authored stdlib library suite (NOT the canonical kernel suite).
//!
//! Mirror of shen-go's `kl/library_test.go`: the Shen standard library
//! functions behave (reverse / append / map / filter / element? / length /
//! reverse-of-nested). Unlike shen-go's `kl` package — where these helpers
//! are hand-written Go — shen-rust's stdlib *is* the booted Shen kernel, so
//! this suite boots ShenOSKernel and drives the functions through the
//! kernel's own `eval` pipeline. That makes it a heavier (boot-once-per-test)
//! suite than the no-boot `primitives_coverage.rs`; the cert/port split is
//! preserved (this is still port-authored, the canonical suite is the
//! `kernel-tests` gate).
//!
//! Results are asserted on `Value` structure (head/tail/int), not rendered
//! strings, so the kernel's list printer formatting can't make them flaky.

use std::path::PathBuf;

use shen_rust::interp::boot::boot_with_kernel;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

fn kernel_klambda_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // workspace root
    p.push("kernel");
    p.push("klambda");
    p
}

fn fresh_booted() -> Interp {
    let mut interp = Interp::new();
    let dir = kernel_klambda_dir();
    boot_with_kernel(&mut interp, &dir).unwrap_or_else(|e| panic!("kernel boot failed: {e}"));
    interp
}

/// Evaluate through the kernel's own `eval` (macro expansion +
/// process-applications), the realistic top-level path.
fn eval(interp: &mut Interp, src: &str) -> Value {
    let expr = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    let eval_sym = interp.intern("eval");
    if interp.env.get_fn(eval_sym).is_some() {
        let quoted = klexpr_to_value(&expr);
        let f = interp.env.get_fn(eval_sym).cloned().unwrap();
        interp
            .apply(f, vec![quoted])
            .unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
    } else {
        interp
            .eval(&expr)
            .unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
    }
}

fn klexpr_to_value(e: &shen_rust::kl::ast::KlExpr) -> Value {
    use shen_rust::kl::ast::KlExpr;
    match e {
        KlExpr::Nil => Value::nil(),
        KlExpr::Bool(b) => Value::bool(*b),
        KlExpr::Int(n) => Value::int(*n),
        KlExpr::Float(x) => Value::float(*x),
        KlExpr::Str(s) => Value::str(s.clone()),
        KlExpr::Sym(s) => Value::sym(*s),
        KlExpr::App(items) => Value::list(items.iter().map(klexpr_to_value)),
    }
}

/// Collect a proper Shen list `Value` into a Vec of i64 (panics on a
/// non-integer element or improper list — fine for these tests).
fn ints(v: &Value) -> Vec<i64> {
    let mut out = Vec::new();
    let mut cur = *v;
    while !cur.is_nil() {
        let h = cur.head().expect("proper list head");
        out.push(h.as_int().expect("integer element"));
        cur = *cur.tail().expect("proper list tail");
    }
    out
}

#[test]
fn reverse_simple() {
    let mut i = fresh_booted();
    let v = eval(&mut i, "(reverse (cons 1 (cons 2 (cons 3 ()))))");
    assert_eq!(ints(&v), vec![3, 2, 1]);
    // reverse of the empty list is the empty list.
    assert!(eval(&mut i, "(reverse ())").is_nil());
}

#[test]
fn append_concatenates() {
    let mut i = fresh_booted();
    let v = eval(&mut i, "(append (cons 1 (cons 2 ())) (cons 3 (cons 4 ())))");
    assert_eq!(ints(&v), vec![1, 2, 3, 4]);
    // append with an empty left/right is identity.
    assert_eq!(ints(&eval(&mut i, "(append () (cons 9 ()))")), vec![9]);
    assert_eq!(ints(&eval(&mut i, "(append (cons 9 ()) ())")), vec![9]);
}

#[test]
fn map_applies_to_each() {
    let mut i = fresh_booted();
    // Define a named unary function and map it (Shen's `map` wants a
    // function value; a defined name curries cleanly).
    eval(&mut i, "(define sq X -> (* X X))");
    let v = eval(&mut i, "(map (function sq) (cons 1 (cons 2 (cons 3 ()))))");
    assert_eq!(ints(&v), vec![1, 4, 9]);
}

#[test]
fn remove_drops_matching() {
    // shen-go's library test uses its own Go `filter` helper; the Shen
    // kernel has no `filter`, but it does have `remove` (drop all elements
    // equal to a value), which is the same element-wise-predicate shape.
    let mut i = fresh_booted();
    let v = eval(&mut i, "(remove 2 (cons 1 (cons 2 (cons 3 (cons 2 ())))))");
    assert_eq!(ints(&v), vec![1, 3]);
}

#[test]
fn subst_and_nth() {
    let mut i = fresh_booted();
    // subst NEW OLD LIST replaces every OLD with NEW.
    let v = eval(&mut i, "(subst 9 2 (cons 1 (cons 2 (cons 3 ()))))");
    assert_eq!(ints(&v), vec![1, 9, 3]);
    // nth is 1-based in Shen.
    assert_eq!(
        eval(&mut i, "(nth 2 (cons 10 (cons 20 (cons 30 ()))))").as_int(),
        Some(20)
    );
}

#[test]
fn element_membership() {
    let mut i = fresh_booted();
    assert_eq!(
        eval(&mut i, "(element? 2 (cons 1 (cons 2 (cons 3 ()))))").as_bool(),
        Some(true)
    );
    assert_eq!(
        eval(&mut i, "(element? 9 (cons 1 (cons 2 (cons 3 ()))))").as_bool(),
        Some(false)
    );
    assert_eq!(eval(&mut i, "(element? 1 ())").as_bool(), Some(false));
}

#[test]
fn length_counts() {
    let mut i = fresh_booted();
    assert_eq!(
        eval(&mut i, "(length (cons 1 (cons 2 (cons 3 ()))))").as_int(),
        Some(3)
    );
    assert_eq!(eval(&mut i, "(length ())").as_int(), Some(0));
}

#[test]
fn kernel_type_predicates() {
    // These predicates are kernel-level Shen (not raw KL primitives), so
    // they belong here rather than in primitives_coverage.rs. Mirror of
    // shen-go's variable?/integer? cases.
    let mut i = fresh_booted();
    assert_eq!(eval(&mut i, "(variable? X)").as_bool(), Some(true));
    assert_eq!(eval(&mut i, "(variable? x)").as_bool(), Some(false));
    assert_eq!(eval(&mut i, "(integer? 42)").as_bool(), Some(true));
    assert_eq!(eval(&mut i, "(integer? 4.5)").as_bool(), Some(false));
}

#[test]
fn reverse_of_nested_lists() {
    // shen-go's TestReverse exercises reversing a list whose elements are
    // themselves lists. Build ((1 2 3) 4) and reverse → (4 (1 2 3)).
    let mut i = fresh_booted();
    let v = eval(
        &mut i,
        "(reverse (cons (cons 1 (cons 2 (cons 3 ()))) (cons 4 ())))",
    );
    // First element after reverse is the atom 4.
    assert_eq!(v.head().and_then(|h| h.as_int()), Some(4));
    // Second element is the inner list (1 2 3).
    let inner = *v.tail().unwrap().head().unwrap();
    assert_eq!(ints(&inner), vec![1, 2, 3]);
}
