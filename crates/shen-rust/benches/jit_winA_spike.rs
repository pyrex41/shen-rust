//! **Win A** spike: do *native bodies with direct native call edges* beat the
//! *FFI-dispatch-per-call* model on cross-function, call-dominated work?
//!
//! See `arch_unboxed_native_codegen.md` (memory) and `design/jit-spike-handoff.md`.
//!
//! ## The question this isolates (and rung-3 did NOT)
//! The rung-3 spike (`jit_spike.rs`) proved native codegen beats interpreter
//! dispatch on **self**-recursion (fib 3.46×, sumto 35.7×) and a cons-FFI shape.
//! It explicitly did **not** test the thing that (a) killed the J2 closure-JIT
//! (−15%) and (b) capped shen-go's compile-to-Go at ~1.9×: a JIT'd function
//! calling **another** JIT'd function. J2 and shen-go both routed every
//! cross-call back through a runtime dispatch/trampoline boundary (an FFI hop per
//! call). **Win A** is the hypothesis that giving compiled functions a shared
//! native ABI — so cross-calls are *direct* machine `call`/`return_call` edges to
//! the other function's code pointer, never an FFI hop — is what removes that tax.
//!
//! This spike builds the same cross-calling functions two ways and times them:
//!   * **jit-direct** — both functions JIT'd; the call edge between them is a
//!     Cranelift `return_call`/`call` to the other's `FuncRef` (direct native).
//!   * **jit-ffi** — both functions JIT'd, but each cross-call leaves native code
//!     for a Rust dispatcher and re-enters (the J2/shen-go shape). For the tail
//!     shape this is a Rust trampoline that re-invokes the JIT each step; for the
//!     non-tail shape it is an `extern "C"` dispatch helper the JIT `call`s.
//!   * **aot** / **tree** — the interpreted dispatch paths, for the 3.55× frame.
//!
//! ## Shapes (call-dominated, minimal body work, so the *call edge* dominates)
//!   * **even?/odd?(N)** — mutual *tail* recursion: every step is a cross-function
//!     tail call, body is just `n=0?` + `n-1`. Tests `return_call` between two
//!     distinct Tail-callconv functions (mutual TCO — the mechanism Shen needs,
//!     and rung-3 only proved the *self* case).
//!   * **ping/pong(N)** — mutual *non-tail* cross-call: `ping(n)=1+pong(n-1)`.
//!     Each step calls the other and *uses its result* — the CPS-continuation
//!     shape the type-checker actually runs. Tests a non-tail direct `call` vs an
//!     `extern "C"` dispatch hop.
//!
//! ## Kill-criterion (stated up front)
//! **jit-direct must materially beat jit-ffi** on the call-dominated shapes. If it
//! does (≥2×), the per-call FFI/dispatch boundary is the wall and a shared native
//! call ABI (Win A) is the real lever → productionize. If jit-direct ≈ jit-ffi,
//! the cross-call boundary is NOT the dominant cost and Win A is weaker than the
//! reframe predicts — a critical negative finding. Report jit-direct vs **aot**
//! too (the interpreted dispatch the kernel-tests profile is dominated by).
//!
//! Run: `cargo bench --features jit --bench jit_winA_spike`
//!
//! ## SPIKE RESULT (2026-06-08) — SPLIT VERDICT, shape-dependent
//! Cranelift 0.132, aarch64, paired min-of-10, all engines `shen_eq`-equal.
//!
//! | shape | jit-direct vs jit-ffi | vs aot-dispatch | vs tree |
//! |---|---|---|---|
//! | even?/odd?(100k) — mutual **tail** | **3.17×** | 36.3× | 269× |
//! | ping/pong(2000) — non-tail **CPS** | **1.14×** | 1.79× | 21.7× |
//!
//! **Tail cross-calls: Win A PASSES big (3.17×).** Direct mutual `return_call`
//! crushes the FFI/trampoline path — the FFI version cannot tail-call through a C
//! boundary, so it pays full call+return+dispatch every step. Mutual `return_call`
//! between two Tail-callconv fns works on aarch64 (rung-3 only proved *self*).
//!
//! **Non-tail CPS cross-calls: Win A is MARGINAL (1.14×) — the load-bearing
//! finding.** This is the shape our type-checker actually runs (call a
//! continuation, use its result). The call-frame setup/teardown cost is identical
//! whether the edge is a direct native `call` or an FFI hop, so removing the hop
//! barely helps. Native codegen of the *bodies* (no per-call intern + dispatch
//! lookup, native arith) is where the modest 1.79×-over-aot comes from — NOT the
//! direct call edge. This is consistent with J2 having lost (−15%) on CPS: the
//! "shared native ABI to avoid FFI hops" thesis is the wrong lever for non-tail
//! continuation code.
//!
//! Caveats: jit-ffi here is an *optimistic* FFI model (thread-local + indirect
//! call), so 1.14× is a lower bound on the direct-vs-FFI delta; a real
//! `call_or_apply` hop is heavier. But the aot column already includes real
//! dispatch, and jit-direct beats it by only 1.79× on CPS — so the ceiling for
//! "native CPS over interpreted CPS" is ~1.8× regardless. The aot "vs" ratios are
//! further inflated by per-call `intern_static` in the baseline seam (directional,
//! not exact). The clean Win A number is jit-direct vs jit-ffi.
//!
//! VERDICT: Win A is real and large for **tail-recursive** code, marginal for
//! **non-tail CPS** (our hot shape). Productionize only if a tail-heavy target is
//! identified; for the type-checker, the direct call edge is not the lever.

