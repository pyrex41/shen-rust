//! SPIKE — does the Copy-heap-reference win SURVIVE A REAL COLLECTOR?
//!
//! This is the gating experiment for the GC ladder (perf-state-and-gc-ladder.md
//! §6e / HANDOFF.md Step 2). The prior `value_repr_spike` established the
//! *ceiling*: an 8-byte `Copy` tagged word over a **leaked** arena runs the
//! list workload ~2.48x faster than today's 24-byte `Rc` enum, because heap
//! references become `Copy` (no refcount per clone/drop). But that ceiling
//! LEAKS — no reclamation ever happens. The open question, on which the whole
//! ladder hinges, is:
//!
//!   > Once a real tracing collector runs (mark + sweep, bounded heap, actual
//!   > reclamation), how much of that 2.48x survives — after paying for the
//!   > mark/sweep tracing the leaked version never did?
//!
//! KILL-CRITERION (from the handoff): reproduce a *material fraction* of the
//! 2.48x Option-B ceiling **with real collection happening** (heap stays
//! bounded; collections > 0), AND demonstrate a workable precise-roots story.
//! If the surviving win is marginal, the GC ladder is not justified and the
//! fallback (fixnums-only, ~1.6x on arithmetic, no GC) is the play instead.
//!
//! What this spike models (and does NOT):
//! * Algorithm: non-moving **mark-sweep** with a `Copy` tagged-word handle —
//!   option 1 in perf-state §6c, the leaning choice. Nodes have stable
//!   addresses (individually heap-allocated, never moved), so a tagged pointer
//!   is a valid `Copy` reference. Reclaimed nodes go on a free-list and are
//!   reused (no malloc/free churn in steady state).
//! * Precise roots via an explicit **shadow stack** (`Vec<Word>`): exactly the
//!   mechanism the real VM would use (it already owns an explicit value-stack).
//!   The workload pushes/pops its live roots; collection traces only from them.
//! * NOT modeled: GC roots living in **AOT-compiled Rust stack frames** — the
//!   single biggest design risk (perf-state §6d). A standalone microbench
//!   cannot exercise real AOT frames; that interaction must be prototyped
//!   separately before any full conversion. This spike de-risks the
//!   *representation + collector throughput* question only.
//!
//! Run: `cargo bench --bench gc_spike`   (harness = false)

#![allow(dead_code)]

use std::hint::black_box;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Live working set: every rep keeps the last `WINDOW` lists reachable, so a
/// collection has a realistic, non-empty root set to trace (≈ `WINDOW * n`
/// live cons cells) — making the mark phase pay its honest cost.
const WINDOW: usize = 4;

// ---------------------------------------------------------------------------
// Baseline A: today's reality — 24-byte Rc enum, full Rc reclamation per iter.
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Boxed {
    Nil,
    Int(i64),
    Cons(Rc<(Boxed, Boxed)>),
}

impl Boxed {
    #[inline]
    fn cons(a: Boxed, b: Boxed) -> Boxed {
        Boxed::Cons(Rc::new((a, b)))
    }
}

/// Build a length-`n` list, sum it, and install it into a `WINDOW`-slot ring
/// of *live* lists (evicting — and so dropping, via Rc cascade — the oldest).
/// The ring gives every rep the SAME realistic working set of live cons cells
/// (so the GC's mark phase has real data to trace, not an empty root set — the
/// flaw in the first cut of this spike), and the same reclamation cadence.
fn boxed_bench(iters: u32, n: i64) -> i64 {
    let mut ring: Vec<Boxed> = (0..WINDOW).map(|_| Boxed::Nil).collect();
    let mut total = 0i64;
    for it in 0..iters as usize {
        let mut xs = Boxed::Nil;
        for i in 0..n {
            xs = Boxed::cons(Boxed::Int(i), xs);
        }
        let mut acc = 0i64;
        let mut cur: &Boxed = &xs;
        while let Boxed::Cons(p) = cur {
            if let Boxed::Int(v) = &p.0 {
                acc = acc.wrapping_add(*v);
            }
            cur = &p.1;
        }
        total = total.wrapping_add(acc);
        // Evicted list drops here: Rc decrement cascades the chain (real
        // reclamation). The GC instead batches this into a later sweep.
        ring[it % WINDOW] = xs;
    }
    total
}

