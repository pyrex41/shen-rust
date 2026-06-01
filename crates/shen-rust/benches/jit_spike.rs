//! GC ladder **rung 3** spike: does a Cranelift JIT, operating on the
//! word-sized `Copy` [`Value`], *materially* beat the interpreter dispatch path?
//!
//! See `design/jit-spike-handoff.md`. The prior rungs (tracing GC, word-sized
//! `Value`) are shipped; the research said a JIT only pays off once `Value` is a
//! single 8-byte word so generated code can hold it in a register and inline
//! fixnum arith + tag tests, FFI-ing to Rust only for heap-touching ops. That
//! prerequisite is now met. This spike measures whether the lever is real.
//!
//! ## What it measures (three shapes × three engines, paired min-of-N)
//!
//! Engines, all producing `shen_eq`-equal results on identical inputs:
//!   * **tree** — the KL body installed as a tree-walked `ClosureKind::Lambda`
//!     and run through `Interp::apply` (what loaded `.shen`/type-checker code
//!     pays; trampoline TCO).
//!   * **aot** — a hand-written `DirectFn` that recurses through the real
//!     direct-call seam (`intern_static` → `get_aot_direct` → indirect call),
//!     exactly mirroring what klcompile emits for an AOT kernel function. This
//!     is the dispatch the profile says dominates kernel-tests. (Rust-recursive,
//!     so the whole harness runs on a 1 GiB-stack thread, like the real binary.)
//!   * **jit** — Cranelift-compiled native code on the `Value` word.
//!
//! Shapes:
//!   * **fib(30)** — non-tail branching recursion, ~2.7M calls at *shallow*
//!     depth: the clean "native vs dispatch" number, no deep-stack artifact.
//!   * **sumto-acc(200000)** — self-tail recursion: validates the JIT's
//!     `CallConv::Tail` + `return_call` TCO mechanism (constant stack) — the
//!     thing Shen's pervasive (and *mutual*) tail recursion needs.
//!   * **build(2000)** — one `cons` per step: measures the `rt_cons` FFI-per-
//!     allocation tax (the JIT's only non-inlined op here).
//!
//! ## Kill-criterion (stated up front, like the GC spikes)
//! The JIT must **materially** beat the AOT-direct dispatch path on the fixnum
//! shape (`fib`): ≥2× is "material" (expecting more); <1.5× would mean
//! setup/FFI overhead dominates and the lever is weaker than the handoff
//! predicts. For `build`, report the JIT/AOT ratio honestly — if cons-FFI
//! collapses it toward ~1×, that is the next-wall finding (→ inline allocation,
//! or tier only arith-heavy leaves).
//!
//! ## The fixnum-as-raw-word trick
//! `Value::int(n)` has bits `n << 3` (tag `000`). So for in-range fixnums,
//! `a + b` and `a - b` are **raw word add/sub** (`(a<<3)+(b<<3) = (a+b)<<3`) and
//! `n == 0` is `bits == 0` — no decode/encode. The spike's JIT exploits this; a
//! production codegen would add a fixnum tag-check + overflow guard (a couple of
//! predicted branches), noted where relevant.
//!
//! Run: `cargo run --release --bench jit_spike`
//! (`harness = false`, so it's an ordinary `main`.)

use std::hint::black_box;
use std::mem::transmute;
use std::rc::Rc;
use std::time::{Duration, Instant};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, AbiParam, InstBuilder, UserFuncName};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module};

use shen_rust::error::ShenResult;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::ast::KlExpr;
use shen_rust::kl::parser::parse_one;
use shen_rust::symbol::SymId;
use shen_rust::value::{shen_eq, Closure, ClosureKind, LambdaBody, Value};

// ---- Value <-> machine word ------------------------------------------------
// `Value` is `#[repr(transparent)]` over `u64` and `Copy`, so reinterpreting it
// as the `i64` the JIT operates on is a layout-valid no-op.

#[inline]
fn w2v(w: i64) -> Value {
    unsafe { transmute::<i64, Value>(w) }
}
#[inline]
fn v2w(v: Value) -> i64 {
    unsafe { transmute::<Value, i64>(v) }
}

