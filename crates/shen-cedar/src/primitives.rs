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
    let k_pvar = interp.well_known.k_shen_pvar;
    let k_null = interp.well_known.k_shen_null;
    let k_fail = interp.well_known.k_shen_fail;

    // element? — linear scan of a proper list with shen_eq.
    interp.register_native("element?", 2, |_, args| {
        let target = &args[0];
        let mut cur: Value = args[1].clone();
        loop {
            match cur {
                Value::Nil => return Ok(Value::bool(false)),
                Value::Cons(ref p) => {
                    if shen_eq(target, &p.0) {
                        return Ok(Value::bool(true));
                    }
                    cur = p.1.clone();
                }
                _ => return Err(ShenError::new(format!("element?: not a list: {cur:?}"))),
            }
        }
    });

    // shen.pvar? — Shen Prolog logic variable check. The kernel
    // representation is an absvector whose slot 0 is the symbol
    // `shen.pvar`.
    interp.register_native("shen.pvar?", 1, move |_, args| {
        Ok(Value::bool(match &args[0] {
            Value::Vec(v) => {
                matches!(v.borrow().first(), Some(Value::Sym(s)) if *s == k_pvar)
            }
            _ => false,
        }))
    });

    // shen.lazyderef — chase Prolog-variable bindings through the
    // *prolog-vector*. The kernel implementation recurses through
    // `shen.lazyderef` itself; we tightloop in Rust.
    interp.register_native("shen.lazyderef", 2, move |_, args| {
        let vec = match &args[1] {
            Value::Vec(v) => v.clone(),
            _ => {
                return Err(ShenError::new(format!(
                    "shen.lazyderef: arg 2 not a vector: {:?}",
                    args[1]
                )))
            }
        };
        let mut cur = args[0].clone();
        loop {
            // Is `cur` a pvar?
            let idx: i64 = if let Value::Vec(v) = &cur {
                let b = v.borrow();
                let is_pvar = matches!(b.first(), Some(Value::Sym(s)) if *s == k_pvar);
                if !is_pvar {
                    drop(b);
                    return Ok(cur);
                }
                match b.get(1) {
                    Some(Value::Int(i)) => *i,
                    _ => {
                        drop(b);
                        return Ok(cur);
                    }
                }
            } else {
                return Ok(cur);
            };
            // Look up its binding in *prolog-vector*.
            let next = {
                let b = vec.borrow();
                b.get(idx as usize).cloned().unwrap_or(Value::nil())
            };
            // Unbound → return the pvar itself.
            if matches!(&next, Value::Sym(s) if *s == k_null) {
                return Ok(cur);
            }
            cur = next;
        }
    });

    // fail — return the magic shen.fail! symbol. Kernel-level
    // `fail` is just `(defun fail () shen.fail!)`.
    interp.register_native("fail", 0, move |_, _args| Ok(Value::sym(k_fail)));

    // value/or — read a global, calling a frozen default on miss
    // instead of erroring. Native because the kernel's version routes
    // through trap-error, which is expensive.
    interp.register_native("value/or", 2, |interp, args| {
        let sym = match &args[0] {
            Value::Sym(s) => *s,
            other => {
                return Err(ShenError::new(format!(
                    "value/or: arg 1 not a symbol: {other:?}"
                )))
            }
        };
        if let Some(v) = interp.env.get_global(sym).cloned() {
            Ok(v)
        } else {
            // arg 2 is a frozen thunk: apply with no args.
            interp.apply(args[1].clone(), vec![])
        }
    });

    // <-address/or — same idea but for absvector slot access.
    interp.register_native("<-address/or", 3, |interp, args| {
        if let (Value::Vec(v), Value::Int(i)) = (&args[0], &args[1]) {
            if let Some(cell) = v.borrow().get(*i as usize).cloned() {
                return Ok(cell);
            }
        }
        interp.apply(args[2].clone(), vec![])
    });

    // read-file-as-bytelist / read-file-as-string — bulk file read
    // instead of byte-by-byte through `read-byte`. The kernel's
    // implementation is a recursive cons-builder; on a large source
    // file this dominates load time.
    interp.register_native("read-file-as-bytelist", 1, |_, args| {
        let path = match &args[0] {
            Value::Str(s) => s.to_string(),
            other => {
                return Err(ShenError::new(format!(
                    "read-file-as-bytelist: not a string: {other:?}"
                )))
            }
        };
        let bytes = std::fs::read(&path)
            .map_err(|e| ShenError::new(format!("read-file-as-bytelist: {path}: {e}")))?;
        Ok(bytes_to_list(&bytes))
    });
    interp.register_native("read-file-as-string", 1, |_, args| {
        let path = match &args[0] {
            Value::Str(s) => s.to_string(),
            other => {
                return Err(ShenError::new(format!(
                    "read-file-as-string: not a string: {other:?}"
                )))
            }
        };
        let bytes = std::fs::read(&path)
            .map_err(|e| ShenError::new(format!("read-file-as-string: {path}: {e}")))?;
        // Kernel semantics: bytes interpreted as a string verbatim.
        let s = String::from_utf8_lossy(&bytes).into_owned();
        Ok(Value::str(Rc::from(s.as_str())))
    });
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
    interp.register_native("number?", 1, |_, args| {
        Ok(Value::bool(matches!(
            &args[0],
            Value::Int(_) | Value::Float(_)
        )))
    });
    interp.register_native("string?", 1, |_, args| {
        Ok(Value::bool(matches!(&args[0], Value::Str(_))))
    });
    interp.register_native("symbol?", 1, |_, args| {
        Ok(Value::bool(matches!(&args[0], Value::Sym(_))))
    });
    interp.register_native("boolean?", 1, |interp, args| {
        let wk = &interp.well_known;
        let b = match &args[0] {
            Value::Bool(_) => true,
            Value::Sym(s) => *s == wk.k_true || *s == wk.k_false,
            _ => false,
        };
        Ok(Value::bool(b))
    });
    interp.register_native("cons?", 1, |_, args| {
        Ok(Value::bool(matches!(&args[0], Value::Cons(_))))
    });
    interp.register_native("absvector?", 1, |_, args| {
        Ok(Value::bool(matches!(&args[0], Value::Vec(_))))
    });
    // Backward-compat alias used in some kernel code paths.
    interp.register_native("vector?", 1, |_, args| {
        Ok(Value::bool(matches!(&args[0], Value::Vec(_))))
    });

    // --- lists ---
    interp.register_native("cons", 2, |_, args| {
        Ok(Value::cons(args[0].clone(), args[1].clone()))
    });
    interp.register_native("hd", 1, |_, args| match &args[0] {
        Value::Cons(p) => Ok(p.0.clone()),
        other => Err(ShenError::new(format!("hd: not a cons: {other:?}"))),
    });
    interp.register_native("tl", 1, |_, args| match &args[0] {
        Value::Cons(p) => Ok(p.1.clone()),
        other => Err(ShenError::new(format!("tl: not a cons: {other:?}"))),
    });

    // --- symbols / strings ---
    interp.register_native("intern", 1, |interp, args| match &args[0] {
        Value::Str(s) => {
            let id = interp.intern(s);
            Ok(Value::sym(id))
        }
        other => Err(ShenError::new(format!("intern: not a string: {other:?}"))),
    });
    interp.register_native("str", 1, |interp, args| {
        Ok(Value::str(value_to_str(interp, &args[0])))
    });
    interp.register_native("cn", 2, |_, args| match (&args[0], &args[1]) {
        (Value::Str(a), Value::Str(b)) => {
            let mut s = String::with_capacity(a.len() + b.len());
            s.push_str(a);
            s.push_str(b);
            Ok(Value::str(s))
        }
        (a, b) => Err(ShenError::new(format!(
            "cn: strings only, got {a:?} and {b:?}"
        ))),
    });
    interp.register_native("pos", 2, |_, args| match (&args[0], &args[1]) {
        (Value::Str(s), Value::Int(i)) => {
            let n = *i;
            if n < 0 || (n as usize) >= s.len() {
                return Err(ShenError::new("pos: index out of range"));
            }
            let byte = s.as_bytes()[n as usize];
            Ok(Value::str(String::from(byte as char)))
        }
        (a, b) => Err(ShenError::new(format!("pos: bad args: {a:?}, {b:?}"))),
    });
    interp.register_native("tlstr", 1, |_, args| match &args[0] {
        Value::Str(s) => {
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
        other => Err(ShenError::new(format!("tlstr: not a string: {other:?}"))),
    });
    interp.register_native("n->string", 1, |_, args| match &args[0] {
        Value::Int(n) => {
            let c = char::from_u32(*n as u32)
                .ok_or_else(|| ShenError::new(format!("n->string: bad codepoint {n}")))?;
            Ok(Value::str(String::from(c)))
        }
        other => Err(ShenError::new(format!("n->string: not an int: {other:?}"))),
    });
    interp.register_native("string->n", 1, |_, args| match &args[0] {
        Value::Str(s) => {
            let b = s.as_bytes();
            if b.is_empty() {
                return Err(ShenError::new("string->n: empty string"));
            }
            Ok(Value::int(b[0] as i64))
        }
        other => Err(ShenError::new(format!(
            "string->n: not a string: {other:?}"
        ))),
    });

    // --- function lookup ---
    interp.register_native("fn", 1, |interp, args| match &args[0] {
        Value::Sym(s) => interp
            .env
            .get_fn(*s)
            .cloned()
            .ok_or_else(|| ShenError::new(format!("fn: undefined: {}", interp.resolve(*s)))),
        other => Err(ShenError::new(format!("fn: not a symbol: {other:?}"))),
    });

    // --- globals (dual namespace `set`/`value`) ---
    interp.register_native("set", 2, |interp, args| match &args[0] {
        Value::Sym(s) => {
            interp.env.set_global(*s, args[1].clone());
            Ok(args[1].clone())
        }
        other => Err(ShenError::new(format!("set: not a symbol: {other:?}"))),
    });
    interp.register_native("value", 1, |interp, args| match &args[0] {
        Value::Sym(s) => interp.env.get_global(*s).cloned().ok_or_else(|| {
            ShenError::new(format!("value: unbound global: {}", interp.resolve(*s)))
        }),
        other => Err(ShenError::new(format!("value: not a symbol: {other:?}"))),
    });

    // --- vectors (absvectors) ---
    interp.register_native("absvector", 1, |_, args| match &args[0] {
        Value::Int(n) if *n >= 0 => {
            let len = *n as usize;
            let cells = vec![Value::sym(crate::symbol::SymId(0)); len];
            // Will be overwritten with an "uninitialized" sentinel in
            // boot.rs once the kernel interns `shen.fail!`. For now we
            // just zero-init with whatever interned id 0 is (`true` per
            // WellKnown ordering — harmless for the kernel).
            Ok(Value::Vec(Rc::new(RefCell::new(cells))))
        }
        other => Err(ShenError::new(format!("absvector: bad arg: {other:?}"))),
    });
    interp.register_native("<-address", 2, |_, args| match (&args[0], &args[1]) {
        (Value::Vec(v), Value::Int(i)) => {
            let idx = *i as usize;
            let cell = v.borrow().get(idx).cloned();
            cell.ok_or_else(|| ShenError::new(format!("<-address: out of range {i}")))
        }
        (a, b) => Err(ShenError::new(format!("<-address: bad args: {a:?}, {b:?}"))),
    });
    interp.register_native("address->", 3, |_, args| match (&args[0], &args[1]) {
        (Value::Vec(v), Value::Int(i)) => {
            let idx = *i as usize;
            let mut borrow = v.borrow_mut();
            if idx >= borrow.len() {
                return Err(ShenError::new(format!("address->: out of range {i}")));
            }
            borrow[idx] = args[2].clone();
            drop(borrow);
            // Returns the vector itself (so it can be chained).
            Ok(args[0].clone())
        }
        (a, b) => Err(ShenError::new(format!("address->: bad args: {a:?}, {b:?}"))),
    });

    // --- errors ---
    interp.register_native("simple-error", 1, |_, args| match &args[0] {
        Value::Str(s) => Err(ShenError::new(s.as_ref())),
        other => Err(ShenError::new(format!("{other:?}"))),
    });
    interp.register_native("error-to-string", 1, |_, args| match &args[0] {
        Value::Error(s) => Ok(Value::str(s.clone())),
        other => Err(ShenError::new(format!(
            "error-to-string: not an error: {other:?}"
        ))),
    });

    // --- meta ---
    interp.register_native("eval-kl", 1, |interp, args| {
        // KL semantics: numbers, strings, booleans, streams, closures,
        // absvectors, and other non-list values are self-evaluating.
        // Only `Cons` (= a syntactic application) and `Sym` need to be
        // converted back into a `KlExpr` and run through `eval`.
        match &args[0] {
            Value::Cons(_) | Value::Sym(_) => {
                let expr = value_to_klexpr(&args[0])?;
                interp.eval(&expr)
            }
            other => Ok(other.clone()),
        }
    });
    interp.register_native("type", 2, |_, args| {
        // (type X T) — at runtime we ignore the annotation and return X.
        Ok(args[0].clone())
    });
    interp.register_native("tc?", 0, |_, _| Ok(Value::bool(false)));
    interp.register_native("tc", 1, |_, args| Ok(args[0].clone()));
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
    interp.register_native("hash", 2, |_, args| match (&args[0], &args[1]) {
        (key, Value::Int(buckets)) => {
            let mut h = DefaultHasher::new();
            value_hash(key, &mut h);
            let buckets = (*buckets).max(1) as u64;
            let v = (h.finish() % buckets) as i64 + 1;
            Ok(Value::int(v))
        }
        (a, b) => Err(ShenError::new(format!("hash: bad args: {a:?}, {b:?}"))),
    });
    interp.register_native("apply", 2, |interp, args| {
        let f = args[0].clone();
        let argv = list_to_vec(&args[1])
            .ok_or_else(|| ShenError::new(format!("apply: not a list: {:?}", args[1])))?;
        interp.apply(f, argv)
    });

    // --- I/O streams ---
    interp.register_native("open", 2, |_, args| match (&args[0], &args[1]) {
        (Value::Str(path), Value::Sym(_mode)) => {
            // Direction is encoded in the mode symbol (`in` / `out`).
            // We accept the symbol name only — minimal v0 surface.
            let path = path.as_ref();
            let mode_sym = match &args[1] {
                Value::Sym(s) => *s,
                _ => unreachable!(),
            };
            // We don't have interp here, so we can't resolve the symbol
            // name. Wire through with a closure that captures interp.
            // For now require literal symbol id comparison via a hack:
            // accept any symbol whose name is "in" or "out" by trying a
            // path-existence/open dispatch.
            let _ = mode_sym;
            // Use a heuristic: if path exists, open read; else open write.
            // Real semantics come once we route interp through.
            if let Ok(f) = std::fs::File::open(path) {
                let stream = Stream::In(Box::new(f) as Box<dyn Read>);
                Ok(Value::Stream(Rc::new(RefCell::new(stream))))
            } else {
                let f = std::fs::File::create(path)
                    .map_err(|e| ShenError::new(format!("open: {e}")))?;
                let stream = Stream::Out(Box::new(f) as Box<dyn std::io::Write>);
                Ok(Value::Stream(Rc::new(RefCell::new(stream))))
            }
        }
        (a, b) => Err(ShenError::new(format!("open: bad args: {a:?}, {b:?}"))),
    });
    interp.register_native("close", 1, |_, args| match &args[0] {
        Value::Stream(s) => {
            *s.borrow_mut() = Stream::Closed;
            Ok(Value::sym(crate::symbol::SymId(0)))
        }
        other => Err(ShenError::new(format!("close: not a stream: {other:?}"))),
    });
    interp.register_native("read-byte", 1, |_, args| match &args[0] {
        Value::Stream(s) => {
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
        other => Err(ShenError::new(format!(
            "read-byte: not a stream: {other:?}"
        ))),
    });
    // Stream-flavour probes the kernel's `pr` / reader rely on. Our
    // streams are byte-oriented (read-byte/write-byte), so report
    // `false` for both. shen-cl maps these to lisp `subtypep` of the
    // stream element type; for shen-cedar there's only one type.
    interp.register_native("shen.char-stoutput?", 1, |_, _args| Ok(Value::bool(false)));
    interp.register_native("shen.char-stinput?", 1, |_, _args| Ok(Value::bool(false)));

    interp.register_native("write-byte", 2, |_, args| match (&args[0], &args[1]) {
        (Value::Int(b), Value::Stream(s)) => {
            let mut s = s.borrow_mut();
            match &mut *s {
                Stream::Out(w) => {
                    let buf = [*b as u8];
                    w.write_all(&buf)
                        .map_err(|e| ShenError::new(format!("write-byte: {e}")))?;
                    Ok(Value::int(*b))
                }
                _ => Err(ShenError::new("write-byte: not an output stream")),
            }
        }
        (a, b) => Err(ShenError::new(format!(
            "write-byte: bad args: {a:?}, {b:?}"
        ))),
    });
}

// --- helpers ---

fn numeric_op<F, I>(args: &[Value], name: &str, f_op: F, i_op: I) -> ShenResult<Value>
where
    F: Fn(f64, f64) -> f64,
    I: Fn(i64, i64) -> Option<i64>,
{
    match (&args[0], &args[1]) {
        (Value::Int(a), Value::Int(b)) => match i_op(*a, *b) {
            Some(v) => Ok(Value::int(v)),
            None => Ok(Value::float(f_op(*a as f64, *b as f64))),
        },
        (Value::Int(a), Value::Float(b)) => Ok(Value::Float(f_op(*a as f64, *b))),
        (Value::Float(a), Value::Int(b)) => Ok(Value::Float(f_op(*a, *b as f64))),
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(f_op(*a, *b))),
        (a, b) => Err(ShenError::new(format!("{name}: bad args: {a:?}, {b:?}"))),
    }
}

