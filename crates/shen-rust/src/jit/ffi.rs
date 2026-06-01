//! `extern "C"` runtime helpers the JIT'd code FFIs into — stage J2.
//!
//! Each helper is a thin, stable-ABI wrapper over the exact `crate::aot::runtime`
//! (`rt::`) semantics, so JIT'd native code matches the AOT/tree-walked
//! interpreter byte-for-byte (the differential oracle gates this). Words in,
//! words out (`Value` is `#[repr(transparent)]` over `u64`).
//!
//! **Error ABI** (`design/jit-productionization-plan.md` §"Error ABI"): fallible
//! helpers take `errflag: *mut u64`. On a Shen error they record the rich error
//! on `Interp::pending_error` (first-wins), set `*errflag = 1`, and return a
//! `nil` sentinel. The JIT body `load`s `errflag` after the call and branches to
//! its error-return block; `call_jit` then surfaces the pending error as `Err`.
//!
//! `#[inline(never)]` keeps a stable call target for symbol registration; the
//! helpers are registered with the `JITModule` by name in `engine`/`mod`.

use crate::aot::runtime as rt;
use crate::error::ShenError;
use crate::interp::eval::Interp;
use crate::value::{ClosureKind, Value};

/// Tally a call from JIT'd code by whether its callee is itself a fully-applied
/// JIT closure (the only edge `return_call_indirect`/Slice C could optimise).
#[inline]
fn tally_callee(f: &Value) {
    let to_jit = f
        .as_closure()
        .map(|c| c.partial.is_empty() && matches!(c.kind, ClosureKind::Jit(_, _)))
        .unwrap_or(false);
    use std::sync::atomic::Ordering::Relaxed;
    if to_jit {
        crate::jit::JIT_CALL_TO_JIT.fetch_add(1, Relaxed);
    } else {
        crate::jit::JIT_CALL_TO_OTHER.fetch_add(1, Relaxed);
    }
}

/// Record a Shen error on the interpreter and raise the JIT error flag.
///
/// # Safety
/// `interp` and `errflag` are the live pointers `call_jit` passed into the JIT
/// entry (or that an enclosing helper forwarded); they are valid for this call
/// and not aliased by any `Value` held across it.
#[inline]
unsafe fn raise(interp: *mut Interp, errflag: *mut u64, e: crate::error::ShenError) -> u64 {
    (*interp).set_pending_error(e);
    *errflag = 1;
    Value::nil().to_word()
}

/// A fallible binary arithmetic/comparison helper over `rt::$f`.
macro_rules! fallible_binary {
    ($name:ident, $f:path) => {
        #[inline(never)]
        pub(crate) extern "C" fn $name(
            interp: *mut Interp,
            errflag: *mut u64,
            a: u64,
            b: u64,
        ) -> u64 {
            match $f(&Value::from_word(a), &Value::from_word(b)) {
                Ok(r) => r.to_word(),
                // SAFETY: see `raise`.
                Err(e) => unsafe { raise(interp, errflag, e) },
            }
        }
    };
}

/// An infallible binary helper over `rt::$f` (returns `Value`, never errors).
macro_rules! infallible_binary {
    ($name:ident, $f:path) => {
        #[inline(never)]
        pub(crate) extern "C" fn $name(a: u64, b: u64) -> u64 {
            $f(&Value::from_word(a), &Value::from_word(b)).to_word()
        }
    };
}

/// A fallible unary helper over `rt::$f`.
macro_rules! fallible_unary {
    ($name:ident, $f:path) => {
        #[inline(never)]
        pub(crate) extern "C" fn $name(interp: *mut Interp, errflag: *mut u64, v: u64) -> u64 {
            match $f(&Value::from_word(v)) {
                Ok(r) => r.to_word(),
                // SAFETY: see `raise`.
                Err(e) => unsafe { raise(interp, errflag, e) },
            }
        }
    };
}

/// An infallible unary predicate over `rt::$f` (returns a `Value::bool`).
macro_rules! infallible_unary {
    ($name:ident, $f:path) => {
        #[inline(never)]
        pub(crate) extern "C" fn $name(v: u64) -> u64 {
            $f(&Value::from_word(v)).to_word()
        }
    };
}

fallible_binary!(rtj_add, rt::add);
fallible_binary!(rtj_sub, rt::sub);
fallible_binary!(rtj_mul, rt::mul);
fallible_binary!(rtj_div, rt::div);
fallible_binary!(rtj_lt, rt::lt);
fallible_binary!(rtj_gt, rt::gt);
fallible_binary!(rtj_lte, rt::lte);
fallible_binary!(rtj_gte, rt::gte);

infallible_binary!(rtj_eq, rt::eq);
infallible_binary!(rtj_cons, rt::cons);

fallible_unary!(rtj_hd, rt::hd);
fallible_unary!(rtj_tl, rt::tl);

infallible_unary!(rtj_is_cons, rt::is_cons);
infallible_unary!(rtj_is_number, rt::is_number);
infallible_unary!(rtj_is_string, rt::is_string);
infallible_unary!(rtj_is_symbol, rt::is_symbol);
infallible_unary!(rtj_is_absvector, rt::is_absvector);

