//! Step-2 gate: does the proven 3.34× from `gc_spike.rs` survive the REAL,
//! block-allocated `src/gc/` collector?
//!
//! `gc_spike.rs` validated the *algorithm* with a throwaway in-bench heap
//! (per-node `Box`, a `Vec`-tracked node set). This bench runs the SAME
//! list-build+sum workload — same `WINDOW` live ring, same un-aligned soft cap,
//! same correctness oracle and anti-vacuous guards — through the shipped
//! [`shen_rust::gc::Heap`] (uniform 24-byte `Node`s, block/slab allocation, a
//! membership table, a `Kind`-tagged trace path). If the production module is
//! markedly slower than the spike, the block layout / header overhead is to
//! blame and must be understood before Step 3 builds on it.
//!
//! Run: `cargo bench --bench gc_module_bench`   (harness = false)

use std::hint::black_box;
use std::rc::Rc;
use std::time::{Duration, Instant};

use shen_rust::gc::{Gc, Heap};

/// Live working set: every rep keeps the last `WINDOW` lists reachable, so each
/// collection has a realistic, non-empty root set to trace.
const WINDOW: usize = 4;

// ---------------------------------------------------------------------------
// Baseline A: today's reality — 24-byte Rc enum, full Rc reclamation per iter.
// (Identical to gc_spike.rs's baseline, re-stated so this bench is standalone.)
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
        ring[it % WINDOW] = xs;
    }
    total
}

// ---------------------------------------------------------------------------
// Baseline B: the real src/gc/ heap, same workload + shadow-stack roots.
// ---------------------------------------------------------------------------

fn gc_bench(heap: &mut Heap, iters: u32, n: i64) -> i64 {
    // roots[0..WINDOW] = live ring; roots[WINDOW] = current build slot.
    let mut roots: Vec<Gc> = vec![Gc::nil(); WINDOW + 1];
    const BUILD: usize = WINDOW;
    let mut total = 0i64;
    for it in 0..iters as usize {
        roots[BUILD] = Gc::nil();
        let mut xs = Gc::nil();
        for i in 0..n {
            // Keep the chain built so far reachable across a GC inside alloc.
            roots[BUILD] = xs;
            xs = heap.alloc_cons(Gc::fixnum(i), xs, &roots);
            roots[BUILD] = xs;
        }
        // Sum: pure Copy-word traversal — no refcount, no alloc, no GC.
        let mut acc = 0i64;
        let mut cur = xs;
        while cur.is_ptr() {
            acc = acc.wrapping_add(heap.cons_head(cur).as_fixnum());
            cur = heap.cons_tail(cur);
        }
        total = total.wrapping_add(acc);
        roots[it % WINDOW] = xs;
        roots[BUILD] = Gc::nil();
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
    println!("GC module bench — does the spike's win survive the real src/gc/ heap?");
    println!("size_of Node     = {} bytes", Heap::node_size());
    println!(
        "size_of Boxed    = {} bytes (24B Rc enum reality)\n",
        std::mem::size_of::<Boxed>()
    );

    const PAIRS: u32 = 12;
    let n = 2_000i64;
    let iters = 2_000u32;

    // Same soft cap as the spike: room for the WINDOW live lists plus headroom,
    // deliberately NOT a multiple of `n` (an exact multiple aligns every
    // collection to a list boundary → empty live set → trivial, under-measured
    // mark phase — the original false-7.26× bug).
    let cap = WINDOW * (n as usize) * 2 + (n as usize) / 2;

    let expected = (iters as i64).wrapping_mul((0..n).sum::<i64>());

    let (mut bx, mut gc) = (Vec::new(), Vec::new());
    let (mut total_collections, mut peak_live) = (0u64, 0usize);

    for pair in 0..PAIRS {
        // --- A: boxed (24B Rc enum), real reclamation as the ring evicts ---
        let t0 = Instant::now();
        let r = black_box(boxed_bench(iters, black_box(n)));
        bx.push(t0.elapsed());
        assert_eq!(r, expected, "boxed produced wrong sum — measurement void");

        // --- B: GC (real src/gc/ heap, Copy words, bounded) ---
        let mut heap = Heap::new(cap);
        let t1 = Instant::now();
        let r = black_box(gc_bench(&mut heap, iters, black_box(n)));
        gc.push(t1.elapsed());
        assert_eq!(r, expected, "GC produced wrong sum — corrupt list, void");
        if pair == PAIRS - 1 {
            total_collections = heap.collections();
            peak_live = heap.peak_live();
        }
    }

    // A collector that never ran, or found a near-empty live set, is not a valid
    // measurement — the same guards the spike uses.
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
    let gc_speedup = bmin.as_secs_f64() / gmin.as_secs_f64();

    println!("list build+sum (n={n}, {iters}x, {PAIRS} pairs, min-of-N):");
    println!(
        "  boxed (24B Rc, reclaim/iter)   min {:.2} ms  mean {:.2} ms",
        ms(*bmin),
        ms(mean(&bx))
    );
    println!(
        "  GC (real src/gc/, Copy word)   min {:.2} ms  mean {:.2} ms",
        ms(*gmin),
        ms(mean(&gc))
    );
    println!();
    println!("  GC speedup vs boxed: {gc_speedup:.2}x");
    println!(
        "  collection ran {total_collections} times (last heap); peak live nodes {peak_live} \
         (~one list = {n} ⇒ heap BOUNDED, real reclamation)"
    );
    println!();
    println!("GATE (handoff Step 2 §4.3): the real module must reproduce a MATERIAL");
    println!("fraction of the spike's 3.34x WITH collection happening (collections>0,");
    println!("peak_live ~ one list). Block allocation should, if anything, beat the");
    println!("spike's per-node Box; a markedly slower result means header / layout");
    println!("overhead to investigate before Step 3.");
}
