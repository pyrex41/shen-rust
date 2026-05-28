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

use crate::error::{ShenError, ShenResult};
use crate::interp::eval::Interp;
use crate::value::Value;
use crate::vm::bytecode::BytecodeFn;
use crate::vm::opcode::Op;

/// Execute a compiled function. `args` is placed in `locals[0..arity)`
/// and the rest of `locals` is filled with `Value::Nil`. Returns the
/// value the function evaluates to.
pub fn exec(
    interp: &mut Interp,
    bf: &BytecodeFn,
    _upvals: &[Value],
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
            // The remaining opcodes land in later B-phases (closures,
            // jumps, tail calls, numeric fast paths). Loudly reject so
            // a stray emission can't silently produce nonsense.
            Op::LoadUpval(_)
            | Op::Jump(_)
            | Op::JumpFalse(_)
            | Op::TailCall(_)
            | Op::SelfTailCall(_)
            | Op::MakeClosure { .. } => {
                return Err(ShenError::new(format!(
                    "vm: opcode {op:?} not implemented in this phase"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_interp() -> Interp {
        Interp::new()
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
        };
        let result = exec(&mut interp, &bf, &[], &[Value::Int(41)]).expect("exec");
        assert!(matches!(result, Value::Int(42)));
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
        };
        assert!(exec(&mut interp, &bf, &[], &[]).is_err());
    }
}
