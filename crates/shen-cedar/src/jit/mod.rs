//! Cranelift JIT — productionization **stage J1** (`design/jit-j1-handoff.md`).
//!
//! This is *not* a code generator (that is J2). It proves the *integration*:
//! one hot, allocation-light, self-tail-recursive kernel leaf — `shen.length-h`
//! — compiled to native code and tiered in through the existing AOT direct-call
//! table, so it executes as JIT'd machine code inside a normal kernel-tests run.
//!
//! The whole module is gated behind the `jit` cargo feature (it pulls in the
//! `cranelift-*` deps), and the tier-in is *additionally* gated at runtime by
//! the `SHEN_CEDAR_JIT` env var, exactly like the VM's `SHEN_CEDAR_VM` — so a
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

use std::mem::transmute;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, AbiParam, BlockArg, InstBuilder, UserFuncName};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module};

use crate::aot::runtime as rt;
use crate::error::ShenError;
use crate::interp::eval::Interp;
use crate::value::Value;

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
    /// Kept alive for the lifetime of the engine: dropping it invalidates
    /// `length_h`. Never used after construction, hence the leading underscore.
    _module: JITModule,
    /// Finalized native entry for `shen.length-h`.
    length_h: LengthHFn,
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
        jb.symbol("rtj_tl", rtj_tl as *const u8);
        jb.symbol("rtj_add", rtj_add as *const u8);
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
            _module: module,
            length_h,
        }
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
            .declare_function("rtj_tl", Linkage::Import, &tl_sig)
            .unwrap();

        let mut add_sig = module.make_signature();
        add_sig.params.push(AbiParam::new(types::I64)); // interp
        add_sig.params.push(AbiParam::new(types::I64)); // a
        add_sig.params.push(AbiParam::new(types::I64)); // b
        add_sig.returns.push(AbiParam::new(types::I64));
        let add_id = module
            .declare_function("rtj_add", Linkage::Import, &add_sig)
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

/// Build the JIT engine and tier `shen.length-h` into the direct-call table,
/// overriding the AOT version — but only when `SHEN_CEDAR_JIT` is set. Call
/// during boot, **after** `aot::kernel::install_all` (so the override sticks).
pub fn install_jit(interp: &mut Interp) {
    if std::env::var_os("SHEN_CEDAR_JIT").is_none() {
        return;
    }
    interp.jit = Some(Box::new(JitEngine::new()));
    interp.register_aot_direct("shen.length-h", jit_shim_length_h);
}
