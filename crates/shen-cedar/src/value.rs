//! Shen runtime value.
//!
//! Mirrors `shen-ocaml/src/runtime/value.ml`, adapted for Rust:
//!
//! * `Rc` for shared structural sharing (single-threaded runtime).
//! * `SymId` for symbols, so equality is O(1) from day one.
//! * `Closure` carries arity for static partial-application checks.
//! * `Foreign` wraps Cedar handles (and any future host objects).

use std::any::Any;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use crate::kl::ast::KlExpr;
use crate::symbol::SymId;

/// Mutable absvector. Shen absvectors are 0-indexed, fixed-length on
/// creation, and mutable per-cell via `(address-> v i x)`.
pub type AbsVec = Rc<RefCell<Vec<Value>>>;

/// I/O stream. Read or write end of a Shen open stream.
pub enum Stream {
    In(Box<dyn std::io::Read>),
    Out(Box<dyn std::io::Write>),
    /// Closed sentinel — Shen's `close` leaves the value as-is, future
    /// operations should error.
    Closed,
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stream::In(_) => f.write_str("Stream::In(..)"),
            Stream::Out(_) => f.write_str("Stream::Out(..)"),
            Stream::Closed => f.write_str("Stream::Closed"),
        }
    }
}

pub type SharedStream = Rc<RefCell<Stream>>;

/// A Shen-level callable. Either a primitive backed by a Rust closure or a
/// user-defined lambda compiled from KL.
pub struct Closure {
    pub name: Option<SymId>,
    pub arity: usize,
    /// Already-supplied args (for partial application).
    pub partial: Vec<Value>,
    pub kind: ClosureKind,
}

impl fmt::Debug for Closure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Closure")
            .field("name", &self.name)
            .field("arity", &self.arity)
            .field("partial", &self.partial.len())
            .finish_non_exhaustive()
    }
}

/// How a closure executes when it has enough arguments.
pub enum ClosureKind {
    /// Native Rust primitive. Receives the full argument vector
    /// (`partial` already prepended).
    Native(Rc<NativeFn>),
    /// User-defined lambda. The actual body lives in the KL AST; the
    /// interpreter knows how to evaluate it. Stored as `Rc` so cloning a
    /// closure value is cheap.
    Lambda(Rc<LambdaBody>),
    /// User-defined function compiled to bytecode (the VM path). Upvals
    /// are captured at closure creation (`MakeClosure`) and stored
    /// by-value, matching shen-go's `scmBytecodeFunc { fn, upvals }`.
    Bytecode(Rc<crate::vm::bytecode::BytecodeFn>, Vec<Value>),
}

/// User-defined lambda body: captured lexical env, formal parameter
/// symbols, and the KL expression to evaluate.
#[derive(Debug)]
pub struct LambdaBody {
    pub captured: Vec<(SymId, Value)>,
    pub params: Vec<SymId>,
    pub body: KlExpr,
}

/// Native primitive signature. The interpreter is the first argument so
/// primitives can intern symbols, look up other functions, etc.
pub type NativeFn =
    dyn Fn(&mut crate::interp::eval::Interp, &[Value]) -> crate::error::ShenResult<Value>;

/// Shen runtime value.
#[derive(Clone)]
pub enum Value {
    /// `()` — the empty list. Distinct from `false` and the symbol `nil`.
    Nil,
    Bool(bool),
    /// Shen numbers are tagged by representation. Operations promote
    /// `Int -> Float` as needed.
    Int(i64),
    Float(f64),
    Str(Rc<str>),
    Sym(SymId),
    /// A cons cell. Head and tail share a single heap allocation — the kernel
    /// is list-processing end to end, so the per-cell allocation/refcount
    /// traffic is the largest single malloc cost. The allocation strategy
    /// lives behind [`ConsCell`] (`crate::cons`) so it can be changed without
    /// touching call sites; see `design/value-representation.md`.
    Cons(crate::cons::ConsCell),
    Vec(AbsVec),
    Closure(Rc<Closure>),
    Stream(SharedStream),
    /// Error object — a trapped error, accessible from a `trap-error`
    /// handler. `error-to-string` extracts the message.
    Error(Rc<str>),
    /// Host-language opaque value, used for Cedar handles in Phase 3.
    Foreign(Rc<dyn Any>),
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Nil => f.write_str("Nil"),
            Value::Bool(b) => write!(f, "Bool({b})"),
            Value::Int(n) => write!(f, "Int({n})"),
            Value::Float(x) => write!(f, "Float({x})"),
            Value::Str(s) => write!(f, "Str({s:?})"),
            Value::Sym(s) => write!(f, "Sym({})", s.0),
            Value::Cons(_) => f.write_str("Cons(..)"),
            Value::Vec(v) => write!(f, "Vec(len={})", v.borrow().len()),
            Value::Closure(c) => write!(f, "Closure(arity={})", c.arity),
            Value::Stream(_) => f.write_str("Stream(..)"),
            Value::Error(s) => write!(f, "Error({s:?})"),
            Value::Foreign(_) => f.write_str("Foreign(..)"),
        }
    }
}

