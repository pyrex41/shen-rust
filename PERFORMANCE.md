# shen-rust performance

## Current state

On the full `--kernel-tests` suite, against the reference `shen-cl` (SBCL)
interpreter, `shen-rust` is **~3.0× slower** (≈3.0s vs ≈1.0s wall, paired
interleaved min-of-5, 2026-06-10) — down from **~17×** at first conformance.
With a warm tc-cache (verdict memoization, off by default) it runs **at
parity**. Measure head-to-head with `scripts/cross-port-bench.sh`
(interleaved; the machine has ~5–12% thermal variance, so trust min-of-N,
not single runs).

The bytecode VM is **~2.3× faster than the tree-walker on warm / served
workloads** (`scripts/warm-bench.sh`), which is why it ships behind `--served`
rather than as the bare default — see "the warm/served decision" below.

The living, detailed record is in `design/`:

- `design/perf-state-and-gc-ladder.md` — the scoreboard + the GC/Value/JIT ladder.
- `design/perf-next-target-handoff.md` — the current next-target analysis (incl.
  §3b, the warm/served decision and the VM 2.3× result).
- `design/jit-productionization-plan.md` — the Cranelift JIT, and its §5
  falsification for type-checker closures.

## How the gap was closed (17× → ~3.0×)

1. **Tree-walker / dispatch surgery** (17.5s → ~5.7s): locals-by-reference +
   scope stack (killed the quadratic per-arg `locals.clone()`), Vec-indexed
   function/global tables (no per-call hashing), single-allocation cons, a
   no-alloc dispatch fast path, FNV + pointer-keyed interning, `SmallVec` arg
   vectors, a direct AOT fn-pointer table, and `opt-level = 2`.
2. **Native hot-fn overrides** for the upstream call-frequency leaders
   (`element?`, `shen.pvar?`, `shen.lazyderef`, `fail`, …) and a `Rc::ptr_eq`
   equality fast path.
3. **Value representation**: `enum Value` (24 B, `Rc` everywhere) → word-sized
   `struct Value(u64)` tagged, with a tracing GC heap behind it (collection
   built + validated, currently grow-only).
4. **Runtime-overhead strip, 2026-06-10** (~18% cumulative, 3.3× → 3.0×):
   release profile to `opt-level=3` + thin LTO + one codegen unit (~5%); the
   **split-TLS heap** — the thread-local `RefCell<Heap>` was a *destructor
   key* paying a dtor-state check plus borrow flags on every `Value` heap op,
   replaced by a no-`Drop` `Cell<*mut Heap>` fast path that compiles to a
   bare TLS load (~8%, the whole profiled TLS tax; adversarially reviewed,
   miri-clean, debug-sentinel tripwire = Gate 8); and a **direct-mapped
   intern cache** for AOT call-target resolution, replacing a per-call FNV
   HashMap probe (~5.5%). One falsified candidate from the same profile:
   filtered closure-capture caching measured −3.5% — the whole-scope memcpy
   in `capture_used` beats per-creation lookups even with the free-var walk
   amortized.

## Why the remaining ~3.0× is structural

Two execution-engine bets — the bytecode VM and the Cranelift closure-JIT —
were both built, validated 134/0, and measured A/B on the one-shot
`--kernel-tests` metric. Both are **non-winners there** (VM neutral/slightly
slower; JIT −15%). The reason is the cost is the **distributed boxed-`Value` +
interpreted-dispatch model itself**, not the per-body dispatch mechanism:
re-encoding how one body runs doesn't change the millions of boxed-value ops,
and a one-shot metric never amortizes runtime compilation (SBCL pre-compiles
ahead of time and pays neither). With the runtime's own overheads now stripped
(item 4 above), the 2026-06-10 profile shows the remaining time is the model
itself: ~21% interpreter dispatch (`eval_in`), ~17% call plumbing, ~14%
allocator churn from arg/closure temporaries — no single hot spot, each
remaining local lever ≤ ~8%. (cons recycling, GC reclamation, `Rc::clone`
removal, faster `lookup_local`, cons-churn elimination, filtered capture
caching — all measured dead.)

## The warm/served decision

