//! Bytecode dispatch loop.
//!
//! Mirrors shen-go's `vmExec` (`../shen-go/kl/vm.go:174–280`) in shape:
//! flat `Vec<Value>` locals + small operand stack, `loop { match op }`
//! over the linear instruction stream. Tail calls return up to the
//! outer caller via a sentinel so the Rust stack doesn't grow on
//! mutual recursion.
//!
//! B1 (current) implements the always-terminal subset:
//! `LoadConst` / `LoadLocal` / `StoreLocal` / `Pop` / `Return` /
//! `Call(n)`. The rest (`Jump`/`JumpFalse`, closures, tail calls,
//! numeric fast-paths) are stubs that return an error so any path that
//! reaches them is loudly diagnosable during the staged rollout.

use std::rc::Rc;

use crate::error::{ShenError, ShenResult};
use crate::interp::eval::Interp;
use crate::value::{Closure, ClosureKind, Value};
use crate::vm::bytecode::BytecodeFn;
use crate::vm::opcode::Op;

/// Execute a compiled function. `args` is placed in `locals[0..arity)`
/// and the rest of `locals` is filled with `Value::Nil`. `upvals` are
/// the values captured at closure creation time (`MakeClosure`).
/// Returns the value the function evaluates to.
pub fn exec(
    interp: &mut Interp,
    bf: &BytecodeFn,
    upvals: &[Value],
    args: &[Value],
) -> ShenResult<Value> {
    if args.len() != bf.arity {
        return Err(ShenError::new(format!(
            "vm: arity mismatch — expected {}, got {}",
            bf.arity,
            args.len()
        )));
    }

    let mut locals: Vec<Value> = Vec::with_capacity(bf.n_locals);
    locals.extend_from_slice(args);
    locals.resize(bf.n_locals, Value::Nil);

    let mut stack: Vec<Value> = Vec::with_capacity(8);
    let mut pc: usize = 0;

    loop {
        let op = bf
            .code
            .get(pc)
            .copied()
            .ok_or_else(|| ShenError::new("vm: pc past end of code"))?;
        pc += 1;
        match op {
            Op::LoadConst(idx) => {
                let v = bf
                    .consts
                    .get(idx as usize)
                    .cloned()
                    .ok_or_else(|| ShenError::new("vm: bad const index"))?;
                stack.push(v);
            }
            Op::LoadLocal(slot) => {
                let v = locals
                    .get(slot as usize)
                    .cloned()
                    .ok_or_else(|| ShenError::new("vm: bad local slot"))?;
                stack.push(v);
            }
            Op::StoreLocal(slot) => {
                let v = stack
                    .pop()
                    .ok_or_else(|| ShenError::new("vm: stack underflow on StoreLocal"))?;
                let s = slot as usize;
                if s >= locals.len() {
                    return Err(ShenError::new("vm: bad local slot on StoreLocal"));
                }
                locals[s] = v;
            }
            Op::Pop => {
                stack
                    .pop()
                    .ok_or_else(|| ShenError::new("vm: stack underflow on Pop"))?;
            }
            Op::Return => {
                return Ok(stack.pop().unwrap_or(Value::Nil));
            }
            Op::Call(n) => {
                let n = n as usize;
                if stack.len() < n + 1 {
                    return Err(ShenError::new("vm: stack underflow on Call"));
                }
                let args_start = stack.len() - n;
                let args: Vec<Value> = stack.drain(args_start..).collect();
                let callee = stack.pop().expect("checked above");
                let result = interp.apply(callee, args)?;
                stack.push(result);
            }
            Op::LoadGlobal(idx) => {
                let v = bf
                    .consts
                    .get(idx as usize)
                    .cloned()
                    .ok_or_else(|| ShenError::new("vm: bad const index for LoadGlobal"))?;
                let sym = match v {
                    Value::Sym(s) => s,
                    other => {
                        return Err(ShenError::new(format!(
                            "vm: LoadGlobal const must be a Sym, got {other:?}"
                        )))
                    }
                };
                let f = interp.env.get_fn(sym).cloned().ok_or_else(|| {
                    ShenError::new(format!("vm: undefined function `{}`", interp.resolve(sym)))
                })?;
                stack.push(f);
            }
            Op::Jump(delta) => {
                pc = jump_target(pc, delta)?;
            }
            Op::JumpFalse(delta) => {
                let v = stack
                    .pop()
                    .ok_or_else(|| ShenError::new("vm: stack underflow on JumpFalse"))?;
                let truthy = match v {
                    Value::Bool(b) => b,
                    Value::Sym(s) if s == interp.well_known.k_true => true,
                    Value::Sym(s) if s == interp.well_known.k_false => false,
                    other => {
                        return Err(ShenError::new(format!(
                            "vm: JumpFalse on non-boolean: {other:?}"
                        )))
                    }
                };
                if !truthy {
                    pc = jump_target(pc, delta)?;
                }
            }
            Op::LoadUpval(idx) => {
                let v = upvals
                    .get(idx as usize)
                    .cloned()
                    .ok_or_else(|| ShenError::new("vm: bad upval index"))?;
                stack.push(v);
            }
            Op::MakeClosure { fn_idx, n_upvals } => {
                let n = n_upvals as usize;
                if stack.len() < n {
                    return Err(ShenError::new("vm: stack underflow on MakeClosure"));
                }
                let captured: Vec<Value> = stack.drain(stack.len() - n..).collect();
                let inner = bf
                    .fn_consts
                    .get(fn_idx as usize)
                    .cloned()
                    .ok_or_else(|| ShenError::new("vm: bad fn_idx on MakeClosure"))?;
                let closure = Closure {
                    name: inner.name,
                    arity: inner.arity,
                    partial: Vec::new(),
                    kind: ClosureKind::Bytecode(inner, captured),
                };
                stack.push(Value::Closure(Rc::new(closure)));
            }
            Op::SelfTailCall(n) => {
                // In-place tail-recursion: copy the top n stack values
                // into `locals[0..n]` and reset `pc = 0`. The Rust
                // stack doesn't grow because we stay inside this
                // `vm::exec` invocation. Lets in slots [n..n_locals]
                // are left as-is; they'll be reassigned by `StoreLocal`
                // as the body re-executes, matching shen-go's behavior
                // (`../shen-go/kl/vm.go:258–263`).
                let n = n as usize;
                if stack.len() < n {
                    return Err(ShenError::new("vm: stack underflow on SelfTailCall"));
                }
                if n > locals.len() {
                    return Err(ShenError::new("vm: SelfTailCall n > n_locals"));
                }
                let new_args_start = stack.len() - n;
                for (i, v) in stack.drain(new_args_start..).enumerate() {
                    locals[i] = v;
                }
                pc = 0;
            }
            // ---- Inlined primitives (B4b) ----------------------------
            // Each one mirrors the `aot::runtime::*` helper of the
            // same name; we keep the helpers as the single source of
            // truth for the semantics.
            Op::Add => binop_fallible(&mut stack, crate::aot::runtime::add)?,
            Op::Sub => binop_fallible(&mut stack, crate::aot::runtime::sub)?,
            Op::Mul => binop_fallible(&mut stack, crate::aot::runtime::mul)?,
            Op::Div => binop_fallible(&mut stack, crate::aot::runtime::div)?,
            Op::Lt => binop_fallible(&mut stack, crate::aot::runtime::lt)?,
            Op::Le => binop_fallible(&mut stack, crate::aot::runtime::lte)?,
            Op::Gt => binop_fallible(&mut stack, crate::aot::runtime::gt)?,
            Op::Ge => binop_fallible(&mut stack, crate::aot::runtime::gte)?,
            Op::Eq => binop_infallible(&mut stack, crate::aot::runtime::eq)?,
            Op::Cons => binop_infallible(&mut stack, crate::aot::runtime::cons)?,
            Op::Hd => unop_fallible(&mut stack, crate::aot::runtime::hd)?,
            Op::Tl => unop_fallible(&mut stack, crate::aot::runtime::tl)?,
            Op::IsCons => unop_infallible(&mut stack, crate::aot::runtime::is_cons)?,
            Op::IsNumber => unop_infallible(&mut stack, crate::aot::runtime::is_number)?,
            Op::IsString => unop_infallible(&mut stack, crate::aot::runtime::is_string)?,
            Op::IsSymbol => unop_infallible(&mut stack, crate::aot::runtime::is_symbol)?,
            Op::IsAbsvector => unop_infallible(&mut stack, crate::aot::runtime::is_absvector)?,
            // Cross-tier TailCall lands in B5/B6.
            Op::TailCall(_) => {
                return Err(ShenError::new(format!(
                    "vm: opcode {op:?} not implemented in this phase"
                )));
            }
        }
    }
}

