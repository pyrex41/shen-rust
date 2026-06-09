//! Lever-B existence test: does klcompile AOT-native code beat the loaded
//! engines on REAL loaded user code that runs HOT?
//!
//! The synthetic 3-way bench (`aot_vs_vm_vs_treewalk.rs`) showed AOT/VM =
//! 3.4–5.6x on fib/sumto/cons microworkloads. This bench asks whether that
//! survives contact with a real program: `interpreter.shen`'s
//! `normal-form` — a recursive lambda-calculus term rewriter over cons
//! trees, the same hot queries `warm_typecheck.rs` serves.
//!
//! Method: boot, `(load "interpreter.shen")` for real (datatypes process at
//! read; defuns install on the session engine — tree-walk by default, VM
//! with `SHEN_RUST_VM=1`), verify the two queries, time min-of-N batches.
//! Then install the klcompile-emitted AOT versions of all 10 defuns over
//! the loaded ones (same `register_native` + `register_aot_direct` path a
//! compiled-load would use), re-verify both queries return identical
//! results, re-time. The loaded-vs-AOT comparison is in-process; tree vs VM
//! is across processes (engine is process-global) — drive interleaved pairs
//! from a shell for the cross-engine picture.
//!
//! The AOT module is generated: `(bootstrap "interpreter.shen")` →
//! `klcompile interpreter.kl` (10 compiled, 0 skipped), `crate::` paths
//! rewritten, checked in under `benches/gen/normal_form_aot_gen.rs` so the
//! bench is self-contained and reproducible.
//!
//!   cargo bench --bench normal_form_aot              # tree-walk vs AOT
//!   SHEN_RUST_VM=1 cargo bench --bench normal_form_aot   # VM vs AOT

// Generated code: keep the lint posture of klcompile output.
#![allow(
    unused_variables,
    unused_braces,
    unused_imports,
    clippy::let_and_return,
    clippy::needless_question_mark,
    clippy::redundant_clone,
    clippy::clone_on_copy,
    clippy::needless_late_init,
    clippy::len_zero,
    clippy::needless_borrow,
    clippy::approx_constant,
    clippy::redundant_closure_call,
    non_snake_case
)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use shen_rust::interp::boot::boot_with_kernel;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::{shen_eq, Value};

mod gen {
    include!("gen/normal_form_aot_gen.rs");
}

const ITERS: usize = 10;
const REPS: usize = 30;

const QUERIES: [&str; 2] = [
    // y-combinator ADD 80 4 → 84 (80 recursive reductions)
    "(normal-form [[[y-combinator [/. ADD [/. X [/. Y [if [= X 0] Y [[ADD [-- X]] [++ Y]]]]]]] 80] 4])",
    // y-combinator APPEND [1] [2] → [cons 1 [cons 2 []]]
    "(normal-form [[[y-combinator [/. APPEND [/. X [/. Y [if [= X []] Y [cons [[/. [cons A B] A] X] [[APPEND [[/. [cons A B] B] X]] Y]]]]]]] [cons 1 []]] [cons 2 []]])",
];

fn main() {
    let handle = std::thread::Builder::new()
        .name("normal-form-aot".to_string())
        .stack_size(1024 * 1024 * 1024)
        .spawn(run)
        .expect("spawn bench thread");
    std::process::exit(handle.join().unwrap_or(2));
}

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn eval_src(interp: &mut Interp, src: &str) -> Value {
    let expr = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    interp
        .eval(&expr)
        .unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
}

/// One timed batch: REPS rounds of both queries.
fn batch(interp: &mut Interp) -> Duration {
    let start = Instant::now();
    for _ in 0..REPS {
        for q in &QUERIES {
            std::hint::black_box(eval_src(interp, q));
        }
    }
    start.elapsed()
}

fn min_of(interp: &mut Interp, iters: usize) -> Duration {
    (0..iters).map(|_| batch(interp)).min().expect("iters > 0")
}

fn run() -> i32 {
    let engine = if std::env::var_os("SHEN_RUST_VM").is_some() {
        "vm"
    } else {
        "tree"
    };

    let mut interp = Interp::new();
    let kernel = workspace_root().join("kernel").join("klambda");
    eprint!("normal-form bench booting kernel [{engine}]… ");
    if let Err(e) = boot_with_kernel(&mut interp, &kernel) {
        eprintln!("FAILED\n  {e}");
        return 2;
    }
    eprintln!("ready.");
    interp.register_native("y-or-n?", 1, |_, _| Ok(Value::bool(true)));

    let tests_dir = workspace_root().join("kernel").join("tests");
    std::env::set_current_dir(&tests_dir).expect("chdir kernel/tests");
    eval_src(&mut interp, "(load \"interpreter.shen\")");

    // Reference results on the loaded engine.
    let expected: Vec<Value> = QUERIES.iter().map(|q| eval_src(&mut interp, q)).collect();
    assert_eq!(expected[0].as_int(), Some(84), "ADD query sanity");

    // Warm + time the LOADED engine.
    let _ = batch(&mut interp);
    let loaded = min_of(&mut interp, ITERS);

    // Install the klcompile AOT versions over the loaded defuns —
    // exactly what a compiled-load overlay would do.
    gen::install(&mut interp);

    // Same results, byte-for-byte?
    for (q, exp) in QUERIES.iter().zip(&expected) {
        let got = eval_src(&mut interp, q);
        assert!(shen_eq(&got, exp), "AOT diverged on {q}");
    }

    let _ = batch(&mut interp);
    let aot = min_of(&mut interp, ITERS);

    println!(
        "engine={engine}  loaded(min) {:>9.3} ms/batch   aot(min) {:>9.3} ms/batch   AOT speedup {:.2}x",
        loaded.as_secs_f64() * 1e3,
        aot.as_secs_f64() * 1e3,
        loaded.as_secs_f64() / aot.as_secs_f64()
    );
    0
}
