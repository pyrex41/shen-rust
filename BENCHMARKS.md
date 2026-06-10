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
is the upper bound for code klcompile sees ahead of time: the kernel (AOT'd at
build time) and, since the overlay shipped, any committed `.shen` file (next
section). Truly runtime-defined code stays the tree-walker's / VM's job.

## AOT overlay on loaded code (`benches/normal_form_aot.rs`, `benches/authz_served.rs`)

The Lever-B measurement: load `.shen` files normally (datatypes/macros/declares
all fire), then swap the loaded defuns for klcompile-emitted native Rust via the
verified overlay (`aot::overlay`). Loaded-vs-AOT is paired in-process;
tree-vs-VM is cross-process interleaved (the engine is process-global). Both
benches assert shen_eq identity on every query before any timing counts, and
`authz_served` installs through the production `install_overlay_if_match` path
(manifest: source FNV + kernel digest), plus a post-timing redefinition leg
proving a `(defun ...)` over the installed overlay wins on both dispatch paths.

| Workload | engine | loaded (min) | AOT (min) | AOT speedup |
|---|---|---:|---:|---:|
| `normal_form_aot` (interpreter.shen, lambda-heavy rewriter) | tree | ≈ 282 ms/batch | ≈ 54 ms/batch | **≈ 5.3×** |
| | VM | ≈ 99 ms/batch | ≈ 58 ms/batch | **≈ 1.7–2.1×** |
| `authz_served` (authz spec: `reaches`/`classify` over a 64-role DAG, 576 classifications/query) | tree | ≈ 300 ms/batch | ≈ 26 ms/batch | **≈ 11.7×** |
| | VM | ≈ 80 ms/batch | ≈ 26 ms/batch | **≈ 3.0–3.2×** |

The AOT arm is engine-independent (~26 ms/batch either way) — it has left the
interpreter entirely. **Kill-gate: AOT ≥ 1.5× over the VM-loaded arm** on
`authz_served` before any default-on consideration (passed at 2× margin; the
overlay still ships opt-in). Pure cons-recursion (authz) benefits more than the
closure-heavy rewriter shape (normal-form). Regenerate artifacts with
`scripts/codegen-shen-aot.sh <out.rs> <in.shen>...` — the bench refuses
(loudly) on a stale manifest.

A `--features jit SHEN_RUST_JIT=1 SHEN_RUST_JIT_STATS=1` run of either bench is
a coverage probe only (the JIT has no tier for named defuns): on `authz_served`
it recorded **zero JIT executions**, which is the measurement that parked
JIT-W2-for-served.

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
