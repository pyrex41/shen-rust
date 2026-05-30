//! Bytecode dispatch loop.
//!
//! Execution model (A1 + A2): a single `exec` invocation runs an entire
//! tree of bytecode-to-bytecode calls. There is **one** value stack and
//! **one** call-frame stack per `exec` entry, both reused across every
//! call within that tree — no per-call `Vec` allocation, and no Rust-stack
//! recursion for bytecode callees. This is the CPython / Lua / shen-go
//! model.
//!
//! * **Value stack** (`stack: Vec<Value>`). Each frame's locals live at
//!   `stack[base .. base + n_locals]`; operands are pushed above them.
//!   `LoadLocal(slot)` reads `stack[base + slot]`; operand pushes/pops act
//!   on the top. `floor = base + n_locals` is the boundary an operand pop
//!   must never cross.
//! * **Frame stack** (`frames: Vec<Frame>`). A `Call` to an exact-arity,
//!   non-partial *bytecode* callee suspends the caller's frame and
//!   continues the same loop with `pc = 0` (no Rust recursion). `Return`
//!   pops a frame and writes the result into the caller's operand slot.
//!   `TailCall` *replaces* the current frame in place — true cross-function
//!   tail-call optimisation, so deeply/mutually tail-recursive Shen code
//!   runs in constant frame space.
//!
//! Calls to `Native` / AOT / tree-walked `Lambda` callees (and partial or
//! over-application) still go out to `Interp::apply` — those are leaves
//! from the VM's perspective. Self-tail-calls keep their dedicated
//! `SelfTailCall` fast path (in-place arg rebind, no frame work).

use std::rc::Rc;

use crate::error::{ShenError, ShenResult};
use crate::interp::eval::Interp;
use crate::value::{Closure, ClosureKind, Value};
use crate::vm::bytecode::BytecodeFn;
use crate::vm::opcode::Op;

// The single value stack: locals + operands for every live frame.
type Stack = Vec<Value>;

/// A suspended caller. The *current* frame's state is kept in local
/// variables of `exec` for speed; `frames` holds the callers waiting for
/// it to return.
struct Frame {
    bf: Rc<BytecodeFn>,
    /// Index in `stack` of this frame's `locals[0]`.
    base: usize,
    /// Saved program counter (points at the instruction after the call
    /// that suspended this frame).
    pc: usize,
    /// Values captured at closure-creation time (`MakeClosure`).
    upvals: Vec<Value>,
    /// Index in `stack` where this frame's return value must be written
    /// (the slot the callee occupied). `usize::MAX` for the outermost
    /// frame, which instead returns out of `exec`.
    ret_dst: usize,
}

