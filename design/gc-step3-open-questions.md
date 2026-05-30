# GC Step 3 — §6 Open Questions RESOLVED (decision record)

**Date**: 2026-05-29. **Status**: research/decision session — no code written.
**Read first**: `design/gc-conversion-handoff.md` (§6 lists these questions; §4
Step 3 is what they gate). **Inputs**: three codebase investigations
(overflow-semantics, float-hotness, roots-audit) summarized below with the
evidence each rests on.

This resolves the five `gc-conversion-handoff.md` §6 open questions so Step 3
(flip `Value` → word-sized `Copy` tagged word over `Gc`) can begin. It also
pins the **concrete 3-bit tag layout** that falls out of the decisions, and
flags **one new blocking finding** (AOT closure captures) the original §6 list
under-anticipated.

---

## TL;DR decisions

| # | Question | Decision |
|---|---|---|
| Q1 | Bignum / overflow | **61-bit fixnum + promote-to-`Float` on overflow.** Matches today's *actual* (non-bignum) semantics, just shifts the threshold 2⁶³→2⁶⁰. No boxed-int `Kind`. Corpus never exceeds ~3.6e6. |
| Q2 | Float repr | **Boxed float** (a heap node holding the f64 bits; leaf). Floats are stone-cold. NaN-boxing rejected. |
| Q3 | Heap layout | **Already resolved by Step 2** — block = 1024×24B nodes, O(1) page-table membership, node-granular. Constants tunable under Step 4. |
| Q4 | Over-retention magnitude | **Deferred to Step 4** per plan — measure on the real heap. §6g's 7.7× is an artificial upper bound. Mitigation (klcompile stack-slot clearing) only if measured-needed. |
| Q5 | Roots created-but-unrooted | **Precise-root inventory produced** (below). **NEW BLOCKER**: AOT `move`-closure captures inside `Rc<NativeFn>` are opaque to both the stack scan and a container list → klcompile must emit a **traceable shadow capture `Vec<Value>`** per `make_aot_closure`. Expands Step 3 scope. |

---

## Q1 — Bignum / overflow semantics → 61-bit fixnum + float promotion

### Ground truth (evidence)
shen-cedar **does not have bignums today.** Integer arithmetic is implemented
identically in three kept-in-sync sites — `primitives.rs:542-577` (interpreter),
`aot/runtime.rs:135-195` (AOT inline, hot), `vm/exec.rs:348-351` (VM, dispatches
to the runtime helpers) — and every one uses:

```rust
// the uniform rule (aot/runtime.rs:136-147 add; sub/mul identical)
(Value::Int(x), Value::Int(y)) => match x.checked_add(*y) {
    Some(v) => Ok(Value::int(v)),
    None    => Ok(Value::float(*x as f64 + *y as f64)),   // overflow → Float (lossy)
}
```

- `+`/`-`/`*`: `checked_add/sub/mul`, on overflow **promote to `Float`** (loses
  precision above 2⁵³). `/`: always via `f64`, returns `Int` only if exact.
  Comparisons via `i64::cmp`. **No `wrapping`, no panic, no `i128`, no bignum
  library anywhere.**
- The KL parser (`kl/parser.rs:282-285`) turns an out-of-`i64`-range integer
  literal into `KlExpr::Float` *silently*. The runtime reader
  (`shen.compute-integer-h`, `aot/kernel/reader.rs:9047`) assembles big REPL
  integers via `rt::mul`/`rt::add`, so they too promote to `Float` past `i64`.
- **The whole test corpus and kernel sources contain no integer beyond ~3.6e6**
  (`10! = 3 628 800`, in a VM compiler test). `prime*?` runs on `1 000 003`;
  `count-change 100` → `4563`. Nothing approaches 2⁶⁰ (~1.15e18).
- Design docs *assume* "Shen expects unbounded integers" (`gc-conversion-handoff.md:197`,
  `value-representation.md:132`), but **that is an aspiration the current
  implementation already does not meet** — it promotes to float.

### Decision
**61-bit inline fixnum; on overflow of `+`/`-`/`*`, promote to a (boxed) `Float`
— exactly the existing rule, with the checked-overflow boundary moved from 2⁶³
to 2⁶⁰.** No new boxed-integer heap `Kind`.

