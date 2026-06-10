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
//! A **thread-local [`gc::Heap`]** (`HEAP_OWNER`/`HEAP_PTR`, see the
//! split-TLS note below) backs construction, so
//! the Step-1 constructor/inspector seam keeps its signatures (`Value::cons(a,
//! b)` gains no `Heap` parameter) and the thousands of call sites don't churn.
//!
//! ## Collection (GC Step 4)
//!
//! The heap is grow-only by default; `SHEN_RUST_GC` switches it to **request
//! mode**: allocation still never collects — `Heap::grow` raises a pending
//! flag, and the interpreter runs `Heap::collect_at_safepoint` when its
//! activation depth returns to 0 (see `interp::eval`'s `DepthGuard`). Roots
//! are hybrid: the interpreter's containers, precisely (`Interp::gc_roots`),
//! plus a conservative native-stack scan (`gc::stack`) for `Value`s in host
//! frames. Reference-returning inspectors (`head`/`tail`/`as_str`) bridge a
//! raw node pointer to a `&Value`/`&str` tied to `&self`: sound because the
//! heap is non-moving (stable addresses) and a node is never freed while
//! reachable — and *unreachable-but-borrowed* cannot happen, because every
//! interpreter-internal bridged borrow lives at activation depth > 0 where
//! collection never runs, and a HOST must not hold a bridged borrow (or an
//! unpinned heap-container `Value` — see `Interp::gc_pins`) across
//! `Interp::eval`/`apply` when the GC is enabled.

use std::any::Any;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::fmt;
use std::rc::Rc;

use crate::gc::{Gc, GcObject, Heap, HeapKind};
use crate::kl::ast::KlExpr;
use crate::symbol::SymId;

// ## The split-TLS heap (perf: the per-access TLS+RefCell tax)
//
// The heap used to live in a single `thread_local! { RefCell<Heap> }`. That
// shape paid three costs on EVERY `Value` heap access (~8.7% of kernel-tests
// wall, 2026-06-10 profile): the TLS address lookup, a destructor-state check
// (`Heap` implements `Drop`, which makes the key a destructor key even with
// const-init), and the `RefCell` borrow-flag read-modify-writes. The RefCell
// guarded nothing in practice: no closure passed to the funnels below
// re-enters heap access (allocation never collects on a grow-only heap, and
// `gc_edges`/opaque `Drop` impls are pure bit-pushes), and every
// reference-returning inspector (`head`/`tail`/`as_str`/`as_closure`) already
// extracts a raw pointer inside the borrow and derefs it after — their
// soundness rests on the non-moving, never-freed heap, not the borrow.
//
// So the heap is split across two keys:
//
// * `HEAP_PTR` — a **no-`Drop`, const-init** `Cell<*mut Heap>`, the fast
//   path. This is the one `thread_local!` shape that compiles to a bare TLS
//   address load: no destructor-state check, no lazy-init branch. Null until
//   the first access; nulled again at thread exit (see `HeapOwner::drop`).
// * `HEAP_OWNER` — the owning slot, lazily initialized on the first heap
//   access. Runs `Heap::drop` at thread exit exactly like the old slot (no
//   leak). The `Box` pins the heap's address independent of TLS storage; the
//   `UnsafeCell` lets the fast path derive `&mut Heap` from a pointer that
//   was obtained through a shared reference.
//
// INVARIANT (debug-checked by `HEAP_BORROWS`, silent in release): code
// reachable from a `with_heap`/`with_heap_mut` closure must never call back
// into these funnels. Today that holds because the closures only call `Heap`
// methods, `Value::from_gc`/`to_gc` bit-casts, the global allocator, and
// `Rc`/`Any` operations. GC Step 4 made the highest-stakes instance LIVE:
// `Heap::collect_at_safepoint` runs inside `with_heap_mut`, its mark phase
// calls `GcObject::gc_edges`, and its sweep runs Closure/Opaque payload
// `Drop`s — all of those must stay free of `Value` heap accessors. The known
// trap class is `Value`'s `Debug`/`Display` formatting (it calls
// `heap_kind()` → `with_heap`): never debug-format a `Value` from a payload
// `Drop` or `gc_edges`. Tripwires: the sweep-Drop `should_panic` test below,
// the poison-on-sweep words, and the `--debug-gc` gate leg.
thread_local! {
    /// Fast-path raw pointer to this thread's heap. See the module note above
    /// for why this is a separate key from the owner.
    static HEAP_PTR: Cell<*mut Heap> = const { Cell::new(std::ptr::null_mut()) };

    /// The per-thread GC heap that backs every heap-allocated `Value`. Grow-only
    /// in Step 3 (see the module docs). Lazily initialized on first use, so
    /// `value.rs` unit tests and the differential oracle — which build `Value`s
    /// with no `Interp` — work with no explicit setup.
    static HEAP_OWNER: HeapOwner = HeapOwner(Box::new(UnsafeCell::new(Heap::grow_only())));

    /// Debug-only reentrancy sentinel standing in for `RefCell`'s dynamic
    /// borrow check: 0 = free, >0 = shared borrows, -1 = mutable borrow.
    /// Catches both directions (mutable-during-shared and anything-during-
    /// mutable) under `cargo test` and miri; compiled out of release builds.
    #[cfg(debug_assertions)]
    static HEAP_BORROWS: Cell<i32> = const { Cell::new(0) };
}

