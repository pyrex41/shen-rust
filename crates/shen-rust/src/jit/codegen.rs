//! Cranelift code generator — stage J2 (`design/jit-productionization-plan.md`).
//!
//! A backend swap of `crate::vm::compiler`: it walks the same `KlExpr` form set
//! with the same variable-resolution / capture / self-tail rules, but emits
//! Cranelift IR (producing a [`super::JitClosure`]) instead of bytecode ops.
//!
//! ## Slice A coverage (this increment)
//! Single-frame, non-nested closure bodies: literals (immediate nil/bool/sym +
//! in-range fixnum), variables (param SSA / upval load / self-evaluating sym),
//! `if`/`cond`/`and`/`or`/`let`/`do`/`type`, the 18 inlinable primitives via
//! `rtj_*` FFI, and named-call / value-application via `rtj_apply_named` /
//! `rtj_apply_value`. Anything else — nested `lambda`/`freeze`, `trap-error`,
//! `thaw`, `quote`, `defun`, float/string/large-int literals — **bails** (returns
//! `Err`), and the caller falls back to the VM/tree-walker, exactly as the VM's
//! compiler bails. Per-body bail keeps every non-JIT'd body correct.
//!
//! No self-tail-call / `return_call`: an anonymous closure has no name to recurse
//! to (mirrors the VM, whose `current_fn` is `None` for nested frames), so every
//! call FFIs back through `apply`. Stack-safety of deep non-self tail chains is a
//! later concern (measure first; `return_call_indirect` is the remedy).

use std::collections::HashMap;

use cranelift_codegen::ir::{
    types, AbiParam, Block, BlockArg, InstBuilder, MemFlags, StackSlotData, StackSlotKind,
    UserFuncName, Value as IrValue,
};
use cranelift_codegen::isa::CallConv;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::JITModule;
use cranelift_module::{FuncId, Linkage, Module};

use crate::interp::eval::Interp;
use crate::kl::ast::KlExpr;
use crate::symbol::SymId;
use crate::value::Value;

/// Compile a runtime closure body into the module and return its entry `FuncId`.
/// The entry has the [`super::JitEntry`] ABI: `(interp, errflag, captures_ptr,
/// args_ptr, nargs) -> result_word`, default call convention. Returns `Err` (the
/// caller bails to the VM/tree-walker) for any form outside the Slice-A subset;
/// on that path nothing is declared/defined, so the module stays clean.
pub(crate) fn compile_closure_body(
    module: &mut JITModule,
    interp: &Interp,
    name: &str,
    params: &[SymId],
    upval_names: &[SymId],
    body: &KlExpr,
) -> Result<FuncId, String> {
    if params.len() > u8::MAX as usize {
        return Err("jit: too many params".into());
    }

    let mut sig = module.make_signature();
    for _ in 0..5 {
        sig.params.push(AbiParam::new(types::I64)); // interp, errflag, captures, args, nargs
    }
    sig.returns.push(AbiParam::new(types::I64));

    let mut ctx = module.make_context();
    ctx.func.signature = sig.clone();
    // Placeholder name during construction; the real id-derived name is set
    // only after a successful build, just before `define_function`. This lets a
    // mid-build bail return `Err` without leaving a declared-but-undefined
    // function in the module.
    ctx.func.name = UserFuncName::user(1, 0);

    let mut fbc = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
        match build_entry(&mut b, module, interp, params, upval_names, body) {
            // `finalize` consumes the builder, so it must run on the owned `b`
            // here (not via the `&mut` inside `build_entry`).
            Ok(()) => b.finalize(),
            // Bail: drop the half-built function and `ctx` without declaring it.
            Err(e) => return Err(e),
        }
    }

    let id = module
        .declare_function(name, Linkage::Local, &sig)
        .map_err(|e| e.to_string())?;
    ctx.func.name = UserFuncName::user(0, id.as_u32());
    module
        .define_function(id, &mut ctx)
        .map_err(|e| e.to_string())?;
    module.clear_context(&mut ctx);
    Ok(id)
}

