//! Compile a `KlExpr` body into a `BytecodeFn`.
//!
//! Roughly mirrors shen-go's `klCompiler` (`../shen-go/kl/compiler.go`):
//! per-function state — local-slot allocation by `SymId`, constants
//! pool, nested-fn pool, code stream, and an upval list for variables
//! captured from enclosing scopes — lives in a `CompilerFrame`, and a
//! stack of frames handles nested lambdas. `resolve_var` walks outward
//! through the stack, registering an `UpvalInfo` at each level that
//! needs to forward the value inward.
//!
//! Special forms (`if` / `let` / `cond` / `do` / `and` / `or` /
//! `lambda` / `freeze`) are lowered statically into opcodes; the VM
//! loop is a flat dispatch with no special-form recognition at run
//! time. `type` is a no-op annotation that just compiles its body.
//! `defun` / `quote` / `trap-error` / `thaw` are not yet supported by
//! the VM compiler (they remain tree-walked / AOT-handled).

use std::collections::HashMap;
use std::rc::Rc;

use crate::interp::eval::Interp;
use crate::kl::ast::KlExpr;
use crate::symbol::SymId;
use crate::value::Value;
use crate::vm::bytecode::BytecodeFn;
use crate::vm::opcode::Op;

/// Compile a top-level Shen function body into a `BytecodeFn`.
///
/// `params` are the formal parameters in declaration order; they
/// occupy `locals[0..params.len())` at execution time. `body` is the
/// expression form to evaluate when the function is called.
pub fn compile_fn(
    interp: &Interp,
    name: Option<SymId>,
    params: &[SymId],
    body: &KlExpr,
) -> Result<BytecodeFn, String> {
    let mut c = Compiler::new(interp);
    for &p in params {
        c.add_local(p);
    }
    c.compile_expr(body)?;
    c.emit(Op::Return);
    assert_eq!(
        c.frames.len(),
        1,
        "vm: compile_fn left {} dangling frames",
        c.frames.len()
    );
    let frame = c.frames.pop().expect("vm: missing top frame");
    Ok(BytecodeFn {
        name,
        arity: params.len(),
        n_locals: frame.n_locals as usize,
        code: frame.code,
        consts: frame.consts,
        fn_consts: frame.fn_consts,
    })
}

struct Compiler<'a> {
    interp: &'a Interp,
    /// Stack of per-function frames. The innermost (nested-most)
    /// compilation is at the end (`top`). The outermost frame is the
    /// top-level function being compiled.
    frames: Vec<CompilerFrame>,
}

#[derive(Default)]
struct CompilerFrame {
    /// Sym → slot index for in-scope locals (params + `let`-bound).
    locals: HashMap<SymId, u16>,
    /// Next free slot. Grows monotonically as `let` introduces new
    /// bindings; we don't reclaim slots after a let body ends because
    /// the size is bounded by the source structure.
    n_locals: u16,
    code: Vec<Op>,
    consts: Vec<Value>,
    fn_consts: Vec<Rc<BytecodeFn>>,
    /// Variables this frame closes over from enclosing scopes. Each
    /// entry records where in the immediate outer frame the value
    /// lives, so the outer frame can emit the right `LoadLocal` /
    /// `LoadUpval` at `MakeClosure` time.
    upvals: Vec<UpvalInfo>,
}

#[derive(Debug, Clone, Copy)]
enum VarKind {
    Local(u16),
    Upval(u16),
}

#[derive(Debug)]
struct UpvalInfo {
    name: SymId,
    source: VarKind,
}

impl<'a> Compiler<'a> {
    fn new(interp: &'a Interp) -> Self {
        Self {
            interp,
            frames: vec![CompilerFrame::default()],
        }
    }

    fn top_mut(&mut self) -> &mut CompilerFrame {
        self.frames.last_mut().expect("vm: no current frame")
    }

    fn current_frame_idx(&self) -> usize {
        self.frames.len() - 1
    }

