//! KL primitives.
//!
//! The full set the kernel expects. Implementations follow `shen-cl`'s
//! `primitives.lsp` and `shen-ocaml`'s `primitives.ml`. Special forms
//! (`if`, `let`, `lambda`, `defun`, `cond`, `freeze`, `trap-error`, `do`,
//! `and`, `or`) are NOT registered here — they're dispatched in
//! `interp::eval::Interp::step` because they require non-strict evaluation.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{ShenError, ShenResult};
use crate::interp::eval::{DirectFn, Interp};
use crate::value::{as_shen_bool, shen_eq, Stream, Value};

/// Cap oversized absvector/vector requests so an absurd size raises a
/// catchable Shen error instead of OOM-aborting. 2^24 (~16.7M slots) is
/// ~800× the largest vector the kernel itself allocates.
const MAX_ABSVECTOR: i64 = 1 << 24;

/// Register every primitive in the function namespace.
pub fn register_all(interp: &mut Interp) {
    register_core(interp);
    crate::cedar::primitives::register_all(interp);
    // Host extensions (shen.x) are installed in register_hot_overrides after
    // kernel boot so globals survive boot side-effects.
}

/// Dual-register a kernel override: the env closure *and* the AOT
/// direct-dispatch slot. Plain `fn`s only — capturing closures cannot
/// populate `aot_direct`, and AOT callers would otherwise skip the override.
fn override_kernel(interp: &mut Interp, name: &str, arity: usize, f: DirectFn) {
    interp.register_native(name, arity, f);
    interp.register_aot_direct(name, f);
}

/// Overrides that must be live *before* `shen.initialise` / `declarations.kl`
/// populate `*property-vector*`. `hash` is the only one: `put`/`get` bucket
/// with `(hash key (limit V))`, and a different algorithm at populate vs
/// lookup corrupts the store. Shen/Scheme drops the kernel `hash` defun from
/// generated code for the same reason; we re-assert the native after `sys.kl`
/// (which would otherwise overwrite the primitive) and again after AOT install.
pub fn register_early_overrides(interp: &mut Interp) {
    override_kernel(interp, "hash", 2, hot_hash);
    // Property store: native put/get must be live before declarations.kl
    // fills *property-vector*, same reason as hash.
    override_kernel(interp, "put", 4, hot_put);
    override_kernel(interp, "get", 3, hot_get);
    override_kernel(interp, "unput", 3, hot_unput);
}

/// Override kernel-level Shen functions with native Rust implementations
/// on hot paths. Per the upstream call-frequency table
/// (`gist.github.com/otabat/0ffb06fb7517fcd11906086fcc511bee`), the
/// per-test-suite call counts dwarf everything else: `element?` 12.6 M,
/// `shen.pvar?` 7 M, `shen.lazyderef` 3.5 M, `fail` 3.4 M. The rest follow
/// Shen/Scheme: the kernel ships portable-but-slow bodies (`integer?` is a
/// recursive subtraction, `symbol?` walks `str` through `analyse-symbol?`,
/// `hash` explodes the value to codepoints) that map to a handful of native
/// operations. Call **after** kernel boot so these override the AOT defuns.
pub fn register_hot_overrides(interp: &mut Interp) {
    // Dual-registered so AOT callers (prolog, typechecker, reader) hit the
    // native via rt::apply_direct rather than the kernel AOT body.
    override_kernel(interp, "element?", 2, hot_element_p);
    override_kernel(interp, "shen.pvar?", 1, hot_pvar_p);
    override_kernel(interp, "shen.lazyderef", 2, hot_lazyderef);
    override_kernel(interp, "fail", 0, hot_fail);
    override_kernel(interp, "value/or", 2, hot_value_or);
    override_kernel(interp, "<-address/or", 3, hot_address_or);
    override_kernel(interp, "<-vector/or", 3, hot_vector_or);

    // Shen/Scheme kernel overrides: portable bodies that are cheap natively.
    register_early_overrides(interp);
    override_kernel(interp, "not", 1, hot_not);
    override_kernel(interp, "boolean?", 1, hot_boolean_p);
    override_kernel(interp, "integer?", 1, hot_integer_p);
    override_kernel(interp, "empty?", 1, hot_empty_p);
    override_kernel(interp, "symbol?", 1, hot_symbol_p);
    override_kernel(interp, "variable?", 1, hot_variable_p);
    override_kernel(interp, "shen.analyse-symbol?", 1, hot_analyse_symbol_p);
    override_kernel(interp, "@p", 2, hot_tuple);
    override_kernel(interp, "tuple?", 1, hot_tuple_p);
    override_kernel(interp, "fst", 1, hot_fst);
    override_kernel(interp, "snd", 1, hot_snd);
    override_kernel(interp, "vector", 1, hot_vector);
    override_kernel(interp, "<-vector", 2, hot_vector_ref);
    override_kernel(interp, "vector->", 3, hot_vector_set);
    override_kernel(interp, "limit", 1, hot_limit);
    override_kernel(interp, "hdstr", 1, hot_hdstr);
    override_kernel(interp, "shen.byte->digit", 1, hot_byte_to_digit);
    override_kernel(interp, "shen.digit?", 1, hot_digit_p);
    override_kernel(interp, "shen.lowercase?", 1, hot_lowercase_p);
    override_kernel(interp, "shen.uppercase?", 1, hot_uppercase_p);

    // read-file-as-bytelist / read-file-as-string — bulk file read
    // instead of byte-by-byte through `read-byte`. The kernel's
    // implementation is a recursive cons-builder; on a large source
    // file this dominates load time.
    override_kernel(
        interp,
        "read-file-as-bytelist",
        1,
        hot_read_file_as_bytelist,
    );
    override_kernel(interp, "read-file-as-string", 1, hot_read_file_as_string);

    // pr — print a string to a stream. The canonical kernel `pr` gates the
    // *whole* write on `*hush*` (see kernel/klambda/writer.kl), so under
    // `-q` / `(set *hush* true)` even a write to an explicitly-opened file
    // stream is silently dropped, producing zero-byte files (issue #2).
    // That gate is only meant to silence interactive console chatter, so we
    // consult `*hush*` ONLY when the target is the standard output stream;
    // writes to any other (file) stream always occur. See #2.
    override_kernel(interp, "pr", 2, hot_pr);

    register_shenx(interp);
}