fn prim_div(_: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let (a, b) = match (&args[0], &args[1]) {
        (Value::Int(a), Value::Int(b)) => (*a as f64, *b as f64),
        (Value::Int(a), Value::Float(b)) => (*a as f64, *b),
        (Value::Float(a), Value::Int(b)) => (*a, *b as f64),
        (Value::Float(a), Value::Float(b)) => (*a, *b),
        (x, y) => return Err(ShenError::new(format!("/: bad args: {x:?}, {y:?}"))),
    };
    if b == 0.0 {
        return Err(ShenError::new("/: division by zero"));
    }
    let r = a / b;
    // Return Int when the result is exact and both inputs are integers.
    if matches!(&args[0], Value::Int(_)) && matches!(&args[1], Value::Int(_)) && r.fract() == 0.0 {
        Ok(Value::int(r as i64))
    } else {
        Ok(Value::float(r))
    }
}

fn compare_op<P>(args: &[Value], name: &str, pred: P) -> ShenResult<Value>
where
    P: Fn(std::cmp::Ordering) -> bool,
{
    let ord_opt = match (&args[0], &args[1]) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (a, b) => return Err(ShenError::new(format!("{name}: bad args: {a:?}, {b:?}"))),
    };
    let ord = ord_opt.ok_or_else(|| ShenError::new(format!("{name}: NaN comparison")))?;
    Ok(Value::bool(pred(ord)))
}

