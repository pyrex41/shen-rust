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

## What's left

- **GC Step 4** — turn collection on (precise shadow-stack + conservative
  AOT-frame scan). ~2–3% speed but a real memory win (today's heap is grow-only)
  and finishes the ladder. The only remaining greenlit rung. (Overlay
  `make_aot_closure` captures are on the Step-4 roots checklist.)
- **JIT Win-A W2 for served: parked on measurement** — the JIT cannot see
  loaded named defuns (no `do_defun` tier) and recorded zero executions on the
  authz workload; revival requires an AOT-overlaid profile showing >~40%
  cross-call edges in a mutual-tail group AOT can't loop-compile, gated vs the
  AOT baseline.

## Reproducing

```sh
cargo build --release --bin shen-rust
./scripts/cross-port-bench.sh                         # vs shen-cl (../shen-cl/bin/sbcl/shen)
./scripts/warm-bench.sh                               # tree-walk vs VM, warm
./target/release/shen-rust --kernel-tests >/tmp/r.log &
sample $! 12 1 -file /tmp/sc.txt                      # leaf profile (self-time)
```