/// (W1) Compile a NAMED top-level defun that may self-call in tail position into
/// a native `return_call` loop, returning the DEFAULT-callconv ENTRY `FuncId`.
///
/// Mirrors `mod.rs`'s `define_length_h_tail` (a `CallConv::Tail` self-recursive
/// loop whose self-call is a `return_call` to its own `FuncId`) + the
/// `define_length_h_entry` default-callconv host trampoline, but lowers an
/// arbitrary Slice-A body via [`Trans`] + [`Trans::lower_tail`]. Returns `Err`
/// (caller does NOT register the shim, the tree-walked defun serves it) for any
/// form outside the Slice-A subset.
///
/// FuncId/finalize discipline (caller finalizes ONCE then `get_finalized_function`
/// on the returned entry): declare `tail_id` → build+define tail (referencing
/// `tail_id` via `declare_func_in_func`) → declare `entry_id` → build+define
/// entry → caller `finalize_definitions()`. Never finalize between the two
/// defines. On a lowering bail this returns `Err` BEFORE declaring/defining the
/// entry trampoline.
pub(crate) fn compile_named_self_tail(
    module: &mut JITModule,
    interp: &Interp,
    name: &str,
    self_sym: SymId,
    params: &[SymId],
    body: &KlExpr,
) -> Result<FuncId, String> {
    if params.len() > u8::MAX as usize {
        return Err("jit: too many params".into());
    }

    // ---- (a) Tail body: CallConv::Tail, SCALAR-param ABI ----
    // `(interp, errflag, captures, p0..p_{N-1})` — recursion state rides in
    // registers (like `mod.rs` `define_length_h_tail` + the spike), NOT a
    // pointer into the frame, so the self `return_call` is sound under the Tail
    // ABI. The default-callconv entry trampoline below unpacks `args_ptr` once.
    let mut tsig = module.make_signature();
    tsig.call_conv = CallConv::Tail;
    for _ in 0..3 + params.len() {
        tsig.params.push(AbiParam::new(types::I64)); // interp, errflag, captures, p0..
    }
    tsig.returns.push(AbiParam::new(types::I64));
    // DECLARE FIRST so `self_id` exists before lowering (inverse of
    // `compile_closure_body`'s declare-after-build; required for
    // `declare_func_in_func(self)` in `lower_tail`).
    let tail_id = module
        .declare_function(&format!("{name}_tail"), Linkage::Local, &tsig)
        .map_err(|e| e.to_string())?;
    let mut ctx = module.make_context();
    ctx.func.signature = tsig.clone();
    ctx.func.name = UserFuncName::user(0, tail_id.as_u32());
    let mut fbc = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fbc);
        // Params arrive as SCALAR register args (interp/errflag/captures =
        // params 0/1/2, then p0..p_{N-1}); no load from a frame slot.
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let interp_v = b.block_params(entry)[0];
        let errflag_v = b.block_params(entry)[1];
        let captures_v = b.block_params(entry)[2];

        let mut locals: Vec<(SymId, IrValue)> = Vec::with_capacity(params.len());
        for (i, &p) in params.iter().enumerate() {
            locals.push((p, b.block_params(entry)[3 + i]));
        }

        let err_block = b.create_block();

        let mut t = Trans {
            b: &mut b,
            module,
            interp,
            imports: HashMap::new(),
            locals,
            upvals: &[], // top-level defun has no upvals
            interp_v,
            errflag_v,
            captures_v,
            err_block,
            self_sym: Some(self_sym),
            self_id: Some(tail_id),
            self_arity: params.len(),
        };
        // `lower_tail` emits the terminators (incl. the self `return_call`).
        let r = t.lower_tail(body);

        // Fill the error block exactly as `build_entry` (nil return).
        t.b.switch_to_block(err_block);
        let nil = t.b.ins().iconst(types::I64, Value::nil().to_word() as i64);
        t.b.ins().return_(&[nil]);

        t.b.seal_all_blocks();
        // On Err propagate (bail before declaring/defining the entry
        // trampoline). `finalize` must run on the owned `b`.
        match r {
            Ok(()) => b.finalize(),
            Err(e) => return Err(e),
        }
    }
    module
        .define_function(tail_id, &mut ctx)
        .map_err(|e| e.to_string())?;
    module.clear_context(&mut ctx);

    // ---- (b) default-callconv entry trampoline (plain call into tail body) ----
    let mut esig = module.make_signature();
    for _ in 0..5 {
        esig.params.push(AbiParam::new(types::I64));
    }
    esig.returns.push(AbiParam::new(types::I64));
    let entry_id = module
        .declare_function(&format!("{name}_entry"), Linkage::Local, &esig)
        .map_err(|e| e.to_string())?;
    let mut ectx = module.make_context();
    ectx.func.signature = esig;
    ectx.func.name = UserFuncName::user(0, entry_id.as_u32());
    let mut efbc = FunctionBuilderContext::new();
    {
        let mut b = FunctionBuilder::new(&mut ectx.func, &mut efbc);
        let blk = b.create_block();
        b.append_block_params_for_function_params(blk);
        b.switch_to_block(blk);
        // Unpack `args_ptr` (param 3) ONCE into scalar params, then PLAIN-call
        // the scalar Tail body. Thereafter recursion passes scalars, so this
        // pointer never crosses a `return_call` into a torn-down frame.
        let interp_v = b.block_params(blk)[0];
        let errflag_v = b.block_params(blk)[1];
        let captures_v = b.block_params(blk)[2];
        let args_v = b.block_params(blk)[3];
        let mut call_args = vec![interp_v, errflag_v, captures_v];
        for i in 0..params.len() {
            let v = b
                .ins()
                .load(types::I64, MemFlags::trusted(), args_v, (i * 8) as i32);
            call_args.push(v);
        }
        // PLAIN call (NOT return_call) into the scalar Tail body.
        let callee = module.declare_func_in_func(tail_id, b.func);
        let call = b.ins().call(callee, &call_args);
        let r = b.inst_results(call)[0];
        b.ins().return_(&[r]);
        b.seal_all_blocks();
        b.finalize();
    }
    module
        .define_function(entry_id, &mut ectx)
        .map_err(|e| e.to_string())?;
    module.clear_context(&mut ectx);
    Ok(entry_id) // caller finalizes ONCE then `get_finalized_function(entry_id)`
}