/// Convert a Shen value to its `str` representation. Mirrors shen-cl's
/// printer for the small set of atoms we expose at this phase.
fn value_to_str(interp: &Interp, v: &Value) -> String {
    match v {
        Value::Nil => "()".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) => format_float(*x),
        Value::Str(s) => format!("\"{s}\""),
        Value::Sym(s) => interp.resolve(*s).to_string(),
        Value::Cons(_) => format!("{v:?}"),
        // Use the Common-Lisp "unreadable object" convention `#<... ...>`,
        // with at least one space inside. The kernel's `symbol?` falls
        // back to parsing the result of `str`; its `shen.analyse-symbol?`
        // accepts the leading `#` (it's in `shen.misc?`), so the
        // disqualifier is the embedded whitespace — `shen.alphanums?`
        // rejects space-containing strings, so a closure can't pose as a
        // symbol. Without the space, `symbol?` returns true for closures
        // and the spreadsheet test (and any `(or (number? V) (symbol? V)
        // ...)` guard pattern) breaks.
        Value::Vec(_) => "#<absvector x>".to_string(),
        Value::Closure(_) => "#<closure x>".to_string(),
        Value::Stream(_) => "#<stream x>".to_string(),
        Value::Error(s) => format!("#<error {s}>"),
        Value::Foreign(_) => "#<foreign x>".to_string(),
    }
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
    let mut cur = v.clone();
    loop {
        match cur {
            Value::Nil => return Some(out),
            Value::Cons(p) => {
                out.push(p.0.clone());
                cur = p.1.clone();
            }
            _ => return None,
        }
    }
}

