# shen-rust GC — Step 3: Flip `Value` to a Word-Sized `Copy` Tagged Word over `Gc` — Implementation Handoff

**Date**: 2026-05-29. **Standalone** — execute from this without the originating
conversation. **Read first, in order**:
1. `design/gc-step3-open-questions.md` — the §6 decisions this builds on (tag
   layout, fixnum/float, the AOT-closure blocker). **Authoritative for the
   "what."**
2. `design/gc-conversion-handoff.md` §4 Step 3 + §5 gates — the overall plan.
3. `crates/shen-rust/src/gc/{mod,node,heap}.rs` — the Step-2 collector you build
   on (commit `71f6b9f`). Read its API; you will extend it.
4. `crates/shen-rust/src/value.rs` — the enum you are replacing, with the Step-1
   constructor/inspector seam already in place.

This is **THE BIG ONE** (handoff §4 Step 3). It is the highest-risk rung of the
ladder. Do it in the sub-steps below, each independently gated; do **not** flip
everything at once.

---

## 0. Where we are

- **Step 1 shipped** (`94ab996`): `Value` has a full constructor/inspector seam
  (`Value::{nil,bool,int,float,sym,str,err,cons,list}` + `{is_nil,is_cons,as_int,
  as_float,as_bool,as_sym,as_str,head,tail}`), all `#[inline]`. Construction is
  funneled through these; klcompile emits through them. **Destructuring `match`
  arms still name the enum** — Step 3 converts those.
- **Step 2 shipped** (`71f6b9f`): `src/gc/` — a non-moving mark-sweep collector
  with a `Copy` `Gc` handle, 24-byte packed `Node` (`Kind` header byte: `Free/
  Cons/Vec/Blob/Opaque`), block allocation, O(1) page-table membership
  (`is_heap_ptr`), `Kind`-tagged trace/reclaim. Miri-clean. Bench 3.62× vs the
  Rc baseline. **Not wired to `Value`.** Its `alloc_*` take an explicit
  `roots: &[Gc]` and collect at a soft cap.
- **§6 resolved** (`5d2ab68`, `gc-step3-open-questions.md`): all five open
  questions decided. **Read that doc** — this handoff implements its decisions.

Your job: make `Value` a word-sized `Copy` tagged word over `Gc`, regenerate
AOT, keep 134/0 + the differential oracle green — **with collection still off
(grow-only)**. Wiring precise roots + turning collection on is **Step 4**, not
this. That seam is deliberate and load-bearing (§2 below).

---

## 1. The target representation (from `gc-step3-open-questions.md`)

`Value` becomes `#[repr(transparent)] pub struct Value(u64); // Copy`. The low 3
bits tag it:

| Tag | Meaning | Payload | Heap? |
|---|---|---|---|
| `000` | fixnum | 61-bit signed, inline | no |
| `001` | heap pointer | node addr (8-aligned); the node's `Kind` byte says which heap variant | **yes** |
| `010` | sym | `SymId` (u32) in high bits | no |
| `011` | nil | — | no |
| `100` | bool | 1 payload bit (true/false) | no |
| `101`–`111` | spare | — | — |

- **Immediates** (`fixnum`, `sym`, `nil`, `bool`) are pure `Copy` words — no
  heap, no refcount. These are the 4 hottest variants (Bool 4360, Sym 3726,
  Nil 1719, Int 974) and where the ~1.5× repr win lives.
- **Heap variants** are tag `001` + a `Gc` pointer; the **Step-2 node `Kind`
  byte** disambiguates them. One pointer tag suffices because the node is
  self-describing. Extend `Kind`:
  - `Cons` → `Kind::Cons` (already exists; traced: head/tail).
  - `Vec`/`AbsVec` → `Kind::Vec` (exists; traced: cells; mutable; cycles).
  - `Str`, `Error` → `Kind::Blob` (exists; leaf; bytes).
  - `Stream`, `Foreign` → `Kind::Opaque` (exists; leaf; runs Rust `Drop` on
    sweep).
  - **`Float` → new `Kind::Float`** (leaf; `f64::to_bits` in node word `a`).
  - **`Closure` → new `Kind::Closure`** (**traced**, NOT opaque: its `partial`
    `Vec<Value>`, its `Bytecode` upvals, and the new AOT shadow-capture vec are
    all edges — see §5).

**Fixnum overflow (Q1):** a 61-bit fixnum can't hold full `i64`. `Value::int(n)`
must check `n` fits `[-2^60, 2^60)`; if not, build a **boxed `Float`**
(`n as f64`). The three arithmetic sites (`primitives.rs:542-577`,
`aot/runtime.rs:135-195`, VM via those) keep their `checked_*`→`Float` path but
the boundary is now the fixnum range — **centralize the fits-check** so all
agree. This matches today's *actual* (non-bignum) behavior; see the decision doc.