Rationale: this **preserves the shipped semantics**; it does not introduce a new
divergence from standard Shen, it merely shifts an already-present float-promotion
threshold by 3 bits. The only behavioral delta is that integers in [2⁶⁰, 2⁶³)
— which are exact `i64` today — would become `Float`. **The corpus contains
none.** Adding a boxed-`i64` path would preserve exactness only up to 2⁶³ (still
not true bignums) at the cost of a whole heap `Kind` + arithmetic branch, for a
case that never occurs. Not worth it now.

Implementation notes for Step 3:
- `Value::int(n: i64)` must check `n` fits 61 bits; if not, construct a `Float`
  (`n as f64`). The three arithmetic sites keep their `checked_*`→float path but
  the threshold is now the 61-bit fixnum range, not `i64`. Centralize the
  fits-in-fixnum check so all three agree.
- `Value::as_int`/`as_fixnum` returns the sign-extended 61-bit value.
- **Gate the delta with the differential oracle + 134/0 kernel-tests** — any
  surprise (a hash, a `str` of a large number, a reader literal) shows up there.
- **Future bignum extension** (if a real workload ever needs it): add a
  `Kind::BigInt` heap node (the Step 2 collector takes new `Kind`s cleanly) and
  promote to it instead of `Float`. Documented as a clean extension point, not
  built now.

---

## Q2 — Float repr → boxed float (low-tag scheme), NaN-boxing rejected

### Ground truth (evidence)
Floats are **stone-cold on the north-star path.**
- **Zero** float literals in the type-checker (`t-star.kl`), sequent engine
  (`sequent.kl`), Prolog engine (`prolog.kl`), or any kernel KL file *except*
  `stlib.kl`'s math/trig library (constants `pi`/`e`/`g`, `sin`/`cos` stubs) —
  none of which the type-checker calls.
- The **entire 134-test corpus has exactly one float**: the spreadsheet test's
  expected `4000.0`/`5000.0` (`kerneltests.shen:29`), from two multiplications
  by `.8`/`.25`. One test case.
- The live leaf profile (`perf-state-and-gc-ladder.md:68-71`) names **no
  float function** in the hot cluster (all `eval_in`/`lookup_local`/cons/list).
- `shen_eq` cross-equates `Int(x) == Float(y)` via `(*x as f64) == *y`
  (`value.rs:326`) — still works through a boxed float (deref then compare).
- **No code depends on specific f64 bit patterns / NaN / infinity** as sentinels
  (only: `compare_op` errors on NaN; `value_hash` uses `to_bits`; div checks
  `== 0.0`). So NaN-boxing would disturb nothing — but it also buys nothing.

### Decision
**Box floats.** A `Float` is a heap node (a new `Kind::Float`) whose payload
word holds `f64::to_bits(x)`; it is a **leaf** (no outgoing edges, no `Drop`),
so it slots straight into the Step-2 collector's existing leaf machinery. The
word-sized `Value` uses the **low-3-bit tag scheme** (fixnums/immediates/pointers
fast); the cold float pays one heap node.

**NaN-boxing rejected**: it would make the f64 inline at the cost of boxing
*everything else* (pointers, the hot fixnums get awkward) — the exact-wrong
tradeoff for a list/int-heavy workload where floats are ~0% of operations.

---

## Q3 — Heap layout → resolved by Step 2

Step 2 (`crates/shen-cedar/src/gc/heap.rs`, commit `71f6b9f`) already chose and
shipped the production layout the §6 question asked for:
- **Block/slab allocation**: `Vec<*mut [Node]>`, `BLOCK_SIZE = 1024` nodes ×
  24 B = 24 KiB blocks, leaked via `Box::into_raw` (Miri-verified — keeping the
  `Box` live would Unique-retag and invalidate node pointers).
- **O(1) membership** (`is_heap_ptr`): a page table `HashMap<page, Vec<blockidx>>`
  with `PAGE_BITS = 12`, then a per-block range + node-stride-alignment check.
  **Node-granular, no interior pointers** (every `Gc` is a tagged head-of-node).
  This replaces the spike's `HashSet<addr>` that "won't scale."

**Decision**: keep as-is. `BLOCK_SIZE`/`PAGE_BITS` are tunable constants; revisit
only if Step 4's real-heap measurement shows membership or block churn on a hot
path (it is currently off the precise-root alloc/collect path entirely).

---

## Q4 — Over-retention magnitude → deferred to Step 4 (per plan)

