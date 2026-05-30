//! The non-moving mark-sweep heap: block allocation, a free-list, a soft-cap
//! collection trigger, and an O(1) membership table.
//!
//! Ported from `benches/gc_spike.rs` (the proven 3.34× collector) and
//! generalized per the Step-2 handoff: block/slab allocation instead of
//! per-node `Box`, a membership table for Step 4's conservative scan, and a
//! [`Kind`]-tagged trace/reclaim path covering every heap variant.
//!
//! # Safety model
//!
//! Node storage is allocated as `Box<[Node]>` blocks and immediately
//! `Box::into_raw`'d into [`Heap::blocks`] as `*mut [Node]`. Going through raw
//! pointers (rather than keeping the `Box`es live) is deliberate and
//! Miri-verified: moving a `Box` asserts *unique* ownership of its pointee under
//! Stacked Borrows, which would invalidate the `*mut Node`s we hand out into the
//! block. With the storage leaked to raw pointers, the block buffers never move
//! or reallocate and the node pointers in [`Heap::all`] / [`Heap::free`] stay
//! valid for the heap's lifetime. The price is a manual free path: `Heap::drop`
//! reclaims every live node's owned resource and then frees the block storage.

use std::any::Any;
use std::collections::HashMap;
use std::ptr::{slice_from_raw_parts_mut, with_exposed_provenance, with_exposed_provenance_mut};

use super::node::{Kind, Node};
use super::{Gc, GcObject};

/// Expose a freshly-`into_raw`'d box pointer and return its address as a payload
/// word, so a later `with_exposed_provenance` reconstruction is sound under
/// Miri's permissive provenance model.
#[inline]
fn expose<T: ?Sized>(p: *mut T) -> u64 {
    (p as *mut u8).expose_provenance() as u64
}

/// Nodes per block. A block is one `Box<[Node]>` allocation, bump-filled onto
/// the free-list. Large enough to amortize allocation, small enough that a
/// modest `cap` still triggers collection rather than just growing.
const BLOCK_SIZE: usize = 1024;

/// Page granularity for the membership table (`addr >> PAGE_BITS` → block).
const PAGE_BITS: u32 = 12;

/// A non-moving, mark-sweep, single-threaded collected heap.
///
/// Collection runs only inside [`Heap::alloc_cons`] (and the other `alloc_*`)
/// when the free-list is empty at the soft `cap`, tracing precisely from the
/// `roots` slice the caller supplies. **Every live heap-pointer the caller
/// holds across an `alloc` — including the `head`/`tail`/cells of the value
/// being built — must appear in `roots`, or it may be reclaimed.**
pub struct Heap {
    /// Leaked node-storage blocks (`Box::into_raw`'d, see the module safety
    /// note). Buffers never move; freed manually in `Drop`.
    blocks: Vec<*mut [Node]>,
    /// Raw pointer to every node ever allocated (stable addresses). Swept.
    all: Vec<*mut Node>,
    /// Reclaimed nodes, reused before growing.
    free: Vec<*mut Node>,
    /// `[base, end)` address range of each block, indexed by block number.
    ranges: Vec<(usize, usize)>,
    /// Membership index: page → the block(s) overlapping it. Consulted only by
    /// [`Heap::is_heap_ptr`] (Step 4's conservative scan) — never on the
    /// precise-root `alloc`/`collect` hot path.
    pages: HashMap<usize, Vec<usize>>,
    /// Soft trigger: once `all.len() >= cap` with an empty free-list, collect
    /// before growing — keeps the heap bounded so collection actually runs.
    cap: usize,
    /// When `false`, [`Heap::obtain_slot`] never auto-collects — the heap only
    /// ever grows. This is **GC Step 3's mode**: the `Value` representation is
    /// flipped onto this heap, but the precise root set (VM stack, `Env`,
    /// closure captures, AOT frames) is not wired until Step 4, so reclaiming
    /// would free live objects. Grow-only is sound for the bounded Step-3 gate
    /// runs; explicit [`Heap::collect`] still works (the new-`Kind` unit tests
    /// drive it directly). See `design/gc-step3-value-flip-handoff.md` §2.
    collection_enabled: bool,
    /// Reused mark-phase work stack (avoids a per-collection allocation).
    mark_stack: Vec<*mut Node>,
    collections: u64,
    peak_live: usize,
}

impl Heap {
    /// A new heap with the given soft node-count cap. Auto-collects at the cap.
    pub fn new(cap: usize) -> Heap {
        Heap {
            blocks: Vec::new(),
            all: Vec::new(),
            free: Vec::new(),
            ranges: Vec::new(),
            pages: HashMap::new(),
            cap,
            collection_enabled: true,
            mark_stack: Vec::new(),
            collections: 0,
            peak_live: 0,
        }
    }

