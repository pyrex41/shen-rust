//! Port-authored primitive-coverage suite (NOT the canonical kernel suite).
//!
//! Mirror of shen-go's `kl/primitives_coverage_test.go` +
//! `kl/primitives_test.go`: drive the KL primitive surface through the
//! evaluator (`Interp::eval`) and assert on results. These exercise the
//! ~46 native primitives `Interp::new()` registers *without* booting the
//! kernel — the shen-rust analogue of shen-go's bare `evalString`. Kernel-
//! level Shen functions (`map`, `variable?`, `integer?`, …) need a booted
//! kernel and live in `library.rs` instead.
//!
//! Where shen-rust legitimately differs from shen-go, the divergence is
//! pinned with the CORRECT shen-rust behavior and a comment, never faked
//! into false parity.

use shen_rust::error::ShenResult;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

fn run(interp: &mut Interp, src: &str) -> ShenResult<Value> {
    let e = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    interp.eval(&e)
}

fn ok(interp: &mut Interp, src: &str) -> Value {
    run(interp, src).unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
}

fn int(interp: &mut Interp, src: &str) -> i64 {
    ok(interp, src)
        .as_int()
        .unwrap_or_else(|| panic!("{src:?} not an int"))
}

fn float(interp: &mut Interp, src: &str) -> f64 {
    ok(interp, src)
        .as_float()
        .unwrap_or_else(|| panic!("{src:?} not a float"))
}

fn boolean(interp: &mut Interp, src: &str) -> bool {
    ok(interp, src)
        .as_bool()
        .unwrap_or_else(|| panic!("{src:?} not a bool"))
}

fn string(interp: &mut Interp, src: &str) -> String {
    ok(interp, src)
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| panic!("{src:?} not a string"))
}

// ---------------------------------------------------------------------------
// Arithmetic — integer and float, including the int→float fallback for
// fractional division (shen-go's "divide fractional" case).
// ---------------------------------------------------------------------------

#[test]
fn arithmetic_integer() {
    let mut i = Interp::new();
    assert_eq!(int(&mut i, "(+ 2 3)"), 5);
    assert_eq!(int(&mut i, "(- 5 3)"), 2);
    assert_eq!(int(&mut i, "(* 6 7)"), 42);
    assert_eq!(int(&mut i, "(/ 20 4)"), 5);
    // Nested.
    assert_eq!(int(&mut i, "(+ 1 (* 2 (- 10 4)))"), 13);
}

#[test]
fn arithmetic_float() {
    let mut i = Interp::new();
    assert!((float(&mut i, "(- 5.5 2.0)") - 3.5).abs() < 1e-9);
    assert!((float(&mut i, "(* 1.5 3.0)") - 4.5).abs() < 1e-9);
    // Whole-number int / int stays an int; fractional promotes to float.
    assert!((float(&mut i, "(/ 7 2)") - 3.5).abs() < 1e-9);
    assert_eq!(int(&mut i, "(/ 6 3)"), 2);
}

#[test]
fn float_comparisons() {
    let mut i = Interp::new();
    assert!(boolean(&mut i, "(> 5 3)"));
    assert!(!boolean(&mut i, "(< 5 3)"));
    assert!(boolean(&mut i, "(>= 5 5)"));
    assert!(boolean(&mut i, "(<= 4 5)"));
    // Cross int/float equality (= 1 1.0) — shen-go's "equality across" case.
    assert!(boolean(&mut i, "(= 1 1.0)"));
    assert!(!boolean(&mut i, "(= 1 2)"));
    assert!(boolean(&mut i, "(> 2.5 2.4)"));
}

// ---------------------------------------------------------------------------
// cons / hd / tl, including the empty-list error paths.
// ---------------------------------------------------------------------------

#[test]
fn cons_hd_tl() {
    let mut i = Interp::new();
    assert_eq!(int(&mut i, "(hd (cons 1 (cons 2 ())))"), 1);
    // (tl (cons 1 (cons 2 ()))) is (2) — head of that is 2.
    assert_eq!(int(&mut i, "(hd (tl (cons 1 (cons 2 ()))))"), 2);
    assert!(ok(&mut i, "(tl (cons 1 ()))").is_nil());
    assert!(boolean(&mut i, "(cons? (cons 1 ()))"));
    assert!(!boolean(&mut i, "(cons? ())"));
}

