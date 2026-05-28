//! Compile a `KlExpr` body into a `BytecodeFn`.
//!
//! Roughly mirrors shen-go's `klCompiler` (`../shen-go/kl/compiler.go`):
//! per-function state tracks local-slot allocation by `SymId`, the
//! constant pool, and the linear instruction stream. Each special form
//! is lowered statically — `if` becomes `JumpFalse`/`Jump`, `let`
//! becomes `StoreLocal` into a fresh slot, `cond` becomes a cascade of
//! conditional jumps, and so on. No special-form dispatch happens at
//! runtime; the VM loop is a flat `switch`.
//!
//! B2 scope: literals, locals, plain calls (head Sym → `LoadGlobal`),
//! and the special forms `if` / `let` / `cond` / `do` / `and` / `or`.
//! `lambda` / `freeze` / `defun` / `quote` / `trap-error` / `thaw` / `type`
//! land in later phases.

use std::collections::HashMap;

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
    c.code.push(Op::Return);
    Ok(BytecodeFn {
        name,
        arity: params.len(),
        n_locals: c.n_locals as usize,
        code: c.code,
        consts: c.consts,
        fn_consts: c.fn_consts,
    })
}

struct Compiler<'a> {
    interp: &'a Interp,
    /// Map from in-scope local name to its slot in the runtime
    /// `locals` array. Nested `let` shadowing is handled by save/restore
    /// of the previous mapping around the body compile.
    locals: HashMap<SymId, u16>,
    /// Next free local slot. Grows monotonically as `let` introduces
    /// new bindings — we don't reclaim slots after a `let` body ends
    /// because the size is bounded by the source structure anyway.
    n_locals: u16,
    code: Vec<Op>,
    consts: Vec<Value>,
    fn_consts: Vec<std::rc::Rc<BytecodeFn>>,
}

impl<'a> Compiler<'a> {
    fn new(interp: &'a Interp) -> Self {
        Self {
            interp,
            locals: HashMap::new(),
            n_locals: 0,
            code: Vec::new(),
            consts: Vec::new(),
            fn_consts: Vec::new(),
        }
    }

    fn add_local(&mut self, sym: SymId) -> u16 {
        let slot = self.n_locals;
        self.n_locals = self
            .n_locals
            .checked_add(1)
            .expect("vm: more than u16::MAX locals in one function");
        self.locals.insert(sym, slot);
        slot
    }

    fn add_const(&mut self, v: Value) -> Result<u16, String> {
        let idx = self.consts.len();
        if idx > u16::MAX as usize {
            return Err("vm: more than u16::MAX constants in one function".into());
        }
        self.consts.push(v);
        Ok(idx as u16)
    }

    fn emit_const(&mut self, v: Value) -> Result<(), String> {
        let idx = self.add_const(v)?;
        self.code.push(Op::LoadConst(idx));
        Ok(())
    }

    /// Emit a jump with a placeholder offset; returns the index of the
    /// emitted instruction so the caller can patch it once the target
    /// is known.
    fn emit_jump(&mut self, mk: fn(i16) -> Op) -> usize {
        let idx = self.code.len();
        self.code.push(mk(0));
        idx
    }

    /// Patch a previously-emitted jump to target the current `pc`.
    fn patch_jump(&mut self, idx: usize) -> Result<(), String> {
        let target = self.code.len();
        // The VM reads `bf.code[pc]` then advances `pc += 1` *before*
        // dispatching, so `Jump(d)` lands at `pc_after + d`, where
        // `pc_after = idx + 1`. Solve for `d = target - (idx + 1)`.
        let delta = (target as i32) - (idx as i32 + 1);
        if !(i16::MIN as i32..=i16::MAX as i32).contains(&delta) {
            return Err(format!("vm: jump delta out of i16 range: {delta}"));
        }
        let delta = delta as i16;
        let patched = match self.code[idx] {
            Op::Jump(_) => Op::Jump(delta),
            Op::JumpFalse(_) => Op::JumpFalse(delta),
            other => return Err(format!("vm: patch_jump on non-jump opcode {other:?}")),
        };
        self.code[idx] = patched;
        Ok(())
    }