/// Owns the thread's heap. On thread exit, nulls `HEAP_PTR` **before**
/// `Heap::drop` runs, so a heap access from another TLS destructor takes the
/// cold path and panics deterministically on the destroyed `HEAP_OWNER` key
/// (matching the old `RefCell` slot's panic) instead of dereferencing a
/// dangling pointer.
struct HeapOwner(Box<UnsafeCell<Heap>>);

impl Drop for HeapOwner {
    fn drop(&mut self) {
        // `HEAP_PTR` has no destructor, so it is accessible during TLS
        // teardown in any key order.
        HEAP_PTR.set(std::ptr::null_mut());
    }
}

/// The thread's heap pointer; initializes the owner on first use.
#[inline]
fn heap_ptr() -> *mut Heap {
    let p = HEAP_PTR.get();
    if p.is_null() {
        heap_ptr_cold()
    } else {
        p
    }
}

/// First access on this thread (or access after teardown, which panics on
/// the destroyed `HEAP_OWNER` key — deliberately).
#[cold]
fn heap_ptr_cold() -> *mut Heap {
    HEAP_OWNER.with(|o| {
        let p = o.0.get();
        HEAP_PTR.set(p);
        p
    })
}

/// Debug-only borrow-sentinel guard (see `HEAP_BORROWS`). Unwind-safe: the
/// `Drop` impl restores the previous state even if the funnel closure panics.
#[cfg(debug_assertions)]
struct BorrowGuard {
    restore: i32,
}

#[cfg(debug_assertions)]
impl BorrowGuard {
    #[inline]
    fn shared() -> Self {
        let v = HEAP_BORROWS.get();
        assert!(
            v >= 0,
            "heap re-entered: shared access during mutable borrow"
        );
        HEAP_BORROWS.set(v + 1);
        BorrowGuard { restore: v }
    }

    #[inline]
    fn exclusive() -> Self {
        let v = HEAP_BORROWS.get();
        assert!(
            v == 0,
            "heap re-entered: mutable access during active borrow"
        );
        HEAP_BORROWS.set(-1);
        BorrowGuard { restore: v }
    }
}

#[cfg(debug_assertions)]
impl Drop for BorrowGuard {
    #[inline]
    fn drop(&mut self) {
        HEAP_BORROWS.set(self.restore);
    }
}

/// Run `f` with shared access to the thread-local heap.
#[inline]
fn with_heap<R>(f: impl FnOnce(&Heap) -> R) -> R {
    let p = heap_ptr();
    #[cfg(debug_assertions)]
    let _guard = BorrowGuard::shared();
    // SAFETY: `p` is non-null (heap_ptr initializes it) and points to this
    // thread's boxed heap, whose address is pinned for the thread's lifetime
    // and which is nulled before `Heap::drop`. Single-threaded by
    // construction (per-thread key); no `&mut` alias can be live here by the
    // non-reentrancy invariant documented above (debug-checked).
    f(unsafe { &*p })
}

/// Run `f` with mutable access to the thread-local heap (allocation).
#[inline]
fn with_heap_mut<R>(f: impl FnOnce(&mut Heap) -> R) -> R {
    let p = heap_ptr();
    #[cfg(debug_assertions)]
    let _guard = BorrowGuard::exclusive();
    // SAFETY: as in `with_heap`, plus exclusivity: the pointer was derived
    // through `UnsafeCell::get`, and the non-reentrancy invariant
    // (debug-checked) guarantees no other `&Heap`/`&mut Heap` derived from it
    // is live during `f`.
    f(unsafe { &mut *p })
}

