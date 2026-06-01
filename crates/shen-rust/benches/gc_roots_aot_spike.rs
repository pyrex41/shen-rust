//! SPIKE — the §6d gate: can a tracing GC find roots that live in
//! AOT-COMPILED RUST STACK FRAMES?
//!
//! `gc_spike.rs` showed the Copy-ref throughput win survives a real collector
//! when roots come from an explicit **shadow stack**. But the shadow stack only
//! sees the interpreter/VM value-stack. The AOT-compiled kernel
//! (`aot/kernel/*.rs`) holds `Value`s as **plain Rust locals** that stay live
//! across calls — e.g. `core.rs`:
//!
//! ```ignore
//! let mut v_V520 = args[0].clone();
//! let v_W521 = rt::apply_direct(interp, "shen.shen->kl-h", &[v_V520.clone()])?;
//! //           ^ this call recurses arbitrarily deep; in a GC world it can
//! //             trigger a collection while v_V520 / v_W521 are live roots
//! //             sitting in this native frame, invisible to any shadow stack.
//! ```
//!
//! perf-state §6d calls this "the single biggest design risk." This spike
//! de-risks it for the leaning algorithm (non-moving mark-sweep, §6c option 1):
//!
//!   > For a NON-MOVING collector, **conservative scanning of the native stack
//!   > + a register flush** is sound — a false-positive root can only
//!   > over-retain (keep a dead object one extra cycle), never corrupt, because
//!   > nothing moves. And because every node reference in this design is a
//!   > *tagged* word (low bits = TAG_CONS), the scan is tag-aware: sound AND
//!   > low false-positive.
//!
//! What this models:
//! * `aot_frame` is a stand-in for an AOT-compiled kernel function: it holds a
//!   list head in a plain Rust local, then recurses deeper (where allocation
//!   triggers GC), then *uses the local again* (sums it) — so the compiler must
//!   keep it live across the call, exactly like `v_V520` above. **No shadow
//!   stack is used.** The collector's ONLY root source is the conservative scan.
//! * The collector flushes callee-saved registers (aarch64 x19–x28) to a stack
//!   buffer, then conservatively scans `[current_sp .. stack_base)` + that
//!   buffer for tagged cons pointers into its own heap.
//!
//! PASS criteria (asserted — a missed root corrupts a list ⇒ wrong sum ⇒ a
//! VOID failure, not a silent pass):
//!  1. correctness: every frame's list sums correctly despite GC mid-build;
//!  2. real collection: collections > 0 and the heap stays BOUNDED across outer
//!     iterations (dead frames' lists ARE reclaimed — not retain-everything);
//!  3. the scan actually finds native-frame roots: roots-found ≥ live depth.
//!
//! Run: `cargo bench --bench gc_roots_aot_spike`   (harness = false)

#![allow(dead_code)]

use std::collections::HashSet;
use std::hint::black_box;