/// Fixnum tag is `000`, so `Value::int(n)` bits == `n << 3`.
const FIX_SHIFT: i64 = 3;
const FIX_ONE: i64 = 1 << FIX_SHIFT; // word delta for "+1" / "-1"
const FIX_TWO: i64 = 2 << FIX_SHIFT; // word delta for "-2"
const FIX_2: i64 = 2 << FIX_SHIFT; // bits of `Value::int(2)` (for `n < 2`)

// ===========================================================================
// Runtime helper the JIT calls (the one op `build` doesn't inline).
// ===========================================================================

/// `cons(head, tail)` over the thread-local GC heap. Pure FFI surface for the
/// JIT's allocating shape. `extern "C"` with the host (Apple aarch64 / SysV)
/// ABI so it matches a Cranelift default-callconv import.
extern "C" fn rt_cons(head: i64, tail: i64) -> i64 {
    v2w(Value::cons(w2v(head), w2v(tail)))
}

// ===========================================================================
// The JIT: compile fib / sumto / build to native code.
// ===========================================================================

struct Jit {
    module: JITModule,
}

type Fn2 = extern "C" fn(i64, i64) -> i64;
type Fn1 = extern "C" fn(i64) -> i64;

impl Jit {
    fn new() -> Jit {
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        let flags = settings::Flags::new(fb);
        let isa = cranelift_native::builder()
            .expect("host machine is not a supported Cranelift target")
            .finish(flags)
            .expect("failed to build target ISA");
        let mut jb = JITBuilder::with_isa(isa, default_libcall_names());
        jb.symbol("rt_cons", rt_cons as *const u8);
        Jit {
            module: JITModule::new(jb),
        }
    }

    /// `fib(n) = if n < 2 then n else fib(n-1) + fib(n-2)` — default callconv,
    /// ordinary self `call` (shallow recursion). All arithmetic is raw-word.
    fn compile_fib(&mut self) -> FuncId {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let id = self
            .module
            .declare_function("fib", Linkage::Local, &sig)
            .unwrap();

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let nw = b.block_params(entry)[0];

            let two = b.ins().iconst(types::I64, FIX_2);
            let small = b.ins().icmp(IntCC::SignedLessThan, nw, two);
            let base = b.create_block();
            let rec = b.create_block();
            b.ins().brif(small, base, &[], rec, &[]);

            b.switch_to_block(base);
            b.ins().return_(&[nw]);

            b.switch_to_block(rec);
            let selfref = self.module.declare_func_in_func(id, b.func);
            let one = b.ins().iconst(types::I64, FIX_ONE);
            let two2 = b.ins().iconst(types::I64, FIX_TWO);
            let n1 = b.ins().isub(nw, one);
            let n2 = b.ins().isub(nw, two2);
            let c1 = b.ins().call(selfref, &[n1]);
            let r1 = b.inst_results(c1)[0];
            let c2 = b.ins().call(selfref, &[n2]);
            let r2 = b.inst_results(c2)[0];
            let sum = b.ins().iadd(r1, r2);
            b.ins().return_(&[sum]);

            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(id, &mut ctx).unwrap();
        self.module.clear_context(&mut ctx);
        id
    }

    /// `sumto(n, acc) = if n == 0 then acc else sumto(n-1, acc+n)` as a
    /// `CallConv::Tail` function that self-`return_call`s (true TCO, constant
    /// stack), plus a default-callconv entry trampoline Rust can call.
    fn compile_sumto(&mut self) -> FuncId {
        let tail = self.define_tail_loop("sumto_tail", |b, n, acc, selfref| {
            // acc + n  (raw word add: both are fixnums)
            let acc1 = b.ins().iadd(acc, n);
            let one = b.ins().iconst(types::I64, FIX_ONE);
            let n1 = b.ins().isub(n, one);
            b.ins().return_call(selfref, &[n1, acc1]);
        });
        self.define_entry("sumto", tail)
    }

