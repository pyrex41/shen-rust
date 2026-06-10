//! Cranelift JIT — productionization **stage J1** (`design/jit-j1-handoff.md`).
//!
//! This is *not* a code generator (that is J2). It proves the *integration*:
//! one hot, allocation-light, self-tail-recursive kernel leaf — `shen.length-h`
//! — compiled to native code and tiered in through the existing AOT direct-call
//! table, so it executes as JIT'd machine code inside a normal kernel-tests run.
//!
//! The whole module is gated behind the `jit` cargo feature (it pulls in the
//! `cranelift-*` deps), and the tier-in is *additionally* gated at runtime by
//! the `SHEN_RUST_JIT` env var, exactly like the VM's `SHEN_RUST_VM` — so a
//! `--features jit` build behaves identically until the var is set, keeping the
//! whole thing trivially A/B-toggleable and bisectable.
//!
//! ## What J1 deliberately keeps simple
//! * **One function**, hand-written IR (the spike `benches/jit_spike.rs` is the
//!   reference for the Cranelift scaffolding lifted here).
//! * **Zero GC-root exposure**: `shen.length-h` allocates nothing and captures
//!   nothing, and the heap is grow-only in GC Step 3 (collection off), so the
//!   safepoint/roots problem stays firmly in J3.
//! * **Error ABI via a `pending_error` slot** on `Interp` (§3c): a fallible
//!   `rtj_*` helper records the error and returns a sentinel word; JIT'd code
//!   runs on unimpeded; the entry shim checks the slot afterwards. No `Result`
//!   threaded through Cranelift.
//!
//! ## The function (KL → what we emit)
//! ```text
//! (defun shen.length-h (V W)
//!   (cond ((= () V) W)
//!         (true (shen.length-h (tl V) (+ W 1)))))
//! ```
//! `(= () V)` is exactly `V.bits == nil`, since `nil` has a unique bit pattern
//! and `shen_eq(nil, x)` is bit-equality (see `value::shen_eq`). `(tl V)` FFIs
//! to `rt::tl` (heap read — error on non-cons matches exactly). `(+ W 1)` is a
//! guarded inline fixnum add: raw-word `+8` when `W` is a fixnum that stays in
//! range, else FFI to `rt::add` so the fixnum→float overflow semantics match
//! byte-for-byte. The self tail call uses `CallConv::Tail` + `return_call`
//! (constant stack), re-entered from Rust through a default-callconv trampoline.

use std::collections::HashMap;
use std::mem::transmute;
use std::rc::Rc;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, AbiParam, BlockArg, InstBuilder, UserFuncName};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module};

use crate::aot::runtime as rt;
use crate::error::{ShenError, ShenResult};
use crate::interp::eval::Interp;
use crate::kl::ast::KlExpr;
use crate::symbol::SymId;
use crate::value::Value;

pub mod codegen;
pub mod ffi;

// ---- Diagnostic counters (Slice-A reassessment) ---------------------------
// Process-wide execution/creation tallies, reported by `JitEngine::drop` under
// `SHEN_RUST_JIT_STATS`. They answer the decision-relevant question: what
// fraction of closure *executions* the JIT actually serves vs the tree-walker,
// and whether the hottest bodies are bailing. Relaxed is fine — single worker
// thread, and these are coarse diagnostics, not control flow.
use std::sync::atomic::{AtomicU64, Ordering};

/// Times a `ClosureKind::Jit` body ran (`call_jit`). Only ever incremented when
/// the JIT is active, so it adds nothing to the default (JIT-off) hot path. The
/// tree-walk `Lambda` side is intentionally *not* counted here — counting it
/// would tax the default build's hottest path; the served-fraction figure is
/// recorded in the design doc instead.
pub(crate) static JIT_EXEC: AtomicU64 = AtomicU64::new(0);
/// Closure values created with a JIT body.
pub(crate) static JIT_CREATE: AtomicU64 = AtomicU64::new(0);
/// Closure values whose body bailed the JIT (engine present, fell to VM/Lambda).
pub(crate) static BAIL_CREATE: AtomicU64 = AtomicU64::new(0);
/// Calls *from* JIT'd code (via `rtj_apply_*`) whose callee is itself a
/// fully-applied JIT closure — the only edges `return_call_indirect` (Slice C)
/// could turn into native calls. Bounds Slice C's ceiling.
pub(crate) static JIT_CALL_TO_JIT: AtomicU64 = AtomicU64::new(0);
/// Calls from JIT'd code whose callee is NOT a JIT closure (tree-walked / native
/// / partial) — these keep paying the FFI boundary even with Slice C.
pub(crate) static JIT_CALL_TO_OTHER: AtomicU64 = AtomicU64::new(0);