The VM *does* win **~2.3×** on a warm / served workload (load a theory once,
serve many type-check / eval requests), where its per-closure compile cost
amortizes — measured paired/interleaved in `scripts/warm-bench.sh`, with the
type-checker's continuations 98.9% VM-served. So the VM ships behind the
`--served` entrypoint (`SHEN_RUST_VM=1`) for long-running embeddings, while the
bare default stays the tree-walker to protect the one-shot cross-port ratio.

## The AOT overlay (loaded code, served shape)

AOT-native-compiling *loaded* user code — the SBCL-shaped answer — **shipped
2026-06-09 for the served niche**, as an opt-in overlay: known `.shen` files
are compiled offline (`scripts/codegen-shen-aot.sh`, the same klcompile that
AOTs the kernel) and, after a normal load (all side effects live), swapped
over the loaded defuns through a verified manifest (source hash + kernel
digest + arity precheck; any mismatch silently falls back to the loaded
engine). Measured on the served authz workload (`benches/authz_served.rs`):
**3.0–3.2× over the VM-loaded arm** (kill-gate was ≥1.5×), 11.4–11.8× over
tree-walk, shen_eq-identical results. Redefinition coherence is guaranteed —
`do_defun`/`register_native` invalidate the direct-dispatch slot, fixing a
split-brain that was live for kernel names. It composes with tc-cache
(fast load) and `--served` (fast dynamic closures): load fast, then the
overlaid spec code runs native. Cold one-shot `--kernel-tests` is unaffected
by design (loaded defuns are ~0% of that wall).

## GC Step 4: collection ON (shipped 2026-06-10, opt-in)

The last greenlit ladder rung. `SHEN_RUST_GC=1` switches the heap to
**request mode**: allocation never collects — `Heap::grow` raises a pending
flag once the footprint outgrows the last live set (heap-doubling policy) —
and the interpreter collects when its activation depth returns to **0**
(guards on `Interp::eval`/`apply`), where no transient `Value` exists in an
owned scope, VM stack, or spilled arg buffer by construction. Roots are the
§6g hybrid: precise enumeration of the interpreter's containers (env tables,
closure-cache constant pools, tc-cache, host `gc_pins`) plus a conservative
native-stack scan with an aarch64 callee-saved register flush for `Value`s
held in host frames. The safepoint choice also collapses the spike's ~7.7×
mid-descent over-retention to ~nothing (dead deep frames sit below the
collect-time stack pointer).

Measured (`benches/gc_boundedness.rs`, 20k served requests, machine-checked):
grow-only ≈ **482 MB and climbing** vs GC ≈ **26 MB flat**, wall-time
neutral; one-shot `--kernel-tests` with GC *on* is ≈ +1–2% — a run sees ~1–2
collections, one of which is a terminal sweep of the full footprint after
results print (pure waste for a one-shot, another reason the default is
off) — and the GC-*off* default path is unchanged (one TLS load + branch
per funnel; paired mins identical). Verification: 134/0 across
release/debug × {GC off, on, aggressive-floor} × {tree-walk, VM}; the
`--debug-gc` gate leg runs the suite under the reentrancy sentinel +
poison-on-sweep; miri covers the precise-collect path. Ship posture:
**off by default** (one-shot needs no reclamation), aarch64 macOS/Linux only
(hard refusal elsewhere), mutually exclusive with the JIT (Cranelift frame
roots unverified), refused on multi-`Interp` threads.

## Engine selection for launcher entrypoints

