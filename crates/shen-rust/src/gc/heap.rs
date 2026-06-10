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
use std::collections::BTreeMap;
use std::ptr::{slice_from_raw_parts_mut, with_exposed_provenance, with_exposed_provenance_mut};

use super::node::{Kind, Node};
use super::{Gc, GcObject, HeapKind};

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
    /// precise-root `alloc`/`collect` hot path. A `BTreeMap` (not `HashMap`) so
    /// it has a `const` constructor, which lets [`Heap::grow_only`] be `const fn`
    /// and the value-layer thread-local use `const`-init (no per-access lazy-init
    /// guard on the hot path). Off the hot path, so O(log n) vs O(1) is moot.
    pages: BTreeMap<usize, Vec<usize>>,
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
    /// **GC Step 4 request mode**: when `true`, allocation never collects
    /// (unlike the `collection_enabled` cap path) — instead [`Heap::grow`]
    /// raises [`Heap::gc_pending`] once footprint outgrows the last live set
    /// by [`Heap::next_trigger`], and the interpreter runs
    /// [`Heap::collect_at_safepoint`] at its next quiescent point (activation
    /// depth 0), where the hybrid root set (precise containers + conservative
    /// native-stack scan) is sound. See `design/` GC Step 4 notes.
    request_mode: bool,
    /// Deferred-collection request, set in [`Heap::grow`], consumed (cleared)
    /// by [`Heap::collect_at_safepoint`].
    gc_pending: bool,
    /// Live-node count measured by the most recent sweep.
    last_live: usize,
    /// `last_live` snapshot at the end of the previous safepoint collection;
    /// the trigger measures footprint growth relative to this.
    live_at_last_collect: usize,
    /// Raise `gc_pending` when `all.len() - live_at_last_collect` reaches
    /// this. Recomputed after each safepoint collection to
    /// `max(last_live, min_trigger)` — i.e. collect roughly when the heap has
    /// doubled past the live set, so collection count grows logarithmically
    /// with peak demand and one-shot runs stay essentially collection-free.
    next_trigger: usize,
    /// Floor for `next_trigger` (avoids thrashing tiny heaps). Set by
    /// [`Heap::enable_request_gc`]; overridable via `SHEN_RUST_GC=<nodes>`.
    min_trigger: usize,
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
            pages: BTreeMap::new(),
            cap,
            collection_enabled: true,
            request_mode: false,
            gc_pending: false,
            last_live: 0,
            live_at_last_collect: 0,
            next_trigger: usize::MAX,
            min_trigger: 0,
            mark_stack: Vec::new(),
            collections: 0,
            peak_live: 0,
        }
    }

    /// A new **grow-only** heap that never auto-collects. Allocation only
    /// ever grows the heap; nothing is reclaimed except explicit
    /// [`Heap::collect`] / [`Heap::collect_at_safepoint`] calls and the final
    /// [`Heap::drop`]. This is the live TLS heap's starting mode (and its
    /// permanent mode unless [`Heap::enable_request_gc`] — GC Step 4 — is
    /// invoked). See the `collection_enabled` field and
    /// `design/gc-step3-value-flip-handoff.md` §2.
    ///
    /// `const fn` so the value-layer thread-local can `const`-init it — that
    /// removes the per-access lazy-initialization branch the `thread_local!`
    /// macro otherwise emits, which sits on every `Value` heap access.
    pub const fn grow_only() -> Heap {
        Heap {
            blocks: Vec::new(),
            all: Vec::new(),
            free: Vec::new(),
            ranges: Vec::new(),
            pages: BTreeMap::new(),
            // Cap is irrelevant when collection is disabled, but keep it large
            // so the invariant "never collect" holds even if a future caller
            // flips the flag without setting a cap.
            cap: usize::MAX,
            collection_enabled: false,
            request_mode: false,
            gc_pending: false,
            last_live: 0,
            live_at_last_collect: 0,
            next_trigger: usize::MAX,
            min_trigger: 0,
            mark_stack: Vec::new(),
            collections: 0,
            peak_live: 0,
        }
    }

    /// Switch this (grow-only) heap to **request mode** (GC Step 4): from now
    /// on, [`Heap::grow`] raises [`Heap::gc_pending`] whenever the footprint
    /// has grown `max(last_live, floor)` nodes past the last live set, and the
    /// owner is expected to call [`Heap::collect_at_safepoint`] at its next
    /// quiescent point. Allocation itself still never collects
    /// (`collection_enabled` stays `false`) — that is the whole point: the
    /// root set is only sound at owner-chosen safepoints.
    pub fn enable_request_gc(&mut self, floor: usize) {
        self.request_mode = true;
        self.min_trigger = floor.max(1);
        self.next_trigger = self.min_trigger;
    }

    /// Has request-mode collection been enabled on this heap?
    pub fn request_gc_enabled(&self) -> bool {
        self.request_mode
    }

    /// Deferred-collection request flag (see [`Heap::enable_request_gc`]).
    #[inline]
    pub fn gc_pending(&self) -> bool {
        self.gc_pending
    }

    /// Run a collection at an owner-chosen quiescent point, then recompute the
    /// trigger and clear [`Heap::gc_pending`].
    ///
    /// Roots are the **hybrid set**: `precise` must contain every root the
    /// owner tracks in heap containers (environment tables, caches, pins);
    /// `Value`s living in native stack frames or callee-saved registers are
    /// found by the conservative scan (see `gc::stack` — on targets where the
    /// scan is unsupported it compiles out, request-mode enable refuses, and
    /// this is precise-only, which is what the miri tests exercise).
    pub fn collect_at_safepoint(&mut self, precise: &[Gc]) {
        self.collect_inner(precise, &[], true);
        self.live_at_last_collect = self.last_live;
        self.next_trigger = self.last_live.max(self.min_trigger);
        self.gc_pending = false;
    }

    // ---- allocation --------------------------------------------------------

    /// Allocate a cons cell. `head`/`tail` that are heap pointers **must** also
    /// be in `roots` (a collection may fire here). See the type docs.
    #[inline]
    pub fn alloc_cons(&mut self, head: Gc, tail: Gc, roots: &[Gc]) -> Gc {
        // Self-root the in-flight operands: a collection fired by this very
        // allocation must keep `head`/`tail` alive even when the caller's
        // `roots` omits them (belt-and-braces for the `Heap::new(cap)`
        // auto-collect path; free on the live request-mode heap, which never
        // collects inside alloc).
        let p = self.obtain_slot_with(roots, &[head, tail]);
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
        // Self-root the cells (see `alloc_cons`): they are boxed only after a
        // slot is obtained, so a collection here must trace them directly.
        let p = self.obtain_slot_with(roots, &cells);
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

    /// Allocate an immutable error-object leaf ([`Kind::Error`]) from its UTF-8
    /// message bytes. Same storage as a blob; a distinct kind so error objects
    /// are distinguishable from strings.
    pub fn alloc_err(&mut self, s: &str, roots: &[Gc]) -> Gc {
        let p = self.obtain_slot(roots);
        let bytes: Box<[u8]> = Box::from(s.as_bytes());
        let len = bytes.len() as u64;
        let a = expose(Box::into_raw(bytes));
        // SAFETY: fresh free slot; `a`/`len` own the boxed bytes until reclaimed.
        unsafe {
            (*p).kind = Kind::Error;
            (*p).mark = false;
            (*p).a = a;
            (*p).b = len;
        }
        Gc::from_node(p)
    }

    /// Allocate a closure node owning `obj`. The collector traces `obj`'s
    /// [`GcObject::gc_edges`] on mark and runs its Rust `Drop` on sweep.
    pub fn alloc_closure(&mut self, obj: Box<dyn GcObject>, roots: &[Gc]) -> Gc {
        // Self-root the closure's edges (see `alloc_cons`): only the cap path
        // can collect inside alloc, so the edge walk is skipped on the live
        // (request-mode / grow-only) heap where it would be pure overhead.
        let mut operands: Vec<Gc> = Vec::new();
        if self.collection_enabled {
            obj.gc_edges(&mut operands);
        }
        let p = self.obtain_slot_with(roots, &operands);
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
        self.obtain_slot_with(roots, &[])
    }

    /// As [`Heap::obtain_slot`], with a second root slice for the allocation's
    /// own in-flight operands (cons head/tail, vec cells, closure edges) —
    /// traced alongside `roots` if the cap path collects here.
    #[inline]
    fn obtain_slot_with(&mut self, roots: &[Gc], operands: &[Gc]) -> *mut Node {
        if self.free.is_empty() {
            if self.collection_enabled && self.all.len() >= self.cap {
                self.collect2(roots, operands);
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
        // Request-mode trigger (GC Step 4). Checked here — not on the alloc
        // fast path — because `grow` runs at most once per `BLOCK_SIZE`
        // allocations: the cost is amortized to ~zero. The collection itself
        // is deferred to the owner's next safepoint; this call still grows,
        // so allocation always succeeds in between.
        if self.request_mode
            && self.all.len().saturating_sub(self.live_at_last_collect) >= self.next_trigger
        {
            self.gc_pending = true;
        }
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
        self.collect_inner(roots, &[], false);
    }

    /// [`Heap::collect`] with a second root slice (in-flight alloc operands).
    fn collect2(&mut self, roots: &[Gc], extra: &[Gc]) {
        self.collect_inner(roots, extra, false);
    }

    /// The full mark-sweep: mark from the conservative native-stack scan (if
    /// requested and supported), then from both precise slices, trace, sweep.
    fn collect_inner(&mut self, roots: &[Gc], extra: &[Gc], scan_stack: bool) {
        // ---- mark ----
        self.mark_stack.clear();
        if scan_stack {
            self.scan_native_roots();
        }
        for &r in roots {
            self.mark_edge(r);
        }
        for &r in extra {
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
        self.last_live = live;
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
            let kind = (*p).kind;
            let a = (*p).a;
            let b = (*p).b;
            // Reset the node BEFORE running any payload `Drop`: if a payload
            // Drop panics (e.g. the debug heap-reentry sentinel tripping on a
            // forbidden funnel call), the unwind then passes an
            // already-`Free` node, and `Heap::drop`'s second `free_resource`
            // pass is a no-op instead of a double-free.
            (*p).kind = Kind::Free;
            // Debug builds poison the freed words instead of zeroing them: a
            // missed root that reads a reclaimed node then yields a
            // deterministic, recognizable pattern (tag bits 0b101 — not a
            // pointer, not nil) rather than silently aliasing the slot's next
            // occupant. Every alloc_* writes both words, so the poison never
            // escapes through a legitimate path.
            #[cfg(debug_assertions)]
            {
                (*p).a = 0xDEAD_DEAD_DEAD_DEAD;
                (*p).b = 0xDEAD_DEAD_DEAD_DEAD;
            }
            #[cfg(not(debug_assertions))]
            {
                (*p).a = 0;
                (*p).b = 0;
            }
            match kind {
                Kind::Vec => {
                    let vp = with_exposed_provenance_mut::<Vec<Gc>>(a as usize);
                    drop(Box::from_raw(vp));
                }
                Kind::Blob | Kind::Error => {
                    let data = with_exposed_provenance_mut::<u8>(a as usize);
                    let slice = slice_from_raw_parts_mut(data, b as usize);
                    drop(Box::from_raw(slice));
                }
                Kind::Closure => {
                    let bp = with_exposed_provenance_mut::<Box<dyn GcObject>>(a as usize);
                    drop(Box::from_raw(bp));
                }
                Kind::Opaque => {
                    let bp = with_exposed_provenance_mut::<Box<dyn Any>>(a as usize);
                    drop(Box::from_raw(bp));
                }
                // `Float` is a leaf with the bits inline in `a` — no owned
                // allocation, nothing to free. `Cons`/`Free` likewise.
                Kind::Float | Kind::Cons | Kind::Free => {}
            }
        }
    }

    /// Conservatively mark roots found on the native stack and in flushed
    /// callee-saved registers (the §6g hybrid — see `gc::stack`). Sound for
    /// this NON-MOVING collector: a false-positive word can only over-retain.
    /// Compiled out under miri and on unsupported targets (where request-mode
    /// enable refuses, so nothing depends on it).
    #[cfg(all(
        target_arch = "aarch64",
        any(target_os = "macos", target_os = "linux"),
        not(miri)
    ))]
    fn scan_native_roots(&mut self) {
        let mut regbuf = [0u64; 10];
        super::stack::flush_callee_saved(&mut regbuf);
        for &w in &regbuf {
            self.mark_conservative(w);
        }
        let lo = super::stack::current_sp() & !0b111;
        let hi = super::stack::stack_base();
        assert!(
            lo < hi,
            "stack scan bounds inverted (sp {lo:#x} >= base {hi:#x})"
        );
        let mut a = lo;
        while a < hi {
            // SAFETY: reading our own live thread's stack region as raw
            // 8-aligned words — the same read the §6g spike validated. The
            // region is mapped (it is between our own sp and the pthread
            // stack base); values are inspected as integers only. Excluded
            // from miri, which cannot reason about this.
            let w = unsafe { core::ptr::read_volatile(a as *const u64) };
            self.mark_conservative(w);
            a += 8;
        }
    }

    #[cfg(not(all(
        target_arch = "aarch64",
        any(target_os = "macos", target_os = "linux"),
        not(miri)
    )))]
    fn scan_native_roots(&mut self) {
        // Precise-only configuration (miri / unsupported target). Request-mode
        // enable refuses on these targets, so no live heap relies on the scan.
    }

    /// Treat `w` as a potential root word from the conservative scan: mark it
    /// iff it is a `TAG_PTR`-tagged address of a live (non-`Free`) node we
    /// own. Stale words naming freed slots are skipped — without the `Free`
    /// check they would pin recycled slots off the free-list for a cycle.
    #[cfg(all(
        target_arch = "aarch64",
        any(target_os = "macos", target_os = "linux"),
        not(miri)
    ))]
    fn mark_conservative(&mut self, w: u64) {
        let g = Gc::from_bits(w);
        if !g.is_ptr() || !self.is_heap_ptr(g.addr()) {
            return;
        }
        // SAFETY: `is_heap_ptr` verified `w` names the head of a node we own.
        if unsafe { (*g.node_ptr()).kind } == Kind::Free {
            return;
        }
        self.mark_edge(g);
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
            Kind::Float | Kind::Blob | Kind::Error | Kind::Opaque | Kind::Free => {}
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

    /// Classify the node a heap-pointer `g` names, for the value layer's
    /// tag-dispatch. Returns `None` if `g` is not a heap pointer (an immediate)
    /// or names a `Free` node (never handed out live).
    pub fn classify(&self, g: Gc) -> Option<HeapKind> {
        if !g.is_ptr() {
            return None;
        }
        let p = g.node_ptr();
        // SAFETY: a `TAG_PTR` handle names one of our live nodes.
        Some(match unsafe { (*p).kind } {
            Kind::Cons => HeapKind::Cons,
            Kind::Vec => HeapKind::Vec,
            Kind::Blob => HeapKind::Str,
            Kind::Error => HeapKind::Error,
            Kind::Float => HeapKind::Float,
            Kind::Closure => HeapKind::Closure,
            Kind::Opaque => HeapKind::Opaque,
            Kind::Free => return None,
        })
    }

    // ---- raw-pointer accessors (the value layer's lifetime bridge) ---------
    //
    // These return a *raw pointer* into a node rather than a borrow, so the
    // caller can drop the thread-local heap borrow and then form a reference
    // whose lifetime it ties to its own `&self` — sound because the heap is
    // non-moving (the node address is stable) and a node is never freed while
    // reachable. With Step-4 request-mode collection, "while reachable" is
    // load-bearing: collection runs only at interpreter depth-0 safepoints,
    // where no interpreter-internal bridged borrow exists, and hosts must not
    // hold one across `Interp::eval`/`apply` (see the `value.rs` module
    // "Collection" note). See `value::Value::{head,tail,as_str}`.

    /// Raw pointers to a cons node's `(head, tail)` words, or `None` if `g` is
    /// not a cons node. Each `*const Gc` aliases a payload word in the pinned
    /// node; it stays valid while `g` is reachable.
    pub fn cons_word_ptrs(&self, g: Gc) -> Option<(*const Gc, *const Gc)> {
        if !g.is_ptr() {
            return None;
        }
        let p = g.node_ptr();
        // SAFETY: a `TAG_PTR` handle names one of our live nodes.
        if unsafe { (*p).kind } != Kind::Cons {
            return None;
        }
        // The payload words are `u64`; a `Gc` is `repr(transparent)` over `u64`,
        // so `&(*p).a as *const u64 as *const Gc` is a valid reinterpretation.
        unsafe {
            let head = core::ptr::addr_of!((*p).a) as *const Gc;
            let tail = core::ptr::addr_of!((*p).b) as *const Gc;
            Some((head, tail))
        }
    }

    /// Raw `(data, len)` of a string ([`Kind::Blob`]) node, or `None`. The
    /// pointer aliases the node's owned byte buffer, valid while `g` is rooted.
    pub fn blob_raw(&self, g: Gc) -> Option<(*const u8, usize)> {
        self.bytes_raw(g, Kind::Blob)
    }

    /// Raw `(data, len)` of an error ([`Kind::Error`]) node, or `None`.
    pub fn err_raw(&self, g: Gc) -> Option<(*const u8, usize)> {
        self.bytes_raw(g, Kind::Error)
    }

    #[inline]
    fn bytes_raw(&self, g: Gc, kind: Kind) -> Option<(*const u8, usize)> {
        if !g.is_ptr() {
            return None;
        }
        let p = g.node_ptr();
        // SAFETY: a `TAG_PTR` handle names one of our live nodes.
        unsafe {
            if (*p).kind != kind {
                return None;
            }
            let data = with_exposed_provenance::<u8>((*p).a as usize);
            Some((data, (*p).b as usize))
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

    /// Hot-path combined accessor: if `g` is a closure node, return a raw
    /// pointer to its object in **one** heap access (vs `classify` *then*
    /// `closure_obj` — two thread-local round-trips). `None` for any non-closure.
    /// The pointer is valid while `g` stays reachable (non-moving heap; see
    /// the lifetime-bridge note above for the Step-4 collection rules).
    #[inline]
    pub fn closure_obj_ptr(&self, g: Gc) -> Option<*const dyn GcObject> {
        if !g.is_ptr() {
            return None;
        }
        let p = g.node_ptr();
        // SAFETY: a `TAG_PTR` handle names one of our live nodes.
        unsafe {
            if (*p).kind != Kind::Closure {
                return None;
            }
            let bp = with_exposed_provenance::<Box<dyn GcObject>>((*p).a as usize);
            Some(&**bp as *const dyn GcObject)
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

    /// Live-node count measured by the most recent sweep (0 before any).
    pub fn last_live(&self) -> usize {
        self.last_live
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
    fn in_flight_operands_survive_alloc_collect() {
        // The roots:&[] hole: a cap-path collection fired by an allocation
        // must not sweep the operands of that very allocation, even when the
        // caller's `roots` slice omits them.
        let mut heap = Heap::new(64);
        for _ in 0..BLOCK_SIZE - 1 {
            heap.alloc_cons(Gc::fixnum(0), Gc::nil(), &[]);
        }
        // Takes the block's last free slot; nothing roots it.
        let inner = heap.alloc_cons(Gc::fixnum(42), Gc::nil(), &[]);
        // Free-list now empty and len >= cap: this alloc collects. `inner`
        // must survive purely as an in-flight operand.
        let outer = heap.alloc_cons(inner, Gc::nil(), &[]);
        assert_eq!(heap.collections(), 1, "cap collection should have fired");
        assert_eq!(heap.cons_head(heap.cons_head(outer)).as_fixnum(), 42);
    }

    #[test]
    fn in_flight_vec_cells_survive_alloc_collect() {
        let mut heap = Heap::new(64);
        for _ in 0..BLOCK_SIZE - 1 {
            heap.alloc_cons(Gc::fixnum(0), Gc::nil(), &[]);
        }
        let cell = heap.alloc_cons(Gc::fixnum(7), Gc::nil(), &[]);
        let v = heap.alloc_vec(vec![cell], &[]);
        assert_eq!(heap.collections(), 1);
        assert_eq!(heap.cons_head(heap.vec_get(v, 0)).as_fixnum(), 7);
    }

    #[test]
    fn request_mode_defers_to_safepoint() {
        // Request mode: alloc never collects; `grow` raises the pending flag
        // once footprint outgrows the trigger; collect_at_safepoint reclaims,
        // recomputes the trigger, and clears the flag.
        let mut heap = Heap::grow_only();
        heap.enable_request_gc(BLOCK_SIZE);
        assert!(heap.request_gc_enabled());
        assert!(!heap.gc_pending());

        let mut roots = vec![Gc::nil()];
        // Two blocks' worth of garbage: the second `grow` sees
        // all.len() (1024) - live_at_last_collect (0) >= trigger (1024).
        build_list(&mut heap, 2 * BLOCK_SIZE as i64, &mut roots, 0);
        assert!(heap.gc_pending(), "grow should have raised the request");
        assert_eq!(heap.collections(), 0, "alloc must never collect here");

        // Keep a small live list; everything else is garbage.
        roots[0] = Gc::nil();
        let live = build_list(&mut heap, 10, &mut roots, 0);
        roots[0] = live;
        heap.collect_at_safepoint(&roots);
        assert!(!heap.gc_pending());
        assert_eq!(heap.collections(), 1);
        assert_eq!(heap.last_live(), 10);
        assert_eq!(sum_list(&heap, live), (0..10).sum::<i64>());
        // Trigger recomputed to max(live, floor) = floor here.
        assert!(heap.free_count() > 0, "sweep must rebuild the free-list");
    }

    #[test]
    fn request_mode_trigger_doubles_with_live_set() {
        let mut heap = Heap::grow_only();
        heap.enable_request_gc(64);
        let mut roots = vec![Gc::nil()];
        // Big live set, then a safepoint collect: next trigger = live size.
        let live = build_list(&mut heap, 3 * BLOCK_SIZE as i64, &mut roots, 0);
        roots[0] = live;
        heap.collect_at_safepoint(&roots);
        assert_eq!(heap.last_live(), 3 * BLOCK_SIZE);
        assert!(!heap.gc_pending());

        // Allocating less than a live-set's worth must NOT re-raise the flag
        // (footprint grew < next_trigger past the last collect)...
        roots.push(Gc::nil());
        build_list(&mut heap, (BLOCK_SIZE / 2) as i64, &mut roots, 1);
        assert!(!heap.gc_pending(), "trigger fired too early");
        // ...but doubling past it must.
        let mut more = vec![live, Gc::nil()];
        let mut xs = Gc::nil();
        for i in 0..(4 * BLOCK_SIZE) as i64 {
            more[1] = xs;
            xs = heap.alloc_cons(Gc::fixnum(i), xs, &more);
            more[1] = xs;
        }
        assert!(heap.gc_pending(), "doubling past the live set must trigger");
    }

    /// Conservative-scan integration tests — only meaningful where the scan
    /// is compiled in (aarch64 + macOS/Linux, not miri).
    #[cfg(all(
        target_arch = "aarch64",
        any(target_os = "macos", target_os = "linux"),
        not(miri)
    ))]
    mod scan {
        use super::*;
        use std::hint::black_box;

        /// Build a list whose head ends up ONLY in the caller's frame /
        /// registers — no external root container survives the return.
        #[inline(never)]
        fn build_keeper(heap: &mut Heap, n: i64) -> Gc {
            let mut roots = vec![Gc::nil()];
            build_list(heap, n, &mut roots, 0)
        }

        /// Build garbage rooted only in this (immediately popped) frame.
        #[inline(never)]
        fn build_garbage(heap: &mut Heap, n: i64) {
            let mut roots = vec![Gc::nil()];
            let xs = build_list(heap, n, &mut roots, 0);
            assert_eq!(sum_list(heap, xs), (0..n).sum::<i64>());
        }

        #[test]
        fn stack_local_survives_safepoint_collect() {
            // The §6g property on the REAL heap: a head held only in a host
            // stack slot / callee-saved register — with NO precise root —
            // survives a safepoint collection via the conservative scan.
            let mut heap = Heap::grow_only();
            heap.enable_request_gc(64);
            let keeper = black_box(build_keeper(&mut heap, 50));
            heap.collect_at_safepoint(&[]);
            assert_eq!(
                sum_list(&heap, black_box(keeper)),
                (0..50).sum::<i64>(),
                "conservative scan missed a live stack root"
            );
        }

        #[test]
        fn popped_frame_garbage_is_mostly_reclaimed() {
            // The depth-0 over-retention claim: garbage whose only handles
            // lived in an already-popped callee frame is reclaimable at a
            // shallow safepoint. Stale spill slots can pin stragglers (that
            // is the conservative tax), so assert "mostly", not "all" — the
            // spike's pathological mid-descent case measured 7.7x retained;
            // here the bulk must be free.
            let mut heap = Heap::grow_only();
            heap.enable_request_gc(64);
            build_garbage(&mut heap, 5_000);
            let keeper = black_box(build_keeper(&mut heap, 10));
            heap.collect_at_safepoint(&[]);
            assert!(
                heap.free_count() * 2 > heap.node_count(),
                "over-retention: {} of {} nodes still pinned after a \
                 shallow-depth collect",
                heap.node_count() - heap.free_count(),
                heap.node_count()
            );
            assert_eq!(sum_list(&heap, black_box(keeper)), (0..10).sum::<i64>());
        }

        #[test]
        fn precise_and_conservative_roots_compose() {
            let mut heap = Heap::grow_only();
            heap.enable_request_gc(64);
            let mut precise = vec![Gc::nil()];
            let in_container = build_list(&mut heap, 20, &mut precise, 0);
            precise[0] = in_container;
            let on_stack = black_box(build_keeper(&mut heap, 30));
            heap.collect_at_safepoint(&precise);
            assert_eq!(sum_list(&heap, in_container), (0..20).sum::<i64>());
            assert_eq!(sum_list(&heap, black_box(on_stack)), (0..30).sum::<i64>());
        }
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