// ===========================================================================
// J2 closure-value ABI — the always-on (cranelift-independent) surface that
// `ClosureKind::Jit` and the interpreter's dispatch sites speak. The Cranelift
// *code generator* that produces these entries lives in `codegen`.
// ===========================================================================

/// Native entry point of a JIT'd closure body. The call convention (see
/// `design/jit-productionization-plan.md` §"Error ABI"):
/// `(interp, errflag, captures_ptr, args_ptr, nargs) -> result_word`.
/// * `errflag` — `*mut u64` the body sets to `1` (via an `rtj_*` helper) when a
///   Shen error occurs; the rich error rides `Interp::pending_error`.
/// * `captures_ptr` / `args_ptr` — `*const u64` over `&[Value]` (zero-copy,
///   `Value` is `#[repr(transparent)]` over `u64`).
pub type JitEntry = extern "C" fn(*mut Interp, *mut u64, *const u64, *const u64, usize) -> u64;

/// A closure body compiled to native code. Mirrors `BytecodeFn`'s role for the
/// VM: the cached, immutable artifact a `ClosureKind::Jit` value points at; the
/// per-creation captured `Value`s live in the `ClosureKind::Jit` vec alongside.
pub struct JitClosure {
    /// Default-callconv host-entry trampoline into the (possibly tail-callconv)
    /// body. Stays valid for the engine's lifetime — the owning `JITModule`
    /// (on `Interp::jit`) outlives every `JitClosure`.
    pub entry: JitEntry,
    pub arity: usize,
    pub name: Option<SymId>,
}

impl std::fmt::Debug for JitClosure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitClosure")
            .field("arity", &self.arity)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// The Rust→native boundary for a fully-applied `ClosureKind::Jit`. Marshals the
/// borrowed capture/arg slices into word pointers (zero-copy), runs the body,
/// and surfaces a `pending_error` as `Err`. Re-entrant: each call owns its
/// `errflag` on its own native frame; the error channel is `Interp`-wide and
/// first-wins (set during J1).
pub fn call_jit(
    interp: &mut Interp,
    jc: &JitClosure,
    captures: &[Value],
    args: &[Value],
) -> ShenResult<Value> {
    if args.len() != jc.arity {
        return Err(ShenError::new(format!(
            "jit: arity mismatch — expected {}, got {}",
            jc.arity,
            args.len()
        )));
    }
    JIT_EXEC.fetch_add(1, Ordering::Relaxed);
    let mut errflag: u64 = 0;
    // SAFETY: `entry` is a finalized code pointer kept alive by the engine's
    // `JITModule` (owned by `interp.jit`, which outlives this call). `Value` is
    // `#[repr(transparent)]` over `u64`, so the slice pointers are valid
    // `*const u64` arrays of exactly `captures.len()` / `args.len()` words.
    let w = (jc.entry)(
        interp as *mut Interp,
        &mut errflag,
        captures.as_ptr() as *const u64,
        args.as_ptr() as *const u64,
        args.len(),
    );
    if errflag != 0 {
        return Err(interp
            .take_pending_error()
            .unwrap_or_else(|| ShenError::new("jit: error flag set without a pending error")));
    }
    Ok(Value::from_word(w))
}

// ---- Value-word constants (must track `value.rs`) --------------------------

/// `Value::nil()` bit pattern (`TAG_NIL`). `(= () v)` ⇔ `v == NIL_BITS`.
const NIL_BITS: i64 = 0b011;
/// Low tag bits; a fixnum is tag `000`.
const TAG_MASK: i64 = 0b111;
/// `Value::int(1)` word: fixnums are `n << 3`, so `+1` is a raw `+8`. Also the
/// second argument handed to `rt::add` on the slow edge.
const FIX_ONE_BITS: i64 = 1 << 3;
/// Largest inline 61-bit fixnum (`FIXNUM_MAX` in `value.rs`).
const FIXNUM_MAX: i64 = (1 << 60) - 1;
/// Highest accumulator *word* for which `+1` stays a fixnum. Above this,
/// `Value::int(acc+1)` would promote to a boxed float, so we must defer to
/// `rt::add` to reproduce the overflow-to-float semantics exactly.
const ADD_INLINE_MAX_BITS: i64 = (FIXNUM_MAX - 1) << 3;