    fn add_local(&mut self, sym: SymId) -> u16 {
        let f = self.top_mut();
        let slot = f.n_locals;
        f.n_locals = f
            .n_locals
            .checked_add(1)
            .expect("vm: more than u16::MAX locals in one function");
        f.locals.insert(sym, slot);
        slot
    }

    fn add_const(&mut self, v: Value) -> Result<u16, String> {
        let f = self.top_mut();
        let idx = f.consts.len();
        if idx > u16::MAX as usize {
            return Err("vm: more than u16::MAX constants in one function".into());
        }
        f.consts.push(v);
        Ok(idx as u16)
    }

    fn add_fn_const(&mut self, bf: Rc<BytecodeFn>) -> Result<u16, String> {
        let f = self.top_mut();
        let idx = f.fn_consts.len();
        if idx > u16::MAX as usize {
            return Err("vm: more than u16::MAX nested fns in one function".into());
        }
        f.fn_consts.push(bf);
        Ok(idx as u16)
    }

    fn emit(&mut self, op: Op) {
        self.top_mut().code.push(op);
    }

    fn emit_const(&mut self, v: Value) -> Result<(), String> {
        let idx = self.add_const(v)?;
        self.emit(Op::LoadConst(idx));
        Ok(())
    }

    /// Emit a jump with a placeholder offset; returns the instruction
    /// index so the caller can patch it once the target is known.
    fn emit_jump(&mut self, mk: fn(i16) -> Op) -> usize {
        let f = self.top_mut();
        let idx = f.code.len();
        f.code.push(mk(0));
        idx
    }

    /// Patch a previously-emitted jump to land at the current `pc`.
    fn patch_jump(&mut self, idx: usize) -> Result<(), String> {
        let f = self.top_mut();
        let target = f.code.len();
        // The VM reads `bf.code[pc]` then advances `pc += 1` before
        // dispatching, so `Jump(d)` lands at `pc_after + d`, where
        // `pc_after = idx + 1`. Solve for `d = target - (idx + 1)`.
        let delta = (target as i32) - (idx as i32 + 1);
        if !(i16::MIN as i32..=i16::MAX as i32).contains(&delta) {
            return Err(format!("vm: jump delta out of i16 range: {delta}"));
        }
        let delta = delta as i16;
        let patched = match f.code[idx] {
            Op::Jump(_) => Op::Jump(delta),
            Op::JumpFalse(_) => Op::JumpFalse(delta),
            other => return Err(format!("vm: patch_jump on non-jump opcode {other:?}")),
        };
        f.code[idx] = patched;
        Ok(())
    }

    /// Resolve `sym` for the frame at `frame_idx`. Returns where it
    /// came from in *that* frame's terms (`Local(slot)` or
    /// `Upval(idx)`), or `None` if the variable isn't bound in any
    /// enclosing scope (the symbol is then self-evaluating or, in
    /// head position, looked up via `LoadGlobal`).
    ///
    /// On the way out, every intermediate frame that didn't already
    /// have `sym` as an upval gets a new `UpvalInfo` pointing into
    /// the next-outer frame. That chain is what `MakeClosure`'s upval
    /// loads will follow at the outer compile site.
    fn resolve_var(&mut self, frame_idx: usize, sym: SymId) -> Option<VarKind> {
        if let Some(&slot) = self.frames[frame_idx].locals.get(&sym) {
            return Some(VarKind::Local(slot));
        }
        if let Some(i) = self.frames[frame_idx]
            .upvals
            .iter()
            .position(|uv| uv.name == sym)
        {
            return Some(VarKind::Upval(i as u16));
        }
        if frame_idx == 0 {
            return None;
        }
        let outer = self.resolve_var(frame_idx - 1, sym)?;
        let new_idx = self.frames[frame_idx].upvals.len();
        if new_idx > u16::MAX as usize {
            return None;
        }
        self.frames[frame_idx].upvals.push(UpvalInfo {
            name: sym,
            source: outer,
        });
        Some(VarKind::Upval(new_idx as u16))
    }

