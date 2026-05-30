//! The heap node: a 24-byte cell with a packed header (kind tag + mark bit) and
//! two payload words.
//!
//! # Why a hand-packed header instead of a Rust enum
//!
//! The proven spike (`gc_spike.rs`) used a tagless `{head, tail, mark}` struct —
//! 24 bytes — because it was cons-only and the *handle* (`Word`) carried the
//! "this is a cons" tag. A real heap is heterogeneous (cons, absvec, blob,
//! opaque), so a pointer-tagged handle can't say which kind a node is — the
//! **node** must carry its own kind tag.
//!
//! A safe `enum Payload { Cons{..}, Vec(..), .. }` re-encodes that kind as an
//! enum discriminant, which (because `Cons` already fills 16 bytes with two
//! arbitrary words, leaving no niche) costs a *separate 8-byte discriminant
//! word* — plus another 8-byte slot for the `mark` byte. That inflates every
//! cons cell to 32 bytes: +33% memory traffic on the list workload that is the
//! entire point of the GC. Measured, it dropped the spike's 3.5× to 2.4×.
//!
//! So the kind tag and mark bit are packed by hand into the first 8-byte word,
//! and the remaining 16 bytes are two payload words ([`Node::a`], [`Node::b`]) —
//! recovering the spike's 24-byte cons cell while still supporting every heap
//! variant. This is the "type tag in the node header" the design calls for, and
//! the layout Step 3's word-sized `Value` builds directly on.
//!
//! # Payload encoding by [`Kind`]
//!
//! | Kind      | Models               | `a`                            | `b`   | Edges        |
//! |-----------|----------------------|--------------------------------|-------|--------------|
//! | `Cons`    | `Value::Cons`        | head `Gc`                      | tail `Gc` | `a`, `b` |
//! | `Vec`     | `Value::Vec` absvec  | thin ptr → `Vec<Gc>`           | —     | every cell   |
//! | `Blob`    | `Value::Str`/`Error` | data ptr → bytes               | len   | none (leaf)  |
//! | `Float`   | `Value::Float`       | `f64::to_bits` of the value    | —     | none (leaf)  |
//! | `Closure` | `Value::Closure`     | thin ptr → `Box<dyn GcObject>` | —     | obj's edges  |
//! | `Opaque`  | `Foreign`/`Stream`   | thin ptr → `Box<dyn Any>`      | —     | none (opaque)|
//! | `Free`    | (on the free-list)   | —                              | —     | none         |
//!
//! `Float` is a leaf with **no owned Rust allocation** (the f64 bits live inline
//! in `a`), so — like `Cons` — it needs no reclaim work. `Closure` owns a boxed
//! [`GcObject`](super::GcObject) trait object: it is **traced** (the collector
//! asks the object to enumerate its `Value` edges — `partial`, upvals, and the
//! AOT shadow-capture vec) *and* runs the object's Rust `Drop` on sweep.
//!
//! The `Vec`/`Blob`/`Opaque` words own a Rust allocation; the owning [`Heap`]
//! must reconstruct and drop it on sweep (or on heap drop). Because the node is
//! plain-old-data with no `Drop` of its own, that reclamation is explicit — see
//! [`super::heap`].
//!
//! [`Heap`]: super::heap::Heap

/// The kind of a heap node — the collector's per-node type tag. `#[repr(u8)]`
/// so it occupies one byte of the packed header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(super) enum Kind {
    /// On the free-list: no live data, no owned resource.
    Free = 0,
    /// A cons pair (`a` = head, `b` = tail) — the hot case.
    Cons,
    /// A mutable absvector (`a` = thin pointer to a `Vec<Gc>`). Cells are edges
    /// and **can form cycles** — the one thing tracing reclaims that `Rc`
    /// cannot.
    Vec,
    /// Immutable bytes (`a` = data pointer, `b` = length). A leaf.
    Blob,
    /// A boxed float (`a` = `f64::to_bits`). A leaf with no owned allocation.
    Float,
    /// A closure (`a` = thin pointer to a `Box<dyn GcObject>`). **Traced** — the
    /// object enumerates its outgoing `Value` edges on mark — and its Rust
    /// `Drop` runs when reclaimed.
    Closure,
    /// An opaque host object (`a` = thin pointer to a `Box<dyn Any>`). Traced
    /// through nothing, never moved, but its Rust `Drop` runs when reclaimed.
    Opaque,
}

/// A heap cell. `#[repr(C)]` fixes the field order: the one-byte `kind` and
/// `mark` sit in the first word (the collector reads them without touching the
/// payload), and the two payload words force 8-byte alignment so a node address
/// has its low three bits free for the [`super::Gc`] tag.
#[repr(C)]
pub(super) struct Node {
    pub(super) kind: Kind,
    /// Reachability mark, set during the mark phase and cleared on sweep.
    pub(super) mark: bool,
    // (6 bytes of implicit `repr(C)` padding to the 8-byte alignment of the
    //  payload words — left unnamed so it isn't a "never read" field.)
    /// First payload word — see the per-[`Kind`] table.
    pub(super) a: u64,
    /// Second payload word — see the per-[`Kind`] table.
    pub(super) b: u64,
}

impl Node {
    /// A fresh, empty node for a newly allocated block.
    pub(super) fn empty() -> Node {
        Node {
            kind: Kind::Free,
            mark: false,
            a: 0,
            b: 0,
        }
    }
}
