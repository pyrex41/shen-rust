# shen-rust Performance: Current State & the GC Ladder to SBCL-Level

**Status**: Authoritative current-state doc (2026-05-29). Supersedes the
premise of `execution-engine-roadmap.md` — that doc assumed the bytecode VM
was the lever for kernel-tests; measurement since has shown otherwise (see §3).
**2026-06-10 update: the ladder is COMPLETE.** Steps 1–3 shipped earlier
(word `Copy` Value over the Step-2 collector, grow-only); **Step 4 (collection
ON) shipped 2026-06-10** as opt-in request-mode collection at interpreter
depth-0 safepoints with the §6g hybrid roots — see `PERFORMANCE.md` ("GC
Step 4") and `benches/gc_boundedness.rs` (482 MB → 26 MB flat, wall-neutral).
The JIT rung was built and falsified for the type-checker shape (J2) and
parked; the served niche is covered by the AOT overlay instead.
**Audience**: anyone continuing the perf effort.
**Companion**: the running measured log lives in auto-memory
(`project_stage_d_finding.md`); this doc is the durable, repo-side synthesis.

---

## 1. The goal and the honest gap

Close the gap vs **shen-cl (SBCL)** on `scripts/kernel-tests.sh`. SBCL runs it
in **~1.0 s**; shen-rust runs it in **~5.0–5.5 s** warm (machine variance
±5–12%; absolute numbers drift with thermal state). So **~5× off**.

This doc records what is *measured*, what is *shipped*, what is *dead*, and the
one path with evidence behind it: **a tracing GC → word-sized `Value` →
Cranelift JIT**, in that order. The GC is the first domino. §5 is the honest
cost.

---

## 2. What is shipped (on `main`, this session)

All paired-A/B measured, "keep only if it beats noise", 134/0 kernel-tests both
engine modes, fmt+clippy+tests green.

| Commit | Change | Measured |
|---|---|---|
| `02f5de9` | VM A1+A2: single value-stack + in-VM call frames + real cross-fn `TailCall`; A3 wires freeze/lambda through the VM (opt-in); A5 differential oracle | VM ~2.7–4× faster than tree-walker **on pure user code** |
| `e09ab60` | Cache `vm_enabled()` env lookup (killed a `getenv` per closure-creation that A3 introduced on the default path) | **+5.9%** kernel-tests |
| `2b9e226` | A4: over-capture closure scope instead of re-running `collect_used_syms` free-var walk per closure creation | **+6.0%** kernel-tests |
| `58bfff5` | Cache compiled lambda/freeze closures (was recompiling ~1.2M times over ~655 distinct bodies; no cache) | removes recompile penalty; VM-mode now ≈ tree-walker on kernel-tests (precise delta pending a cool-machine measure) |

**Cumulative kernel-tests improvement this session: ~+8.9%** (session-start
`02f5de9` 5.51s → 5.01s, 10 alternating pairs). The VM remains **opt-in**
(`SHEN_RUST_VM=1`): on the AOT-dominated kernel-tests, runtime-compiling the
minority of dynamic closures doesn't beat the tree-walker; the VM's big win is
for user-defined Shen code run via `eval`.

---

## 3. What is DEAD (measured, do not re-open without new evidence)

| Hypothesis | Predicted | Measured | Method |
|---|---|---|---|
| Cons recycling pool | large | ~2.5% | A/B CAP=0 vs 64K |
| Remove per-call `Rc::clone` on dispatch | some | ~0% | before/after |
| Eliminate **all** cons churn (leaked bump arena = GC reclamation ceiling) | ~32% | **~2.4%** | paired A/B, 6 pairs |
| TW1: remove per-step `Rc<[KlExpr]>` clone in `eval_in` (`mem::replace`) | some | within noise (−0.3%) | 10 alternating pairs |
| TW3: faster `lookup_local` | some | no clean/safe win; clone is load-bearing, scan already short (avg frame 12.7, 0 dups) | instrumented + analysis |

**The decisive one is the arena spike**: deleting *all* cons reclamation work
moved wall-clock ~2.4%. So **GC/arena *reclamation* is not the lever**, and no
`Value`-representation change motivated by *churn* will help. This was long read
as "GC won't help" — §4 shows that conclusion was too broad.

Root cause of the 5× (by elimination + profiling): **distributed
execution-model cost** — every interpreted, dynamically-dispatched, boxed
operation costs ~2–5× its SBCL native+GC equivalent, across millions of ops.
SBCL native-compiles every loaded `define` incl. runtime `freeze`/`lambda`
continuations and runs them on GC'd values; we tree-walk them over `Rc<Value>`.

Fresh leaf profile (2026-05-29, default kernel-tests, ignore `__ulock_wait` =
idle main thread): tree-walker cluster dominates — `eval_in` 360, `lookup_local`
189, `collect_used_syms` 148, `eval_args` 144, `tail_apply` 97 ≈ **940**; value
churn `drop_in_place<Value>` 551 + malloc/free ≈ **850** (the dead lever); AOT
kernel leaves only ≈ **250**. The type-checker proves theorems about *loaded
`.shen` files*, which run tree-walked — so the engine model, not the AOT kernel,
is the cost.

---

## 4. The GC-first finding (the new evidence that reframes everything)

The Cranelift research (crates 0.132) established: a JIT only pays off if
`Value` is **word-sized**, because JIT'd code cannot hold/operate on the current
24-byte `Rc`-laden enum without an FFI call into Rust per operation. So a JIT is
gated on a value-representation change.

We then spiked the representation directly (`benches/value_repr_spike.rs`),
three reps, identical workloads, min-of-N paired:

| Workload | Boxed (today, 24B Rc enum) | Option A: tagged 8B word **over Rc** | Option B: tagged 8B word **+ GC** (Copy heap refs) |
|---|---|---|---|
| W1 tight arithmetic (no heap) | 1.00× | **1.64×** | 1.64× |
| W2 cons list build+sum (heap) | 1.00× | **1.00× — nothing** | **2.48×** |

The pivotal result: **Option A buys nothing on list-heavy code.** A tagged word
that points into `Rc` memory must still refcount on every clone/drop (mandatory
for safety without a GC) — and *the refcount traffic is the heap cost, not the
box size*. The win only appears under Option B, where heap references are `Copy`
(no per-op refcount) — which requires a garbage collector.

This **refines, not contradicts, the dead arena result.** A GC has two
properties that were conflated under "GC won't help":
- **Reclamation** (freeing dead objects) — confirmed *not* a lever (~2.4%).
- **`Copy` heap references** (no refcount on clone/drop) — **worth ~2.5× on
  list workloads, and is the JIT prerequisite.** Never isolated before.

Shen — and the type-checker specifically — is **list-heavy**. So:

> **Approaching SBCL requires a GC.** Not for reclamation speed, but because it
> makes heap `Value` references `Copy`, which (a) removes refcount-per-op on the
> dominant list workload and (b) lets a JIT manipulate values in registers.

The W1 fixnum win (~1.64×) is **lifetime-independent** — immediates never touch
the heap — so it is available even without a GC (see §6, fallback).

---

## 5. The ladder (with honest cost)

```
   tracing GC (heap Value refs become Copy)
        │   ← the first domino; large subsystem; ~2.5× on list code
        ▼
   word-sized tagged Value (8B Copy word)
        │   ← +1.6× arithmetic; mechanical but wide (see §6 surface)
        ▼
   Cranelift JIT of hot bytecode functions
        │   ← native code calling native code, no FFI per op
        ▼
   approach ~1.5–2× of SBCL (literal 1.0× parity needs more)
```

Each rung gates the next and must be measured before the next is funded. LLVM
and Wasm remain off the path (see `execution-engine-roadmap.md` Part C; verdict
unchanged).

---

## 6. GC design decisions (to settle before any code)

This is the hard, TCB-growing subsystem. Decisions, with the constraints the
codebase imposes:

### 6a. Conversion surface (measured)
- **8,395 `Value::` occurrences across 35 files** in `crates/shen-rust/src/`.
- **22 `Value::` sites in `crates/klcompile/src/main.rs`** → the AOT codegen
  emits `Value::` literals, so **AOT regen is required** and the generated
  `aot/kernel/*.rs` (~12 MB) changes. The `ConsCell` seam (`cons.rs`) was built
  precisely to localize cons changes, but a word-sized `Value` is wider than the
  cons cell — it touches every variant.
- Funnel as much as possible through constructor/accessor methods first
  (`Value::cons`, `Value::int`, head/tl helpers) to shrink the literal-match
  surface before the repr flip — an enabling refactor, low risk, do it first.

### 6b. Constraints from the value types
- **`AbsVec = Rc<RefCell<Vec<Value>>>` and `Stream`**: interior-mutable, can
  form **cycles**, and need **stable identity** (mutation must be visible
  through all references). This **rules out a naive copying/compacting GC for
  these** (or requires pinning/handles for them), and rules out "just add cycle
  collection to Rc" as sufficient on its own if we also want Copy refs.
- **`Foreign(Rc<dyn Any>)`** (Cedar handles): opaque host objects. The GC must
  treat them as **opaque roots / leaves** — trace through nothing, never move,
  drop via their Rust `Drop` when unreachable.
- **`Str`/`Error` (`Rc<str>`)**: immutable, no outgoing pointers — easy.
- **`Cons`**: the hot, high-volume, immutable pair — the case the GC most needs
  to make `Copy` and cheap.

### 6c. Algorithm candidates
1. **Mark-sweep, non-moving, with a `Gc<T>` Copy handle** (pointer into a
   GC-owned heap; values are `Copy` words; collector marks from roots and frees
   unmarked). Pros: non-moving → `AbsVec`/`Foreign` identity preserved; `Copy`
   refs achieved; conceptually closest to what we need. Cons: precise root
   enumeration in a Rust interpreter is the hard part (the Rust stack holds
   `Value`s; need a shadow stack or conservative scanning).
2. **Copying/generational** (SBCL-like, best raw throughput). Cons: moving
   breaks `AbsVec`/`Foreign`/`Stream` identity unless those are kept off-heap
   behind handles; most invasive; precise roots still required.
3. **Rc + cycle collector** (keep `Rc`, add a cycle detector). Cons: does **not**
   give `Copy` refs — still refcounts per clone, so per §4 it does **not** win
   the list workload or enable the JIT. **Rejected for this goal** (it only
   solves leaks, which we don't have).
4. **Region/arena per evaluation** (bump-allocate transient terms, free the
   region at well-defined points). Pros: simplest, no tracing, `Copy` refs
   within a region. Cons: needs evaluation-scoped lifetime structure; the
   type-checker's continuations may outlive naive regions; correctness risk.

**Leaning**: option 1 (non-moving mark-sweep with `Copy` handles) is the best
fit for the identity constraints in §6b, with a **shadow stack** for precise
roots. But this is a decision to validate with a *focused GC spike* (see §7),
not to lock in here.

### 6d. The precise-roots problem (the real difficulty) — PROTOTYPED, see §6g
A tracing GC must find all live `Value`s. In a Rust tree-walker/VM they live in
Rust locals, the VM value-stack, `Interp` fields, and AOT Rust stack frames. A
conservative stack scan is fragile across Rust's unspecified layout; a precise
**shadow stack** (the VM already has an explicit value-stack — a natural root
set) is cleaner but the **AOT kernel** holds `Value`s in native Rust frames that
a shadow stack doesn't see. This interaction — **GC roots inside AOT-compiled
code** — is the single biggest design risk and must be prototyped early.
**→ Prototyped 2026-05-29 (§6g): conservative native-stack scan is SOUND for a
non-moving collector and finds AOT-frame roots, but over-retains (~7.7×).**

### 6e. Kill-criteria
- A focused GC spike (§7) must show the **Copy-ref list win survives a real
  collector** (i.e. reproduce ~2× of the Option-B ceiling on a list workload
  *with* actual collection happening), or stop. **→ PASSED, see §6f.**
- Full integration must keep **134/0 kernel-tests**, the differential oracle
  green, Miri clean on all new `unsafe`, and not regress the AOT path.

### 6f. GC spike RESULT (2026-05-29) — kill-criterion PASSED
`benches/gc_spike.rs`: non-moving **mark-sweep** with a `Copy` tagged-word
handle + an explicit **shadow stack** for precise roots, on a list workload with
a realistic live working set (`WINDOW=4` retained lists) that forces real
collection. 12 paired min-of-N runs, correctness-asserted (sums verified — a
corrupt list aborts as a *void* measurement, not a fast one):

| Rep | min | vs boxed |
|---|---|---|
| boxed (24B `Rc` enum, reclaim) | 138.7 ms | 1.00× |
| **GC (mark-sweep, `Copy` word)** | **41.6 ms** | **3.34×** |
| leaked ceiling (Option B, no reclaim) | 40.9 ms | 3.39× |

- **The GC retains ~98% of the no-reclaim ceiling** — i.e. mark/sweep tracing is
  nearly free relative to the refcount traffic it removes. Collection ran 498×
  with peak-live ≈ 9000 nodes (heap **bounded** near `cap`, so reclamation is
  real, not a disguised leak — asserted).
- Confirms the §4 thesis at the collector level: **refcount-per-clone *is* the
  heap cost**; making heap refs `Copy` (which needs a GC) recovers ~all of it.
- **First cut was a false 7.26×**: `cap = 4*n` aligned every collection to a
  list boundary (empty live set → trivial mark), and there was no correctness
  assert. Fixed by the live `WINDOW` + un-aligned `cap` + sum assertions +
  `peak_live ≥ n` guard. (Logged because "too good" is the tell, per §8.)
- **Caveat (addressed next, §6g)**: this models collector + representation
  throughput and a shadow-stack roots story. It does **not** model GC roots in
  **AOT-compiled Rust frames** (§6d) — addressed by the §6g spike.

### 6g. AOT-frame roots spike RESULT (2026-05-29) — §6d feasibility CONFIRMED, with a cost
`benches/gc_roots_aot_spike.rs`: the §6d question — can the GC find roots that
live in **AOT-compiled Rust stack frames** (which a shadow stack can't see)?
Model: `aot_frame` stands in for an AOT kernel fn — holds a list head in a plain
Rust local across a GC-triggering recursive call, then re-uses it (so the
compiler keeps it live across the call, exactly like the generated
`v_V520`/`v_W521` in `aot/kernel/core.rs`). **No shadow stack** — the collector's
*only* root source is a **conservative scan of the native stack + an aarch64
callee-saved register flush (x19–x28)**, tag-aware (every node ref is a `Word`
tagged `CONS`). 25 keeper frames live during deep collections, interleaved
garbage to force mid-descent GC.

**Result (asserted; a missed root corrupts a list → wrong sum → void):**
- **SOUND / feasible: YES.** Correctness held across **10,999 collections**; max
  roots found in one collection = 50 (≥ the 25 live keeper frames) — the scan
  genuinely discovers heads held *only* in native frames. Non-moving ⇒ a
  false-positive root can only **over-retain, never corrupt**. So a non-moving
  mark-sweep **can tolerate AOT frames it cannot precisely enumerate**. The §6d
  risk is de-risked: **we are not blocked.**
- **COST: conservative scanning OVER-RETAINS (~7.7× here).** Cause: a returned
  `make_garbage` frame leaves a stale list-head pointer in its popped, uncleared
  stack slot; the full-range scan finds it and retains the whole dead chain.
  **Bounded** (≈ one descent's worth, independent of iteration count — *not* a
  leak: heap 56k vs 22M-if-leaked), but a real footprint tax.
- **Design implication**: favors a **hybrid** — a *precise* shadow stack for the
  VM/interpreter value-stack (which we own), conservative scan *only* for AOT
  native frames, and/or compiler-emitted **stack-slot clearing on AOT fn exit**
  to kill the stale-pointer source. Measure the tax on the *real* heap before
  deciding whether precise stack maps (heavier) are warranted. Also: this is a
  second independent argument **against a moving GC** (moving needs precise maps
  for these AOT frames — can't conservatively scan if pointers move).
- **Production caveats**: register flush is arch-specific (aarch64 here; need
  setjmp / per-arch spills elsewhere, or restrict the GC build); heap-membership
  test must be a page/block table, not a `HashSet`, at scale; **no interior
  pointers** (every ref is a tagged head-of-node `Word`) — this is what makes the
  tag-aware scan sound and must remain invariant.

---

## 7. Sequencing

1. **Enabling refactor** (low risk): route all `Value` construction/inspection
   through methods, shrinking the 8,395-site literal surface. Measure: zero
   change expected; it's preparation.
2. **Focused GC spike** (de-risk before committing): a standalone prototype of
   option-1 mark-sweep with `Copy` handles + a shadow stack, on a list workload
   that actually triggers collection. **Gate**: reproduce a material fraction of
   the 2.48× Option-B ceiling *with real collection*, and demonstrate a workable
   precise-roots story including the AOT-frame interaction (§6d).
   **→ DONE 2026-05-29: BOTH halves PASSED. Throughput (§6f): 3.34×, 98% of
   ceiling. AOT-frame precise-roots (§6g): conservative scan is sound + finds
   AOT roots (10,999 collections, 0 corruption), with a ~7.7× over-retention
   tax that argues for a hybrid (precise shadow stack + conservative AOT scan +
   stack-slot clearing). The GC ladder is GREENLIT to design; step 3 can begin
   once the enabling refactor (step 1) lands.**
3. **Full `Value` → word-sized + GC** conversion (the big one): AOT regen, all
   gates, Miri. Design the collector per §6c option-1 (non-moving mark-sweep) +
   the §6g hybrid roots story; quantify the over-retention tax on the real heap
   early and add stack-slot clearing to klcompile's AOT codegen if it's high.
4. **Re-measure** vs SBCL. Then word-size-dependent VM tuning.
5. **Cranelift JIT** (only if 1–4 land and a hot compute core remains).

### Fallback (if the GC spike fails or is too costly)
Ship the **fixnums-only** win: inline-tag the immediates (`Int`/`Bool`/`Nil`/
`Sym`) so arithmetic-heavy paths get the lifetime-independent ~1.6×, leaving
heap types `Rc`-boxed behind the tag. No GC, incremental, but caps well short of
parity (~3–4× off SBCL). This banks real value if the GC proves infeasible.

---

## 8. Measurement discipline (non-negotiable, learned repeatedly)
- Machine variance ~5–12% > most single wins. **Always paired A/B**, alternate
  runs, report **min**-of-N (least-noisy), rebuild **both** sides clean (a stale
  `/tmp` binary once produced a false −2.3% for TW1).
- **Profile self-samples ≠ wall-clock** (the arena lesson; also TW1).
- A spike that breaks the workload gives a **void** measurement, not a fast one
  (a `lookup_local` returning Nil aborted the suite in 0.01s — discard, don't
  celebrate).
- Thermals: today's GC-decision spikes ran on a hot machine (abs 2× the cool
  baseline); the **paired ratios** are what's trusted, not absolutes. Re-take
  decision-critical absolute numbers cool.