    fn compile_expr(&mut self, expr: &KlExpr) -> Result<(), String> {
        match expr {
            KlExpr::Nil => self.emit_const(Value::Nil)?,
            KlExpr::Bool(b) => self.emit_const(Value::Bool(*b))?,
            KlExpr::Int(n) => self.emit_const(Value::Int(*n))?,
            KlExpr::Float(x) => self.emit_const(Value::Float(*x))?,
            KlExpr::Str(s) => self.emit_const(Value::Str(s.clone()))?,
            KlExpr::Sym(s) => {
                let idx = self.current_frame_idx();
                match self.resolve_var(idx, *s) {
                    Some(VarKind::Local(slot)) => self.emit(Op::LoadLocal(slot)),
                    Some(VarKind::Upval(uv)) => self.emit(Op::LoadUpval(uv)),
                    None => {
                        // Self-evaluating symbol (innocent-symbol
                        // semantics matches the tree-walker's
                        // `eval_symbol`).
                        self.emit_const(Value::Sym(*s))?;
                    }
                }
            }
            KlExpr::App(items) => self.compile_app(items)?,
        }
        Ok(())
    }

    fn compile_app(&mut self, items: &[KlExpr]) -> Result<(), String> {
        if items.is_empty() {
            return self.emit_const(Value::Nil);
        }
        if let KlExpr::Sym(head_sym) = &items[0] {
            let wk = &self.interp.well_known;
            let sym = *head_sym;
            if sym == wk.k_if {
                return self.compile_if(&items[1..]);
            }
            if sym == wk.k_let {
                return self.compile_let(&items[1..]);
            }
            if sym == wk.k_and {
                return self.compile_and(&items[1..]);
            }
            if sym == wk.k_or {
                return self.compile_or(&items[1..]);
            }
            if sym == wk.k_do {
                return self.compile_do(&items[1..]);
            }
            if sym == wk.k_cond {
                return self.compile_cond(&items[1..]);
            }
            if sym == wk.k_lambda {
                return self.compile_lambda(&items[1..]);
            }
            if sym == wk.k_freeze {
                return self.compile_freeze(&items[1..]);
            }
            if sym == wk.k_type {
                if items.len() < 2 {
                    return Err("type: expected at least 1 arg".into());
                }
                return self.compile_expr(&items[1]);
            }
            if sym == wk.k_defun || sym == wk.k_trap_error || sym == wk.k_thaw || sym == wk.k_quote
            {
                return Err(format!(
                    "vm: special form `{}` not yet supported",
                    self.interp.resolve(sym)
                ));
            }
        }

        // Plain call: head value, then args, then `Call(n)`.
        self.compile_head(&items[0])?;
        let n_args = items.len() - 1;
        if n_args > u8::MAX as usize {
            return Err(format!(
                "vm: more than u8::MAX args at call site ({n_args})"
            ));
        }
        for arg in &items[1..] {
            self.compile_expr(arg)?;
        }
        self.emit(Op::Call(n_args as u8));
        Ok(())
    }

    fn compile_head(&mut self, head: &KlExpr) -> Result<(), String> {
        if let KlExpr::Sym(s) = head {
            let idx = self.current_frame_idx();
            match self.resolve_var(idx, *s) {
                Some(VarKind::Local(slot)) => {
                    self.emit(Op::LoadLocal(slot));
                    return Ok(());
                }
                Some(VarKind::Upval(uv)) => {
                    self.emit(Op::LoadUpval(uv));
                    return Ok(());
                }
                None => {
                    let const_idx = self.add_const(Value::Sym(*s))?;
                    self.emit(Op::LoadGlobal(const_idx));
                    return Ok(());
                }
            }
        }
        self.compile_expr(head)
    }

    fn compile_if(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.len() != 3 {
            return Err("if: expected 3 args".into());
        }
        self.compile_expr(&args[0])?;
        let else_jump = self.emit_jump(Op::JumpFalse);
        self.compile_expr(&args[1])?;
        let end_jump = self.emit_jump(Op::Jump);
        self.patch_jump(else_jump)?;
        self.compile_expr(&args[2])?;
        self.patch_jump(end_jump)?;
        Ok(())
    }

