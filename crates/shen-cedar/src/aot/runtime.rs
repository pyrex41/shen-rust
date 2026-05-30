//! Runtime support for AOT-compiled KL functions.
//!
//! Calls go through `apply_direct` (fast path via raw fn pointer table for
//! AOT + hot natives) or fall back to `apply_named`. This gives direct
//! AOT-to-AOT with no per-module knowledge required in the generator.
//! centralises Shen's boolean semantics (`Bool(b)` and the symbols
//! `true`/`false`). `make_aot_closure` packages a Rust closure as a
//! Shen `Value::Closure`.

use std::rc::Rc;

use crate::error::{ShenError, ShenResult};
use crate::interp::eval::Interp;
use crate::value::{shen_eq, Closure, ClosureKind, NativeFn, Value};

/// Look up `name` in the function namespace and apply it to `args`.
/// This is the call site for everything AOT code does — primitives,
/// kernel functions, peer AOT functions. `args` is a borrowed slice
/// (emitted as a stack array `&[a, b]`) so the overwhelmingly common
/// full-arity native call allocates nothing.
pub fn apply_named(interp: &mut Interp, name: &'static str, args: &[Value]) -> ShenResult<Value> {
    let sym = interp.intern_static(name);
    let f = interp.env.get_fn(sym).cloned().ok_or_else(|| {
        ShenError::new(format!("aot: undefined function `{}`", interp.resolve(sym)))
    })?;
    call_or_apply(interp, f, args)
}

/// Fast path for AOT-to-AOT and AOT-to-hot-native calls.
///
/// Looks up a raw fn pointer in the direct table (populated by
/// `register_aot_direct` during AOT install). On hit: single indirect
/// call with zero Rc traffic or Closure construction. On miss (user
/// functions, tree-walked lambdas, partials, etc.): falls back to the
/// normal apply_named path.
pub fn apply_direct(interp: &mut Interp, name: &'static str, args: &[Value]) -> ShenResult<Value> {
    let sym = interp.intern_static(name);
    if let Some(f) = interp.get_aot_direct(sym) {
        f(interp, args)
    } else {
        apply_named(interp, name, args)
    }
}

/// Apply an already-resolved `Value::Closure` to `args`.
pub fn apply_value(interp: &mut Interp, f: Value, args: &[Value]) -> ShenResult<Value> {
    call_or_apply(interp, f, args)
}

/// Dispatch `f` on `args`. Fast path: a fully-applied native function
/// (no partial args, exact arity) is invoked directly on the borrowed
/// slice with zero allocation — this is the hot AOT-to-AOT call. Anything
/// else (partial application, arity mismatch, tree-walked lambda) falls
/// back to the general `Interp::apply`, materialising the args only then.
#[inline]
fn call_or_apply(interp: &mut Interp, f: Value, args: &[Value]) -> ShenResult<Value> {
    // Fast path: full-arity, no partial — dispatch in-place on the borrowed
    // closure. `f` is an owned local, disjoint from `interp`, so we can call
    // through `&c.kind` while passing `&mut interp` with no clone: this is the
    // single hottest path in the type-checker's continuation-passing proof
    // search, and a per-call `Rc::clone`/`Vec::clone` here is pure refcount/
    // alloc traffic that SBCL's native `funcall` does not pay. The slow path
    // (partial, arity mismatch, tree-walked lambda) still materialises args.
    if let Some(c) = f.as_closure() {
        if c.partial.is_empty() && args.len() == c.arity {
            match &c.kind {
                ClosureKind::Native(nf, _captures) => return nf(interp, args),
                ClosureKind::Bytecode(bf, upvals) => {
                    return crate::vm::exec::exec(interp, bf, upvals, args)
                }
                ClosureKind::Lambda(_) => {}
            }
        }
    }
    interp.apply(f, args.to_vec()) // boundary: public apply still takes Vec
}

/// Match Shen's boolean semantics: `Bool(true)`, `Sym(true)` → true;
/// `Bool(false)`, `Sym(false)` → false; anything else → error.
pub fn is_truthy(interp: &Interp, v: &Value) -> ShenResult<bool> {
    if let Some(b) = v.as_bool() {
        return Ok(b);
    }
    match v.as_sym() {
        Some(s) if s == interp.well_known.k_true => Ok(true),
        Some(s) if s == interp.well_known.k_false => Ok(false),
        _ => Err(ShenError::new(format!("aot: not a boolean: {v:?}"))),
    }
}

/// Wrap a Rust closure as a Shen `Value::Closure`. Used for AOT-compiled
/// lambdas. `arity` is the formal-parameter count; partial application
/// works via the same path as any other closure.
pub fn make_aot_closure<F>(name: &str, arity: usize, f: F, interp: &mut Interp) -> Value
where
    F: Fn(&mut Interp, &[Value]) -> ShenResult<Value> + 'static,
{
    let sym = interp.intern(name);
    let closure = Closure {
        name: Some(sym),
        arity,
        partial: Vec::new(),
        // Empty shadow captures for now: GC Step 3 runs collection OFF, so the
        // captures need not be traceable yet. Sub-step 4f wires klcompile to
        // emit the real capture vec here and regenerates AOT. See §5.
        kind: ClosureKind::Native(Rc::new(f) as Rc<NativeFn>, Vec::new()),
    };
    Value::closure(closure)
}

/// Look up a global. Used for `(value GLOBAL)` form.
pub fn global_value(interp: &mut Interp, name: &str) -> ShenResult<Value> {
    let sym = interp.intern(name);
    interp
        .env
        .get_global(sym)
        .cloned()
        .ok_or_else(|| ShenError::new(format!("aot: unbound global `{name}`")))
}

