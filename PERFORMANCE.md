# shen-rust performance

## Current state

On the full `--kernel-tests` suite, against the reference `shen-cl` (SBCL)
interpreter, `shen-rust` is **~3.55× slower** (≈7s vs ≈2s loaded) — down from
**~17×** at first conformance. Measure head-to-head with
`scripts/cross-port-bench.sh` (interleaved; the machine has ~5–12% thermal
variance, so trust min-of-N, not single runs).

The bytecode VM is **~2.3× faster than the tree-walker on warm / served
workloads** (`scripts/warm-bench.sh`), which is why it ships behind `--served`
rather than as the bare default — see "the warm/served decision" below.

The living, detailed record is in `design/`:

- `design/perf-state-and-gc-ladder.md` — the scoreboard + the GC/Value/JIT ladder.
- `design/perf-next-target-handoff.md` — the current next-target analysis (incl.
  §3b, the warm/served decision and the VM 2.3× result).
- `design/jit-productionization-plan.md` — the Cranelift JIT, and its §5
  falsification for type-checker closures.

## How the gap was closed (17× → ~3.55×)

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

## Why ~3.55× is structural

Two execution-engine bets — the bytecode VM and the Cranelift closure-JIT —
were both built, validated 134/0, and measured A/B on the one-shot
`--kernel-tests` metric. Both are **non-winners there** (VM neutral/slightly
slower; JIT −15%). The reason is the cost is the **distributed boxed-`Value` +
interpreted-dispatch model itself**, not the per-body dispatch mechanism:
re-encoding how one body runs doesn't change the millions of boxed-value ops,
and a one-shot metric never amortizes runtime compilation (SBCL pre-compiles
ahead of time and pays neither). Every local lever that left the model in place
returned ≤ ~5%. (cons recycling, GC reclamation, `Rc::clone` removal, faster
`lookup_local`, cons-churn elimination — all measured dead.)

## The warm/served decision

The VM *does* win **~2.3×** on a warm / served workload (load a theory once,
serve many type-check / eval requests), where its per-closure compile cost
amortizes — measured paired/interleaved in `scripts/warm-bench.sh`, with the
type-checker's continuations 98.9% VM-served. So the VM ships behind the
`--served` entrypoint (`SHEN_RUST_VM=1`) for long-running embeddings, while the
bare default stays the tree-walker to protect the one-shot cross-port ratio.

## What's left

- **GC Step 4** — turn collection on (precise shadow-stack + conservative
  AOT-frame scan). ~2–3% speed but a real memory win (today's heap is grow-only)
  and finishes the ladder. The only remaining greenlit rung.
- **Closing to ~1×** would require AOT-native-compiling *loaded* user code (the
  SBCL-shaped answer) — a different project; see
  `design/execution-engine-roadmap.md`. Not currently funded.

## Reproducing

```sh
cargo build --release --bin shen-rust
./scripts/cross-port-bench.sh                         # vs shen-cl (../shen-cl/bin/sbcl/shen)
./scripts/warm-bench.sh                               # tree-walk vs VM, warm
./target/release/shen-rust --kernel-tests >/tmp/r.log &
sample $! 12 1 -file /tmp/sc.txt                      # leaf profile (self-time)
```
