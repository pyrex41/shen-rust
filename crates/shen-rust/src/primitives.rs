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
use crate::interp::eval::Interp;
use crate::value::{shen_eq, Stream, Value};

/// Register every primitive in the function namespace.
pub fn register_all(interp: &mut Interp) {
    register_core(interp);
    crate::cedar::primitives::register_all(interp);
}

/// Override kernel-level Shen functions with native Rust implementations
/// on hot paths. Per the upstream call-frequency table
/// (`gist.github.com/otabat/0ffb06fb7517fcd11906086fcc511bee`), the
/// per-test-suite call counts dwarf everything else: `element?` 12.6 M,
/// `shen.pvar?` 7 M, `shen.lazyderef` 3.5 M, `fail` 3.4 M. Inlining
/// these into Rust skips both the AOT trampoline and the kernel-level
/// `cond`/`absvector?` chain. Call **after** kernel boot so these
/// override the AOT-compiled defuns.
pub fn register_hot_overrides(interp: &mut Interp) {
    // The six overrides below shadow AOT-compiled kernel defuns that have
    // entries in the direct-dispatch table. They are plain `fn`s (not
    // capturing closures) so they can be dual-registered — register_native
    // for the closure world + register_aot_direct for the raw-pointer fast
    // path — otherwise the kernel's own AOT callers (prolog, typechecker,
    // the reader's load path) would dispatch past them via rt::apply_direct.
    interp.register_native("element?", 2, hot_element_p);
    interp.register_aot_direct("element?", hot_element_p);

    interp.register_native("shen.pvar?", 1, hot_pvar_p);
    interp.register_aot_direct("shen.pvar?", hot_pvar_p);

    interp.register_native("shen.lazyderef", 2, hot_lazyderef);
    interp.register_aot_direct("shen.lazyderef", hot_lazyderef);

    interp.register_native("fail", 0, hot_fail);
    interp.register_aot_direct("fail", hot_fail);

    // value/or — read a global, calling a frozen default on miss
    // instead of erroring. Native because the kernel's version routes
    // through trap-error, which is expensive.
    interp.register_native("value/or", 2, |interp, args| {
        let sym = match args[0].as_sym() {
            Some(s) => s,
            None => {
                return Err(ShenError::new(format!(
                    "value/or: arg 1 not a symbol: {:?}",
                    args[0]
                )))
            }
        };
        if let Some(v) = interp.env.get_global(sym).cloned() {
            Ok(v)
        } else {
            // arg 2 is a frozen thunk: apply with no args.
            interp.apply(args[1], vec![])
        }
    });

    // <-address/or — same idea but for absvector slot access.
    interp.register_native("<-address/or", 3, |interp, args| {
        if let Some(i) = args[1].as_int() {
            if let Some(cell) = args[0].vec_get_opt(i as usize) {
                return Ok(cell);
            }
        }
        interp.apply(args[2], vec![])
    });

    // read-file-as-bytelist / read-file-as-string — bulk file read
    // instead of byte-by-byte through `read-byte`. The kernel's
    // implementation is a recursive cons-builder; on a large source
    // file this dominates load time.
    interp.register_native("read-file-as-bytelist", 1, hot_read_file_as_bytelist);
    interp.register_aot_direct("read-file-as-bytelist", hot_read_file_as_bytelist);

    interp.register_native("read-file-as-string", 1, hot_read_file_as_string);
    interp.register_aot_direct("read-file-as-string", hot_read_file_as_string);
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
        Some(s) => {
            let id = interp.intern(s);
            Ok(Value::sym(id))
        }
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
                if n < 0 || (n as usize) >= s.len() {
                    return Err(ShenError::new("pos: index out of range"));
                }
                let byte = s.as_bytes()[n as usize];
                Ok(Value::str(String::from(byte as char)))
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
            // Operate on bytes — matches shen-cl semantics where Shen
            // strings are byte strings.
            let rest = &s.as_bytes()[1..];
            let rest_str = std::str::from_utf8(rest)
                .map_err(|_| ShenError::new("tlstr: produced non-UTF-8 result"))?;
            Ok(Value::str(rest_str))
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
        Some(s) => {
            let b = s.as_bytes();
            if b.is_empty() {
                return Err(ShenError::new("string->n: empty string"));
            }
            Ok(Value::int(b[0] as i64))
        }
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
    interp.register_native("absvector", 1, |_, args| match args[0].as_int() {
        Some(n) if n >= 0 => {
            let len = n as usize;
            let cells = vec![Value::sym(crate::symbol::SymId(0)); len];
            // Will be overwritten with an "uninitialized" sentinel in
            // boot.rs once the kernel interns `shen.fail!`. For now we
            // just zero-init with whatever interned id 0 is (`true` per
            // WellKnown ordering — harmless for the kernel).
            Ok(Value::absvector(cells))
        }
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
    interp.register_native("hash", 2, |_, args| match args[1].as_int() {
        Some(buckets) => {
            let mut h = DefaultHasher::new();
            value_hash(&args[0], &mut h);
            let buckets = buckets.max(1) as u64;
            let v = (h.finish() % buckets) as i64 + 1;
            Ok(Value::int(v))
        }
        None => Err(ShenError::new(format!(
            "hash: bad args: {:?}, {:?}",
            args[0], args[1]
        ))),
    });
    interp.register_native("apply", 2, |interp, args| {
        let f = args[0];
        let argv = list_to_vec(&args[1])
            .ok_or_else(|| ShenError::new(format!("apply: not a list: {:?}", args[1])))?;
        interp.apply(f, argv)
    });

    // --- I/O streams ---
    interp.register_native("open", 2, |_, args| {
        match (args[0].as_str(), args[1].is_sym()) {
            (Some(path), true) => {
                // Direction is encoded in the mode symbol (`in` / `out`).
                // We accept the symbol name only — minimal v0 surface.
                //
                // We don't have interp here, so we can't resolve the symbol
                // name. Use a heuristic: if path exists, open read; else open
                // write. Real semantics come once we route interp through.
                if let Ok(f) = std::fs::File::open(path) {
                    let stream = Stream::In(Box::new(f) as Box<dyn Read>);
                    Ok(Value::stream(Rc::new(RefCell::new(stream))))
                } else {
                    let f = std::fs::File::create(path)
                        .map_err(|e| ShenError::new(format!("open: {e}")))?;
                    let stream = Stream::Out(Box::new(f) as Box<dyn std::io::Write>);
                    Ok(Value::stream(Rc::new(RefCell::new(stream))))
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
fn format_float(x: f64) -> String {
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
    // Hash a small kind discriminant first, then the structural contents — the
    // post-flip `Value` is a `Copy` word with no enum discriminant.
    if v.is_nil() {
        0u8.hash(h);
    } else if let Some(b) = v.as_bool() {
        1u8.hash(h);
        b.hash(h);
    } else if let Some(n) = v.as_int() {
        2u8.hash(h);
        n.hash(h);
    } else if let Some(x) = v.as_float() {
        3u8.hash(h);
        x.to_bits().hash(h);
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