/// Set up the entry block (load params from `args_ptr`, stash the ABI words),
/// lower the body, and fill the shared error-return block. On a lowering bail the
/// half-built function is discarded by the caller (never defined).
fn build_entry(
    b: &mut FunctionBuilder,
    module: &mut JITModule,
    interp: &Interp,
    params: &[SymId],
    upval_names: &[SymId],
    body: &KlExpr,
) -> Result<(), String> {
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let interp_v = b.block_params(entry)[0];
    let errflag_v = b.block_params(entry)[1];
    let captures_v = b.block_params(entry)[2];
    let args_v = b.block_params(entry)[3];
    // nargs (param 4) is unused — arity is checked in `call_jit`.

    // Params become the initial locals, loaded once from the arg array.
    let mut locals: Vec<(SymId, IrValue)> = Vec::with_capacity(params.len());
    for (i, &p) in params.iter().enumerate() {
        let v = b
            .ins()
            .load(types::I64, MemFlags::trusted(), args_v, (i * 8) as i32);
        locals.push((p, v));
    }

    // Shared error-return block: yields the nil sentinel; `call_jit` reads the
    // flag and surfaces the real pending error.
    let err_block = b.create_block();

    let mut t = Trans {
        b,
        module,
        interp,
        imports: HashMap::new(),
        locals,
        upvals: upval_names,
        interp_v,
        errflag_v,
        captures_v,
        err_block,
        // Anonymous closure path: no self-name → the self-tail guard never fires.
        self_sym: None,
        self_id: None,
        self_arity: 0,
    };
    let result = t.lower(body)?;
    t.b.ins().return_(&[result]);

    // Fill the error block.
    t.b.switch_to_block(err_block);
    let nil = t.b.ins().iconst(types::I64, Value::nil().to_word() as i64);
    t.b.ins().return_(&[nil]);

    // Seal here (all predecessors known); the caller owns `finalize`.
    t.b.seal_all_blocks();
    Ok(())
}

/// Where a bare symbol resolves in the single closure frame.
enum Var {
    /// A param or `let`-bound local — already an SSA word.
    Local(IrValue),
    /// A captured upval — index into `captures_ptr`.
    Upval(usize),
    /// Neither — a free symbol (self-evaluating in value position, a global
    /// function in head position).
    Free,
}

struct Trans<'a, 'b> {
    b: &'b mut FunctionBuilder<'a>,
    module: &'b mut JITModule,
    interp: &'b Interp,
    /// `rtj_*` imports declared into the module for this compile (name → id).
    imports: HashMap<&'static str, FuncId>,
    /// Params + live `let` bindings, innermost last (reverse-scanned).
    locals: Vec<(SymId, IrValue)>,
    upvals: &'b [SymId],
    interp_v: IrValue,
    errflag_v: IrValue,
    captures_v: IrValue,
    err_block: Block,
    /// (W1) Defun name to recognise a self-call head; `None` on the anonymous
    /// closure path (so the self-tail guard can never fire there).
    self_sym: Option<SymId>,
    /// (W1) This body's OWN (`CallConv::Tail`) `FuncId`, declared BEFORE
    /// lowering so `lower_tail` can `declare_func_in_func(self)`.
    self_id: Option<FuncId>,
    /// (W1) `== params.len()`; `0` on the anonymous path.
    self_arity: usize,
}

impl Trans<'_, '_> {
    fn resolve(&self, s: SymId) -> Var {
        if let Some((_, v)) = self.locals.iter().rev().find(|(n, _)| *n == s) {
            return Var::Local(*v);
        }
        if let Some(i) = self.upvals.iter().position(|n| *n == s) {
            return Var::Upval(i);
        }
        Var::Free
    }

    /// Declare (once per compile) and reference a runtime helper of `nparams`
    /// `i64` args returning one `i64`.
    fn helper(&mut self, name: &'static str, nparams: usize) -> cranelift_codegen::ir::FuncRef {
        let id = if let Some(id) = self.imports.get(name) {
            *id
        } else {
            let mut sig = self.module.make_signature();
            for _ in 0..nparams {
                sig.params.push(AbiParam::new(types::I64));
            }
            sig.returns.push(AbiParam::new(types::I64));
            let id = self
                .module
                .declare_function(name, Linkage::Import, &sig)
                .expect("declare rtj_ import");
            self.imports.insert(name, id);
            id
        };
        self.module.declare_func_in_func(id, self.b.func)
    }

