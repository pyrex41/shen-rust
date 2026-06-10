//! Shared host + Cedar helpers for the Shen+Cedar authorization examples.
//!
//! `ShenHost` boots the Shen engine in served / VM mode (the `--served`
//! embedding) and exposes the marshalling surface the examples need. The free
//! functions wrap the Cedar schema (loaded from `authz.cedarschema`) and the
//! strict policy/entity validation the hardened examples run.

use std::rc::Rc;
use std::str::FromStr;

use cedar_policy::{Entities, PolicySet, Schema, ValidationMode, Validator};

use shen_rust::error::ShenError;
use shen_rust::interp::boot::boot;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::ast::KlExpr;
use shen_rust::kl::parser::{parse_all, parse_one};
use shen_rust::symbol::SymId;
use shen_rust::value::Value;

/// The Cedar schema, embedded at compile time — the contract both the
/// hand-written (`verify`) and generated (`generate`) policy sets validate
/// against.
pub const SCHEMA_SRC: &str = include_str!("../authz.cedarschema");

/// Parse the embedded Cedar schema.
pub fn schema() -> Result<Schema, String> {
    Schema::from_str(SCHEMA_SRC).map_err(|e| format!("schema parse: {e}"))
}

/// Strict-validate a policy set against the schema. Returns the list of
/// validation error strings (empty == valid).
pub fn validate_policies(set: &PolicySet, schema: &Schema) -> Vec<String> {
    let result = Validator::new(schema.clone()).validate(set, ValidationMode::Strict);
    result.validation_errors().map(|e| e.to_string()).collect()
}

/// Parse + schema-validate entities in one step (the JSON must conform).
pub fn entities(json: &str, schema: &Schema) -> Result<Entities, String> {
    Entities::from_json_str(json, Some(schema)).map_err(|e| format!("entities: {e}"))
}

/// The served Shen engine + the marshalling surface the examples use.
pub struct ShenHost {
    pub interp: Interp,
}

impl ShenHost {
    /// Boot the kernel with the bytecode VM enabled (served mode).
    pub fn new() -> Result<Self, String> {
        shen_rust::interp::eval::enable_vm();
        let mut interp = Interp::new();
        boot(&mut interp).map_err(|e| format!("shen boot: {e}"))?;
        Ok(Self { interp })
    }

    /// Evaluate one Shen source form through the kernel's own `eval`.
    pub fn eval(&mut self, src: &str) -> Result<Value, ShenError> {
        let expr = parse_one(src, &mut self.interp.symbols)
            .map_err(|e| ShenError::new(format!("parse: {e}")))?;
        self.eval_expr(&expr)
    }

    /// Load every top-level form in a source blob (e.g. a `.shen` spec file).
    pub fn load_source(&mut self, src: &str) -> Result<(), String> {
        let forms =
            parse_all(src, &mut self.interp.symbols).map_err(|e| format!("parse spec: {e}"))?;
        for form in &forms {
            self.eval_expr(form)
                .map_err(|e| format!("load form: {e}"))?;
        }
        Ok(())
    }

    fn eval_expr(&mut self, expr: &KlExpr) -> Result<Value, ShenError> {
        let eval_sym = self.interp.intern("eval");
        if self.interp.env.get_fn(eval_sym).is_some() {
            let quoted = klexpr_to_value(expr);
            let f = self.interp.env.get_fn(eval_sym).cloned().unwrap();
            self.interp.apply(f, vec![quoted])
        } else {
            self.interp.eval(expr)
        }
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

    /// A Shen string `Value`.
    pub fn string(&self, s: &str) -> Value {
        Value::str(Rc::<str>::from(s))
    }

    /// A Shen symbol `Value` (interns `name`).
    pub fn symbol(&mut self, name: &str) -> Value {
        Value::sym(self.interp.intern(name))
    }

    /// A Shen proper list from an iterator of `Value`s.
    pub fn list<I: IntoIterator<Item = Value>>(&self, items: I) -> Value
    where
        I::IntoIter: DoubleEndedIterator,
    {
        Value::list(items)
    }

    /// Resolve a symbol/string/int `Value` to Rust text.
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

    pub fn intern(&mut self, name: &str) -> SymId {
        self.interp.intern(name)
    }

    /// Opt-in AOT overlay: swap loaded defuns for a klcompile-emitted
    /// native module iff its manifest matches the live sources and the
    /// booted kernel (see `shen_rust::aot::overlay`). `live_src` must be
    /// the exact source text this host loaded, concatenated in load
    /// order. Returns whether the overlay installed; on any mismatch the
    /// loaded engine keeps serving — pure speed swap, never an error.
    pub fn install_aot_overlay(
        &mut self,
        module: &shen_rust::aot::overlay::OverlayModule,
        live_src: &str,
    ) -> bool {
        match shen_rust::interp::boot::find_kernel_dir() {
            Ok(dir) => self.interp.install_overlay_if_match(module, live_src, &dir),
            Err(_) => false,
        }
    }
}

/// Walk a Shen proper list into a `Vec<Value>` (`Value` is `Copy`).
///
/// GC note: if the embedding enables `SHEN_RUST_GC`, do NOT hold this `Vec`
/// across a later `eval`/`call` — a heap-allocated `Vec`'s elements are
/// invisible to the collector's stack scan. Either consume it first, or pin
/// the values via `Interp::gc_pins` for the duration (see the `value.rs`
/// "Collection" note).
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