// ---- GC Step 4 glue: request-mode collection at interpreter safepoints ----
//
// The interpreter (`interp::eval`) owns WHEN to collect (activation depth 0,
// see its `DepthGuard`); the heap owns the trigger bookkeeping and the hybrid
// mark (precise roots + conservative native-stack scan). These funnels bridge
// the two without exposing `Gc` or the TLS heap outside this module.

/// Can request-mode collection run on this build? False where the
/// conservative native-stack scan is unsupported (non-aarch64, miri) — the
/// enable path must refuse rather than collect with an unsound root set.
pub(crate) fn gc_scan_supported() -> bool {
    crate::gc::stack::SCAN_SUPPORTED
}

thread_local! {
    /// Mirror of this thread's `Heap::request_gc_enabled()`, kept as its own
    /// no-`Drop` const-init key so the interpreter's per-`eval`/`apply` "is
    /// the GC even on?" check is a single bare TLS load — the GC-off default
    /// path pays one predicted branch, not the full depth-guard protocol.
    static GC_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Has request-mode collection been enabled on this thread? (The cheap
/// mirror — see `GC_ACTIVE`.)
#[inline]
pub(crate) fn gc_request_active() -> bool {
    GC_ACTIVE.with(|g| g.get())
}

/// Switch this thread's heap to request mode (see `Heap::enable_request_gc`).
pub(crate) fn gc_enable_request_mode(floor: usize) {
    with_heap_mut(|h| h.enable_request_gc(floor));
    GC_ACTIVE.with(|g| g.set(true));
}

/// Has request mode been enabled on this thread's heap?
pub(crate) fn gc_request_enabled() -> bool {
    with_heap(|h| h.request_gc_enabled())
}

/// Is a deferred collection pending? Cheap (one TLS load + a field read);
/// polled by the interpreter at depth-0 funnel exits.
#[inline]
pub(crate) fn gc_request_pending() -> bool {
    with_heap(|h| h.gc_pending())
}

/// Run a safepoint collection with `precise` as the precise root set (the
/// conservative stack scan supplies the rest). Returns
/// `(collections, last_live, node_count)` for stats reporting.
///
/// The caller must hold NO bridged heap borrows (`&str` from `as_str`,
/// `&Closure` from `as_closure`, …) across this call — the interpreter
/// guarantees that structurally by collecting only at activation depth 0.
pub(crate) fn gc_collect_at_safepoint(precise: &[Value]) -> (u64, usize, usize) {
    with_heap_mut(|h| {
        // SAFETY: `Value` and `Gc` are both `#[repr(transparent)]` over `u64`
        // and share the fixnum/ptr/nil tag encodings (module docs); `sym`/
        // `bool` words read as non-pointer `Gc`s the collector ignores. The
        // cast is the bulk form of `Value::to_gc`.
        let gcs: &[Gc] =
            unsafe { std::slice::from_raw_parts(precise.as_ptr().cast::<Gc>(), precise.len()) };
        h.collect_at_safepoint(gcs);
        (h.collections(), h.last_live(), h.node_count())
    })
}

/// `(collections, last_live, node_count)` of this thread's heap — for stats
/// reporting and the GC stress/boundedness harnesses.
pub(crate) fn gc_heap_stats() -> (u64, usize, usize) {
    with_heap(|h| (h.collections(), h.last_live(), h.node_count()))
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
    /// User-defined closure body compiled to native code by the Cranelift JIT
    /// (stage J2, `design/jit-productionization-plan.md`). The `Vec<Value>` is
    /// the traceable shadow-capture list — identical role to `Bytecode`'s
    /// upvals: the JIT'd body reads captured `Value`s through a raw word
    /// pointer the collector cannot see, so these handles keep them reachable.
    #[cfg(feature = "jit")]
    Jit(Rc<crate::jit::JitClosure>, Vec<Value>),
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
            #[cfg(feature = "jit")]
            ClosureKind::Jit(_, captures) => {
                for v in captures {
                    out.push(v.to_gc());
                }
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

    /// The raw tagged word, for the JIT FFI boundary (`src/jit/`). `Value` is
    /// `#[repr(transparent)]` over this `u64`, so JIT'd native code operates on
    /// exactly these bits. Sound and lossless — the inverse of [`Value::from_word`].
    #[cfg(feature = "jit")]
    #[inline]
    pub(crate) fn to_word(self) -> u64 {
        self.0
    }

    /// Rebuild a `Value` from a raw tagged word produced by JIT'd code or an
    /// `rtj_*` helper. The word must be a valid tagged `Value` bit pattern
    /// (it always is on this path — it originated from [`Value::to_word`] or a
    /// runtime helper that returns one).
    #[cfg(feature = "jit")]
    #[inline]
    pub(crate) fn from_word(w: u64) -> Value {
        Value(w)
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
    /// from the pinned heap node (sound: non-moving, and never freed while
    /// reachable — see the module's "Collection" note for the borrow rules).
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
    /// pinned node (sound: non-moving, never freed while reachable — module
    /// "Collection" note).
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
    /// the pinned node (sound: non-moving, never freed while reachable —
    /// module "Collection" note).
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

    /// Locks the debug-only reentrancy sentinel itself: heap access from
    /// inside a `with_heap_mut` closure must panic in debug builds (in
    /// release it would be UB — this test is the tripwire that keeps the
    /// non-reentrancy invariant honest; see the split-TLS module note).
    #[test]
    #[should_panic(expected = "heap re-entered")]
    #[cfg(debug_assertions)]
    fn heap_reentry_panics_in_debug() {
        with_heap_mut(|_| {
            // Any Value heap accessor re-enters the funnels.
            let _ = Value::cons(Value::int(1), Value::nil());
        });
    }

    /// The sentinel restores state between sequential accesses: a mix of
    /// shared (eq/inspectors) and mutable (alloc) funnel calls keeps working.
    #[test]
    fn heap_sentinel_recovers_between_accesses() {
        let v = Value::cons(Value::int(1), Value::nil());
        assert!(shen_eq(&v, &v));
        let w = Value::cons(Value::int(2), v);
        assert!(w.is_cons());
    }

    /// GC Step 4 sweep-Drop tripwire: a heap payload whose `Drop` calls a
    /// `Value` heap accessor must trip the reentrancy sentinel when it is
    /// reclaimed by a sweep (the sweep runs inside the live `&mut Heap` of
    /// `with_heap_mut` — an accessor there is debug-panic / release-UB).
    /// `heap_reentry_panics_in_debug` covers funnel reentry; this covers the
    /// *sweep-Drop* path specifically. Runs on a dedicated thread because the
    /// deliberate mid-sweep panic leaves that thread's heap unusable.
    #[test]
    #[cfg(debug_assertions)]
    fn sweep_drop_calling_funnel_panics_in_debug() {
        let result = std::thread::spawn(|| {
            struct EvilDrop;
            impl Drop for EvilDrop {
                fn drop(&mut self) {
                    // The forbidden class: any Value accessor during sweep.
                    let _ = Value::str("boom".to_string());
                }
            }
            let _unrooted = Value::foreign(Rc::new(EvilDrop));
            // Precise-only collect with no roots: the foreign node is swept
            // and its Drop runs inside the exclusive heap borrow.
            with_heap_mut(|h| h.collect(&[]));
        })
        .join();
        let payload = result.expect_err("sweep-Drop funnel call must panic in debug");
        let msg = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            msg.contains("heap re-entered"),
            "expected the reentrancy sentinel, got: {msg:?}"
        );
    }

    /// The positive side of the sweep-Drop constraint: well-behaved payload
    /// `Drop`s (streams, foreign handles, closures) run exactly once when
    /// swept, and the heap stays healthy afterwards.
    #[test]
    fn sweep_runs_well_behaved_drops_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let drops = Arc::new(AtomicUsize::new(0));
        let drops_in = Arc::clone(&drops);
        std::thread::spawn(move || {
            struct CountedDrop(Arc<AtomicUsize>);
            impl Drop for CountedDrop {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
            let _foreign = Value::foreign(Rc::new(CountedDrop(Arc::clone(&drops_in))));
            let _stream = Value::stream(Rc::new(RefCell::new(Stream::Closed)));
            let _closure = Value::closure(Closure {
                name: None,
                arity: 1,
                partial: vec![Value::int(1)],
                kind: ClosureKind::Native(
                    Rc::new(|_: &mut crate::interp::eval::Interp, _: &[Value]| Ok(Value::nil())),
                    vec![Value::int(2)],
                ),
            });
            with_heap_mut(|h| h.collect(&[]));
            // Heap stays usable after the sweep.
            let v = Value::cons(Value::int(1), Value::nil());
            assert!(v.is_cons());
        })
        .join()
        .expect("well-behaved sweep must not panic");
        assert_eq!(
            drops.load(Ordering::Relaxed),
            1,
            "foreign Drop must run exactly once"
        );
    }

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