    fn compile_expr(&mut self, expr: &KlExpr) -> Result<(), String> {
        match expr {
            KlExpr::Nil => self.emit_const(Value::Nil)?,
            KlExpr::Bool(b) => self.emit_const(Value::Bool(*b))?,
            KlExpr::Int(n) => self.emit_const(Value::Int(*n))?,
            KlExpr::Float(x) => self.emit_const(Value::Float(*x))?,
            KlExpr::Str(s) => self.emit_const(Value::Str(s.clone()))?,
            KlExpr::Sym(s) => {
                // Operand-position symbol: lexical binding if in scope,
                // otherwise self-evaluating (innocent-symbol semantics
                // matches the tree-walker's `eval_symbol`).
                if let Some(&slot) = self.locals.get(s) {
                    self.code.push(Op::LoadLocal(slot));
                } else {
                    self.emit_const(Value::Sym(*s))?;
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
            if sym == wk.k_type {
                // (type X T) — type annotation; just evaluate X.
                if items.len() < 2 {
                    return Err("type: expected at least 1 arg".into());
                }
                return self.compile_expr(&items[1]);
            }
            if sym == wk.k_lambda
                || sym == wk.k_freeze
                || sym == wk.k_defun
                || sym == wk.k_trap_error
                || sym == wk.k_thaw
                || sym == wk.k_quote
            {
                return Err(format!(
                    "vm: special form `{}` not yet supported in B2",
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
        self.code.push(Op::Call(n_args as u8));
        Ok(())
    }

    /// Compile the head of a call. Head-position `Sym` not in lexical
    /// scope resolves to the function namespace (`LoadGlobal`); any
    /// other shape falls through to operand-style compilation.
    fn compile_head(&mut self, head: &KlExpr) -> Result<(), String> {
        if let KlExpr::Sym(s) = head {
            if !self.locals.contains_key(s) {
                let idx = self.add_const(Value::Sym(*s))?;
                self.code.push(Op::LoadGlobal(idx));
                return Ok(());
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
        // Reserve a fresh slot for the binding. We save/restore the
        // previous mapping so nested `let` with the same name correctly
        // restores the outer slot after the inner body finishes.
        let slot = self.n_locals;
        self.n_locals = self
            .n_locals
            .checked_add(1)
            .ok_or_else(|| "vm: more than u16::MAX locals in one function".to_string())?;
        let prev = self.locals.insert(var, slot);
        self.code.push(Op::StoreLocal(slot));
        let res = self.compile_expr(&args[2]);
        match prev {
            Some(p) => {
                self.locals.insert(var, p);
            }
            None => {
                self.locals.remove(&var);
            }
        }
        res
    }

    fn compile_and(&mut self, args: &[KlExpr]) -> Result<(), String> {
        if args.len() != 2 {
            return Err("and: expected 2 args".into());
        }
        // (and A B): A; JumpFalse(else); B; Jump(end); else: false; end:
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
        // (or A B): A; JumpFalse(eval_b); true; Jump(end); eval_b: B; end:
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
        // Evaluate all but the last for side effects (pop the result);
        // the last expression's value is the `do` value.
        let last = args.len() - 1;
        for (i, e) in args.iter().enumerate() {
            self.compile_expr(e)?;
            if i != last {
                self.code.push(Op::Pop);
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
        // so this fall-through is unreachable in practice — but we
        // emit a defensive `Nil` so the stack invariant holds even on
        // synthetic inputs.
        self.emit_const(Value::Nil)?;
        for j in end_jumps {
            self.patch_jump(j)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kl::parser::parse_one;
    use crate::vm::exec::exec;

    /// Parse a top-level KL expression and unwrap `(defun NAME PARAMS BODY)`
    /// into its constituent parts. Helper for tests that want to start
    /// from concrete Shen source rather than hand-built ASTs.
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
        // Register the compiled fn so the recursive (fact ...) call resolves.
        let bf = compile_fn(&interp, Some(name), &params, &body).expect("compile");
        // Stash the bytecode as a Native closure that re-enters exec.
        // (B5 will wire this through the normal Closure machinery; this
        // is a B2 test scaffold.)
        let bf_rc = std::rc::Rc::new(bf);
        let bf_for_native = bf_rc.clone();
        interp.register_native("fact", 1, move |interp, args| {
            exec(interp, &bf_for_native, &[], args)
        });
        let result = exec(&mut interp, &bf_rc, &[], &[Value::Int(10)]).expect("exec");
        assert!(matches!(result, Value::Int(3628800)), "got {result:?}");
    }

    #[test]
    fn loop_sum_via_vm() {
        // Recursive (non-self-tail in B2; B4 makes it a SelfTailCall).
        // Use modest N to stay within the Rust stack.
        let mut interp = Interp::new();
        let (name, params, body) = parse_defun(
            "(defun loop-sum (N ACC) (if (= N 0) ACC (loop-sum (- N 1) (+ ACC 1))))",
            &mut interp,
        );
        let bf =
            std::rc::Rc::new(compile_fn(&interp, Some(name), &params, &body).expect("compile"));
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
}