#[inline]
fn binop_fallible(
    stack: &mut Vec<Value>,
    f: fn(&Value, &Value) -> ShenResult<Value>,
) -> ShenResult<()> {
    let b = stack
        .pop()
        .ok_or_else(|| ShenError::new("vm: stack underflow on binop"))?;
    let a = stack
        .pop()
        .ok_or_else(|| ShenError::new("vm: stack underflow on binop"))?;
    let v = f(&a, &b)?;
    stack.push(v);
    Ok(())
}

#[inline]
fn binop_infallible(stack: &mut Vec<Value>, f: fn(&Value, &Value) -> Value) -> ShenResult<()> {
    let b = stack
        .pop()
        .ok_or_else(|| ShenError::new("vm: stack underflow on binop"))?;
    let a = stack
        .pop()
        .ok_or_else(|| ShenError::new("vm: stack underflow on binop"))?;
    let v = f(&a, &b);
    stack.push(v);
    Ok(())
}

#[inline]
fn unop_fallible(stack: &mut Vec<Value>, f: fn(&Value) -> ShenResult<Value>) -> ShenResult<()> {
    let a = stack
        .pop()
        .ok_or_else(|| ShenError::new("vm: stack underflow on unop"))?;
    let v = f(&a)?;
    stack.push(v);
    Ok(())
}

