//! DECISIVE 3-way microbench: tree-walk vs bytecode VM vs AOT-native.
//!
//! Question: does klcompile-style AOT-native code (offline-compiled loaded
//! defuns) actually BEAT the shipped bytecode VM on hot compute defuns?
//!
//! Methodology is lifted verbatim from `vm_vs_treewalk.rs`: paired
//! alternating runs (now THREE engines per pair — tree, vm, aot, alternated
//! within each pair to share thermal state), `black_box`, min-of-N (>= 12
//! pairs), the per-workload iter counts, a 1 GB worker-stack thread (fib(28)
//! tree-walk recurses deep), and a result-equality sanity check across all
//! three engines.
//!
//! FIDELITY: the AOT engine is NOT hand-written Rust recursion. The AOT
//! functions below were emitted by `cargo run --release -p klcompile -- ...`
//! on the exact same three workloads, then transcribed into this bench with
//! only the module path rewritten (`crate::` -> `shen_rust::`) — the body is
//! byte-for-byte the klcompile output: arithmetic/cons/hd/tl inline to
//! `rt::add/sub/lt/eq/cons/hd/tl/is_cons`, and every recursive / cross-fn
//! call goes through `rt::apply_direct(interp, "NAME", &[..])?` against the
//! raw fn-pointer table. The functions are registered via
//! `interp.register_native` + `interp.register_aot_direct` exactly as the
//! generated `install()` does, so `apply_direct` resolves them through the
//! real loaded-AOT path (boxed Values, no native Rust recursion edge).
//!
//!   cargo run --release --bench aot_vs_vm_vs_treewalk
//!
//! (`harness = false`, so it's an ordinary `main`.)

// The AOT sections are klcompile output transcribed verbatim; keep the
// lint posture of generated code rather than rewriting the emitted shape.
#![allow(clippy::clone_on_copy, clippy::redundant_clone)]

use std::hint::black_box;
use std::rc::Rc;
use std::time::{Duration, Instant};

use shen_rust::aot::runtime as rt;
use shen_rust::error::{ShenError, ShenResult};
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::{Closure, ClosureKind, LambdaBody, Value};

// ====================================================================
// AOT functions — TRANSCRIBED VERBATIM from klcompile output.
// Source: cargo run --release -p klcompile -- /tmp/aot_bench_workloads.kl
// Only change vs generated.rs: `crate::` -> `shen_rust::` (bench is a
// separate crate). Bodies are unchanged: rt:: inline helpers + apply_direct.
// ====================================================================

/// AOT-compiled from KL `(defun fib ...)`
pub fn aot_fib(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if args.len() != 1 {
        return Err(ShenError::new(format!(
            "fib: expected 1 args, got {}",
            args.len()
        )));
    }
    #[allow(unused_mut)]
    let mut v_n = args[0].clone();
    #[allow(clippy::never_loop)]
    loop {
        {
            let __t2 = {
                let __t0 = v_n.clone();
                let __t1 = Value::int(2i64);
                rt::lt(&__t0, &__t1)?
            };
            if match rt::is_truthy(interp, &__t2) {
                Ok(b) => b,
                Err(e) => break Err(e),
            } {
                break Ok(v_n.clone());
            } else {
                break Ok({
                    let __t6 = {
                        let __t5 = {
                            let __t3 = v_n.clone();
                            let __t4 = Value::int(1i64);
                            rt::sub(&__t3, &__t4)?
                        };
                        rt::apply_direct(interp, "fib", &[__t5])?
                    };
                    let __t10 = {
                        let __t9 = {
                            let __t7 = v_n.clone();
                            let __t8 = Value::int(2i64);
                            rt::sub(&__t7, &__t8)?
                        };
                        rt::apply_direct(interp, "fib", &[__t9])?
                    };
                    rt::add(&__t6, &__t10)?
                });
            }
        }
    }
}

/// AOT-compiled from KL `(defun sumto-acc ...)`
pub fn aot_sumto_x2d_acc(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if args.len() != 2 {
        return Err(ShenError::new(format!(
            "sumto-acc: expected 2 args, got {}",
            args.len()
        )));
    }
    #[allow(unused_mut)]
    let mut v_n = args[0].clone();
    #[allow(unused_mut)]
    let mut v_acc = args[1].clone();
    #[allow(clippy::never_loop)]
    loop {
        {
            let __t13 = {
                let __t11 = v_n.clone();
                let __t12 = Value::int(0i64);
                rt::eq(&__t11, &__t12)
            };
            if match rt::is_truthy(interp, &__t13) {
                Ok(b) => b,
                Err(e) => break Err(e),
            } {
                break Ok(v_acc.clone());
            } else {
                {
                    let __t16 = {
                        let __t14 = v_n.clone();
                        let __t15 = Value::int(1i64);
                        rt::sub(&__t14, &__t15)?
                    };
                    let __t19 = {
                        let __t17 = v_acc.clone();
                        let __t18 = v_n.clone();
                        rt::add(&__t17, &__t18)?
                    };
                    v_n = __t16;
                    v_acc = __t19;
                    continue;
                }
            }
        }
    }
}

