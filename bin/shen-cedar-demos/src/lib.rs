//! Shared host for the Shen+Cedar integration prototypes.
//!
//! `ShenHost` boots the Shen engine in served / VM mode (the `--served`
//! entrypoint shipped for long-running embeddings) and exposes the small
//! marshalling surface the demos need: evaluate a source line, call a named
//! Shen function with constructed `Value` arguments, and convert Shen lists
//! to/from Rust.

use std::rc::Rc;

use shen_cedar::error::ShenError;
use shen_cedar::interp::boot::boot;
use shen_cedar::interp::eval::Interp;
use shen_cedar::kl::ast::KlExpr;
use shen_cedar::kl::parser::parse_one;
use shen_cedar::value::Value;

pub struct ShenHost {
    pub interp: Interp,
}

impl ShenHost {
    /// Boot the kernel with the bytecode VM enabled (served mode).
    pub fn new() -> Result<Self, String> {
        shen_cedar::interp::eval::enable_vm();
        let mut interp = Interp::new();
        boot(&mut interp).map_err(|e| format!("shen boot: {e}"))?;
        Ok(Self { interp })
    }

    /// Evaluate one Shen source form through the kernel's own `eval`.
    pub fn eval(&mut self, src: &str) -> Result<Value, ShenError> {
        let expr = parse_one(src, &mut self.interp.symbols)
            .map_err(|e| ShenError::new(format!("parse: {e}")))?;
        let eval_sym = self.interp.intern("eval");
        if self.interp.env.get_fn(eval_sym).is_some() {
            let quoted = klexpr_to_value(&expr);
            let f = self.interp.env.get_fn(eval_sym).cloned().unwrap();
            self.interp.apply(f, vec![quoted])
        } else {
            self.interp.eval(&expr)
        }
    }

    /// Define every form (e.g. a block of `defun`s — the Shen "program").
    pub fn define_all(&mut self, forms: &[&str]) -> Result<(), String> {
        for f in forms {
            self.eval(f).map_err(|e| format!("define {f:?}: {e}"))?;
        }
        Ok(())
    }

    /// Call a named Shen function with already-built `Value` arguments.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, ShenError> {
        let s = self.interp.intern(name);
        let f = self
            .interp
            .env
            .get_fn(s)
            .cloned()
            .ok_or_else(|| ShenError::new(format!("no such fn: {name}")))?;
        self.interp.apply(f, args)
    }

    /// A Shen string `Value` from a Rust `&str`.
    pub fn string(&self, s: &str) -> Value {
        Value::str(Rc::<str>::from(s))
    }

    /// Resolve a symbol/string `Value` to its Rust text (for reading results).
    pub fn text(&self, v: &Value) -> String {
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
        if let Some(s) = v.as_sym() {
            return self.interp.resolve(s).to_string();
        }
        if let Some(n) = v.as_int() {
            return n.to_string();
        }
        "<value>".to_string()
    }
}

/// Walk a Shen proper list into a `Vec<Value>` (`Value` is `Copy`).
pub fn read_list(v: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cur = *v;
    while let (Some(h), Some(t)) = (cur.head(), cur.tail()) {
        out.push(*h);
        cur = *t;
    }
    out
}

pub fn klexpr_to_value(e: &KlExpr) -> Value {
    match e {
        KlExpr::Nil => Value::nil(),
        KlExpr::Bool(b) => Value::bool(*b),
        KlExpr::Int(n) => Value::int(*n),
        KlExpr::Float(x) => Value::float(*x),
        KlExpr::Str(s) => Value::str(s.clone()),
        KlExpr::Sym(s) => Value::sym(*s),
        KlExpr::App(items) => Value::list(items.iter().map(klexpr_to_value)),
    }
}