// ===========================================================================
// Runtime FFI surface — `extern "C"` over the exact `rt::` semantics.
// ===========================================================================
//
// `#[inline(never)]` + stable C ABI + `u64` words + `*mut Interp`. On a Shen
// error these set `Interp::pending_error` (first-error-wins) and return a
// `nil` sentinel; the JIT keeps running and the entry shim surfaces the error.

/// `(tl v)` — `rt::tl`. Heap read; errors on a non-cons.
#[inline(never)]
extern "C" fn rtj_tl(interp: *mut Interp, v: u64) -> u64 {
    let vv = Value::from_word(v);
    match rt::tl(&vv) {
        Ok(t) => t.to_word(),
        Err(e) => {
            // SAFETY: `interp` is the live `&mut Interp` the entry shim passed
            // into the JIT call; no aliasing `Value` is held across this.
            unsafe { (*interp).set_pending_error(e) };
            Value::nil().to_word()
        }
    }
}

/// `(+ a b)` — `rt::add` (incl. fixnum→float overflow). Only reached on the
/// slow edge of the inline guard (non-fixnum or near-`FIXNUM_MAX` accumulator).
#[inline(never)]
extern "C" fn rtj_add(interp: *mut Interp, a: u64, b: u64) -> u64 {
    let (av, bv) = (Value::from_word(a), Value::from_word(b));
    match rt::add(&av, &bv) {
        Ok(r) => r.to_word(),
        Err(e) => {
            // SAFETY: see `rtj_tl`.
            unsafe { (*interp).set_pending_error(e) };
            Value::nil().to_word()
        }
    }
}

// ===========================================================================
// The engine: owns the module + the finalized entry pointer.
// ===========================================================================

/// Native code signature for the `shen.length-h` entry trampoline:
/// `(interp, list_word, acc_word) -> result_word`.
type LengthHFn = extern "C" fn(*mut Interp, u64, u64) -> u64;

/// Owns the long-lived `JITModule` (which **must outlive** every finalized code
/// pointer it hands out — a dropped module frees the code) and the finalized
/// entry pointers. Lives on `Interp` so its lifetime matches the interpreter's.
pub struct JitEngine {
    /// The one long-lived module. **Must outlive** every finalized code pointer
    /// it hands out (`length_h` + every cached `JitClosure::entry`). Kept
    /// mutable so runtime closure tier-up can declare/define/finalize more
    /// functions into it on demand.
    module: JITModule,
    /// Finalized native entry for `shen.length-h` (the J1 anchor).
    length_h: LengthHFn,
    /// Runtime closure-body cache, keyed by body address (like the VM's
    /// `closure_cache`). `Some` = compiled; `None` = known-unJITable (so the
    /// hot 1.2M-creation path bails instantly instead of re-running Cranelift).
    /// Each entry holds the `form` guard so the keyed address stays unique/live.
    cache: HashMap<usize, CacheEntry>,
    /// Monotonic suffix for unique Cranelift function names per compiled body.
    counter: u32,
    /// Cumulative wall-time spent in Cranelift compilation (diagnostics: the
    /// cold-start tax of runtime tier-up). Reported by `Drop` under
    /// `SHEN_RUST_JIT_STATS`.
    compile_nanos: u128,
    /// (W1) Named top-level defuns compiled to a native self-tail `return_call`
    /// loop, keyed by their defun `SymId`. Each entry is the finalized
    /// default-callconv host trampoline (the `JitEntry` ABI).
    named_self_tail: HashMap<SymId, JitEntry>,
}