/// pr — write the string `args[0]` to the output stream `args[1]`, honouring
/// `*hush*` only for the standard output stream (issue #2). Returns the
/// original string, matching the kernel contract.
fn hot_pr(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let s = match args[0].as_str() {
        Some(s) => s.to_string(),
        None => {
            return Err(ShenError::new(format!(
                "pr: arg 1 not a string: {:?}",
                args[0]
            )))
        }
    };
    let target = match args[1].as_stream() {
        Some(t) => t,
        None => {
            return Err(ShenError::new(format!(
                "pr: arg 2 not a stream: {:?}",
                args[1]
            )))
        }
    };

    // Suppress the write only when `*hush*` is set AND the target is the
    // standard output stream. File (and other) streams always get written.
    let hush_sym = interp.intern("*hush*");
    let hush = matches!(
        interp.env.get_global(hush_sym).and_then(|v| v.as_bool()),
        Some(true)
    );
    if hush {
        let stout_sym = interp.intern("*stoutput*");
        let is_stdout = interp
            .env
            .get_global(stout_sym)
            .and_then(|v| v.as_stream())
            .is_some_and(|out| Rc::ptr_eq(&out, &target));
        if is_stdout {
            return Ok(args[0]);
        }
    }

    let mut stream = target.borrow_mut();
    match &mut *stream {
        Stream::Out(w) => {
            w.write_all(s.as_bytes())
                .map_err(|e| ShenError::new(format!("pr: {e}")))?;
        }
        _ => return Err(ShenError::new("pr: not an output stream")),
    }
    Ok(args[0])
}

// --- hot-override bodies (DirectFn-compatible plain fns) ---

/// element? — linear scan of a proper list with shen_eq.
fn hot_element_p(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let target = &args[0];
    let mut cur: Value = args[1];
    loop {
        if cur.is_nil() {
            return Ok(Value::bool(false));
        }
        let (h, t) = match (cur.head(), cur.tail()) {
            (Some(h), Some(t)) => (*h, *t),
            _ => return Err(ShenError::new(format!("element?: not a list: {cur:?}"))),
        };
        if shen_eq(target, &h) {
            return Ok(Value::bool(true));
        }
        cur = t;
    }
}

/// shen.pvar? — Shen Prolog logic variable check. The kernel
/// representation is an absvector whose slot 0 is the symbol `shen.pvar`.
fn hot_pvar_p(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let k_pvar = interp.well_known.k_shen_pvar;
    Ok(Value::bool(
        args[0].vec_get_opt(0).and_then(|c| c.as_sym()) == Some(k_pvar),
    ))
}

/// shen.lazyderef — chase Prolog-variable bindings through the
/// *prolog-vector*. The kernel implementation recurses through
/// `shen.lazyderef` itself; we tightloop in Rust.
fn hot_lazyderef(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let k_pvar = interp.well_known.k_shen_pvar;
    let k_null = interp.well_known.k_shen_null;
    let vec = args[1];
    if !vec.is_vec() {
        return Err(ShenError::new(format!(
            "shen.lazyderef: arg 2 not a vector: {:?}",
            args[1]
        )));
    }
    let mut cur = args[0];
    loop {
        // Is `cur` a pvar (slot 0 == shen.pvar)?
        let is_pvar = cur.vec_get_opt(0).and_then(|c| c.as_sym()) == Some(k_pvar);
        if !is_pvar {
            return Ok(cur);
        }
        let idx: i64 = match cur.vec_get_opt(1).and_then(|c| c.as_int()) {
            Some(i) => i,
            None => return Ok(cur),
        };
        // Look up its binding in *prolog-vector*.
        let next = vec.vec_get_opt(idx as usize).unwrap_or(Value::nil());
        // Unbound → return the pvar itself.
        if next.as_sym() == Some(k_null) {
            return Ok(cur);
        }
        cur = next;
    }
}

/// fail — return the magic shen.fail! symbol. Kernel-level `fail` is just
/// `(defun fail () shen.fail!)`.
fn hot_fail(interp: &mut Interp, _args: &[Value]) -> ShenResult<Value> {
    Ok(Value::sym(interp.well_known.k_shen_fail))
}

fn hot_value_or(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let sym = match args[0].as_sym() {
        Some(s) => s,
        None => {
            return Err(ShenError::new(format!(
                "value/or: arg 1 not a symbol: {:?}",
                args[0]
            )))
        }
    };
    if let Some(v) = interp.env.get_global(sym).copied() {
        Ok(v)
    } else {
        interp.apply(args[1], vec![])
    }
}

fn hot_address_or(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if let Some(i) = args[1].as_int() {
        if let Some(cell) = args[0].vec_get_opt(i as usize) {
            return Ok(cell);
        }
    }
    interp.apply(args[2], vec![])
}

/// Native `hash` matching kernel contract: result in `1..Bound`, never 0
/// (bucket 0 stores the vector length). Algorithm need not match the kernel's
/// `hashkey`/`mod` — only the 0-guard must, since the same override is used
/// at both populate and lookup (Shen/Scheme `overrides.shen`).
fn hash_bound(v: &Value, bound: i64) -> ShenResult<i64> {
    if bound < 1 {
        return Err(ShenError::new(format!(
            "hash: bound must be >= 1, got {bound}"
        )));
    }
    let mut h = DefaultHasher::new();
    value_hash(v, &mut h);
    let r = (h.finish() % bound as u64) as i64;
    Ok(if r == 0 { 1 } else { r })
}

fn hot_hash(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    match args[1].as_int() {
        Some(bound) => Ok(Value::int(hash_bound(&args[0], bound)?)),
        None => Err(ShenError::new(format!(
            "hash: bad args: {:?}, {:?}",
            args[0], args[1]
        ))),
    }
}

fn hot_not(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    Ok(Value::bool(!shen_boolean(interp, &args[0])?))
}

fn hot_boolean_p(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    Ok(Value::bool(is_boolean(interp, &args[0])))
}

fn hot_integer_p(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    Ok(Value::bool(is_shen_integer(&args[0])))
}

fn hot_empty_p(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    Ok(Value::bool(args[0].is_nil()))
}

fn hot_symbol_p(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    Ok(Value::bool(is_shen_symbol(interp, &args[0])))
}

fn hot_variable_p(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let v = &args[0];
    if is_boolean(interp, v) || v.is_number() || v.is_str() {
        return Ok(Value::bool(false));
    }
    let Some(s) = v.as_sym() else {
        return Ok(Value::bool(false));
    };
    Ok(Value::bool(analyse_variable(interp.resolve(s))))
}

