//! Shen runtime value — a **word-sized, `Copy`, tagged `u64`** over the
//! non-moving GC heap (`crate::gc`).
//!
//! This is **GC Step 3** (see `design/gc-step3-value-flip-handoff.md`): `Value`
//! used to be an `Rc`-backed enum; it is now a single `u64` whose low three bits
//! tag it. The four hottest variants ride inline as immediates (no heap, no
//! refcount); everything else is a [`gc::Gc`] pointer to a self-describing heap
//! node whose `Kind` byte disambiguates the variant.
//!
//! | tag `0bxyz` | meaning | payload |
//! |---|---|---|
//! | `000` | fixnum | 61-bit signed, inline |
//! | `001` | heap pointer | node addr (the node's kind says which variant) |
//! | `010` | sym | `SymId` (u32), inline |
//! | `011` | nil | — |
//! | `100` | bool | 1 payload bit |
//!
//! The tag scheme is **bit-compatible** with [`gc::Gc`] on the three shared tags
//! (`fixnum`/`ptr`/`nil`), so converting a `Value` to a `Gc` (and back) is a
//! reinterpretation — and the collector, which only ever follows the `ptr` tag,
//! correctly ignores `sym`/`bool`/`fixnum` immediates stored in node payload
//! words.
//!
//! ## Where the heap lives
//!
//! A **thread-local grow-only [`gc::Heap`]** ([`HEAP`]) backs construction, so
//! the Step-1 constructor/inspector seam keeps its signatures (`Value::cons(a,
//! b)` gains no `Heap` parameter) and the thousands of call sites don't churn.
//! Collection stays **off** in Step 3 (the precise root set is Step 4); the heap
//! only grows. Reference-returning inspectors (`head`/`tail`/`as_str`) bridge a
//! raw node pointer to a `&Value`/`&str` tied to `&self`: sound because the heap
//! is non-moving (stable addresses) and grow-only (nodes never freed).

use std::any::Any;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use crate::gc::{Gc, GcObject, Heap, HeapKind};
use crate::kl::ast::KlExpr;
use crate::symbol::SymId;

thread_local! {
    /// The per-thread GC heap that backs every heap-allocated `Value`. Grow-only
    /// in Step 3 (see the module docs). Lazily initialized on first use, so
    /// `value.rs` unit tests and the differential oracle — which build `Value`s
    /// with no `Interp` — work with no explicit setup.
    static HEAP: RefCell<Heap> = const { RefCell::new(Heap::grow_only()) };
}

/// Run `f` with shared access to the thread-local heap.
#[inline]
fn with_heap<R>(f: impl FnOnce(&Heap) -> R) -> R {
    HEAP.with(|h| f(&h.borrow()))
}

/// Run `f` with mutable access to the thread-local heap (allocation).
#[inline]
fn with_heap_mut<R>(f: impl FnOnce(&mut Heap) -> R) -> R {
    HEAP.with(|h| f(&mut h.borrow_mut()))
}

/// Mutable absvector backing type (legacy alias — absvectors now live as
/// [`gc::Heap`] `Vec` nodes; kept for any external signature still naming it).
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

/// A shared, mutable stream handle. Stored in an [`gc::Heap`] `Opaque` node and
/// recovered by [`Value::as_stream`]; the `Rc` keeps the historical sharing
/// semantics (two `Value`s can name the same open stream).
pub type SharedStream = Rc<RefCell<Stream>>;

/// A Shen-level callable. Either a primitive backed by a Rust closure or a
/// user-defined lambda compiled from KL. Lives in a `Closure` heap node; the
/// collector traces its `Value` edges via the [`GcObject`] impl below.
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
    /// Native Rust primitive. Receives the full argument vector (`partial`
    /// already prepended). The `Vec<Value>` is the **traceable shadow capture
    /// list** (GC Step 3 §5): the AOT generator `move`-captures `Value`s into
    /// the opaque `dyn Fn`, where the collector cannot see them; this vec holds
    /// the *same* handles so they stay reachable. Empty for primitives that
    /// capture nothing.
    Native(Rc<NativeFn>, Vec<Value>),
    /// User-defined lambda. The body lives in the KL AST.
    Lambda(Rc<LambdaBody>),
    /// User-defined function compiled to bytecode (the VM path). Upvals are
    /// captured at closure creation and stored by-value.
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

