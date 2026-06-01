# `Value` Representation Design

**Status**: Draft → in implementation (Phase 1)
**Date**: 2026-05
**Related**: PERFORMANCE.md, design/runtime-execution-strategy.md, `crates/shen-rust/src/value.rs`

---

## 1. Why this document exists

`PERFORMANCE.md` names a "Value-representation overhaul + cons arena" as part of
the path from ~5.7s to sub-2s on `--kernel-tests`. The dominant profile entry is:

```
drop_in_place<Value>   ≈ 857 samples   (#1)
eval_in   ≈ 376  +  lookup_local ≈ 241  (the tree-walker / type-checker)
```

The instinct (and the deferred **T4** note, and the generic "blazing-fast Shen
port" advice about *unboxing* and *NaN-boxing*) is to reach for a tagged-pointer
or NaN-boxed `Value`. This document argues that **that instinct is aimed at the
wrong cost**, grounds the decision in the actual representation, and lays out a
phased plan whose ordering is driven by blast radius and the project's
robustness/TCB ethos.

## 2. The current representation (measured, not assumed)

`Value` is a plain Rust enum (`value.rs`). Variant payload sizes on this target:

| Variant | Payload | Heap? |
|---|---|---|
| `Nil` / `Bool` / `Int(i64)` / `Float(f64)` / `Sym(SymId)` | ≤ 8 B, inline | **no** |
| `Cons(Rc<(Value, Value)>)` | 8 B (thin ptr) | yes (1 alloc/cell) |
| `Vec` / `Closure` | 8 B (thin `Rc`) | yes |
| `Str(Rc<str>)` / `Error(Rc<str>)` | **16 B (fat ptr)** | yes |
| `Foreign(Rc<dyn Any>)` | **16 B (fat ptr)** | yes |

`i64`/`f64` occupy a full 8-byte word with **no spare niche**, so the
discriminant cannot be packed in for free. The fattest payload is 16 B (the
`Rc<str>` / `Rc<dyn Any>` fat pointers), so:

```
size_of::<Value>() == 24   (16 B payload + 8 B tag/align)
```

A cons cell is therefore `Rc<(Value, Value)>` = `2×24` payload + 16 B of `Rc`
strong/weak counts = **64 B/cell**.

## 3. The key insight: immediates are *already* unboxed

The headline win people expect from NaN-boxing/tagging is "small scalars stop
touching the heap." **In this enum they already don't.** `Int`, `Float`, `Sym`,
`Bool`, and `Nil` are inline, zero-allocation, today. There is no fixnum boxing
to eliminate.

What remains on the heap is genuinely *structural* data: cons cells, strings,
closures, vectors, foreign handles. Of these, the profile is unambiguous — the
type-checker is **list-processing end to end**, building and discarding huge
numbers of transient cons cells. The cost is not "scalars are boxed"; it is:

1. **`malloc`/`free` traffic** for transient cons cells (the bulk of
   `drop_in_place<Value>` is the `free()` at the bottom of each cell's drop), and
2. **`clone`/move byte traffic** for a 24-byte `Value` on the hot paths
   (`lookup_local` clones, argument passing, cons head/tail extraction).

**A tagged/NaN-boxed `Value` does not reduce (1) at all** — a cons cell is still
a heap-allocated pair in any representation; you still `malloc` and `free` it.
It only helps (2), and only partially, and at the cost of a large `unsafe`
surface and the i64 problem below. So NaN-boxing is *not* the highest-leverage
move here. This is the central correction this document makes.

## 4. Option space

### Option A — Cons-cell recycling pool *(recommended first)*

Replace `Rc<(Value, Value)>` with a thin, manually ref-counted cons pointer
backed by a **thread-local free list**. On the last drop a cell is pushed onto
the free list instead of being returned to `malloc`; allocation pops from the
free list instead of calling `malloc`. Ref-count semantics (shared sub-structure,
`ptr_eq` fast path in `shen_eq`) are preserved exactly.

- **Attacks the #1 cost directly**: turns N×`free()` + N×`malloc()` on transient
  lists into N×`Vec::push` + N×`Vec::pop`.
- **Blast radius is tiny and self-contained**: generated AOT code builds cons via
  `rt::cons(&a,&b)` → `Value::cons(...)`, and reads via `p.0`/`p.1`. Everything
  funnels through `value.rs` + ~16 call sites. **No codegen change, no AOT
  regeneration, and `kernel-aot-audit.sh` (byte-diff of generated source) stays
  green automatically.** Fast build/validate loop.
- **Cost / risk**: introduces a small `unsafe` module (manual refcount + free
  list). Mitigated by isolation, the 134 cons-heavy kernel tests, dedicated unit
  tests, and **Miri** on those unit tests.
- **Bonus available**: an *iterative* spine-drop (flatten the linked-list drop
  instead of recursing) would also let us delete the 1 GB worker-stack hack in
  `bin/shen-rust/src/main.rs`. Deferred to a follow-up to keep the first cut
  semantically identical to today.

### Option B — Shrink `Value` 24 B → 16 B *(safe, complementary, later)*

Move the three 16-byte fat variants (`Str`, `Error`, `Foreign` — and `Stream`)
behind a single thin `Rc<Cold>` wrapper so every payload is ≤ 8 B and
`size_of::<Value>() == 16`.

- **Win**: every `Value` clone/move drops from 24→16 B (−33%); cons cells drop
  64→48 B (−25%) → better cache density and a smaller malloc size class. Helps
  cost (2) for *every* engine (tree-walker, VM, AOT) equally.
- **No `unsafe`.**
- **Downside**: strings/errors/foreign become **2 allocations** instead of 1
  (the cold `Rc` node + the string buffer), because std's only single-allocation
  shared string (`Rc<str>`) is the fat pointer we're trying to remove. Profiling
  says these are *cold* (the `(tc +)` loop is symbols + cons, not string
  construction), so this is expected to be acceptable — **but it must be
  measured, not assumed.**
- **Downside**: changes how codegen emits string literals
  (`Value::Str(Rc::from(..))` → `Value::str(..)`), so it **requires a full AOT
  regen + re-bless of the audit gate** (long loop). This is why it sequences
  *after* Option A despite being the safer change in isolation.

16 B is the floor for a safe enum (the i64/f64 tag problem). Going below requires
Option C.

### Option C — Tagged-pointer / NaN-boxed 8-byte `Value` *(evaluated, deferred)*

Pack everything into one 64-bit word: immediates as tagged bit patterns, heap
objects as pointers in the spare bits.

Rejected as a near-term step, for concrete reasons:

1. **The win is marginal over A+B.** Immediates are already unboxed (§3); cons
   `malloc`/`free` is unchanged (a cons is still a heap pair); the only delta
   over Option B is 16 B → 8 B on clones. Diminishing returns.
2. **i64 doesn't fit.** Shen numbers are `i64`. NaN-boxing/low-bit tagging leaves
   ~48–61 payload bits, so full-width integers must be heap-boxed — *introducing*
   boxing for a type that is inline today. A net regression for integer-heavy code
   unless paired with a fixnum/bignum split.
3. **Large `unsafe` surface + manual refcounting** for *every* heap variant (you
   can no longer lean on `Rc`'s `Drop`). For a project whose primary value is a
   small, auditable TCB, this is the worst risk/reward of the three.

Revisit only if A+B land and clone-byte-traffic still dominates a profile.

### Option D — Tracing GC

The essay's "generational collector." Replaces `Rc` wholesale. Largest TCB
impact, largest engineering cost, no incremental landing path. Out of scope.

## 5. Recommended sequence

| Phase | Change | `unsafe`? | Regen? | Audit gate | Expected effect |
|---|---|---|---|---|---|
| **P1a** | Encapsulate cons behind a `ConsCell` newtype + accessors (internals unchanged) | no | no | auto-green | none (refactor; de-risks P1b) |
| **P1b** | Swap `ConsCell` internals to a recycling pool | yes (isolated) | no | auto-green | attacks `drop_in_place` #1 |
| **P2**  | Shrink `Value` to 16 B (cold-variant wrapper) | no | **yes** | re-bless | −33% clone bytes, −25% cell size |
| **P3**  | Tagged/NaN-box 8 B | — | — | — | **deferred** (§4C) |

P1a is a pure, instantly-landable refactor that turns both P1b and any future
representation swap into a single-file change. P1b is the real lever and is
fully validatable in one build cycle (no regen). P2 is a safe, complementary win
gated on measurement and a regen cycle.

## 6. Validation & robustness

- **Differential**: the existing tree-walker remains the reference; run the same
  cons-heavy fragments through both (already the plan in
  runtime-execution-strategy.md §6).
- **Unit tests** for the pool: refcount inc/dec, recycle-on-zero, shared
  sub-structure not recycled while aliased, `ptr_eq` identity, free-list cap.
- **Miri** on the pool unit tests to catch UB in the `unsafe` block.
- **Gates**: `scripts/gates.sh` (all 8) green; `--kernel-tests` 134/0; warm
  timing recorded in PERFORMANCE.md before/after each phase.
- `unsafe` confined to one module (`value/cons.rs`) with a header comment stating
  the invariants, consistent with how the project isolates `unsafe` elsewhere.

## 6b. Measured results — P1a + P1b (2026-05-28)

Implemented `ConsCell` (P1a, safe newtype) and the recycling pool (P1b,
isolated `unsafe`, Miri-clean). All 8 gates' substance green: workspace tests
pass (lib 50, integration 45), `--kernel-tests` 134/0, `kernel-aot-audit` OK
(generated source unchanged, as predicted), fmt + clippy `-D warnings` clean.

**Performance: the pool is a small lever — within machine noise.** Back-to-back
A/B on the same build (cleanest controlled comparison):

| Config | `run time` (×2) |
|---|---|
| pool **off** (`CAP=0`) | 5.57, 5.58 s |
| pool **on** (`CAP=64K`) | 5.47, 5.43 s |

≈ **2.5%** for the pool. But a later settled batch drifted 5.84→6.07s under
thermal throttling, i.e. run-to-run variance (~5–12%) **exceeds** the effect.
So: real, repeatable in controlled A/B, but small.

**Why so small, vs. `drop_in_place` being ~30% of samples?** `free()` is only
part of a cons drop; the refcount decrement, the recursive spine walk, and
dropping the *head* `Value`s are untouched by pooling — as is the tree-walker
(`eval_in`/`lookup_local`), which the single heaviest test (≈3.3s, the
type-checker on `interpreter.shen`) is dominated by. **This empirically confirms
the doc's thesis**: cons `malloc`/`free` is a minor lever; the
tree-walker→bytecode-VM migration is the real one. The lasting value of P1 is
the `ConsCell` **seam** (P1a), which makes P2's 16-byte shrink — and any future
repr swap — a single-file change.

**Recommendation for review**: keep P1a unconditionally. Treat P1b (the pool)
as optional — it pays ~2.5% for ~180 lines of isolated, Miri-validated `unsafe`.
Reasonable to keep, tune (cap), fold into an eventual iterative-drop that retires
the 1 GB stack hack, or revert to P1a-only pending the VM work. A `CAP=0` build
makes it a no-op without code changes.

## 6c. Arena spike — value-churn lever FALSIFIED (2026-05-28)

To test whether the value-churn bucket (`drop_in_place<Value>` ≈ 20% of profile
self-samples — the largest single leaf) is a real wall-clock lever, we replaced
`ConsCell` with a **leaked bump arena**: `Copy` pointer into never-freed chunks,
**no `Drop`, no refcount, no `free`, no recursive spine drop**. This is the
*ceiling* of any value-churn optimization (a perfect GC could do no better than
"free").

Controlled **paired** A/B (alternating `rc` and `arena` binaries, 6 pairs, so
thermal load is shared; everything held constant except the cons impl):

| | mean | min |
|---|---|---|
| RC (refcount + recursive drop + free) | 7.53 s | 6.71 s |
| ARENA (zero cons churn) | 7.35 s | 6.46 s |

**Delta ≈ 2.4%, within per-pair noise.** So eliminating *all* cons value churn
buys ~2–3%, not ~20%. The profile's self-time attribution to `drop_in_place`
does **not** translate to wall-clock: remove it and bump-alloc overhead +
everything else absorbs it (modern `free` and non-atomic `Rc` decrement are
cheap).

**Conclusion: a GC or arena will NOT close the 5× gap vs shen-cl.** Combined
with the earlier nulls (cons pool ~2.5%, dispatch `Rc::clone` removal ~0%), the
5× is **not a concentrated lever** — it is distributed: every interpreted,
dynamically-dispatched, boxed operation costs ~2–5× its SBCL native+GC
equivalent, across millions of operations. The only remaining real lever is the
**execution model** (tree-walk + dynamic `Value::Closure` dispatch → near-native
codegen for dynamic code), per runtime-execution-strategy.md. The biggest
addressable chunk is interpretation + dispatch (~36%).

**Decision**: keep P1a (the `ConsCell` seam — harmless, no `unsafe`, no leak);
**drop the pool** (its `unsafe` is not worth ~2.5% when even the ceiling is
~2.4%); shelve P2/P3 (Value-repr work cannot move a distributed-execution-cost
gap). Redirect effort to the bytecode VM.

## 7. Open questions

- Free-list cap policy (fixed cap vs. high-water shrink) — start with a fixed cap
  (e.g. 64K nodes), tune from a heap profile.
- Should P1b also adopt iterative spine-drop now (and retire the 1 GB stack
  hack), or land that as an explicit follow-up? (Leaning: follow-up, to keep the
  first cut byte-for-byte semantically identical.)
- Does P2's string double-allocation regress any string-heavy gate? Measure
  `writer`/reader paths specifically.
