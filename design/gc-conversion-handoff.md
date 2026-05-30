# shen-cedar GC + Word-Sized `Value` — Implementation Handoff

**Date**: 2026-05-29. **Standalone** — execute from this without the originating
conversation. **Read first**: `design/perf-state-and-gc-ladder.md` (the
authoritative state + design doc; this is its build instructions). This
supersedes the "NEXT ACTION" of `design/HANDOFF.md` (whose next action was the
GC spike — now done, see below).

---

## 0. Why you're here (one paragraph)

shen-cedar is **~5× slower** than shen-cl (SBCL) on `scripts/kernel-tests.sh`
(~5.0 s vs ~1.0 s). The gap is **distributed execution-model cost** on a
list-heavy workload (the type-checker). Reclamation/arena/refcount-removal/VM
levers are all measured dead or non-moving on the north-star (§2 of perf-state).
The one path with evidence is **tracing GC → word-sized `Value` → Cranelift
JIT**, GC first — because the heap cost is *refcount-per-clone*, and only a GC
that makes heap refs `Copy` removes it (and unlocks the JIT). **Both gating
spikes have now passed** (§1). This doc is the plan to ship the GC + word-sized
`Value` (rungs 1–2). The JIT (rung 3) is a later, separate effort.

---

## 1. What the spikes already proved (your blueprints)

Two committed, runnable spikes de-risked the whole approach. **They are your
reference implementations — port from them, don't reinvent.**

- **`benches/gc_spike.rs`** (commit `5108f6a`) — *the collector core*. Non-moving
  mark-sweep, `Copy` tagged-word handle, explicit shadow stack. On a list
  workload with real collection: **3.34× vs today's `Rc` enum, 98% of the
  no-reclaim ceiling**. Proves: mark/sweep tracing is nearly free vs the refcount
  traffic it removes. `cargo bench --bench gc_spike`.
- **`benches/gc_roots_aot_spike.rs`** (commit `7e4eb78`) — *the roots story*.
  Roots held only in native Rust frames (no shadow stack), found by **conservative
  native-stack scan + aarch64 callee-saved register flush (x19–x28)**, tag-aware.
  **Sound across 10,999 collections, 0 corruption**; finds AOT-frame roots.
  **Cost: ~7.7× over-retention** from stale pointers in popped, uncleared stack
  slots. `cargo bench --bench gc_roots_aot_spike`.

**Net**: the design is greenlit. The collector works; precise+conservative roots
work; the open *engineering* problem is the over-retention tax and the sheer
surface of the `Value` conversion.

---

## 2. Settled design decisions (don't re-litigate without new evidence)

1. **Algorithm: non-moving mark-sweep** with a `Copy` `Gc<T>` handle (perf-state
   §6c option 1). Non-moving is forced by §6b (identity constraints) AND it's
   what makes conservative AOT-frame scanning sound (a moving GC would need
   precise stack maps for AOT frames — we can't produce those without rewriting
   klcompile codegen). Two independent reasons → **non-moving**.
2. **`Value` becomes a word-sized (8-byte) `Copy` tagged word.** Low-bit tag
   scheme (the spikes use 3 bits): `000` fixnum, pointer tags for heap types,
   immediate tags for `Nil`/`Bool`/`Sym`. **`Value: Copy`** is the prize — no
   refcount on clone/drop; that's the ~2.5× on lists and the JIT prerequisite.