impl Value {
    // ---- Constructors ------------------------------------------------------
    //
    // Every `Value` should be built through one of these rather than naming the
    // enum variant directly. They are the stable seam for the word-sized repr
    // flip (see `design/gc-conversion-handoff.md`): when `Value` becomes a
    // tagged word over `Gc<T>`, only these bodies change — call sites don't.
    // All are `#[inline]`, so routing construction through them is zero-cost.

    /// The empty list `()`. Distinct from `false` and the symbol `nil`.
    #[inline]
    pub fn nil() -> Value {
        Value::Nil
    }

    /// A boolean. (Note: the kernel sometimes uses the symbols `true`/`false`
    /// instead; see [`shen_eq`] for the cross-equate rule.)
    #[inline]
    pub fn bool(b: bool) -> Value {
        Value::Bool(b)
    }

    /// An integer. Today backed by `i64`; after the repr flip this becomes a
    /// fixnum with a boxed-wide overflow path.
    #[inline]
    pub fn int(n: i64) -> Value {
        Value::Int(n)
    }

    /// A floating-point number.
    #[inline]
    pub fn float(x: f64) -> Value {
        Value::Float(x)
    }

    /// A symbol, identified by its interned [`SymId`].
    #[inline]
    pub fn sym(id: SymId) -> Value {
        Value::Sym(id)
    }

    /// A string. Accepts anything convertible into the shared `Rc<str>`
    /// backing (`&str`, `String`, `Rc<str>`).
    #[inline]
    pub fn str(s: impl Into<Rc<str>>) -> Value {
        Value::Str(s.into())
    }

    /// An error object (a trapped error message).
    #[inline]
    pub fn err(msg: impl Into<Rc<str>>) -> Value {
        Value::Error(msg.into())
    }

    /// Construct a cons cell from head and tail in a single allocation.
    #[inline]
    pub fn cons(head: Value, tail: Value) -> Value {
        Value::Cons(crate::cons::ConsCell::new(head, tail))
    }

    /// Build a proper list from an iterator. Trailing `Nil`.
    pub fn list<I: IntoIterator<Item = Value>>(items: I) -> Value
    where
        I::IntoIter: DoubleEndedIterator,
    {
        let mut acc = Value::Nil;
        for v in items.into_iter().rev() {
            acc = Value::cons(v, acc);
        }
        acc
    }

    // ---- Inspectors --------------------------------------------------------
    //
    // Non-destructuring inspection should go through these rather than ad-hoc
    // `matches!`/`if let`. Like the constructors, they are the stable seam for
    // the repr flip: today they match the enum; after the flip they become
    // tag-checks + accessors with the same signatures. The `head`/`tail`
    // accessors return `&Value` borrowed from `&self` — sound today (into the
    // `Rc` cell) and after the flip (the non-moving GC pins the cell, so the
    // reference stays valid while `self` is reachable).
    //
    // Destructuring `match` arms that bind inner data are intentionally left
    // naming the enum for now; Step 3 of the conversion rewrites those into
    // tag-dispatch.

    /// Is this the empty list `()`?
    #[inline]
    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }

    /// Is this a cons cell?
    #[inline]
    pub fn is_cons(&self) -> bool {
        matches!(self, Value::Cons(_))
    }

    /// The integer value, if this is an `Int`.
    #[inline]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// The float value, if this is a `Float`.
    #[inline]
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(x) => Some(*x),
            _ => None,
        }
    }

    /// The boolean value, if this is a `Bool`.
    #[inline]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The symbol id, if this is a `Sym`.
    #[inline]
    pub fn as_sym(&self) -> Option<SymId> {
        match self {
            Value::Sym(s) => Some(*s),
            _ => None,
        }
    }

    /// The string contents, if this is a `Str`.
    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The head (car) of a cons cell, if this is a `Cons`.
    #[inline]
    pub fn head(&self) -> Option<&Value> {
        match self {
            Value::Cons(c) => Some(&c.0),
            _ => None,
        }
    }

    /// The tail (cdr) of a cons cell, if this is a `Cons`.
    #[inline]
    pub fn tail(&self) -> Option<&Value> {
        match self {
            Value::Cons(c) => Some(&c.1),
            _ => None,
        }
    }
}