fn hot_analyse_symbol_p(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    match args[0].as_str() {
        Some(s) if !s.is_empty() => Ok(Value::bool(analyse_symbol(s))),
        Some(_) => Err(ShenError::new(
            "implementation error in shen.analyse-symbol?",
        )),
        None => Err(ShenError::new(format!(
            "shen.analyse-symbol?: not a string: {:?}",
            args[0]
        ))),
    }
}

fn hot_tuple(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    Ok(Value::absvector(vec![
        Value::sym(interp.well_known.k_shen_tuple),
        args[0],
        args[1],
    ]))
}

fn hot_tuple_p(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    Ok(Value::bool(
        args[0].vec_get_opt(0).and_then(|c| c.as_sym()) == Some(interp.well_known.k_shen_tuple),
    ))
}

fn hot_fst(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    args[0]
        .vec_get_opt(1)
        .ok_or_else(|| ShenError::new(format!("fst: bad tuple: {:?}", args[0])))
}

fn hot_snd(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    args[0]
        .vec_get_opt(2)
        .ok_or_else(|| ShenError::new(format!("snd: bad tuple: {:?}", args[0])))
}

fn hot_vector(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let n = match args[0].as_int() {
        Some(n) if n >= 0 => n,
        _ => return Err(ShenError::new(format!("vector: bad arg: {:?}", args[0]))),
    };
    let len = n.checked_add(1).ok_or_else(|| {
        ShenError::new(format!("vector size {n} out of range (0..{MAX_ABSVECTOR})"))
    })?;
    if len > MAX_ABSVECTOR {
        return Err(ShenError::new(format!(
            "vector size {n} out of range (0..{MAX_ABSVECTOR})"
        )));
    }
    let fail = Value::sym(interp.well_known.k_shen_fail);
    let mut cells = vec![fail; len as usize];
    cells[0] = Value::int(n);
    Ok(Value::absvector(cells))
}

fn hot_vector_ref(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let i = match args[1].as_int() {
        Some(i) => i,
        None => {
            return Err(ShenError::new(format!(
                "<-vector: bad index: {:?}",
                args[1]
            )))
        }
    };
    if i == 0 {
        return Err(ShenError::new("cannot access 0th element of a vector\n"));
    }
    match args[0].vec_get_opt(i as usize) {
        Some(v) if v.as_sym() == Some(interp.well_known.k_shen_fail) => {
            Err(ShenError::new("vector element not found\n"))
        }
        Some(v) => Ok(v),
        None => Err(ShenError::new("vector element not found\n")),
    }
}

fn hot_vector_set(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let i = match args[1].as_int() {
        Some(i) => i,
        None => {
            return Err(ShenError::new(format!(
                "vector->: bad index: {:?}",
                args[1]
            )))
        }
    };
    if i == 0 {
        return Err(ShenError::new("cannot access 0th element of a vector\n"));
    }
    if !args[0].is_vec() {
        return Err(ShenError::new(format!(
            "vector->: not a vector: {:?}",
            args[0]
        )));
    }
    let idx = i as usize;
    if idx >= args[0].vec_len() {
        return Err(ShenError::new(format!("vector->: out of range {i}")));
    }
    args[0].vec_set(idx, args[2]);
    Ok(args[0])
}

fn hot_limit(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    args[0]
        .vec_get_opt(0)
        .ok_or_else(|| ShenError::new(format!("limit: not a vector: {:?}", args[0])))
}

/// `(<-vector V N)` that returns the default thunk instead of erroring on
/// index 0, OOB, or an uninitialised `(fail)` slot.
fn hot_vector_or(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if let Some(i) = args[1].as_int() {
        if i != 0 {
            if let Some(v) = args[0].vec_get_opt(i as usize) {
                if v.as_sym() != Some(interp.well_known.k_shen_fail) {
                    return Ok(v);
                }
            }
        }
    }
    interp.apply(args[2], vec![])
}

/// `None` = uninitialised / OOB slot (kernel `<-vector` would error).
fn vector_bucket(interp: &Interp, dict: Value, key: &Value) -> ShenResult<(i64, Option<Value>)> {
    let bound = dict
        .vec_get_opt(0)
        .and_then(|v| v.as_int())
        .ok_or_else(|| ShenError::new("put/get: not a vector"))?;
    let h = hash_bound(key, bound)?;
    let bucket = match dict.vec_get_opt(h as usize) {
        Some(v) if v.as_sym() == Some(interp.well_known.k_shen_fail) => None,
        Some(v) => Some(v),
        None => None,
    };
    Ok((h, bucket))
}

/// Association-list lookup: first pair whose `hd` equals `key`.
fn assoc_entry(key: &Value, mut list: Value) -> ShenResult<Option<Value>> {
    loop {
        if list.is_nil() {
            return Ok(None);
        }
        let (h, t) = match (list.head(), list.tail()) {
            (Some(h), Some(t)) => (*h, *t),
            _ => return Err(ShenError::new("attempt to search a non-list with assoc\n")),
        };
        if let Some(hh) = h.head() {
            if shen_eq(hh, key) {
                return Ok(Some(h));
            }
        }
        list = t;
    }
}

fn change_pointer(key: Value, prop: Value, val: Value, bucket: Value) -> ShenResult<Value> {
    if bucket.is_nil() {
        return Ok(Value::cons(
            Value::cons(Value::cons(key, Value::cons(prop, Value::nil())), val),
            Value::nil(),
        ));
    }
    let (h, t) = match (bucket.head(), bucket.tail()) {
        (Some(h), Some(t)) => (*h, *t),
        _ => {
            return Err(ShenError::new(
                "implementation error in shen.change-pointer-value",
            ))
        }
    };
    let match_here = h.head().and_then(|pair| {
        let k = pair.head()?;
        let rest = pair.tail()?;
        let p = rest.head()?;
        let rest2 = rest.tail()?;
        if rest2.is_nil() && shen_eq(k, &key) && shen_eq(p, &prop) {
            Some(())
        } else {
            None
        }
    });
    if match_here.is_some() {
        let pair = h
            .head()
            .copied()
            .ok_or_else(|| ShenError::new("implementation error in shen.change-pointer-value"))?;
        Ok(Value::cons(Value::cons(pair, val), t))
    } else {
        Ok(Value::cons(h, change_pointer(key, prop, val, t)?))
    }
}