/// `is_truthy` for `if`/`cond`/`and`/`or` conditions. Returns `1`/`0`; on a
/// non-boolean it raises the error flag and returns `0`.
#[inline(never)]
pub(crate) extern "C" fn rtj_is_truthy(interp: *mut Interp, errflag: *mut u64, v: u64) -> u64 {
    // SAFETY: `interp` is the live pointer from the JIT entry.
    let i = unsafe { &mut *interp };
    match rt::is_truthy(i, &Value::from_word(v)) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            // SAFETY: see `raise`. We discard the nil sentinel and return 0;
            // the JIT branches on the flag, not on this value.
            unsafe { raise(interp, errflag, e) };
            0
        }
    }
}

/// Reinterpret a JIT arg/capture word array as a borrowed `Value` slice.
///
/// # Safety
/// `ptr` points at `n` contiguous `Value` words (the JIT stack-slot array or a
/// `ClosureKind::Jit` capture vec); `Value` is `#[repr(transparent)]` over `u64`.
#[inline]
unsafe fn arg_slice<'a>(ptr: *const u64, n: usize) -> &'a [Value] {
    std::slice::from_raw_parts(ptr as *const Value, n)
}

/// `(f a0..a_{n-1})` where the head is a free symbol naming a global function —
/// the tree-walker's named-call path (`eval.rs` `step`): resolve through the
/// function namespace, then apply. The JIT only emits this for symbols it
/// statically knows are free (not a param/upval/let), so the tree-walker's
/// `lookup_local`-first probe is a guaranteed miss and `get_fn` is exact.
#[inline(never)]
pub(crate) extern "C" fn rtj_apply_named(
    interp: *mut Interp,
    errflag: *mut u64,
    sym_word: u64,
    args_ptr: *const u64,
    n: usize,
) -> u64 {
    let Some(s) = Value::from_word(sym_word).as_sym() else {
        // SAFETY: see `raise`.
        return unsafe {
            raise(
                interp,
                errflag,
                ShenError::new("jit: apply_named on non-symbol"),
            )
        };
    };
    // SAFETY: live interp ptr from the JIT entry; the borrow ends at `.cloned()`.
    let f = unsafe { (*interp).env.get_fn(s).cloned() };
    let Some(f) = f else {
        // SAFETY: live interp ptr; the `&str` borrow ends inside `format!`.
        let msg = format!("undefined function: {}", unsafe { (*interp).resolve(s) });
        // SAFETY: see `raise`.
        return unsafe { raise(interp, errflag, ShenError::new(msg)) };
    };
    tally_callee(&f);
    // SAFETY: `args_ptr`/`n` describe the JIT's arg array; live interp ptr.
    let args = unsafe { arg_slice(args_ptr, n) };
    match rt::apply_value(unsafe { &mut *interp }, f, args) {
        Ok(v) => v.to_word(),
        // SAFETY: see `raise`.
        Err(e) => unsafe { raise(interp, errflag, e) },
    }
}

/// Apply a computed/closure value (head resolved to a local, upval, or
/// expression) to `n` args. Mirrors the tree-walker's `tail_apply` /
/// `call_or_apply` dispatch via `rt::apply_value`.
#[inline(never)]
pub(crate) extern "C" fn rtj_apply_value(
    interp: *mut Interp,
    errflag: *mut u64,
    fval: u64,
    args_ptr: *const u64,
    n: usize,
) -> u64 {
    let f = Value::from_word(fval);
    tally_callee(&f);
    // SAFETY: `args_ptr`/`n` describe the JIT's arg array; live interp ptr.
    let args = unsafe { arg_slice(args_ptr, n) };
    match rt::apply_value(unsafe { &mut *interp }, f, args) {
        Ok(v) => v.to_word(),
        // SAFETY: see `raise`.
        Err(e) => unsafe { raise(interp, errflag, e) },
    }
}

/// The J2 runtime-helper registry: `(symbol name, code address, param count)`.
/// One source of truth for both `JITBuilder::symbol` registration and the
/// `Linkage::Import` declaration in `codegen`. Every signature is `n × i64 → i64`
/// (words), so the param count fully determines the Cranelift signature. Names
/// are distinct from J1's `rtj1_*` so the shared module's symbol table can't
/// collide.
pub(crate) fn symbol_registry() -> Vec<(&'static str, *const u8, usize)> {
    vec![
        ("rtj_add", rtj_add as *const u8, 4),
        ("rtj_sub", rtj_sub as *const u8, 4),
        ("rtj_mul", rtj_mul as *const u8, 4),
        ("rtj_div", rtj_div as *const u8, 4),
        ("rtj_lt", rtj_lt as *const u8, 4),
        ("rtj_gt", rtj_gt as *const u8, 4),
        ("rtj_lte", rtj_lte as *const u8, 4),
        ("rtj_gte", rtj_gte as *const u8, 4),
        ("rtj_eq", rtj_eq as *const u8, 2),
        ("rtj_cons", rtj_cons as *const u8, 2),
        ("rtj_hd", rtj_hd as *const u8, 3),
        ("rtj_tl", rtj_tl as *const u8, 3),
        ("rtj_is_cons", rtj_is_cons as *const u8, 1),
        ("rtj_is_number", rtj_is_number as *const u8, 1),
        ("rtj_is_string", rtj_is_string as *const u8, 1),
        ("rtj_is_symbol", rtj_is_symbol as *const u8, 1),
        ("rtj_is_absvector", rtj_is_absvector as *const u8, 1),
        ("rtj_is_truthy", rtj_is_truthy as *const u8, 3),
        ("rtj_apply_named", rtj_apply_named as *const u8, 5),
        ("rtj_apply_value", rtj_apply_value as *const u8, 5),
    ]
}