    /// `build(n, acc) = if n == 0 then acc else build(n-1, cons(n, acc))` —
    /// same tail shape, but each step calls the `rt_cons` FFI helper. Defined
    /// directly (not via `define_tail_loop`) since the body needs the `rt_cons`
    /// import plus the module to declare it.
    fn compile_build(&mut self) -> FuncId {
        // Import rt_cons (default/C callconv to match `extern "C"`).
        let mut cons_sig = self.module.make_signature();
        cons_sig.params.push(AbiParam::new(types::I64));
        cons_sig.params.push(AbiParam::new(types::I64));
        cons_sig.returns.push(AbiParam::new(types::I64));
        let cons_id = self
            .module
            .declare_function("rt_cons", Linkage::Import, &cons_sig)
            .unwrap();
        let tail = self.define_build_tail(cons_id);
        self.define_entry("build", tail)
    }

    /// Tail body for `build` (needs the `rt_cons` import + the module, so it
    /// can't go through the generic `define_tail_loop`).
    fn define_build_tail(&mut self, cons_id: FuncId) -> FuncId {
        let mut sig = self.module.make_signature();
        sig.call_conv = CallConv::Tail;
        sig.params.push(AbiParam::new(types::I64)); // n
        sig.params.push(AbiParam::new(types::I64)); // acc (the list)
        sig.returns.push(AbiParam::new(types::I64));
        let id = self
            .module
            .declare_function("build_tail", Linkage::Local, &sig)
            .unwrap();

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let n = b.block_params(entry)[0];
            let acc = b.block_params(entry)[1];

            let zero = b.ins().iconst(types::I64, 0);
            let done = b.ins().icmp(IntCC::Equal, n, zero);
            let ret = b.create_block();
            let rec = b.create_block();
            b.ins().brif(done, ret, &[], rec, &[]);

            b.switch_to_block(ret);
            b.ins().return_(&[acc]);

            b.switch_to_block(rec);
            let cons_ref = self.module.declare_func_in_func(cons_id, b.func);
            let call = b.ins().call(cons_ref, &[n, acc]);
            let cell = b.inst_results(call)[0];
            let one = b.ins().iconst(types::I64, FIX_ONE);
            let n1 = b.ins().isub(n, one);
            let selfref = self.module.declare_func_in_func(id, b.func);
            b.ins().return_call(selfref, &[n1, cell]);

            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(id, &mut ctx).unwrap();
        self.module.clear_context(&mut ctx);
        id
    }

    /// Define a 2-arg `CallConv::Tail` function whose body is
    /// `if n == 0 { return acc } else { <emit> }`, where `<emit>` ends in a
    /// `return_call` to the function itself. Used for the allocation-free
    /// self-tail shapes.
    fn define_tail_loop<F>(&mut self, name: &str, emit: F) -> FuncId
    where
        F: FnOnce(
            &mut FunctionBuilder,
            cranelift_codegen::ir::Value,   // n
            cranelift_codegen::ir::Value,   // acc
            cranelift_codegen::ir::FuncRef, // self
        ),
    {
        let mut sig = self.module.make_signature();
        sig.call_conv = CallConv::Tail;
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let id = self
            .module
            .declare_function(name, Linkage::Local, &sig)
            .unwrap();

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let n = b.block_params(entry)[0];
            let acc = b.block_params(entry)[1];

            let zero = b.ins().iconst(types::I64, 0);
            let done = b.ins().icmp(IntCC::Equal, n, zero);
            let ret = b.create_block();
            let rec = b.create_block();
            b.ins().brif(done, ret, &[], rec, &[]);

            b.switch_to_block(ret);
            b.ins().return_(&[acc]);

            b.switch_to_block(rec);
            let selfref = self.module.declare_func_in_func(id, b.func);
            emit(&mut b, n, acc, selfref);

            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(id, &mut ctx).unwrap();
        self.module.clear_context(&mut ctx);
        id
    }

