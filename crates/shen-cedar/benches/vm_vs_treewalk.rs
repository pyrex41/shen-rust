//! Stage-D measurement instrument: VM vs tree-walker on identical workloads.
//!
//! The whole execution-engine plan (`design/perf-handoff.md`) gates on a
//! harness that can reliably resolve a small (~5%) delta despite machine
//! thermal/run variance (~5–12%). A single before/after comparison is
//! untrustworthy. So this harness:
//!
//!   * compiles the *same* parsed KL body two ways — a tree-walked
//!     `ClosureKind::Lambda` and a bytecode `ClosureKind::Bytecode` — and
//!     installs whichever one is under test into the function namespace,
//!   * runs them **alternately** (A, B, A, B, …) so both share the same
//!     thermal state, reporting mean delta *and* min-of-N (the min is the
//!     least-noisy estimate of true cost),
//!   * uses `black_box` to keep the optimizer from folding the work away.
//!
//! Not a criterion bench — no external dep, and the alternating paired
//! design is exactly what the perf investigation found necessary. Run:
//!
//!   cargo run --release --bench vm_vs_treewalk
//!
//! (`harness = false`, so it's an ordinary `main`.)

use std::hint::black_box;
use std::rc::Rc;
use std::time::{Duration, Instant};

use shen_cedar::interp::eval::Interp;
use shen_cedar::kl::parser::parse_one;
use shen_cedar::value::{Closure, ClosureKind, LambdaBody, Value};

/// One benchmark workload: a self-contained set of `defun`s (the last of
/// which is the entry point) plus the call to make and its expected result.
struct Workload {
    name: &'static str,
    /// `(defun NAME (PARAMS) BODY)` forms, in dependency order. Every one
    /// is installed under both engines for a run.
    defuns: &'static [&'static str],
    /// The entry function name and the single integer argument to call it
    /// with.
    entry: &'static str,
    arg: i64,
    /// Expected result (sanity check that both engines agree and that the
    /// optimizer can't have skipped the work).
    expect: Value,
}

#[derive(Clone, Copy, PartialEq)]
enum Engine {
    TreeWalk,
    Vm,
}

/// Parse a `(defun NAME (PARAMS) BODY)` form into (name, params, body).
fn parse_defun(
    interp: &mut Interp,
    src: &str,
) -> (
    shen_cedar::symbol::SymId,
    Vec<shen_cedar::symbol::SymId>,
    shen_cedar::kl::ast::KlExpr,
) {
    use shen_cedar::kl::ast::KlExpr;
    let form = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    let items = match form {
        KlExpr::App(items) => items,
        other => panic!("expected defun app, got {other:?}"),
    };
    // items = [defun, NAME, (PARAMS), BODY]
    assert_eq!(items.len(), 4, "defun must be (defun NAME (PARAMS) BODY)");
    let name = match &items[1] {
        KlExpr::Sym(s) => *s,
        other => panic!("defun name must be sym, got {other:?}"),
    };
    let params: Vec<_> = match &items[2] {
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
    let body = items[3].clone();
    (name, params, body)
}

/// Install every defun in the workload under the chosen engine. Recursive
/// references resolve through the function namespace, so installing each
/// `defun` as the chosen `ClosureKind` is enough to make the whole call
/// graph run on that engine.
fn install(interp: &mut Interp, w: &Workload, engine: Engine) {
    for src in w.defuns {
        let (name, params, body) = parse_defun(interp, src);
        let kind = match engine {
            Engine::TreeWalk => ClosureKind::Lambda(Rc::new(LambdaBody {
                captured: Vec::new(),
                params: params.clone(),
                body,
            })),
            Engine::Vm => {
                let bf = shen_cedar::vm::compile_fn(interp, Some(name), &params, &body)
                    .unwrap_or_else(|e| panic!("compile {}: {e}", w.name));
                ClosureKind::Bytecode(Rc::new(bf), Vec::new())
            }
        };
        let closure = Closure {
            name: Some(name),
            arity: params.len(),
            partial: Vec::new(),
            kind,
        };
        interp.env.set_fn(name, Value::Closure(Rc::new(closure)));
    }
}

/// Run the workload's entry point once and return the result.
fn run_once(interp: &mut Interp, w: &Workload) -> Value {
    let entry = interp.intern(w.entry);
    let f = interp
        .env
        .get_fn(entry)
        .cloned()
        .unwrap_or_else(|| panic!("entry {} not installed", w.entry));
    interp
        .apply(f, vec![Value::Int(black_box(w.arg))])
        .unwrap_or_else(|e| panic!("run {}: {e}", w.name))
}

/// Time `iters` runs of the workload on one engine. Reinstalls the engine
/// fresh each call so the two engines never contaminate each other's
/// function cells.
fn time_engine(interp: &mut Interp, w: &Workload, engine: Engine, iters: u32) -> Duration {
    install(interp, w, engine);
    // Warm up once (and verify correctness).
    let got = run_once(interp, w);
    assert!(
        shen_cedar::value::shen_eq(&got, &w.expect),
        "{} [{}]: expected {:?}, got {:?}",
        w.name,
        engine_name(engine),
        w.expect,
        got
    );
    let start = Instant::now();
    for _ in 0..iters {
        black_box(run_once(interp, w));
    }
    start.elapsed()
}

fn engine_name(e: Engine) -> &'static str {
    match e {
        Engine::TreeWalk => "tree",
        Engine::Vm => "vm",
    }
}