impl Drop for JitEngine {
    fn drop(&mut self) {
        if std::env::var_os("SHEN_RUST_JIT_STATS").is_some() {
            let (compiled, failed) = self.stats();
            let jx = JIT_EXEC.load(Ordering::Relaxed);
            let jc = JIT_CREATE.load(Ordering::Relaxed);
            let bc = BAIL_CREATE.load(Ordering::Relaxed);
            let c2j = JIT_CALL_TO_JIT.load(Ordering::Relaxed);
            let c2o = JIT_CALL_TO_OTHER.load(Ordering::Relaxed);
            let c2j_pct = if c2j + c2o > 0 {
                100.0 * c2j as f64 / (c2j + c2o) as f64
            } else {
                0.0
            };
            eprintln!(
                "[jit] bodies: compiled={compiled} bailed={failed} compile_time={:.3}s\n\
                 [jit] creations: jit={jc} bailed={bc}\n\
                 [jit] executions: jit={jx}\n\
                 [jit] calls-from-jit: to_jit={c2j} to_other={c2o}  → {c2j_pct:.1}% JIT→JIT (Slice C ceiling)",
                self.compile_nanos as f64 / 1e9
            );
        }
    }
}

struct CacheEntry {
    /// `Some((compiled, upval order))` or `None` for a body that bailed.
    result: Option<(Rc<JitClosure>, Vec<SymId>)>,
    /// Keeps the body address (the cache key) alive and unique — see the VM's
    /// `CompiledClosure::_form_guard`.
    _form_guard: Rc<[KlExpr]>,
}

/// Result of probing the closure-body cache.
pub(crate) enum Lookup {
    /// Compiled: the closure + its capture order (to re-gather current values).
    Hit(Rc<JitClosure>, Vec<SymId>),
    /// Known-unJITable — bail to the tree-walker/VM without recompiling.
    Failed,
    /// Not seen yet — compile it.
    Miss,
}

impl JitEngine {
    /// Build the engine: set up the ISA, register the `rtj_*` symbols, compile
    /// `shen.length-h`, finalize, and cache the entry pointer.
    pub fn new() -> JitEngine {
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        let flags = settings::Flags::new(fb);
        let isa = cranelift_native::builder()
            .expect("host machine is not a supported Cranelift target")
            .finish(flags)
            .expect("failed to build target ISA");
        let mut jb = JITBuilder::with_isa(isa, default_libcall_names());
        jb.symbol("rtj1_tl", rtj_tl as *const u8);
        jb.symbol("rtj1_add", rtj_add as *const u8);
        // J2 closure-body runtime helpers (`ffi::*`), distinct names so the
        // shared module's symbol table can't collide with J1's two above.
        for (name, ptr, _n) in ffi::symbol_registry() {
            jb.symbol(name, ptr);
        }
        let mut module = JITModule::new(jb);

        let entry_id = Self::compile_length_h(&mut module);
        module
            .finalize_definitions()
            .expect("cranelift: finalize_definitions failed");
        // SAFETY: `entry_id` was just defined and finalized with the matching
        // C-ABI signature `(i64, i64, i64) -> i64`; `*mut Interp`/`u64` are
        // layout-compatible with `i64` words. The module is moved into the
        // returned struct, so the code stays mapped as long as the pointer.
        let length_h: LengthHFn =
            unsafe { transmute::<*const u8, LengthHFn>(module.get_finalized_function(entry_id)) };

        JitEngine {
            module,
            length_h,
            cache: HashMap::new(),
            counter: 0,
            compile_nanos: 0,
            named_self_tail: HashMap::new(),
        }
    }

    /// (W1) Compile a named top-level defun into a native self-tail
    /// `return_call` loop, finalize it, cache the entry by `name`, and return
    /// the finalized [`JitEntry`]. Returns `Err` (caller does NOT register the
    /// shim) for a body outside Slice-A.
    pub(crate) fn compile_named(
        &mut self,
        interp: &Interp,
        name_str: &str,
        name: SymId,
        params: &[SymId],
        body: &KlExpr,
    ) -> Result<JitEntry, String> {
        let entry_id = codegen::compile_named_self_tail(
            &mut self.module,
            interp,
            name_str,
            name,
            params,
            body,
        )?;
        // ONCE, covering both the tail body and the entry trampoline.
        self.module
            .finalize_definitions()
            .map_err(|e| e.to_string())?;
        let p = self.module.get_finalized_function(entry_id);
        // SAFETY: `entry_id` was just defined+finalized with the 5-word
        // `JitEntry` C-ABI signature; the module is owned by `self` (on
        // `Interp`) and outlives the pointer.
        let entry: JitEntry = unsafe { transmute::<*const u8, JitEntry>(p) };
        self.named_self_tail.insert(name, entry);
        Ok(entry)
    }