    /// A default-callconv (host C ABI) trampoline `entry(n, acc) = tail(n, acc)`
    /// so Rust can call a `CallConv::Tail` recursive function. (A normal `call`
    /// into a tail-callconv callee is the standard host-entry pattern.)
    fn define_entry(&mut self, name: &str, tail_id: FuncId) -> FuncId {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let id = self
            .module
            .declare_function(name, Linkage::Local, &sig)
            .unwrap();

        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let n = b.block_params(entry)[0];
            let acc = b.block_params(entry)[1];
            let callee = self.module.declare_func_in_func(tail_id, b.func);
            let call = b.ins().call(callee, &[n, acc]);
            let r = b.inst_results(call)[0];
            b.ins().return_(&[r]);
            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(id, &mut ctx).unwrap();
        self.module.clear_context(&mut ctx);
        id
    }
}

// ===========================================================================
// Interpreter baselines.
// ===========================================================================

/// Faithful re-implementation of the AOT direct-call seam using only public
/// `Interp` API (`intern_static` + `get_aot_direct`). This is the dispatch the
/// kernel-tests profile is dominated by.
#[inline]
fn apply_direct(interp: &mut Interp, name: &'static str, args: &[Value]) -> ShenResult<Value> {
    let sym = interp.intern_static(name);
    let f = interp
        .get_aot_direct(sym)
        .expect("aot-direct fn must be registered");
    f(interp, args)
}

fn fib_aot(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let n = args[0];
    let nv = n.as_int().unwrap();
    if nv < 2 {
        return Ok(n);
    }
    let a = apply_direct(interp, "fib", &[Value::int(nv - 1)])?;
    let b = apply_direct(interp, "fib", &[Value::int(nv - 2)])?;
    Ok(Value::int(a.as_int().unwrap() + b.as_int().unwrap()))
}

fn sumto_aot(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let n = args[0];
    let acc = args[1];
    if n.as_int() == Some(0) {
        return Ok(acc);
    }
    let n1 = Value::int(n.as_int().unwrap() - 1);
    let acc1 = Value::int(acc.as_int().unwrap() + n.as_int().unwrap());
    apply_direct(interp, "sumto-acc", &[n1, acc1])
}

fn build_aot(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let n = args[0];
    let acc = args[1];
    if n.as_int() == Some(0) {
        return Ok(acc);
    }
    let n1 = Value::int(n.as_int().unwrap() - 1);
    let cell = Value::cons(n, acc);
    apply_direct(interp, "build", &[n1, cell])
}

/// Parse `(defun NAME (PARAMS) BODY)` and install it as a tree-walked
/// `ClosureKind::Lambda` (mirrors `vm_vs_treewalk.rs`).
fn install_lambda(interp: &mut Interp, src: &str) {
    let form = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    let items = match form {
        KlExpr::App(items) => items,
        other => panic!("expected defun, got {other:?}"),
    };
    let name = match &items[1] {
        KlExpr::Sym(s) => *s,
        other => panic!("defun name not a sym: {other:?}"),
    };
    let params: Vec<SymId> = match &items[2] {
        KlExpr::Nil => Vec::new(),
        KlExpr::App(ps) => ps
            .iter()
            .map(|p| match p {
                KlExpr::Sym(s) => *s,
                other => panic!("param not a sym: {other:?}"),
            })
            .collect(),
        other => panic!("param list malformed: {other:?}"),
    };
    let body = items[3].clone();
    let arity = params.len();
    let kind = ClosureKind::Lambda(Rc::new(LambdaBody {
        captured: Vec::new(),
        params,
        body,
    }));
    let closure = Closure {
        name: Some(name),
        arity,
        partial: Vec::new(),
        kind,
    };
    interp.env.set_fn(name, Value::closure(closure));
}

/// Fetch an installed function value (the tree-walked closure) by name.
fn fn_value(interp: &mut Interp, name: &str) -> Value {
    let sym = interp.intern(name);
    interp
        .env
        .get_fn(sym)
        .cloned()
        .unwrap_or_else(|| panic!("{name} not installed"))
}

// ===========================================================================
// Harness.
// ===========================================================================

fn min_of(samples: &[Duration]) -> Duration {
    samples.iter().copied().min().unwrap()
}

fn ms_per_iter(d: Duration, iters: u32) -> f64 {
    d.as_secs_f64() * 1000.0 / iters as f64
}

fn ratio(slow: Duration, fast: Duration) -> f64 {
    slow.as_secs_f64() / fast.as_secs_f64()
}