    fn compile_let(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.len() != 3 {
            return Err("let: expected 3 args".into());
        }
        let var = match &args[0] {
            KlExpr::Sym(s) => *s,
            _ => return Err("let: var must be a symbol".into()),
        };
        self.compile_expr(&args[1])?;
        // Reserve a fresh slot in the *current* frame and shadow any
        // outer binding of the same name. Save the prior mapping so a
        // nested `(let X .. (let X .. X))` correctly restores the
        // outer slot after the inner body finishes.
        let (slot, prev) = {
            let f = self.top_mut();
            let slot = f.n_locals;
            f.n_locals = f
                .n_locals
                .checked_add(1)
                .ok_or_else(|| "vm: more than u16::MAX locals in one function".to_string())?;
            let prev = f.locals.insert(var, slot);
            (slot, prev)
        };
        self.emit(Op::StoreLocal(slot));
        let res = self.compile_expr(&args[2]);
        {
            let f = self.top_mut();
            match prev {
                Some(p) => {
                    f.locals.insert(var, p);
                }
                None => {
                    f.locals.remove(&var);
                }
            }
        }
        res
    }

    fn compile_and(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.len() != 2 {
            return Err("and: expected 2 args".into());
        }
        self.compile_expr(&args[0])?;
        let else_jump = self.emit_jump(Op::JumpFalse);
        self.compile_expr(&args[1])?;
        let end_jump = self.emit_jump(Op::Jump);
        self.patch_jump(else_jump)?;
        self.emit_const(Value::Bool(false))?;
        self.patch_jump(end_jump)?;
        Ok(())
    }

    fn compile_or(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.len() != 2 {
            return Err("or: expected 2 args".into());
        }
        self.compile_expr(&args[0])?;
        let eval_b = self.emit_jump(Op::JumpFalse);
        self.emit_const(Value::Bool(true))?;
        let end_jump = self.emit_jump(Op::Jump);
        self.patch_jump(eval_b)?;
        self.compile_expr(&args[1])?;
        self.patch_jump(end_jump)?;
        Ok(())
    }

    fn compile_do(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.is_empty() {
            return self.emit_const(Value::Nil);
        }
        let last = args.len() - 1;
        for (i, e) in args.iter().enumerate() {
            self.compile_expr(e)?;
            if i != last {
                self.emit(Op::Pop);
            }
        }
        Ok(())
    }

    fn compile_cond(&mut self, args: &[KlExpr]) -> Result<(), String> {
        let mut end_jumps: Vec<usize> = Vec::with_capacity(args.len());
        for clause in args {
            let pair = match clause {
                KlExpr::App(items) if items.len() == 2 => items,
                _ => return Err("cond: clauses must be 2-element lists".into()),
            };
            self.compile_expr(&pair[0])?;
            let next = self.emit_jump(Op::JumpFalse);
            self.compile_expr(&pair[1])?;
            end_jumps.push(self.emit_jump(Op::Jump));
            self.patch_jump(next)?;
        }
        // Kernel cond chains always end with a `(true ...)` catch-all,
        // so the fall-through is unreachable in practice; emit a
        // defensive `Nil` to keep the stack invariant for synthetic
        // inputs.
        self.emit_const(Value::Nil)?;
        for j in end_jumps {
            self.patch_jump(j)?;
        }
        Ok(())
    }

    fn compile_lambda(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.len() != 2 {
            return Err("lambda: expected (lambda PARAM BODY)".into());
        }
        let param = match &args[0] {
            KlExpr::Sym(s) => *s,
            _ => return Err("lambda: param must be a symbol".into()),
        };
        self.compile_nested(1, &[param], &args[1])
    }

    fn compile_freeze(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.len() != 1 {
            return Err("freeze: expected 1 arg".into());
        }
        self.compile_nested(0, &[], &args[0])
    }