fn remove_pointer(key: Value, prop: Value, bucket: Value) -> ShenResult<Value> {
    if bucket.is_nil() {
        return Ok(Value::nil());
    }
    let (h, t) = match (bucket.head(), bucket.tail()) {
        (Some(h), Some(t)) => (*h, *t),
        _ => {
            return Err(ShenError::new(
                "implementation error in shen.remove-pointer",
            ))
        }
    };
    let match_here = h.head().and_then(|pair| {
        let k = pair.head()?;
        let rest = pair.tail()?;
        let p = rest.head()?;
        let rest2 = rest.tail()?;
        if rest2.is_nil() && shen_eq(k, &key) && shen_eq(p, &prop) {
            Some(())
        } else {
            None
        }
    });
    if match_here.is_some() {
        Ok(t)
    } else {
        Ok(Value::cons(h, remove_pointer(key, prop, t)?))
    }
}

fn hot_put(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let (key, prop, val, dict) = (args[0], args[1], args[2], args[3]);
    let (h, bucket) = vector_bucket(interp, dict, &key)?;
    let new_bucket = change_pointer(key, prop, val, bucket.unwrap_or(Value::nil()))?;
    let idx = h as usize;
    if idx >= dict.vec_len() {
        return Err(ShenError::new(format!("put: out of range {h}")));
    }
    dict.vec_set(idx, new_bucket);
    Ok(val)
}

fn hot_get(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let (key, prop, dict) = (args[0], args[1], args[2]);
    let (_h, bucket) = vector_bucket(interp, dict, &key)?;
    let ks = value_to_str(interp, &key);
    let ps = value_to_str(interp, &prop);
    let Some(bucket) = bucket else {
        return Err(ShenError::new(format!("{ks} has no attributes: {ps}\n")));
    };
    let needle = Value::cons(key, Value::cons(prop, Value::nil()));
    match assoc_entry(&needle, bucket)? {
        Some(entry) => entry
            .tail()
            .copied()
            .ok_or_else(|| ShenError::new("implementation error in shen.change-pointer-value")),
        None => Err(ShenError::new(format!(
            "attribute {ps} not found for {ks}\n"
        ))),
    }
}

fn hot_unput(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let (key, prop, dict) = (args[0], args[1], args[2]);
    let (h, bucket) = vector_bucket(interp, dict, &key)?;
    let new_bucket = remove_pointer(key, prop, bucket.unwrap_or(Value::nil()))?;
    let idx = h as usize;
    if idx >= dict.vec_len() {
        return Err(ShenError::new(format!("unput: out of range {h}")));
    }
    dict.vec_set(idx, new_bucket);
    Ok(key)
}

fn hot_hdstr(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    match args[0].as_str() {
        Some(s) => s
            .chars()
            .next()
            .map(|c| Value::str(c.to_string()))
            .ok_or_else(|| ShenError::new("hdstr: empty string")),
        None => Err(ShenError::new(format!(
            "hdstr: not a string: {:?}",
            args[0]
        ))),
    }
}

fn hot_byte_to_digit(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    match args[0].as_int() {
        Some(n) => Ok(Value::int(n - 48)),
        None => Err(ShenError::new(format!(
            "shen.byte->digit: not an int: {:?}",
            args[0]
        ))),
    }
}

fn hot_digit_p(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    Ok(Value::bool(
        args[0].as_int().is_some_and(|n| (48..=57).contains(&n)),
    ))
}

fn hot_lowercase_p(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    Ok(Value::bool(
        args[0].as_int().is_some_and(|n| (97..=122).contains(&n)),
    ))
}

fn hot_uppercase_p(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    Ok(Value::bool(
        args[0].as_int().is_some_and(|n| (65..=90).contains(&n)),
    ))
}

fn is_boolean(interp: &Interp, v: &Value) -> bool {
    v.as_bool().is_some()
        || matches!(v.as_sym(), Some(s) if s == interp.well_known.k_true || s == interp.well_known.k_false)
}

fn shen_boolean(interp: &Interp, v: &Value) -> ShenResult<bool> {
    if let Some(b) = v.as_bool() {
        return Ok(b);
    }
    match v.as_sym() {
        Some(s) if s == interp.well_known.k_true => Ok(true),
        Some(s) if s == interp.well_known.k_false => Ok(false),
        _ => Err(ShenError::new(format!("not a boolean: {v:?}"))),
    }
}

fn is_shen_integer(v: &Value) -> bool {
    if v.as_int().is_some() {
        return true;
    }
    v.as_float()
        .is_some_and(|x| x.is_finite() && x.fract() == 0.0)
}

fn is_shen_symbol(interp: &Interp, v: &Value) -> bool {
    if is_boolean(interp, v)
        || v.is_number()
        || v.is_str()
        || v.is_cons()
        || v.is_nil()
        || v.is_vec()
    {
        return false;
    }
    let Some(s) = v.as_sym() else {
        return false;
    };
    let name = interp.resolve(s);
    matches!(name, "{" | "}" | ":" | ";" | ",") || analyse_symbol(name)
}

/// Kernel `shen.analyse-symbol?`: first char is `alpha?` (letter or misc),
/// rest are `alpha?` or digit. Misc is `=*/+-_?$!@~><&%'#.` — not `{ } : ; ,`.
fn analyse_symbol(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    is_symbol_alpha(bytes[0])
        && bytes[1..]
            .iter()
            .copied()
            .all(|b| is_symbol_alpha(b) || b.is_ascii_digit())
}

fn analyse_variable(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    bytes[0].is_ascii_uppercase()
        && bytes[1..]
            .iter()
            .copied()
            .all(|b| is_symbol_alpha(b) || b.is_ascii_digit())
}

fn is_symbol_alpha(b: u8) -> bool {
    matches!(
        b,
        b'a'..=b'z'
            | b'A'..=b'Z'
            | b'=' | b'-' | b'*' | b'/' | b'+' | b'_' | b'?' | b'$' | b'!'
            | b'@' | b'~' | b'.' | b'>' | b'<' | b'&' | b'%' | b'\'' | b'#' | b'`'
    )
}

fn hot_read_file_as_bytelist(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let path = match args[0].as_str() {
        Some(s) => s.to_string(),
        None => {
            return Err(ShenError::new(format!(
                "read-file-as-bytelist: not a string: {:?}",
                args[0]
            )))
        }
    };
    let bytes = std::fs::read(&path)
        .map_err(|e| ShenError::new(format!("read-file-as-bytelist: {path}: {e}")))?;
    Ok(bytes_to_list(&bytes))
}