    /// A new **grow-only** heap that never auto-collects (GC Step 3). Allocation
    /// only ever grows the heap; nothing is reclaimed except explicit
    /// [`Heap::collect`] calls and the final [`Heap::drop`]. See the
    /// `collection_enabled` field and `design/gc-step3-value-flip-handoff.md` §2.
    pub fn grow_only() -> Heap {
        Heap {
            blocks: Vec::new(),
            all: Vec::new(),
            free: Vec::new(),
            ranges: Vec::new(),
            pages: HashMap::new(),
            // Cap is irrelevant when collection is disabled, but keep it large
            // so the invariant "never collect" holds even if a future caller
            // flips the flag without setting a cap.
            cap: usize::MAX,
            collection_enabled: false,
            mark_stack: Vec::new(),
            collections: 0,
            peak_live: 0,
        }
    }

    // ---- allocation --------------------------------------------------------

    /// Allocate a cons cell. `head`/`tail` that are heap pointers **must** also
    /// be in `roots` (a collection may fire here). See the type docs.
    #[inline]
    pub fn alloc_cons(&mut self, head: Gc, tail: Gc, roots: &[Gc]) -> Gc {
        let p = self.obtain_slot(roots);
        // SAFETY: `p` is a fresh free slot owned by this heap (see `obtain_slot`).
        unsafe {
            (*p).kind = Kind::Cons;
            (*p).mark = false;
            (*p).a = head.bits();
            (*p).b = tail.bits();
        }
        Gc::from_node(p)
    }

    /// Allocate an absvector node from its cells. Pointer cells **must** also
    /// be in `roots`.
    pub fn alloc_vec(&mut self, cells: Vec<Gc>, roots: &[Gc]) -> Gc {
        let p = self.obtain_slot(roots);
        let a = expose(Box::into_raw(Box::new(cells)));
        // SAFETY: fresh free slot; `a` owns the boxed `Vec` until reclaimed.
        unsafe {
            (*p).kind = Kind::Vec;
            (*p).mark = false;
            (*p).a = a;
            (*p).b = 0;
        }
        Gc::from_node(p)
    }

    /// Allocate an immutable byte blob (a string/error leaf).
    pub fn alloc_blob(&mut self, bytes: impl Into<Box<[u8]>>, roots: &[Gc]) -> Gc {
        let p = self.obtain_slot(roots);
        let bytes: Box<[u8]> = bytes.into();
        let len = bytes.len() as u64;
        let a = expose(Box::into_raw(bytes));
        // SAFETY: fresh free slot; `a`/`len` own the boxed bytes until reclaimed.
        unsafe {
            (*p).kind = Kind::Blob;
            (*p).mark = false;
            (*p).a = a;
            (*p).b = len;
        }
        Gc::from_node(p)
    }

    /// Allocate an opaque host object. Its Rust `Drop` runs when the node is
    /// swept (or when the heap drops).
    pub fn alloc_opaque(&mut self, obj: Box<dyn Any>, roots: &[Gc]) -> Gc {
        let p = self.obtain_slot(roots);
        // Double-box so the payload word is a *thin* pointer (a `dyn Any` box is
        // a fat pointer; this keeps the node 24 bytes and the reclaim path off
        // unstable fat-pointer reconstruction).
        let a = expose(Box::into_raw(Box::new(obj)));
        // SAFETY: fresh free slot; `a` owns the boxed object until reclaimed.
        unsafe {
            (*p).kind = Kind::Opaque;
            (*p).mark = false;
            (*p).a = a;
            (*p).b = 0;
        }
        Gc::from_node(p)
    }

    /// Allocate a boxed float leaf holding `bits` (`f64::to_bits`). No owned
    /// allocation; the bits live inline in the node.
    pub fn alloc_float(&mut self, bits: u64, roots: &[Gc]) -> Gc {
        let p = self.obtain_slot(roots);
        // SAFETY: fresh free slot owned by this heap.
        unsafe {
            (*p).kind = Kind::Float;
            (*p).mark = false;
            (*p).a = bits;
            (*p).b = 0;
        }
        Gc::from_node(p)
    }

    /// Allocate an immutable string leaf (a UTF-8 [`Kind::Blob`]).
    pub fn alloc_str(&mut self, s: &str, roots: &[Gc]) -> Gc {
        self.alloc_blob(s.as_bytes(), roots)
    }

    /// Allocate a closure node owning `obj`. The collector traces `obj`'s
    /// [`GcObject::gc_edges`] on mark and runs its Rust `Drop` on sweep.
    pub fn alloc_closure(&mut self, obj: Box<dyn GcObject>, roots: &[Gc]) -> Gc {
        let p = self.obtain_slot(roots);
        // Double-box so the payload word is a *thin* pointer (a `dyn GcObject`
        // box is a fat pointer); mirrors `alloc_opaque`.
        let a = expose(Box::into_raw(Box::new(obj)));
        // SAFETY: fresh free slot; `a` owns the boxed object until reclaimed.
        unsafe {
            (*p).kind = Kind::Closure;
            (*p).mark = false;
            (*p).a = a;
            (*p).b = 0;
        }
        Gc::from_node(p)
    }