    /// (W1) Whether a named defun was compiled to native code (positive
    /// ran-signal for the oracle; `jit_stats().compiled` counts only the
    /// anonymous body cache and would be a false 0 here).
    pub(crate) fn named_compiled(&self, s: SymId) -> bool {
        self.named_self_tail.contains_key(&s)
    }

    /// `(compiled bodies, bailed bodies)` — for tests/diagnostics, to confirm the
    /// JIT path is actually exercised (and how often it bails).
    pub(crate) fn stats(&self) -> (usize, usize) {
        let mut compiled = 0;
        let mut failed = 0;
        for e in self.cache.values() {
            if e.result.is_some() {
                compiled += 1;
            } else {
                failed += 1;
            }
        }
        (compiled, failed)
    }

    /// Probe the closure-body cache by body address. Returns owned clones so the
    /// caller can drop its borrow of `self` before re-gathering captures.
    pub(crate) fn lookup(&self, key: usize) -> Lookup {
        match self.cache.get(&key) {
            None => Lookup::Miss,
            Some(CacheEntry { result: None, .. }) => Lookup::Failed,
            Some(CacheEntry {
                result: Some((jc, names)),
                ..
            }) => Lookup::Hit(Rc::clone(jc), names.clone()),
        }
    }

    /// Compile a runtime closure body to native code and cache the result
    /// (success *or* failure). Returns the compiled closure, or `None` if the
    /// body uses a form the generator doesn't lower yet (the caller falls back
    /// to the VM / tree-walker). Mirrors `try_compile_closure` for the VM.
    pub(crate) fn compile_and_cache(
        &mut self,
        interp: &Interp,
        key: usize,
        params: &[SymId],
        upval_names: &[SymId],
        body: &KlExpr,
        form: &Rc<[KlExpr]>,
    ) -> Option<Rc<JitClosure>> {
        let name = format!("jit_closure_{}", self.counter);
        self.counter += 1;
        let t0 = std::time::Instant::now();
        let result = match codegen::compile_closure_body(
            &mut self.module,
            interp,
            &name,
            params,
            upval_names,
            body,
        ) {
            Ok(func_id) => {
                self.module
                    .finalize_definitions()
                    .expect("cranelift: finalize_definitions failed");
                // SAFETY: `func_id` was just defined+finalized with the
                // `JitEntry` C-ABI signature `(i64,i64,i64,i64,i64)->i64`; the
                // module is owned by `self` (on `Interp`) and outlives the ptr.
                let ptr = self.module.get_finalized_function(func_id);
                let entry: JitEntry = unsafe { transmute::<*const u8, JitEntry>(ptr) };
                let jc = Rc::new(JitClosure {
                    entry,
                    arity: params.len(),
                    name: None,
                });
                self.cache.insert(
                    key,
                    CacheEntry {
                        result: Some((Rc::clone(&jc), upval_names.to_vec())),
                        _form_guard: Rc::clone(form),
                    },
                );
                Some(jc)
            }
            Err(_reason) => {
                // Bail recorded so the hot path never re-attempts this body.
                self.cache.insert(
                    key,
                    CacheEntry {
                        result: None,
                        _form_guard: Rc::clone(form),
                    },
                );
                None
            }
        };
        // Cold-tax diagnostic: wall-time for IR build + define + finalize.
        self.compile_nanos += t0.elapsed().as_nanos();
        result
    }

