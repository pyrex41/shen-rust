# shen-rust benchmarks

Apple M-series, single thread, release mode. The machine has ~5–12% thermal /
run variance, so the paired harnesses report min-of-N and what matters is the
ratio, not the absolute. Harnesses live in `crates/shen-rust/benches/`,
`crates/shen-rust/tests/`, and `scripts/`.

## Cross-port: shen-rust vs shen-cl (the headline)

`scripts/cross-port-bench.sh` runs the upstream Shen kernel test suite (134
tests) through both ports on the same machine, interleaved.

| Port | `--kernel-tests` | vs shen-cl |
|---|---:|---:|
| shen-cl (SBCL interpreted) | ≈ 2 s | 1× |
| **shen-rust (release)** | ≈ 7 s | **~3.55×** |

Down from ~17× at first conformance (see `PERFORMANCE.md` for the path). The
remaining gap is the boxed-`Value` + interpreted-dispatch model, not a single
hot spot — every local lever returned ≤ ~5%.

## Warm / served: VM vs tree-walker

`scripts/warm-bench.sh` (+ `benches/warm_typecheck.rs`) measures a load-once /
serve-many workload: type-check the heavy corpus once, then repeatedly run the
`normal-form` lambda-calculus reductions (runtime-closure execution).

| Engine | per-batch (min-of-N, paired) | speedup |
|---|---:|---:|
| tree-walker | ≈ 5.84 ms | 1× |
| **bytecode VM** (`--served`) | ≈ 2.50 ms | **~2.3×** |

Coherent with the one-shot result (VM neutral there): warm, the per-closure
compile cost amortizes and the VM's per-body win shows through. The
type-checker's continuations are 98.9% VM-served. This is why the VM ships
behind `--served`. The compute-shaped micro-benchmark `benches/vm_vs_treewalk.rs`
(fib / sumto / cons-walk) shows the VM 1.3–4× on those shapes.

## AOT hot-loop synthetic (`tests/aot_smoke.rs::aot_perf_smoke`)

Runs `(loop-sum 50 0)` — a 2-arg self-tail-recursive integer loop — 5000 times
on the tree-walker vs the AOT-compiled body.

```shen
(defun loop-sum (N ACC) (if (= N 0) ACC (loop-sum (- N 1) (+ ACC 1))))
```

With `#[inline(always)]` on the `rt::add`/`rt::sub`/`rt::eq` helpers, each
Int+Int op collapses to a `match` + inline `checked_add` and the hot tuple stays
in registers — a large (tens-of-×) speedup over the tree-walker in release. This
is the upper bound for *kernel* code (AOT'd at build time); it does **not** apply
to runtime-defined user code, which is the tree-walker's / VM's job.

## Kernel conformance (`scripts/kernel-tests.sh`)

The upstream suite runs end-to-end via `bin/shen-rust --kernel-tests`:
**passed: 134, failed: 0** — in every engine mode (tree-walk, `SHEN_RUST_VM=1`,
`--served`). This is Gate 7 in `scripts/gates.sh`.

## Methodology

- Wall-clock from `std::time::Instant`, single thread, release.
- Paired harnesses interleave A/B runs to share thermal state; take min-of-N.
- The differential test suites (`vm_differential`, `jit_*_differential`) keep
  every non-default tier byte-identical to the tree-walker, so a speed number is
  never bought with a correctness regression.