**Float (Q2):** boxed, never inline. NaN-boxing was rejected.

---

## 2. ⚠️ THE central sequencing decision: collection stays OFF in Step 3

The collector reclaims only what its root set marks. The **real** root set (VM
stack, `Env.functions/globals/properties`, closure captures, AOT native frames)
is wired in **Step 4** (`gc-step3-open-questions.md` §Q5 has the full inventory).
If Step 3 turned collection on without that, it would free live objects → chaos.

**So Step 3 runs the heap GROW-ONLY (no reclamation).** Concretely: the
thread-local heap (§3) is constructed so `collect()` never fires during a run —
either a `collection_enabled: false` flag short-circuiting `alloc_raw`'s
collect-branch, or an effectively-infinite `cap`. Allocation just grows; nothing
is reclaimed; `Drop`-bearing leaves (`Stream`/`Foreign`) run their `Drop` only at
process exit via `Heap::drop`.

This is sound and sufficient for the Step-3 gate: kernel-tests + the differential
oracle are **bounded runs**, so grow-only memory is fine, and it proves the
**representation** is correct in isolation from the roots machinery. **Step 4**
then registers roots, lowers the cap, turns collection on, and re-gates under
sustained collection (handoff §4 Step 4). Do not try to collect in Step 3.

> If a reviewer asks "doesn't grow-only leak?": yes, deliberately and
> temporarily — one bounded test run. It is the only way to land the repr flip
> without also landing the (separately-gated) roots subsystem in the same step.

---

## 3. Where the `Heap` lives — a thread-local, so constructors stay signature-stable

Step 1's whole point was a stable `Value::cons(a, b)` / `Value::str(s)` API so
the flip doesn't re-churn thousands of call sites. But `Value::cons` has **no
`Heap` parameter** and must not gain one. Resolution: a **thread-local GC heap**.

```rust
thread_local! { static HEAP: RefCell<Heap> = RefCell::new(Heap::grow_only()); }
```

- `Value::cons(a, b)` does `HEAP.with(|h| Value::from_gc(h.borrow_mut().alloc_cons(a.to_gc(), b.to_gc(), &[])))`.
  (Roots `&[]` because collection is off in Step 3.)
- `Value::str`, `Value::float`, vec construction, closure construction likewise.
- Lazy-init on first access → `value.rs` unit tests and the differential oracle
  (which build `Value`s without an `Interp`) work with no explicit setup.