/// AOT-compiled from KL `(defun sumto ...)`
pub fn aot_sumto(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if args.len() != 1 {
        return Err(ShenError::new(format!(
            "sumto: expected 1 args, got {}",
            args.len()
        )));
    }
    #[allow(unused_mut)]
    let mut v_n = args[0].clone();
    #[allow(clippy::never_loop)]
    loop {
        break Ok({
            let __t20 = v_n.clone();
            let __t21 = Value::int(0i64);
            rt::apply_direct(interp, "sumto-acc", &[__t20, __t21])?
        });
    }
}

/// AOT-compiled from KL `(defun build ...)`
pub fn aot_build(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if args.len() != 2 {
        return Err(ShenError::new(format!(
            "build: expected 2 args, got {}",
            args.len()
        )));
    }
    #[allow(unused_mut)]
    let mut v_n = args[0].clone();
    #[allow(unused_mut)]
    let mut v_acc = args[1].clone();
    #[allow(clippy::never_loop)]
    loop {
        {
            let __t24 = {
                let __t22 = v_n.clone();
                let __t23 = Value::int(0i64);
                rt::eq(&__t22, &__t23)
            };
            if match rt::is_truthy(interp, &__t24) {
                Ok(b) => b,
                Err(e) => break Err(e),
            } {
                break Ok(v_acc.clone());
            } else {
                {
                    let __t27 = {
                        let __t25 = v_n.clone();
                        let __t26 = Value::int(1i64);
                        rt::sub(&__t25, &__t26)?
                    };
                    let __t30 = {
                        let __t28 = v_n.clone();
                        let __t29 = v_acc.clone();
                        rt::cons(&__t28, &__t29)
                    };
                    v_n = __t27;
                    v_acc = __t30;
                    continue;
                }
            }
        }
    }
}

/// AOT-compiled from KL `(defun walk ...)`
pub fn aot_walk(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if args.len() != 2 {
        return Err(ShenError::new(format!(
            "walk: expected 2 args, got {}",
            args.len()
        )));
    }
    #[allow(unused_mut)]
    let mut v_xs = args[0].clone();
    #[allow(unused_mut)]
    let mut v_acc = args[1].clone();
    #[allow(clippy::never_loop)]
    loop {
        {
            let __t32 = {
                let __t31 = v_xs.clone();
                rt::is_cons(&__t31)
            };
            if match rt::is_truthy(interp, &__t32) {
                Ok(b) => b,
                Err(e) => break Err(e),
            } {
                {
                    let __t34 = {
                        let __t33 = v_xs.clone();
                        rt::tl(&__t33)?
                    };
                    let __t38 = {
                        let __t35 = v_acc.clone();
                        let __t37 = {
                            let __t36 = v_xs.clone();
                            rt::hd(&__t36)?
                        };
                        rt::add(&__t35, &__t37)?
                    };
                    v_xs = __t34;
                    v_acc = __t38;
                    continue;
                }
            } else {
                break Ok(v_acc.clone());
            }
        }
    }
}

/// AOT-compiled from KL `(defun count-down-list ...)`
pub fn aot_count_x2d_down_x2d_list(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if args.len() != 1 {
        return Err(ShenError::new(format!(
            "count-down-list: expected 1 args, got {}",
            args.len()
        )));
    }
    #[allow(unused_mut)]
    let mut v_n = args[0].clone();
    #[allow(clippy::never_loop)]
    loop {
        break Ok({
            let __t41 = {
                let __t39 = v_n.clone();
                let __t40 = Value::nil();
                rt::apply_direct(interp, "build", &[__t39, __t40])?
            };
            let __t42 = Value::int(0i64);
            rt::apply_direct(interp, "walk", &[__t41, __t42])?
        });
    }
}