    /// Get a fresh, reusable node slot, collecting (tracing from `roots`) or
    /// growing as needed. The returned node is `Free` (any prior resource was
    /// already dropped on the sweep that freed it), so the caller may overwrite
    /// its fields directly.
    #[inline]
    fn obtain_slot(&mut self, roots: &[Gc]) -> *mut Node {
        if self.free.is_empty() {
            if self.collection_enabled && self.all.len() >= self.cap {
                self.collect(roots);
            }
            if self.free.is_empty() {
                self.grow();
            }
        }
        self.free.pop().unwrap()
    }

    /// Grow the heap by one block, appending its nodes to the free-list and
    /// registering it in the membership table.
    fn grow(&mut self) {
        let block: Box<[Node]> = (0..BLOCK_SIZE).map(|_| Node::empty()).collect();
        // Leak the box to a raw pointer *before* deriving any node pointers, so
        // there is no live `Box` whose later move would Unique-retag the
        // allocation and invalidate those pointers (Miri-verified).
        let raw: *mut [Node] = Box::into_raw(block);
        let base = raw as *mut Node;
        // Expose provenance so `Gc::node_ptr` / `is_heap_ptr` reconstructions
        // are sound under Miri's permissive model.
        let base_addr = base.expose_provenance();
        let stride = std::mem::size_of::<Node>();
        let end_addr = base_addr + BLOCK_SIZE * stride;

        let block_idx = self.ranges.len();
        self.ranges.push((base_addr, end_addr));
        let lo_page = base_addr >> PAGE_BITS;
        let hi_page = (end_addr - 1) >> PAGE_BITS;
        for page in lo_page..=hi_page {
            self.pages.entry(page).or_default().push(block_idx);
        }

        self.all.reserve(BLOCK_SIZE);
        self.free.reserve(BLOCK_SIZE);
        for i in 0..BLOCK_SIZE {
            // SAFETY: `i < BLOCK_SIZE`, so `base.add(i)` is in-bounds of the
            // block allocation and shares its (leaked) provenance.
            let p = unsafe { base.add(i) };
            self.all.push(p);
            self.free.push(p);
        }
        self.blocks.push(raw);
    }

    // ---- collection --------------------------------------------------------

    /// Mark-sweep collection tracing precisely from `roots`.
    ///
    /// Mark: DFS from every root, setting `mark` on reachable nodes. Sweep:
    /// rebuild the free-list from unmarked nodes — reclaiming each via
    /// [`Heap::free_resource`], which frees any owned resource (a `Vec` buffer /
    /// blob bytes / opaque `Drop`) and resets the node to `Free` — and clear
    /// marks on survivors. Free nodes are unmarked, so they correctly stay free.
    pub fn collect(&mut self, roots: &[Gc]) {
        // ---- mark ----
        self.mark_stack.clear();
        for &r in roots {
            self.mark_edge(r);
        }
        while let Some(p) = self.mark_stack.pop() {
            self.trace_node(p);
        }

        // ---- sweep ----
        self.free.clear();
        let mut live = 0usize;
        for &p in &self.all {
            // SAFETY: every `p` in `all` is a live, owned node address.
            unsafe {
                if (*p).mark {
                    (*p).mark = false;
                    live += 1;
                } else {
                    // Reclaim: free any owned resource (Vec buffer, blob bytes,
                    // opaque `Drop`) and return the slot to the free-list.
                    Self::free_resource(p);
                    self.free.push(p);
                }
            }
        }
        self.peak_live = self.peak_live.max(live);
        self.collections += 1;
    }

    /// Drop any Rust allocation a node owns and reset it to `Free`. A no-op for
    /// `Cons`/`Free` (no owned resource). Idempotent: a reclaimed node is left
    /// `Free` with cleared words, so a later sweep or the heap's `Drop` won't
    /// double-free it.
    ///
    /// # Safety
    /// `p` must point to a live, owned node whose `a`/`b` words match its `kind`
    /// (i.e. were written by the corresponding `alloc_*`).
    unsafe fn free_resource(p: *mut Node) {
        unsafe {
            match (*p).kind {
                Kind::Vec => {
                    let vp = with_exposed_provenance_mut::<Vec<Gc>>((*p).a as usize);
                    drop(Box::from_raw(vp));
                }
                Kind::Blob => {
                    let data = with_exposed_provenance_mut::<u8>((*p).a as usize);
                    let slice = slice_from_raw_parts_mut(data, (*p).b as usize);
                    drop(Box::from_raw(slice));
                }
                Kind::Closure => {
                    let bp = with_exposed_provenance_mut::<Box<dyn GcObject>>((*p).a as usize);
                    drop(Box::from_raw(bp));
                }
                Kind::Opaque => {
                    let bp = with_exposed_provenance_mut::<Box<dyn Any>>((*p).a as usize);
                    drop(Box::from_raw(bp));
                }
                // `Float` is a leaf with the bits inline in `a` — no owned
                // allocation, nothing to free. `Cons`/`Free` likewise.
                Kind::Float | Kind::Cons | Kind::Free => {}
            }
            (*p).kind = Kind::Free;
            (*p).a = 0;
            (*p).b = 0;
        }
    }

