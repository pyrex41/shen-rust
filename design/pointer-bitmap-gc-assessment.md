# Pointer-bitmap GC assessment (Fil-C-style shadow bitmask)

**Status: evaluated 2026-08-06. Verdict: the word-granular (1 bit per 64-bit
word, ~1.5%) pointer/integer shadow bitmap is NOT worth building here — the
tagged `Value` representation already provides its precision guarantees
in-band, and the one place the runtime is imprecise (native stack/registers)
cannot be covered by a bitmap without owning the compiler. A node-granular
descendant of the idea — a live-node bitmap for the conservative scan, 1 bit
per 24-byte node (~0.5%) — IS worthwhile and is prototyped on this branch.**

The proposal under evaluation (inspired by Fil-C, discussed for Zig): keep a
shadow bitmask recording, for every word of memory, whether that word
currently holds a genuine pointer. Claimed benefits: precise GC root scanning
(no conservative false positives), pointer/integer type-confusion detection,
and hardening against use-after-free exploitation patterns.

## 1. The current GC architecture (what the bitmap would sit on)

Facts relevant to this assessment, from `crates/shen-rust/src/gc/` and
`src/value.rs` (see `design/gc-conversion-handoff.md` for the settled
rationale):

- **`Value`/`Gc` is a tagged `u64`.** Low 3 bits: `000` fixnum, `001` heap
  pointer, `010` sym, `011` nil, `100` bool. A word *carries its own*
  pointer/integer discrimination, updated atomically with the word itself.
- **The heap is per-thread, non-moving mark-sweep** (`gc/heap.rs`), opt-in
  via `SHEN_RUST_GC`. Nodes are 24 bytes — a packed header (kind byte + mark)
  plus two payload words — allocated in leaked 1024-node blocks with a
  free-list. A page table (`Heap::pages`, `addr >> 12` → block) answers
  head-of-node membership for the conservative scan.
- **Heap tracing is already fully precise.** `trace_node` switches on the
  node's `Kind`: cons payload words are tagged `Gc` bits; vec cells live in a
  side `Vec<Gc>` (all tagged); closures enumerate their edges via
  `GcObject::gc_edges`; blobs/floats/opaques are leaves. No word in the
  managed heap is ever ambiguous between pointer and integer.
- **Roots are hybrid** (Step 4): precise enumeration of the interpreter's
  containers (env tables, closure-cache/bytecode constant pools, tc-cache,
  host `gc_pins`, VM stack/frames at mid-run safepoints) **plus a
  conservative native-stack scan** — flush aarch64 callee-saved registers,
  walk `[sp, stack_base)`, and treat every word that is `TAG_PTR`-tagged,
  head-of-node aligned, inside a block, and non-`Free` as a root. aarch64
  macOS/Linux only; anywhere else request-mode collection refuses (hard
  fail-closed, never a degraded scan).
- **Collection runs only at safepoints**: depth-0 funnel exits, plus the VM
  mid-run safepoint (every 1024 ops), which sweeps **Cons/Float only**
  because suspended frames may hold *derived* pointers into node payloads
  (`&Closure`, bridged `&str`) whose base tagged word the compiler may have
  dropped from the frame.
- **GC is mutually exclusive with the Cranelift JIT** (frame roots
  unverified) and refuses on multi-`Interp` threads.

## 2. Where could a word-granular bitmap help? (area by area)

### 2a. The GC heap itself — redundant by construction

Fil-C needs a shadow bitmap because C values are **untagged**: a `long` and a
`char*` are indistinguishable words, so pointerness must be tracked out of
band. Shen-rust's whole value representation is the opposite design: the
3-bit tag rides *in* the word. For every managed word (cons cells, vec
cells, closure edges, roots in containers) the runtime already knows
pointer-vs-integer exactly, with zero memory overhead and zero possibility
of desynchronization — the tag moves with the word in one store.

A shadow bitmap over the heap would be strictly worse than what exists:

- **Zero precision gained.** Mark already follows exactly the `TAG_PTR`
  words; sweep already knows every node's kind.
- **A new desync failure class.** Word and shadow bit are two stores; every
  `set_cons_head`/`vec_set`/payload write would need a paired bitmap update,
  and any missed pairing is a new bug category the tag scheme cannot have.
