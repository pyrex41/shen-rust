# shen-rust benchmarks

Numbers from `aot_perf_smoke` and `aot_kernel_bench`, run on the same
machine in release mode (`cargo test --release`). Results vary 10-20%
across runs; what matters is the order of magnitude.

## Hot-loop synthetic (`aot_perf_smoke`)

Source: `crates/shen-rust/tests/aot_smoke.rs`. Runs `(loop-sum 50 0)`
5000 times. `loop-sum` is a 2-arg self-tail-recursive integer loop —
the simplest thing that exercises arithmetic primitives and tail
dispatch without touching reader, pattern matcher, or globals.

```shen
(defun loop-sum (N ACC)
  (if (= N 0) ACC (loop-sum (- N 1) (+ ACC 1))))
```

| Backend                         | Time (5000 calls) | Per call |
|---------------------------------|-------------------|----------|
| Tree-walker                     | ~150 ms           | ~30 µs   |
| AOT, no primitive inlining      | ~58 ms            | ~12 µs   |
| AOT, hot primitives inlined     | ~3.1 ms           | ~0.6 µs  |

Reading the third row: every iteration is `(= N 0)` + `(- N 1)` +
`(+ ACC 1)` + tail dispatch. With `#[inline(always)]` on the `rt::add`/
`rt::sub`/`rt::eq` helpers, each Int+Int operation collapses to a
`match` on `Value::Int` + an inline `checked_add` — release-mode LLVM
keeps the hot tuple in registers and we never touch the heap.

**Release mode speedup over tree-walker: ~48×.** Dev mode is ~28× —
the gap is mostly the `#[inline(always)]` annotation doing its job.

## End-to-end kernel call (`aot_kernel_bench`)

Source: `crates/shen-rust/tests/aot_kernel_bench.rs`. Marked
`#[ignore]`; run with `cargo test --release --test aot_kernel_bench --
--ignored --nocapture`.

Calls `(reverse (append [1 2 3] [4 5 6]))` 2000 times. `append` and
`reverse` are kernel functions (in `core.kl` / `stlib.kl`) so they get
the AOT-compiled bodies installed by `crate::aot::kernel::install_all`.

This is a smoke benchmark — it exists to confirm the AOT-kernel boot
path stays fast end-to-end. The interesting AOT-vs-tree-walker delta
on user-defined Shen is already captured by `aot_perf_smoke` above.

## Kernel test suite (`scripts/kernel-tests.sh`)

The upstream Shen kernel suite (`kernel/tests/kerneltests.shen`) runs
end-to-end against shen-rust via `bin/shen-rust --kernel-tests`.

Current result: **98 passed, 36 failed** (~73% pass rate). The
failures are real bugs in our port, mostly in the
`prolog-interpreter` report (`type error in rule 1 of my-occurs?`)
and a few sequent-calculus edge cases. None of them are blocked on
performance; they're correctness gaps tracked as future work.

Wall time: ~80–115 s in dev mode (depends on system load). Release
mode is faster but rarely run against the kernel suite since dev mode
is good enough for finding the bugs.

## Cross-port: shen-cl interpreted vs shen-rust release

`scripts/cross-port-bench.sh` runs `kernel/tests/runme.shen` (the
upstream Shen kernel test suite, 134 tests) through both ports on the
same machine. Apple M-series, single thread.

| Port                       | Wall time | Per-test |
|----------------------------|----------:|---------:|
| shen-cl (SBCL interpreted) |    ~1.0 s |   ~7 ms |
| shen-rust (release)       |   ~17.5 s |  ~130 ms |
| shen-rust (dev)           |   ~117 s  |  ~870 ms |

shen-rust is currently **~17× slower** than shen-cl on the full kernel
test workload. Some context:

- On a single file load (no type-checking), e.g.
  `(load "kernel/tests/prologinterp.shen")`, the gap shrinks to
  **~6× slower** (169 ms vs 27 ms). The bigger ratio on the full
  suite reflects `(tc +)` mode, which dominates wall time for the
  type-checked reports.
- Boot time is **~10× slower** (140 ms vs 14 ms): we load + AOT-install
  21 kernel files at every startup, while shen-cl uses pre-compiled
  FASLs.

### Where the gap comes from

shen-cl benefits from decades of SBCL optimization — static dispatch
through Common Lisp's package system, native register-based calling
conventions, no per-call HashMap probes. shen-rust's tree-walked Shen
code (most user code, including the test files) pays:

- A `HashMap<SymId, Value>` probe on every function call.
- An `Rc<Closure>` clone on every dispatch.
- A `Vec<Value>` allocation per call site.
- A `Rc<dyn Fn>` virtual call for native primitives.

The AOT-compiled kernel skips some of this but still goes through
`rt::apply_named` for any non-inlined helper — which is most of the
non-arithmetic kernel surface.

### What we've done (Phase 8 so far)

- **8A native hot-fn overrides**: `element?`, `shen.pvar?`,
  `shen.lazyderef`, `fail`, `value/or`, `<-address/or`,
  `read-file-as-bytelist`, `read-file-as-string` replaced with native
  Rust. Per the upstream call-frequency table, these are the
  per-suite call leaders. ~12% wall-time improvement in dev mode.
- **8B eq fast-path**: shen_eq now short-circuits on `Rc::ptr_eq` for
  cons/string pairs (common case when the kernel compares a value to
  itself during proof search).

### What remains (Phase 8 stretch, not yet shipped)

- **Pre-interned SymIds at codegen time** — every call site currently
  does `interp.intern("foo")` at runtime. With per-module `OnceLock`
  caches, that becomes a single atomic load.
- **`SmallVec<[Value; 4]>` args** — eliminates a heap allocation per
  call for the ~90% of call sites with ≤4 args.
- **Direct AOT-to-AOT calls** — skip env-routing entirely for
  statically-known internal callees. Biggest expected lever.

These three combined are estimated at another 35-50% on `aot_perf_smoke`
and proportionally more on a kernel workload. Realistic to close the
gap to ~5× of shen-cl without a `Value` representation rewrite.

## Methodology notes

- All numbers are wall-clock from `std::time::Instant`, single thread.
- The tree-walker baseline includes the AOT-kernel-install step (the
  default boot path); we override individual functions with native
  closures when we need a non-AOT comparison.
- Release mode (`cargo test --release`) is required for the numbers to
  mean anything. Debug builds are 5-10× slower across the board.