    /// If `g` is a heap pointer to an unmarked node, mark it and enqueue it.
    #[inline]
    fn mark_edge(&mut self, g: Gc) {
        if !g.is_ptr() {
            return;
        }
        let p = g.node_ptr();
        // SAFETY: a `TAG_PTR` handle is only ever minted for one of our nodes,
        // which outlives the collection.
        unsafe {
            if (*p).mark {
                return;
            }
            (*p).mark = true;
        }
        self.mark_stack.push(p);
    }

    /// Enqueue the outgoing edges of an already-marked node, switching on its
    /// kind tag. All edge words are copied out into locals *before* any
    /// `mark_edge` call, so a self-referential node never has a live reference
    /// into it while its mark bit is written (sound under Miri / Tree Borrows).
    fn trace_node(&mut self, p: *mut Node) {
        // SAFETY: `p` is a live node; reads here are plain word/loads.
        let kind = unsafe { (*p).kind };
        match kind {
            Kind::Cons => {
                let (head, tail) = unsafe { (Gc::from_bits((*p).a), Gc::from_bits((*p).b)) };
                self.mark_edge(head);
                self.mark_edge(tail);
            }
            Kind::Vec => {
                // The cells live in a *separate* allocation (the boxed `Vec`'s
                // buffer), so reading them does not alias node `p`; `mark_edge`
                // writing `p.mark` cannot conflict, even on a self-cycle.
                let a = unsafe { (*p).a };
                let vp = with_exposed_provenance::<Vec<Gc>>(a as usize);
                // SAFETY: `vp` is the live boxed `Vec` this node owns; extract
                // its data pointer and length (Copy) and drop the borrow before
                // tracing.
                let (data, len) = unsafe { ((*vp).as_ptr(), (*vp).len()) };
                for i in 0..len {
                    // SAFETY: `i < len` indexes within the `Vec`'s buffer, which
                    // is not mutated or freed during the mark phase.
                    let g = unsafe { *data.add(i) };
                    self.mark_edge(g);
                }
            }
            Kind::Closure => {
                // The closure object lives in a *separate* allocation (the
                // double-boxed `dyn GcObject`), so reading its edges does not
                // alias node `p`. Collect edges into a buffer first, then mark
                // — never hold a borrow into the object across `mark_edge`.
                let a = unsafe { (*p).a };
                let bp = with_exposed_provenance::<Box<dyn GcObject>>(a as usize);
                let mut edges: Vec<Gc> = Vec::new();
                // SAFETY: `bp` is the live double-boxed object this node owns;
                // it is not mutated or freed during the mark phase.
                unsafe { (**bp).gc_edges(&mut edges) };
                for g in edges {
                    self.mark_edge(g);
                }
            }
            Kind::Float | Kind::Blob | Kind::Opaque | Kind::Free => {}
        }
    }

    // ---- membership table (Step 4's conservative scan) ---------------------

    /// Does `addr` point at the **head of a node we own**? O(1) via the page
    /// table. The collector maintains **no interior pointers** — every [`Gc`]
    /// is a tagged head-of-node — so this is node-granular: an interior or
    /// unaligned address answers `false`.
    ///
    /// Built and tested in Step 2; consumed by Step 4's conservative
    /// native-stack scan. Not used on the precise-root hot path.
    pub fn is_heap_ptr(&self, addr: usize) -> bool {
        let stride = std::mem::size_of::<Node>();
        let page = addr >> PAGE_BITS;
        let Some(blocks) = self.pages.get(&page) else {
            return false;
        };
        for &idx in blocks {
            let (base, end) = self.ranges[idx];
            if addr >= base && addr < end && (addr - base) % stride == 0 {
                return true;
            }
        }
        false
    }

    // ---- accessors (read/mutate node payloads) -----------------------------
    //
    // These deref a node behind a `Gc`. The caller must hold `g` reachable
    // (rooted) for the duration; the non-moving heap then guarantees the node
    // address is stable and live.

    /// The head of a cons cell. Panics if `g` is not a cons node.
    pub fn cons_head(&self, g: Gc) -> Gc {
        let p = self.node_of(g, Kind::Cons, "cons_head");
        // SAFETY: `node_of` verified `p` is a live cons node we own.
        unsafe { Gc::from_bits((*p).a) }
    }

    /// The tail of a cons cell. Panics if `g` is not a cons node.
    pub fn cons_tail(&self, g: Gc) -> Gc {
        let p = self.node_of(g, Kind::Cons, "cons_tail");
        // SAFETY: as `cons_head`.
        unsafe { Gc::from_bits((*p).b) }
    }