- Single-threaded runtime (the kernel runs on one worker thread), so a
  thread-local is correct; if a second thread ever touches `Value`, it gets its
  own heap (acceptable — they don't share `Value`s today).

Trade-off: thread-local access has a small cost on the hot path. Accept it for
now; if Step 5 profiling shows `HEAP.with` hot, revisit (e.g. pass the heap via
`Interp` on the paths that have one). **Do not** prematurely thread `&mut Heap`
everywhere — that re-churns the call sites Step 1 protected.

`shen_eq` and the `head`/`tail` inspectors also reach `HEAP` to deref nodes
(§4c).

---

## 4. Build order (each sub-step independently compiles + gates)

### 4a. Extend the gc module (`src/gc/`) — additive, Miri-gated
Add to the Step-2 collector, mirroring its existing patterns:
- `Kind::Float` (leaf) and `Kind::Closure` (traced) variants.
- `Heap::alloc_float(bits: u64, roots) -> Gc`, `alloc_str(&str)`, and a closure
  allocator carrying the traced fields (head/tail-style). Accessors:
  `float_bits(gc)`, `str_bytes(gc)`, closure-field readers.
- Extend `trace_node` for `Kind::Closure` (trace partial + upvals + shadow
  captures) — `Float`/`Blob`/`Opaque` stay leaves.
- A `Heap::grow_only()` constructor (or a `collection_enabled` flag) per §2.
- **Gate**: new unit tests (alloc/read each new Kind; closure cycle reclaimed
  when collection *is* manually invoked in a test); `cargo +nightly miri test
  -p shen-rust gc::` clean; fmt/clippy.

### 4b. The `Value(u64)` newtype + conversions — behind the existing API
- Define `Value(u64)`, the tag constants, `to_gc()`/`from_gc()` internal
  converters, and **reimplement the Step-1 constructors/inspectors** over it.
  Keep their **signatures identical** (the seam) except where physically forced
  (§4c head/tail).
- Wire `Value::int` to the centralized 61-bit fits-check → fixnum or boxed Float.
- **Gate after this sub-step is hard**: the rest of the crate still
  pattern-matches the *old* enum, so do 4b + 4d together on a branch, or land 4b
  with a temporary `From`/compat shim. Recommended: do 4b→4d as one compile unit.

### 4c. The `head`/`tail` inspector lifetime
Step 1 deliberately made `head(&self) -> Option<&Value>` so post-flip it returns
a reference **into the pinned heap node** (non-moving GC ⇒ the node address is
stable while `self` is reachable). Implement by reading the node via `HEAP` and
returning a `&Value` into the node's payload word, with the borrow tied to
`&self`. This needs a small, well-commented `unsafe` lifetime bridge (the
reference's true provenance is the heap node, not `self`); justify it by the
non-moving invariant. If the lifetime proves too thorny, the fallback is to
change `head`/`tail` to return `Value` (by Copy) and fix the ~N call sites — but
try the `&Value` form first to honor the stable API.

### 4d. Convert the destructuring `match` arms to tag-dispatch
The ~184+ arms Step 1 left naming the enum (`Value::Int(n) =>`, `Value::Cons(c)
=>`, …) must become tag-checks + accessors. This is the bulk of the mechanical
work. Technique that worked in Step 1: **the compiler is your safety net** —
after removing the enum variants, every un-converted `match` arm is a hard type
error, so you cannot silently miscompile. Convert file-by-file, leaning on
`cargo check`. `shen_eq` (`value.rs:305`) is the densest single site (reach
`HEAP` to compare cons/vec/str structurally).

### 4e. The `Copy`-`Value` fallout (do not skip — it fails the gate otherwise)
`Value: Copy` means `value.clone()` triggers `clippy::clone_on_copy`, and the
gate is `clippy -D warnings`. There are thousands of `.clone()` sites. Two
options:
- **Fast/reversible**: a crate-level `#![allow(clippy::clone_on_copy)]` to land
  the flip, with a TODO to clean up.
- **Clean**: codemod `.clone()` off `Value` receivers (safe — since `Value:
  Copy`, removing `.clone()` never changes behavior; the compiler confirms).
  Hard to scope textually (which `.clone()` is on a `Value`?), so likely a
  guided pass.

Recommend allow-first to land, optional cleanup after green. Also: `Value` no
longer has a meaningful `Drop` — resource `Drop` for `Stream`/`Foreign` now
happens on GC sweep (Step 4) or at `Heap::drop` (Step 3). Verify nothing relies
on eager `Value`-drop ordering (streams flushed on drop, etc.); the differential
oracle + kernel-tests should surface regressions.

### 4f. The AOT shadow-capture rework + regen — see §5
Then **regenerate AOT** (`scripts/codegen-kernel-aot.sh`) and gate the full set.

---

## 5. 🔴 The AOT closure-capture rework (the one NEW surface — see Q5)

This is the load-bearing new piece and **must** be done as part of Step 3, not
deferred. The AOT codegen emits closures that **`move`-capture cloned `Value`s**
into `ClosureKind::Native(Rc<dyn Fn>)` via `make_aot_closure`
(`aot/runtime.rs:92-104`). Example (`aot/kernel/types.rs:50-61`):

```rust
let v_V5883 = v_V5883.clone();
let v_W5885 = v_W5885.clone();
rt::make_aot_closure("<lambda>", 1, move |interp, args| { … }, interp)
```

Post-flip, `v_V5883`/`v_W5885` are GC handles **sealed inside an opaque
`Box<dyn Fn>`** — the collector cannot trace into a `dyn Fn`'s captures, and the
closure being rooted does **not** make its captures traceable. Step 4's
collection would free those nodes → dangling `Gc`. (Untraceable by BOTH the
conservative stack scan and any container list.)

**Fix (additive, mechanical):** carry a **traceable shadow `Vec<Value>` of the
captured handles** alongside the closure, and trace it.
1. Change `make_aot_closure` to also take `captures: Vec<Value>` and store it in
   the `Closure` (extend `ClosureKind::Native` to `Native(Rc<NativeFn>,
   Vec<Value>)`, mirroring `Bytecode(Rc<BytecodeFn>, Vec<Value>)`).
2. Change **klcompile** (`crates/klcompile/src/main.rs`) to emit, next to the
   existing per-capture `.clone()` lines, the vec: `vec![v_V5883, v_W5885]`.
   (klcompile already knows the capture list — it generates those `.clone()`s.)
3. The collector traces that shadow vec (the `Kind::Closure` edges). Because
   `Gc` is `Copy`, marking those nodes keeps the *same* nodes the `move`-captured
   handles point at alive — **the closure body is unchanged**; the shadow vec
   exists purely so the GC can reach the nodes.

This is correctness-critical for *every* AOT closure. Regen AOT after the
klcompile change.

> Note: in Step 3 (collection off) nothing is freed, so a bug here won't crash
> the Step-3 gate — it will only bite in Step 4. **Build and test it now anyway**
> (e.g. a unit test that manually collects with an AOT-style native closure
> rooted and asserts its captures survive), because Step 4 will assume it's done.

---

## 6. The gate ("Step 3 done")

1. `scripts/kernel-tests.sh` → **134 passed, 0 failed** (both engine modes).
2. `tests/vm_differential.rs` green (VM↔tree-walker equivalence — your primary
   correctness net through the conversion).
3. `cargo test --workspace` green; `cargo +nightly miri test -p shen-rust gc::`
   clean (the gc module's new `unsafe`); `scripts/kernel-aot-audit.sh`;
   `scripts/gates.sh` all green.
4. `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -D
   warnings` (this is what forces the §4e `clone_on_copy` decision).
5. **Paired A/B, min-of-N, cool**: report the kernel-tests number. Expectation
   with collection OFF: roughly flat-to-modestly-better (the immediate `Copy`
   win minus thread-local/boxing overheads; the big list win needs collection's
   Copy-refs, which is Step 4). **Do not expect the full 2-3× yet** — that's the
   Step-4/5 number. Just confirm no large regression.

**This is a flip, so `Value` size/Copy-ness is itself a gate**: assert
`size_of::<Value>() == 8` and `Value: Copy` in a test.

---

## 7. Risks, measurement discipline, fallback

- **Measurement** (has caught false results repeatedly): machine variance
  ~5–12% → always paired A/B, alternate runs, min-of-N, rebuild both sides
  clean. A spike/flip that breaks the workload gives a **void** measurement, not
  a fast one — assert the suite actually ran 134/0.
- **The fixnum-threshold delta (Q1)**: integers in [2^60, 2^63) become `Float`
  where they were exact `i64`. The corpus has none, but the differential oracle +
  134/0 will catch any surprise (a hash, a `str` of a big number, a reader
  literal). If one appears, the boxed-`i64`/`Kind::BigInt` extension is the
  escape hatch (decision doc Q1).
- **head/tail lifetime (§4c)** and the **AOT shadow-capture (§5)** are the two
  places most likely to go wrong. Give them dedicated unit tests.
- **Fallback** (handoff §8): if the flip proves too costly or destabilizing,
  the **fixnums-only** variant (inline-tag `Int/Bool/Nil/Sym`, keep heap types
  `Rc` behind the tag, no GC heap for them) banks the lifetime-independent ~1.6×
  on arithmetic paths with far less risk. Keep it in your pocket.
- Disk hit 99% mid-session once — a "no space left" error can masquerade as a
  test failure; `df` if tests fail weirdly. The AOT regen writes ~7.8 MB.

---

## 8. File map

| Path | Role |
|---|---|
| `design/gc-step3-open-questions.md` | **The decisions** (tag layout, Q1–Q5). Authoritative. |
| `crates/shen-rust/src/value.rs` | The enum → `Value(u64)`. The flip's center. `shen_eq` is the densest match site. |
| `crates/shen-rust/src/gc/{mod,node,heap}.rs` | The Step-2 collector. Extend `Kind` (+Float, +Closure), add alloc/accessors, add grow-only mode (§4a). |
| `crates/shen-rust/src/cons.rs` | The `ConsCell` seam — folds into the `Gc` cons (or becomes a thin wrapper). |
| `crates/shen-rust/src/aot/runtime.rs` | `make_aot_closure` (§5 rework); the three arithmetic sites (fixnum overflow). |
| `crates/klcompile/src/main.rs` | Emit the §5 shadow-capture vec; already emits through Step-1 constructors. Drives AOT regen. |
| `crates/shen-rust/src/aot/kernel/*.rs` | Generated (~7.8 MB) — regenerated by `scripts/codegen-kernel-aot.sh`. |
| `crates/shen-rust/src/{interp/eval.rs,vm/exec.rs,env.rs}` | Hold `Value`s; match-arm conversion (§4d). Roots wiring is **Step 4**, not here. |
| `crates/shen-rust/tests/vm_differential.rs` | The equivalence oracle — your correctness net. |
| `scripts/{kernel-tests,gates,kernel-aot-audit,codegen-kernel-aot}.sh` | Bench + CI + AOT regen. |

---

## 9. What you are explicitly NOT doing in Step 3 (it's Step 4)

- Registering precise roots (the §Q5 inventory) or turning collection on.
- The conservative AOT-frame stack scan + register flush
  (`gc_roots_aot_spike.rs`).
- klcompile stack-slot clearing / over-retention mitigation.
- Measuring real over-retention.

Step 3 = **representation flip, collection off, grow-only, 134/0 + oracle
green.** Hand off to Step 4 with the heap wired into `Value` and the AOT
shadow-captures in place, so Step 4 only has to register roots and flip
collection on.

**Repo conventions**: commits on `main` for perf work (user-authorized; confirm
scope before committing). `includeCoAuthoredBy: false` → **no Co-Authored-By
trailer**.