#[inline]
fn unop_infallible(stack: &mut Vec<Value>, f: fn(&Value) -> Value) -> ShenResult<()> {
    let a = stack
        .pop()
        .ok_or_else(|| ShenError::new("vm: stack underflow on unop"))?;
    let v = f(&a);
    stack.push(v);
    Ok(())
}

/// Compute the new `pc` after a Jump/JumpFalse. `pc` here is the value
/// *already past* the jump instruction (we increment before dispatch),
/// so the absolute target is `pc + delta`. Errors only on a target
/// that would be negative — out-of-range past-end errors are caught by
/// the next iteration's bounds check.
#[inline]
fn jump_target(pc: usize, delta: i16) -> ShenResult<usize> {
    let target = (pc as i32) + (delta as i32);
    if target < 0 {
        Err(ShenError::new(format!(
            "vm: jump target out of range ({target})"
        )))
    } else {
        Ok(target as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Closure, ClosureKind};
    use std::rc::Rc;

    fn fresh_interp() -> Interp {
        Interp::new()
    }

    /// Wrap a BytecodeFn as a `Value::Closure` with `ClosureKind::Bytecode`,
    /// no captured upvals. Used by B3a tests to verify the dispatch path
    /// (`call_or_apply` → `vm::exec`, `Interp::apply` / `call_strict` →
    /// `vm::exec`).
    fn bytecode_closure(bf: BytecodeFn, name: Option<crate::symbol::SymId>) -> Value {
        let arity = bf.arity;
        Value::Closure(Rc::new(Closure {
            name,
            arity,
            partial: Vec::new(),
            kind: ClosureKind::Bytecode(Rc::new(bf), Vec::new()),
        }))
    }

    #[test]
    fn dispatch_via_interp_apply() {
        // (defun id (X) X) — wrap as ClosureKind::Bytecode and call
        // through `Interp::apply`. Exercises the call_strict path.
        let mut interp = fresh_interp();
        let name = interp.intern("id-vm");
        let bf = BytecodeFn {
            name: Some(name),
            arity: 1,
            n_locals: 1,
            code: vec![Op::LoadLocal(0), Op::Return],
            consts: vec![],
            fn_consts: vec![],
        };
        let f = bytecode_closure(bf, Some(name));
        let r = interp.apply(f, vec![Value::Int(7)]).expect("apply");
        assert!(matches!(r, Value::Int(7)));
    }

    #[test]
    fn dispatch_via_apply_named_fast_path() {
        // Register a bytecode-backed closure in the env, then call
        // through rt::apply_named which routes through `call_or_apply`.
        // The Fast::Bytecode arm of call_or_apply is the path under test.
        let mut interp = fresh_interp();
        let name = "id-vm";
        let sym = interp.intern(name);
        let bf = BytecodeFn {
            name: Some(sym),
            arity: 1,
            n_locals: 1,
            code: vec![Op::LoadLocal(0), Op::Return],
            consts: vec![],
            fn_consts: vec![],
        };
        interp.env.set_fn(sym, bytecode_closure(bf, Some(sym)));
        let r = crate::aot::runtime::apply_named(&mut interp, "id-vm", &[Value::Int(99)])
            .expect("apply_named");
        assert!(matches!(r, Value::Int(99)));
    }

    #[test]
    fn identity_function() {
        // (defun id (X) X) compiles to:
        //   LoadLocal(0)      ; push X
        //   Return            ; return top of stack
        let mut interp = fresh_interp();
        let id_sym = interp.intern("id");
        let bf = BytecodeFn {
            name: Some(id_sym),
            arity: 1,
            n_locals: 1,
            code: vec![Op::LoadLocal(0), Op::Return],
            consts: vec![],
            fn_consts: vec![],
        };
        let result = exec(&mut interp, &bf, &[], &[Value::Int(42)]).expect("exec");
        assert!(matches!(result, Value::Int(42)));
    }

    #[test]
    fn constant_function() {
        // (defun answer () 42) compiles to:
        //   LoadConst(0)      ; push 42
        //   Return
        let mut interp = fresh_interp();
        let bf = BytecodeFn {
            name: Some(interp.intern("answer")),
            arity: 0,
            n_locals: 0,
            code: vec![Op::LoadConst(0), Op::Return],
            consts: vec![Value::Int(42)],
            fn_consts: vec![],
        };
        let result = exec(&mut interp, &bf, &[], &[]).expect("exec");
        assert!(matches!(result, Value::Int(42)));
    }

    #[test]
    fn let_via_store_local() {
        // (defun double-via-let (X) (let Y X Y)) compiles to:
        //   LoadLocal(0)      ; push X
        //   StoreLocal(1)     ; locals[1] = X   (Y's slot)
        //   LoadLocal(1)      ; push Y
        //   Return
        let mut interp = fresh_interp();
        let bf = BytecodeFn {
            name: Some(interp.intern("double-via-let")),
            arity: 1,
            n_locals: 2,
            code: vec![
                Op::LoadLocal(0),
                Op::StoreLocal(1),
                Op::LoadLocal(1),
                Op::Return,
            ],
            consts: vec![],
            fn_consts: vec![],
        };
        let result = exec(&mut interp, &bf, &[], &[Value::Int(7)]).expect("exec");
        assert!(matches!(result, Value::Int(7)));
    }

    #[test]
    fn call_via_existing_primitive() {
        // (defun plus-one (X) (+ X 1)) — exercise the Call path through
        // the live interpreter's registered `+` primitive.
        //   LoadConst(0)        ; push the `+` Value::Closure
        //   LoadLocal(0)        ; push X
        //   LoadConst(1)        ; push 1
        //   Call(2)             ; (+ X 1)
        //   Return
        let mut interp = fresh_interp();
        let plus_sym = interp.intern("+");
        let plus = interp
            .env
            .get_fn(plus_sym)
            .cloned()
            .expect("+ should be registered by Interp::new()");
        let bf = BytecodeFn {
            name: Some(interp.intern("plus-one")),
            arity: 1,
            n_locals: 1,
            code: vec![
                Op::LoadConst(0),
                Op::LoadLocal(0),
                Op::LoadConst(1),
                Op::Call(2),
                Op::Return,
            ],
            consts: vec![plus, Value::Int(1)],
            fn_consts: vec![],
        };
        let result = exec(&mut interp, &bf, &[], &[Value::Int(41)]).expect("exec");
        assert!(matches!(result, Value::Int(42)));
    }

    #[test]
    fn make_closure_zero_upvals() {
        // Outer creates an inner `(lambda X X)` (no upvals captured),
        // calls it with 42, returns the result.
        // Inner code:  LoadLocal(0), Return       (just returns its arg)
        // Outer code:  MakeClosure{fn_idx=0, n_upvals=0}  -> stack: [closure]
        //              LoadConst(0)              -> stack: [closure, 42]
        //              Call(1)                   -> stack: [42]
        //              Return
        let mut interp = fresh_interp();
        let inner = Rc::new(BytecodeFn {
            name: None,
            arity: 1,
            n_locals: 1,
            code: vec![Op::LoadLocal(0), Op::Return],
            consts: vec![],
            fn_consts: vec![],
        });
        let outer = BytecodeFn {
            name: Some(interp.intern("outer")),
            arity: 0,
            n_locals: 0,
            code: vec![
                Op::MakeClosure {
                    fn_idx: 0,
                    n_upvals: 0,
                },
                Op::LoadConst(0),
                Op::Call(1),
                Op::Return,
            ],
            consts: vec![Value::Int(42)],
            fn_consts: vec![inner],
        };
        let result = exec(&mut interp, &outer, &[], &[]).expect("exec");
        assert!(matches!(result, Value::Int(42)));
    }

    #[test]
    fn make_closure_with_upval() {
        // Outer captures Y=10 into an inner `(lambda X (+ Y X))`, calls
        // it with X=5, returns 15. Exercises LoadUpval inside the inner
        // body and MakeClosure popping captures off the outer stack.
        let mut interp = fresh_interp();
        // Inner: LoadGlobal(0)  -- push `+`
        //        LoadUpval(0)   -- push Y
        //        LoadLocal(0)   -- push X
        //        Call(2); Return
        let inner = Rc::new(BytecodeFn {
            name: Some(interp.intern("inner-add")),
            arity: 1,
            n_locals: 1,
            code: vec![
                Op::LoadGlobal(0),
                Op::LoadUpval(0),
                Op::LoadLocal(0),
                Op::Call(2),
                Op::Return,
            ],
            consts: vec![Value::Sym(interp.intern("+"))],
            fn_consts: vec![],
        });
        // Outer: LoadConst(0)              -- push 10  (will become upval)
        //        MakeClosure{0, 1}         -- pop 1 upval, push closure
        //        LoadConst(1)              -- push 5   (call arg)
        //        Call(1); Return
        let outer = BytecodeFn {
            name: Some(interp.intern("outer-add")),
            arity: 0,
            n_locals: 0,
            code: vec![
                Op::LoadConst(0),
                Op::MakeClosure {
                    fn_idx: 0,
                    n_upvals: 1,
                },
                Op::LoadConst(1),
                Op::Call(1),
                Op::Return,
            ],
            consts: vec![Value::Int(10), Value::Int(5)],
            fn_consts: vec![inner],
        };
        let result = exec(&mut interp, &outer, &[], &[]).expect("exec");
        assert!(matches!(result, Value::Int(15)), "got {result:?}");
    }

    #[test]
    fn arity_mismatch_is_an_error() {
        let mut interp = fresh_interp();
        let bf = BytecodeFn {
            name: Some(interp.intern("id")),
            arity: 1,
            n_locals: 1,
            code: vec![Op::LoadLocal(0), Op::Return],
            consts: vec![],
            fn_consts: vec![],
        };
        assert!(exec(&mut interp, &bf, &[], &[]).is_err());
    }
}