/// Equality matching KL's `=` semantics:
/// * `Int 1` and `Float 1.0` are equal (numeric coercion).
/// * Lists and absvectors compare structurally.
/// * Closures and streams are not user-comparable; equal iff same Rc.
pub fn shen_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    // Fast path: identical Rc pointer for shared structures. The kernel
    // type-checker frequently compares a value to itself when chasing
    // proof goals, and the reader produces shared sub-structures via
    // cons sharing. `Rc::ptr_eq` is O(1) and skips a deep walk.
    if let (Cons(p1), Cons(p2)) = (a, b) {
        if p1.ptr_eq(p2) {
            return true;
        }
    }
    if let (Str(x), Str(y)) = (a, b) {
        if Rc::ptr_eq(x, y) {
            return true;
        }
    }
    match (a, b) {
        (Nil, Nil) => true,
        (Bool(x), Bool(y)) => x == y,
        (Int(x), Int(y)) => x == y,
        (Float(x), Float(y)) => x == y,
        (Int(x), Float(y)) | (Float(y), Int(x)) => (*x as f64) == *y,
        (Str(x), Str(y)) => x == y,
        (Sym(x), Sym(y)) => x == y,
        // Cross-equate `Bool(true)` with the symbol `true`, and likewise
        // for false. Our KL parser interns `true`/`false` as `Bool`, but
        // the kernel's Shen reader interns them as symbols. When a
        // predicate returns `Bool(true)` and the kernel compares it
        // against the literal `true` (which after parsing is the symbol
        // `Sym(k_true)`), structural equality must hold or
        // `(= true (boolean? X))`-style code breaks. See
        // `BOOLEAN_SYM_IDS` for how we look up the well-known ids.
        (Bool(b), Sym(s)) | (Sym(s), Bool(b)) => {
            let (kt, kf) = boolean_sym_ids();
            (*b && *s == kt) || (!*b && *s == kf)
        }
        (Cons(p1), Cons(p2)) => shen_eq(&p1.0, &p2.0) && shen_eq(&p1.1, &p2.1),
        (Vec(x), Vec(y)) => {
            let x = x.borrow();
            let y = y.borrow();
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| shen_eq(a, b))
        }
        (Closure(x), Closure(y)) => Rc::ptr_eq(x, y),
        (Stream(x), Stream(y)) => Rc::ptr_eq(x, y),
        (Error(x), Error(y)) => x == y,
        (Foreign(x), Foreign(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

/// Process-wide cache of the `SymId`s for the kernel symbols `true` and
/// `false`. Initialised once at interpreter boot via
/// [`set_boolean_sym_ids`]. We accept the slight ugliness of a global
/// because `shen_eq` is on the hot path and must not require an
/// `Interp` reference.
static BOOLEAN_SYM_IDS: std::sync::OnceLock<(SymId, SymId)> = std::sync::OnceLock::new();

pub fn set_boolean_sym_ids(true_id: SymId, false_id: SymId) {
    let _ = BOOLEAN_SYM_IDS.set((true_id, false_id));
}

fn boolean_sym_ids() -> (SymId, SymId) {
    // If the cache isn't initialised (e.g. unit tests that don't boot
    // the kernel), fall back to SymId(0)/(1) — these won't match any
    // real Bool, so the cross-equate branch becomes a no-op.
    *BOOLEAN_SYM_IDS
        .get()
        .unwrap_or(&(SymId(u32::MAX - 1), SymId(u32::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_builder_round_trip() {
        let v = Value::list([Value::Int(1), Value::Int(2), Value::Int(3)]);
        match v {
            Value::Cons(p) => {
                assert!(matches!(p.0, Value::Int(1)));
                if let Value::Cons(p2) = &p.1 {
                    assert!(matches!(p2.0, Value::Int(2)));
                } else {
                    panic!("expected cons");
                }
            }
            _ => panic!("expected cons"),
        }
    }

    #[test]
    fn int_float_equality() {
        assert!(shen_eq(&Value::Int(1), &Value::Float(1.0)));
        assert!(!shen_eq(&Value::Int(1), &Value::Float(1.5)));
    }

    #[test]
    fn nil_only_equals_nil() {
        assert!(shen_eq(&Value::Nil, &Value::Nil));
        assert!(!shen_eq(&Value::Nil, &Value::Bool(false)));
    }

    #[test]
    fn constructors_and_inspectors_round_trip() {
        assert!(Value::nil().is_nil());
        assert_eq!(Value::int(7).as_int(), Some(7));
        assert_eq!(Value::float(1.5).as_float(), Some(1.5));
        assert_eq!(Value::bool(true).as_bool(), Some(true));
        assert_eq!(Value::sym(SymId(42)).as_sym(), Some(SymId(42)));
        assert_eq!(Value::str("hi").as_str(), Some("hi"));
        assert_eq!(Value::err("boom").as_str(), None);

        let c = Value::cons(Value::int(1), Value::int(2));
        assert!(c.is_cons());
        assert_eq!(c.head().and_then(Value::as_int), Some(1));
        assert_eq!(c.tail().and_then(Value::as_int), Some(2));

        // Wrong-type inspectors return None, not panic.
        assert_eq!(Value::nil().as_int(), None);
        assert_eq!(Value::int(1).as_str(), None);
        assert!(Value::int(1).head().is_none());
    }
}
