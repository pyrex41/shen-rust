//! Port-authored reader/eval robustness corpus (NOT the canonical kernel
//! suite).
//!
//! Mirror of shen-go's `kl/reader_fuzz_test.go` — translated to a
//! DETERMINISTIC, SEEDED corpus (no `rand`, no Date, no cargo-fuzz / nightly,
//! as the target spec requires). The contract under test:
//!
//!   for ANY input string, the pipeline `parse_one -> Interp::eval -> render`
//!   must terminate WITHOUT an uncaught Rust panic.
//!
//! Errors are fine — they just have to be Shen-level (a `ShenError` returned
//! from `eval`) or a parse error from `parse_one`. The failure mode we hunt
//! is the one that puts a Rust backtrace on a user's stdout: a `panic!`,
//! `unwrap` on `None`, or index-out-of-bounds that escapes the evaluator.
//!
//! Seeds are biased toward malformed Shen — the shape of input most likely
//! to trip a port glitch — including the `(/. _ false)` family from the
//! shen-go investigation, unbalanced parens, and resource-exhaustion inputs.
//! Non-terminating programs are bounded with `Interp::set_budget` so an
//! infinite tail-recursion seed surfaces as a catchable cancellation rather
//! than hanging the test.

use std::panic::{catch_unwind, AssertUnwindSafe};

use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;

/// The deterministic corpus. Each entry must drive reader+eval to a clean
/// terminal state (Ok value, parse error, or Shen error) without panicking.
const CORPUS: &[&str] = &[
    // --- golden path -------------------------------------------------------
    "(+ 1 2)",
    "(let X 1 (+ X 1))",
    "(if true 1 2)",
    "((lambda X (+ X 1)) 41)",
    "(cons 1 (cons 2 ()))",
    // --- error paths that MUST surface as catchable Shen errors -----------
    "(trap-error (overflow->str) (lambda E (error-to-string E)))",
    "(trap-error (value never-bound) (lambda E (error-to-string E)))",
    "(trap-error (if 42 1 2) (lambda E (error-to-string E)))",
    r#"(trap-error (simple-error "x") (lambda E (error-to-string E)))"#,
    "(trap-error (42 1) (lambda E (error-to-string E)))",
    "(trap-error (hd 5) (lambda E (error-to-string E)))",
    "(trap-error (/ 1 0) (lambda E (error-to-string E)))",
    // --- malformed-but-parseable, the (/. _ false) family -----------------
    // `_` as a lambda parameter is illegal Shen; the evaluator must not
    // panic on it (it may error, self-evaluate, or bind — any non-panic
    // terminal state is acceptable).
    "(/. _ false)",
    "(lambda _ false)",
    "(lambda X _)",
    // --- non-termination & resource exhaustion ----------------------------
    // Infinite self-tail-recursion: the step budget turns this into a
    // catchable cancellation instead of a hang.
    "(do (defun loopf (X) (loopf X)) (loopf 0))",
    // Absurd allocation: must error (or be capped), never abort the process.
    "(absvector 100000000000)",
    "(absvector -1)",
    // --- reader edge cases that should at minimum not panic ----------------
    "",
    " ",
    "()",
    "(",
    ")",
    "((",
    "))",
    "(()",
    "())",
    r#""unterminated"#,
    "#\\",
    "(((((((((((",
    ")))))))))))",
    ". . .",
    "(1 . 2 . 3)",
    "(quote)",
    "(let)",
    "(if)",
    "(defun)",
    "(cons)",
    "(a b c d e f g h i j k l m n o p)",
    "garbage tokens with no parens at all",
    "(\u{0000})", // embedded NUL
    "(λ x x)",    // non-ASCII identifier
    "(+ 1 2",     // missing close
    "+ 1 2)",     // missing open
    "(set)",
    "(value)",
];

/// Run one corpus entry through reader + eval under a step budget. Returns
/// `Err(panic_msg)` if a Rust panic escapes; `Ok(())` for any well-behaved
/// terminal state (Ok value, parse error, or Shen error).
fn drive(input: &str) -> Result<(), String> {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // silence the default backtrace dump
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut interp = Interp::new();
        match parse_one(input, &mut interp.symbols) {
            // A parse error is an acceptable terminal state for malformed
            // input — the reader rejected it cleanly.
            Err(_) => {}
            Ok(expr) => {
                // Cap evaluation so a non-terminating seed becomes a
                // catchable cancellation rather than a hung worker.
                interp.set_budget(2_000_000);
                // Either outcome is fine; we only care that it didn't panic.
                let _ = interp.eval(&expr);
            }
        }
    }));
    std::panic::set_hook(prev);
    result.map_err(|e| {
        if let Some(s) = e.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
            s.clone()
        } else {
            "non-string panic payload".to_string()
        }
    })
}

#[test]
fn corpus_never_panics() {
    let mut failures = Vec::new();
    for input in CORPUS {
        if let Err(msg) = drive(input) {
            failures.push(format!("input {input:?} PANICKED: {msg}"));
        }
    }
    assert!(
        failures.is_empty(),
        "reader+eval panicked on {} input(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A focused regression on the exact `(/. _ false)` shape that triggered the
/// shen-go investigation: it must reach a non-panic terminal state.
#[test]
fn underscore_lambda_param_does_not_panic() {
    for input in ["(/. _ false)", "(lambda _ false)", "(lambda _ _)"] {
        assert!(
            drive(input).is_ok(),
            "expected {input:?} to terminate without a panic"
        );
    }
}

/// Unbalanced parentheses, in both directions and at depth, must be a clean
/// parse error (or non-panic eval), never a crash.
#[test]
fn unbalanced_parens_do_not_panic() {
    for input in ["(", ")", "((", "))", "(()", "())", "(((((", ")))))"] {
        assert!(
            drive(input).is_ok(),
            "expected unbalanced {input:?} to terminate without a panic"
        );
    }
}
