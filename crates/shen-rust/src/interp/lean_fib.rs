//! Lean-boot fixture. `shen.initialise` and a non-defun top-level form both
//! call `bound?`, which klcompile lowers onto `value/or`, and they also call
//! `<-address/or` and `<-vector/or`. Those three names are not kernel defuns.
//! [`crate::interp::boot::boot_aot_slice`] has to register them before either
//! call, including a `--no-default-features` build.

use crate::aot::runtime as rt;
use crate::error::{ShenError, ShenResult};
use crate::interp::boot::boot_aot_slice;
use crate::interp::eval::{DirectFn, Interp};
use crate::value::Value;

fn reg(interp: &mut Interp, name: &str, arity: usize, f: DirectFn) {
    interp.register_native(name, arity, f);
    interp.register_aot_direct(name, f);
}

fn unbound_sentinel(interp: &mut Interp, _args: &[Value]) -> ShenResult<Value> {
    Ok(Value::sym(
        interp.intern_static("shen.this-symbol-is-unbound"),
    ))
}

fn false_handler(_interp: &mut Interp, _args: &[Value]) -> ShenResult<Value> {
    Ok(Value::bool(false))
}

fn zero_handler(_interp: &mut Interp, _args: &[Value]) -> ShenResult<Value> {
    Ok(Value::int(0))
}

/// AOT-shaped `bound?`: `(symbol? X)` then `value/or`.
fn aot_bound(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let is_sym = rt::apply_direct(interp, "symbol?", &[args[0]])?;
    if !rt::is_truthy(interp, &is_sym)? {
        return Ok(Value::bool(false));
    }
    let handler = rt::make_aot_closure("<freeze>", 0, unbound_sentinel, Vec::new(), interp);
    let got = rt::apply_direct(interp, "value/or", &[args[0], handler])?;
    let unbound = got.as_sym() == Some(interp.intern_static("shen.this-symbol-is-unbound"));
    Ok(Value::bool(!unbound))
}

fn aot_symbol(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let v = args[0];
    if v.as_bool().is_some() || v.is_number() || v.is_str() || v.is_cons() || v.is_nil() {
        return Ok(Value::bool(false));
    }
    let is_vec = rt::apply_direct(interp, "vector?", &[v])?;
    if rt::is_truthy(interp, &is_vec)? {
        return Ok(Value::bool(false));
    }
    Ok(Value::bool(v.as_sym().is_some()))
}

fn aot_vector(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    if !args[0].is_vec() {
        return Ok(Value::bool(false));
    }
    let handler = rt::make_aot_closure("<freeze>", 0, false_handler, Vec::new(), interp);
    let cell = rt::apply_direct(interp, "<-address/or", &[args[0], Value::int(0), handler])?;
    match cell.as_int() {
        Some(n) if n >= 0 => Ok(Value::bool(true)),
        _ => Ok(Value::bool(false)),
    }
}

fn aot_probe_vector(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let handler = rt::make_aot_closure("<freeze>", 0, zero_handler, Vec::new(), interp);
    rt::apply_direct(interp, "<-vector/or", &[args[0], Value::int(1), handler])
}

fn aot_initialise(interp: &mut Interp, _args: &[Value]) -> ShenResult<Value> {
    let home = Value::sym(interp.intern_static("*home-directory*"));
    let bound_home = rt::apply_direct(interp, "bound?", &[home])?;
    if !rt::is_truthy(interp, &bound_home)? {
        return Err(ShenError::new(
            "shen.initialise: *home-directory* was not bound",
        ));
    }
    let missing = Value::sym(interp.intern_static("not-bound-symbol"));
    let bound_missing = rt::apply_direct(interp, "bound?", &[missing])?;
    if rt::is_truthy(interp, &bound_missing)? {
        return Err(ShenError::new(
            "shen.initialise: unbound symbol reported as bound",
        ));
    }
    let cells = Value::absvector(vec![Value::int(3), Value::int(4)]);
    let is_vec = rt::apply_direct(interp, "vector?", &[cells])?;
    if !rt::is_truthy(interp, &is_vec)? {
        return Err(ShenError::new(
            "shen.initialise: vector? did not see an absvector",
        ));
    }
    let probed = rt::apply_direct(interp, "probe-vector-or", &[cells])?;
    if probed.as_int() != Some(4) {
        return Err(ShenError::new(format!(
            "shen.initialise: <-vector/or returned {probed:?}"
        )));
    }
    Ok(Value::sym(interp.intern_static("shen.ok")))
}

fn aot_fib(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let n = args[0]
        .as_int()
        .ok_or_else(|| ShenError::new("fib: not an integer"))?;
    if n <= 1 {
        return Ok(Value::int(n.max(0)));
    }
    let a = rt::apply_direct(interp, "fib", &[Value::int(n - 1)])?;
    let b = rt::apply_direct(interp, "fib", &[Value::int(n - 2)])?;
    let Some(x) = a.as_int() else {
        return Err(ShenError::new("fib: non-integer result"));
    };
    let Some(y) = b.as_int() else {
        return Err(ShenError::new("fib: non-integer result"));
    };
    Ok(Value::int(x + y))
}

fn install(interp: &mut Interp) {
    reg(interp, "bound?", 1, aot_bound);
    reg(interp, "symbol?", 1, aot_symbol);
    reg(interp, "vector?", 1, aot_vector);
    reg(interp, "probe-vector-or", 1, aot_probe_vector);
    reg(interp, "shen.initialise", 0, aot_initialise);
    reg(interp, "fib", 1, aot_fib);
}

fn toplevel(interp: &mut Interp) -> ShenResult<Value> {
    let home = Value::sym(interp.intern_static("*home-directory*"));
    rt::apply_direct(interp, "bound?", &[home])
}

/// Boot the lean slice and return `(fib 10)`.
pub fn fib10() -> ShenResult<i64> {
    let mut interp = Interp::new();
    boot_aot_slice(&mut interp, install, toplevel)?;
    let v = rt::apply_direct(&mut interp, "fib", &[Value::int(10)])?;
    v.as_int()
        .ok_or_else(|| ShenError::new(format!("fib 10: non-integer {v:?}")))
}
