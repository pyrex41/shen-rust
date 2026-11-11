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

// GC Step 4 contract for this module's `pub` entry points: with
// `SHEN_RUST_GC` enabled, a HOST must not call `apply_named`/`apply_direct`/
// `apply_value` at activation depth 0 — enter through `Interp::eval`/`apply`
// instead. These are the hottest call edges in the engine (every AOT call
// dispatches here), so unlike `vm::exec::exec` they take no runtime depth
// guard — measured +3% on kernel-tests, the exact tax the design review
// rejected. In-tree they are only ever reached beneath a guarded funnel (or
// from benches that don't enable the GC); the debug assert below makes any
// violation deterministic in debug builds and the `--debug-gc` gate leg.

/// Look up `name` in the function namespace and apply it to `args`.
/// This is the call site for everything AOT code does — primitives,
/// kernel functions, peer AOT functions. `args` is a borrowed slice
/// (emitted as a stack array `&[a, b]`) so the overwhelmingly common
/// full-arity native call allocates nothing.
pub fn apply_named(interp: &mut Interp, name: &'static str, args: &[Value]) -> ShenResult<Value> {
    crate::interp::eval::debug_assert_gc_safe_entry();
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
    crate::interp::eval::debug_assert_gc_safe_entry();
    let sym = interp.intern_static(name);
    if let Some(f) = interp.get_aot_direct(sym) {
        f(interp, args)
    } else {
        apply_named(interp, name, args)
    }
}

/// Apply an already-resolved `Value::Closure` to `args`.
pub fn apply_value(interp: &mut Interp, f: Value, args: &[Value]) -> ShenResult<Value> {
    crate::interp::eval::debug_assert_gc_safe_entry();
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
                    // Bytecode closures are produced by the tree-walker. A
                    // build without `interpreter` does not compile the VM,
                    // and AOT code never constructs these.
                    #[cfg(feature = "interpreter")]
                    {
                        return crate::vm::exec::exec(interp, bf, upvals, args);
                    }
                    #[cfg(not(feature = "interpreter"))]
                    {
                        let _ = (bf, upvals, args);
                        return Err(ShenError::new(
                            "bytecode VM is not included in this build (shen-rust feature `interpreter`)",
                        ));
                    }
                }
                #[cfg(feature = "jit")]
                ClosureKind::Jit(jc, captures) => {
                    return crate::jit::call_jit(interp, jc, captures, args)
                }
                ClosureKind::Lambda(_) => {}
            }
        }
    }
    interp.apply(f, args.to_vec()) // boundary: public apply still takes Vec
}

/// Match Shen's boolean semantics: `Bool(true)`, `Sym(true)` → true;
/// `Bool(false)`, `Sym(false)` → false; anything else → error.
#[inline]
pub fn is_truthy(interp: &Interp, v: &Value) -> ShenResult<bool> {
    // Hot path is a single tag decode; keep it tiny so this folds into the
    // generated AOT `if` site. The non-boolean diagnostic is cold and outlined
    // so its `format!` doesn't block inlining (it was the bulk of this leaf's
    // self-time on the kernel-tests profile).
    if let Some(b) = v.as_bool() {
        return Ok(b);
    }
    match v.as_sym() {
        Some(s) if s == interp.well_known.k_true => Ok(true),
        Some(s) if s == interp.well_known.k_false => Ok(false),
        _ => Err(not_a_boolean(v)),
    }
}

#[cold]
#[inline(never)]
fn not_a_boolean(v: &Value) -> ShenError {
    ShenError::new(format!("aot: not a boolean: {v:?}"))
}

/// Wrap a Rust closure as a Shen closure value. Used for AOT-compiled lambdas.
/// `arity` is the formal-parameter count; partial application works via the same
/// path as any other closure.
///
/// `captures` is the **traceable shadow capture list** (GC Step 3 §5): the
/// generated AOT body `move`-captures `Value`s into the opaque `dyn Fn`, where
/// the collector cannot see them. klcompile emits the *same* handles here so the
/// `Closure` node's [`crate::value::Closure`] `GcObject::gc_edges` keeps them
/// reachable across a collection (Step 4). Because `Value` is `Copy`, this list
/// and the move-captures hold copies of the same tagged words — the closure body
/// is unchanged. Empty when the lambda captures nothing.
pub fn make_aot_closure<F>(
    name: &'static str,
    arity: usize,
    f: F,
    captures: Vec<Value>,
    interp: &mut Interp,
) -> Value
where
    F: Fn(&mut Interp, &[Value]) -> ShenResult<Value> + 'static,
{
    // `name` is always an AOT-emitted literal (`"<lambda>"`/`"<freeze>"`);
    // route through the pointer-cached path so hot lambda allocation never
    // re-probes the intern HashMap.
    let sym = interp.intern_static(name);
    let closure = Closure {
        name: Some(sym),
        arity,
        partial: Vec::new(),
        kind: ClosureKind::Native(Rc::new(f) as Rc<NativeFn>, captures),
    };
    Value::closure(closure)
}