fn main() {
    // Each workload uses only forms the VM compiler supports today:
    // `if`, `let`, arithmetic, comparisons, `cons`/`hd`/`tl`, and calls
    // (incl. self-recursion / self-tail-calls).
    let workloads: &[Workload] = &[
        Workload {
            name: "fib(28) — non-tail recursion + arithmetic",
            defuns: &["(defun fib (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))"],
            entry: "fib",
            arg: 28,
            expect: Value::Int(317811),
        },
        Workload {
            name: "sumto(200000) — self-tail-call accumulator loop",
            defuns: &[
                "(defun sumto-acc (n acc) (if (= n 0) acc (sumto-acc (- n 1) (+ acc n))))",
                "(defun sumto (n) (sumto-acc n 0))",
            ],
            entry: "sumto",
            arg: 200_000,
            // 200000*200001/2
            expect: Value::Int(20_000_100_000),
        },
        Workload {
            name: "count-down-list(4000) — cons build + hd/tl walk",
            defuns: &[
                // build [n, n-1, ..., 1] then sum it by walking with hd/tl
                "(defun build (n acc) (if (= n 0) acc (build (- n 1) (cons n acc))))",
                "(defun walk (xs acc) (if (cons? xs) (walk (tl xs) (+ acc (hd xs))) acc))",
                "(defun count-down-list (n) (walk (build n ()) 0))",
            ],
            entry: "count-down-list",
            arg: 4000,
            expect: Value::Int(8_002_000),
        },
    ];

    // Pairs of alternating runs. More pairs = tighter estimate; each pair
    // shares thermal state.
    const PAIRS: u32 = 12;

    println!("Stage-D microbench — VM vs tree-walker (paired alternating, min-of-N)\n");

    for w in workloads {
        // Size iters per workload so a single timed batch is ~tens of ms
        // (long enough to dwarf timer/dispatch noise, short enough to
        // iterate). Cheap-per-call workloads get more iters.
        let iters: u32 = match w.entry {
            "fib" => 20,
            "sumto" => 60,
            _ => 200,
        };

        let mut tree_samples: Vec<Duration> = Vec::with_capacity(PAIRS as usize);
        let mut vm_samples: Vec<Duration> = Vec::with_capacity(PAIRS as usize);

        // Fresh interp per workload; reuse across pairs (function cells get
        // overwritten by install()).
        let mut interp = Interp::new();

        for _ in 0..PAIRS {
            // Alternate order each pair would be even better, but install()
            // already isolates the engines; keep tree-then-vm for clarity.
            let t = time_engine(&mut interp, w, Engine::TreeWalk, iters);
            let v = time_engine(&mut interp, w, Engine::Vm, iters);
            tree_samples.push(t);
            vm_samples.push(v);
        }

        let tree_min = tree_samples.iter().min().copied().unwrap();
        let vm_min = vm_samples.iter().min().copied().unwrap();
        let tree_mean = mean(&tree_samples);
        let vm_mean = mean(&vm_samples);

        let speedup_min = tree_min.as_secs_f64() / vm_min.as_secs_f64();
        let speedup_mean = tree_mean.as_secs_f64() / vm_mean.as_secs_f64();

        println!("{}", w.name);
        println!(
            "  tree : min {:>9.3} ms   mean {:>9.3} ms   ({} iters x {} pairs)",
            ms(tree_min) / iters as f64,
            ms_dur(tree_mean) / iters as f64,
            iters,
            PAIRS
        );
        println!(
            "  vm   : min {:>9.3} ms   mean {:>9.3} ms",
            ms(vm_min) / iters as f64,
            ms_dur(vm_mean) / iters as f64,
        );
        println!(
            "  VM speedup (min) {:>5.2}x   (mean) {:>5.2}x   {}\n",
            speedup_min,
            speedup_mean,
            verdict(speedup_min),
        );
    }
    println!("GATE: VM must be >= 1.20x (tree/vm) on the min estimate to proceed to A3.");
}

fn mean(ds: &[Duration]) -> Duration {
    let total: Duration = ds.iter().sum();
    total / ds.len() as u32
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn ms_dur(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn verdict(speedup_min: f64) -> &'static str {
    if speedup_min >= 1.20 {
        "[PASS gate]"
    } else if speedup_min >= 1.0 {
        "[faster but under 20% gate]"
    } else {
        "[SLOWER than tree-walker]"
    }
}