// ---------------------------------------------------------------------------
// The tagged Copy word, shared by the leaked ceiling and the GC version.
//
// Low 3 bits tag. 000 = fixnum (61-bit inline). 001 = cons (pointer to a node,
// 8-aligned so low bits are free). 011 = nil. The word is `Copy`: assigning /
// reading / traversing it does NO refcount work — that is the whole point.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
struct Word(u64);

const TAG_MASK: u64 = 0b111;
const TAG_BITS: u64 = 3;
const TAG_FIXNUM: u64 = 0b000;
const TAG_CONS: u64 = 0b001;
const TAG_NIL: u64 = 0b011;

impl Word {
    #[inline]
    fn nil() -> Word {
        Word(TAG_NIL)
    }
    #[inline]
    fn fixnum(v: i64) -> Word {
        Word(((v as u64) << TAG_BITS) | TAG_FIXNUM)
    }
    #[inline]
    fn is_fixnum(self) -> bool {
        (self.0 & TAG_MASK) == TAG_FIXNUM
    }
    #[inline]
    fn as_fixnum(self) -> i64 {
        (self.0 as i64) >> TAG_BITS
    }
    #[inline]
    fn is_cons(self) -> bool {
        (self.0 & TAG_MASK) == TAG_CONS
    }
    #[inline]
    fn cons_ptr(self) -> *mut GcCons {
        (self.0 & !TAG_MASK) as *mut GcCons
    }
    #[inline]
    fn from_ptr(p: *mut GcCons) -> Word {
        Word((p as u64) | TAG_CONS)
    }
}

// ---------------------------------------------------------------------------
// Ceiling: 8-byte Copy word, leaked arena, NO reclamation (Option-B ceiling).
// Identical to `value_repr_spike`'s tagged_leak — re-stated here so this bench
// is standalone. This is the number a perfect (free) GC could not exceed.
// ---------------------------------------------------------------------------

#[repr(align(8))]
struct LeakNode {
    head: Word,
    tail: Word,
}

fn leak_bench(iters: u32, n: i64) -> i64 {
    let mut ring = [Word::nil(); WINDOW]; // shape parity; leaked nodes never freed
    let mut total = 0i64;
    for it in 0..iters as usize {
        let mut xs = Word::nil();
        for i in 0..n {
            let p = Box::into_raw(Box::new(LeakNode {
                head: Word::fixnum(i),
                tail: xs,
            }));
            xs = Word::from_ptr(p as *mut GcCons);
        }
        let mut acc = 0i64;
        let mut cur = xs;
        while cur.is_cons() {
            // SAFETY: leaked pointer, never freed within the bench.
            let p = cur.cons_ptr() as *const LeakNode;
            unsafe {
                acc = acc.wrapping_add((*p).head.as_fixnum());
                cur = (*p).tail;
            }
        }
        total = total.wrapping_add(acc);
        ring[it % WINDOW] = xs; // evicted list just leaks — no reclamation (ceiling)
    }
    total
}

// ---------------------------------------------------------------------------
// The real thing: non-moving mark-sweep heap with a free-list.
// ---------------------------------------------------------------------------

#[repr(align(8))]
struct GcCons {
    head: Word,
    tail: Word,
    mark: bool,
}

struct Heap {
    /// Every node ever allocated (stable addresses — never moved). Owns them;
    /// freed in `Drop`. Iterated during sweep.
    all: Vec<*mut GcCons>,
    /// Reclaimable nodes, reused before growing.
    free: Vec<*mut GcCons>,
    /// Soft trigger: once `all.len() >= cap` and `free` is empty, collect
    /// before growing. Keeps the heap bounded so collection actually runs.
    cap: usize,
    /// Reused mark-phase work stack (avoids a per-collection allocation).
    mark_stack: Vec<Word>,
    collections: u64,
    peak_live: usize,
}

impl Heap {
    fn new(cap: usize) -> Heap {
        Heap {
            all: Vec::new(),
            free: Vec::new(),
            cap,
            mark_stack: Vec::new(),
            collections: 0,
            peak_live: 0,
        }
    }