/// Look up a global. Used for `(value GLOBAL)` form.
pub fn global_value(interp: &mut Interp, name: &'static str) -> ShenResult<Value> {
    let sym = interp.intern_static(name);
    interp
        .env
        .get_global(sym)
        .cloned()
        .ok_or_else(|| ShenError::new(format!("aot: unbound global `{name}`")))
}

/// Look up a function as a value (`(fn NAME)`).
pub fn fn_value(interp: &mut Interp, name: &'static str) -> ShenResult<Value> {
    let sym = interp.intern_static(name);
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

#[inline(always)]
pub fn is_empty(v: &Value) -> Value {
    Value::bool(v.is_nil())
}

#[inline(always)]
pub fn not(interp: &Interp, v: &Value) -> ShenResult<Value> {
    Ok(Value::bool(!is_truthy(interp, v)?))
}

#[inline(always)]
pub fn is_boolean(interp: &Interp, v: &Value) -> Value {
    let b = v.as_bool().is_some()
        || matches!(v.as_sym(), Some(s) if s == interp.well_known.k_true || s == interp.well_known.k_false);
    Value::bool(b)
}

#[inline(always)]
pub fn fail(interp: &Interp) -> Value {
    Value::sym(interp.well_known.k_shen_fail)
}

#[inline(always)]
pub fn intern(interp: &mut Interp, s: &Value) -> ShenResult<Value> {
    match s.as_str() {
        Some(s) => Ok(interp.intern_kl(s)),
        None => Err(ShenError::new(format!("intern: not a string: {s:?}"))),
    }
}

#[inline(always)]
pub fn cn(a: &Value, b: &Value) -> ShenResult<Value> {
    match (a.as_str(), b.as_str()) {
        (Some(x), Some(y)) => {
            let mut s = String::with_capacity(x.len() + y.len());
            s.push_str(x);
            s.push_str(y);
            Ok(Value::str(s))
        }
        _ => Err(ShenError::new(format!(
            "cn: strings only, got {a:?} and {b:?}"
        ))),
    }
}

#[inline(always)]
pub fn hdstr(v: &Value) -> ShenResult<Value> {
    match v.as_str() {
        Some(s) => s
            .chars()
            .next()
            .map(|c| Value::str(c.to_string()))
            .ok_or_else(|| ShenError::new("hdstr: empty string")),
        None => Err(ShenError::new(format!("hdstr: not a string: {v:?}"))),
    }
}

#[inline(always)]
pub fn tlstr(v: &Value) -> ShenResult<Value> {
    match v.as_str() {
        Some(s) => {
            if s.is_empty() {
                return Err(ShenError::new("tlstr: empty string"));
            }
            let first_len = s.chars().next().expect("non-empty").len_utf8();
            Ok(Value::str(&s[first_len..]))
        }
        None => Err(ShenError::new(format!("tlstr: not a string: {v:?}"))),
    }
}

#[inline(always)]
pub fn string_to_n(v: &Value) -> ShenResult<Value> {
    match v.as_str() {
        Some(s) => s
            .chars()
            .next()
            .map(|c| Value::int(c as i64))
            .ok_or_else(|| ShenError::new("string->n: empty string")),
        None => Err(ShenError::new(format!("string->n: not a string: {v:?}"))),
    }
}

#[inline(always)]
pub fn n_to_string(v: &Value) -> ShenResult<Value> {
    match v.as_int() {
        Some(n) => {
            let c = char::from_u32(n as u32)
                .ok_or_else(|| ShenError::new(format!("n->string: bad codepoint {n}")))?;
            Ok(Value::str(String::from(c)))
        }
        None => Err(ShenError::new(format!("n->string: not an int: {v:?}"))),
    }
}

/// `(cons? v)` and head `tag` and a proper tail of `len` elements.
///
/// Shen's pattern compiler emits that shape as a nest of `cons?` / `hd` / `tl`.
/// One walk matches it. A short or untagged list is false, not an error, which
/// is what the `and` short-circuit did.
#[inline(never)]
pub fn headed_list(interp: &mut Interp, v: &Value, tag: &str, len: usize) -> ShenResult<Value> {
    let want = interp.intern(tag);
    let mut cur = *v;
    for i in 0..len {
        if !cur.is_cons() {
            return Ok(Value::bool(false));
        }
        if i == 0 {
            let h = hd(&cur)?;
            if h.as_sym() != Some(want) {
                return Ok(Value::bool(false));
            }
        }
        cur = tl(&cur)?;
    }
    Ok(Value::bool(cur.is_nil()))
}

/// `(hd (tl^n v))` as one walk. Cold error path lives here, not at every field.
#[inline(never)]
pub fn list_ref(v: &Value, n: usize) -> ShenResult<Value> {
    hd(&list_tail(v, n)?)
}

/// `(tl^n v)`.
#[inline(never)]
pub fn list_tail(v: &Value, n: usize) -> ShenResult<Value> {
    let mut cur = *v;
    for _ in 0..n {
        cur = tl(&cur)?;
    }
    Ok(cur)
}

#[inline(always)]
pub fn address_ref(v: &Value, n: &Value) -> ShenResult<Value> {
    match (v.is_vec(), n.as_int()) {
        (true, Some(i)) => v
            .vec_get_opt(i as usize)
            .ok_or_else(|| ShenError::new(format!("<-address: out of range {i}"))),
        _ => Err(ShenError::new(format!("<-address: bad args: {v:?}, {n:?}"))),
    }
}

#[inline(always)]
pub fn address_set(v: &Value, n: &Value, x: &Value) -> ShenResult<Value> {
    match (v.is_vec(), n.as_int()) {
        (true, Some(i)) => {
            let idx = i as usize;
            if idx >= v.vec_len() {
                return Err(ShenError::new(format!("address->: out of range {i}")));
            }
            v.vec_set(idx, *x);
            Ok(*v)
        }
        _ => Err(ShenError::new(format!("address->: bad args: {v:?}, {n:?}"))),
    }
}

/// `(set NAME Val)` when NAME is a free symbol — intern_static + store.
#[inline(always)]
pub fn set_global(interp: &mut Interp, name: &'static str, val: Value) -> Value {
    let sym = interp.intern_static(name);
    interp.env.set_global(sym, val);
    val
}

/// Function pointer stored in an AOT install table.
pub type KlFn = fn(&mut Interp, &[Value]) -> ShenResult<Value>;

/// One registration loop for an AOT module. The straight-line `install_*`
/// helpers were inlined into the binary's `main`.
#[inline(never)]
pub fn install_fns(interp: &mut Interp, table: &[(&str, usize, KlFn)]) {
    for &(name, arity, f) in table {
        interp.register_native(name, arity, f);
        interp.register_aot_direct(name, f);
    }
}

/// Shared arity error. One `format!` instead of one per defun.
#[cold]
#[inline(never)]
pub fn arity_mismatch(name: &str, expected: usize, got: usize) -> ShenError {
    ShenError::new(format!("{name}: expected {expected} args, got {got}"))
}

/// Shared `(cond …)` fallthrough. The string used to be inlined at every cond.
#[cold]
#[inline(never)]
pub fn cond_failed() -> ShenError {
    ShenError::new("cond: no clause matched")
}

/// Consecutive `(set NAME LITERAL)` steps from `shen.initialise`.
pub fn set_lits(interp: &mut Interp, rows: &[(&str, Lit)]) -> ShenResult<Value> {
    let mut last = Value::nil();
    for &(name, lit) in rows {
        last = lit_value(interp, lit)?;
        let sym = interp.intern(name);
        interp.env.set_global(sym, last);
    }
    Ok(last)
}

/// One element of a static list emitted for a long literal `cons` spine
/// (arity tables, external-symbol lists, `shen.*special*`).
///
/// klcompile turns those spines into data plus one call to [`lit_list`]
/// instead of a straight-line chain of `cons` instructions.
#[derive(Clone, Copy)]
pub enum Lit {
    Sym(&'static str),
    Int(i64),
    Str(&'static str),
    Bool(bool),
    Nil,
    /// `(intern "…")`, including Shen's `true`/`false` → boolean rule.
    Intern(&'static str),
    /// `(vector N)` — the kernel vector constructor, called while the list
    /// is built (defuns are already installed when init runs).
    Vector(i64),
}

/// Build a right-nested KL list from [`Lit`]s, head first.
pub fn lit_list(interp: &mut Interp, lits: &[Lit]) -> ShenResult<Value> {
    let mut tail = Value::nil();
    for lit in lits.iter().rev() {
        let head = lit_value(interp, *lit)?;
        tail = cons(&head, &tail);
    }
    Ok(tail)
}

fn lit_value(interp: &mut Interp, lit: Lit) -> ShenResult<Value> {
    Ok(match lit {
        Lit::Sym(s) => Value::sym(interp.intern(s)),
        Lit::Int(n) => Value::int(n),
        Lit::Str(s) => Value::str(s),
        Lit::Bool(b) => Value::bool(b),
        Lit::Nil => Value::nil(),
        Lit::Intern(s) => interp.intern_kl(s),
        Lit::Vector(n) => apply_direct(interp, "vector", &[Value::int(n)])?,
    })
}

/// Nested 1-arg lambdas whose body is `(name x1 … xn)`, the shape of every
/// `shen.lambda-form` entry the kernel installs. One builder replaces the
/// straight-line closure nest.
pub fn curried_fn(interp: &mut Interp, name: &'static str, arity: usize) -> Value {
    curried_from(interp, name, arity, Vec::new())
}

fn curried_from(
    interp: &mut Interp,
    name: &'static str,
    arity: usize,
    captured: Vec<Value>,
) -> Value {
    let captured_body = captured.clone();
    make_aot_closure(
        "<lambda>",
        1,
        move |interp, args| {
            let mut all = captured_body.clone();
            all.push(args[0]);
            if all.len() >= arity {
                apply_direct(interp, name, &all)
            } else {
                Ok(curried_from(interp, name, arity, all))
            }
        },
        captured,
        interp,
    )
}

/// `(cons name (curried-fn name arity))`.
pub fn lambda_entry(interp: &mut Interp, name: &'static str, arity: usize) -> Value {
    let sym = Value::sym(interp.intern(name));
    let lam = curried_fn(interp, name, arity);
    cons(&sym, &lam)
}

/// The `shen.*lambdatable*` alist: a list of [`lambda_entry`] pairs.
pub fn lambda_alist(interp: &mut Interp, entries: &[(&'static str, usize)]) -> Value {
    let mut tail = Value::nil();
    for &(name, arity) in entries.iter().rev() {
        let e = lambda_entry(interp, name, arity);
        tail = cons(&e, &tail);
    }
    tail
}

/// Run `(shen.set-lambda-form-entry (cons name <curried>))` for each row.
/// Returns whatever the last call returned, matching a `do` chain.
pub fn install_lambda_forms(
    interp: &mut Interp,
    entries: &[(&'static str, usize)],
) -> ShenResult<Value> {
    let mut result = Value::nil();
    for &(name, arity) in entries {
        let entry = lambda_entry(interp, name, arity);
        result = apply_direct(interp, "shen.set-lambda-form-entry", &[entry])?;
    }
    Ok(result)
}

#[cfg(test)]
mod table_tests {
    use super::*;
    use crate::interp::eval::Interp;

    #[test]
    fn lit_list_matches_nested_cons() {
        let mut interp = Interp::new();
        let built = lit_list(
            &mut interp,
            &[
                Lit::Sym("hd"),
                Lit::Int(1),
                Lit::Intern(":"),
                Lit::Bool(true),
                Lit::Nil,
            ],
        )
        .unwrap();
        let colon = interp.intern_kl(":");
        let manual = cons(
            &Value::sym(interp.intern("hd")),
            &cons(
                &Value::int(1),
                &cons(
                    &colon,
                    &cons(&Value::bool(true), &cons(&Value::nil(), &Value::nil())),
                ),
            ),
        );
        assert!(crate::value::shen_eq(&built, &manual));
    }

    #[test]
    fn headed_list_matches_shen_pattern() {
        let mut interp = Interp::new();
        let tag = Value::sym(interp.intern("group-members"));
        let name = Value::sym(interp.intern("ada"));
        let good = cons(&tag, &cons(&name, &Value::nil()));
        let hit = headed_list(&mut interp, &good, "group-members", 2).unwrap();
        assert_eq!(hit.as_bool(), Some(true));
        let short = cons(&tag, &Value::nil());
        let miss = headed_list(&mut interp, &short, "group-members", 2).unwrap();
        assert_eq!(miss.as_bool(), Some(false));
        let other = cons(&name, &cons(&tag, &Value::nil()));
        let miss_tag = headed_list(&mut interp, &other, "group-members", 2).unwrap();
        assert_eq!(miss_tag.as_bool(), Some(false));
        let atom = Value::int(1);
        let miss_atom = headed_list(&mut interp, &atom, "group-members", 2).unwrap();
        assert_eq!(miss_atom.as_bool(), Some(false));
    }

    #[test]
    fn curried_fn_is_nested_application() {
        let mut interp = Interp::new();
        let f = curried_fn(&mut interp, "+", 2);
        let g = interp.apply(f, vec![Value::int(20)]).unwrap();
        let r = interp.apply(g, vec![Value::int(22)]).unwrap();
        assert_eq!(r.as_int(), Some(42));
    }
}