/// Execute a compiled function. `args` is placed in `locals[0..arity)` and
/// the remaining locals are `Value::nil()`. `upvals` are the values captured
/// at closure creation. Returns the value the function evaluates to.
pub fn exec(
    interp: &mut Interp,
    bf: &Rc<BytecodeFn>,
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

    // Frame 0 lives at base 0. Args become locals[0..arity); the rest of
    // the locals slots are Nil. Operands push above `floor`.
    let mut stack: Stack = Vec::with_capacity(bf.n_locals + 8);
    stack.extend(args.iter().cloned());
    stack.resize(bf.n_locals, Value::nil());

    let mut frames: Vec<Frame> = Vec::new();
    let mut cur_bf: Rc<BytecodeFn> = Rc::clone(bf);
    let mut cur_upvals: Vec<Value> = upvals.to_vec();
    let mut base: usize = 0;
    let mut pc: usize = 0;
    let mut ret_dst: usize = usize::MAX;
    let mut floor: usize = bf.n_locals;

    loop {
        // Charge a reduction step against any active budget/deadline so a
        // Call-heavy bytecode tree is cancelable mid-run, exactly like the
        // tree-walked trampoline in `eval_in`.
        interp.charge_step()?;

        let op = cur_bf
            .code
            .get(pc)
            .copied()
            .ok_or_else(|| ShenError::new("vm: pc past end of code"))?;
        pc += 1;
        match op {
            Op::LoadConst(idx) => {
                let v = cur_bf
                    .consts
                    .get(idx as usize)
                    .cloned()
                    .ok_or_else(|| ShenError::new("vm: bad const index"))?;
                stack.push(v);
            }
            Op::LoadLocal(slot) => {
                let s = slot as usize;
                if s >= cur_bf.n_locals {
                    return Err(ShenError::new("vm: bad local slot"));
                }
                stack.push(stack[base + s].clone());
            }
            Op::StoreLocal(slot) => {
                let s = slot as usize;
                if s >= cur_bf.n_locals {
                    return Err(ShenError::new("vm: bad local slot on StoreLocal"));
                }
                if stack.len() <= floor {
                    return Err(ShenError::new("vm: stack underflow on StoreLocal"));
                }
                let v = stack.pop().unwrap();
                stack[base + s] = v;
            }
            Op::Pop => {
                if stack.len() <= floor {
                    return Err(ShenError::new("vm: stack underflow on Pop"));
                }
                stack.pop();
            }
            Op::LoadUpval(idx) => {
                let v = cur_upvals
                    .get(idx as usize)
                    .cloned()
                    .ok_or_else(|| ShenError::new("vm: bad upval index"))?;
                stack.push(v);
            }
            Op::LoadGlobal(idx) => {
                let v = cur_bf
                    .consts
                    .get(idx as usize)
                    .cloned()
                    .ok_or_else(|| ShenError::new("vm: bad const index for LoadGlobal"))?;
                let sym = match v.as_sym() {
                    Some(s) => s,
                    None => {
                        return Err(ShenError::new(format!(
                            "vm: LoadGlobal const must be a Sym, got {v:?}"
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
                if stack.len() <= floor {
                    return Err(ShenError::new("vm: stack underflow on JumpFalse"));
                }
                let v = stack.pop().unwrap();
                let truthy = if let Some(b) = v.as_bool() {
                    b
                } else {
                    match v.as_sym() {
                        Some(s) if s == interp.well_known.k_true => true,
                        Some(s) if s == interp.well_known.k_false => false,
                        _ => {
                            return Err(ShenError::new(format!(
                                "vm: JumpFalse on non-boolean: {v:?}"
                            )))
                        }
                    }
                };
                if !truthy {
                    pc = jump_target(pc, delta)?;
                }
            }
            Op::MakeClosure { fn_idx, n_upvals } => {
                let n = n_upvals as usize;
                if stack.len() < floor + n {
                    return Err(ShenError::new("vm: stack underflow on MakeClosure"));
                }
                let captured: Vec<Value> = stack.drain(stack.len() - n..).collect();
                let inner = cur_bf
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
                stack.push(Value::closure(closure));
            }
            Op::Return => {
                let retval = if stack.len() > floor {
                    stack.pop().unwrap()
                } else {
                    Value::nil()
                };
                if ret_dst == usize::MAX {
                    return Ok(retval);
                }
                stack.truncate(ret_dst);
                stack.push(retval);
                let caller = frames.pop().expect("vm: frame underflow on Return");
                cur_bf = caller.bf;
                base = caller.base;
                pc = caller.pc;
                cur_upvals = caller.upvals;
                ret_dst = caller.ret_dst;
                floor = base + cur_bf.n_locals;
            }
            Op::Call(n) => {
                let n = n as usize;
                let l = stack.len();
                if l < n + 1 {
                    return Err(ShenError::new("vm: stack underflow on Call"));
                }
                let callee_idx = l - n - 1;
                let callee = stack[callee_idx];
                if let Some(c) = callee.as_closure() {
                    if c.partial.is_empty() && c.arity == n {
                        match &c.kind {
                            ClosureKind::Native(nf, _captures) => {
                                // Leaf call: dispatch on the borrowed arg
                                // slice with no allocation, then collapse
                                // callee+args to the single result slot.
                                let r = nf(interp, &stack[l - n..l])?;
                                stack.truncate(callee_idx);
                                stack.push(r);
                                continue;
                            }
                            ClosureKind::Bytecode(new_bf, new_up) => {
                                // Suspend the caller and continue in the
                                // callee's frame — no Rust recursion. Args
                                // are already contiguous at [l-n .. l] and
                                // become the new frame's locals[0..n).
                                let new_base = l - n;
                                let new_ret = callee_idx; // callee slot
                                let new_bf = Rc::clone(new_bf);
                                let new_up = new_up.clone();
                                let caller = Frame {
                                    bf: std::mem::replace(&mut cur_bf, new_bf),
                                    base,
                                    pc,
                                    upvals: std::mem::take(&mut cur_upvals),
                                    ret_dst,
                                };
                                frames.push(caller);
                                cur_upvals = new_up;
                                base = new_base;
                                pc = 0;
                                ret_dst = new_ret;
                                let nl = cur_bf.n_locals;
                                stack.resize(base + nl, Value::nil());
                                floor = base + nl;
                                continue;
                            }
                            ClosureKind::Lambda(_) => { /* fall through to apply */ }
                        }
                    }
                }
                // Fallback: partial / over-application / arity mismatch /
                // tree-walked lambda. Materialise the args and re-dispatch.
                let argv: Vec<Value> = stack.drain(l - n..).collect();
                stack.pop(); // remove callee
                let r = interp.apply(callee, argv)?;
                stack.push(r);
            }
            Op::TailCall(n) => {
                let n = n as usize;
                let l = stack.len();
                if l < n + 1 {
                    return Err(ShenError::new("vm: stack underflow on TailCall"));
                }
                let callee_idx = l - n - 1;
                let callee = stack[callee_idx];
                if let Some(c) = callee.as_closure() {
                    if c.partial.is_empty() && c.arity == n {
                        if let ClosureKind::Bytecode(new_bf, new_up) = &c.kind {
                            // Replace the current frame in place: move the
                            // args down over the current frame's locals,
                            // then re-enter at pc=0. base and ret_dst are
                            // preserved → no frame growth (true TCO).
                            for i in 0..n {
                                let v = std::mem::replace(&mut stack[l - n + i], Value::nil());
                                stack[base + i] = v;
                            }
                            stack.truncate(base + n);
                            cur_bf = Rc::clone(new_bf);
                            cur_upvals = new_up.clone();
                            let nl = cur_bf.n_locals;
                            stack.resize(base + nl, Value::nil());
                            floor = base + nl;
                            pc = 0;
                            continue;
                        }
                    }
                }
                // Fallback: apply the non-bytecode callee, then return its
                // result as this frame's result (still a tail position, so
                // we do not grow the frame stack).
                let argv: Vec<Value> = stack.drain(l - n..).collect();
                stack.pop(); // remove callee
                let r = interp.apply(callee, argv)?;
                if ret_dst == usize::MAX {
                    return Ok(r);
                }
                stack.truncate(ret_dst);
                stack.push(r);
                let caller = frames
                    .pop()
                    .expect("vm: frame underflow on TailCall return");
                cur_bf = caller.bf;
                base = caller.base;
                pc = caller.pc;
                cur_upvals = caller.upvals;
                ret_dst = caller.ret_dst;
                floor = base + cur_bf.n_locals;
            }
            Op::SelfTailCall(n) => {
                // In-place self-recursion: copy the top n operands into
                // locals[0..n) and reset pc=0. No frame work at all.
                let n = n as usize;
                let l = stack.len();
                if l < floor + n {
                    return Err(ShenError::new("vm: stack underflow on SelfTailCall"));
                }
                if n > cur_bf.n_locals {
                    return Err(ShenError::new("vm: SelfTailCall n > n_locals"));
                }
                for i in 0..n {
                    let v = std::mem::replace(&mut stack[l - n + i], Value::nil());
                    stack[base + i] = v;
                }
                stack.truncate(floor);
                pc = 0;
            }
            // ---- Inlined primitives (B4b) ----------------------------
            // Each mirrors the `aot::runtime::*` helper of the same name;
            // the helpers are the single source of truth for semantics.
            Op::Add => binop_fallible(&mut stack, floor, crate::aot::runtime::add)?,
            Op::Sub => binop_fallible(&mut stack, floor, crate::aot::runtime::sub)?,
            Op::Mul => binop_fallible(&mut stack, floor, crate::aot::runtime::mul)?,
            Op::Div => binop_fallible(&mut stack, floor, crate::aot::runtime::div)?,
            Op::Lt => binop_fallible(&mut stack, floor, crate::aot::runtime::lt)?,
            Op::Le => binop_fallible(&mut stack, floor, crate::aot::runtime::lte)?,
            Op::Gt => binop_fallible(&mut stack, floor, crate::aot::runtime::gt)?,
            Op::Ge => binop_fallible(&mut stack, floor, crate::aot::runtime::gte)?,
            Op::Eq => binop_infallible(&mut stack, floor, crate::aot::runtime::eq)?,
            Op::Cons => binop_infallible(&mut stack, floor, crate::aot::runtime::cons)?,
            Op::Hd => unop_fallible(&mut stack, floor, crate::aot::runtime::hd)?,
            Op::Tl => unop_fallible(&mut stack, floor, crate::aot::runtime::tl)?,
            Op::IsCons => unop_infallible(&mut stack, floor, crate::aot::runtime::is_cons)?,
            Op::IsNumber => unop_infallible(&mut stack, floor, crate::aot::runtime::is_number)?,
            Op::IsString => unop_infallible(&mut stack, floor, crate::aot::runtime::is_string)?,
            Op::IsSymbol => unop_infallible(&mut stack, floor, crate::aot::runtime::is_symbol)?,
            Op::IsAbsvector => {
                unop_infallible(&mut stack, floor, crate::aot::runtime::is_absvector)?
            }
        }
    }
}

#[inline]
fn binop_fallible(
    stack: &mut Stack,
    floor: usize,
    f: fn(&Value, &Value) -> ShenResult<Value>,
) -> ShenResult<()> {
    if stack.len() < floor + 2 {
        return Err(ShenError::new("vm: stack underflow on binop"));
    }
    let b = stack.pop().unwrap();
    let a = stack.pop().unwrap();
    let v = f(&a, &b)?;
    stack.push(v);
    Ok(())
}

#[inline]
fn binop_infallible(
    stack: &mut Stack,
    floor: usize,
    f: fn(&Value, &Value) -> Value,
) -> ShenResult<()> {
    if stack.len() < floor + 2 {
        return Err(ShenError::new("vm: stack underflow on binop"));
    }
    let b = stack.pop().unwrap();
    let a = stack.pop().unwrap();
    let v = f(&a, &b);
    stack.push(v);
    Ok(())
}

#[inline]
fn unop_fallible(
    stack: &mut Stack,
    floor: usize,
    f: fn(&Value) -> ShenResult<Value>,
) -> ShenResult<()> {
    if stack.len() < floor + 1 {
        return Err(ShenError::new("vm: stack underflow on unop"));
    }
    let a = stack.pop().unwrap();
    let v = f(&a)?;
    stack.push(v);
    Ok(())
}

#[inline]
fn unop_infallible(stack: &mut Stack, floor: usize, f: fn(&Value) -> Value) -> ShenResult<()> {
    if stack.len() < floor + 1 {
        return Err(ShenError::new("vm: stack underflow on unop"));
    }
    let a = stack.pop().unwrap();
    let v = f(&a);
    stack.push(v);
    Ok(())
}

/// Compute the new `pc` after a Jump/JumpFalse. `pc` here is already past
/// the jump instruction (we increment before dispatch), so the absolute
/// target is `pc + delta`. Errors only on a negative target — past-end
/// targets are caught by the next iteration's bounds check.
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
    /// no captured upvals.
    fn bytecode_closure(bf: BytecodeFn, name: Option<crate::symbol::SymId>) -> Value {
        let arity = bf.arity;
        Value::closure(Closure {
            name,
            arity,
            partial: Vec::new(),
            kind: ClosureKind::Bytecode(Rc::new(bf), Vec::new()),
        })
    }

    #[test]
    fn dispatch_via_interp_apply() {
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
        let r = interp.apply(f, vec![Value::int(7)]).expect("apply");
        assert!(r.as_int() == Some(7));
    }

    #[test]
    fn dispatch_via_apply_named_fast_path() {
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
        let r = crate::aot::runtime::apply_named(&mut interp, "id-vm", &[Value::int(99)])
            .expect("apply_named");
        assert!(r.as_int() == Some(99));
    }

    #[test]
    fn identity_function() {
        let mut interp = fresh_interp();
        let id_sym = interp.intern("id");
        let bf = Rc::new(BytecodeFn {
            name: Some(id_sym),
            arity: 1,
            n_locals: 1,
            code: vec![Op::LoadLocal(0), Op::Return],
            consts: vec![],
            fn_consts: vec![],
        });
        let result = exec(&mut interp, &bf, &[], &[Value::int(42)]).expect("exec");
        assert!(result.as_int() == Some(42));
    }

    #[test]
    fn constant_function() {
        let mut interp = fresh_interp();
        let bf = Rc::new(BytecodeFn {
            name: Some(interp.intern("answer")),
            arity: 0,
            n_locals: 0,
            code: vec![Op::LoadConst(0), Op::Return],
            consts: vec![Value::int(42)],
            fn_consts: vec![],
        });
        let result = exec(&mut interp, &bf, &[], &[]).expect("exec");
        assert!(result.as_int() == Some(42));
    }

    #[test]
    fn let_via_store_local() {
        let mut interp = fresh_interp();
        let bf = Rc::new(BytecodeFn {
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
        });
        let result = exec(&mut interp, &bf, &[], &[Value::int(7)]).expect("exec");
        assert!(result.as_int() == Some(7));
    }

    #[test]
    fn call_via_existing_primitive() {
        // (defun plus-one (X) (+ X 1)) using the registered `+` closure
        // via Call (not the inlined Add opcode).
        let mut interp = fresh_interp();
        let plus_sym = interp.intern("+");
        let plus = interp
            .env
            .get_fn(plus_sym)
            .cloned()
            .expect("+ should be registered by Interp::new()");
        let bf = Rc::new(BytecodeFn {
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
            consts: vec![plus, Value::int(1)],
            fn_consts: vec![],
        });
        let result = exec(&mut interp, &bf, &[], &[Value::int(41)]).expect("exec");
        assert!(result.as_int() == Some(42));
    }

    #[test]
    fn make_closure_zero_upvals() {
        let mut interp = fresh_interp();
        let inner = Rc::new(BytecodeFn {
            name: None,
            arity: 1,
            n_locals: 1,
            code: vec![Op::LoadLocal(0), Op::Return],
            consts: vec![],
            fn_consts: vec![],
        });
        let outer = Rc::new(BytecodeFn {
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
            consts: vec![Value::int(42)],
            fn_consts: vec![inner],
        });
        let result = exec(&mut interp, &outer, &[], &[]).expect("exec");
        assert!(result.as_int() == Some(42));
    }

    #[test]
    fn make_closure_with_upval() {
        let mut interp = fresh_interp();
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
            consts: vec![Value::sym(interp.intern("+"))],
            fn_consts: vec![],
        });
        let outer = Rc::new(BytecodeFn {
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
            consts: vec![Value::int(10), Value::int(5)],
            fn_consts: vec![inner],
        });
        let result = exec(&mut interp, &outer, &[], &[]).expect("exec");
        assert!(result.as_int() == Some(15), "got {result:?}");
    }

    #[test]
    fn arity_mismatch_is_an_error() {
        let mut interp = fresh_interp();
        let bf = Rc::new(BytecodeFn {
            name: Some(interp.intern("id")),
            arity: 1,
            n_locals: 1,
            code: vec![Op::LoadLocal(0), Op::Return],
            consts: vec![],
            fn_consts: vec![],
        });
        assert!(exec(&mut interp, &bf, &[], &[]).is_err());
    }

    #[test]
    fn nested_bytecode_call_runs_in_one_exec() {
        // (defun add1 (x) (+ x 1)) ; (defun caller (x) (add1 (add1 x)))
        // caller's two calls to add1 must run as in-VM frames (Call to a
        // bytecode callee), not via interp.apply recursion.
        let mut interp = fresh_interp();
        let add1_sym = interp.intern("add1");
        let add1 = Rc::new(BytecodeFn {
            name: Some(add1_sym),
            arity: 1,
            n_locals: 1,
            code: vec![Op::LoadLocal(0), Op::LoadConst(0), Op::Add, Op::Return],
            consts: vec![Value::int(1)],
            fn_consts: vec![],
        });
        interp.env.set_fn(
            add1_sym,
            Value::closure(Closure {
                name: Some(add1_sym),
                arity: 1,
                partial: Vec::new(),
                kind: ClosureKind::Bytecode(Rc::clone(&add1), Vec::new()),
            }),
        );
        // caller: LoadGlobal add1, (LoadGlobal add1, LoadLocal0, Call1), Call1
        let caller = Rc::new(BytecodeFn {
            name: Some(interp.intern("caller")),
            arity: 1,
            n_locals: 1,
            code: vec![
                Op::LoadGlobal(0),
                Op::LoadGlobal(0),
                Op::LoadLocal(0),
                Op::Call(1),
                Op::Call(1),
                Op::Return,
            ],
            consts: vec![Value::sym(add1_sym)],
            fn_consts: vec![],
        });
        let r = exec(&mut interp, &caller, &[], &[Value::int(40)]).expect("exec");
        assert!(r.as_int() == Some(42), "got {r:?}");
    }

    #[test]
    fn tail_call_does_not_grow_frames() {
        // (defun countdown (n) (if (= n 0) 99 (countdown2 (- n 1))))
        // (defun countdown2 (n) (if (= n 0) 99 (countdown (- n 1))))
        // Mutual cross-function tail recursion via Op::TailCall must run in
        // constant frame space (no Rust-stack growth, no frames blowup).
        let mut interp = fresh_interp();
        let cd_sym = interp.intern("countdown");
        let cd2_sym = interp.intern("countdown2");

        // Each body: if (= n 0) 99 (OTHER (- n 1))  — OTHER in tail position.
        let cd = Rc::new(BytecodeFn {
            name: Some(cd_sym),
            arity: 1,
            n_locals: 1,
            code: vec![
                Op::LoadLocal(0),
                Op::LoadConst(0), // 0
                Op::Eq,
                Op::JumpFalse(2),
                Op::LoadConst(1), // 99
                Op::Return,
                Op::LoadGlobal(3), // countdown2
                Op::LoadLocal(0),
                Op::LoadConst(2), // 1
                Op::Sub,
                Op::TailCall(1),
            ],
            consts: vec![
                Value::int(0),
                Value::int(99),
                Value::int(1),
                Value::sym(cd2_sym),
            ],
            fn_consts: vec![],
        });
        let cd2 = Rc::new(BytecodeFn {
            name: Some(cd2_sym),
            arity: 1,
            n_locals: 1,
            code: vec![
                Op::LoadLocal(0),
                Op::LoadConst(0),
                Op::Eq,
                Op::JumpFalse(2),
                Op::LoadConst(1),
                Op::Return,
                Op::LoadGlobal(3), // countdown
                Op::LoadLocal(0),
                Op::LoadConst(2),
                Op::Sub,
                Op::TailCall(1),
            ],
            consts: vec![
                Value::int(0),
                Value::int(99),
                Value::int(1),
                Value::sym(cd_sym),
            ],
            fn_consts: vec![],
        });
        interp.env.set_fn(
            cd_sym,
            Value::closure(Closure {
                name: Some(cd_sym),
                arity: 1,
                partial: Vec::new(),
                kind: ClosureKind::Bytecode(Rc::clone(&cd), Vec::new()),
            }),
        );
        interp.env.set_fn(
            cd2_sym,
            Value::closure(Closure {
                name: Some(cd2_sym),
                arity: 1,
                partial: Vec::new(),
                kind: ClosureKind::Bytecode(Rc::clone(&cd2), Vec::new()),
            }),
        );
        // 2,000,000 mutual tail calls — would overflow an 8MB Rust stack if
        // each call recursed. Must complete in constant space.
        let r = exec(&mut interp, &cd, &[], &[Value::int(2_000_000)]).expect("exec");
        assert!(r.as_int() == Some(99), "got {r:?}");
    }
}