    /// Compile `shen.length-h` into the module and return the *entry*
    /// (default-callconv trampoline) `FuncId`. Internally defines the
    /// `CallConv::Tail` self-recursive loop body plus the two `rtj_*` imports.
    fn compile_length_h(module: &mut JITModule) -> FuncId {
        // --- imports: rtj_tl(interp, v), rtj_add(interp, a, b) --------------
        let mut tl_sig = module.make_signature();
        tl_sig.params.push(AbiParam::new(types::I64)); // interp
        tl_sig.params.push(AbiParam::new(types::I64)); // v
        tl_sig.returns.push(AbiParam::new(types::I64));
        let tl_id = module
            .declare_function("rtj1_tl", Linkage::Import, &tl_sig)
            .unwrap();

        let mut add_sig = module.make_signature();
        add_sig.params.push(AbiParam::new(types::I64)); // interp
        add_sig.params.push(AbiParam::new(types::I64)); // a
        add_sig.params.push(AbiParam::new(types::I64)); // b
        add_sig.returns.push(AbiParam::new(types::I64));
        let add_id = module
            .declare_function("rtj1_add", Linkage::Import, &add_sig)
            .unwrap();

        let tail_id = Self::define_length_h_tail(module, tl_id, add_id);
        Self::define_length_h_entry(module, tail_id)
    }

    /// The `CallConv::Tail` loop body:
    /// `length_h(interp, v, acc) = if v == nil { acc } else { length_h(interp, tl(v), acc+1) }`.
    fn define_length_h_tail(module: &mut JITModule, tl_id: FuncId, add_id: FuncId) -> FuncId {
        let mut sig = module.make_signature();
        sig.call_conv = CallConv::Tail;
        sig.params.push(AbiParam::new(types::I64)); // interp
        sig.params.push(AbiParam::new(types::I64)); // v   (the list)
        sig.params.push(AbiParam::new(types::I64)); // acc (the count word)
        sig.returns.push(AbiParam::new(types::I64));
        let id = module
            .declare_function("shen_length_h_tail", Linkage::Local, &sig)
            .unwrap();

        let mut ctx = module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let interp = b.block_params(entry)[0];
            let v = b.block_params(entry)[1];
            let acc = b.block_params(entry)[2];

            // base: (= () v)  ⇔  v == NIL_BITS
            let nil = b.ins().iconst(types::I64, NIL_BITS);
            let is_nil = b.ins().icmp(IntCC::Equal, v, nil);
            let ret = b.create_block();
            let rec = b.create_block();
            b.ins().brif(is_nil, ret, &[], rec, &[]);

            b.switch_to_block(ret);
            b.ins().return_(&[acc]);

            // rec: tl(v) first (so its error wins), then acc+1, then tail-call.
            b.switch_to_block(rec);
            let tl_ref = module.declare_func_in_func(tl_id, b.func);
            let tl_call = b.ins().call(tl_ref, &[interp, v]);
            let next_v = b.inst_results(tl_call)[0];

            // Inline fixnum add when `acc` is a fixnum that stays in range;
            // else fall back to `rt::add` for exact overflow-to-float behaviour.
            let tag = b.ins().band_imm(acc, TAG_MASK);
            let is_fix = b.ins().icmp_imm(IntCC::Equal, tag, 0);
            let in_range = b
                .ins()
                .icmp_imm(IntCC::SignedLessThanOrEqual, acc, ADD_INLINE_MAX_BITS);
            let fast_ok = b.ins().band(is_fix, in_range);
            let fast = b.create_block();
            let slow = b.create_block();
            let cont = b.create_block();
            b.append_block_param(cont, types::I64); // acc'
            b.ins().brif(fast_ok, fast, &[], slow, &[]);

            b.switch_to_block(fast);
            let acc_fast = b.ins().iadd_imm(acc, FIX_ONE_BITS);
            b.ins().jump(cont, &[BlockArg::Value(acc_fast)]);

            b.switch_to_block(slow);
            let add_ref = module.declare_func_in_func(add_id, b.func);
            let one = b.ins().iconst(types::I64, FIX_ONE_BITS);
            let add_call = b.ins().call(add_ref, &[interp, acc, one]);
            let acc_slow = b.inst_results(add_call)[0];
            b.ins().jump(cont, &[BlockArg::Value(acc_slow)]);

            b.switch_to_block(cont);
            let acc1 = b.block_params(cont)[0];
            let selfref = module.declare_func_in_func(id, b.func);
            b.ins().return_call(selfref, &[interp, next_v, acc1]);

            b.seal_all_blocks();
            b.finalize();
        }
        module.define_function(id, &mut ctx).unwrap();
        module.clear_context(&mut ctx);
        id
    }

    /// A default-callconv (host C ABI) trampoline so Rust can re-enter the
    /// `CallConv::Tail` loop: `entry(interp, v, acc) = tail(interp, v, acc)`.
    fn define_length_h_entry(module: &mut JITModule, tail_id: FuncId) -> FuncId {
        let mut sig = module.make_signature();
        sig.params.push(AbiParam::new(types::I64)); // interp
        sig.params.push(AbiParam::new(types::I64)); // v
        sig.params.push(AbiParam::new(types::I64)); // acc
        sig.returns.push(AbiParam::new(types::I64));
        let id = module
            .declare_function("shen_length_h", Linkage::Local, &sig)
            .unwrap();

        let mut ctx = module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        let mut fbc = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let interp = b.block_params(entry)[0];
            let v = b.block_params(entry)[1];
            let acc = b.block_params(entry)[2];
            let callee = module.declare_func_in_func(tail_id, b.func);
            let call = b.ins().call(callee, &[interp, v, acc]);
            let r = b.inst_results(call)[0];
            b.ins().return_(&[r]);
            b.seal_all_blocks();
            b.finalize();
        }
        module.define_function(id, &mut ctx).unwrap();
        module.clear_context(&mut ctx);
        id
    }
}