    /// Emit a call and, for fallible helpers, the `errflag` load + branch to the
    /// shared error block. Returns the call's result word (valid in the
    /// continuation block).
    fn call(
        &mut self,
        name: &'static str,
        nparams: usize,
        args: &[IrValue],
        fallible: bool,
    ) -> IrValue {
        let fref = self.helper(name, nparams);
        let call = self.b.ins().call(fref, args);
        let res = self.b.inst_results(call)[0];
        if fallible {
            let flag = self
                .b
                .ins()
                .load(types::I64, MemFlags::trusted(), self.errflag_v, 0);
            let cont = self.b.create_block();
            self.b.ins().brif(flag, self.err_block, &[], cont, &[]);
            self.b.switch_to_block(cont);
        }
        res
    }

    /// Marshal lowered arg words into a fresh stack-slot array; returns
    /// `(base_ptr, n)`. `n == 0` passes a null base (the helper never reads it).
    fn marshal(&mut self, argv: &[IrValue]) -> (IrValue, IrValue) {
        let n = argv.len();
        let n_v = self.b.ins().iconst(types::I64, n as i64);
        if n == 0 {
            let z = self.b.ins().iconst(types::I64, 0);
            return (z, n_v);
        }
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            (n * 8) as u32,
            3, // 8-byte aligned
        ));
        for (i, v) in argv.iter().enumerate() {
            self.b.ins().stack_store(*v, slot, (i * 8) as i32);
        }
        let addr = self.b.ins().stack_addr(types::I64, slot, 0);
        (addr, n_v)
    }

    fn iconst_word(&mut self, w: u64) -> IrValue {
        self.b.ins().iconst(types::I64, w as i64)
    }

    fn lower(&mut self, expr: &KlExpr) -> Result<IrValue, String> {
        match expr {
            KlExpr::Nil => Ok(self.iconst_word(Value::nil().to_word())),
            KlExpr::Bool(b) => Ok(self.iconst_word(Value::bool(*b).to_word())),
            KlExpr::Int(n) => {
                // Only inline fixnums (tag 000). Out-of-range ints box a heap
                // float — bail (heap literal; see Slice B's constant table).
                let w = Value::int(*n).to_word();
                if w & 0b111 == 0 {
                    Ok(self.iconst_word(w))
                } else {
                    Err("jit: non-fixnum integer literal".into())
                }
            }
            KlExpr::Float(_) => Err("jit: float literal (heap)".into()),
            KlExpr::Str(_) => Err("jit: string literal (heap)".into()),
            KlExpr::Sym(s) => match self.resolve(*s) {
                Var::Local(v) => Ok(v),
                Var::Upval(i) => Ok(self.b.ins().load(
                    types::I64,
                    MemFlags::trusted(),
                    self.captures_v,
                    (i * 8) as i32,
                )),
                Var::Free => Ok(self.iconst_word(Value::sym(*s).to_word())),
            },
            KlExpr::App(items) => self.lower_app(items),
        }
    }

    fn lower_app(&mut self, items: &[KlExpr]) -> Result<IrValue, String> {
        if items.is_empty() {
            return Ok(self.iconst_word(Value::nil().to_word()));
        }
        if let KlExpr::Sym(s) = &items[0] {
            let s = *s;
            let args = &items[1..];
            let wk = &self.interp.well_known;
            // Special forms first, by well-known sym id (unconditional, as in the
            // tree-walker / VM — a special form is never shadowed in kernel code).
            if s == wk.k_if {
                return self.lower_if(args);
            }
            if s == wk.k_cond {
                return self.lower_cond(args);
            }
            if s == wk.k_and {
                return self.lower_and_or(args, true);
            }
            if s == wk.k_or {
                return self.lower_and_or(args, false);
            }
            if s == wk.k_let {
                return self.lower_let(args);
            }
            if s == wk.k_do {
                return self.lower_do(args);
            }
            if s == wk.k_type {
                if args.is_empty() {
                    return Err("type: expected at least 1 arg".into());
                }
                return self.lower(&args[0]);
            }
            // Unsupported special forms / heap-producing forms → bail.
            if s == wk.k_lambda || s == wk.k_freeze {
                return Err("jit: nested closure (Slice 3b)".into());
            }
            if s == wk.k_defun || s == wk.k_trap_error || s == wk.k_thaw || s == wk.k_quote {
                return Err("jit: unsupported special form".into());
            }

            // Inlinable primitive — only when the name isn't lexically shadowed.
            if matches!(self.resolve(s), Var::Free) {
                let name = self.interp.resolve(s);
                if let Some((sym, nparams, fallible)) = inlinable_jit(name, args.len()) {
                    let mut call_args: Vec<IrValue> = Vec::with_capacity(nparams);
                    if fallible {
                        call_args.push(self.interp_v);
                        call_args.push(self.errflag_v);
                    }
                    for a in args {
                        call_args.push(self.lower(a)?);
                    }
                    return Ok(self.call(sym, nparams, &call_args, fallible));
                }
            }

            // General call. A shadowing local/upval head is a value to apply;
            // a free head names a global function.
            match self.resolve(s) {
                Var::Local(v) => self.apply_value(v, args),
                Var::Upval(i) => {
                    let v = self.b.ins().load(
                        types::I64,
                        MemFlags::trusted(),
                        self.captures_v,
                        (i * 8) as i32,
                    );
                    self.apply_value(v, args)
                }
                Var::Free => {
                    let sym_word = self.iconst_word(Value::sym(s).to_word());
                    let mut argv = Vec::with_capacity(args.len());
                    for a in args {
                        argv.push(self.lower(a)?);
                    }
                    let (ptr, n) = self.marshal(&argv);
                    Ok(self.call(
                        "rtj_apply_named",
                        5,
                        &[self.interp_v, self.errflag_v, sym_word, ptr, n],
                        true,
                    ))
                }
            }
        } else {
            // Head is an expression evaluating to a closure value.
            let f = self.lower(&items[0])?;
            self.apply_value(f, &items[1..])
        }
    }

    fn apply_value(&mut self, fval: IrValue, args: &[KlExpr]) -> Result<IrValue, String> {
        let mut argv = Vec::with_capacity(args.len());
        for a in args {
            argv.push(self.lower(a)?);
        }
        let (ptr, n) = self.marshal(&argv);
        Ok(self.call(
            "rtj_apply_value",
            5,
            &[self.interp_v, self.errflag_v, fval, ptr, n],
            true,
        ))
    }

    /// `is_truthy` of a value word: fallible FFI → `i64` 0/1, with the err check.
    fn truthy(&mut self, v: IrValue) -> IrValue {
        let iv = self.interp_v;
        let ev = self.errflag_v;
        self.call("rtj_is_truthy", 3, &[iv, ev, v], true)
    }

    fn lower_if(&mut self, args: &[KlExpr]) -> Result<IrValue, String> {
        if args.len() != 3 {
            return Err("if: expected 3 args".into());
        }
        let c = self.lower(&args[0])?;
        let t = self.truthy(c);
        let then_b = self.b.create_block();
        let else_b = self.b.create_block();
        let join = self.b.create_block();
        self.b.append_block_param(join, types::I64);
        self.b.ins().brif(t, then_b, &[], else_b, &[]);

        self.b.switch_to_block(then_b);
        let v1 = self.lower(&args[1])?;
        self.b.ins().jump(join, &[BlockArg::Value(v1)]);

        self.b.switch_to_block(else_b);
        let v2 = self.lower(&args[2])?;
        self.b.ins().jump(join, &[BlockArg::Value(v2)]);

        self.b.switch_to_block(join);
        Ok(self.b.block_params(join)[0])
    }

    fn lower_and_or(&mut self, args: &[KlExpr], is_and: bool) -> Result<IrValue, String> {
        if args.len() != 2 {
            return Err("and/or: expected 2 args".into());
        }
        // Match the tree-walker (`step_and`/`step_or`) exactly: evaluate `a`,
        // require it boolean (`truthy` errors otherwise), then on the
        // non-short-circuit path return `b`'s **raw** value — NOT normalised to a
        // boolean. `and` short-circuits to `false`, `or` to `true`.
        let a = self.lower(&args[0])?;
        let ta = self.truthy(a);
        let eval_b = self.b.create_block();
        let short_b = self.b.create_block();
        let join = self.b.create_block();
        self.b.append_block_param(join, types::I64);
        let (then_b, else_b) = if is_and {
            (eval_b, short_b)
        } else {
            (short_b, eval_b)
        };
        self.b.ins().brif(ta, then_b, &[], else_b, &[]);

        self.b.switch_to_block(eval_b);
        let bw = self.lower(&args[1])?;
        self.b.ins().jump(join, &[BlockArg::Value(bw)]);

        self.b.switch_to_block(short_b);
        let short_w = self.iconst_word(Value::bool(!is_and).to_word());
        self.b.ins().jump(join, &[BlockArg::Value(short_w)]);

        self.b.switch_to_block(join);
        Ok(self.b.block_params(join)[0])
    }

    fn lower_let(&mut self, args: &[KlExpr]) -> Result<IrValue, String> {
        if args.len() != 3 {
            return Err("let: expected 3 args".into());
        }
        let var = match &args[0] {
            KlExpr::Sym(s) => *s,
            _ => return Err("let: var must be a symbol".into()),
        };
        let val = self.lower(&args[1])?;
        self.locals.push((var, val));
        let res = self.lower(&args[2]);
        self.locals.pop();
        res
    }

    fn lower_do(&mut self, args: &[KlExpr]) -> Result<IrValue, String> {
        if args.is_empty() {
            return Ok(self.iconst_word(Value::nil().to_word()));
        }
        let last = args.len() - 1;
        let mut result = self.iconst_word(Value::nil().to_word());
        for (i, e) in args.iter().enumerate() {
            let v = self.lower(e)?;
            if i == last {
                result = v;
            }
        }
        Ok(result)
    }

    fn lower_cond(&mut self, args: &[KlExpr]) -> Result<IrValue, String> {
        // Lower as a chain of ifs. Each clause: (test body). Fall-through (no
        // clause matched) yields nil, matching the VM's defensive tail.
        let join = self.b.create_block();
        self.b.append_block_param(join, types::I64);
        for clause in args {
            let pair = match clause {
                KlExpr::App(items) if items.len() == 2 => items,
                _ => return Err("cond: clauses must be 2-element lists".into()),
            };
            let c = self.lower(&pair[0])?;
            let t = self.truthy(c);
            let body_b = self.b.create_block();
            let next_b = self.b.create_block();
            self.b.ins().brif(t, body_b, &[], next_b, &[]);
            self.b.switch_to_block(body_b);
            let v = self.lower(&pair[1])?;
            self.b.ins().jump(join, &[BlockArg::Value(v)]);
            self.b.switch_to_block(next_b);
        }
        // Fall-through: nil.
        let nil = self.iconst_word(Value::nil().to_word());
        self.b.ins().jump(join, &[BlockArg::Value(nil)]);
        self.b.switch_to_block(join);
        Ok(self.b.block_params(join)[0])
    }

    // =======================================================================
    // (W1) Tail-position lowering — the ONLY new emission path. Each method
    // EMITS a terminator (`return_` or `return_call`) and returns `()`; it is
    // called only where the body result is consumed. Tail-transparent special
    // forms recurse via the `*_tail` helpers; everything else lowers as a VALUE
    // via the EXISTING `self.lower()` and then `return_`s it. Arg/test/value
    // positions stay NON-TAIL via the existing `lower()`.
    // =======================================================================

    fn lower_tail(&mut self, expr: &KlExpr) -> Result<(), String> {
        if let KlExpr::App(items) = expr {
            if !items.is_empty() {
                if let KlExpr::Sym(s) = &items[0] {
                    let s = *s;
                    let args = &items[1..];
                    let wk = &self.interp.well_known;
                    if s == wk.k_if {
                        return self.lower_if_tail(args);
                    }
                    if s == wk.k_cond {
                        return self.lower_cond_tail(args);
                    }
                    if s == wk.k_let {
                        return self.lower_let_tail(args);
                    }
                    if s == wk.k_do {
                        return self.lower_do_tail(args);
                    }
                    // `and`/`or`: the eval-arm is tail but the short arm is a
                    // const; lowering as a value then `return_` (fall through
                    // below) is the simplest correct treatment.
                    //
                    // SELF-TAIL CALL: free head == self_sym, exact arity.
                    if Some(s) == self.self_sym
                        && args.len() == self.self_arity
                        && matches!(self.resolve(s), Var::Free)
                    {
                        // Args are NON-TAIL → existing value `lower()`.
                        let mut argv = Vec::with_capacity(args.len());
                        for a in args {
                            argv.push(self.lower(a)?);
                        }
                        let selfref = self
                            .module
                            .declare_func_in_func(self.self_id.unwrap(), self.b.func);
                        // SCALAR self `return_call` (matches `mod.rs`
                        // `define_length_h_tail`): recursion args ride in
                        // registers, never a pointer into the dying frame.
                        let mut call_args = vec![self.interp_v, self.errflag_v, self.captures_v];
                        call_args.extend_from_slice(&argv);
                        self.b.ins().return_call(selfref, &call_args);
                        // `return_call` terminates the block: NO result, NO
                        // errflag brif.
                        return Ok(());
                    }
                }
            }
        }
        // Non-self / non-tail-form tail position: lower as a value, then
        // `return_` it.
        let v = self.lower(expr)?;
        self.b.ins().return_(&[v]);
        Ok(())
    }

    fn lower_if_tail(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.len() != 3 {
            return Err("if: expected 3 args".into());
        }
        let c = self.lower(&args[0])?; // condition NON-TAIL
        let t = self.truthy(c);
        let then_b = self.b.create_block();
        let else_b = self.b.create_block();
        self.b.ins().brif(t, then_b, &[], else_b, &[]);

        self.b.switch_to_block(then_b);
        self.lower_tail(&args[1])?; // then arm tail
        self.b.switch_to_block(else_b);
        self.lower_tail(&args[2])?; // else arm tail
        Ok(())
    }

    fn lower_cond_tail(&mut self, args: &[KlExpr]) -> Result<(), String> {
        for clause in args {
            let pair = match clause {
                KlExpr::App(items) if items.len() == 2 => items,
                _ => return Err("cond: clauses must be 2-element lists".into()),
            };
            let c = self.lower(&pair[0])?; // test NON-TAIL
            let t = self.truthy(c);
            let body_b = self.b.create_block();
            let next_b = self.b.create_block();
            self.b.ins().brif(t, body_b, &[], next_b, &[]);
            self.b.switch_to_block(body_b);
            self.lower_tail(&pair[1])?; // body tail
            self.b.switch_to_block(next_b);
        }
        // Fall-through: nil.
        let nil = self.iconst_word(Value::nil().to_word());
        self.b.ins().return_(&[nil]);
        Ok(())
    }

    fn lower_let_tail(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.len() != 3 {
            return Err("let: expected 3 args".into());
        }
        let var = match &args[0] {
            KlExpr::Sym(s) => *s,
            _ => return Err("let: var must be a symbol".into()),
        };
        let val = self.lower(&args[1])?; // value NON-TAIL
        self.locals.push((var, val));
        let r = self.lower_tail(&args[2]); // body tail
        self.locals.pop();
        r
    }

    fn lower_do_tail(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.is_empty() {
            let nil = self.iconst_word(Value::nil().to_word());
            self.b.ins().return_(&[nil]);
            return Ok(());
        }
        let last = args.len() - 1;
        for e in &args[..last] {
            self.lower(e)?; // NON-TAIL (effect)
        }
        self.lower_tail(&args[last]) // last is tail
    }
}

