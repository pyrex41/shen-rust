//! A non-moving, mark-sweep garbage collector with `Copy` heap handles.
//!
//! This is **Step 2** of the GC ladder (see `design/gc-conversion-handoff.md`
//! and `design/gc-step2-collector-handoff.md`): the collector machinery landed
//! as a real, tested subsystem **but not yet wired to [`crate::value::Value`]**.
//! Step 3 flips `Value` onto this heap; Step 4 adds the conservative
//! AOT-frame root scan that the membership table here ([`Heap::is_heap_ptr`])
//! exists to serve.
//!
//! It is a faithful productionization of the proven spike
//! `benches/gc_spike.rs` (which measured 3.34× over the 24-byte `Rc` enum on a
//! list workload, retaining 98% of the no-reclaim ceiling), generalized from
//! cons-only to every heap variant `Value` will eventually carry.
//!
//! # Design (settled — see the handoff §3, do not re-litigate)
//!
//! * **Non-moving mark-sweep.** Nodes have stable addresses for their whole
//!   lifetime, so (a) identity-constrained variants (`Vec`/`Stream`/`Foreign`)
//!   keep stable identity, and (b) the Step-4 conservative native-stack scan is
//!   *sound* — a false-positive root can only over-retain, never corrupt.
//! * **`Copy` handle ([`Gc`]).** Reading, assigning, or tracing a `Gc` does
//!   **zero** refcount work. That `Copy`-ness is the entire ~2.5× lever and the
//!   JIT prerequisite. A `Gc` is a tagged `u64` word: the low three bits tag it
//!   as a heap-node pointer, an inline fixnum, or nil. Immediates ride inline
//!   exactly as the spike modeled — the collector only ever follows the pointer
//!   tag. Step 3 widens the immediate tag-space into the full `Value`.
//! * **Type tag in the node header, not a vtable.** Each heap node carries a
//!   packed one-byte kind tag ([`node::Kind`]); the collector switches on it to
//!   enumerate outgoing edges and to free owned resources on sweep. Opaque
//!   variants (`Foreign`/`Stream`) trace nothing but run their Rust `Drop` when
//!   swept.
//! * **Block allocation + O(1) membership table.** Nodes live in fixed,
//!   never-reallocated blocks; freed nodes go on a free-list and are reused.
//!   The membership table ([`Heap::is_heap_ptr`]) answers "does this raw word
//!   point at the head of a node we own?" in O(1) — built now, consumed by
//!   Step 4's conservative scan. It is **never touched on the precise-root
//!   `alloc`/`collect` hot path**.
//! * **Safepoints only at `alloc`.** Collection runs only when a soft cap is
//!   hit with an empty free-list, so the root set need only be valid at
//!   allocation points.

mod heap;
pub mod node;

pub use heap::Heap;

/// The runtime kind of a heap node, exposed to the value layer so it can
/// classify a heap-pointer [`Gc`] (the node's private [`node::Kind`] header is
/// internal to the collector). Mirrors the heap-backed `Value` variants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeapKind {
    Cons,
    Vec,
    Str,
    Error,
    Float,
    Closure,
    Opaque,
}

/// A heap object the collector can both **trace** and **down-cast**.
///
/// Stored behind a [`node::Kind::Closure`] node (a `Box<dyn GcObject>`). The
/// collector cannot know the layout of an arbitrary higher-layer type (e.g.
/// `value::Closure`), so the object itself enumerates its outgoing edges via
/// [`GcObject::gc_edges`]; the value layer recovers the concrete type via
/// [`GcObject::as_any`] + `downcast_ref`. This keeps `gc` free of any dependency
/// on `value` while still letting the collector reach `Value`s sealed inside a
/// closure (its `partial`, upvals, and the AOT shadow-capture vec).
pub trait GcObject: std::any::Any {
    /// Push every outgoing heap edge (as a [`Gc`]) into `out`. Immediates
    /// (fixnum/sym/nil/bool) need not be pushed — the collector ignores
    /// non-pointer handles — but pushing them is harmless.
    fn gc_edges(&self, out: &mut Vec<Gc>);