`script` and `eval` (the kernel launcher's batch entry words) **default the
bytecode VM on**. Loaded user code amortizes the per-body compile cost; on
urdr's pure-Shen SHA/bigint suites the VM was measured ~30–40% faster wall
than the tree-walker while preserving exact-golden digests. The bare REPL and
`--kernel-tests` keep the tree-walker so the one-shot cross-port ratio is
untouched. Override with `SHEN_RUST_VM=0` (force tree-walk) or any other set
value / `enable_vm()` (force VM).

## GC for script workloads (mid-run VM safepoint)

A `script`/`eval` run hands the whole workload to **one** `Interp::apply`
(the launcher's `launch-shen`), so activation depth never returns to 0 until
the script ends — the depth-0 funnel safepoint from Step 4 is unreachable for
the entire run, and with only that path `SHEN_RUST_GC=1` was a silent
grow-only mode (issue #11: multi-GB RSS on urdr replay/prng).

**Fix (shipped on `perf/urdr-workload`)**: the VM dispatch loop polls a
hazard-gated **mid-run** safepoint every 1024 ops. Collection fires only when:

1. `gc_pending` is raised (same heap-doubling request as Step 4),
2. this is the sole `exec` activation on the thread (`exec_depth == 1`),
3. no tree-walker malloc-backed `Value` containers are live (`tw_hazards == 0`
   — owned scopes, spilled arg buffers, materialised lambda locals),
4. only one `Interp` is live on the thread.

Roots: `Interp::gc_roots` + this activation's value stack, frame upvals, and
bytecode constant pools, plus the §6g conservative scan. Sweep is
**Cons/Float-only**: payload-bearing kinds (Vec/Blob/Error/Closure/Opaque)
are retained for the mid-run pass so derived payload borrows in suspended
frames (`&Closure`, bridged `&str`) cannot dangle — the depth-0 safepoint
still sweeps every kind when a funnel finally exits. On list-dominated
workloads (software bigint/SHA) retained kinds are a rounding error of the
footprint.

On the tree-walker, mid-run collection does not exist: with `SHEN_RUST_GC`
and `SHEN_RUST_VM=0` the launcher prints a loud warning rather than silently
growing. `tests/gc_vm_safepoint.rs` is the single-activation boundedness
oracle; `tests/gc_stress.rs` remains the served depth-0 oracle.

`SHEN_RUST_GC_STATS=1` logs both depth-0 and mid-run collections.

Measured on urdr (macOS aarch64, 2026-07-27, release `shen-rust`, ALL PASS):

| suite | config | wall | max RSS |
|---|---|---:|---:|
| prng | VM default, GC off | 8.25s | 1.21 GB |
| prng | VM default + `SHEN_RUST_GC=1` | **6.73s** | **698 MB** |
| prng | tree-walk (`SHEN_RUST_VM=0`) | 16.6s | 1.23 GB |
| prng | tree-walk + GC | 19.0s | 1.17 GB (grow-only; warn) |
| world | VM default, GC off | 6.13s | 788 MB |
| world | VM default + GC | **4.98s** | **344 MB** |

Mid-run collections fire (visible with `SHEN_RUST_GC_STATS=1`). RSS is
reduced, not fully flat on SHA-heavy prng: payload kinds (strings/blobs/
closures) are retained mid-run by design, so blob-heavy churn still grows
until a depth-0 exit. Cons/Float reclamation is what the safepoint can
safely do inside a long activation.

Honest wall-clock note: this does **not** by itself close the ~20–26×
pure-Shen SHA gap vs shen-cl (CL prng ≈0.76s). The next structural lever for
batch urdr code is an AOT overlay of the hot loaded files (same machinery as
the served authz overlay), not more safepoint tuning.

## What's left

- **JIT Win-A W2 for served: parked on measurement** — the JIT cannot see
  loaded named defuns (no `do_defun` tier) and recorded zero executions on the
  authz workload; revival requires an AOT-overlaid profile showing >~40%
  cross-call edges in a mutual-tail group AOT can't loop-compile, gated vs the
  AOT baseline.
- **x86_64 conservative scan** — a `rbx/rbp/r12–r15` register spill would
  extend `SHEN_RUST_GC` beyond aarch64; mechanical, unfunded.
- **AOT overlay of urdr prng/SHA** — if script wall remains ~10–20× CL after
  VM+GC, compile the known hot `.shen` files offline (same
  `scripts/codegen-shen-aot.sh` path as served authz). Not started; needs a
  measured kill-gate against the VM+GC baseline.

## Reproducing

```sh
cargo build --release --bin shen-rust
./scripts/cross-port-bench.sh                         # vs shen-cl (../shen-cl/bin/sbcl/shen)
./scripts/warm-bench.sh                               # tree-walk vs VM, warm
./target/release/shen-rust --kernel-tests >/tmp/r.log &
sample $! 12 1 -file /tmp/sc.txt                      # leaf profile (self-time)
```