fn run() {
    println!("Cranelift JIT spike — GC ladder rung 3 (paired alternating, min-of-N)\n");

    // --- Build the interpreter (no kernel boot needed; primitives only) -----
    let mut interp = Interp::new();
    install_lambda(
        &mut interp,
        "(defun fib (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))",
    );
    install_lambda(
        &mut interp,
        "(defun sumto-acc (n acc) (if (= n 0) acc (sumto-acc (- n 1) (+ acc n))))",
    );
    install_lambda(
        &mut interp,
        "(defun build (n acc) (if (= n 0) acc (build (- n 1) (cons n acc))))",
    );
    interp.register_aot_direct("fib", fib_aot);
    interp.register_aot_direct("sumto-acc", sumto_aot);
    interp.register_aot_direct("build", build_aot);

    // --- Compile the JIT engines -------------------------------------------
    let mut jit = Jit::new();
    let fib_id = jit.compile_fib();
    let sumto_id = jit.compile_sumto();
    let build_id = jit.compile_build();
    // Finalize once (the last finalize covers all defs); fetch all pointers.
    jit.module.finalize_definitions().unwrap();
    let jit_fib: Fn1 = unsafe { transmute(jit.module.get_finalized_function(fib_id)) };
    let jit_sumto: Fn2 = unsafe { transmute(jit.module.get_finalized_function(sumto_id)) };
    let jit_build: Fn2 = unsafe { transmute(jit.module.get_finalized_function(build_id)) };

    const PAIRS: u32 = 10;

    // ---------------------------------------------------------------- fib(30)
    {
        let arg: i64 = 30;
        let iters: u32 = 8;
        let expect = Value::int(832040);

        let fib_tree = fn_value(&mut interp, "fib");

        // Correctness: all three agree (and aren't optimized away).
        let t = interp.apply(fib_tree, vec![Value::int(arg)]).unwrap();
        let a = apply_direct(&mut interp, "fib", &[Value::int(arg)]).unwrap();
        let j = w2v(jit_fib(v2w(Value::int(arg))));
        assert!(shen_eq(&t, &expect), "fib tree = {t:?}");
        assert!(shen_eq(&a, &expect), "fib aot  = {a:?}");
        assert!(shen_eq(&j, &expect), "fib jit  = {j:?}");

        let (mut tr, mut ao, mut ji) = (vec![], vec![], vec![]);
        for _ in 0..PAIRS {
            let s = Instant::now();
            for _ in 0..iters {
                black_box(
                    interp
                        .apply(fib_tree, vec![Value::int(black_box(arg))])
                        .unwrap(),
                );
            }
            tr.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(apply_direct(&mut interp, "fib", &[Value::int(black_box(arg))]).unwrap());
            }
            ao.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(jit_fib(v2w(Value::int(black_box(arg)))));
            }
            ji.push(s.elapsed());
        }
        report(
            "fib(30) — non-tail recursion, fixnum arith, shallow stack",
            &tr,
            &ao,
            &ji,
            iters,
        );
    }

    // ------------------------------------------------------- sumto-acc(200000)
    {
        let arg: i64 = 200_000;
        let iters: u32 = 60;
        let expect = Value::int(20_000_100_000);

        let sumto_tree = fn_value(&mut interp, "sumto-acc");
        let t = interp
            .apply(sumto_tree, vec![Value::int(arg), Value::int(0)])
            .unwrap();
        let a = apply_direct(&mut interp, "sumto-acc", &[Value::int(arg), Value::int(0)]).unwrap();
        let j = w2v(jit_sumto(v2w(Value::int(arg)), v2w(Value::int(0))));
        assert!(shen_eq(&t, &expect), "sumto tree = {t:?}");
        assert!(shen_eq(&a, &expect), "sumto aot  = {a:?}");
        assert!(shen_eq(&j, &expect), "sumto jit  = {j:?}");

        let (mut tr, mut ao, mut ji) = (vec![], vec![], vec![]);
        for _ in 0..PAIRS {
            let s = Instant::now();
            for _ in 0..iters {
                black_box(
                    interp
                        .apply(sumto_tree, vec![Value::int(black_box(arg)), Value::int(0)])
                        .unwrap(),
                );
            }
            tr.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(
                    apply_direct(
                        &mut interp,
                        "sumto-acc",
                        &[Value::int(black_box(arg)), Value::int(0)],
                    )
                    .unwrap(),
                );
            }
            ao.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(jit_sumto(
                    v2w(Value::int(black_box(arg))),
                    v2w(Value::int(0)),
                ));
            }
            ji.push(s.elapsed());
        }
        report(
            "sumto-acc(200000) — self-tail (JIT: return_call TCO, constant stack)",
            &tr,
            &ao,
            &ji,
            iters,
        );
    }

    // ----------------------------------------------------------- build(2000)
    {
        let arg: i64 = 2000;
        let iters: u32 = 80;

        let build_tree = fn_value(&mut interp, "build");
        let t = interp
            .apply(build_tree, vec![Value::int(arg), Value::nil()])
            .unwrap();
        let a = apply_direct(&mut interp, "build", &[Value::int(arg), Value::nil()]).unwrap();
        let j = w2v(jit_build(v2w(Value::int(arg)), v2w(Value::nil())));
        // All three must build the identical list. `build` conses n while
        // counting DOWN onto the front, so the result is [1, 2, ..., arg]
        // (head = 1, last = arg).
        assert!(shen_eq(&t, &a), "build tree != aot");
        assert!(shen_eq(&a, &j), "build aot != jit");
        // Spot-check: the JIT built a real cons whose head is the last value
        // consed (1) — confirms the rt_cons FFI path produced a live list.
        assert!(j.is_cons(), "build jit head not a cons");
        assert_eq!(j.head().and_then(Value::as_int), Some(1), "build head");

        let (mut tr, mut ao, mut ji) = (vec![], vec![], vec![]);
        for _ in 0..PAIRS {
            let s = Instant::now();
            for _ in 0..iters {
                black_box(
                    interp
                        .apply(build_tree, vec![Value::int(black_box(arg)), Value::nil()])
                        .unwrap(),
                );
            }
            tr.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(
                    apply_direct(
                        &mut interp,
                        "build",
                        &[Value::int(black_box(arg)), Value::nil()],
                    )
                    .unwrap(),
                );
            }
            ao.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(jit_build(
                    v2w(Value::int(black_box(arg))),
                    v2w(Value::nil()),
                ));
            }
            ji.push(s.elapsed());
        }
        report(
            "build(2000) — one cons/step (JIT: rt_cons FFI per alloc)",
            &tr,
            &ao,
            &ji,
            iters,
        );
    }

    // Keep the JIT module alive until every call above has returned.
    drop(jit);

    println!("KILL-CRITERION: JIT must beat AOT-direct dispatch on fib by >=2x to be 'material'.");
    println!("  (>=2x: lever is real -> productionize.  <1.5x: setup/FFI dominates -> re-scope.)");
}

fn report(title: &str, tree: &[Duration], aot: &[Duration], jit: &[Duration], iters: u32) {
    let (tm, am, jm) = (min_of(tree), min_of(aot), min_of(jit));
    println!("{title}");
    println!("  tree : {:>9.4} ms/call", ms_per_iter(tm, iters));
    println!("  aot  : {:>9.4} ms/call", ms_per_iter(am, iters));
    println!("  jit  : {:>9.4} ms/call", ms_per_iter(jm, iters));
    println!(
        "  JIT speedup:  {:>5.2}x vs aot-dispatch   {:>5.2}x vs tree-walk\n",
        ratio(am, jm),
        ratio(tm, jm),
    );
}

fn main() {
    // The AOT-direct baseline is genuinely Rust-recursive (like real AOT code,
    // which leans on the binary's 1 GiB worker stack), and sumto-acc(200000)
    // recurses 200k deep. Run the whole harness on a large-stack thread so that
    // baseline doesn't overflow. (The JIT's return_call gives it constant stack;
    // the tree-walker uses its trampoline — only `aot` needs the headroom.)
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(run)
        .unwrap()
        .join()
        .unwrap();
}