    /// Up-cast for the value layer to `downcast_ref` to the concrete type.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// A `Copy` garbage-collected handle: a tagged `u64` word.
///
/// The low three bits are a tag:
/// * `TAG_PTR` — a pointer to the head of a heap [`node::Node`] (nodes are
///   8-aligned, so the low three bits are free for the tag).
/// * `TAG_FIXNUM` — a 61-bit inline integer (a non-traced leaf value). Present
///   so the collector's list workload — and Step 3's `Value` — can carry
///   immediates inline without a heap node, exactly as `gc_spike.rs` modeled.
/// * `TAG_NIL` — the empty/absent sentinel.
///
/// Assigning, copying, reading, or tracing a `Gc` is pure data movement — no
/// refcount, no allocation. The collector follows only `TAG_PTR` words.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Gc(u64);

const TAG_MASK: u64 = 0b111;
const TAG_BITS: u64 = 3;
const TAG_FIXNUM: u64 = 0b000;
const TAG_PTR: u64 = 0b001;
const TAG_NIL: u64 = 0b011;

impl Gc {
    /// The empty/absent sentinel.
    #[inline]
    pub fn nil() -> Gc {
        Gc(TAG_NIL)
    }

    /// An inline 61-bit fixnum (a non-traced immediate leaf).
    #[inline]
    pub fn fixnum(v: i64) -> Gc {
        Gc(((v as u64) << TAG_BITS) | TAG_FIXNUM)
    }

    /// Is this the nil sentinel?
    #[inline]
    pub fn is_nil(self) -> bool {
        (self.0 & TAG_MASK) == TAG_NIL
    }

    /// Is this an inline fixnum?
    #[inline]
    pub fn is_fixnum(self) -> bool {
        (self.0 & TAG_MASK) == TAG_FIXNUM
    }

    /// The inline integer value. Only meaningful when [`Gc::is_fixnum`].
    #[inline]
    pub fn as_fixnum(self) -> i64 {
        (self.0 as i64) >> TAG_BITS
    }

    /// Is this a pointer to a heap node — i.e. an edge the collector traces?
    #[inline]
    pub fn is_ptr(self) -> bool {
        (self.0 & TAG_MASK) == TAG_PTR
    }

    /// The node address this handle points at (low bits masked off). Only
    /// meaningful when [`Gc::is_ptr`].
    #[inline]
    fn addr(self) -> usize {
        (self.0 & !TAG_MASK) as usize
    }

    /// Recover the node pointer. Only meaningful when [`Gc::is_ptr`].
    ///
    /// Uses exposed provenance: the address was exposed in [`Gc::from_node`]
    /// when the handle was minted, so reconstructing the pointer here is sound
    /// under Miri's permissive provenance model.
    #[inline]
    fn node_ptr(self) -> *mut node::Node {
        core::ptr::with_exposed_provenance_mut::<node::Node>(self.addr())
    }

    /// The raw tagged bits, for storing a handle in a node payload word.
    #[inline]
    pub(crate) fn bits(self) -> u64 {
        self.0
    }

    /// Reconstruct a handle from raw tagged bits read out of a payload word.
    #[inline]
    pub(crate) fn from_bits(bits: u64) -> Gc {
        Gc(bits)
    }

    /// Mint a handle from a freshly allocated node pointer, exposing its
    /// provenance so later [`Gc::node_ptr`] reconstructions are sound.
    #[inline]
    fn from_node(p: *mut node::Node) -> Gc {
        let addr = p.expose_provenance();
        debug_assert_eq!(addr & TAG_MASK as usize, 0, "node not 8-aligned");
        Gc((addr as u64) | TAG_PTR)
    }
}

impl std::fmt::Debug for Gc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_nil() {
            f.write_str("Gc::nil")
        } else if self.is_fixnum() {
            write!(f, "Gc::fixnum({})", self.as_fixnum())
        } else {
            write!(f, "Gc::ptr({:#x})", self.addr())
        }
    }
}