#[test]
fn hd_tl_of_non_cons_error() {
    let mut i = Interp::new();
    // shen-go pins this as a catchable error; shen-rust's message is
    // "hd: not a cons: Int(5)" (idiomatic divergence in wording).
    assert!(run(&mut i, "(hd 5)").is_err());
    assert!(run(&mut i, "(tl 5)").is_err());
    // Catchable via trap-error, leaving state clean.
    let msg = string(&mut i, "(trap-error (hd 5) (lambda E (error-to-string E)))");
    assert!(msg.contains("hd"), "message should mention hd: {msg:?}");
    assert_eq!(int(&mut i, "(+ 40 2)"), 42);
}

// ---------------------------------------------------------------------------
// Type predicates available pre-boot. (`variable?`/`integer?` are kernel
// functions — covered in library.rs.)
// ---------------------------------------------------------------------------

#[test]
fn type_predicates() {
    let mut i = Interp::new();
    assert!(boolean(&mut i, "(number? 42)"));
    assert!(boolean(&mut i, "(number? 4.5)"));
    assert!(!boolean(&mut i, r#"(number? "x")"#));
    assert!(boolean(&mut i, r#"(string? "hi")"#));
    assert!(!boolean(&mut i, "(string? 1)"));
    assert!(boolean(&mut i, "(symbol? hello)"));
    assert!(!boolean(&mut i, "(symbol? 1)"));
    assert!(boolean(&mut i, "(cons? (cons 1 ()))"));
    assert!(!boolean(&mut i, "(cons? 1)"));
    assert!(boolean(&mut i, "(absvector? (absvector 3))"));
    assert!(!boolean(&mut i, "(absvector? 1)"));
    assert!(boolean(&mut i, "(boolean? true)"));
    assert!(boolean(&mut i, "(boolean? false)"));
    assert!(!boolean(&mut i, "(boolean? 1)"));
}

// ---------------------------------------------------------------------------
// String primitives: string->n / n->string / cn / tlstr / pos / str.
// ---------------------------------------------------------------------------

#[test]
fn string_ops() {
    let mut i = Interp::new();
    assert_eq!(int(&mut i, r#"(string->n "A")"#), 65);
    assert_eq!(string(&mut i, "(n->string 65)"), "A");
    assert_eq!(string(&mut i, r#"(cn "foo" "bar")"#), "foobar");
    assert_eq!(string(&mut i, r#"(tlstr "hello")"#), "ello");
    assert_eq!(string(&mut i, r#"(pos "hello" 1)"#), "e");
    // Round trip string->n . n->string.
    assert_eq!(string(&mut i, r#"(n->string (string->n "Z"))"#), "Z");
}

#[test]
fn string_op_errors() {
    let mut i = Interp::new();
    // tlstr of empty string and string->n of empty string both error.
    assert!(run(&mut i, r#"(tlstr "")"#).is_err());
    assert!(run(&mut i, r#"(string->n "")"#).is_err());
    // pos out of range errors.
    assert!(run(&mut i, r#"(pos "hi" 5)"#).is_err());
}

#[test]
fn str_renders_atoms() {
    let mut i = Interp::new();
    // `str` stringifies any atom.
    assert_eq!(string(&mut i, "(str 42)"), "42");
    assert_eq!(string(&mut i, "(str foo)"), "foo");
}

// ---------------------------------------------------------------------------
// symbols: intern / value / set (global value cell round trip).
// ---------------------------------------------------------------------------

#[test]
fn intern_value_set() {
    let mut i = Interp::new();
    assert!(ok(&mut i, r#"(intern "abc")"#).is_sym());
    // set returns the value; value reads it back.
    assert_eq!(int(&mut i, "(set foo 99)"), 99);
    assert_eq!(int(&mut i, "(value foo)"), 99);
    // Overwrite.
    ok(&mut i, "(set foo 7)");
    assert_eq!(int(&mut i, "(value foo)"), 7);
}

#[test]
fn value_of_unbound_errors() {
    let mut i = Interp::new();
    // shen-rust message: "value: unbound global: NAME" (shen-go says
    // "variable NAME not bound" — idiomatic wording divergence).
    let msg = string(
        &mut i,
        "(trap-error (value never-bound-xyz) (lambda E (error-to-string E)))",
    );
    assert!(
        msg.contains("never-bound-xyz") && msg.contains("unbound"),
        "unexpected unbound message: {msg:?}"
    );
}

// ---------------------------------------------------------------------------
// absvector / address-> / <-address, including uninitialized slots.
// ---------------------------------------------------------------------------

#[test]
fn absvector_round_trip() {
    let mut i = Interp::new();
    ok(&mut i, "(set v (absvector 3))");
    // address-> returns the vector, so it chains.
    ok(&mut i, "(address-> (value v) 0 100)");
    ok(&mut i, "(address-> (value v) 1 200)");
    assert_eq!(int(&mut i, "(<-address (value v) 0)"), 100);
    assert_eq!(int(&mut i, "(<-address (value v) 1)"), 200);
    // Chained set+get in one expression (shen-go's "vector round trip").
    assert_eq!(
        int(&mut i, "(<-address (address-> (absvector 3) 1 7) 1)"),
        7
    );
}

#[test]
fn absvector_uninitialized_slot() {
    // shen-go: an uninitialized slot reads back the special `undefined`
    // object. shen-rust pre-boot zero-fills slots with interned symbol id 0
    // (the `true` symbol per WellKnown ordering — see primitives.rs); after a
    // kernel boot it becomes the `shen.fail!` sentinel. Either way the read
    // must SUCCEED (not panic / not out-of-range) for an in-bounds slot.
    let mut i = Interp::new();
    let v = ok(&mut i, "(<-address (absvector 3) 0)");
    assert!(
        v.is_sym() || v.is_nil(),
        "uninitialized slot should read a sentinel symbol, got {v:?}"
    );
}

#[test]
fn absvector_out_of_range_errors() {
    let mut i = Interp::new();
    // Reading past the end is a catchable error, not a panic.
    assert!(run(&mut i, "(<-address (absvector 2) 5)").is_err());
    assert!(run(&mut i, "(address-> (absvector 2) 5 1)").is_err());
}

// ---------------------------------------------------------------------------
// hash — deterministic, in-range, reasonably distributed
// (mirror of shen-go's TestPrimHash).
// ---------------------------------------------------------------------------

#[test]
fn hash_is_deterministic_and_in_range() {
    let mut i = Interp::new();
    let a = int(&mut i, r#"(hash "session-token-42" 256)"#);
    let b = int(&mut i, r#"(hash "session-token-42" 256)"#);
    assert_eq!(a, b, "hash must be deterministic for equal keys");

    // Bucket index lands in [1, 256] for many distinct keys (shen-rust's
    // contract is `(h % buckets) + 1`).
    for n in 0..500 {
        let h = int(&mut i, &format!(r#"(hash "k{n}" 256)"#));
        assert!(
            (1..=256).contains(&h),
            "hash {h} out of [1,256] for key {n}"
        );
    }

    // limit 1 collapses to 1.
    assert_eq!(int(&mut i, r#"(hash "x" 1)"#), 1);

    // Distinct keys shouldn't all collide.
    let mut seen = std::collections::HashSet::new();
    for n in 0..100 {
        seen.insert(int(&mut i, &format!(r#"(hash "distinct-{n}" 256)"#)));
    }
    assert!(
        seen.len() >= 40,
        "poor distribution: {} buckets",
        seen.len()
    );
}

// ---------------------------------------------------------------------------
// get-time — both arms must yield a number (mirror of shen-go's TestGetTime).
// ---------------------------------------------------------------------------

#[test]
fn get_time_returns_number() {
    let mut i = Interp::new();
    for kind in ["unix", "run", "real"] {
        let v = ok(&mut i, &format!("(get-time {kind})"));
        assert!(
            v.is_number(),
            "get-time {kind} should be a number, got {v:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// logic / control primitives reachable pre-boot.
// ---------------------------------------------------------------------------

#[test]
fn logic_and_control() {
    let mut i = Interp::new();
    // and / or short-circuit.
    assert!(boolean(&mut i, "(and true (> 5 3))"));
    assert!(!boolean(&mut i, "(and false (> 5 3))"));
    assert!(boolean(&mut i, "(or false true)"));
    // if as a value-producing special form.
    assert_eq!(int(&mut i, "(if true 1 2)"), 1);
    assert_eq!(int(&mut i, "(if false 1 2)"), 2);
    // eval-kl builds and runs (+ 3 4).
    assert_eq!(int(&mut i, "(eval-kl (cons + (cons 3 (cons 4 ()))))"), 7);
}

// ---------------------------------------------------------------------------
// error primitives: simple-error / error-to-string / trap-error.
// ---------------------------------------------------------------------------

#[test]
fn error_primitives() {
    let mut i = Interp::new();
    assert_eq!(
        string(
            &mut i,
            r#"(trap-error (simple-error "boom") (lambda E (error-to-string E)))"#
        ),
        "boom"
    );
    // error-to-string of a non-error is itself an error.
    assert!(run(&mut i, "(error-to-string 5)").is_err());
}