fn hot_read_file_as_string(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let path = match args[0].as_str() {
        Some(s) => s.to_string(),
        None => {
            return Err(ShenError::new(format!(
                "read-file-as-string: not a string: {:?}",
                args[0]
            )))
        }
    };
    let bytes = std::fs::read(&path)
        .map_err(|e| ShenError::new(format!("read-file-as-string: {path}: {e}")))?;
    // Kernel semantics: bytes interpreted as a string verbatim.
    let s = String::from_utf8_lossy(&bytes).into_owned();
    Ok(Value::str(Rc::from(s.as_str())))
}

/// Build a Shen cons-list `(b0 b1 … bN)` of bytes from a slice. Used by
/// the native `read-file-as-bytelist`.
fn bytes_to_list(bytes: &[u8]) -> Value {
    let mut acc = Value::nil();
    for &b in bytes.iter().rev() {
        acc = Value::cons(Value::int(b as i64), acc);
    }
    acc
}

fn register_core(interp: &mut Interp) {
    // --- arithmetic ---
    interp.register_native("+", 2, |_, args| {
        numeric_op(args, "+", |a, b| a + b, |a, b| a.checked_add(b))
    });
    interp.register_native("-", 2, |_, args| {
        numeric_op(args, "-", |a, b| a - b, |a, b| a.checked_sub(b))
    });
    interp.register_native("*", 2, |_, args| {
        numeric_op(args, "*", |a, b| a * b, |a, b| a.checked_mul(b))
    });
    interp.register_native("/", 2, prim_div);
    interp.register_native(">", 2, |_, args| {
        compare_op(args, ">", |o| o == std::cmp::Ordering::Greater)
    });
    interp.register_native("<", 2, |_, args| {
        compare_op(args, "<", |o| o == std::cmp::Ordering::Less)
    });
    interp.register_native(">=", 2, |_, args| {
        compare_op(args, ">=", |o| o != std::cmp::Ordering::Less)
    });
    interp.register_native("<=", 2, |_, args| {
        compare_op(args, "<=", |o| o != std::cmp::Ordering::Greater)
    });

    // --- equality ---
    interp.register_native("=", 2, |_, args| {
        Ok(Value::bool(shen_eq(&args[0], &args[1])))
    });

    // --- predicates ---
    interp.register_native("number?", 1, |_, args| Ok(Value::bool(args[0].is_number())));
    interp.register_native("string?", 1, |_, args| Ok(Value::bool(args[0].is_str())));
    interp.register_native("symbol?", 1, |_, args| Ok(Value::bool(args[0].is_sym())));
    interp.register_native("boolean?", 1, |interp, args| {
        let wk = &interp.well_known;
        let b = args[0].as_bool().is_some()
            || matches!(args[0].as_sym(), Some(s) if s == wk.k_true || s == wk.k_false);
        Ok(Value::bool(b))
    });
    interp.register_native("cons?", 1, |_, args| Ok(Value::bool(args[0].is_cons())));
    interp.register_native("absvector?", 1, |_, args| Ok(Value::bool(args[0].is_vec())));
    // Backward-compat alias used in some kernel code paths.
    interp.register_native("vector?", 1, |_, args| Ok(Value::bool(args[0].is_vec())));

    // --- lists ---
    interp.register_native("cons", 2, |_, args| Ok(Value::cons(args[0], args[1])));
    interp.register_native("hd", 1, |_, args| match args[0].head() {
        Some(h) => Ok(*h),
        None => Err(ShenError::new(format!("hd: not a cons: {:?}", args[0]))),
    });
    interp.register_native("tl", 1, |_, args| match args[0].tail() {
        Some(t) => Ok(*t),
        None => Err(ShenError::new(format!("tl: not a cons: {:?}", args[0]))),
    });

    // --- symbols / strings ---
    interp.register_native("intern", 1, |interp, args| match args[0].as_str() {
        Some(s) => Ok(interp.intern_kl(s)),
        None => Err(ShenError::new(format!(
            "intern: not a string: {:?}",
            args[0]
        ))),
    });
    interp.register_native("str", 1, |interp, args| {
        Ok(Value::str(value_to_str(interp, &args[0])))
    });
    interp.register_native("cn", 2, |_, args| {
        match (args[0].as_str(), args[1].as_str()) {
            (Some(a), Some(b)) => {
                let mut s = String::with_capacity(a.len() + b.len());
                s.push_str(a);
                s.push_str(b);
                Ok(Value::str(s))
            }
            _ => Err(ShenError::new(format!(
                "cn: strings only, got {:?} and {:?}",
                args[0], args[1]
            ))),
        }
    });
    interp.register_native("pos", 2, |_, args| {
        match (args[0].as_str(), args[1].as_int()) {
            (Some(s), Some(n)) => {
                if n < 0 {
                    return Err(ShenError::new("pos: index out of range"));
                }
                s.chars()
                    .nth(n as usize)
                    .map(|c| Value::str(c.to_string()))
                    .ok_or_else(|| ShenError::new("pos: index out of range"))
            }
            _ => Err(ShenError::new(format!(
                "pos: bad args: {:?}, {:?}",
                args[0], args[1]
            ))),
        }
    });
    interp.register_native("tlstr", 1, |_, args| match args[0].as_str() {
        Some(s) => {
            if s.is_empty() {
                return Err(ShenError::new("tlstr: empty string"));
            }
            let first_len = s
                .chars()
                .next()
                .expect("non-empty string has a first character")
                .len_utf8();
            Ok(Value::str(&s[first_len..]))
        }
        None => Err(ShenError::new(format!(
            "tlstr: not a string: {:?}",
            args[0]
        ))),
    });
    interp.register_native("n->string", 1, |_, args| match args[0].as_int() {
        Some(n) => {
            let c = char::from_u32(n as u32)
                .ok_or_else(|| ShenError::new(format!("n->string: bad codepoint {n}")))?;
            Ok(Value::str(String::from(c)))
        }
        None => Err(ShenError::new(format!(
            "n->string: not an int: {:?}",
            args[0]
        ))),
    });
    interp.register_native("string->n", 1, |_, args| match args[0].as_str() {
        Some(s) => s
            .chars()
            .next()
            .map(|c| Value::int(c as i64))
            .ok_or_else(|| ShenError::new("string->n: empty string")),
        None => Err(ShenError::new(format!(
            "string->n: not a string: {:?}",
            args[0]
        ))),
    });

    // --- function lookup ---
    interp.register_native("fn", 1, |interp, args| match args[0].as_sym() {
        Some(s) => interp
            .env
            .get_fn(s)
            .cloned()
            .ok_or_else(|| ShenError::new(format!("fn: undefined: {}", interp.resolve(s)))),
        None => Err(ShenError::new(format!("fn: not a symbol: {:?}", args[0]))),
    });

    // --- globals (dual namespace `set`/`value`) ---
    interp.register_native("set", 2, |interp, args| match args[0].as_sym() {
        Some(s) => {
            interp.env.set_global(s, args[1]);
            Ok(args[1])
        }
        None => Err(ShenError::new(format!("set: not a symbol: {:?}", args[0]))),
    });
    interp.register_native("value", 1, |interp, args| match args[0].as_sym() {
        Some(s) => {
            interp.env.get_global(s).cloned().ok_or_else(|| {
                ShenError::new(format!("value: unbound global: {}", interp.resolve(s)))
            })
        }
        None => Err(ShenError::new(format!(
            "value: not a symbol: {:?}",
            args[0]
        ))),
    });

    // --- vectors (absvectors) ---
    // Cap oversized requests so an absurd size raises a CATCHABLE Shen error
    // instead of OOM-aborting the whole process (e.g. `(absvector 100000000000)`
    // from the reader-fuzz corpus). Mirrors shen-go and shen-cl (#3).
    interp.register_native("absvector", 1, |_, args| match args[0].as_int() {
        Some(n) if (0..=MAX_ABSVECTOR).contains(&n) => {
            let len = n as usize;
            let cells = vec![Value::sym(crate::symbol::SymId(0)); len];
            // Will be overwritten with an "uninitialized" sentinel in
            // boot.rs once the kernel interns `shen.fail!`. For now we
            // just zero-init with whatever interned id 0 is (`true` per
            // WellKnown ordering — harmless for the kernel).
            Ok(Value::absvector(cells))
        }
        Some(n) if n > MAX_ABSVECTOR => Err(ShenError::new(format!(
            "absvector size {n} out of range (0..{MAX_ABSVECTOR})"
        ))),
        _ => Err(ShenError::new(format!("absvector: bad arg: {:?}", args[0]))),
    });
    interp.register_native("<-address", 2, |_, args| {
        match (args[0].is_vec(), args[1].as_int()) {
            (true, Some(i)) => args[0]
                .vec_get_opt(i as usize)
                .ok_or_else(|| ShenError::new(format!("<-address: out of range {i}"))),
            _ => Err(ShenError::new(format!(
                "<-address: bad args: {:?}, {:?}",
                args[0], args[1]
            ))),
        }
    });
    interp.register_native("address->", 3, |_, args| {
        match (args[0].is_vec(), args[1].as_int()) {
            (true, Some(i)) => {
                let idx = i as usize;
                if idx >= args[0].vec_len() {
                    return Err(ShenError::new(format!("address->: out of range {i}")));
                }
                args[0].vec_set(idx, args[2]);
                // Returns the vector itself (so it can be chained).
                Ok(args[0])
            }
            _ => Err(ShenError::new(format!(
                "address->: bad args: {:?}, {:?}",
                args[0], args[1]
            ))),
        }
    });

    // --- errors ---
    interp.register_native("simple-error", 1, |_, args| match args[0].as_str() {
        Some(s) => Err(ShenError::new(s)),
        None => Err(ShenError::new(format!("{:?}", args[0]))),
    });
    interp.register_native("error-to-string", 1, |_, args| {
        match args[0].error_message() {
            Some(s) => Ok(Value::str(s)),
            None => Err(ShenError::new(format!(
                "error-to-string: not an error: {:?}",
                args[0]
            ))),
        }
    });

    // --- meta ---
    interp.register_native("eval-kl", 1, |interp, args| {
        // KL semantics: numbers, strings, booleans, streams, closures,
        // absvectors, and other non-list values are self-evaluating.
        // Only `Cons` (= a syntactic application) and `Sym` need to be
        // converted back into a `KlExpr` and run through `eval`.
        if args[0].is_cons() || args[0].is_sym() {
            let expr = value_to_klexpr(&args[0])?;
            interp.eval(&expr)
        } else {
            Ok(args[0])
        }
    });
    interp.register_native("type", 2, |_, args| {
        // (type X T) — at runtime we ignore the annotation and return X.
        Ok(args[0])
    });
    interp.register_native("tc?", 0, |_, _| Ok(Value::bool(false)));
    interp.register_native("tc", 1, |_, args| Ok(args[0]));
    interp.register_native("get-time", 1, |_, args| {
        // (get-time TYPE) where TYPE is `real`, `run`, `unix`. We only
        // expose wall-clock seconds since UNIX epoch for now; the kernel
        // uses this just to seed RNGs.
        let _ = args;
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        Ok(Value::float(secs))
    });
    interp.register_native("hash", 2, hot_hash);
    interp.register_native("apply", 2, |interp, args| {
        let f = args[0];
        let argv = list_to_vec(&args[1])
            .ok_or_else(|| ShenError::new(format!("apply: not a list: {:?}", args[1])))?;
        interp.apply(f, argv)
    });

    // --- I/O streams ---
    interp.register_native("open", 2, |interp, args| {
        match (args[0].as_str(), args[1].as_sym()) {
            (Some(path), Some(mode)) => {
                // Direction is encoded in the mode symbol (`in` / `out`).
                match interp.resolve(mode) {
                    "in" => {
                        let f = std::fs::File::open(path)
                            .map_err(|e| ShenError::new(format!("open: {path}: {e}")))?;
                        let stream = Stream::In(Box::new(f) as Box<dyn Read>);
                        Ok(Value::stream(Rc::new(RefCell::new(stream))))
                    }
                    "out" => {
                        let f = std::fs::File::create(path)
                            .map_err(|e| ShenError::new(format!("open: {path}: {e}")))?;
                        let stream = Stream::Out(Box::new(f) as Box<dyn std::io::Write>);
                        Ok(Value::stream(Rc::new(RefCell::new(stream))))
                    }
                    other => Err(ShenError::new(format!("open: invalid direction: {other}"))),
                }
            }
            _ => Err(ShenError::new(format!(
                "open: bad args: {:?}, {:?}",
                args[0], args[1]
            ))),
        }
    });
    interp.register_native("close", 1, |_, args| match args[0].as_stream() {
        Some(s) => {
            *s.borrow_mut() = Stream::Closed;
            Ok(Value::sym(crate::symbol::SymId(0)))
        }
        None => Err(ShenError::new(format!(
            "close: not a stream: {:?}",
            args[0]
        ))),
    });
    interp.register_native("read-byte", 1, |_, args| match args[0].as_stream() {
        Some(s) => {
            let mut s = s.borrow_mut();
            match &mut *s {
                Stream::In(r) => {
                    let mut buf = [0u8; 1];
                    match r.read(&mut buf) {
                        Ok(0) => Ok(Value::int(-1)),
                        Ok(_) => Ok(Value::int(buf[0] as i64)),
                        Err(e) => Err(ShenError::new(format!("read-byte: {e}"))),
                    }
                }
                _ => Err(ShenError::new("read-byte: not an input stream")),
            }
        }
        None => Err(ShenError::new(format!(
            "read-byte: not a stream: {:?}",
            args[0]
        ))),
    });
    // Stream-flavour probes the kernel's `pr` / reader rely on. Our
    // streams are byte-oriented (read-byte/write-byte), so report
    // `false` for both. shen-cl maps these to lisp `subtypep` of the
    // stream element type; for shen-rust there's only one type.
    interp.register_native("shen.char-stoutput?", 1, |_, _args| Ok(Value::bool(false)));
    interp.register_native("shen.char-stinput?", 1, |_, _args| Ok(Value::bool(false)));

    interp.register_native("write-byte", 2, |_, args| {
        match (args[0].as_int(), args[1].as_stream()) {
            (Some(b), Some(s)) => {
                let mut s = s.borrow_mut();
                match &mut *s {
                    Stream::Out(w) => {
                        let buf = [b as u8];
                        w.write_all(&buf)
                            .map_err(|e| ShenError::new(format!("write-byte: {e}")))?;
                        Ok(Value::int(b))
                    }
                    _ => Err(ShenError::new("write-byte: not an output stream")),
                }
            }
            _ => Err(ShenError::new(format!(
                "write-byte: bad args: {:?}, {:?}",
                args[0], args[1]
            ))),
        }
    });
}

