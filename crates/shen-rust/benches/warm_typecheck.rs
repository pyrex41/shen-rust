//! Warm/served measurement: does the bytecode VM beat the tree-walker on a
//! *repeated* type-checking workload at the suite level?
//!
//! Context (`design/perf-next-target-handoff.md`): on the one-shot
//! `--kernel-tests` metric the VM measured neutral/slightly-slower, because
//! the metric never amortizes the VM's runtime compile cost. The warm thesis
//! is that a long-running / served type-checker amortizes that cost and the
//! VM's per-body execution win (1.3–4× on `vm_vs_treewalk` compute loops)
//! shows through. This harness tests that thesis on the *real* dominant
//! workload — the Shen type-checker proving theorems about `c-minus.shen` and
//! `interpreter.shen` — rather than the synthetic compute loops.
//!
//! Design:
//!   * boot the kernel ONCE, then loop the heavy `(tc +)` + two `(load …)`
//!     stanza N times in one process, timing each iteration;
//!   * iteration 1 is cold (boot caches warm, but first closure compiles +
//!     first proof); iterations 2..N are the steady "served" state;
//!   * the engine is process-global (`SHEN_RUST_VM` is read once into a
//!     `OnceCell`), so tree-walk vs VM is an across-process comparison —
//!     drive it with `scripts/warm-bench.sh` for paired min-of-N;
//!   * prints VM coverage (`vm::stats`): what fraction of runtime closure
//!     bodies the VM served vs bailed on. A low coverage means the type
//!     checker's `trap-error`-heavy continuations bail to the tree-walker and
//!     flipping the flag barely touches the hot path.
//!
//!   cargo run --release --bench warm_typecheck            # tree-walker
//!   SHEN_RUST_VM=1 cargo run --release --bench warm_typecheck  # VM
//!
//! (`harness = false`, so it's an ordinary `main`.)

use std::path::PathBuf;
use std::time::{Duration, Instant};

use shen_rust::interp::boot::boot;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::ast::KlExpr;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

/// Number of warm iterations. Iteration 1 is reported separately (cold);
/// the steady-state estimate is min/mean over iterations 2..N.
const ITERS: usize = 8;

fn main() {
    // The type-checker recurses deep through non-tail frames; mirror the
    // 1 GB worker stack the real `--kernel-tests` path uses.
    let handle = std::thread::Builder::new()
        .name("warm-typecheck".to_string())
        .stack_size(1024 * 1024 * 1024)
        .spawn(run)
        .expect("spawn warm-typecheck thread");
    std::process::exit(handle.join().unwrap_or(2));
}