/// JIT analogue of the VM's `inlinable_op` / klcompile's `inlinable`: maps a
/// primitive `(name, arity)` to its `rtj_*` symbol, the helper's `i64` param
/// count, and whether it is fallible (needs the post-call `errflag` check).
fn inlinable_jit(name: &str, arity: usize) -> Option<(&'static str, usize, bool)> {
    Some(match (name, arity) {
        ("+", 2) => ("rtj_add", 4, true),
        ("-", 2) => ("rtj_sub", 4, true),
        ("*", 2) => ("rtj_mul", 4, true),
        ("/", 2) => ("rtj_div", 4, true),
        ("<", 2) => ("rtj_lt", 4, true),
        (">", 2) => ("rtj_gt", 4, true),
        ("<=", 2) => ("rtj_lte", 4, true),
        (">=", 2) => ("rtj_gte", 4, true),
        ("=", 2) => ("rtj_eq", 2, false),
        ("cons", 2) => ("rtj_cons", 2, false),
        ("hd", 1) => ("rtj_hd", 3, true),
        ("tl", 1) => ("rtj_tl", 3, true),
        ("cons?", 1) => ("rtj_is_cons", 1, false),
        ("number?", 1) => ("rtj_is_number", 1, false),
        ("string?", 1) => ("rtj_is_string", 1, false),
        ("symbol?", 1) => ("rtj_is_symbol", 1, false),
        ("absvector?", 1) | ("vector?", 1) => ("rtj_is_absvector", 1, false),
        _ => return None,
    })
}