    /// Compile a nested closure body in a fresh frame and emit the
    /// `MakeClosure` plus its upval loads in the current (outer) frame.
    fn compile_nested(
        &mut self,
        arity: usize,
        params: &[SymId],
        body: &KlExpr,
    ) -> Result<(), String> {
        // Enter the inner frame.
        self.frames.push(CompilerFrame::default());
        for &p in params {
            self.add_local(p);
        }
        // Compile the body. On error, drop the dangling frame so the
        // compiler isn't left in a bad state.
        if let Err(e) = self.compile_expr(body) {
            self.frames.pop();
            return Err(e);
        }
        self.emit(Op::Return);
        let inner = self.frames.pop().expect("vm: missing inner frame");
        let inner_upvals = inner.upvals;
        let inner_bf = Rc::new(BytecodeFn {
            name: None,
            arity,
            n_locals: inner.n_locals as usize,
            code: inner.code,
            consts: inner.consts,
            fn_consts: inner.fn_consts,
        });
        let fn_idx = self.add_fn_const(inner_bf)?;
        if inner_upvals.len() > u8::MAX as usize {
            return Err(format!(
                "vm: more than u8::MAX upvals on closure ({})",
                inner_upvals.len()
            ));
        }
        let n_upvals = inner_upvals.len() as u8;
        // Emit a `LoadLocal` / `LoadUpval` in the *outer* frame for
        // each upval, in declaration order. The VM's `MakeClosure`
        // will pop them in that same order into the new closure's
        // upvals array.
        for uv in &inner_upvals {
            match uv.source {
                VarKind::Local(slot) => self.emit(Op::LoadLocal(slot)),
                VarKind::Upval(idx) => self.emit(Op::LoadUpval(idx)),
            }
        }
        self.emit(Op::MakeClosure { fn_idx, n_upvals });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kl::parser::parse_one;
    use crate::vm::exec::exec;

    /// Parse a top-level KL expression and unwrap `(defun NAME PARAMS BODY)`
    /// into its constituent parts. Helper for tests.
    fn parse_defun(src: &str, interp: &mut Interp) -> (SymId, Vec<SymId>, KlExpr) {
        let form = parse_one(src, &mut interp.symbols).expect("parse");
        let items = match form {
            KlExpr::App(items) => items,
            other => panic!("expected (defun ...), got {other:?}"),
        };
        assert_eq!(items.len(), 4, "defun must have 4 elements");
        let name = match &items[1] {
            KlExpr::Sym(s) => *s,
            other => panic!("defun name not a symbol: {other:?}"),
        };
        let params: Vec<SymId> = match &items[2] {
            KlExpr::Nil => Vec::new(),
            KlExpr::App(ps) => ps
                .iter()
                .map(|p| match p {
                    KlExpr::Sym(s) => *s,
                    other => panic!("param not a symbol: {other:?}"),
                })
                .collect(),
            other => panic!("params not a list: {other:?}"),
        };
        let body = items[3].clone();
        (name, params, body)
    }

    #[test]
    fn double_via_vm() {
        let mut interp = Interp::new();
        let (name, params, body) = parse_defun("(defun double (X) (* X 2))", &mut interp);
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        let result = exec(&mut interp, &bf, &[], &[Value::Int(21)]).expect("exec");
        assert!(matches!(result, Value::Int(42)));
    }

    #[test]
    fn if_branches() {
        let mut interp = Interp::new();
        let (name, params, body) =
            parse_defun("(defun branch (X) (if (= X 0) 100 200))", &mut interp);
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        let zero = exec(&mut interp, &bf, &[], &[Value::Int(0)]).expect("exec");
        assert!(matches!(zero, Value::Int(100)));
        let other = exec(&mut interp, &bf, &[], &[Value::Int(7)]).expect("exec");
        assert!(matches!(other, Value::Int(200)));
    }

    #[test]
    fn fact_via_vm() {
        let mut interp = Interp::new();
        let (name, params, body) = parse_defun(
            "(defun fact (N) (if (= N 0) 1 (* N (fact (- N 1)))))",
            &mut interp,
        );
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        let bf_rc = Rc::new(bf);
        let bf_for_native = bf_rc.clone();
        interp.register_native("fact", 1, move |interp, args| {
            exec(interp, &bf_for_native, &[], args)
        });
        let result = exec(&mut interp, &bf_rc, &[], &[Value::Int(10)]).expect("exec");
        assert!(matches!(result, Value::Int(3628800)), "got {result:?}");
    }

    #[test]
    fn loop_sum_via_vm() {
        let mut interp = Interp::new();
        let (name, params, body) = parse_defun(
            "(defun loop-sum (N ACC) (if (= N 0) ACC (loop-sum (- N 1) (+ ACC 1))))",
            &mut interp,
        );
        let bf = Rc::new(compile_fn(&interp, Some(name), &params, &body).expect("compile"));
        let bf_for_native = bf.clone();
        interp.register_native("loop-sum", 2, move |interp, args| {
            exec(interp, &bf_for_native, &[], args)
        });
        let result = exec(&mut interp, &bf, &[], &[Value::Int(100), Value::Int(0)]).expect("exec");
        assert!(matches!(result, Value::Int(100)), "got {result:?}");
    }

    #[test]
    fn let_shadowing() {
        let mut interp = Interp::new();
        let (name, params, body) = parse_defun(
            "(defun shadow (X) (let X (* X 10) (let X (+ X 1) X)))",
            &mut interp,
        );
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        let result = exec(&mut interp, &bf, &[], &[Value::Int(5)]).expect("exec");
        // (let X (* 5 10) (let X (+ 50 1) X)) → 51
        assert!(matches!(result, Value::Int(51)), "got {result:?}");
    }

    #[test]
    fn do_returns_last() {
        let mut interp = Interp::new();
        let (name, params, body) =
            parse_defun("(defun seq (X) (do (+ X 1) (+ X 2) (+ X 3)))", &mut interp);
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        let result = exec(&mut interp, &bf, &[], &[Value::Int(10)]).expect("exec");
        assert!(matches!(result, Value::Int(13)));
    }

    #[test]
    fn and_short_circuits() {
        let mut interp = Interp::new();
        let (name, params, body) =
            parse_defun("(defun aab (X) (and (= X 1) (= X 1)))", &mut interp);
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        let truthy = exec(&mut interp, &bf, &[], &[Value::Int(1)]).expect("exec");
        assert!(matches!(truthy, Value::Bool(true)));
        let falsy = exec(&mut interp, &bf, &[], &[Value::Int(0)]).expect("exec");
        assert!(matches!(falsy, Value::Bool(false)));
    }

    #[test]
    fn or_short_circuits() {
        let mut interp = Interp::new();
        let (name, params, body) = parse_defun("(defun oab (X) (or (= X 1) (= X 2)))", &mut interp);
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        let one = exec(&mut interp, &bf, &[], &[Value::Int(1)]).expect("exec");
        assert!(matches!(one, Value::Bool(true)));
        let two = exec(&mut interp, &bf, &[], &[Value::Int(2)]).expect("exec");
        assert!(matches!(two, Value::Bool(true)));
        let neither = exec(&mut interp, &bf, &[], &[Value::Int(3)]).expect("exec");
        assert!(matches!(neither, Value::Bool(false)));
    }

    #[test]
    fn cond_cascade() {
        let mut interp = Interp::new();
        let (name, params, body) = parse_defun(
            "(defun grade (S) (cond ((>= S 90) 4) ((>= S 80) 3) ((>= S 70) 2) (true 0)))",
            &mut interp,
        );
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        let a = exec(&mut interp, &bf, &[], &[Value::Int(95)]).expect("exec");
        assert!(matches!(a, Value::Int(4)));
        let b = exec(&mut interp, &bf, &[], &[Value::Int(85)]).expect("exec");
        assert!(matches!(b, Value::Int(3)));
        let f = exec(&mut interp, &bf, &[], &[Value::Int(50)]).expect("exec");
        assert!(matches!(f, Value::Int(0)));
    }

    // ---- B3c tests: lambda / freeze + upvalue resolution ----

    #[test]
    fn lambda_captures_outer_let() {
        // (defun make-adder (Y) (lambda X (+ X Y)))
        // ((make-adder 10) 5) → 15
        let mut interp = Interp::new();
        let (name, params, body) =
            parse_defun("(defun make-adder (Y) (lambda X (+ X Y)))", &mut interp);
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        // Run it directly: make-adder(10) → closure that adds 10 to its arg.
        let closure = exec(&mut interp, &bf, &[], &[Value::Int(10)]).expect("exec");
        // Apply the closure to 5.
        let result = interp.apply(closure, vec![Value::Int(5)]).expect("apply");
        assert!(matches!(result, Value::Int(15)), "got {result:?}");
    }

    #[test]
    fn lambda_only_captures_referenced_vars() {
        // The inner lambda body references only Y, not Z. The
        // compiler's resolve_var should only register Y as an upval,
        // keeping the closure's upvals slim — matches the tree-walker's
        // Phase 1a free-var analysis behaviour.
        let mut interp = Interp::new();
        let (name, params, body) =
            parse_defun("(defun pick-y (Y Z) (lambda X (+ X Y)))", &mut interp);
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        // First sanity: the result still computes correctly.
        let closure =
            exec(&mut interp, &bf, &[], &[Value::Int(100), Value::Int(999)]).expect("exec");
        let result = interp
            .apply(closure.clone(), vec![Value::Int(1)])
            .expect("apply");
        assert!(matches!(result, Value::Int(101)), "got {result:?}");
        // Second: check the compiled closure has exactly one upval (Y),
        // not two. The closure is `Value::Closure(Rc<Closure>)` with
        // `ClosureKind::Bytecode(_, upvals)`.
        if let Value::Closure(c) = &closure {
            if let crate::value::ClosureKind::Bytecode(_, upvals) = &c.kind {
                assert_eq!(
                    upvals.len(),
                    1,
                    "expected 1 upval (Y), got {}",
                    upvals.len()
                );
            } else {
                panic!("closure kind is not Bytecode");
            }
        } else {
            panic!("expected Value::Closure, got {closure:?}");
        }
    }

    #[test]
    fn freeze_thaw_via_apply() {
        // (defun delayed (X) (freeze (* X X)))
        // Apply the freeze with zero args to get X*X.
        let mut interp = Interp::new();
        let (name, params, body) = parse_defun("(defun delayed (X) (freeze (* X X)))", &mut interp);
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        let frozen = exec(&mut interp, &bf, &[], &[Value::Int(7)]).expect("exec");
        let result = interp.apply(frozen, vec![]).expect("apply");
        assert!(matches!(result, Value::Int(49)), "got {result:?}");
    }

    #[test]
    fn nested_lambda_two_levels() {
        // (defun outer (A) (lambda B (lambda C (+ A (+ B C)))))
        // ((((outer 1) 2) 3)) — well, two applications: ((outer 1) 2) → closure,
        // then that applied to 3 → 6. Tests upval propagation through two levels:
        // the innermost (lambda C ...) needs A as an upval, but A is local to
        // the OUTERMOST frame; resolve_var must thread it through the middle.
        let mut interp = Interp::new();
        let (name, params, body) = parse_defun(
            "(defun mk (A) (lambda B (lambda C (+ A (+ B C)))))",
            &mut interp,
        );
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        let lvl1 = exec(&mut interp, &bf, &[], &[Value::Int(1)]).expect("exec");
        let lvl2 = interp.apply(lvl1, vec![Value::Int(2)]).expect("apply lvl1");
        let result = interp.apply(lvl2, vec![Value::Int(3)]).expect("apply lvl2");
        assert!(matches!(result, Value::Int(6)), "got {result:?}");
    }
}