/// Optional [shen-extensions](https://github.com/pyrex41/shen-extensions) host
/// SHA-256 (`sha2` crate). Disable with `SHEN_X_SHA256=pure`.
/// Call from [`register_hot_overrides`] after kernel boot.
pub fn register_shenx(interp: &mut Interp) {
    // Shen Batteries' `library.current-features` probes this primitive to
    // determine which optional host capabilities the port provides.  Keep
    // the query installed even when the SHA backend is explicitly disabled;
    // in pure mode it simply reports an empty feature set.
    interp.register_native("shen.x.features.current", 0, shenx_current_features);
    // Keep the AOT direct-dispatch table coherent with the live closure
    // registration.  A Batteries module may itself be AOT compiled and call
    // this query from generated code.
    interp.register_aot_direct("shen.x.features.current", shenx_current_features);

    if std::env::var_os("SHEN_X_SHA256").as_deref() == Some(std::ffi::OsStr::new("pure")) {
        return;
    }
    use sha2::{Digest, Sha256};

    interp.register_native("shen.x.sha256-octets-host", 1, |_, args| {
        let mut bytes = Vec::new();
        let mut cur = args[0].clone();
        while !cur.is_nil() {
            let (h, t) = match (cur.head(), cur.tail()) {
                (Some(h), Some(t)) => (h.clone(), t.clone()),
                _ => {
                    return Err(ShenError::new(
                        "shen.x.sha256-octets-host: expected list of bytes 0..255",
                    ))
                }
            };
            let n = h.as_int().ok_or_else(|| {
                ShenError::new("shen.x.sha256-octets-host: expected list of bytes 0..255")
            })?;
            if !(0..=255).contains(&n) {
                return Err(ShenError::new(
                    "shen.x.sha256-octets-host: expected list of bytes 0..255",
                ));
            }
            bytes.push(n as u8);
            cur = t;
        }
        let digest = Sha256::digest(&bytes);
        Ok(bytes_to_list(&digest))
    });

    let backend = interp.symbols.intern("shen.x.*sha256-backend*");
    let host = interp.symbols.intern("host");
    interp.env.set_global(backend, Value::sym(host));
}

