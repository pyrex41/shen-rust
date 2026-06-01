# shen-rust GC — Step 2: Land the Collector as `src/gc/` — Implementation Handoff

**Date**: 2026-05-29. **Standalone** — execute from this without the
originating conversation. **Read first**: `design/gc-conversion-handoff.md`
(the whole-conversion plan; this is its Step 2 in detail) and, for context,
`design/perf-state-and-gc-ladder.md` §6 (design decisions + spike results).

---

## 0. Where we are (one paragraph)

The GC ladder (tracing GC → word-sized `Value` → Cranelift JIT) is greenlit:
both gating spikes passed (collector throughput 3.34×; conservative AOT-frame
roots sound). **Step 1 (the enabling refactor) is shipped** — commit
`94ab996` on `main`: `Value` now has a full constructor/inspector seam
(`Value::{nil,bool,int,float,sym,str,err}` + `{is_nil,is_cons,as_int,as_float,
as_bool,as_sym,as_str,head,tail}`), all `#[inline]`, and every hand-written
construction site + all of klcompile's emit code route through it. The enum
itself is unchanged; destructuring `match` arms still name the variants (Step 3
flips those). **Your job is Step 2: port the proven collector spike into a real,
tested `src/gc/` subsystem — NOT yet wired to `Value`.** This is almost entirely
additive: you create a new module and a bench; you touch no existing runtime
code. The point is to land the collector machinery, prove it correct (unit
tests + Miri) and fast (reproduce the spike's ~3.34× in isolation), so that
Step 3 (the big, risky `Value` flip) drops onto a collector that already works.

---

## 1. Your blueprint (port from this — don't reinvent)

**`crates/shen-rust/benches/gc_spike.rs`** (commit `5108f6a`) is the reference
implementation of exactly what Step 2 productionizes. Read it top to bottom
before writing anything — every design choice in it is deliberate and the
header comment explains why. Run it: `cargo bench --bench gc_spike`. Its proven
result is your Step-2 throughput gate:

```
boxed (24B Rc enum, reclaim/iter)   138.7 ms      (today's reality)
GC   (mark-sweep, Copy word)         41.6 ms      = 3.34× vs boxed
leaked ceiling (Option B)            40.9 ms      = 3.39× (no-reclaim ceiling)
→ GC retains ~98% of the ceiling, with collection running 498× (heap bounded).
```

What the spike already gives you, ready to lift:
- `Word` — the `Copy` `u64` tagged-word handle (low-3-bit tag: `000` fixnum,
  `001` cons, `011` nil). `gc_spike.rs:104-142`.
- `GcCons { head, tail, mark }`, `#[repr(align(8))]` so the low bits are free
  for the tag. `gc_spike.rs:188-193`.
- `Heap` — `all`/`free` vectors, soft `cap` trigger, reused `mark_stack`,
  `collections`/`peak_live` instrumentation. `alloc` (with the
  collect-before-grow path), `collect` (mark DFS + sweep-to-free-list), `Drop`.
  `gc_spike.rs:195-304`.
- The shadow-stack roots discipline: `roots: &[Word]` passed into `alloc`,
  pushed/popped around the live working set. `gc_spike.rs:310-343`.
- The measurement harness: 12 paired min-of-N, correctness oracle, and the two
  guard asserts (`collections > 0`, `peak_live ≥ one list`) that catch a
  vacuous "too-good" result. `gc_spike.rs:352-456`.

The companion **`benches/gc_roots_aot_spike.rs`** (commit `7e4eb78`) is the
*roots* blueprint — **for Step 4, not Step 2.** You don't need it yet, but its
existence is why Step 2 must build a real membership table (see §3.3).

---

## 2. Scope of Step 2 — what to build, what NOT to touch

**Build** a new `crates/shen-rust/src/gc/` module (register `pub mod gc;` in
`lib.rs`), a self-contained collector with:
1. `Gc<T>` — a `Copy` handle (tagged pointer) to a heap object of type `T`.
2. A non-moving `Heap`: block/slab allocation + free-list, soft-cap collection
   trigger, mark-sweep `collect(roots)`.
3. A `Trace` mechanism: how the collector enumerates a heap object's outgoing
   edges.
4. An O(1) **block/page membership table** (`is_heap_ptr(addr) -> bool`) — built
   now, consumed by Step 4's conservative scan.

**Do NOT** in Step 2:
- **Do NOT touch `value.rs` or wire the GC to `Value`.** That is Step 3, and it
  is gated on open design questions (bignum/overflow semantics of a 61-bit
  fixnum, boxed-float repr) that Step 2 does not need to answer. Keep the
  collector generic / tested against its own node types.
- **Do NOT regenerate AOT or change klcompile.** No call-site churn in Step 2.
- **Do NOT implement the conservative native-stack scan or register flush.**
  That is Step 4 (port `gc_roots_aot_spike.rs` then). Step 2's roots are
  precise (an explicit `&[root]` slice / shadow stack), exactly as in the spike.

If you find yourself editing a file outside `src/gc/`, `src/lib.rs` (one `mod`
line), and `Cargo.toml` (one `[[bench]]` block), stop — you've left Step 2.

---

## 3. Settled design decisions (don't re-litigate without new evidence)

These are fixed by `design/perf-state-and-gc-ladder.md` §6b/§6c and the spikes:

### 3.1 Algorithm: non-moving mark-sweep, `Copy` handle
Non-moving is doubly forced: (a) identity-constrained heap types (`Vec`,
`Stream`, `Foreign`) need stable addresses, and (b) it's what makes the Step-4
conservative AOT-frame scan *sound* (a false-positive root only over-retains,
never corrupts — moving would need precise stack maps we can't emit). The handle
is `Copy`: assigning / reading / tracing a `Gc<T>` does **zero** refcount work.
That `Copy`-ness is the entire ~2.5× win and the JIT prerequisite — preserve it.

### 3.2 The `Trace` story (design for ALL heap variants; test with cons + a cycle)
The collector must enumerate the outgoing `Gc`/`Value` edges of each heap
object. Anticipate every future heap variant even though Step 2 only *exercises*
a couple:
- **cons** → trace `head` and `tail`.
- **absvector (`Vec`)** → trace every cell. (Its cells are values and can form
  **cycles** — *this is the whole reason we need tracing rather than `Rc` + a
  cycle collector*.)
- **`Str` / `Error`** → no outgoing edges (leaf; bytes only).
- **`Foreign` / `Stream`** → **opaque leaves**: trace nothing, never move, but
  **run their Rust `Drop` when swept** (they own real resources — file handles,
  host objects). Decide and document how sweep distinguishes "plain reclaim"
  (cons → free-list) from "reclaim + run `Drop`" (Foreign/Stream).

Pick a representation for "what kind of node is this and what are its edges":
either a `Trace` trait object-ish dispatch, or (faster, and what the word-repr
wants) a **type tag in the node header / pointer** that the collector switches
on. Lean toward the tag — it's where Step 3 ends up anyway, and it avoids a
vtable per node. Whatever you choose, the `mark` bit and the type discriminant
live in the node header.

**Mandatory Step-2 test that the spike did NOT cover: cycle reclamation.**
Lists are acyclic, so `gc_spike.rs` never proved the tracing GC does the one
thing `Rc` cannot. Add a unit test: build a cyclic structure (e.g. two vec/cons
nodes pointing at each other), drop all external roots, `collect()`, and assert
the nodes were reclaimed (free-list grew / `peak_live` dropped). This is the
load-bearing correctness property of the whole approach — prove it explicitly.

### 3.3 Heap layout: block allocation + O(1) membership table (NOT the spike's `Box`/`HashSet`)
The spike allocates each node with `Box::into_raw` and tracks them in a `Vec`.
Production wants:
- **Block/slab allocation**: allocate nodes in contiguous blocks (e.g. a
  `Vec<Box<[Node; BLOCK]>>` or raw page-aligned blocks), bump-allocate within a
  block, reclaim to a free-list. Cuts per-node malloc and improves locality.
- **O(1) membership test** `is_heap_ptr(addr) -> bool` (and, for the
  conservative scan, "does this word point *into* a node we own?"): a sorted
  block-range table or a page table keyed by `addr >> PAGE_BITS`. The spike has
  no membership test at all (precise roots don't need one); **Step 4's
  conservative scan does**, and it's cleanest to build + unit-test the table now
  as part of the heap rather than bolt it on later. Test it directly: valid
  node addr → true; just-past-end / unaligned / heap-adjacent non-node →
  correct answer; interior pointer policy (we maintain **no interior pointers**
  — every `Gc` is a tagged head-of-node — so the table can be node-granular).

### 3.4 Safepoints: collection only at `alloc`
Collection runs only when `alloc` finds the free-list empty at the soft cap
(the spike model). Keep this — it means the root set only has to be valid at
allocation points, which Step 3/4 can reason about precisely.

### 3.5 Rejected (per §6c): `Rc` + cycle collector
It doesn't give `Copy` refs, so it wins neither the list workload nor the JIT.
Don't propose it.

---

## 4. The gate (non-negotiable — this is what "Step 2 done" means)

1. **Unit tests** in `src/gc/` covering: alloc → reachable after collect;
   unreachable node reclaimed; **cycle reclaimed** (§3.2); free-list reuse (no
   unbounded growth under churn); membership table correctness (§3.3); opaque
   leaf `Drop` actually runs on sweep (§3.2). These run under `cargo test
   --workspace` (already a gate).
2. **Miri clean on every `unsafe`.** The spikes were never Miri'd; this is
   shipped code with raw pointers, so it must be. Miri may not be installed —
   `rustup component add miri` (on the project's toolchain; add `+nightly` if
   the default channel lacks it), then
   `cargo +nightly miri test -p shen-rust gc::`. Expect to fix provenance
   issues (use `*mut`→`addr`→`*mut` round-trips carefully; consider
   `std::ptr` strict-provenance APIs). **Do not hand-wave a Miri failure** — a
   provenance error here is a real soundness bug that would corrupt the heap
   once `Value` is wired in.
3. **Bench reproduces the spike** in isolation: a `[[bench]]` (e.g.
   `gc_module_bench`, `harness = false`, registered in `Cargo.toml` next to the
   existing `gc_spike`/`gc_roots_aot_spike` entries) that runs the same
   list-build+sum workload through the **real `src/gc/` heap** and shows a
   **material fraction of the 3.34×** vs the 24B-`Rc` baseline, **with
   collection actually running** (assert `collections > 0` and `peak_live ≥ one
   list`, exactly like the spike — a vacuous run is a void measurement, not a
   fast one). Block allocation should, if anything, *beat* the spike's per-node
   `Box`; if the real module is markedly *slower* than the spike, find out why
   before declaring done (header overhead? membership-table cost on the hot
   path? — the membership table must NOT be touched during precise-root
   `alloc`/`collect`, only during the Step-4 conservative scan).
4. **Standard gates stay green**: `cargo fmt --all -- --check`; `cargo clippy
   --workspace --all-targets -- -D warnings`; `cargo test --workspace`;
   `scripts/gates.sh` all green (Step 2 shouldn't perturb kernel-tests at all —
   nothing is wired in — but run it to be sure nothing broke).

---

## 5. Measurement discipline (has caught false results repeatedly)

- Machine variance is ~5–12%, larger than many wins → **paired A/B, alternate
  runs, min-of-N, rebuild both sides clean.** The spike's harness already does
  this; copy its shape.
- **A "too-good" number is the tell, not the prize.** The gc_spike's first cut
  showed a false 7.26× because `cap` aligned every collection to a list boundary
  (empty live set → trivial mark). The AOT-roots spike took 3 wrong cuts. Both
  were caught by *asserting the hard case actually ran* (`peak_live ≥ n`,
  `collections > 0`). Keep those guards.
- Profile self-samples ≠ wall-clock. Trust the min wall-clock of the paired run.

---

## 6. File map

| Path | Role |
|---|---|
| `crates/shen-rust/benches/gc_spike.rs` | **The blueprint to port.** Collector core, Copy word, shadow-stack roots, 3.34× proven. |
| `crates/shen-rust/benches/gc_roots_aot_spike.rs` | Roots blueprint — **Step 4, not now**. Why §3.3's membership table exists. |
| `crates/shen-rust/src/gc/` (NEW) | What you build. |
| `crates/shen-rust/src/lib.rs` | Add one line: `pub mod gc;`. |
| `crates/shen-rust/Cargo.toml` | Add one `[[bench]]` for the module bench (see existing gc_spike entry ~line 29). |
| `crates/shen-rust/src/value.rs` | The seam Step 1 built. **Do not touch in Step 2** — reference only, to see the heap variants `Trace` must eventually cover (`Cons`/`Vec`/`Str`/`Closure`/`Stream`/`Error`/`Foreign`). |
| `design/gc-conversion-handoff.md` | The full plan; Step 2 is §4 there. |
| `design/perf-state-and-gc-ladder.md` | §6b constraints, §6c algorithm, §6f/§6g spike results. |

---

## 7. After Step 2 → Step 3 (so you know what you're enabling, not to do now)

Step 3 flips `Value` to `Value(u64)` over your `Gc`, converts the destructuring
match arms Step 1 left as enum to tag-dispatch, and regenerates AOT. It is
**blocked on open design questions** (`design/gc-conversion-handoff.md` §6):
bignum/overflow semantics for the 61-bit fixnum, boxed-`Float` repr, and the
real-heap over-retention magnitude. Resolve those **before** Step 3, not during
Step 2. Step 4 then wires hybrid roots (precise for the VM stack / `Interp`,
conservative for AOT frames) using the membership table you build now. Step 5
measures vs SBCL against a **hard ≥1.5× kill-gate** on real kernel-tests.

**Repo conventions**: commits go on `main` for perf work (user-authorized;
confirm scope before committing). Repo sets `includeCoAuthoredBy: false` →
**no `Co-Authored-By` trailer**. Disk hit 99% mid-session once — a "no space
left" error can masquerade as a test failure; check `df` if tests fail weirdly.