fn run() -> i32 {
    let vm_on = std::env::var_os("SHEN_RUST_VM").is_some();
    let engine = if vm_on { "VM" } else { "tree-walk" };

    let mut interp = Interp::new();
    eprint!("warm-typecheck booting kernel [{engine}]… ");
    if let Err(e) = boot(&mut interp) {
        eprintln!("FAILED\n  {e}");
        return 2;
    }
    eprintln!("ready.");

    interp.register_native("y-or-n?", 1, |_, _args| Ok(Value::bool(true)));

    // chdir into kernel/tests so `(load "c-minus.shen")` resolves with a
    // bare relative path (same as the real kernel-tests driver).
    let tests_dir = workspace_root().join("kernel").join("tests");
    if let Err(e) = std::env::set_current_dir(&tests_dir) {
        eprintln!("chdir {}: {e}", tests_dir.display());
        return 1;
    }

    // ---- COLD phase: load the heavy corpus ONCE, type-checked. -------------
    //
    // Reloading these files is NOT idempotent — Shen `datatype` rules
    // accumulate, so a second `(load "interpreter.shen")` blows the type
    // checker's inference budget ("maximum inferences exceeded"). So the
    // faithful warm/served unit is *load once, serve many queries*, not
    // *reload the corpus N×*. The cold load IS the type-checking workload;
    // its VM coverage is the answer to "does flipping the flag touch the
    // proof-search hot path?".
    crate_reset_stats();
    let cold_start = Instant::now();
    for src in [
        "(tc +)",
        "(load \"c-minus.shen\")",
        "(load \"interpreter.shen\")",
        "(tc -)",
    ] {
        if let Err(e) = eval_src(&mut interp, src) {
            eprintln!("cold load: {src} failed: {e}");
            return 1;
        }
    }
    let cold = cold_start.elapsed();
    let cold_cov = shen_rust::vm::stats::snapshot();

    // ---- WARM phase: serve repeatable queries against the loaded theory. ----
    //
    // `normal-form` runs the lambda-calculus interpreter defined in
    // interpreter.shen — a recursive term-rewriter built from user `defun`s
    // and `lambda`/`freeze` continuations: exactly the runtime-closure
    // execution the VM is supposed to win on, and purely functional so it is
    // safely repeatable (no global redefinition between iterations). Two
    // queries per batch; `REPS` batches per timed iteration to dwarf timer
    // noise.
    const REPS: usize = 60;
    let queries = [
        // y-combinator ADD 80 4 → 84  (80 recursive reductions)
        "(normal-form [[[y-combinator [/. ADD [/. X [/. Y [if [= X 0] Y [[ADD [-- X]] [++ Y]]]]]]] 80] 4])",
        // y-combinator APPEND [1] [2] → [cons 1 [cons 2 []]]
        "(normal-form [[[y-combinator [/. APPEND [/. X [/. Y [if [= X []] Y [cons [[/. [cons A B] A] X] [[APPEND [[/. [cons A B] B] X]] Y]]]]]]] [cons 1 []]] [cons 2 []]])",
    ];

    // Warm the closure cache / type info with one untimed batch. Reset
    // first so the snapshot afterward counts exactly the closure bodies the
    // *queries* introduce at runtime (those not already compiled when
    // interpreter.shen loaded) — the execution-phase coverage.
    crate_reset_stats();
    for q in &queries {
        if let Err(e) = eval_src(&mut interp, q) {
            eprintln!("warm query failed: {q}\n  {e}");
            return 1;
        }
    }
    let exec_cov = shen_rust::vm::stats::snapshot();

    let mut samples: Vec<Duration> = Vec::with_capacity(ITERS);
    for i in 0..ITERS {
        let start = Instant::now();
        for _ in 0..REPS {
            for q in &queries {
                if let Err(e) = eval_src(&mut interp, q) {
                    eprintln!("iter {i}: query failed: {e}");
                    return 1;
                }
            }
        }
        let dt = start.elapsed();
        samples.push(dt);
        eprintln!(
            "  warm iter {:>2}: {:>8.3} ms ({} batches)",
            i,
            ms(dt),
            REPS
        );
    }

    let warm_min = samples.iter().min().copied().unwrap();
    let warm_mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    let per_batch_min = ms(warm_min) / REPS as f64;

    println!("\n==== warm_typecheck [{engine}] ====");
    println!("cold load (1×)          : {:>9.3} ms", ms(cold));
    println!(
        "warm exec ({} batches)   : min {:>9.3} ms   mean {:>9.3} ms   (per-batch min {:.4} ms)",
        REPS,
        ms(warm_min),
        ms(warm_mean),
        per_batch_min,
    );
    if vm_on {
        println!(
            "VM coverage @ type-check: closures {}/{} ({:.1}%)   defuns {}/{}",
            cold_cov.closure_served,
            cold_cov.closure_served + cold_cov.closure_bailed,
            cold_cov.closure_coverage() * 100.0,
            cold_cov.defun_served,
            cold_cov.defun_served + cold_cov.defun_bailed,
        );
        let exec_new = exec_cov.closure_served
            + exec_cov.closure_bailed
            + exec_cov.defun_served
            + exec_cov.defun_bailed;
        println!(
            "VM coverage @ execution : {exec_new} new bodies compiled by the queries \
             (0 = served entirely from load-time compiles)"
        );
    }
    0
}

/// `vm::stats` is process-global; reset before each timed iteration so the
/// final snapshot reflects one steady-state iteration's coverage (not the
/// accumulation across all of them — body addresses repeat per load).
fn crate_reset_stats() {
    shen_rust::vm::stats::reset();
}

/// Dispatch a source line through the kernel's own `eval` (macro expansion +
/// process-applications), exactly like the REPL / kernel-tests driver.
fn eval_src(interp: &mut Interp, src: &str) -> Result<Value, shen_rust::error::ShenError> {
    let expr = parse_one(src, &mut interp.symbols)
        .map_err(|e| shen_rust::error::ShenError::new(format!("parse {src:?}: {e}")))?;
    let eval_sym = interp.intern("eval");
    if interp.env.get_fn(eval_sym).is_some() {
        let quoted = klexpr_to_value(&expr);
        let f = interp.env.get_fn(eval_sym).cloned().unwrap();
        interp.apply(f, vec![quoted])
    } else {
        interp.eval(&expr)
    }
}

fn klexpr_to_value(e: &KlExpr) -> Value {
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

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/shen-rust
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root above crates/shen-rust")
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