3. **Hybrid roots**:
   - **Precise** for what we own: the VM value stack (`vm/exec.rs` `stack:
     Vec<Value>`, frame `upvals`), `Interp` fields, partial-application vecs.
     These are already explicit Rust structures — register them as roots
     directly (cheapest, zero over-retention).
   - **Conservative** (stack scan + register flush) **only** for AOT-compiled
     native frames, which hold `Value`s as plain Rust locals (`aot/kernel/*.rs`,
     e.g. `let v_V520 = args[0].clone(); … rt::apply_direct(…)?`) that no shadow
     stack can see. Sound because non-moving.
   - **Over-retention mitigation**: emit **stack-slot clearing** from klcompile
     (zero a fn's `Value` locals on exit) to kill the stale-pointer source.
     *Measure the tax on the real heap first* — if it's small, skip; if large,
     this is the lever. Don't pay for precise stack maps unless measured-needed.
4. **Identity-constrained heap types** (`Vec`/`AbsVec`, `Stream`, `Foreign`):
   non-moving preserves their identity automatically. The collector **traces
   through `Vec`** (its cells are `Value`s and can form cycles — this is *why* we
   need tracing, not just `Rc`+cycle-collector), treats **`Foreign`/`Stream` as
   opaque leaves** (trace nothing, never move, run their Rust `Drop` when swept).
5. **Rc + cycle collector is REJECTED** (perf-state §6c) — it doesn't give `Copy`
   refs, so it wins neither the list workload nor the JIT.

---

## 3. The conversion surface (measured, current)

```
11,792  Value:: occurrences across 35 files in crates/shen-cedar/src/
    22  Value:: sites in crates/klcompile/src/main.rs  → AOT regen REQUIRED
```
Dominated by **immediates** (`Bool` 4360, `Sym` 3726, `Nil` 1719, `Int` 974) and
**match arms**, not heap construction. **`Cons` is already only 16 sites** —
because it funnels through the `ConsCell` seam (`src/cons.rs`) + `Value::cons`.
**That seam is your template**: do the same for every variant before flipping the
repr, so the flip touches one file per type, not thousands of call sites.

klcompile emits `Value::` literals, so a repr change regenerates
`aot/kernel/*.rs` (~7.8 MB). Make klcompile emit **through the constructors**
(`Value::int(..)`, `Value::sym(..)`, …) so regen is mechanical and the generated
code is repr-agnostic.

---

## 4. The plan (staged; each stage gated by the prior)

### Step 1 — Enabling refactor (low risk, do first, ~0 perf)
Funnel all `Value` construction and inspection through methods, shrinking the
literal/match surface before the repr flip. Model: `Value::cons` + `ConsCell`.
- Add constructors: `Value::int`, `::float`, `::sym`, `::bool`, `::nil`,
  `::str`, `::err` (cons exists). Add inspectors: `is_nil`, `is_cons`,
  `as_int`/`as_fixnum`, `as_sym`, `head`/`tail` (or `car`/`cdr`), etc.
- Mechanically route call sites through them (codemod + review). Match arms that
  *destructure* (`Value::Int(n) =>`) are the hard part — leave them matching the
  enum for now; the repr flip will convert these to tag-checks + accessors. The
  win in Step 1 is killing **construction** literals and ad-hoc inspection.
- Make **klcompile** emit through the constructors (the 22 sites).
- **Gate**: `scripts/kernel-tests.sh` → 134/0; `cargo test --workspace`; fmt +
  clippy; paired-A/B shows **no regression** (it's preparation, expect ~0%).

### Step 2 — Land the collector as its own module (no `Value` change yet)
Port `gc_spike.rs` into `src/gc/` as a real, tested subsystem, *not yet wired to
`Value`*:
- `Gc<T>` `Copy` handle; non-moving heap (slabs/page-table, not per-node `Box` —
  the spike used `Box` for simplicity; production wants block allocation + an
  O(1) page/block membership table for the conservative scan).
- `Heap::alloc`, `collect()` (mark from a root provider, sweep to free-list).
- A `Trace` story: how the collector enumerates outgoing edges of a heap object
  (cons → head/tail; `Vec` → cells; `Str`/`Error` → none; `Foreign`/`Stream` →
  none/opaque).
- **Gate**: unit tests + **Miri clean** on all `unsafe` (the spikes weren't
  Miri'd — do it here, this is shipped code). Bench reproduces the spike's
  ~3.34× in isolation.

### Step 3 — Flip `Value` to the word-sized tagged repr over `Gc` (THE BIG ONE)
- Rewrite `value.rs`: `Value(u64)` (or a `#[repr(transparent)]` newtype),
  `Copy`. Immediates inline; heap variants are `Gc<…>` pointers behind tags.
  Keep the *public* constructor/inspector API from Step 1 stable so call sites
  don't churn again.
- Convert the destructuring match arms (the ones Step 1 left) to tag-dispatch.
- **Bignum/overflow**: a 61-bit fixnum can't hold full `i64`. Decide and
  implement the overflow path (box a wide int, or promote) — Shen needs real
  integer semantics; today `Value::Int` is `i64`. **Resolve before flipping**
  (see §6). `Float` doesn't fit 8B with a low tag → **box `Float`** (or switch to
  NaN-boxing; low-tag + boxed float is simpler and Shen is int/list-heavy).
- Regenerate AOT (`crates/klcompile` → `aot/kernel/*.rs`); the diff should be
  large but mechanical if Step 1 funneled klcompile through constructors.
- **Gate**: 134/0; differential oracle (`tests/vm_differential.rs`) green; full
  workspace tests; Miri; fmt+clippy.

### Step 4 — Wire the hybrid roots + safepoints
- **Precise**: register the VM `stack`, frame `upvals`, `Interp` value-holding
  fields, and partial-application `Vec<Value>`s as the precise root set.
- **Conservative**: port the `gc_roots_aot_spike` scanner (stack range + aarch64
  register flush, tag-aware, page/block membership) for AOT native frames.
- **Safepoints**: collection runs only at `alloc` (the spike model). Confirm no
  `Value` is created-but-unrooted across an alloc on any path (the precise set
  must cover every live `Value` not in an AOT frame).
- **Over-retention**: measure it on the real heap. If high, add klcompile
  stack-slot clearing on AOT fn exit. Report the tax (don't let it hide).
- **Gate**: 134/0 under sustained collection; differential oracle; no AOT-path
  regression; Miri.

### Step 5 — Measure vs SBCL, then decide on the JIT
Re-take the kernel-tests number **cool** (paired A/B, min-of-N). Expected from
GC + word `Value`: **~2–3×** on the list-heavy type-checker (the spike ceilings,
discounted by real-engine overheads + the over-retention tax). Then re-profile;
pursue Cranelift (rung 3) only if a hot compute core remains. JIT notes:
Cranelift 0.132, `CallConv::Tail`/`return_call` default-on for aarch64, runtime
calls via `extern "C"` + `JITBuilder::symbol`. JIT'd code can hold the *word-
sized* `Value` in registers — that's why this conversion is its prerequisite.

---

## 5. Gates / kill-criteria / measurement discipline (non-negotiable)

- Every stage keeps: `cargo fmt --all --check`; `cargo clippy --workspace
  --all-targets -- -D warnings`; `cargo test --workspace`;
  `scripts/kernel-aot-audit.sh`; `scripts/kernel-tests.sh` → **134 passed, 0
  failed** (both engine modes); `tests/vm_differential.rs` green; **Miri clean
  on every new `unsafe`**; `scripts/gates.sh` all green.
- **Measurement** (has caught false results repeatedly): machine variance
  ~5–12% > most wins → **always paired A/B, alternate runs, min-of-N, rebuild
  both sides clean**. Profile self-samples ≠ wall-clock. A spike that breaks the
  workload gives a **void** measurement, not a fast one — assert the hard case
  actually ran (the AOT-roots spike took 3 wrong cuts before an honest number;
  each "too good" result was the tell). Re-take decision-critical absolutes cool.
- **Kill-criterion for the whole conversion**: if, after Step 4, the real
  kernel-tests number doesn't beat the current ~5.0 s by a **material margin**
  (target ≥1.5×, i.e. ≤ ~3.3 s), stop and reassess — hold the GC to the same bar
  as the dead levers. The spike ceilings predict more; if the real engine
  doesn't deliver, the over-retention tax or unrooting overhead is eating it —
  diagnose before pushing on.

---

## 6. Open questions to resolve (mostly before Step 3)

> **RESOLVED 2026-05-29 — see `design/gc-step3-open-questions.md`** (decision
> record, evidence-backed). Summary: Q1 = 61-bit fixnum + promote-to-`Float` on
> overflow (matches today's actual non-bignum semantics; no boxed-int). Q2 =
> boxed float (floats are cold; NaN-boxing rejected). Q3 = settled by Step 2
> (block=1024×24B + O(1) page table). Q4 = deferred to Step 4 (measure real
> over-retention). Q5 = precise-root inventory produced **+ a new blocker**: AOT
> `move`-closure captures inside `Rc<NativeFn>` are untraceable, so klcompile
> must emit a shadow capture `Vec<Value>` per `make_aot_closure` (expands Step 3
> scope). That doc also pins the concrete 3-bit tag layout. Original questions
> kept below for context.

- **Bignum/overflow semantics**: what does today's `Value::Int(i64)` do on
  overflow, and what must the 61-bit fixnum + boxed-wide path preserve? (Shen
  kernel expects unbounded integers in places.) **Blocking for Step 3.**
- **Float repr**: low-tag + boxed `Float` (simpler) vs NaN-boxing (float inline,
  everything else boxed). Recommend boxed float; confirm float-heavy paths aren't
  hot first.
- **Heap layout**: block/slab size + page-table granularity for the conservative
  membership test (the spike's `HashSet<addr>` won't scale).
- **Over-retention magnitude on the real heap** — measure early in Step 4; it
  decides whether klcompile stack-slot clearing is needed.
- **Where exactly are roots created-but-unrooted across an alloc?** Audit
  `interp/eval.rs` and `aot/runtime.rs` for `Value`s live across `alloc`-able
  calls that aren't on the VM stack or in an AOT frame.

---

## 7. File map (real paths)

| Path | Role in the conversion |
|---|---|
| `design/perf-state-and-gc-ladder.md` | Authoritative state + design (§4 finding, §6 GC design, §6f/§6g spike results). **Read first.** |
| `crates/shen-cedar/benches/gc_spike.rs` | Collector-core blueprint (mark-sweep, Copy word). |
| `crates/shen-cedar/benches/gc_roots_aot_spike.rs` | Roots blueprint (conservative scan + aarch64 reg flush). |
| `crates/shen-cedar/src/value.rs` | `Value` enum (12 variants) + `Closure`/`ClosureKind`/`LambdaBody` + `shen_eq`. **The repr to flip (Step 3).** Add constructors/inspectors here (Step 1). |
| `crates/shen-cedar/src/cons.rs` | `ConsCell` seam — the **template** for funneling each variant. |
| `crates/shen-cedar/src/gc/` (new) | The collector module (Step 2). |
| `crates/shen-cedar/src/vm/exec.rs` | VM value stack (`stack: Vec<Value>`) + frame `upvals` — the **precise root set** (Step 4). |
| `crates/shen-cedar/src/interp/eval.rs` | Tree-walker + `Interp` (holds `Value`s → precise roots; audit for unrooted-across-alloc). |
| `crates/shen-cedar/src/aot/runtime.rs` | AOT runtime (`apply_direct`, prims). AOT frames here need conservative scanning. |
| `crates/shen-cedar/src/aot/kernel/*.rs` | Generated (~7.8 MB); regenerated on the repr flip. |
| `crates/klcompile/src/main.rs` | KL→Rust AOT compiler — emits `Value::` (22 sites); route through constructors (Step 1); add stack-slot clearing (Step 4 if needed). |
| `crates/shen-cedar/tests/vm_differential.rs` | VM↔tree-walker equivalence oracle — the correctness net throughout. |
| `scripts/kernel-tests.sh`, `scripts/gates.sh`, `scripts/kernel-aot-audit.sh` | Benchmark + CI gates. |

---

## 8. Sequencing recap

```
Step 1 enabling refactor (≈0%)  →  Step 2 collector module (Miri)  →
Step 3 flip Value to word+GC (AOT regen)  →  Step 4 hybrid roots + safepoints  →
Step 5 measure vs SBCL  →  GATE (≥1.5× or reassess)  →  (later) Cranelift JIT
```

**Fallback** (if Step 2/3 prove too costly or the real number won't move): ship
**fixnums-only** — inline-tag `Int`/`Bool`/`Nil`/`Sym`, keep heap types `Rc`
behind the tag. Banks the lifetime-independent **~1.6×** on arithmetic-heavy
paths, no GC, incremental. Caps ~3–4× off SBCL but de-risked and real.

**Repo conventions**: commits go on `main` for perf work (user-authorized;
confirm scope); `includeCoAuthoredBy: false` → **no Co-Authored-By trailer**.
Disk hit 99% mid-session once — a "no space left" error can masquerade as a test
failure; check `df` if tests fail weirdly.