    /// Allocate a cons. May trigger a collection (tracing from `roots`) when
    /// the free-list is empty and the heap is at its soft cap. `roots` MUST
    /// reflect every live `Word` at the call site, or live nodes get reclaimed.
    #[inline]
    fn alloc(&mut self, head: Word, tail: Word, roots: &[Word]) -> Word {
        if self.free.is_empty() {
            if self.all.len() >= self.cap {
                self.collect(roots);
            }
            if self.free.is_empty() {
                // Genuinely out of room (heap below cap, or everything live):
                // grow by one real allocation.
                let p = Box::into_raw(Box::new(GcCons {
                    head: Word::nil(),
                    tail: Word::nil(),
                    mark: false,
                }));
                self.all.push(p);
                self.free.push(p);
            }
        }
        let p = self.free.pop().unwrap();
        // SAFETY: `p` came from `free`, so it is not reachable from any root
        // (sweep only frees unmarked nodes) and we hold no other reference.
        unsafe {
            (*p).head = head;
            (*p).tail = tail;
            (*p).mark = false;
        }
        Word::from_ptr(p)
    }

    /// Mark-sweep. Mark: DFS from every root, set `mark` on reachable nodes.
    /// Sweep: rebuild the free-list from unmarked nodes (free nodes are
    /// unmarked, so they correctly stay free); clear marks on survivors.
    fn collect(&mut self, roots: &[Word]) {
        // ---- mark ----
        self.mark_stack.clear();
        self.mark_stack.extend_from_slice(roots);
        while let Some(w) = self.mark_stack.pop() {
            if !w.is_cons() {
                continue;
            }
            let p = w.cons_ptr();
            // SAFETY: `p` is a live cons pointer from a root or a traced edge;
            // all nodes outlive the collection (freed only in Heap::drop).
            unsafe {
                if (*p).mark {
                    continue;
                }
                (*p).mark = true;
                self.mark_stack.push((*p).head);
                self.mark_stack.push((*p).tail);
            }
        }
        // ---- sweep ----
        self.free.clear();
        let mut live = 0usize;
        for &p in &self.all {
            unsafe {
                if (*p).mark {
                    (*p).mark = false;
                    live += 1;
                } else {
                    self.free.push(p);
                }
            }
        }
        if live > self.peak_live {
            self.peak_live = live;
        }
        self.collections += 1;
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        for &p in &self.all {
            // SAFETY: each pointer came from `Box::into_raw`; freed exactly once.
            unsafe { drop(Box::from_raw(p)) };
        }
    }
}

/// GC version. Cons cells come from the collected heap; the live list head is
/// kept on the shadow stack (`roots`) so a collection triggered mid-build can't
/// reclaim it. A `WINDOW`-slot ring (roots[0..WINDOW]) holds the live working
/// set; `roots[WINDOW]` is the transient build slot.
fn gc_bench(heap: &mut Heap, iters: u32, n: i64) -> i64 {
    // roots[0..WINDOW] = live ring; roots[WINDOW] = current build slot.
    let mut roots: Vec<Word> = vec![Word::nil(); WINDOW + 1];
    const BUILD: usize = WINDOW;
    let mut total = 0i64;
    for it in 0..iters as usize {
        roots[BUILD] = Word::nil();
        let mut xs = Word::nil();
        for i in 0..n {
            // Keep the chain built so far reachable across a possible GC
            // *inside* alloc. The fixnum addend is immediate — not a root.
            roots[BUILD] = xs;
            xs = heap.alloc(Word::fixnum(i), xs, &roots);
            roots[BUILD] = xs;
        }
        // Sum: pure Copy-word traversal — no refcount, no alloc, no GC.
        let mut acc = 0i64;
        let mut cur = xs;
        while cur.is_cons() {
            let p = cur.cons_ptr();
            // SAFETY: `cur` is rooted (BUILD slot holds the head) for the walk.
            unsafe {
                acc = acc.wrapping_add((*p).head.as_fixnum());
                cur = (*p).tail;
            }
        }
        total = total.wrapping_add(acc);
        // Install into the live ring; the evicted list becomes garbage,
        // reclaimed by a LATER sweep (batched), not eagerly like Rc.
        roots[it % WINDOW] = xs;
        roots[BUILD] = Word::nil();
    }
    total
}

