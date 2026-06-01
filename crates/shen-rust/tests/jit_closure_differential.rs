//! JIT closure differential oracle — stage J2, Slice A
//! (`design/jit-productionization-plan.md`).
//!
//! For a corpus of expressions that create and apply runtime `lambda`/`freeze`
//! closures, we assert the result with the JIT engine installed is `shen_eq`-equal
//! **and** `Ok`/`Err`-matching to the tree-walked reference. The JIT lowers each
//! supported closure body to native `ClosureKind::Jit`; unsupported bodies (e.g.
//! nested closures) bail to the tree-walker, so those entries verify the bail path
//! stays correct too.
//!
//! Reference = a plain `Interp` (no engine installed → `try_compile_jit` is a
//! no-op → every closure is tree-walked `ClosureKind::Lambda`). Subject = an
//! `Interp` with `jit::install_jit` run under `SHEN_RUST_JIT`.
//!
//! Gated on the `jit` feature; a default build compiles it away.
#![cfg(feature = "jit")]

use shen_rust::error::ShenResult;
use shen_rust::interp::eval::Interp;
use shen_rust::jit;
use shen_rust::value::{shen_eq, Value};

fn run(interp: &mut Interp, src: &str) -> ShenResult<Value> {
    let e = shen_rust::kl::parser::parse_one(src, &mut interp.symbols)
        .expect("corpus expression should parse");
    interp.eval(&e)
}

#[test]
fn jit_closures_match_tree_walk() {
    // `install_jit` is env-gated; set it before building the subject interpreter.
    // SAFETY: single-threaded test setup before any interpreter exists.
    std::env::set_var("SHEN_RUST_JIT", "1");

    // Each entry is a self-contained top-level expression. Those that need a
    // helper function define it with `do` + `defun` (run tree-walked at top
    // level; the lambda inside is what gets JIT'd).
    let corpus: &[(&str, &str)] = &[
        // --- literals + if (cond is fallible: is_truthy) -------------------
        ("if_cons", "((lambda X (if (cons? X) (hd X) X)) (cons 1 2))"),
        ("if_noncons", "((lambda X (if (cons? X) (hd X) X)) 42)"),
        ("identity_int", "((lambda X X) 7)"),
        ("identity_str", "((lambda X X) \"hello\")"),
        ("identity_nil", "((lambda X X) ())"),
        // --- primitives ----------------------------------------------------
        ("add1", "((lambda X (+ X 1)) 5)"),
        ("arith_mix", "((lambda X (- (* X 2) 3)) 10)"),
        ("compare", "((lambda X (if (< X 5) X 0)) 3)"),
        ("cons_build", "((lambda X (cons X (cons X ()))) 9)"),
        // --- captures (upvals) ---------------------------------------------
        ("capture", "(let Y 10 ((lambda X (+ X Y)) 5))"),
        (
            "capture2",
            "(let A 1 (let B 2 ((lambda X (+ X (+ A B))) 100)))",
        ),
        // --- let / do in body ----------------------------------------------
        ("let_body", "((lambda X (let Z (* X 2) (+ Z 1))) 4)"),
        ("do_body", "((lambda X (do (+ X 1) (* X 2))) 5)"),
        // --- cond ----------------------------------------------------------
        (
            "cond",
            "((lambda X (cond ((= X 0) 100) ((= X 1) 200) (true 300))) 1)",
        ),
        // --- and / or return the RAW second operand ------------------------
        ("and_raw", "((lambda X (and (number? X) X)) 7)"),
        ("or_short", "((lambda X (or (number? X) X)) 7)"),
        ("or_raw", "((lambda X (or (cons? X) X)) 7)"),
        // --- named call (apply_named) --------------------------------------
        (
            "named_call",
            "(do (defun jt-inc (N) (+ N 1)) ((lambda X (jt-inc X)) 5))",
        ),
        // --- value application (apply_value) -------------------------------
        ("value_apply", "((lambda F (F 5)) (lambda X (+ X 1)))"),
        // --- freeze / thaw (0-arity closure) -------------------------------
        ("freeze_thaw", "(thaw (freeze (+ 1 2)))"),
        // --- error paths (must Err identically) ----------------------------
        ("err_hd", "((lambda X (hd X)) 5)"),
        (
            "err_named",
            "(do (defun jt-boom (N) (hd N)) ((lambda X (jt-boom X)) 5))",
        ),
        ("err_add", "((lambda X (+ X 1)) (cons 1 ()))"),
        // --- nested closure: outer body bails to tree-walk, still correct --
        ("nested_apply", "(((lambda X (lambda Y (+ X Y))) 3) 4)"),
        ("nested_cons", "(((lambda X (lambda Y (cons X Y))) 1) 2)"),
    ];

    let mut total_compiled = 0;
    for (label, src) in corpus {
        let mut refi = Interp::new(); // tree-walk reference (no engine)
        let mut jiti = Interp::new();
        jit::install_jit(&mut jiti);
        assert!(
            jiti.jit_active(),
            "[{label}] jit engine should be installed"
        );

        let want = run(&mut refi, src);
        let got = run(&mut jiti, src);

        match (&want, &got) {
            (Ok(a), Ok(b)) => assert!(
                shen_eq(a, b),
                "[{label}] JIT result {b:?} != tree-walk {a:?}  (src: {src})"
            ),
            (Err(_), Err(_)) => {}
            (Ok(a), Err(e)) => {
                panic!("[{label}] tree-walk Ok({a:?}) but JIT Err({e:?})  (src: {src})")
            }
            (Err(e), Ok(b)) => {
                panic!("[{label}] tree-walk Err({e:?}) but JIT Ok({b:?})  (src: {src})")
            }
        }

        let (compiled, _failed) = jiti.jit_stats().expect("engine installed");
        total_compiled += compiled;
    }

    // Sanity: the JIT path must actually be exercised, not silently bailing on
    // every body (which would make the oracle vacuously pass).
    assert!(
        total_compiled > 0,
        "expected the JIT to compile at least one closure body across the corpus"
    );
}