- **The address-space problem.** Node payloads that matter (vec cell
  buffers, blob bytes, closure objects) live in **global-allocator memory at
  arbitrary addresses**, not in the node blocks. A 1-bit-per-word bitmap
  covering them means Fil-C-style reserved shadow memory spanning the whole
  address space (mmap'd shadow regions, address→shadow translation on every
  access). That is a memory-manager rewrite, for benefit already ruled zero.
- **Hot-path cost.** Allocation and cons/vec writes are the hottest paths in
  the runtime (the 24-byte node and `Copy` word exist because +8 bytes per
  cons measurably cost 3.5×→2.4× on the list workload). A second cache-line
  touch per write fails the repo's own perf bar.

### 2b. Native stack and registers — the real imprecision, but a bitmap can't reach it

This is the one place conservatism actually lives, so it is where the
proposal aims. Two independent blockers:

**Blocker 1: no compiler cooperation.** Maintaining "which stack words hold
pointers right now" requires instrumenting every spill/store of a
pointer-typed value. Fil-C *is* a C compiler; Zig owns its compiler and
codegen. Shen-rust is a library/runtime compiled by stock rustc: LLVM
chooses stack layout, spills, and register allocation invisibly, and stable
Rust exposes no stack maps, no GC statepoints, and no store instrumentation
for ordinary locals. The same applies to Cranelift-JIT'd frames (why GC+JIT
is already mutually exclusive) and to klcompile-generated AOT Rust (compiled
by the same rustc). There is no seam to hook a stack bitmap into, short of
becoming a compiler — which the settled design explicitly declines
("don't pay for precise stack maps", `gc-conversion-handoff.md` §2).