fn mean(ds: &[Duration]) -> Duration {
    ds.iter().sum::<Duration>() / ds.len() as u32
}
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    println!("GC spike — does the Copy-ref list win survive a real collector?");
    println!("size_of GcCons   = {} bytes", std::mem::size_of::<GcCons>());
    println!(
        "size_of Boxed    = {} bytes (24B Rc enum reality)\n",
        std::mem::size_of::<Boxed>()
    );

    const PAIRS: u32 = 12;
    let n = 2_000i64;
    let iters = 2_000u32;

    // Heap soft cap: enough for the WINDOW live lists plus headroom, so the
    // working set survives but collection still runs MANY times. Deliberately
    // NOT a multiple of `n` — an exact multiple aligns every collection to a
    // list boundary (empty live set → trivial mark phase), which silently
    // under-measures GC cost (the bug in the first cut of this spike).
    let cap = WINDOW * (n as usize) * 2 + (n as usize) / 2;

    // Correctness oracle: each list sums 0..n; total = iters * sum(0..n).
    let expected = (iters as i64).wrapping_mul((0..n).sum::<i64>());

    let (mut bx, mut gc, mut lk) = (Vec::new(), Vec::new(), Vec::new());
    let (mut total_collections, mut peak_live) = (0u64, 0usize);

    for pair in 0..PAIRS {
        // --- A: boxed (24B Rc enum), real reclamation as the ring evicts ---
        let t0 = Instant::now();
        let r = black_box(boxed_bench(iters, black_box(n)));
        bx.push(t0.elapsed());
        assert_eq!(r, expected, "boxed produced wrong sum — measurement void");

        // --- B: GC (mark-sweep, bounded heap, Copy words) ---
        let mut heap = Heap::new(cap);
        let t1 = Instant::now();
        let r = black_box(gc_bench(&mut heap, iters, black_box(n)));
        gc.push(t1.elapsed());
        assert_eq!(r, expected, "GC produced wrong sum — corrupt list, void");
        if pair == PAIRS - 1 {
            total_collections = heap.collections;
            peak_live = heap.peak_live;
        }

        // --- C: leaked Copy word (Option-B ceiling, no reclamation) ---
        let t2 = Instant::now();
        let r = black_box(leak_bench(iters, black_box(n)));
        lk.push(t2.elapsed());
        assert_eq!(r, expected, "leak produced wrong sum — measurement void");
    }

    // A collector that never ran, or that found a near-empty live set, is not
    // a valid measurement — guard against silently re-introducing that bug.
    assert!(
        total_collections > 0,
        "no collection ran — heap cap too high"
    );
    assert!(
        peak_live as i64 >= n,
        "peak live ({peak_live}) < one list ({n}) — live set not traced, void"
    );

    let bmin = bx.iter().min().unwrap();
    let gmin = gc.iter().min().unwrap();
    let lmin = lk.iter().min().unwrap();

    let gc_speedup = bmin.as_secs_f64() / gmin.as_secs_f64();
    let ceiling = bmin.as_secs_f64() / lmin.as_secs_f64();
    let survived = gc_speedup / ceiling;

    println!("W list build+sum (n={n}, {iters}x, {PAIRS} pairs, min-of-N):");
    println!(
        "  boxed (24B Rc, reclaim/iter)   min {:.2} ms  mean {:.2} ms",
        ms(*bmin),
        ms(mean(&bx))
    );
    println!(
        "  GC (mark-sweep, Copy word)     min {:.2} ms  mean {:.2} ms",
        ms(*gmin),
        ms(mean(&gc))
    );
    println!(
        "  leaked ceiling (Option B)      min {:.2} ms  mean {:.2} ms",
        ms(*lmin),
        ms(mean(&lk))
    );
    println!();
    println!("  GC speedup vs boxed:     {gc_speedup:.2}x");
    println!("  leaked ceiling vs boxed: {ceiling:.2}x  (Option-B / no-reclaim)");
    println!("  fraction of ceiling GC retains: {:.0}%", survived * 100.0);
    println!();
    println!(
        "  collection ran {total_collections} times (last heap); peak live nodes {peak_live} \
         (~one list = {n} ⇒ heap BOUNDED, real reclamation, NOT a leak)"
    );
    println!();
    println!("KILL-CRITERION (HANDOFF Step 2): GC must retain a material fraction");
    println!("of the {ceiling:.2}x ceiling WITH collection happening. collections>0 and");
    println!("peak_live ~ one list confirm reclamation is real (not a disguised leak).");
    println!("If GC speedup is marginal (≈1.0x), the ladder is NOT justified → take");
    println!("the fixnums-only fallback (~1.6x arithmetic, no GC) instead.");
    println!();
    println!("CAVEAT (perf-state §6d): this models the collector + representation");
    println!("throughput and a shadow-stack precise-roots story. It does NOT model");
    println!("GC roots inside AOT-compiled Rust frames — the largest remaining design");
    println!("risk — which needs its own prototype before any full Value conversion.");
}
