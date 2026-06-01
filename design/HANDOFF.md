# shen-rust Performance — Handoff (2026-05-29)

> **SUPERSEDED for next action (2026-05-29 later):** both GC gating spikes have
> since PASSED (throughput 3.34× + AOT-frame roots sound — see
> `perf-state-and-gc-ladder.md` §6f/§6g). The implementation plan to actually
> ship the GC + word-sized `Value` now lives in
> **`design/gc-conversion-handoff.md`** — start there. The §-below "NEXT ACTION"
> (do the spike) is DONE. This doc remains accurate for shipped state + context.

Standalone. Pick up the perf effort without the originating conversation.
**Read `design/perf-state-and-gc-ladder.md` first** — it's the authoritative
current-state doc; this is the action-oriented companion.

---

## TL;DR

- Goal: close the ~5× gap vs shen-cl (SBCL) on `scripts/kernel-tests.sh`
  (SBCL ~1.0 s, shen-rust ~5.0–5.5 s warm).
- This session shipped **~+8.9%** on kernel-tests (5 commits, all on `main`,
  all gates green) and — more importantly — **measured the actual path to
  SBCL-level**: `tracing GC → word-sized Value → Cranelift JIT`, with the GC
  as the **first** rung.
- **Pivotal finding**: a word-sized (tagged) `Value` over `Rc` gives **nothing**
  on list-heavy code (the refcount on clone/drop is the cost, not box size);
  the win (~2.5× on lists) needs a GC that makes heap refs `Copy`. The fixnum
  win (~1.6×) is the exception — lifetime-independent.
- **Next concrete action**: an *enabling refactor* (shrink the `Value::`
  literal surface) then a **focused GC spike** that must hit a kill-criterion
  before any full conversion. Details in §4.

---

## State of the repo

- Branch: `main`. Working tree clean. (Branch `perf/vm-execution-model` exists,
  already merged ff into main — ignore/delete at will.)
- All work committed. Repo has `includeCoAuthoredBy: false` → **no
  Co-Authored-By trailer** on commits.
- **Commit convention learned this session**: the user authorizes commits/merges
  for perf work, but confirm scope; don't invent timelines.

Commits this session (newest first):
```
b7afccf docs: authoritative perf-state + GC-ladder design doc
bfbac84 bench: extend value-repr spike with honest Rc-tagged rep (Option A vs B)
62c4642 bench: value-repr spike — word-sized tagged Value vs 24-byte Rc enum
58bfff5 perf(vm): cache compiled lambda/freeze closures (kill ~1.2M recompiles)
2b9e226 perf(eval): over-capture closure scope instead of free-var walk (A4)
e09ab60 perf(eval): cache vm_enabled() env lookup (kill getenv on closure-creation)
02f5de9 perf(vm): single value-stack + in-VM call frames; wire freeze/lambda
```

---

## How to build / test / measure

```sh
cargo build --release
cargo test --workspace                              # 107 tests, must stay green
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
./target/release/shen-rust --kernel-tests          # must print "passed: 134, failed: 0"
SHEN_RUST_VM=1 ./target/release/shen-rust --kernel-tests   # VM mode: also 134/0

# Microbenches (harness=false, no external deps):
cargo bench --bench vm_vs_treewalk      # VM vs tree-walker on pure user code
cargo bench --bench value_repr_spike    # tagged Value vs Rc enum; Option A vs B
```

**Measurement discipline (non-negotiable — has caught false results twice):**
- Machine variance ~5–12% > most single wins. Always **paired A/B**, alternate
  runs, report **min**-of-N. **Rebuild BOTH sides clean** (a stale `/tmp` binary
  once gave a false −2.3%).
- Profile self-samples ≠ wall-clock (the arena lesson).
- Profiling: `./target/release/shen-rust --kernel-tests & ; /usr/bin/sample
  <pid> 6 -file /tmp/p.txt`. Use the **"Sort by top of stack"** (leaf) section,
  near the end of the file. **Ignore `__ulock_wait`** — it's the idle main
  thread joining the worker, not work.