    /// Overwrite a cons cell's head. Panics if `g` is not a cons node.
    pub fn set_cons_head(&mut self, g: Gc, head: Gc) {
        let p = self.node_of(g, Kind::Cons, "set_cons_head");
        // SAFETY: `&mut self` grants exclusive access to the verified node.
        unsafe { (*p).a = head.bits() }
    }

    /// Overwrite a cons cell's tail. Panics if `g` is not a cons node. Used to
    /// tie cycles (e.g. tests) the immutable constructors can't build directly.
    pub fn set_cons_tail(&mut self, g: Gc, tail: Gc) {
        let p = self.node_of(g, Kind::Cons, "set_cons_tail");
        // SAFETY: as `set_cons_head`.
        unsafe { (*p).b = tail.bits() }
    }

    /// The number of cells in an absvector node. Panics if `g` is not a vec.
    pub fn vec_len(&self, g: Gc) -> usize {
        let p = self.node_of(g, Kind::Vec, "vec_len");
        // SAFETY: `p` is a live vec node; `a` is its boxed `Vec` pointer.
        unsafe {
            let v: &Vec<Gc> = &*with_exposed_provenance::<Vec<Gc>>((*p).a as usize);
            v.len()
        }
    }

    /// Read cell `i` of an absvector node. Panics if `g` is not a vec or `i` is
    /// out of bounds.
    pub fn vec_get(&self, g: Gc, i: usize) -> Gc {
        let p = self.node_of(g, Kind::Vec, "vec_get");
        // SAFETY: as `vec_len`; indexing bounds-checks `i`.
        unsafe {
            let v: &Vec<Gc> = &*with_exposed_provenance::<Vec<Gc>>((*p).a as usize);
            v[i]
        }
    }

    /// Write cell `i` of an absvector node. Panics if `g` is not a vec or `i`
    /// is out of bounds.
    pub fn vec_set(&mut self, g: Gc, i: usize, val: Gc) {
        let p = self.node_of(g, Kind::Vec, "vec_set");
        // SAFETY: `&mut self` grants exclusive access; indexing bounds-checks.
        unsafe {
            let v: &mut Vec<Gc> = &mut *with_exposed_provenance_mut::<Vec<Gc>>((*p).a as usize);
            v[i] = val;
        }
    }

    /// The bytes of a blob node. Panics if `g` is not a blob.
    pub fn blob_bytes(&self, g: Gc) -> &[u8] {
        let p = self.node_of(g, Kind::Blob, "blob_bytes");
        // SAFETY: `p` is a live blob node; `a`/`b` are its data pointer and
        // length, and the bytes live as long as the node (hence `&self`).
        unsafe {
            let data = with_exposed_provenance::<u8>((*p).a as usize);
            std::slice::from_raw_parts(data, (*p).b as usize)
        }
    }

    /// The `f64::to_bits` of a float node. Panics if `g` is not a float.
    pub fn float_bits(&self, g: Gc) -> u64 {
        let p = self.node_of(g, Kind::Float, "float_bits");
        // SAFETY: `p` is a live float node; `a` holds the bits inline.
        unsafe { (*p).a }
    }

    /// The closure object behind a closure node. Panics if `g` is not a
    /// closure. The returned reference is valid while `g` stays rooted (the
    /// non-moving heap pins the node). The value layer `downcast_ref`s it via
    /// [`GcObject::as_any`].
    pub fn closure_obj(&self, g: Gc) -> &dyn GcObject {
        let p = self.node_of(g, Kind::Closure, "closure_obj");
        // SAFETY: `p` is a live closure node we own; `a` is its double-boxed
        // object pointer, alive as long as the node (hence borrowed from `&self`).
        unsafe {
            let bp = with_exposed_provenance::<Box<dyn GcObject>>((*p).a as usize);
            &**bp
        }
    }

    /// The opaque host object behind an opaque node. Panics if `g` is not
    /// opaque. Borrowed from `&self`; the value layer `downcast_ref`s it.
    pub fn opaque_ref(&self, g: Gc) -> &dyn Any {
        let p = self.node_of(g, Kind::Opaque, "opaque_ref");
        // SAFETY: `p` is a live opaque node; `a` is its double-boxed `dyn Any`
        // pointer, alive as long as the node.
        unsafe {
            let bp = with_exposed_provenance::<Box<dyn Any>>((*p).a as usize);
            &**bp
        }
    }

    /// Resolve `g` to a node pointer, asserting it is a heap pointer of the
    /// expected `kind`. The caller must hold `g` rooted; the non-moving heap
    /// then guarantees the address is stable and live for the heap's lifetime.
    #[inline]
    fn node_of(&self, g: Gc, kind: Kind, what: &str) -> *mut Node {
        assert!(g.is_ptr(), "{what} on a non-pointer Gc");
        let p = g.node_ptr();
        // SAFETY: a `TAG_PTR` handle only ever names one of our live nodes.
        assert_eq!(unsafe { (*p).kind }, kind, "{what} on wrong node kind");
        p
    }