impl Default for JitEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Tier-in: the Rust shim registered in the AOT direct-call table.
// ===========================================================================

/// `DirectFn` shim for `shen.length-h`: marshals the borrowed `&[Value]` into
/// words, calls the finalized JIT entry through the cached pointer, then checks
/// the `pending_error` slot (§3c) and converts a present error into `Err`.
///
/// Mirrors `aot::kernel::sys::aot_shen_x2e_length_x2d_h` for the arity guard so
/// the differential oracle sees identical `Ok`/`Err` for every input.
fn jit_shim_length_h(interp: &mut Interp, args: &[Value]) -> crate::error::ShenResult<Value> {
    if args.len() != 2 {
        return Err(ShenError::new(format!(
            "shen.length-h: expected 2 args, got {}",
            args.len()
        )));
    }
    // Copy the Code pointer out first so the immutable borrow of `interp.jit`
    // ends before we hand `interp` to the JIT call as `*mut`.
    let code = interp
        .jit
        .as_ref()
        .expect("jit engine present when the shim is registered")
        .length_h;
    let v = args[0].to_word();
    let acc = args[1].to_word();
    let w = code(interp as *mut Interp, v, acc);
    if let Some(e) = interp.take_pending_error() {
        return Err(e);
    }
    Ok(Value::from_word(w))
}

/// (W1) `DirectFn` shim for the W1 test fn `shen-w1-sumto` (2-ary): resolve the
/// finalized native entry through `named_self_tail`, call it via the 5-word
/// `JitEntry` ABI (null captures — a top-level defun has no upvals), and surface
/// a `pending_error` as `Err`. The body's else-arm self-call is the native
/// `return_call` (no `rtj_apply_named` FFI hop).
fn jit_shim_w1_sumto(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if args.len() != 2 {
        return Err(ShenError::new(format!(
            "shen-w1-sumto: expected 2 args, got {}",
            args.len()
        )));
    }
    let sym = interp.intern("shen-w1-sumto");
    // Copy the `Copy` fn pointer out, ending the immutable borrow before
    // handing `interp` to the JIT call as `*mut`.
    let entry = interp
        .jit
        .as_ref()
        .expect("jit present")
        .named_self_tail
        .get(&sym)
        .copied()
        .expect("w1 fn compiled");
    let mut errflag: u64 = 0;
    let argw = [args[0].to_word(), args[1].to_word()];
    // 5-word JitEntry: (interp, errflag, captures_ptr=null, args_ptr, nargs).
    let w = entry(
        interp as *mut Interp,
        &mut errflag,
        std::ptr::null(),
        argw.as_ptr(),
        2,
    );
    if errflag != 0 {
        return Err(interp
            .take_pending_error()
            .unwrap_or_else(|| ShenError::new("jit: errflag set without pending error")));
    }
    Ok(Value::from_word(w))
}