- A spike that breaks the workload (aborts early) gives a **void** measurement,
  not a fast one. Sanity-check the run completed (`passed: 134`).
- Re-take decision-critical absolute numbers on a **cool** machine; today's were
  thermally inflated (abs ~2× cool baseline), so only paired *ratios* are trusted.

---

## What's shipped (and why VM stays opt-in)

The bytecode VM (`crates/shen-rust/src/vm/`) was rebuilt this session: A1+A2
gave it a single shared value-stack + in-VM call frames + real cross-function
`TailCall` (commit `02f5de9`); it now beats the tree-walker **2.7–4× on pure
user code** (`cargo bench --bench vm_vs_treewalk`). A3 wires `lambda`/`freeze`
through it; A5 added a differential oracle (`tests/vm_differential.rs`).

**But the VM is gated behind `SHEN_RUST_VM=1`** and should stay there until the
GC ladder lands. Reason (measured): `--kernel-tests` is dominated by the
**tree-walker** running the type-checker over *loaded `.shen` files* — and the
type-checker's hot continuations are AOT-native closures (`make_aot_closure`,
built by klcompile at build time) that bounce back into the tree-walker. The VM
compiles the runtime closures but can't stay "in-VM" across that boundary, so on
kernel-tests it ≈ tree-walker (was a ~7% regression before the closure-compile
cache `58bfff5` removed 1.2M redundant recompiles).

The three small wins (`e09ab60` getenv-cache +5.9%, `2b9e226` A4 over-capture
+6.0%, `58bfff5` closure-cache) are tree-walker / default-path wins → they ship
for real.

---

## What's DEAD (don't re-open without new evidence)

| Hypothesis | Measured | 
|---|---|
| Cons recycling pool | ~2.5% |
| Remove per-call `Rc::clone` on dispatch | ~0% |
| Eliminate ALL cons churn (leaked arena = GC *reclamation* ceiling) | ~2.4% |
| TW1: remove per-step `Rc<[KlExpr]>` clone in `eval_in` | within noise |
| TW3: faster `lookup_local` | no clean/safe win (clone load-bearing, scan short) |