use std::cell::Cell;
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

// ---- Value <-> machine word (Value is #[repr(transparent)] over u64, Copy) ---
#[inline]
fn w2v(w: i64) -> Value {
    unsafe { transmute::<i64, Value>(w) }
}
#[inline]
fn v2w(v: Value) -> i64 {
    unsafe { transmute::<Value, i64>(v) }
}

const FIX_SHIFT: i64 = 3;
const FIX_ONE: i64 = 1 << FIX_SHIFT; // word delta for +1 / -1 on fixnums
/// Sentinel returned by the even/odd *step* fn at n==0. Not a multiple of 8, so
/// it can never collide with a fixnum CONT word `(n-1)<<3` (always >= 0).
const STEP_DONE: i64 = -1;

type Fn1 = extern "C" fn(i64) -> i64;

// ===========================================================================
// jit-ffi cross-call plumbing (non-tail ping/pong): the JIT calls these Rust
// dispatch helpers, which re-enter the JIT — modelling the FFI hop per call.
// ===========================================================================

thread_local! {
    /// (ping_ffi_entry, pong_ffi_entry) raw code pointers, set after finalize.
    static PP_FFI: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

extern "C" fn rt_ping(n: i64) -> i64 {
    let p = PP_FFI.with(|c| c.get().0);
    let f: Fn1 = unsafe { transmute(p) };
    f(n)
}
extern "C" fn rt_pong(n: i64) -> i64 {
    let p = PP_FFI.with(|c| c.get().1);
    let f: Fn1 = unsafe { transmute(p) };
    f(n)
}

// ===========================================================================
// The JIT.
// ===========================================================================

struct Jit {
    module: JITModule,
}

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
        jb.symbol("rt_ping", rt_ping as *const u8);
        jb.symbol("rt_pong", rt_pong as *const u8);
        Jit {
            module: JITModule::new(jb),
        }
    }

    fn sig1_tail(&self) -> cranelift_codegen::ir::Signature {
        let mut s = self.module.make_signature();
        s.call_conv = CallConv::Tail;
        s.params.push(AbiParam::new(types::I64));
        s.returns.push(AbiParam::new(types::I64));
        s
    }
    fn sig1_c(&self) -> cranelift_codegen::ir::Signature {
        let mut s = self.module.make_signature();
        s.params.push(AbiParam::new(types::I64));
        s.returns.push(AbiParam::new(types::I64));
        s
    }

    // ---------------------------------------------------------------- even/odd
    /// `parity(n) = if n == 0 then RET_WORD else return_call OTHER(n-1)` as a
    /// Tail-callconv function. `self_id`/`other_id` are pre-declared so the two
    /// can reference each other (mutual `return_call`). The base case returns a
    /// distinct boolean word per function (even→true, odd→false).
    fn define_parity_tail(&mut self, id: FuncId, other_id: FuncId, ret_word: i64) {
        let mut ctx = self.module.make_context();
        ctx.func.signature = self.sig1_tail();
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let n = b.block_params(entry)[0];

            let zero = b.ins().iconst(types::I64, 0);
            let done = b.ins().icmp(IntCC::Equal, n, zero);
            let ret = b.create_block();
            let rec = b.create_block();
            b.ins().brif(done, ret, &[], rec, &[]);

            b.switch_to_block(ret);
            let rw = b.ins().iconst(types::I64, ret_word);
            b.ins().return_(&[rw]);

            b.switch_to_block(rec);
            let other = self.module.declare_func_in_func(other_id, b.func);
            let one = b.ins().iconst(types::I64, FIX_ONE);
            let n1 = b.ins().isub(n, one);
            // DIRECT native tail call to the *other* JIT'd function.
            b.ins().return_call(other, &[n1]);

            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(id, &mut ctx).unwrap();
        self.module.clear_context(&mut ctx);
    }

    /// even?/odd? as direct-cross-calling Tail fns + a C-callconv entry. Returns
    /// the entry FuncId (call with N, get a `Value::bool` word back).
    fn compile_evenodd_direct(&mut self) -> FuncId {
        let even = self
            .module
            .declare_function("eo_even_tail", Linkage::Local, &self.sig1_tail())
            .unwrap();
        let odd = self
            .module
            .declare_function("eo_odd_tail", Linkage::Local, &self.sig1_tail())
            .unwrap();
        self.define_parity_tail(even, odd, v2w(Value::bool(true)));
        self.define_parity_tail(odd, even, v2w(Value::bool(false)));
        self.define_c_entry("eo_entry", even)
    }

    /// One even/odd *step* as a plain C-callconv fn: `step(n) = if n==0 DONE else
    /// n-1`. The jit-ffi engine drives two of these from a Rust trampoline,
    /// modelling "return to the runtime dispatcher between every call".
    fn compile_step(&mut self, name: &str) -> FuncId {
        let id = self
            .module
            .declare_function(name, Linkage::Local, &self.sig1_c())
            .unwrap();
        let mut ctx = self.module.make_context();
        ctx.func.signature = self.sig1_c();
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let n = b.block_params(entry)[0];
            let zero = b.ins().iconst(types::I64, 0);
            let done = b.ins().icmp(IntCC::Equal, n, zero);
            let ret = b.create_block();
            let rec = b.create_block();
            b.ins().brif(done, ret, &[], rec, &[]);
            b.switch_to_block(ret);
            let d = b.ins().iconst(types::I64, STEP_DONE);
            b.ins().return_(&[d]);
            b.switch_to_block(rec);
            let one = b.ins().iconst(types::I64, FIX_ONE);
            let n1 = b.ins().isub(n, one);
            b.ins().return_(&[n1]);
            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(id, &mut ctx).unwrap();
        self.module.clear_context(&mut ctx);
        id
    }

    // --------------------------------------------------------------- ping/pong
    /// `pp(n) = if n==0 then 0 else 1 + <call>(n-1)`. `<call>` is either a direct
    /// `call` to `peer` (when `import` is None → jit-direct) or a `call` to the
    /// imported C dispatch helper `import` (jit-ffi). Non-tail: the result is
    /// used (`+1`), exactly the CPS-continuation shape.
    fn define_pingpong(&mut self, id: FuncId, peer: Option<FuncId>, import: Option<FuncId>) {
        let mut ctx = self.module.make_context();
        ctx.func.signature = self.sig1_c();
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let n = b.block_params(entry)[0];

            let zero = b.ins().iconst(types::I64, 0);
            let done = b.ins().icmp(IntCC::Equal, n, zero);
            let ret = b.create_block();
            let rec = b.create_block();
            b.ins().brif(done, ret, &[], rec, &[]);

            b.switch_to_block(ret);
            let z = b.ins().iconst(types::I64, 0); // Value::int(0) == word 0
            b.ins().return_(&[z]);

            b.switch_to_block(rec);
            let callee = match (peer, import) {
                (Some(p), None) => self.module.declare_func_in_func(p, b.func), // direct
                (None, Some(i)) => self.module.declare_func_in_func(i, b.func), // ffi hop
                _ => unreachable!("exactly one of peer/import"),
            };
            let one = b.ins().iconst(types::I64, FIX_ONE);
            let n1 = b.ins().isub(n, one);
            let call = b.ins().call(callee, &[n1]);
            let r = b.inst_results(call)[0];
            let r1 = b.ins().iadd(r, one); // 1 + peer(n-1)
            b.ins().return_(&[r1]);

            b.seal_all_blocks();
            b.finalize();
        }
        self.module.define_function(id, &mut ctx).unwrap();
        self.module.clear_context(&mut ctx);
    }

    /// ping/pong with DIRECT mutual `call` edges. Returns the `ping` entry id.
    fn compile_pingpong_direct(&mut self) -> FuncId {
        let ping = self
            .module
            .declare_function("pp_ping_d", Linkage::Local, &self.sig1_c())
            .unwrap();
        let pong = self
            .module
            .declare_function("pp_pong_d", Linkage::Local, &self.sig1_c())
            .unwrap();
        self.define_pingpong(ping, Some(pong), None);
        self.define_pingpong(pong, Some(ping), None);
        ping
    }

    /// ping/pong whose cross-calls go through the `rt_ping`/`rt_pong` C dispatch
    /// helpers (FFI hop per call). Returns (ping_entry, pong_entry) so the driver
    /// can publish their pointers into `PP_FFI` for the helpers to re-enter.
    fn compile_pingpong_ffi(&mut self) -> (FuncId, FuncId) {
        let mut imp = |name: &str| {
            self.module
                .declare_function(name, Linkage::Import, &self.sig1_c())
                .unwrap()
        };
        let rt_ping_id = imp("rt_ping");
        let rt_pong_id = imp("rt_pong");
        let ping = self
            .module
            .declare_function("pp_ping_f", Linkage::Local, &self.sig1_c())
            .unwrap();
        let pong = self
            .module
            .declare_function("pp_pong_f", Linkage::Local, &self.sig1_c())
            .unwrap();
        // ping calls rt_pong (→ pong_ffi); pong calls rt_ping (→ ping_ffi).
        self.define_pingpong(ping, None, Some(rt_pong_id));
        self.define_pingpong(pong, None, Some(rt_ping_id));
        (ping, pong)
    }

    /// C-callconv trampoline `entry(n) = tail(n)` so Rust can call a Tail-cc fn.
    fn define_c_entry(&mut self, name: &str, tail_id: FuncId) -> FuncId {
        let id = self
            .module
            .declare_function(name, Linkage::Local, &self.sig1_c())
            .unwrap();
        let mut ctx = self.module.make_context();
        ctx.func.signature = self.sig1_c();
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let n = b.block_params(entry)[0];
            let callee = self.module.declare_func_in_func(tail_id, b.func);
            let call = b.ins().call(callee, &[n]);
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
// Interpreter baselines (tree-walked + AOT-direct dispatch).
// ===========================================================================

#[inline]
fn apply_direct(interp: &mut Interp, name: &'static str, args: &[Value]) -> ShenResult<Value> {
    let sym = interp.intern_static(name);
    let f = interp
        .get_aot_direct(sym)
        .expect("aot-direct fn must be registered");
    f(interp, args)
}

fn even_aot(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let n = args[0].as_int().unwrap();
    if n == 0 {
        return Ok(Value::bool(true));
    }
    apply_direct(interp, "odd?", &[Value::int(n - 1)])
}
fn odd_aot(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let n = args[0].as_int().unwrap();
    if n == 0 {
        return Ok(Value::bool(false));
    }
    apply_direct(interp, "even?", &[Value::int(n - 1)])
}
fn ping_aot(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let n = args[0].as_int().unwrap();
    if n == 0 {
        return Ok(Value::int(0));
    }
    let r = apply_direct(interp, "pong", &[Value::int(n - 1)])?;
    Ok(Value::int(1 + r.as_int().unwrap()))
}
fn pong_aot(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let n = args[0].as_int().unwrap();
    if n == 0 {
        return Ok(Value::int(0));
    }
    let r = apply_direct(interp, "ping", &[Value::int(n - 1)])?;
    Ok(Value::int(1 + r.as_int().unwrap()))
}

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

#[allow(clippy::too_many_arguments)]
fn report4(
    title: &str,
    tree: &[Duration],
    aot: &[Duration],
    ffi: &[Duration],
    direct: &[Duration],
    iters: u32,
) {
    let (tm, am, fm, dm) = (min_of(tree), min_of(aot), min_of(ffi), min_of(direct));
    println!("{title}");
    println!("  tree       : {:>10.4} ms/call", ms_per_iter(tm, iters));
    println!("  aot        : {:>10.4} ms/call", ms_per_iter(am, iters));
    println!("  jit-ffi    : {:>10.4} ms/call", ms_per_iter(fm, iters));
    println!("  jit-direct : {:>10.4} ms/call", ms_per_iter(dm, iters));
    println!(
        "  >>> jit-direct vs jit-ffi: {:>5.2}x   (vs aot-dispatch: {:>5.2}x   vs tree: {:>5.2}x)\n",
        ratio(fm, dm),
        ratio(am, dm),
        ratio(tm, dm),
    );
}

fn run() {
    println!("Win A spike — native bodies w/ DIRECT native call edges vs FFI-dispatch-per-call");
    println!("(paired alternating, min-of-N; jit-direct vs jit-ffi is the Win A number)\n");

    let mut interp = Interp::new();
    install_lambda(
        &mut interp,
        "(defun even? (n) (if (= n 0) true (odd? (- n 1))))",
    );
    install_lambda(
        &mut interp,
        "(defun odd? (n) (if (= n 0) false (even? (- n 1))))",
    );
    install_lambda(
        &mut interp,
        "(defun ping (n) (if (= n 0) 0 (+ 1 (pong (- n 1)))))",
    );
    install_lambda(
        &mut interp,
        "(defun pong (n) (if (= n 0) 0 (+ 1 (ping (- n 1)))))",
    );
    interp.register_aot_direct("even?", even_aot);
    interp.register_aot_direct("odd?", odd_aot);
    interp.register_aot_direct("ping", ping_aot);
    interp.register_aot_direct("pong", pong_aot);

    // --- Compile JIT engines ----------------------------------------------
    let mut jit = Jit::new();
    let eo_direct = jit.compile_evenodd_direct();
    let even_step = jit.compile_step("eo_even_step");
    let odd_step = jit.compile_step("eo_odd_step");
    let pp_direct = jit.compile_pingpong_direct();
    let (pp_ffi_ping, pp_ffi_pong) = jit.compile_pingpong_ffi();
    jit.module.finalize_definitions().unwrap();

    let jit_eo_direct: Fn1 = unsafe { transmute(jit.module.get_finalized_function(eo_direct)) };
    let jit_even_step: Fn1 = unsafe { transmute(jit.module.get_finalized_function(even_step)) };
    let jit_odd_step: Fn1 = unsafe { transmute(jit.module.get_finalized_function(odd_step)) };
    let jit_pp_direct: Fn1 = unsafe { transmute(jit.module.get_finalized_function(pp_direct)) };
    let pp_ffi_ping_ptr = jit.module.get_finalized_function(pp_ffi_ping) as usize;
    let pp_ffi_pong_ptr = jit.module.get_finalized_function(pp_ffi_pong) as usize;
    let jit_pp_ffi_ping: Fn1 = unsafe { transmute(pp_ffi_ping_ptr) };
    PP_FFI.with(|c| c.set((pp_ffi_ping_ptr, pp_ffi_pong_ptr)));

    // jit-ffi even/odd: a Rust trampoline that re-enters the JIT each step
    // (the "return to runtime dispatcher between every call" model). Returns the
    // parity boolean as a Value word.
    let eo_ffi = |n0: i64| -> i64 {
        let mut nw = v2w(Value::int(n0));
        let mut even_turn = true;
        loop {
            let r = if even_turn {
                jit_even_step(nw)
            } else {
                jit_odd_step(nw)
            };
            if r == STEP_DONE {
                // the fn that reached 0 is `even_turn`; even reaching 0 ⇒ N even.
                return v2w(Value::bool(even_turn));
            }
            nw = r;
            even_turn = !even_turn;
        }
    };

    const PAIRS: u32 = 10;

    // -------------------------------------------------- even?/odd? (mutual tail)
    {
        let arg: i64 = 100_000;
        let iters: u32 = 20;
        let expect = Value::bool(arg % 2 == 0);

        let even_tree = fn_value(&mut interp, "even?");
        let t = interp.apply(even_tree, vec![Value::int(arg)]).unwrap();
        let a = apply_direct(&mut interp, "even?", &[Value::int(arg)]).unwrap();
        let f = w2v(eo_ffi(arg));
        let d = w2v(jit_eo_direct(v2w(Value::int(arg))));
        assert!(shen_eq(&t, &expect), "even tree = {t:?}");
        assert!(shen_eq(&a, &expect), "even aot  = {a:?}");
        assert!(shen_eq(&f, &expect), "even ffi  = {f:?}");
        assert!(shen_eq(&d, &expect), "even direct = {d:?}");

        let (mut tr, mut ao, mut fi, mut di) = (vec![], vec![], vec![], vec![]);
        for _ in 0..PAIRS {
            let s = Instant::now();
            for _ in 0..iters {
                black_box(
                    interp
                        .apply(even_tree, vec![Value::int(black_box(arg))])
                        .unwrap(),
                );
            }
            tr.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(
                    apply_direct(&mut interp, "even?", &[Value::int(black_box(arg))]).unwrap(),
                );
            }
            ao.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(eo_ffi(black_box(arg)));
            }
            fi.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(jit_eo_direct(v2w(Value::int(black_box(arg)))));
            }
            di.push(s.elapsed());
        }
        report4(
            "even?/odd?(100000) — mutual TAIL recursion (direct: mutual return_call)",
            &tr,
            &ao,
            &fi,
            &di,
            iters,
        );
    }

    // ------------------------------------------------ ping/pong (mutual non-tail)
    {
        let arg: i64 = 2000;
        let iters: u32 = 200;
        let expect = Value::int(arg);

        let ping_tree = fn_value(&mut interp, "ping");
        let t = interp.apply(ping_tree, vec![Value::int(arg)]).unwrap();
        let a = apply_direct(&mut interp, "ping", &[Value::int(arg)]).unwrap();
        let f = w2v(jit_pp_ffi_ping(v2w(Value::int(arg))));
        let d = w2v(jit_pp_direct(v2w(Value::int(arg))));
        assert!(shen_eq(&t, &expect), "ping tree = {t:?}");
        assert!(shen_eq(&a, &expect), "ping aot  = {a:?}");
        assert!(shen_eq(&f, &expect), "ping ffi  = {f:?}");
        assert!(shen_eq(&d, &expect), "ping direct = {d:?}");

        let (mut tr, mut ao, mut fi, mut di) = (vec![], vec![], vec![], vec![]);
        for _ in 0..PAIRS {
            let s = Instant::now();
            for _ in 0..iters {
                black_box(
                    interp
                        .apply(ping_tree, vec![Value::int(black_box(arg))])
                        .unwrap(),
                );
            }
            tr.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(
                    apply_direct(&mut interp, "ping", &[Value::int(black_box(arg))]).unwrap(),
                );
            }
            ao.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(jit_pp_ffi_ping(v2w(Value::int(black_box(arg)))));
            }
            fi.push(s.elapsed());

            let s = Instant::now();
            for _ in 0..iters {
                black_box(jit_pp_direct(v2w(Value::int(black_box(arg)))));
            }
            di.push(s.elapsed());
        }
        report4(
            "ping/pong(2000) — mutual NON-TAIL cross-call (CPS shape; direct: native call)",
            &tr,
            &ao,
            &fi,
            &di,
            iters,
        );
    }

    drop(jit);

    println!("KILL-CRITERION: jit-direct must materially beat jit-ffi (>=2x) on the");
    println!("  call-dominated shapes -> the per-call FFI/dispatch boundary is the wall,");
    println!("  and a shared native call ABI (Win A) is the real lever -> productionize.");
    println!("  jit-direct ~= jit-ffi -> the boundary is NOT the dominant cost (negative).");
}

fn main() {
    // The aot baseline is genuinely Rust-recursive (mutual), and the jit-ffi
    // ping/pong interleaves JIT+Rust frames; run on a big stack like the binary.
    std::thread::Builder::new()
        .stack_size(1 << 30)
        .spawn(run)
        .unwrap()
        .join()
        .unwrap();
}