/// Register the AOT functions for a workload. Mirrors the generated
/// `install_*` helpers verbatim: register_native + register_aot_direct so
/// `apply_direct` resolves through the raw fn-pointer table.
fn install_aot(interp: &mut Interp, name: &'static str) {
    match name {
        "fib" => {
            interp.register_native("fib", 1, aot_fib);
            interp.register_aot_direct("fib", aot_fib);
        }
        "sumto" => {
            interp.register_native("sumto-acc", 2, aot_sumto_x2d_acc);
            interp.register_aot_direct("sumto-acc", aot_sumto_x2d_acc);
            interp.register_native("sumto", 1, aot_sumto);
            interp.register_aot_direct("sumto", aot_sumto);
        }
        "count-down-list" => {
            interp.register_native("build", 2, aot_build);
            interp.register_aot_direct("build", aot_build);
            interp.register_native("walk", 2, aot_walk);
            interp.register_aot_direct("walk", aot_walk);
            interp.register_native("count-down-list", 1, aot_count_x2d_down_x2d_list);
            interp.register_aot_direct("count-down-list", aot_count_x2d_down_x2d_list);
        }
        other => panic!("no AOT install for {other}"),
    }
}

// ====================================================================
// Harness (lifted from vm_vs_treewalk.rs, extended to 3 engines).
// ====================================================================

struct Workload {
    name: &'static str,
    defuns: &'static [&'static str],
    entry: &'static str,
    arg: i64,
    expect: Value,
}

#[derive(Clone, Copy, PartialEq)]
enum Engine {
    TreeWalk,
    Vm,
    Aot,
}