#[cfg(test)]
mod abi_spike {
    //! De-risks the two Cranelift patterns the J1 engine never exercised and
    //! the J2 `BodyTranslator` depends on, *before* the generator is built.
    //!
    //! Pattern 1 — the **flag-pointer error ABI**: `load` an `errflag` after a
    //! fallible `rtj_*` FFI call and `brif` to a shared error-return block.
    //!
    //! Pattern 2 — **stack-slot argument marshaling**: pack words into a
    //! `create_sized_stack_slot` and pass `stack_addr` + length to a helper
    //! taking `*const u64`.
    //!
    //! (4-arg `return_call` is mechanically identical to J1's proven 3-arg loop,
    //! so it is not re-spiked here.)

    use std::mem::transmute;

    use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MemFlags, UserFuncName};
    use cranelift_codegen::settings::{self, Configurable};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    use cranelift_jit::{JITBuilder, JITModule};
    use cranelift_module::{default_libcall_names, FuncId, Linkage, Module};

    use crate::interp::eval::Interp;
    use crate::jit::ffi;
    use crate::value::Value;

    /// Test-only helper for pattern (2): `cons(arr[0], arr[1])` read through a
    /// raw word pointer — the exact shape the apply path will use.
    extern "C" fn rtj_test_cons2(ptr: *const u64) -> u64 {
        // SAFETY: the JIT passes a stack-slot base holding ≥2 words.
        let (a, b) = unsafe { (*ptr, *ptr.add(1)) };
        Value::cons(Value::from_word(a), Value::from_word(b)).to_word()
    }

    fn new_module() -> JITModule {
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, default_libcall_names());
        jb.symbol("rtj_hd", ffi::rtj_hd as *const u8);
        jb.symbol("rtj_test_cons2", rtj_test_cons2 as *const u8);
        JITModule::new(jb)
    }

    /// `hd_checked(interp, errflag, v)`: `x = rtj_hd(interp, errflag, v); if
    /// *errflag != 0 { return nil } else { return x }`. Proves load-after-call +
    /// brif to a shared error block.
    fn build_hd_checked(module: &mut JITModule) -> FuncId {
        let mut hd_sig = module.make_signature();
        for _ in 0..3 {
            hd_sig.params.push(AbiParam::new(types::I64));
        }
        hd_sig.returns.push(AbiParam::new(types::I64));
        let hd_id = module
            .declare_function("rtj_hd", Linkage::Import, &hd_sig)
            .unwrap();

        let mut sig = module.make_signature();
        for _ in 0..3 {
            sig.params.push(AbiParam::new(types::I64)); // interp, errflag, v
        }
        sig.returns.push(AbiParam::new(types::I64));
        let id = module
            .declare_function("hd_checked", Linkage::Local, &sig)
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
            let errflag = b.block_params(entry)[1];
            let v = b.block_params(entry)[2];

            let hd_ref = module.declare_func_in_func(hd_id, b.func);
            let call = b.ins().call(hd_ref, &[interp, errflag, v]);
            let x = b.inst_results(call)[0];

            // load.i64 errflag; brif → err / ok
            let flag = b.ins().load(types::I64, MemFlags::trusted(), errflag, 0);
            let okb = b.create_block();
            let errb = b.create_block();
            b.ins().brif(flag, errb, &[], okb, &[]);

            b.switch_to_block(okb);
            b.ins().return_(&[x]);

            b.switch_to_block(errb);
            let nil = b.ins().iconst(types::I64, Value::nil().to_word() as i64);
            b.ins().return_(&[nil]);

            b.seal_all_blocks();
            b.finalize();
        }
        module.define_function(id, &mut ctx).unwrap();
        module.clear_context(&mut ctx);
        id
    }

    /// `cons_args(interp, errflag, args_ptr, n)`: pack `args[0]`,`args[1]` into a
    /// fresh stack slot and call `rtj_test_cons2(slot)`. Proves
    /// `create_sized_stack_slot` + `stack_store` + `stack_addr`.
    fn build_cons_args(module: &mut JITModule) -> FuncId {
        let mut c2_sig = module.make_signature();
        c2_sig.params.push(AbiParam::new(types::I64)); // ptr
        c2_sig.returns.push(AbiParam::new(types::I64));
        let c2_id = module
            .declare_function("rtj_test_cons2", Linkage::Import, &c2_sig)
            .unwrap();

        let mut sig = module.make_signature();
        for _ in 0..4 {
            sig.params.push(AbiParam::new(types::I64)); // interp, errflag, args_ptr, n
        }
        sig.returns.push(AbiParam::new(types::I64));
        let id = module
            .declare_function("cons_args", Linkage::Local, &sig)
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
            let args_ptr = b.block_params(entry)[2];

            let a0 = b.ins().load(types::I64, MemFlags::trusted(), args_ptr, 0);
            let a1 = b.ins().load(types::I64, MemFlags::trusted(), args_ptr, 8);

            let slot = b.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
                cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                16,
                3, // log2(8) alignment
            ));
            b.ins().stack_store(a0, slot, 0);
            b.ins().stack_store(a1, slot, 8);
            let addr = b.ins().stack_addr(types::I64, slot, 0);

            let c2_ref = module.declare_func_in_func(c2_id, b.func);
            let call = b.ins().call(c2_ref, &[addr]);
            let r = b.inst_results(call)[0];
            b.ins().return_(&[r]);

            b.seal_all_blocks();
            b.finalize();
        }
        module.define_function(id, &mut ctx).unwrap();
        module.clear_context(&mut ctx);
        id
    }

    #[test]
    fn flag_pointer_error_abi() {
        let mut module = new_module();
        let id = build_hd_checked(&mut module);
        module.finalize_definitions().unwrap();
        let f: extern "C" fn(*mut Interp, *mut u64, u64) -> u64 =
            unsafe { transmute(module.get_finalized_function(id)) };

        let mut interp = Interp::new();
        // ok: hd of a proper cons → head, flag stays 0.
        let list = Value::cons(Value::int(7), Value::nil());
        let mut flag: u64 = 0;
        let r = f(&mut interp, &mut flag, list.to_word());
        assert_eq!(flag, 0, "no error expected on a cons");
        assert_eq!(Value::from_word(r).as_int(), Some(7));

        // err: hd of a non-cons → flag raised, pending error recorded.
        let mut flag2: u64 = 0;
        let _ = f(&mut interp, &mut flag2, Value::int(5).to_word());
        assert_eq!(flag2, 1, "error flag must be raised on hd of a non-cons");
        assert!(
            interp.take_pending_error().is_some(),
            "rich error must ride pending_error"
        );
        // keep module alive until after the calls
        drop(module);
    }

    #[test]
    fn stack_slot_arg_marshaling() {
        let mut module = new_module();
        let id = build_cons_args(&mut module);
        module.finalize_definitions().unwrap();
        let f: extern "C" fn(*mut Interp, *mut u64, *const u64, usize) -> u64 =
            unsafe { transmute(module.get_finalized_function(id)) };

        let mut interp = Interp::new();
        let mut flag: u64 = 0;
        let args = [Value::int(3).to_word(), Value::int(4).to_word()];
        let r = f(&mut interp, &mut flag, args.as_ptr(), args.len());
        let v = Value::from_word(r);
        // expect cons(3, 4)
        assert_eq!(v.head().and_then(Value::as_int), Some(3));
        assert_eq!(v.tail().and_then(Value::as_int), Some(4));
        drop(module);
    }
}