**Blocker 2: it solves the wrong problem even in principle.** The measured
cost of the conservative scan is **not** integer/pointer confusion. A false
positive requires a random word to be `TAG_PTR`-tagged (1/8), exactly
24-byte-stride-aligned inside a mapped block, and naming a live slot —
rare, and in a non-moving collector it can only over-retain, never corrupt.
The over-retention that *was* measured (~7.7× in the §6g spike's worst case)
comes from **stale genuine pointers** in popped-but-uncleared stack slots.
A pointer bitmap does not filter those: the word *was* a pointer when
stored, its hypothetical bit would be set, and nothing clears dead frame
slots. Precision against stale slots needs liveness information (stack maps
or slot clearing), which is exactly what a pointerness bitmap doesn't carry.
Meanwhile the depth-0/mid-run safepoint choice already collapsed that
over-retention to ~nothing in practice (dead deep frames sit below the
collect-time SP; see PERFORMANCE.md "GC Step 4").

**Registers:** strictly worse than the stack — no shadow location exists at
all without codegen control. The current callee-saved flush is the only
sound option, and it is already exact about *which* registers can carry a
value across the call into the collector.

### 2c. Pointer/integer exclusivity and UAF hardening — already provided at the meaningful granularity

- **Type confusion:** every managed word self-describes via its tag; every
  node self-describes via its `Kind`; typed accessors (`node_of`) assert the
  kind on every deref. A bitmap adds a second, weaker copy of this.
- **Use-after-free:** sweep resets nodes to `Kind::Free`; debug builds
  poison freed payload words with a recognizable non-pointer pattern
  (`0xDEAD…`, tag `101`); the conservative scan rejects words naming freed
  slots. The prototype below strengthens that last check.
- Shen itself is memory-safe at the language level; the exploit-hardening
  framing of Fil-C targets raw C programs, which is not the threat model of
  this runtime's unsafe-but-audited internals.

## 3. What survives the analysis: a node-granular live bitmap (prototyped)

One retargeted version of the idea does pay: not 1 bit per *word* asking
"is this a pointer" (the tag answers that), but **1 bit per node slot**
asking "is this slot currently an allocated node" — the question the
conservative scan actually needs, at the granularity the heap actually
manages.

Before: `mark_conservative` validated a candidate word with the page-table
range check **plus a read of the node's `kind` header** to reject freed
slots — i.e. every tag-plausible stale word cost a node cache-line touch,
and "is this slot allocated" lived implicitly in node memory.

Now (prototyped on this branch):

- `Node`'s 6 padding bytes gain a `slot: u32` — the node's global index
  (`block_idx * BLOCK_SIZE + offset`), stamped once at block-grow time. The
  node **stays 24 bytes** (the `node_is_word_aligned_and_small` guard test
  still passes).
- `Heap.live_bits: Vec<u64>` — one bit per slot. Set in `obtain_slot_with`
  (one masked store per alloc, indexed directly by the header's `slot`, no
  address math), cleared in the sweep loop (bit cleared *before* the payload
  `Drop` runs, so a mid-sweep panic that poisons the heap can never leave a
  half-freed node answering "live").
- `Heap::is_live_heap_ptr(addr)` — the scan's whole validity test in one
  query: block membership + stride alignment + live bit. `mark_conservative`
  now uses it and no longer reads node headers for candidates.

Properties:

- **Memory:** 1 bit / 24-byte node = **~0.52%** of node storage (128 bytes
  per 24 KiB block). Compare ~1.5% for the word-granular proposal, which
  would additionally need address-space-wide shadow for payload allocations.
- **Mutator cost:** one masked store per alloc and per sweep-free. No cost
  on reads, writes, or copies of `Value`s (where the word-granular design
  bleeds). The store lands in a small, hot array; if measurement ever shows
  it on a profile, it can be gated behind request-mode with a
  rebuild-bitmap-on-enable step (`all` is enumerable), at the price of a
  branch in the same place.
- **Precision:** the scan's stale-word filtering is now exact against freed
  slots *without touching node memory*, and `is_live_heap_ptr` doubles as a
  cheap validity oracle usable by debug assertions on any `Gc` deref.
- **Soundness:** unchanged in kind — the collector stays non-moving, false
  positives still only over-retain. The bitmap tightens over-retention of
  recycled-slot aliases (previously: a stale word naming a freed slot was
  rejected only until the slot was reallocated; that ABA window is inherent
  to conservatism and unchanged, but the common freed-slot case now costs
  one bit-load).

Verified: full `shen-rust` test suite green (24 binaries), clippy clean, two
new unit tests (`live_bitmap_tracks_alloc_and_sweep`,
`live_bitmap_spans_blocks`). The conservative-scan integration tests are
aarch64-only and should be re-run on an aarch64 box before merging; miri
covers the precise-collect path which now includes bitmap maintenance.
Not yet done: an aarch64 perf sanity run (`--kernel-tests` paired mins, and
`benches/gc_boundedness.rs`) to confirm the alloc-path store is noise-level.

## 4. If more precision is ever funded, the levers are elsewhere

In benefit order for long-running/`--served` workloads:

1. **Payload-span table for full mid-run sweeps.** The known RSS gap
   (PERFORMANCE.md: SHA/prng not flat under GC) is the mid-run safepoint
   retaining all payload-bearing kinds because suspended frames can hold
   *derived* payload pointers the head-of-node scan can't see. A side range
   map (payload allocation span → owning node), maintained on
   `Vec`/`Blob`/`Closure`/`Opaque` alloc/free, would let the scan recognize
   interior payload pointers and root their owners — enabling every-kind
   sweeps mid-run. This is Boehm-style interior-pointer handling, not a
   bitmap, and it is the actual precision gap in this runtime. Cost: a
   `BTreeMap` insert/remove per payload alloc/free (payload kinds are cold
   relative to cons), plus scan-time range lookups.
2. **x86_64 conservative scan** (`rbx/rbp/r12–r15` spill) — already
   documented as mechanical; extends `SHEN_RUST_GC` beyond aarch64.
3. **klcompile stack-slot clearing** for AOT frames — the settled design's
   own answer to stale-slot over-retention, to be built only if measured.

## 5. Blocker summary

| Blocker | Affects | Nature |
|---|---|---|
| Stock rustc/LLVM: no stack maps, no store instrumentation, opaque spills | stack/register bitmap | **Fundamental** — requires owning the compiler (Fil-C/Zig do; we don't) |
| Payloads live in global-allocator memory at arbitrary addresses | heap word bitmap | Needs process-wide shadow memory (Fil-C-scale machinery) for zero tracing gain |
| Word+bit updates are two stores | any shadow scheme | New desync bug class; in-band tags are desync-free |
| Alloc/write hot paths are perf-sacred (24-byte node, `Copy` word) | any per-write scheme | Repo's own kill-gates; word-granular maintenance fails them |
| Stale-pointer over-retention is liveness, not pointerness | the headline benefit | A pointer bitmap wouldn't filter stale genuine pointers |
| Cranelift JIT frames | all stack precision | Already handled by GC↔JIT mutual exclusion |
| Miri/provenance discipline | shadow-of-arbitrary-allocations | The GC is carefully Miri-clean; address-space shadow addressing is Miri-hostile |