/// Convert a Shen value (list-form code) into a `KlExpr` AST. Used by
/// `eval-kl`.
fn value_to_klexpr(v: &Value) -> ShenResult<crate::kl::ast::KlExpr> {
    use crate::kl::ast::KlExpr;
    Ok(match v {
        Value::Nil => KlExpr::Nil,
        Value::Bool(b) => KlExpr::Bool(*b),
        Value::Int(n) => KlExpr::Int(*n),
        Value::Float(x) => KlExpr::Float(*x),
        Value::Str(s) => KlExpr::Str(s.clone()),
        Value::Sym(s) => KlExpr::Sym(*s),
        Value::Cons(_) => {
            let items = list_to_vec(v).ok_or_else(|| ShenError::new("eval-kl: improper list"))?;
            let mut elems = Vec::with_capacity(items.len());
            for it in items {
                elems.push(value_to_klexpr(&it)?);
            }
            KlExpr::App(elems.into())
        }
        other => {
            return Err(ShenError::new(format!(
                "eval-kl: cannot convert {other:?} to KlExpr"
            )))
        }
    })
}

fn value_hash<H: Hasher>(v: &Value, h: &mut H) {
    use Value::*;
    std::mem::discriminant(v).hash(h);
    match v {
        Nil => {}
        Bool(b) => b.hash(h),
        Int(n) => n.hash(h),
        Float(x) => x.to_bits().hash(h),
        Str(s) => s.hash(h),
        Sym(s) => s.0.hash(h),
        Cons(p) => {
            value_hash(&p.0, h);
            value_hash(&p.1, h);
        }
        Vec(v) => {
            for cell in v.borrow().iter() {
                value_hash(cell, h);
            }
        }
        Closure(c) => (Rc::as_ptr(c) as usize).hash(h),
        Stream(s) => (Rc::as_ptr(s) as usize).hash(h),
        Error(s) => s.hash(h),
        Foreign(f) => (Rc::as_ptr(f) as *const () as usize).hash(h),
    }
}