**Crucial nuance** (don't let the dead arena result over-generalize): GC
*reclamation* is dead (~2.4%), but GC *Copy-heap-references* (no refcount per
clone/drop) is a **live ~2.5× lever** — see next section. Two different things.

---

## The finding that sets the direction (§4 of perf-state doc)

Cranelift research + the value-repr spikes (`benches/value_repr_spike.rs`)
established, min-of-N paired:

| Workload | Boxed (today, 24B `Rc` enum) | Tagged 8B over `Rc` (no GC) | Tagged 8B + GC (Copy refs) |
|---|---|---|---|
| W1 arithmetic (no heap) | 1.00× | **1.64×** | 1.64× |
| W2 cons list (heap) | 1.00× | **1.00× — nothing** | **2.48×** |

- The current `Value` is **24 bytes, align 8, non-`Copy`, `Rc`-laden**. A
  Cranelift JIT can't operate on it without an FFI call per op → JIT needs a
  word-sized `Value` first.
- A word-sized `Value` **over `Rc`** still refcounts on clone/drop (mandatory
  for safety) → erases the list win. **The refcount is the heap cost.**
- Only a **GC** (heap refs become `Copy`) wins the dominant list workload AND
  enables the JIT. Hence the ladder, GC first.

---

## NEXT ACTION (concrete, in order)

### Step 1 — Enabling refactor (low risk, do first)
Funnel `Value` construction/inspection through methods to shrink the literal
surface before any repr change. **8,395 `Value::` occurrences across 35 files**
in `crates/shen-rust/src/`, plus 22 in `crates/klcompile/src/main.rs` (so a
repr change forces **AOT regen** of `aot/kernel/*.rs`). Add/route through
`Value::int`, `Value::cons` (exists), head/tl/is_* helpers, etc. Expect **zero**
perf change — it's preparation; measure to confirm no regression. Keep 134/0.

### Step 2 — Focused GC spike (de-risk; HARD part; do NOT skip to full conversion)
Standalone prototype: **non-moving mark-sweep with a `Copy` `Gc<T>` handle +
a shadow stack** for precise roots, on a list workload that actually triggers
collection.
- **Kill-criterion**: reproduce a material fraction of the **2.48×** Option-B
  ceiling *with real collection happening* (not leaked), OR stop.
- **Biggest risk to prototype early**: precise GC roots when `Value`s live in
  **AOT-compiled Rust stack frames** (not just the VM value-stack). A shadow
  stack sees the VM; it doesn't see AOT frames. This GC-roots-in-AOT interaction
  is the single largest design risk (perf-state doc §6d).
- **Constraints** (§6b): `AbsVec`/`Stream` are `RefCell` interior-mutable, can
  cycle, need stable identity → non-moving (no copying GC for those, or pin
  them). `Foreign(Rc<dyn Any>)` Cedar handles = opaque roots, never trace/move.

### Step 3 — Full `Value` → word-sized + GC conversion
Only if Step 2 hits its kill-criterion. AOT regen, all gates, Miri on all new
`unsafe`. 134/0 + differential oracle green.

### Step 4 — Re-measure vs SBCL, then Cranelift JIT
Cranelift 0.132 (crates: `cranelift-jit/module/codegen/frontend/native`). Tail
calls (`CallConv::Tail` + `return_call`) are default-on for aarch64. JIT'd code
calls Rust runtime via `extern "C"` + `JITBuilder::symbol`. Only pursue if 1–3
land and a hot compute core remains.

### Fallback (if the GC spike fails or is too costly)
Ship **fixnums-only**: inline-tag `Int`/`Bool`/`Nil`/`Sym`, heap types stay
`Rc` behind the tag. Banks the lifetime-independent **~1.6×** on
arithmetic-heavy paths, no GC, incremental. Caps ~3–4× off SBCL.

---

## File map

| Path | Role |
|---|---|
| `design/perf-state-and-gc-ladder.md` | **Authoritative state + GC design.** Read first. |
| `design/HANDOFF.md` | This file. |
| `crates/shen-rust/src/interp/eval.rs` | Tree-walker (dominant engine). `eval_in`, `lookup_local`, `build_closure`/`try_compile_closure` (closure cache + over-capture), `vm_enabled` (cached). |
| `crates/shen-rust/src/vm/{exec,compiler,opcode,bytecode}.rs` | Bytecode VM (A1/A2 done; opt-in). |
| `crates/shen-rust/src/value.rs` | `Value` (24B), `ClosureKind`. The repr to convert. |
| `crates/shen-rust/src/cons.rs` | `ConsCell` seam. |
| `crates/shen-rust/src/aot/{runtime,kernel/*}.rs` | AOT runtime + generated kernel (~12 MB; regen on Value change). |
| `crates/klcompile/src/main.rs` | Build-time KL→Rust AOT compiler (emits `Value::` literals; `compile_lambda`/`compile_freeze` lower closures to Rust). |
| `crates/shen-rust/benches/vm_vs_treewalk.rs` | VM vs tree-walker microbench. |
| `crates/shen-rust/benches/value_repr_spike.rs` | Tagged-Value spike (Boxed vs tagged_rc vs tagged_leak). |
| `crates/shen-rust/tests/vm_differential.rs` | VM↔tree-walker equivalence oracle. |
| `scripts/kernel-tests.sh`, `scripts/gates.sh` | The benchmark/correctness suite + CI gates. |

---

## Gotchas observed this session
- **Disk**: machine hit 99% full mid-session; freed ~54G (Docker prune, Rust
  `target/` dirs, caches). A "no space left on device" error can masquerade as a
  test failure — check `df` if tests fail weirdly.
- **Bash backgrounding**: long `cargo` invocations auto-background in this
  harness; read the task output file rather than chaining `sleep`.
- **`/tmp` binary staleness**: always rebuild both A/B sides from clean git
  state; don't reuse an old `/tmp` copy.