/// (W1) Define the `shen-w1-sumto` defun into the env (so the reference / non-JIT
/// dispatch / partial-application fallback exist), compile its body to a native
/// self-tail loop, and — only on a successful compile — override the direct-call
/// slot with [`jit_shim_w1_sumto`]. On a bail the tree-walked defun serves it.
fn install_w1_sumto(interp: &mut Interp) {
    // (a) Define the defun in env (reference + non-JIT + partial fallback).
    let src = "(defun shen-w1-sumto (N ACC) (if (= N 0) ACC (shen-w1-sumto (- N 1) (+ ACC N))))";
    let e = crate::kl::parser::parse_one(src, &mut interp.symbols).expect("w1 defun parses");
    interp.eval(&e).expect("w1 defun installs");

    // (b) Recover (params, body) for the JIT compile by re-parsing the body —
    // the body `KlExpr` is independent of env.
    let params = [interp.intern("N"), interp.intern("ACC")];
    let body = crate::kl::parser::parse_one(
        "(if (= N 0) ACC (shen-w1-sumto (- N 1) (+ ACC N)))",
        &mut interp.symbols,
    )
    .expect("w1 body parses");

    // (c) self_sym = the defun name.
    let self_sym = interp.intern("shen-w1-sumto");

    // (d) Take the engine out to avoid the `&mut self` + `&Interp` borrow clash.
    let mut engine = interp.jit.take().expect("engine present");
    let res = engine.compile_named(interp, "shen_w1_sumto", self_sym, &params, &body);
    interp.jit = Some(engine);

    // (e) Only on Ok: override the direct slot. On Err (a form outside Slice-A)
    // the tree-walked defun from (a) serves it correctly (silent-bail).
    if res.is_ok() {
        interp.register_aot_direct("shen-w1-sumto", jit_shim_w1_sumto);
    }
}

/// Build the JIT engine and tier `shen.length-h` into the direct-call table,
/// overriding the AOT version — but only when `SHEN_RUST_JIT` is set. Call
/// during boot, **after** `aot::kernel::install_all` (so the override sticks).
pub fn install_jit(interp: &mut Interp) {
    if std::env::var_os("SHEN_RUST_JIT").is_none() {
        return;
    }
    if crate::value::gc_request_enabled() {
        // GC Step 4 mutual exclusion: Cranelift's frame spill discipline is
        // unverified against the conservative scan — a JIT'd frame could hold
        // the only reference to a node in a slot the scan cannot prove it
        // sees. `Interp::maybe_enable_gc` refuses in the other order.
        eprintln!(
            "shen-rust: SHEN_RUST_JIT ignored: SHEN_RUST_GC is enabled and \
             JIT frame roots are unverified"
        );
        return;
    }
    interp.jit = Some(Box::new(JitEngine::new()));
    interp.register_aot_direct("shen.length-h", jit_shim_length_h);
    install_w1_sumto(interp);
}

#[cfg(test)]
mod w1_self_edge {
    //! Strong W1 ran-signal: the self-tail `return_call` edge does NOT hit
    //! `rtj_apply_named`/`tally_callee` (ffi.rs:25). The `JIT_CALL_TO_*`
    //! counters are process-global, so this lives in-crate (can read the
    //! `pub(crate)` atomics). A single pure deep run of `shen-w1-sumto` must
    //! add `(0, 0)` to the FFI-edge tallies.

    use super::*;
    use crate::aot::runtime as rt;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn self_tail_edge_bypasses_ffi() {
        // SAFETY: single-threaded test setup.
        std::env::set_var("SHEN_RUST_JIT", "1");
        let mut interp = Interp::new();
        install_jit(&mut interp);
        assert!(interp.jit_active());
        assert!(interp.jit_named_compiled("shen-w1-sumto"));

        let before = (
            JIT_CALL_TO_JIT.load(Relaxed),
            JIT_CALL_TO_OTHER.load(Relaxed),
        );
        // A pure deep self-recursive run: every self-call is the native
        // return_call, so neither FFI-edge counter should move.
        let r = rt::apply_direct(
            &mut interp,
            "shen-w1-sumto",
            &[Value::int(100_000), Value::int(0)],
        )
        .unwrap();
        assert_eq!(r.as_int(), Some(5_000_050_000));
        let after = (
            JIT_CALL_TO_JIT.load(Relaxed),
            JIT_CALL_TO_OTHER.load(Relaxed),
        );
        assert_eq!(
            (after.0 - before.0, after.1 - before.1),
            (0, 0),
            "self-tail return_call must bypass rtj_apply_named/tally_callee"
        );
    }
}
