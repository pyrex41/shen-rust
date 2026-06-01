//! SPIKE — de-risk the tagged-pointer `Value` conversion (JIT prerequisite).
//!
//! The Cranelift research (design notes 2026-05-29) found a JIT only pays
//! off if `Value` is word-sized, so generated code can hold/operate on
//! values in registers instead of FFI-ing back to Rust for every op. The
//! current `Value` is a 24-byte, non-`Copy`, `Rc`-laden enum.
//!
//! Converting it is large surgery (every match site + AOT regen). Before
//! committing, this standalone microbench isolates the *interpreter-level*
//! effect of the representation alone — the part measurable WITHOUT a JIT:
//! move/copy size, refcount traffic, cons-cell size, cache behavior. (The
//! prior arena spike isolated only drop/free ≈ 2.4%; it did NOT cover
//! these.) The JIT-inlining win is separate and not measured here.
//!
//! Two representations, identical workloads:
//! * `Boxed`  — a 24-byte enum with `Rc` heap fields (mirrors today's `Value`).
//! * `Tagged` — an 8-byte `Copy` word: 61-bit fixnums inline, heap types
//!   (cons/float/str) as tagged pointers into leaked arena slots (so the
//!   bench measures the *representation*, not an allocator; both reps use the
//!   same leaked-Box strategy for heap nodes to keep allocation cost equal).
//!
//! Decision rule: if `Tagged` materially beats `Boxed` (>~10% on the
//! interpreter-style workloads, min-of-N paired), the word-sized
//! prerequisite is independently justified (not only on speculative JIT
//! gains). If marginal, reconsider committing to the conversion.
//!
//! Run: `cargo bench --bench value_repr_spike`  (harness = false)

// The `Float`/`Str` variants and `Tagged::float` are kept to document the
// full representation design (heap types that a real conversion must handle)
// even though the two benchmarked workloads only exercise int + cons.
#![allow(dead_code)]

use std::hint::black_box;
use std::rc::Rc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Representation A: 24-byte Rc-laden enum (mirrors the current Value shape).
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Boxed {
    Nil,
    #[allow(dead_code)]
    Bool(bool),
    Int(i64),
    Float(f64),
    #[allow(dead_code)]
    Str(Rc<str>),
    Cons(Rc<(Boxed, Boxed)>),
}

impl Boxed {
    #[inline]
    fn cons(a: Boxed, b: Boxed) -> Boxed {
        Boxed::Cons(Rc::new((a, b)))
    }
    #[inline]
    fn add(&self, other: &Boxed) -> Boxed {
        match (self, other) {
            (Boxed::Int(x), Boxed::Int(y)) => Boxed::Int(x.wrapping_add(*y)),
            _ => Boxed::Nil,
        }
    }
}

// ---------------------------------------------------------------------------
// Representation B: 8-byte Copy tagged word.
//
// Low 3 bits tag. 000 = fixnum (61-bit, value in high 61 bits). Other tags
// are pointers (8-aligned, low 3 bits free) into leaked heap nodes, so the
// representation is a single machine word that is `Copy` (no clone/drop).
//
// To keep the comparison about *representation* and not allocator, heap
// nodes are allocated the same way as `Boxed`'s Rc (a heap box); the
// difference under test is: word-sized Copy + inline fixnums + 16-byte cons
// vs 24-byte enum + Rc refcount + 40-byte cons(Rc<(Boxed,Boxed)>).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
struct Tagged(u64);

const TAG_BITS: u64 = 3;
const TAG_MASK: u64 = 0b111;
const TAG_FIXNUM: u64 = 0b000;
const TAG_CONS: u64 = 0b001;
const TAG_FLOAT: u64 = 0b010;
const TAG_NIL: u64 = 0b011;

#[repr(align(8))]
struct ConsNode {
    head: Tagged,
    tail: Tagged,
}

