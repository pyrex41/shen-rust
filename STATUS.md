# shen-rust status

## Current state

A working port of the Shen language to Rust. It boots the upstream
**ShenOSKernel-41.1** and passes the full conformance suite — **134 / 134**
kernel tests (`scripts/kernel-tests.sh`) — in every execution mode. All gates
green (`scripts/gates.sh`): shengen-codegen, fmt+clippy, build, test
(unit + integration suites), shen-check, tcb-audit, kernel-aot-audit,
kernel-tests, kernel-tests-debug (the debug-build run carries the heap
reentrancy sentinel — see the split-TLS note in `value.rs`), and
kernel-tests-debug-gc (the same debug suite with GC collection forced
aggressive, under the sentinel plus poison-on-sweep).

What's in the tree today:

- **Engine** (`crates/shen-rust/`) — KL runtime, tree-walking evaluator with
  trampolined TCO, full kernel boot. `Value` is a word-sized `struct Value(u64)`
  (tagged; immediates unboxed, cons/string/foreign heap-boxed) over a
  non-moving mark-sweep GC heap. **Collection is opt-in** (`SHEN_RUST_GC`,
  GC Step 4): deferred to interpreter depth-0 safepoints, hybrid roots
  (precise interpreter tables + conservative native-stack scan + aarch64
  register flush); default stays grow-only. Bounded heap for served
  embeddings — 482 MB → 26 MB flat on the 20k-request demo
  (`benches/gc_boundedness.rs`), wall-neutral.
- **AOT kernel** — every kernel `.kl` compiled to Rust at build time by
  `crates/klcompile/`, installed over the tree-walked defuns.
- **AOT overlay for loaded code** (opt-in) — known `.shen` files compiled
  offline by the same klcompile (now lib+CLI; `scripts/codegen-shen-aot.sh`)
  and swapped over the loaded defuns through a verified manifest
  (`aot/overlay.rs`: source hash + kernel digest, all-or-nothing arity
  precheck, silent fallback). ~3.1× over the VM on the served authz workload
  (`benches/authz_served.rs`, gate ≥1.5×); redefinition coherence guaranteed
  (`do_defun`/`register_native` clear the direct-dispatch slot —
  `tests/aot_redefine_coherence.rs`, a fix that was live for kernel names too).
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

Performance vs the reference `shen-cl` (SBCL) on one-shot `--kernel-tests`:
**~3.0× bare** (≈3.0 s vs ≈1.0 s, paired 2026-06-10; down from ~17× at first
conformance), **at parity with warm tc-cache** (≈1.0 s, off by default). The
remaining bare gap is structural — the boxed-`Value` + interpreted-dispatch
model, not a single hot spot. On served workloads the story inverts: VM
~2.3× warm, AOT overlay ~3.1× over that on spec code. Full story in
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
- **AOT overlay productionized** (2026-06-09) — two-table redefinition
  coherence fixed (was live for ~1123 kernel names), klcompile split lib+CLI
  with zero kernel byte drift, verified overlay install API + canonical
  codegen pipeline, `authz_served` bench gates the lever at 3.0–3.2× over the
  VM-loaded arm; verify/generate examples serve it opt-in. JIT-W2-for-served
  parked on a measured 0.0% (the JIT cannot see loaded named defuns; zero JIT
  executions on the authz workload).
- **Runtime-overhead strip** (2026-06-10) — a profiling round took one-shot
  `--kernel-tests` from ~3.3× to ~3.0× off shen-cl (~18% cumulative):
  thin-LTO release profile, the **split-TLS heap** (the thread-local
  `RefCell<Heap>` was a destructor key; now a no-`Drop` raw-pointer fast path
  — adversarially reviewed, miri-clean, with a new debug-sentinel gate), and
  a direct-mapped intern cache for AOT call-target resolution. One falsified
  candidate recorded (filtered closure-capture caching, −3.5%).
- **GC Step 4: collection ON (opt-in)** (2026-06-10) — the last greenlit
  ladder rung. Request-mode collection: allocation never collects (`grow`
  raises a pending flag at heap-doubling pressure); the interpreter collects
  at activation-depth-0 safepoints where the transient-root problem vanishes
  by construction. Hybrid roots per the §6g spike: precise container
  enumeration (env tables, closure-cache constant pools, tc-cache, host
  pins) + conservative native-stack scan with aarch64 callee-saved register
  flush (pthread stack bounds; hard refusal on unsupported targets). Off by
  default; JIT mutually excluded; multi-`Interp` threads refuse collection.
  134/0 across release/debug × GC off/on/aggressive × tree-walk/VM; miri
  clean; one-shot wall unchanged (GC off identical binary path, GC on ≈ +1%).
- **Rename** — the engine port `shen-cedar` → **`shen-rust`** (the name
  `shen-cedar` now denotes the Shen+Cedar examples). History before that commit
  says "shen-cedar".

## Known limitations

- Boot loads + AOT-installs 21 kernel files every startup (~sub-second release);
  shen-cl uses pre-compiled FASLs.
- GC collection is opt-in (`SHEN_RUST_GC=1`) and requires aarch64
  macOS/Linux (the conservative stack scan is unimplemented elsewhere —
  refused with a warning, heap stays grow-only). Hosts embedding the engine
  under GC must follow the pin/borrow rules in `value.rs` ("Collection"
  note) and `Interp::gc_pins`.
- The JIT is experimental, off by default, and mutually exclusive with the GC
  (Cranelift frame roots unverified).