    // ---- instrumentation (bench/test guards) -------------------------------

    /// Total collections run so far.
    pub fn collections(&self) -> u64 {
        self.collections
    }

    /// Largest live-node count observed at the end of any sweep.
    pub fn peak_live(&self) -> usize {
        self.peak_live
    }

    /// Total nodes ever allocated (heap footprint, in nodes).
    pub fn node_count(&self) -> usize {
        self.all.len()
    }

    /// Nodes currently on the free-list.
    pub fn free_count(&self) -> usize {
        self.free.len()
    }

    /// Size of one node, in bytes (for footprint reporting).
    pub fn node_size() -> usize {
        std::mem::size_of::<Node>()
    }
}

impl Drop for Heap {
    /// Free every owned resource, then the leaked block storage. Nodes are
    /// plain-old-data (no `Drop` of their own), so the `Vec`/`Blob`/`Opaque`
    /// allocations live nodes still hold are reclaimed explicitly first;
    /// reclaimed (`Free`) nodes hold nothing, so this never double-frees. The
    /// block buffers (`Box::into_raw`'d in `grow`) are then reconstructed and
    /// dropped, freeing the node storage itself.
    fn drop(&mut self) {
        for &p in &self.all {
            // SAFETY: every `p` in `all` is an owned node; `free_resource` is a
            // no-op for `Cons`/`Free` and reclaims exactly once otherwise.
            unsafe { Self::free_resource(p) };
        }
        for &raw in &self.blocks {
            // SAFETY: each `raw` came from `Box::into_raw` in `grow` and is
            // freed exactly once here; no node pointer is used afterwards.
            unsafe { drop(Box::from_raw(raw)) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// Build a length-`n` list `(n-1 … 1 0)` of fixnums on `heap`, keeping the
    /// growing head rooted in `roots[slot]` across every alloc.
    fn build_list(heap: &mut Heap, n: i64, roots: &mut [Gc], slot: usize) -> Gc {
        let mut xs = Gc::nil();
        for i in 0..n {
            roots[slot] = xs;
            xs = heap.alloc_cons(Gc::fixnum(i), xs, roots);
            roots[slot] = xs;
        }
        xs
    }

    fn sum_list(heap: &Heap, mut xs: Gc) -> i64 {
        let mut acc = 0i64;
        while xs.is_ptr() {
            acc = acc.wrapping_add(heap.cons_head(xs).as_fixnum());
            xs = heap.cons_tail(xs);
        }
        acc
    }

    #[test]
    fn node_is_word_aligned_and_small() {
        // Low 3 bits free for the tag, and not bloated by the rare variants.
        assert_eq!(std::mem::align_of::<Node>() % 8, 0);
        assert!(
            Heap::node_size() <= 24,
            "Node grew to {} bytes — the packed header regressed; a cons cell \
             must stay 24 bytes (two words + header) to match the spike",
            Heap::node_size()
        );
    }

    #[test]
    fn alloc_reachable_after_collect() {
        let mut heap = Heap::new(64);
        let mut roots = vec![Gc::nil()];
        let xs = build_list(&mut heap, 10, &mut roots, 0);
        roots[0] = xs;
        heap.collect(&roots);
        // Survives because it's rooted; still sums correctly.
        assert_eq!(sum_list(&heap, xs), (0..10).sum::<i64>());
    }

    #[test]
    fn unreachable_node_reclaimed() {
        let mut heap = Heap::new(64);
        let mut roots = vec![Gc::nil()];
        let _garbage = build_list(&mut heap, 10, &mut roots, 0);
        let allocated = heap.node_count();
        assert!(allocated >= 10);
        // Drop the only root; everything becomes unreachable.
        roots[0] = Gc::nil();
        heap.collect(&roots);
        assert_eq!(heap.free_count(), allocated, "all 10 nodes should be free");
        assert_eq!(heap.peak_live(), 0);
    }

    #[test]
    fn cycle_is_reclaimed() {
        // The load-bearing property the cons spike never proved: tracing
        // reclaims a cycle that refcounting would leak forever.
        let mut heap = Heap::new(64);
        let mut roots = vec![Gc::nil(), Gc::nil()];

        // Two cons cells; tie b->a and a->b into a cycle, both rooted while
        // building so a mid-build collection can't reclaim them.
        let a = heap.alloc_cons(Gc::fixnum(1), Gc::nil(), &roots);
        roots[0] = a;
        let b = heap.alloc_cons(Gc::fixnum(2), a, &roots);
        roots[1] = b;
        heap.set_cons_tail(a, b); // a <-> b cycle

        // A collection with both rooted keeps both alive.
        heap.collect(&roots);
        assert_eq!(heap.peak_live(), 2);

        // Drop both external roots; the cycle is now unreachable.
        roots[0] = Gc::nil();
        roots[1] = Gc::nil();
        heap.collect(&roots);
        assert_eq!(heap.free_count(), heap.node_count(), "cycle not reclaimed");
        assert_eq!(heap.peak_live(), 2, "peak is a high-water mark");
    }

    #[test]
    fn vec_cycle_is_reclaimed() {
        // Absvector cells forming a cycle — the §3.2 motivating case.
        let mut heap = Heap::new(64);
        let mut roots = vec![Gc::nil(), Gc::nil()];
        let v1 = heap.alloc_vec(vec![Gc::nil()], &roots);
        roots[0] = v1;
        let v2 = heap.alloc_vec(vec![v1], &roots);
        roots[1] = v2;
        heap.vec_set(v1, 0, v2); // v1[0] -> v2, v2[0] -> v1

        roots[0] = Gc::nil();
        roots[1] = Gc::nil();
        heap.collect(&roots);
        assert_eq!(
            heap.free_count(),
            heap.node_count(),
            "vec cycle not reclaimed"
        );
    }

    #[test]
    fn free_list_reuse_bounds_growth() {
        // Repeatedly build and drop a list; the heap must reuse swept nodes
        // instead of growing without bound.
        let mut heap = Heap::new(256);
        let mut roots = vec![Gc::nil()];
        for _ in 0..200 {
            roots[0] = Gc::nil();
            let xs = build_list(&mut heap, 100, &mut roots, 0);
            assert_eq!(sum_list(&heap, xs), (0..100).sum::<i64>());
        }
        roots[0] = Gc::nil();
        // Footprint stays a small multiple of the live set, not 200*100.
        assert!(
            heap.node_count() < 2048,
            "heap grew to {} nodes — free-list not reused",
            heap.node_count()
        );
        assert!(heap.collections() > 0, "never collected — cap too high");
    }

    #[test]
    fn membership_table_is_node_granular() {
        let mut heap = Heap::new(64);
        let roots = [Gc::nil()];
        // Two nodes from the same block: their addresses differ by exactly one
        // stride, so each confirms the other is a recognized head and the bytes
        // strictly between them are interior (not recognized).
        let g1 = heap.alloc_cons(Gc::fixnum(1), Gc::nil(), &roots);
        let g2 = heap.alloc_cons(Gc::fixnum(2), Gc::nil(), &roots);
        let a1 = g1.node_ptr() as usize;
        let a2 = g2.node_ptr() as usize;
        let stride = Heap::node_size();
        assert_eq!(
            a1.abs_diff(a2),
            stride,
            "consecutive nodes are one stride apart"
        );

        assert!(heap.is_heap_ptr(a1), "valid node head not recognized");
        assert!(heap.is_heap_ptr(a2), "valid node head not recognized");
        // Interior / unaligned addresses within a node must be false.
        assert!(
            !heap.is_heap_ptr(a1 + 1),
            "interior/unaligned must be false"
        );
        assert!(
            !heap.is_heap_ptr(a1 + stride / 2),
            "node interior must be false"
        );
        // A wildly out-of-range address is not ours.
        assert!(!heap.is_heap_ptr(0xdead_beef));
    }

    #[test]
    fn opaque_drop_runs_on_sweep() {
        // Foreign/Stream stand-in: its Rust `Drop` must run when reclaimed.
        struct Resource(Rc<Cell<u32>>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let dropped = Rc::new(Cell::new(0u32));
        let mut heap = Heap::new(64);
        let mut roots = vec![Gc::nil()];

        let obj = heap.alloc_opaque(Box::new(Resource(dropped.clone())), &roots);
        roots[0] = obj;
        heap.collect(&roots);
        assert_eq!(dropped.get(), 0, "live opaque must not be dropped");

        roots[0] = Gc::nil();
        heap.collect(&roots);
        assert_eq!(
            dropped.get(),
            1,
            "swept opaque must run its Drop exactly once"
        );
    }

    #[test]
    fn opaque_drop_runs_on_heap_drop() {
        struct Resource(Rc<Cell<u32>>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let dropped = Rc::new(Cell::new(0u32));
        {
            let mut heap = Heap::new(64);
            let roots = [Gc::nil()];
            let _obj = heap.alloc_opaque(Box::new(Resource(dropped.clone())), &roots);
            // never collected — must still drop when the heap drops.
        }
        assert_eq!(dropped.get(), 1, "heap Drop must run live payloads' Drop");
    }

    #[test]
    fn blob_is_a_leaf() {
        let mut heap = Heap::new(64);
        let mut roots = vec![Gc::nil()];
        let s = heap.alloc_blob(*b"hello", &roots);
        roots[0] = s;
        heap.collect(&roots);
        assert_eq!(heap.blob_bytes(s), b"hello");
        roots[0] = Gc::nil();
        heap.collect(&roots);
        assert_eq!(heap.free_count(), heap.node_count(), "blob not reclaimed");
    }

    #[test]
    fn float_leaf_round_trips_and_reclaims() {
        let mut heap = Heap::new(64);
        let mut roots = vec![Gc::nil()];
        let bits = 3.5f64.to_bits();
        let g = heap.alloc_float(bits, &roots);
        roots[0] = g;
        heap.collect(&roots);
        assert_eq!(f64::from_bits(heap.float_bits(g)), 3.5);
        roots[0] = Gc::nil();
        heap.collect(&roots);
        assert_eq!(heap.free_count(), heap.node_count(), "float not reclaimed");
    }

    #[test]
    fn str_blob_round_trips() {
        let mut heap = Heap::new(64);
        let mut roots = vec![Gc::nil()];
        let g = heap.alloc_str("hello", &roots);
        roots[0] = g;
        heap.collect(&roots);
        assert_eq!(heap.blob_bytes(g), b"hello");
    }

    /// A test stand-in for `value::Closure`: a node that owns some `Gc` edges
    /// (its "captures") and counts its own drops.
    struct FakeClosure {
        captures: Vec<Gc>,
        dropped: Rc<Cell<u32>>,
    }
    impl Drop for FakeClosure {
        fn drop(&mut self) {
            self.dropped.set(self.dropped.get() + 1);
        }
    }
    impl GcObject for FakeClosure {
        fn gc_edges(&self, out: &mut Vec<Gc>) {
            out.extend_from_slice(&self.captures);
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn closure_captures_survive_collect_while_rooted() {
        // The §5 correctness property: a rooted closure keeps its captured
        // handles' nodes alive through a collection (they are *only* reachable
        // via the closure's traced edge list — the analogue of the AOT
        // move-capture shadow vec).
        let mut heap = Heap::new(64);
        let mut roots = vec![Gc::nil(), Gc::nil()];
        let dropped = Rc::new(Cell::new(0u32));

        // A cons cell that nothing roots directly — only the closure captures it.
        let captured = heap.alloc_cons(Gc::fixnum(7), Gc::nil(), &roots);
        roots[0] = captured; // keep it alive across the next alloc only
        let clo = heap.alloc_closure(
            Box::new(FakeClosure {
                captures: vec![captured],
                dropped: dropped.clone(),
            }),
            &roots,
        );
        roots[0] = clo; // now ONLY the closure roots `captured`
        roots[1] = Gc::nil();

        heap.collect(&roots);
        // Both the closure and its captured cons must survive.
        assert_eq!(dropped.get(), 0, "live closure dropped");
        assert_eq!(heap.cons_head(captured).as_fixnum(), 7, "capture freed");
        // Sanity: the closure object downcasts back.
        let obj = heap.closure_obj(clo).as_any().downcast_ref::<FakeClosure>();
        assert!(obj.is_some(), "closure object did not downcast");

        // Drop the only root → both reclaimed, closure Drop runs once.
        roots[0] = Gc::nil();
        heap.collect(&roots);
        assert_eq!(dropped.get(), 1, "swept closure must Drop exactly once");
        assert_eq!(
            heap.free_count(),
            heap.node_count(),
            "closure+capture leaked"
        );
    }

    #[test]
    fn grow_only_never_auto_collects() {
        // Step-3 mode: even far past any reasonable cap, allocation only grows;
        // unreachable garbage is retained (no auto-collect), proving the flag.
        let mut heap = Heap::grow_only();
        let mut roots = vec![Gc::nil()];
        for _ in 0..2000 {
            roots[0] = Gc::nil();
            let _garbage = build_list(&mut heap, 50, &mut roots, 0);
        }
        roots[0] = Gc::nil();
        assert_eq!(heap.collections(), 0, "grow-only heap must never collect");
        assert!(
            heap.node_count() >= 2000 * 50,
            "grow-only heap reused/reclaimed nodes it should have retained"
        );
    }

    #[test]
    fn auto_collection_keeps_heap_bounded() {
        // Drive allocation through the cap so `alloc` collects on its own (no
        // manual `collect`), with a live working set — the bench scenario.
        const WINDOW: usize = 4;
        let n = 500i64;
        let cap = WINDOW * n as usize * 2 + n as usize / 2;
        let mut heap = Heap::new(cap);
        let mut roots = vec![Gc::nil(); WINDOW + 1];

        let mut total = 0i64;
        for it in 0..200usize {
            let xs = build_list(&mut heap, n, &mut roots, WINDOW);
            total = total.wrapping_add(sum_list(&heap, xs));
            roots[it % WINDOW] = xs; // keep the last WINDOW lists live
            roots[WINDOW] = Gc::nil();
        }
        assert_eq!(total, 200i64 * (0..n).sum::<i64>());
        assert!(heap.collections() > 0, "cap never triggered a collection");
        assert!(
            heap.peak_live() as i64 >= n,
            "peak live {} < one list {n} — live set not traced",
            heap.peak_live()
        );
        // Bounded: near cap, not 200 * n.
        assert!(heap.node_count() < cap + BLOCK_SIZE);
    }
}