The handoff itself scopes this to "measure early in Step 4." The §6g spike's
**~7.7×** is an artificial upper bound (a synthetic bench that deliberately
leaves stale list-heads in popped, uncleared stack slots). The real number
depends on the real call shapes and is **not knowable without the wired-in
collector**. 

**Decision**: do not pre-optimize. Step 4 measures the real over-retention; the
mitigation lever (klcompile emitting **stack-slot clearing** — zero a fn's
`Value` locals on exit) is implemented **only if the measured tax is large**.
Don't pay for precise stack maps. Recorded here so Step 3 doesn't try to solve
it prematurely.

---

## Q5 — Roots: precise-root inventory + a NEW blocking finding

The hybrid model: a **conservative native-stack scan + callee-saved register
flush** covers every `Value` in a plain Rust local on the live call stack
(tree-walker `eval.rs`, VM `exec.rs`, **and** AOT `aot/kernel/*.rs` frames — all
ordinary Rust frames). **Precise roots** are needed for `Value`s that live in a
**heap-allocated Rust container** reachable only through a pointer (a stack scan
finds the container *pointer*, not the `Value`s inside the malloc buffer).

### The precise-root set (must be registered / traced in Step 4)
- **`Interp.env`** (`env.rs:22-27`) — the dominant long-lived roots:
  `functions: Vec<Option<Value>>` (every defined fn as a `Value::Closure`),
  `globals: Vec<Option<Value>>` (`(set ..)`), `properties: HashMap<(SymId,SymId),Value>`.
- **`Interp.closure_cache`** (`eval.rs:142`) — roots `Value`s *transitively* via
  cached `Rc<BytecodeFn>.consts` (easy to miss: the cache's own fields look
  `Value`-free; the `Value`s hide one `Rc` deeper).
- **VM state** (`vm/exec.rs`): `stack: Vec<Value>` (live region `[0..len]`),
  `cur_upvals: Vec<Value>`, and each suspended `Frame.upvals: Vec<Value>`
  (`exec.rs:42-55`); plus every reachable **`BytecodeFn.consts: Vec<Value>`** and
  recursive `fn_consts` (`bytecode.rs:28-29`).
- **Closures** (`value.rs`): `Closure.partial: Vec<Value>` (`:49`),
  `ClosureKind::Bytecode(Rc<BytecodeFn>, Vec<Value>)` upvals (`:75`),
  `LambdaBody.captured: Vec<(SymId, Value)>` (`:82`). All behind `Rc` on the heap.
- **Absvectors**: `AbsVec = Rc<RefCell<Vec<Value>>>` (`value.rs:20`) — trace every
  cell (these can form cycles — *why* we trace rather than `Rc`+cycle-collector).
- **Leaf heap `Value`s** (`Str`/`Error`/`Stream`/`Foreign`) hold no nested
  `Value`s → GC objects but not interior roots. (`Foreign` *could* wrap host
  types holding `Value`s; current Cedar wrappers don't.)
- **No `Value`-holding statics/thread-locals** exist (only `BOOLEAN_SYM_IDS`
  and `VM_ENABLED`, neither a `Value`). Confirmed.

### 🔴 NEW BLOCKING FINDING — AOT `move`-closure captures are opaque
`make_aot_closure` (`aot/runtime.rs:92-104`) packages a Rust `move |interp,args| {…}`
as `ClosureKind::Native(Rc<NativeFn>)`. The generated AOT kernel emits these by
the thousand on the **hot CPS proof-search path** (e.g. `aot/kernel/types.rs:50-61`,
`t_star.rs:497`), each **capturing `Value` locals by value**:

```rust
let v_V5883 = v_V5883.clone();
let v_W5885 = v_W5885.clone();
rt::make_aot_closure("<lambda>", 1, move |interp, args| { … }, interp)
```

After the flip those captured `Value`s are **GC handles sealed inside a
`Box<dyn Fn>` on the malloc heap**. A stack scan sees only the `Rc<Closure>`
pointer; the collector cannot trace into a `dyn Fn`'s capture environment (no
field layout); and the closure being *reachable* (rooted via `Env.functions`)
does **not** make its captures *traceable*. So those handles' nodes would be
swept while the closure is live → dangling `Gc` → corruption. **This is not
covered by either the conservative scan or any registered-container list.**