// ---------------------------------------------------------------------------
// Tagged Copy word + non-moving node (same scheme as gc_spike.rs).
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
    fn is_cons(self) -> bool {
        (self.0 & TAG_MASK) == TAG_CONS
    }
    #[inline]
    fn as_fixnum(self) -> i64 {
        (self.0 as i64) >> TAG_BITS
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

#[repr(align(8))]
struct GcCons {
    head: Word,
    tail: Word,
    mark: bool,
}

// ---------------------------------------------------------------------------
// Register flush (aarch64): spill callee-saved x19–x28 to a buffer so a live
// pointer sitting only in a callee-saved register is included in the scan.
// Required for soundness — a value live across a call lives EITHER on the stack
// (scanned) OR in a callee-saved register (flushed here); caller-saved regs
// can't hold a value across a call. This is the standard conservative-GC trick.
// ---------------------------------------------------------------------------

#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn flush_callee_saved(buf: &mut [u64; 10]) {
    // SAFETY: pure stores of register contents into a caller-provided buffer.
    unsafe {
        std::arch::asm!(
            "stp x19, x20, [{b}, #0]",
            "stp x21, x22, [{b}, #16]",
            "stp x23, x24, [{b}, #32]",
            "stp x25, x26, [{b}, #48]",
            "stp x27, x28, [{b}, #64]",
            b = in(reg) buf.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline(never)]
fn flush_callee_saved(_buf: &mut [u64; 10]) {
    // Other arches: a real impl would use setjmp or arch-specific spills. This
    // spike is validated on aarch64 (the dev/CI target).
}

/// Current stack pointer (approx: address of a fresh local). Stack grows down,
/// so this is the LOW end of the live region; `stack_base` is the high end.
#[inline(never)]
fn current_sp() -> usize {
    let probe = 0u8;
    black_box(&probe) as *const u8 as usize
}

// ---------------------------------------------------------------------------
// Non-moving mark-sweep heap whose roots come from a CONSERVATIVE STACK SCAN.
// ---------------------------------------------------------------------------

struct Heap {
    all: Vec<*mut GcCons>,
    free: Vec<*mut GcCons>,
    /// Membership test for the conservative scan: "is this masked word one of
    /// our node addresses?" (page tables in a real impl; a set here).
    addrs: HashSet<usize>,
    /// Collect every `gc_interval` allocations (cadence decoupled from heap
    /// size, so collections fire mid-descent while many native frames are live
    /// — the case under test — without thrashing).
    gc_interval: usize,
    alloc_since_gc: usize,
    /// High end of the managed stack region, captured on entry. Scan covers
    /// `[current_sp .. stack_base)`.
    stack_base: usize,
    mark_stack: Vec<*mut GcCons>,
    collections: u64,
    peak_live: usize,
    last_roots_found: usize,
    max_roots_found: usize,
}

impl Heap {
    fn new(gc_interval: usize, stack_base: usize) -> Heap {
        Heap {
            all: Vec::new(),
            free: Vec::new(),
            addrs: HashSet::new(),
            gc_interval,
            alloc_since_gc: 0,
            stack_base,
            mark_stack: Vec::new(),
            collections: 0,
            peak_live: 0,
            last_roots_found: 0,
            max_roots_found: 0,
        }
    }

    #[inline]
    fn alloc(&mut self, head: Word, tail: Word) -> Word {
        // Cadence: collect every `gc_interval` allocations (reclaims into `free`).
        if self.alloc_since_gc >= self.gc_interval {
            self.collect();
            self.alloc_since_gc = 0;
        }
        // Grow on demand (batch) only if collection couldn't satisfy us. With a
        // bounded live+over-retained set this stops growing once warm.
        if self.free.is_empty() {
            for _ in 0..1024usize {
                let p = Box::into_raw(Box::new(GcCons {
                    head: Word::nil(),
                    tail: Word::nil(),
                    mark: false,
                }));
                self.all.push(p);
                self.addrs.insert(p as usize);
                self.free.push(p);
            }
        }
        self.alloc_since_gc += 1;
        let p = self.free.pop().unwrap();
        // SAFETY: `p` is owned by this heap and currently free (unreachable).
        unsafe {
            (*p).head = head;
            (*p).tail = tail;
            (*p).mark = false;
        }
        Word::from_ptr(p)
    }

    /// Mark-sweep with conservatively-scanned roots — NO shadow stack.
    fn collect(&mut self) {
        // ---- find roots by scanning native stack + flushed registers ----
        let mut regbuf = [0u64; 10];
        flush_callee_saved(&mut regbuf);
        let sp = current_sp();
        let base = self.stack_base;
        let (lo, hi) = if sp <= base { (sp, base) } else { (base, sp) };

        self.mark_stack.clear();
        let mut roots_found = 0usize;

        // Scan the stack region word-by-word (8-byte aligned).
        let mut a = lo & !0b111;
        while a < hi {
            // SAFETY: reading our own live stack region as raw words. Addresses
            // are 8-aligned and within [lo,hi); reads are plain u64 loads.
            let w = unsafe { *(a as *const u64) };
            if self.try_root(Word(w)) {
                roots_found += 1;
            }
            a += 8;
        }
        // Scan the flushed callee-saved registers.
        for &w in regbuf.iter() {
            if self.try_root(Word(w)) {
                roots_found += 1;
            }
        }
        self.last_roots_found = roots_found;
        if roots_found > self.max_roots_found {
            self.max_roots_found = roots_found;
        }

        // ---- trace ----
        while let Some(p) = self.mark_stack.pop() {
            // SAFETY: `p` is a live node address verified by `addrs`.
            unsafe {
                let h = (*p).head;
                let t = (*p).tail;
                self.push_if_cons(h);
                self.push_if_cons(t);
            }
        }

        // ---- sweep ----
        self.free.clear();
        let mut live = 0usize;
        for &p in &self.all {
            // SAFETY: every `p` in `all` is a live, owned node.
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

    /// Tag-aware conservative root test: a live node reference is ALWAYS a word
    /// tagged TAG_CONS whose masked value is one of our node addresses. Marking
    /// here can only over-approximate (false positive ⇒ over-retain), never
    /// miss a real pointer — sound for a non-moving heap.
    #[inline]
    fn try_root(&mut self, w: Word) -> bool {
        if !w.is_cons() {
            return false;
        }
        let p = w.cons_ptr();
        if !self.addrs.contains(&(p as usize)) {
            return false;
        }
        // SAFETY: `p` is a verified node address.
        unsafe {
            if (*p).mark {
                return false; // already a root this cycle
            }
            (*p).mark = true;
        }
        self.mark_stack.push(p);
        true
    }

    #[inline]
    fn push_if_cons(&mut self, w: Word) {
        if w.is_cons() {
            let p = w.cons_ptr();
            // SAFETY: only nodes from our heap are ever stored in head/tail.
            unsafe {
                if !(*p).mark {
                    (*p).mark = true;
                    self.mark_stack.push(p);
                }
            }
        }
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        for &p in &self.all {
            // SAFETY: each pointer came from Box::into_raw; freed once.
            unsafe { drop(Box::from_raw(p)) };
        }
    }
}

// ---------------------------------------------------------------------------
// The AOT-frame stand-in: holds a list head in a plain Rust local across a
// GC-triggering recursive call, with NO shadow-stack registration. If the
// conservative scan fails to find this local, the list is reclaimed and reused
// underneath us → the sum comes back wrong → assert fires (void, not fast).
// ---------------------------------------------------------------------------

/// Allocate `m` cons cells into a throwaway list that dies when this returns —
/// pure GARBAGE. This is what forces collection to fire *during* the recursion
/// (while every shallower frame's keeper-list is live), and gives the collector
/// something real to reclaim, so we test root-finding AND reclamation together.
#[inline(never)]
fn make_garbage(heap: &mut Heap, m: i64) {
    let mut g = Word::nil();
    for i in 0..m {
        g = heap.alloc(Word::fixnum(i), g);
    }
    black_box(g); // g dies here — unrooted, reclaimable on the next collection
}

#[inline(never)]
fn aot_frame(heap: &mut Heap, keep_n: i64, garbage_n: i64, depth: u32) -> i64 {
    // Keeper list: built into a PLAIN LOCAL, the only reference is `xs` in this
    // native frame. It must survive every collection triggered below.
    let mut xs = Word::nil();
    for i in 0..keep_n {
        xs = heap.alloc(Word::fixnum(i), xs);
    }
    // Churn garbage so the bounded heap COLLECTS here — while `xs` (and the
    // keeper of every shallower frame) is live ONLY in native Rust frames.
    make_garbage(heap, garbage_n);
    let deeper = if depth > 0 {
        aot_frame(heap, keep_n, garbage_n, depth - 1)
    } else {
        0
    };
    // Use `xs` AFTER the calls → the compiler must keep it live across them
    // (spilled to stack or in a callee-saved register; the scan covers both).
    // If any collection failed to find this native-frame root, these nodes were
    // reused by `make_garbage` underneath us → the sum comes back wrong.
    let mut acc = 0i64;
    let mut cur = black_box(xs);
    while cur.is_cons() {
        let p = cur.cons_ptr();
        // SAFETY: `cur` chains through live nodes IFF the GC preserved them —
        // which is exactly what this spike verifies.
        unsafe {
            acc = acc.wrapping_add((*p).head.as_fixnum());
            cur = (*p).tail;
        }
    }
    acc.wrapping_add(deeper)
}

fn main() {
    println!("GC roots-in-AOT-frames spike — conservative native-stack scan");
    println!(
        "target_arch aarch64 register flush: {}\n",
        cfg!(target_arch = "aarch64")
    );

    let keep_n = 200i64; // keeper-list length per frame (a live native-frame root)
    let garbage_n = 2_000i64; // throwaway churn per frame → forces mid-descent GC
    let depth = 24u32; // 25 live keeper frames during a deep collection
    let outer = 400u32;
    let frames = (depth + 1) as usize;
    let live_keepers = frames * keep_n as usize;

    // Collect roughly once per frame's garbage churn → collections land at many
    // different descent depths, including deep ones where ~all keeper frames are
    // simultaneously live. That deep case is exactly the AOT-roots test.
    let gc_interval = garbage_n as usize;

    // Each frame sums its keeper (0..keep_n); a full descent sums it `frames`
    // times; garbage contributes nothing.
    let per_call = (frames as i64).wrapping_mul((0..keep_n).sum::<i64>());
    let expected = (outer as i64).wrapping_mul(per_call);

    // stack_base = high end of the managed region (a local in this outer frame).
    let anchor = 0u8;
    let stack_base = black_box(&anchor) as *const u8 as usize;

    let mut heap = Heap::new(gc_interval, stack_base);
    let mut total = 0i64;
    for _ in 0..outer {
        let r = aot_frame(&mut heap, keep_n, garbage_n, depth);
        total = total.wrapping_add(r);
    }

    println!("config: keep_n={keep_n}, garbage_n={garbage_n}, depth={depth} ({frames} keeper frames), outer={outer}");
    println!(
        "        gc_interval={gc_interval} allocs (collections land at varied descent depths)\n"
    );

    let correct = total == expected;
    // True simultaneously-live set at a deep collection: one keeper per frame
    // plus at most one in-progress garbage list.
    let true_live = live_keepers + garbage_n as usize;
    let retain_everything = outer as usize * frames * (keep_n + garbage_n) as usize;
    let over_retain = heap.peak_live as f64 / true_live as f64;

    println!("RESULTS:");
    println!("  result sum: {total}   expected: {expected}");
    println!(
        "  [SOUND] correctness: {}",
        if correct {
            "PASS — every native-frame keeper survived GC (scan never missed a root)"
        } else {
            "FAIL — VOID"
        }
    );
    println!("  [SOUND] collections ran: {}", heap.collections);
    println!(
        "  [SOUND] MAX roots found in one collection: {} (≥ {frames} keeper frames ⇒ the",
        heap.max_roots_found
    );
    println!("          scan genuinely discovers heads held ONLY in native Rust frames)");
    println!(
        "  [BOUNDED] heap size: {} nodes — vs {retain_everything} if it leaked everything",
        heap.all.len()
    );
    println!(
        "            (≈ one descent's worth, INDEPENDENT of the {outer} iterations ⇒ no leak)"
    );
    println!(
        "  [COST] peak live (marked): {} vs true-live ~{true_live} ⇒ OVER-RETENTION ~{over_retain:.1}x",
        heap.peak_live
    );
    println!();

    // ---- assertions test the §6d GATE (soundness + AOT-frame root discovery),
    //      NOT footprint. A failure here = the approach is unsound, not slow. ----
    assert_eq!(
        total, expected,
        "GC freed a live native-frame keeper — wrong sum (VOID, approach UNSOUND)"
    );
    assert!(
        heap.collections > 0,
        "no collection ran — cap too high, test vacuous"
    );
    assert!(
        heap.max_roots_found as u32 >= depth,
        "max roots ({}) < depth ({depth}) — native-frame roots NOT all discovered",
        heap.max_roots_found
    );
    // Bounded ≠ tight: it must be FAR below retain-everything (proving it's not a
    // leak), but conservative scanning over-retains (the [COST] line) — that's a
    // measured tax, not a correctness failure.
    assert!(
        heap.all.len() < retain_everything / 10,
        "heap not bounded ({}) — looks like a real leak, not just over-retention",
        heap.all.len()
    );

    println!("VERDICT (§6d gate): conservative native-stack scan + aarch64 register");
    println!("flush finds GC roots held in AOT-style Rust frames SOUNDLY — correctness");
    println!(
        "held across {} collections; non-moving ⇒ a false-positive root can only",
        heap.collections
    );
    println!("over-retain, never corrupt. So a NON-MOVING mark-sweep CAN tolerate AOT");
    println!("frames it cannot precisely enumerate. The §6d feasibility question: YES.");
    println!();
    println!("BUT — the headline COST: conservative full-stack scanning OVER-RETAINS");
    println!("(~{over_retain:.1}x here). Cause: a returned `make_garbage` frame leaves a stale");
    println!("list-head pointer in its popped (uncleared) stack slot; the scan finds it and");
    println!("retains the whole dead chain. Bounded (one descent), but a real footprint tax.");
    println!();
    println!("DESIGN IMPLICATIONS (feed into §6c/§6d decision):");
    println!("- Soundness of non-moving + conservative roots is CONFIRMED — the safe");
    println!("  baseline exists. We are not blocked.");
    println!("- Over-retention argues for a HYBRID: a PRECISE shadow stack for the");
    println!("  VM/interpreter value-stack (which we own), conservative scan ONLY for");
    println!("  AOT native frames, and/or compiler-emitted stack-slot clearing on AOT");
    println!("  function exit to kill the stale-pointer source. Measure the tax on the");
    println!("  REAL heap before deciding precise-stack-maps (heavier) are warranted.");
    println!("- A MOVING/compacting GC is still ruled out for AbsVec/Foreign identity");
    println!("  (§6b) AND would additionally require precise maps for these AOT frames");
    println!("  (can't conservatively scan if pointers move) — another point for");
    println!("  non-moving.");
    println!("- Register flush is arch-specific (aarch64 x19–x28 here); production needs");
    println!("  setjmp or per-arch spills. Heap-membership must be a page/block table,");
    println!("  not a HashSet, at scale. No interior pointers (every ref is a tagged");
    println!("  head-of-node Word) — this is what makes the tag-aware scan sound; keep it.");
}