/// Return host capabilities using the feature names understood by Shen
/// Batteries.  This is deliberately a native function rather than a global:
/// Batteries calls it after boot and treats a missing/erroring query as an
/// empty feature set.  The Rust port currently ships only the SHA host
/// implementation; its presence is gated by `SHEN_X_SHA256=pure` in the same
/// way as the extension itself.
fn shenx_current_features(interp: &mut Interp, _args: &[Value]) -> ShenResult<Value> {
    if std::env::var_os("SHEN_X_SHA256").as_deref() == Some(std::ffi::OsStr::new("pure")) {
        return Ok(Value::nil());
    }
    let feature = interp.symbols.intern("shen.x/sha256-host");
    Ok(Value::cons(Value::sym(feature), Value::nil()))
}

// --- helpers ---

fn numeric_op<F, I>(args: &[Value], name: &str, f_op: F, i_op: I) -> ShenResult<Value>
where
    F: Fn(f64, f64) -> f64,
    I: Fn(i64, i64) -> Option<i64>,
{
    if let (Some(a), Some(b)) = (args[0].as_int(), args[1].as_int()) {
        return Ok(match i_op(a, b) {
            Some(v) => Value::int(v),
            None => Value::float(f_op(a as f64, b as f64)),
        });
    }
    match (args[0].as_number_f64(), args[1].as_number_f64()) {
        (Some(a), Some(b)) => Ok(Value::float(f_op(a, b))),
        _ => Err(ShenError::new(format!(
            "{name}: bad args: {:?}, {:?}",
            args[0], args[1]
        ))),
    }
}

fn prim_div(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let both_int = args[0].as_int().is_some() && args[1].as_int().is_some();
    let (Some(a), Some(b)) = (args[0].as_number_f64(), args[1].as_number_f64()) else {
        return Err(ShenError::new(format!(
            "/: bad args: {:?}, {:?}",
            args[0], args[1]
        )));
    };
    if b == 0.0 {
        return Err(ShenError::new("/: division by zero"));
    }
    let r = a / b;
    // Return Int when the result is exact and both inputs are integers.
    if both_int && r.fract() == 0.0 {
        Ok(Value::int(r as i64))
    } else {
        Ok(Value::float(r))
    }
}