/// Look up a function as a value (`(fn NAME)`).
pub fn fn_value(interp: &mut Interp, name: &str) -> ShenResult<Value> {
    let sym = interp.intern(name);
    interp
        .env
        .get_fn(sym)
        .cloned()
        .ok_or_else(|| ShenError::new(format!("aot: undefined function `{name}`")))
}

// --- Inline helpers for hot primitives ---
//
// klcompile emits direct calls to these for a fixed set of well-known
// names (`+`, `-`, `<`, `=`, `cons`, `hd`, predicates, …) instead of
// routing through `apply_named`. `#[inline(always)]` lets release-mode LLVM
// collapse the helper call so the AOT output for `(+ X 1)` becomes a
// direct `Value::Int` match. Semantics mirror `primitives.rs` exactly;
// keep the two in sync.

#[inline(always)]
pub fn add(a: &Value, b: &Value) -> ShenResult<Value> {
    if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
        return Ok(match x.checked_add(y) {
            Some(v) => Value::int(v),
            None => Value::float(x as f64 + y as f64),
        });
    }
    match (a.as_number_f64(), b.as_number_f64()) {
        (Some(x), Some(y)) => Ok(Value::float(x + y)),
        _ => Err(ShenError::new(format!("+: bad args: {a:?}, {b:?}"))),
    }
}

#[inline(always)]
pub fn sub(a: &Value, b: &Value) -> ShenResult<Value> {
    if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
        return Ok(match x.checked_sub(y) {
            Some(v) => Value::int(v),
            None => Value::float(x as f64 - y as f64),
        });
    }
    match (a.as_number_f64(), b.as_number_f64()) {
        (Some(x), Some(y)) => Ok(Value::float(x - y)),
        _ => Err(ShenError::new(format!("-: bad args: {a:?}, {b:?}"))),
    }
}

#[inline(always)]
pub fn mul(a: &Value, b: &Value) -> ShenResult<Value> {
    if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
        return Ok(match x.checked_mul(y) {
            Some(v) => Value::int(v),
            None => Value::float(x as f64 * y as f64),
        });
    }
    match (a.as_number_f64(), b.as_number_f64()) {
        (Some(x), Some(y)) => Ok(Value::float(x * y)),
        _ => Err(ShenError::new(format!("*: bad args: {a:?}, {b:?}"))),
    }
}

#[inline(always)]
pub fn div(a: &Value, b: &Value) -> ShenResult<Value> {
    let both_int = a.as_int().is_some() && b.as_int().is_some();
    let (Some(x), Some(y)) = (a.as_number_f64(), b.as_number_f64()) else {
        return Err(ShenError::new(format!("/: bad args: {a:?}, {b:?}")));
    };
    if y == 0.0 {
        return Err(ShenError::new("/: division by zero"));
    }
    let r = x / y;
    if both_int && r.fract() == 0.0 {
        Ok(Value::int(r as i64))
    } else {
        Ok(Value::float(r))
    }
}

#[inline(always)]
fn cmp_op(a: &Value, b: &Value, name: &str) -> ShenResult<std::cmp::Ordering> {
    if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
        return Ok(x.cmp(&y));
    }
    let (Some(x), Some(y)) = (a.as_number_f64(), b.as_number_f64()) else {
        return Err(ShenError::new(format!("{name}: bad args: {a:?}, {b:?}")));
    };
    x.partial_cmp(&y)
        .ok_or_else(|| ShenError::new(format!("{name}: NaN comparison")))
}

#[inline(always)]
pub fn lt(a: &Value, b: &Value) -> ShenResult<Value> {
    Ok(Value::bool(cmp_op(a, b, "<")? == std::cmp::Ordering::Less))
}

#[inline(always)]
pub fn gt(a: &Value, b: &Value) -> ShenResult<Value> {
    Ok(Value::bool(
        cmp_op(a, b, ">")? == std::cmp::Ordering::Greater,
    ))
}

#[inline(always)]
pub fn lte(a: &Value, b: &Value) -> ShenResult<Value> {
    Ok(Value::bool(
        cmp_op(a, b, "<=")? != std::cmp::Ordering::Greater,
    ))
}

#[inline(always)]
pub fn gte(a: &Value, b: &Value) -> ShenResult<Value> {
    Ok(Value::bool(cmp_op(a, b, ">=")? != std::cmp::Ordering::Less))
}

#[inline(always)]
pub fn eq(a: &Value, b: &Value) -> Value {
    Value::bool(shen_eq(a, b))
}

#[inline(always)]
pub fn cons(a: &Value, b: &Value) -> Value {
    Value::cons(*a, *b)
}

#[inline(always)]
pub fn hd(v: &Value) -> ShenResult<Value> {
    match v.head() {
        Some(h) => Ok(*h),
        None => Err(ShenError::new(format!("hd: not a cons: {v:?}"))),
    }
}

#[inline(always)]
pub fn tl(v: &Value) -> ShenResult<Value> {
    match v.tail() {
        Some(t) => Ok(*t),
        None => Err(ShenError::new(format!("tl: not a cons: {v:?}"))),
    }
}

#[inline(always)]
pub fn is_cons(v: &Value) -> Value {
    Value::bool(v.is_cons())
}

#[inline(always)]
pub fn is_number(v: &Value) -> Value {
    Value::bool(v.is_number())
}

#[inline(always)]
pub fn is_string(v: &Value) -> Value {
    Value::bool(v.is_str())
}

#[inline(always)]
pub fn is_symbol(v: &Value) -> Value {
    Value::bool(v.is_sym())
}

#[inline(always)]
pub fn is_absvector(v: &Value) -> Value {
    Value::bool(v.is_vec())
}