fn parse_defun(
    interp: &mut Interp,
    src: &str,
) -> (
    shen_rust::symbol::SymId,
    Vec<shen_rust::symbol::SymId>,
    shen_rust::kl::ast::KlExpr,
) {
    use shen_rust::kl::ast::KlExpr;
    let form = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    let items = match form {
        KlExpr::App(items) => items,
        other => panic!("expected defun app, got {other:?}"),
    };
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

/// Install every defun in the workload under the chosen engine.
fn install(interp: &mut Interp, w: &Workload, engine: Engine) {
    match engine {
        Engine::TreeWalk | Engine::Vm => {
            for src in w.defuns {
                let (name, params, body) = parse_defun(interp, src);
                let kind = match engine {
                    Engine::TreeWalk => ClosureKind::Lambda(Rc::new(LambdaBody {
                        captured: Vec::new(),
                        params: params.clone(),
                        body,
                    })),
                    Engine::Vm => {
                        let bf = shen_rust::vm::compile_fn(interp, Some(name), &params, &body)
                            .unwrap_or_else(|e| panic!("compile {}: {e}", w.name));
                        ClosureKind::Bytecode(Rc::new(bf), Vec::new())
                    }
                    Engine::Aot => unreachable!(),
                };
                let closure = Closure {
                    name: Some(name),
                    arity: params.len(),
                    partial: Vec::new(),
                    kind,
                };
                interp.env.set_fn(name, Value::closure(closure));
            }
        }
        Engine::Aot => {
            // Install the transcribed klcompile output: register_native +
            // register_aot_direct, exactly as generated install() does.
            install_aot(interp, w.entry);
        }
    }
}

fn run_once(interp: &mut Interp, w: &Workload) -> Value {
    let entry = interp.intern(w.entry);
    let f = interp
        .env
        .get_fn(entry)
        .cloned()
        .unwrap_or_else(|| panic!("entry {} not installed", w.entry));
    interp
        .apply(f, vec![Value::int(black_box(w.arg))])
        .unwrap_or_else(|e| panic!("run {}: {e}", w.name))
}

fn time_engine(interp: &mut Interp, w: &Workload, engine: Engine, iters: u32) -> Duration {
    install(interp, w, engine);
    let got = run_once(interp, w);
    assert!(
        shen_rust::value::shen_eq(&got, &w.expect),
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
        Engine::Aot => "aot",
    }
}

fn run() {
    let workloads: &[Workload] = &[
        Workload {
            name: "fib(28) — non-tail recursion + arithmetic",
            defuns: &["(defun fib (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))"],
            entry: "fib",
            arg: 28,
            expect: Value::int(317811),
        },
        Workload {
            name: "sumto(200000) — self-tail-call accumulator loop",
            defuns: &[
                "(defun sumto-acc (n acc) (if (= n 0) acc (sumto-acc (- n 1) (+ acc n))))",
                "(defun sumto (n) (sumto-acc n 0))",
            ],
            entry: "sumto",
            arg: 200_000,
            expect: Value::int(20_000_100_000),
        },
        Workload {
            name: "count-down-list(4000) — cons build + hd/tl walk",
            defuns: &[
                "(defun build (n acc) (if (= n 0) acc (build (- n 1) (cons n acc))))",
                "(defun walk (xs acc) (if (cons? xs) (walk (tl xs) (+ acc (hd xs))) acc))",
                "(defun count-down-list (n) (walk (build n ()) 0))",
            ],
            entry: "count-down-list",
            arg: 4000,
            expect: Value::int(8_002_000),
        },
    ];

    const PAIRS: u32 = 14;

    println!("3-way microbench — tree-walk vs bytecode VM vs AOT-native");
    println!("(paired alternating, min-of-N, {PAIRS} pairs)\n");

    for w in workloads {
        let iters: u32 = match w.entry {
            "fib" => 20,
            "sumto" => 60,
            _ => 200,
        };

        let mut tree_samples: Vec<Duration> = Vec::with_capacity(PAIRS as usize);
        let mut vm_samples: Vec<Duration> = Vec::with_capacity(PAIRS as usize);
        let mut aot_samples: Vec<Duration> = Vec::with_capacity(PAIRS as usize);

        // Fresh interp per workload. Function cells get overwritten by
        // install() each pair; the three engines never coexist in the cell.
        let mut interp = Interp::new();

        // Warm up all three once before timing.
        let _ = time_engine(&mut interp, w, Engine::TreeWalk, 1);
        let _ = time_engine(&mut interp, w, Engine::Vm, 1);
        let _ = time_engine(&mut interp, w, Engine::Aot, 1);

        for _ in 0..PAIRS {
            // Alternate all three within the pair to share thermal state.
            let t = time_engine(&mut interp, w, Engine::TreeWalk, iters);
            let v = time_engine(&mut interp, w, Engine::Vm, iters);
            let a = time_engine(&mut interp, w, Engine::Aot, iters);
            tree_samples.push(t);
            vm_samples.push(v);
            aot_samples.push(a);
        }

        let tree_min = tree_samples.iter().min().copied().unwrap();
        let vm_min = vm_samples.iter().min().copied().unwrap();
        let aot_min = aot_samples.iter().min().copied().unwrap();
        let tree_mean = mean(&tree_samples);
        let vm_mean = mean(&vm_samples);
        let aot_mean = mean(&aot_samples);

        let vm_over_tree = tree_min.as_secs_f64() / vm_min.as_secs_f64();
        let aot_over_tree = tree_min.as_secs_f64() / aot_min.as_secs_f64();
        let aot_over_vm = vm_min.as_secs_f64() / aot_min.as_secs_f64();

        println!("{}", w.name);
        println!(
            "  tree : min {:>9.4} ms   mean {:>9.4} ms   ({} iters x {} pairs)",
            ms(tree_min) / iters as f64,
            ms(tree_mean) / iters as f64,
            iters,
            PAIRS
        );
        println!(
            "  vm   : min {:>9.4} ms   mean {:>9.4} ms",
            ms(vm_min) / iters as f64,
            ms(vm_mean) / iters as f64,
        );
        println!(
            "  aot  : min {:>9.4} ms   mean {:>9.4} ms",
            ms(aot_min) / iters as f64,
            ms(aot_mean) / iters as f64,
        );
        println!(
            "  ratios (min): VM/tree {:>5.2}x   AOT/tree {:>5.2}x   AOT/VM {:>5.2}x   {}\n",
            vm_over_tree,
            aot_over_tree,
            aot_over_vm,
            verdict(aot_over_vm),
        );
    }
    println!("HEADLINE: AOT/VM > 1.00x means AOT-native beats the shipped bytecode VM.");
}

fn main() {
    // fib(28) tree-walk recurses ~28 deep with heavy per-frame state; the
    // VM and AOT use bounded native stack. Give a 1 GB worker stack so the
    // tree-walk arm cannot overflow and skew the comparison.
    let child = std::thread::Builder::new()
        .stack_size(1024 * 1024 * 1024)
        .spawn(run)
        .expect("spawn worker thread");
    child.join().expect("worker thread panicked");
}

fn mean(ds: &[Duration]) -> Duration {
    let total: Duration = ds.iter().sum();
    total / ds.len() as u32
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn verdict(aot_over_vm: f64) -> &'static str {
    if aot_over_vm >= 1.05 {
        "[AOT beats VM]"
    } else if aot_over_vm >= 0.95 {
        "[~parity]"
    } else {
        "[AOT SLOWER than VM]"
    }
}