fn compare_op<P>(args: &[Value], name: &str, pred: P) -> ShenResult<Value>
where
    P: Fn(std::cmp::Ordering) -> bool,
{
    let ord = if let (Some(a), Some(b)) = (args[0].as_int(), args[1].as_int()) {
        a.cmp(&b)
    } else {
        let (Some(a), Some(b)) = (args[0].as_number_f64(), args[1].as_number_f64()) else {
            return Err(ShenError::new(format!(
                "{name}: bad args: {:?}, {:?}",
                args[0], args[1]
            )));
        };
        a.partial_cmp(&b)
            .ok_or_else(|| ShenError::new(format!("{name}: NaN comparison")))?
    };
    Ok(Value::bool(pred(ord)))
}

/// Convert a Shen value to its `str` representation. Mirrors shen-cl's
/// printer for the small set of atoms we expose at this phase.
fn value_to_str(interp: &Interp, v: &Value) -> String {
    if v.is_nil() {
        return "()".to_string();
    }
    if let Some(b) = v.as_bool() {
        return if b { "true" } else { "false" }.to_string();
    }
    if let Some(n) = v.as_int() {
        return n.to_string();
    }
    if let Some(x) = v.as_float() {
        return format_float(x);
    }
    if let Some(s) = v.as_str() {
        return format!("\"{s}\"");
    }
    if let Some(s) = v.as_sym() {
        return interp.resolve(s).to_string();
    }
    if let Some(s) = v.error_message() {
        return format!("#<error {s}>");
    }
    if v.is_cons() {
        return format!("{v:?}");
    }
    // Use the Common-Lisp "unreadable object" convention `#<... ...>`, with at
    // least one space inside. The kernel's `symbol?` falls back to parsing the
    // result of `str`; its `shen.analyse-symbol?` accepts the leading `#` (it's
    // in `shen.misc?`), so the disqualifier is the embedded whitespace —
    // `shen.alphanums?` rejects space-containing strings, so a closure can't
    // pose as a symbol. Without the space, `symbol?` returns true for closures
    // and the spreadsheet test (and any `(or (number? V) (symbol? V) ...)` guard
    // pattern) breaks.
    if v.is_vec() {
        return "#<absvector x>".to_string();
    }
    if v.is_closure() {
        return "#<closure x>".to_string();
    }
    if v.as_stream().is_some() {
        return "#<stream x>".to_string();
    }
    "#<foreign x>".to_string()
}

/// Format an `f64` the way Shen expects: always include a decimal
/// point. `format!("{x}")` on `4000.0` yields `"4000"`, which matches
/// the int display and breaks any test comparing `(* 5000 .8)` against
/// `4000.0`. Match shen-cl's behavior of always printing the decimal.
///
/// NaN renders as lowercase `nan`. Rust's `Display` spells the infinities
/// lowercase (`inf` / `-inf`) but NaN as `NaN`; the cross-port convention
/// is all-lowercase, and shen-go / shen-lua both print `nan`. Keep this in
/// sync with the REPL renderer's copy in `bin/shen-rust/src/main.rs`.
fn format_float(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_finite() && x == x.trunc() && x.abs() < 1e16 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

fn list_to_vec(v: &Value) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    let mut cur = *v;
    loop {
        if cur.is_nil() {
            return Some(out);
        }
        match (cur.head(), cur.tail()) {
            (Some(h), Some(t)) => {
                out.push(*h);
                cur = *t;
            }
            _ => return None,
        }
    }
}

/// Convert a Shen value (list-form code) into a `KlExpr` AST. Used by
/// `eval-kl`.
fn value_to_klexpr(v: &Value) -> ShenResult<crate::kl::ast::KlExpr> {
    use crate::kl::ast::KlExpr;
    if v.is_nil() {
        return Ok(KlExpr::Nil);
    }
    if let Some(b) = v.as_bool() {
        return Ok(KlExpr::Bool(b));
    }
    if let Some(n) = v.as_int() {
        return Ok(KlExpr::Int(n));
    }
    if let Some(x) = v.as_float() {
        return Ok(KlExpr::Float(x));
    }
    if let Some(s) = v.as_str() {
        return Ok(KlExpr::Str(Rc::from(s)));
    }
    if let Some(s) = v.as_sym() {
        return Ok(KlExpr::Sym(s));
    }
    if v.is_cons() {
        let items = list_to_vec(v).ok_or_else(|| ShenError::new("eval-kl: improper list"))?;
        let mut elems = Vec::with_capacity(items.len());
        for it in items {
            elems.push(value_to_klexpr(&it)?);
        }
        return Ok(KlExpr::App(elems.into()));
    }
    Err(ShenError::new(format!(
        "eval-kl: cannot convert {v:?} to KlExpr"
    )))
}

fn value_hash<H: Hasher>(v: &Value, h: &mut H) {
    // Discriminants must match `shen_eq`: Bool and the symbols true/false
    // share a bucket, as do Int N and Float N.0 (and +0.0/-0.0).
    if v.is_nil() {
        0u8.hash(h);
    } else if let Some(b) = as_shen_bool(v) {
        1u8.hash(h);
        b.hash(h);
    } else if v.is_number() {
        2u8.hash(h);
        let x = v.as_number_f64().expect("is_number");
        let bits = if x == 0.0 {
            0.0f64.to_bits()
        } else {
            x.to_bits()
        };
        bits.hash(h);
    } else if let Some(s) = v.as_str() {
        4u8.hash(h);
        s.hash(h);
    } else if let Some(s) = v.as_sym() {
        5u8.hash(h);
        s.0.hash(h);
    } else if let Some(s) = v.error_message() {
        8u8.hash(h);
        s.hash(h);
    } else if v.is_cons() {
        6u8.hash(h);
        value_hash(v.head().unwrap(), h);
        value_hash(v.tail().unwrap(), h);
    } else if v.is_vec() {
        7u8.hash(h);
        for cell in v.vec_cells() {
            value_hash(&cell, h);
        }
    } else {
        // Closures, streams, foreign: hash by node identity (the tagged word).
        9u8.hash(h);
        v.to_gc().bits().hash(h);
    }
}