**Resolution (additive, mechanical):** klcompile must emit, alongside each
`make_aot_closure`, an explicit **traceable shadow `Vec<Value>` of the same
captured handles**, stored in the `Closure` (extend `ClosureKind::Native` to
carry a `Vec<Value>`, mirroring how `ClosureKind::Bytecode` already carries its
upvals). The collector traces that `Vec`; because `Gc` is a `Copy` handle, marking
those nodes keeps the *same* nodes the `move`-captured handles point at alive —
**the closure body is unchanged**, it still reads its move-captures; the shadow
`Vec` exists purely so the GC can reach the nodes. klcompile already generates
the per-capture `.clone()` lines, so emitting `vec![v_V5883, v_W5885, …]` next to
them is mechanical, and it regenerates with the rest of the AOT.

This **expands Step 3's scope**: the `make_aot_closure` signature +
`ClosureKind::Native` shape + the klcompile codegen for captures must change as
part of the flip (not a separable afterthought), because correctness of *every*
AOT closure depends on it.

### Lesser hazards (covered, but noted)
- Tree-walker `ArgVec = SmallVec<[Value;4]>` (`eval.rs:41`) and VM argv buffers
  (`exec.rs:186,271,309`): inline (≤4) → on the stack (scan-covered); **spilled
  (>4) → malloc buffer** held live across `apply` → needs rooting (or guarantee
  it stays scan-reachable via its stack-resident handle — verify in Step 4).
- `Scope::Owned(Vec<(SymId,Value)>)` (`eval.rs:54-101`): heap `Vec`, but its
  handle is a live `eval_in` stack local while in scope → scan-covered.

---

## The tag layout this pins down (3 low bits, 8 slots)

Resolving Q1 (fixnum) + Q2 (boxed float) + the Step-2 node `Kind` header lets us
fix the word scheme. **One pointer tag suffices** because the heap `Node` already
carries a `Kind` byte (Step 2) that distinguishes the heap variants:

| Tag `0bxyz` | Meaning | Payload |
|---|---|---|
| `000` | **fixnum** | 61-bit signed, inline |
| `001` | **heap pointer** | node addr; node's `Kind` byte = Cons/Vec/Str/Closure/Stream/Error/**Float**/Foreign |
| `010` | **sym** | `SymId` (u32) inline |
| `011` | **nil** | — |
| `100` | **bool** | 1 payload bit (true/false) |
| `101`–`111` | spare | (char? immediate-small-str? future) |

The four **hottest** variants — `Bool` (4360 sites), `Sym` (3726), `Nil` (1719),
fixnum `Int` (974) — are all **immediates** (no heap, pure `Copy`), which is
exactly where the value-repr spike's ~1.5× lives.

**Step-2 `Kind` extension for Step 3**: the collector's `Kind` enum (today
`Free/Cons/Vec/Blob/Opaque`) extends to the heap variants —
`Str`/`Error` → `Blob` (leaf), `Stream`/`Foreign` → `Opaque` (leaf + `Drop`),
**`Float` → a new leaf `Kind`** (f64 bits in word `a`), and **`Closure` → a new
*traced* `Kind`** (its `partial` + upvals + the Q5 shadow-capture `Vec` are
edges — it is NOT opaque). The Step-2 heap was built generic for exactly this;
adding `Kind`s is the designed extension path.

---

## Revised Step 3 scope (what these resolutions imply)

1. Rewrite `value.rs`: `Value(u64)` `#[repr(transparent)]` `Copy`, the tag scheme
   above, immediates inline, heap variants as `Gc` behind tag `001`. Keep the
   Step-1 constructor/inspector API stable.
2. Centralize the **61-bit fits-in-fixnum** check; wire the three arithmetic
   sites' overflow→`Float` path to it.
3. Extend the gc `Kind` set: `Float` (leaf), `Closure` (traced).
4. **Rework `make_aot_closure` + `ClosureKind::Native` to carry a traceable
   shadow capture `Vec<Value>`**, and update klcompile to emit it — the Q5
   blocker. (This is the one piece of *new* surface beyond "flip + regen.")
5. Convert the destructuring match arms Step 1 left to tag-dispatch.
6. Regenerate AOT; gate on 134/0 + differential oracle + workspace tests + Miri +
   fmt/clippy. Roots wiring + over-retention measurement remain Step 4.

**Still genuinely open (not blocking Step 3 design, but Step 4 inputs):** the
real over-retention magnitude (Q4), and whether spilled-`SmallVec` argv buffers
need explicit rooting or are reliably scan-reachable (Q5 lesser hazard).
