# shen-rust status

## Current state

A working port of the Shen language to Rust. It boots the upstream
**ShenOSKernel-41.1** and passes the full conformance suite — **134 / 134**
kernel tests (`scripts/kernel-tests.sh`) — in every execution mode. All gates
green (`scripts/gates.sh`): shengen-codegen, fmt+clippy, build, test
(unit + 10 integration suites), shen-check, tcb-audit, kernel-aot-audit,
kernel-tests.

What's in the tree today:

- **Engine** (`crates/shen-rust/`) — KL runtime, tree-walking evaluator with
  trampolined TCO, full kernel boot. `Value` is a word-sized `struct Value(u64)`
  (tagged; immediates unboxed, cons/string/foreign heap-boxed).
- **AOT kernel** — every kernel `.kl` compiled to Rust at build time by
  `crates/klcompile/`, installed over the tree-walked defuns.
- **Bytecode VM** (`src/vm/`) — runtime closures compile to bytecode; **~2.3×
  warm** via `--served` / `SHEN_RUST_VM=1` (`scripts/warm-bench.sh`).
- **Cranelift JIT** (`src/jit/`, `--features jit`) — experimental; correct and
  gated off (falsified for the type-checker's CPS continuations; see
  `design/jit-productionization-plan.md`).
- **Cedar integration** — (1) in-process `cedar.*` primitives (`src/cedar/`)
  exposing Cedar as first-class Shen values; (2) worked patterns in
  `examples/shen-cedar-authz` (gate / verify / generate).
- **shengen** (`crates/shengen-rust/`) — Shen sequent-calc specs → Rust guard
  types; `specs/core.shen`.

Performance vs the reference `shen-cl` (SBCL) on `--kernel-tests`: **~3.55×**
(down from ~17× at first conformance). The remaining gap is structural — the
boxed-`Value` + interpreted-dispatch model, not a single hot spot. Full story in
`PERFORMANCE.md`, `BENCHMARKS.md`, and `design/perf-*.md`.

## Milestones

- **Phase 0–2** — workspace + vendored kernel; KL runtime (parser, interner,
  dual-namespace env, tree-walker w/ trampolined TCO); boot ShenOSKernel-41.1,
  REPL up.
- **Phase 3** — `cedar.*` primitives: Cedar policies/entities/requests as
  first-class Shen values (`src/cedar/`, `cedar_smoke` tests).
- **Phase 4** — `shengen-rust` + the backpressure gates (specs → guard types,
  forgery boundary enforced at `cargo build`).
- **Phase 5–6** — `klcompile` AOT (KL → Rust) for the whole kernel; hot-primitive
  inlining; `--kernel-tests` runner.
- **Phase 7** — **134 / 0**: fixed `Bool` vs `Sym("true")` equality, closure
  `symbol?` misclassification, Shen surface-syntax parsing. Full suite passes.
- **Phase 8+** — performance program: stacked tree-walker/dispatch wins
  (17.5s → ~5.7s), then the execution-engine + memory ladder:
  - word-sized `Copy` `Value` (24B `Rc` enum → tagged `u64`) + GC spikes
    (mark-sweep + shadow-stack + conservative AOT-frame scan); GC Steps 1–3
    shipped grow-only;
  - bytecode VM built, then the **warm/served decision**: VM is ~2.3× warm but
    neutral one-shot, so it ships behind `--served` rather than default-on
    (`design/perf-next-target-handoff.md` §3b);
  - Cranelift JIT spike → J1/J2 — falsified for closures, kept gated.
- **Cedar app work** — three hardened integration examples consolidated into
  `examples/shen-cedar-authz` (gate / verify / generate).
- **Rename** — the engine port `shen-cedar` → **`shen-rust`** (the name
  `shen-cedar` now denotes the Shen+Cedar examples). History before that commit
  says "shen-cedar".

## Known limitations

- Boot loads + AOT-installs 21 kernel files every startup (~sub-second release);
  shen-cl uses pre-compiled FASLs.
- Heap is grow-only (GC collection not yet enabled — Step 4 is the remaining
  greenlit ladder rung; reclamation is a memory win, ~2–3% speed).
- The JIT is experimental and off by default.