impl Tagged {
    #[inline]
    fn nil() -> Tagged {
        Tagged(TAG_NIL)
    }
    #[inline]
    fn fixnum(v: i64) -> Tagged {
        // 61-bit fixnum: shift left past the tag. (Spike: no range check;
        // the bench values fit.)
        Tagged(((v as u64) << TAG_BITS) | TAG_FIXNUM)
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
    fn cons(a: Tagged, b: Tagged) -> Tagged {
        // Leak a heap node (matches Boxed using a fresh Rc alloc per cons;
        // both leak/are-dropped equivalently within a single bench batch —
        // we measure throughput of construction+access, not reclamation).
        let p = Box::into_raw(Box::new(ConsNode { head: a, tail: b }));
        Tagged((p as u64) | TAG_CONS)
    }
    #[inline]
    fn cons_ptr(self) -> *const ConsNode {
        (self.0 & !TAG_MASK) as *const ConsNode
    }
    #[inline]
    fn is_cons(self) -> bool {
        (self.0 & TAG_MASK) == TAG_CONS
    }
    #[inline]
    fn head(self) -> Tagged {
        unsafe { (*self.cons_ptr()).head }
    }
    #[inline]
    fn tail(self) -> Tagged {
        unsafe { (*self.cons_ptr()).tail }
    }
    #[inline]
    #[allow(dead_code)]
    fn float(v: f64) -> Tagged {
        let p = Box::into_raw(Box::new(v));
        Tagged((p as u64) | TAG_FLOAT)
    }
    #[inline]
    fn add(self, other: Tagged) -> Tagged {
        if self.is_fixnum() && other.is_fixnum() {
            Tagged::fixnum(self.as_fixnum().wrapping_add(other.as_fixnum()))
        } else {
            Tagged::nil()
        }
    }
}

// ---------------------------------------------------------------------------
// Representation C: TaggedRc — the HONEST tagged word for the real conversion.
//
// 8-byte payload like `Tagged`, but NON-`Copy`: heap tags hold an owned
// `Rc` strong count, so it must refcount on clone and drop exactly as the
// real `Value` would. Immediates (fixnum/nil) are inline and clone/drop for
// free. This models Option A (tagged word over Rc, no GC): does the
// word-sized win survive once refcount traffic is reintroduced on heap
// values? `Tagged` (leaked, Copy) is the Option-B ceiling; `TaggedRc` is
// the Option-A reality.
// ---------------------------------------------------------------------------

struct TaggedRc(u64);

#[repr(align(8))]
struct RcConsNode {
    head: TaggedRc,
    tail: TaggedRc,
}

impl TaggedRc {
    #[inline]
    fn nil() -> TaggedRc {
        TaggedRc(TAG_NIL)
    }
    #[inline]
    fn fixnum(v: i64) -> TaggedRc {
        TaggedRc(((v as u64) << TAG_BITS) | TAG_FIXNUM)
    }
    #[inline]
    fn tag(&self) -> u64 {
        self.0 & TAG_MASK
    }
    #[inline]
    fn is_fixnum(&self) -> bool {
        self.tag() == TAG_FIXNUM
    }
    #[inline]
    fn is_cons(&self) -> bool {
        self.tag() == TAG_CONS
    }
    #[inline]
    fn as_fixnum(&self) -> i64 {
        (self.0 as i64) >> TAG_BITS
    }
    #[inline]
    fn cons(a: TaggedRc, b: TaggedRc) -> TaggedRc {
        let p = Rc::into_raw(Rc::new(RcConsNode { head: a, tail: b }));
        TaggedRc((p as u64) | TAG_CONS)
    }
    #[inline]
    fn cons_ptr(&self) -> *const RcConsNode {
        (self.0 & !TAG_MASK) as *const RcConsNode
    }
    #[inline]
    fn head_ref(&self) -> &TaggedRc {
        unsafe { &(*self.cons_ptr()).head }
    }
    #[inline]
    fn tail_ref(&self) -> &TaggedRc {
        unsafe { &(*self.cons_ptr()).tail }
    }
    #[inline]
    fn add(&self, other: &TaggedRc) -> TaggedRc {
        if self.is_fixnum() && other.is_fixnum() {
            TaggedRc::fixnum(self.as_fixnum().wrapping_add(other.as_fixnum()))
        } else {
            TaggedRc::nil()
        }
    }
}

impl Clone for TaggedRc {
    #[inline]
    fn clone(&self) -> TaggedRc {
        // Heap tags must bump the strong count (real Value semantics).
        if self.is_cons() {
            unsafe { Rc::increment_strong_count(self.cons_ptr()) };
        }
        TaggedRc(self.0)
    }
}

impl Drop for TaggedRc {
    #[inline]
    fn drop(&mut self) {
        if self.is_cons() {
            unsafe { Rc::decrement_strong_count(self.cons_ptr()) };
        }
    }
}

// ---------------------------------------------------------------------------
// Workloads (identical shape for both reps).
// ---------------------------------------------------------------------------

/// W1: tight arithmetic accumulation — immediate-heavy, the case tagged
/// fixnums should win biggest (no heap, no refcount; pure Copy word vs
/// 24-byte enum move).
fn boxed_arith(n: i64) -> i64 {
    let mut acc = Boxed::Int(0);
    for _ in 0..n {
        // black_box the addend each iter so the loop can't be folded to a
        // closed form or eliminated; the running value stays a real Boxed.
        let one = black_box(Boxed::Int(black_box(1)));
        acc = acc.add(&one);
    }
    // Force the result to be observed as a real value.
    match acc {
        Boxed::Int(v) => v,
        _ => -1,
    }
}
fn tagged_arith(n: i64) -> i64 {
    let mut acc = Tagged::fixnum(0);
    for _ in 0..n {
        let one = black_box(Tagged::fixnum(black_box(1)));
        acc = acc.add(one);
    }
    acc.as_fixnum()
}

/// W2: build a cons list of length n, then sum by walking it. Heap + access
/// pattern; tests 16-byte cons + Copy traversal vs 40-byte Rc cons + clone.
fn boxed_list_sum(n: i64) -> i64 {
    let mut xs = Boxed::Nil;
    for i in 0..n {
        xs = Boxed::cons(Boxed::Int(i), xs);
    }
    let mut acc = Boxed::Int(0);
    let mut cur: &Boxed = &xs;
    while let Boxed::Cons(p) = cur {
        acc = acc.add(&p.0);
        cur = &p.1;
    }
    // xs drops here (real reclamation), matching TaggedRc.
    match acc {
        Boxed::Int(v) => v,
        _ => -1,
    }
}
// Option-B ceiling: leaked Copy word, no reclamation (best case).
fn tagged_list_sum(n: i64) -> i64 {
    let mut xs = Tagged::nil();
    for i in 0..n {
        xs = Tagged::cons(Tagged::fixnum(i), xs);
    }
    let mut acc = Tagged::fixnum(0);
    let mut cur = xs;
    while cur.is_cons() {
        acc = acc.add(cur.head());
        cur = cur.tail();
    }
    acc.as_fixnum()
}
// Option-A reality: tagged word over Rc, refcounts on clone/drop, list is
// reclaimed at end of scope (Drop walks the chain) — the honest number.
fn tagged_rc_list_sum(n: i64) -> i64 {
    let mut xs = TaggedRc::nil();
    for i in 0..n {
        xs = TaggedRc::cons(TaggedRc::fixnum(i), xs);
    }
    let mut acc = TaggedRc::fixnum(0);
    let mut cur: &TaggedRc = &xs;
    while cur.is_cons() {
        acc = acc.add(cur.head_ref());
        cur = cur.tail_ref();
    }
    // xs drops here: decrement_strong_count cascades down the chain, exactly
    // like the Boxed/Rc path.
    acc.as_fixnum()
}

fn mean(ds: &[Duration]) -> Duration {
    ds.iter().sum::<Duration>() / ds.len() as u32
}
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn main() {
    println!("Value-repr spike — Boxed(24B Rc enum) vs Tagged(8B Copy word)");
    println!("size_of Boxed  = {} bytes", std::mem::size_of::<Boxed>());
    println!("size_of Tagged = {} bytes", std::mem::size_of::<Tagged>());
    println!(
        "size_of Boxed cons node  = {} (Rc<(Boxed,Boxed)> payload)",
        std::mem::size_of::<(Boxed, Boxed)>()
    );
    println!(
        "size_of Tagged cons node = {} (ConsNode)\n",
        std::mem::size_of::<ConsNode>()
    );

    const PAIRS: u32 = 12;

    // W1: arithmetic
    {
        let n = 5_000_000i64;
        let iters = 20u32;
        let (mut bx, mut tg) = (Vec::new(), Vec::new());
        for _ in 0..PAIRS {
            let t0 = Instant::now();
            let mut s0 = 0i64;
            for _ in 0..iters {
                s0 = s0.wrapping_add(black_box(boxed_arith(black_box(n))));
            }
            black_box(s0);
            bx.push(t0.elapsed());
            let t1 = Instant::now();
            let mut s1 = 0i64;
            for _ in 0..iters {
                s1 = s1.wrapping_add(black_box(tagged_arith(black_box(n))));
            }
            black_box(s1);
            tg.push(t1.elapsed());
        }
        let (bmin, tmin) = (bx.iter().min().unwrap(), tg.iter().min().unwrap());
        println!("W1 arith (n={n}, {iters}x):");
        println!(
            "  boxed  min {:.2} ms  mean {:.2} ms",
            ms(*bmin),
            ms(mean(&bx))
        );
        println!(
            "  tagged min {:.2} ms  mean {:.2} ms",
            ms(*tmin),
            ms(mean(&tg))
        );
        println!(
            "  tagged speedup (min) {:.2}x\n",
            bmin.as_secs_f64() / tmin.as_secs_f64()
        );
    }

    // W2: list build + sum. Three reps, all reclaimed equally except the
    // Option-B ceiling (tagged_leaked):
    //   boxed       — 24B Rc enum, Rc drop (today's reality)
    //   tagged_rc   — 8B word over Rc, refcounts + Rc drop (Option A reality)
    //   tagged_leak — 8B Copy word, leaked, no reclaim (Option B / GC ceiling)
    {
        let n = 2_000i64;
        let iters = 2_000u32;
        let (mut bx, mut trc, mut tl) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..PAIRS {
            let t0 = Instant::now();
            let mut s = 0i64;
            for _ in 0..iters {
                s = s.wrapping_add(black_box(boxed_list_sum(black_box(n))));
            }
            black_box(s);
            bx.push(t0.elapsed());

            let t1 = Instant::now();
            let mut s = 0i64;
            for _ in 0..iters {
                s = s.wrapping_add(black_box(tagged_rc_list_sum(black_box(n))));
            }
            black_box(s);
            trc.push(t1.elapsed());

            let t2 = Instant::now();
            let mut s = 0i64;
            for _ in 0..iters {
                s = s.wrapping_add(black_box(tagged_list_sum(black_box(n))));
            }
            black_box(s);
            tl.push(t2.elapsed());
        }
        let bmin = bx.iter().min().unwrap();
        let trmin = trc.iter().min().unwrap();
        let tlmin = tl.iter().min().unwrap();
        println!("W2 list build+sum (n={n}, {iters}x):");
        println!("  boxed (24B Rc)       min {:.2} ms", ms(*bmin));
        println!("  tagged_rc (8B+Rc, A) min {:.2} ms", ms(*trmin));
        println!("  tagged_leak (8B, B)  min {:.2} ms", ms(*tlmin));
        println!(
            "  Option A (tagged_rc) speedup vs boxed: {:.2}x",
            bmin.as_secs_f64() / trmin.as_secs_f64()
        );
        println!(
            "  Option B (leaked) speedup vs boxed:    {:.2}x  (GC ceiling)\n",
            bmin.as_secs_f64() / tlmin.as_secs_f64()
        );
    }

    println!("READING (measured 2026-05-29):");
    println!("- W1 (fixnum arith) ~1.64x: no heap, no refcount; survives any");
    println!("  lifetime choice. The FLOOR a word-sized Value always delivers.");
    println!("- W2 Option A (tagged_rc over Rc) ~1.00x: reintroducing the refcount");
    println!("  on clone/drop (mandatory for safety WITHOUT a GC) erases the");
    println!("  word-sized list win entirely. The refcount IS the heap cost, not");
    println!("  the 24B-vs-8B box size.");
    println!("- W2 Option B (leaked, GC ceiling) ~2.48x: what a GC/arena could");
    println!("  reach by making heap refs Copy. The A→B gap = the value of a GC.");
    println!("CONCLUSION: a tagged Value without a GC helps ONLY arithmetic-heavy");
    println!("code. The full win (and JIT enablement) needs a GC. Ladder to SBCL:");
    println!("tracing GC (Copy refs) -> word-sized Value -> Cranelift JIT.");
}