impl GcObject for Closure {
    fn gc_edges(&self, out: &mut Vec<Gc>) {
        for v in &self.partial {
            out.push(v.to_gc());
        }
        match &self.kind {
            ClosureKind::Native(_, captures) => {
                for v in captures {
                    out.push(v.to_gc());
                }
            }
            ClosureKind::Lambda(b) => {
                for (_, v) in &b.captured {
                    out.push(v.to_gc());
                }
            }
            ClosureKind::Bytecode(bf, upvals) => {
                for v in upvals {
                    out.push(v.to_gc());
                }
                push_bytecode_consts(bf, out);
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Push every `Value` edge reachable from a `BytecodeFn`'s constant pool,
/// recursing into nested compiled functions (`fn_consts`).
fn push_bytecode_consts(bf: &crate::vm::bytecode::BytecodeFn, out: &mut Vec<Gc>) {
    for v in &bf.consts {
        out.push(v.to_gc());
    }
    for nested in &bf.fn_consts {
        push_bytecode_consts(nested, out);
    }
}

// ---- tag layout ------------------------------------------------------------

const TAG_MASK: u64 = 0b111;
const TAG_BITS: u64 = 3;
const TAG_FIXNUM: u64 = 0b000;
const TAG_PTR: u64 = 0b001;
const TAG_SYM: u64 = 0b010;
const TAG_NIL: u64 = 0b011;
const TAG_BOOL: u64 = 0b100;

/// Inclusive bounds of an inline 61-bit signed fixnum. Integers outside this
/// range promote to a boxed `Float` (GC Step 3 Q1 — preserves today's
/// float-promotion-on-overflow semantics, threshold shifted 2⁶³→2⁶⁰).
const FIXNUM_MIN: i64 = -(1 << 60);
const FIXNUM_MAX: i64 = (1 << 60) - 1;

/// Shen runtime value: a tagged 64-bit word (see the module docs).
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Value(u64);

impl Value {
    // ---- internal Gc bridge -----------------------------------------------

    /// Reinterpret this value's bits as a [`Gc`] handle. Sound because the two
    /// share the `fixnum`/`ptr`/`nil` tag encodings; `sym`/`bool` words read as
    /// non-pointer `Gc`s, which the collector ignores.
    #[inline]
    pub(crate) fn to_gc(self) -> Gc {
        Gc::from_bits(self.0)
    }

    /// Wrap a `Gc` handle as a `Value` (bit reinterpretation).
    #[inline]
    pub(crate) fn from_gc(g: Gc) -> Value {
        Value(g.bits())
    }

    #[inline]
    fn tag(self) -> u64 {
        self.0 & TAG_MASK
    }

    /// The heap-node kind, if this is a heap pointer (else `None`).
    #[inline]
    fn heap_kind(self) -> Option<HeapKind> {
        if self.tag() != TAG_PTR {
            return None;
        }
        with_heap(|h| h.classify(self.to_gc()))
    }

    // ---- Constructors ------------------------------------------------------

    /// The empty list `()`. Distinct from `false` and the symbol `nil`.
    #[inline]
    pub fn nil() -> Value {
        Value(TAG_NIL)
    }

    /// A boolean.
    #[inline]
    pub fn bool(b: bool) -> Value {
        Value(((b as u64) << TAG_BITS) | TAG_BOOL)
    }

    /// An integer. Inline as a 61-bit fixnum when it fits; otherwise promoted to
    /// a boxed `Float` (matches Shen's overflow-to-float semantics).
    #[inline]
    pub fn int(n: i64) -> Value {
        if (FIXNUM_MIN..=FIXNUM_MAX).contains(&n) {
            Value(((n as u64) << TAG_BITS) | TAG_FIXNUM)
        } else {
            Value::float(n as f64)
        }
    }

    /// A floating-point number (boxed — a `Float` heap node).
    #[inline]
    pub fn float(x: f64) -> Value {
        Value::from_gc(with_heap_mut(|h| h.alloc_float(x.to_bits(), &[])))
    }

    /// A symbol, identified by its interned [`SymId`].
    #[inline]
    pub fn sym(id: SymId) -> Value {
        Value(((id.0 as u64) << TAG_BITS) | TAG_SYM)
    }

    /// A string. Accepts anything convertible into `Rc<str>` (the bound is kept
    /// for call-site compatibility; the bytes are copied into a heap blob).
    #[inline]
    pub fn str(s: impl Into<Rc<str>>) -> Value {
        let s: Rc<str> = s.into();
        Value::from_gc(with_heap_mut(|h| h.alloc_str(&s, &[])))
    }

    /// An error object (a trapped error message).
    #[inline]
    pub fn err(msg: impl Into<Rc<str>>) -> Value {
        let s: Rc<str> = msg.into();
        Value::from_gc(with_heap_mut(|h| h.alloc_err(&s, &[])))
    }

    /// Construct a cons cell from head and tail.
    #[inline]
    pub fn cons(head: Value, tail: Value) -> Value {
        Value::from_gc(with_heap_mut(|h| {
            h.alloc_cons(head.to_gc(), tail.to_gc(), &[])
        }))
    }

    /// Build a proper list from an iterator. Trailing `Nil`.
    pub fn list<I: IntoIterator<Item = Value>>(items: I) -> Value
    where
        I::IntoIter: DoubleEndedIterator,
    {
        let mut acc = Value::nil();
        for v in items.into_iter().rev() {
            acc = Value::cons(v, acc);
        }
        acc
    }

    /// A mutable absvector from its initial cells.
    pub fn absvector(cells: Vec<Value>) -> Value {
        let gcs: Vec<Gc> = cells.iter().map(|v| v.to_gc()).collect();
        Value::from_gc(with_heap_mut(|h| h.alloc_vec(gcs, &[])))
    }

    /// A closure value (boxes `c` into a `Closure` heap node).
    pub fn closure(c: Closure) -> Value {
        Value::from_gc(with_heap_mut(|h| h.alloc_closure(Box::new(c), &[])))
    }

    /// A shared I/O stream value.
    pub fn stream(s: SharedStream) -> Value {
        Value::from_gc(with_heap_mut(|h| h.alloc_opaque(Box::new(s), &[])))
    }

    /// A host-language opaque value (Cedar handles, etc.).
    pub fn foreign(obj: Rc<dyn Any>) -> Value {
        Value::from_gc(with_heap_mut(|h| h.alloc_opaque(Box::new(obj), &[])))
    }

    // ---- Inspectors --------------------------------------------------------

    /// Is this the empty list `()`?
    #[inline]
    pub fn is_nil(&self) -> bool {
        self.tag() == TAG_NIL
    }

    /// Is this a cons cell?
    #[inline]
    pub fn is_cons(&self) -> bool {
        self.heap_kind() == Some(HeapKind::Cons)
    }

    /// Is this an absvector?
    #[inline]
    pub fn is_vec(&self) -> bool {
        self.heap_kind() == Some(HeapKind::Vec)
    }

    /// Is this a string?
    #[inline]
    pub fn is_str(&self) -> bool {
        self.heap_kind() == Some(HeapKind::Str)
    }

    /// Is this an error object?
    #[inline]
    pub fn is_error(&self) -> bool {
        self.heap_kind() == Some(HeapKind::Error)
    }

    /// Is this a symbol?
    #[inline]
    pub fn is_sym(&self) -> bool {
        self.tag() == TAG_SYM
    }

    /// Is this a closure?
    #[inline]
    pub fn is_closure(&self) -> bool {
        self.heap_kind() == Some(HeapKind::Closure)
    }

    /// Is this a number (fixnum or boxed float)?
    #[inline]
    pub fn is_number(&self) -> bool {
        self.tag() == TAG_FIXNUM || self.heap_kind() == Some(HeapKind::Float)
    }

    /// The integer value, if this is a fixnum.
    #[inline]
    pub fn as_int(&self) -> Option<i64> {
        if self.tag() == TAG_FIXNUM {
            Some((self.0 as i64) >> TAG_BITS)
        } else {
            None
        }
    }

    /// The float value, if this is a (boxed) `Float`.
    #[inline]
    pub fn as_float(&self) -> Option<f64> {
        if self.tag() != TAG_PTR {
            return None;
        }
        let g = self.to_gc();
        with_heap(|h| {
            if h.classify(g) == Some(HeapKind::Float) {
                Some(f64::from_bits(h.float_bits(g)))
            } else {
                None
            }
        })
    }

    /// The numeric value as `f64` (fixnum widened, or the float), else `None`.
    #[inline]
    pub fn as_number_f64(&self) -> Option<f64> {
        if let Some(n) = self.as_int() {
            Some(n as f64)
        } else {
            self.as_float()
        }
    }

    /// The boolean value, if this is a `Bool`.
    #[inline]
    pub fn as_bool(&self) -> Option<bool> {
        if self.tag() == TAG_BOOL {
            Some((self.0 >> TAG_BITS) & 1 != 0)
        } else {
            None
        }
    }

    /// The symbol id, if this is a `Sym`.
    #[inline]
    pub fn as_sym(&self) -> Option<SymId> {
        if self.tag() == TAG_SYM {
            Some(SymId((self.0 >> TAG_BITS) as u32))
        } else {
            None
        }
    }

    /// The string contents, if this is a `Str`. The returned `&str` is borrowed
    /// from the pinned heap node (sound: non-moving + grow-only).
    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        let raw = with_heap(|h| h.blob_raw(self.to_gc()));
        raw.map(|(ptr, len)| {
            // SAFETY: `ptr`/`len` name the node's owned UTF-8 byte buffer, which
            // stays valid and immovable while `self` is reachable. The lifetime
            // is tied to `&self`.
            unsafe {
                let bytes = std::slice::from_raw_parts(ptr, len);
                std::str::from_utf8_unchecked(bytes)
            }
        })
    }

    /// The error message, if this is an error object. Borrowed like [`as_str`].
    #[inline]
    pub fn error_message(&self) -> Option<&str> {
        let raw = with_heap(|h| h.err_raw(self.to_gc()));
        raw.map(|(ptr, len)| {
            // SAFETY: as `as_str`, over the error node's owned bytes.
            unsafe {
                let bytes = std::slice::from_raw_parts(ptr, len);
                std::str::from_utf8_unchecked(bytes)
            }
        })
    }

    /// The head (car) of a cons cell, if this is a `Cons`. Borrowed from the
    /// pinned node (sound: non-moving + grow-only).
    #[inline]
    pub fn head(&self) -> Option<&Value> {
        let ptrs = with_heap(|h| h.cons_word_ptrs(self.to_gc()));
        ptrs.map(|(head, _tail)| {
            // SAFETY: `head` aliases the cons node's head word (a `Gc`/`Value`
            // bit-pattern) in immovable, never-freed storage; lifetime tied to
            // `&self`. A `Gc` is `repr(transparent)` over `u64` and so is
            // `Value`, so the reinterpret is layout-valid.
            unsafe { &*(head as *const Value) }
        })
    }

    /// The tail (cdr) of a cons cell, if this is a `Cons`.
    #[inline]
    pub fn tail(&self) -> Option<&Value> {
        let ptrs = with_heap(|h| h.cons_word_ptrs(self.to_gc()));
        ptrs.map(|(_head, tail)| {
            // SAFETY: as `head`, over the tail word.
            unsafe { &*(tail as *const Value) }
        })
    }

    // ---- heap-backed accessors --------------------------------------------

    /// The number of cells in an absvector. Panics if not a vec.
    pub fn vec_len(&self) -> usize {
        with_heap(|h| h.vec_len(self.to_gc()))
    }

    /// Read cell `i` of an absvector. Panics if not a vec or `i` out of bounds.
    pub fn vec_get(&self, i: usize) -> Value {
        Value::from_gc(with_heap(|h| h.vec_get(self.to_gc(), i)))
    }

    /// Read cell `i` of an absvector, or `None` if this is not a vec or `i` is
    /// out of bounds. The non-panicking analogue of [`Value::vec_get`].
    pub fn vec_get_opt(&self, i: usize) -> Option<Value> {
        if self.heap_kind() != Some(HeapKind::Vec) {
            return None;
        }
        with_heap(|h| {
            let g = self.to_gc();
            if i < h.vec_len(g) {
                Some(Value::from_gc(h.vec_get(g, i)))
            } else {
                None
            }
        })
    }

    /// Write cell `i` of an absvector. Panics if not a vec or `i` out of bounds.
    pub fn vec_set(&self, i: usize, val: Value) {
        with_heap_mut(|h| h.vec_set(self.to_gc(), i, val.to_gc()));
    }

    /// Collect an absvector's cells into an owned `Vec<Value>`. Panics if not a
    /// vec. (Convenience for the few sites that iterate/compare every cell.)
    pub fn vec_cells(&self) -> Vec<Value> {
        with_heap(|h| {
            let g = self.to_gc();
            let n = h.vec_len(g);
            (0..n).map(|i| Value::from_gc(h.vec_get(g, i))).collect()
        })
    }

    /// Borrow the closure behind a closure value, if this is one. Borrowed from
    /// the pinned node (sound: non-moving + grow-only).
    pub fn as_closure(&self) -> Option<&Closure> {
        // Hot path (every AOT/VM/tree-walker call dispatches through here):
        // a single thread-local heap access resolves node→kind→object pointer,
        // then we drop the borrow and bridge the lifetime to `&self`.
        if self.tag() != TAG_PTR {
            return None;
        }
        let obj_ptr = with_heap(|h| h.closure_obj_ptr(self.to_gc()))?;
        // SAFETY: the closure object lives in immovable, never-freed storage
        // while `self` is reachable; lifetime tied to `&self`.
        let obj: &dyn GcObject = unsafe { &*obj_ptr };
        obj.as_any().downcast_ref::<Closure>()
    }

    /// The shared stream handle, if this value is a stream.
    pub fn as_stream(&self) -> Option<SharedStream> {
        if self.heap_kind() != Some(HeapKind::Opaque) {
            return None;
        }
        with_heap(|h| {
            h.opaque_ref(self.to_gc())
                .downcast_ref::<SharedStream>()
                .cloned()
        })
    }

    /// The host opaque handle, if this value is a `Foreign`.
    pub fn as_foreign(&self) -> Option<Rc<dyn Any>> {
        if self.heap_kind() != Some(HeapKind::Opaque) {
            return None;
        }
        with_heap(|h| {
            h.opaque_ref(self.to_gc())
                .downcast_ref::<Rc<dyn Any>>()
                .cloned()
        })
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_nil() {
            return f.write_str("Nil");
        }
        if let Some(b) = self.as_bool() {
            return write!(f, "Bool({b})");
        }
        if let Some(n) = self.as_int() {
            return write!(f, "Int({n})");
        }
        if let Some(x) = self.as_float() {
            return write!(f, "Float({x})");
        }
        if let Some(s) = self.as_sym() {
            return write!(f, "Sym({})", s.0);
        }
        if let Some(s) = self.as_str() {
            return write!(f, "Str({s:?})");
        }
        if let Some(s) = self.error_message() {
            return write!(f, "Error({s:?})");
        }
        match self.heap_kind() {
            Some(HeapKind::Cons) => f.write_str("Cons(..)"),
            Some(HeapKind::Vec) => write!(f, "Vec(len={})", self.vec_len()),
            Some(HeapKind::Closure) => {
                let arity = self.as_closure().map(|c| c.arity).unwrap_or(0);
                write!(f, "Closure(arity={arity})")
            }
            Some(HeapKind::Opaque) => {
                if self.as_stream().is_some() {
                    f.write_str("Stream(..)")
                } else {
                    f.write_str("Foreign(..)")
                }
            }
            _ => f.write_str("Value(?)"),
        }
    }
}

/// Equality matching KL's `=` semantics:
/// * `Int 1` and `Float 1.0` are equal (numeric coercion).
/// * Lists and absvectors compare structurally.
/// * Closures and streams are not user-comparable; equal iff the same heap node.
pub fn shen_eq(a: &Value, b: &Value) -> bool {
    // Identical bits: same immediate (fixnum/sym/nil/bool) *or* the same heap
    // node — this subsumes the old `Rc::ptr_eq` fast paths for cons/str.
    if a.0 == b.0 {
        return true;
    }

    // Numbers cross-equate (incl. fixnum vs boxed float).
    if a.is_number() && b.is_number() {
        return a.as_number_f64() == b.as_number_f64();
    }

    // Cross-equate `Bool(true)` with the symbol `true`, and `false` likewise.
    // (Our KL parser interns booleans as `Bool`; the kernel reader interns them
    // as the symbols `true`/`false`.)
    if let (Some(bv), Some(s)) = (a.as_bool(), b.as_sym()) {
        let (kt, kf) = boolean_sym_ids();
        return (bv && s == kt) || (!bv && s == kf);
    }
    if let (Some(s), Some(bv)) = (a.as_sym(), b.as_bool()) {
        let (kt, kf) = boolean_sym_ids();
        return (bv && s == kt) || (!bv && s == kf);
    }

    match (a.heap_kind(), b.heap_kind()) {
        (Some(HeapKind::Cons), Some(HeapKind::Cons)) => {
            shen_eq(a.head().unwrap(), b.head().unwrap())
                && shen_eq(a.tail().unwrap(), b.tail().unwrap())
        }
        (Some(HeapKind::Str), Some(HeapKind::Str)) => a.as_str() == b.as_str(),
        (Some(HeapKind::Error), Some(HeapKind::Error)) => a.error_message() == b.error_message(),
        (Some(HeapKind::Vec), Some(HeapKind::Vec)) => {
            let x = a.vec_cells();
            let y = b.vec_cells();
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| shen_eq(a, b))
        }
        _ => false,
    }
}

/// Process-wide cache of the `SymId`s for the kernel symbols `true` and
/// `false`. Initialised once at interpreter boot via [`set_boolean_sym_ids`].
static BOOLEAN_SYM_IDS: std::sync::OnceLock<(SymId, SymId)> = std::sync::OnceLock::new();

pub fn set_boolean_sym_ids(true_id: SymId, false_id: SymId) {
    let _ = BOOLEAN_SYM_IDS.set((true_id, false_id));
}

fn boolean_sym_ids() -> (SymId, SymId) {
    // If the cache isn't initialised (e.g. unit tests that don't boot the
    // kernel), fall back to ids that won't match any real `Bool`, so the
    // cross-equate branch becomes a no-op.
    *BOOLEAN_SYM_IDS
        .get()
        .unwrap_or(&(SymId(u32::MAX - 1), SymId(u32::MAX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_sized_and_copy() {
        assert_eq!(std::mem::size_of::<Value>(), 8, "Value must be one word");
        // `Copy` is asserted structurally: this compiles only if `Value: Copy`.
        fn _assert_copy<T: Copy>() {}
        _assert_copy::<Value>();
    }

    #[test]
    fn list_builder_round_trip() {
        let v = Value::list([Value::int(1), Value::int(2), Value::int(3)]);
        assert!(v.is_cons());
        assert_eq!(v.head().and_then(Value::as_int), Some(1));
        let tail = v.tail().copied().unwrap();
        assert_eq!(tail.head().and_then(Value::as_int), Some(2));
    }

    #[test]
    fn int_float_equality() {
        assert!(shen_eq(&Value::int(1), &Value::float(1.0)));
        assert!(!shen_eq(&Value::int(1), &Value::float(1.5)));
    }

    #[test]
    fn nil_only_equals_nil() {
        assert!(shen_eq(&Value::nil(), &Value::nil()));
        assert!(!shen_eq(&Value::nil(), &Value::bool(false)));
    }

    #[test]
    fn fixnum_overflow_promotes_to_float() {
        // Inside the 61-bit range: stays an exact fixnum.
        assert_eq!(Value::int(FIXNUM_MAX).as_int(), Some(FIXNUM_MAX));
        assert_eq!(Value::int(FIXNUM_MIN).as_int(), Some(FIXNUM_MIN));
        // Outside: promotes to float (no longer an int).
        let big = Value::int(i64::MAX);
        assert_eq!(big.as_int(), None);
        assert_eq!(big.as_float(), Some(i64::MAX as f64));
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
        assert_eq!(Value::err("boom").error_message(), Some("boom"));

        let c = Value::cons(Value::int(1), Value::int(2));
        assert!(c.is_cons());
        assert_eq!(c.head().and_then(Value::as_int), Some(1));
        assert_eq!(c.tail().and_then(Value::as_int), Some(2));

        // Wrong-type inspectors return None, not panic.
        assert_eq!(Value::nil().as_int(), None);
        assert_eq!(Value::int(1).as_str(), None);
        assert!(Value::int(1).head().is_none());
    }

    #[test]
    fn absvector_round_trip() {
        let v = Value::absvector(vec![Value::int(10), Value::int(20), Value::int(30)]);
        assert!(v.is_vec());
        assert_eq!(v.vec_len(), 3);
        assert_eq!(v.vec_get(1).as_int(), Some(20));
        v.vec_set(1, Value::int(99));
        assert_eq!(v.vec_get(1).as_int(), Some(99));
        assert!(shen_eq(
            &v,
            &Value::absvector(vec![Value::int(10), Value::int(99), Value::int(30)])
        ));
    }

    #[test]
    fn native_closure_captures_are_traced() {
        // GC Step 3 §5: a Native closure's shadow captures (and `partial`) must
        // appear as GC edges, so the collector can reach `Value`s sealed inside
        // the opaque `dyn Fn` that the AOT generator `move`-captures into.
        let cap_heap = Value::cons(Value::int(1), Value::nil()); // a heap handle
        let cap_imm = Value::int(7); // an immediate (harmless to push)
        let partial = Value::str("p");
        let f: Rc<NativeFn> =
            Rc::new(|_: &mut crate::interp::eval::Interp, _: &[Value]| Ok(Value::nil()));
        let c = Closure {
            name: None,
            arity: 1,
            partial: vec![partial],
            kind: ClosureKind::Native(f, vec![cap_heap, cap_imm]),
        };
        let mut edges = Vec::new();
        c.gc_edges(&mut edges);
        // The captured heap handle and the partial arg must be enumerated.
        assert!(edges.contains(&cap_heap.to_gc()), "heap capture not traced");
        assert!(edges.contains(&partial.to_gc()), "partial not traced");
    }
}
