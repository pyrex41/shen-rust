# shen-rust

[![CI](https://github.com/pyrex41/shen-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/pyrex41/shen-rust/actions/workflows/ci.yml)

A port of the [Shen](https://shenlanguage.org/) programming language to Rust,
with native [AWS Cedar](https://www.cedarpolicy.com/) authorization integration.

Shen is a functional language with an integrated logic engine and an optional,
very expressive type system (a sequent-calculus theorem prover). `shen-rust`
boots the upstream **ShenOSKernel-41.1** and passes its full conformance suite —
**134 / 134 kernel tests**, in every execution mode.

The name follows the Shen-port convention (`shen-cl`, `shen-go`, `shen-ocaml`):
the engine is `shen-rust`. The name `shen-cedar` is reused for the Shen **+**
Cedar integration that ships in `examples/`.

## Quick start

```sh
# Build the workspace
cargo build --release

# REPL
cargo run --release --bin shen-rust

# Long-running / served mode (enables the bytecode VM — ~2.3× warm)
cargo run --release --bin shen-rust -- --served

# Run the upstream Shen kernel conformance suite (expect 134/0)
cargo run --release --bin shen-rust -- --kernel-tests
```

Rustup users get the pinned toolchain from `rust-toolchain.toml`
automatically; a Nix flake (`nix develop`) provides a dev shell.

## Documentation

| Doc | Contents |
|---|---|
| [STATUS.md](STATUS.md) | current state, milestones, known limitations |
| [ARCHITECTURE.md](ARCHITECTURE.md) | layering, value representation, execution tiers, Cedar bridge |
| [PERFORMANCE.md](PERFORMANCE.md) | how the gap to `shen-cl` was closed (17× → ~3×), and what remains |
| [BENCHMARKS.md](BENCHMARKS.md) | the numbers, with methodology — all reproducible from `scripts/` and `benches/` |
| `design/` | internal working notes: decision records, handoffs, falsified experiments |

## Execution engine

The same Shen semantics run on tiers chosen for the workload:

| Tier | When | Notes |
|---|---|---|
| **Tree-walker** | default | Allocation-light interpreter over the KL AST. Best for one-shot runs. |
| **AOT kernel** | build time | The 21 kernel KL files are compiled to Rust ahead of time by `crates/klcompile` — control flow lowered (self-tail-calls → loops; `if`/`let`/`cond` and ~18 primitives inlined). |
| **Bytecode VM** | `--served` / `SHEN_RUST_VM=1` | Runtime closures (`defun`/`lambda`/`freeze`) compile to bytecode. **~2.3× faster than the tree-walker on warm / served workloads** (load a theory once, serve many requests), where the compile cost amortizes. Not the bare default because a one-shot run can't amortize it. See `scripts/warm-bench.sh`. |
| **AOT overlay** | opt-in, per `.shen` file | Known `.shen` files are compiled to Rust offline (`scripts/codegen-shen-aot.sh`, same klcompile) and committed; after a normal load (all side effects live) the host swaps the loaded defuns for the native versions via a verified manifest (`Interp::install_overlay_if_match` — source hash + kernel digest, silent fallback on mismatch). **~3.1× over the VM, ~11.7× over the tree-walker on served authz workloads** (`benches/authz_served.rs`). |
| **Cranelift JIT** | `--features jit`, `SHEN_RUST_JIT=1` | Experimental; native codegen for runtime closures. Wins on compute loops but was falsified for the type-checker's CPS continuations — kept gated/off. See `design/jit-productionization-plan.md`. |

Every tier is differentially tested against the tree-walker and held at 134/0.

**Memory**: `Value` is a word-sized `Copy` tagged `u64` over a non-moving
mark-sweep GC heap. Collection is opt-in (`SHEN_RUST_GC=1`): the heap stays
grow-only by default (protects one-shot latency), and in GC mode a long-running
embedding gets a **bounded heap** — collection runs at interpreter safepoints
with hybrid roots (precise interpreter tables + a conservative native-stack
scan, aarch64 macOS/Linux). On a 20k-request served loop:
grow-only ≈ 482 MB and climbing; GC ≈ 26 MB flat, wall-time neutral
(`cargo bench --bench gc_boundedness`, machine-checked).

## Shen + Cedar

Cedar is AWS's authorization-policy language. `shen-rust` combines with it on
two levels:

**1. Cedar as first-class Shen values.** The engine embeds the `cedar-policy`
crate and exposes ~15 `cedar.*` primitives (`crates/shen-rust/src/cedar/`), so
Shen *programs* can parse, author, evaluate, and validate Cedar policies
directly — Cedar handles travel as ordinary Shen values:

```shen
(set p (cedar.parse-policy "permit(principal, action, resource);"))
(set ps (cedar.policy-set-add (cedar.empty-policy-set) (value p)))
(cedar.is-authorized (value ps) (cedar.empty-entities)
   (cedar.make-request (cedar.make-entity-uid "User" "alice")
                       (cedar.make-entity-uid "Action" "read")
                       (cedar.make-entity-uid "Doc" "d1") ()))
;; => Allow
```

**2. Worked integration patterns.** `examples/shen-cedar-authz` shows three ways
the engine (in served / VM mode) and Cedar combine on the Rust side, natively in
one process:

```sh
cargo run -p shen-cedar-authz --example gate      # Cedar gates Shen evaluation
cargo run -p shen-cedar-authz --example verify    # Shen reasons ABOUT Cedar policies
cargo run -p shen-cedar-authz --example generate  # Cedar generated FROM a Shen spec
```

- **gate** — each request `(principal, action, resource, shen-source)` is
  authorized by Cedar (schema-validated); only on `Allow` does the served VM
  evaluate the source.
- **verify** — Shen-authored, hierarchy-aware analysis of a Cedar `PolicySet`:
  flags dead (shadowed) permits and partial conflicts that Cedar's per-request
  evaluator won't surface, cross-checked against the live authorizer.
- **generate** — a committed Shen spec (`spec/authz.shen`) is the source of
  truth; the Shen engine computes the transitive role-grant closure; the host
  renders, strict-validates, and enforces the resulting Cedar policy.

See the crate's [README](examples/shen-cedar-authz/README.md) for details.

## shengen — formal backpressure

`crates/shengen-rust` compiles Shen sequent-calculus specs (`specs/`) into Rust
**guard types**, so a spec change becomes a compile error rather than silent
drift — Shen as a formal-spec language for other code. The specs are also
re-type-checked by the engine itself on every CI run (`scripts/shen-check.sh`).

## Layout

```
crates/shen-rust/           the engine: KL runtime, kernel boot, tree-walker, VM, AOT, JIT
crates/klcompile/           build-time KL → Rust AOT compiler for the kernel
crates/shengen-rust/        Shen sequent-calc specs → Rust guard types (backpressure)
bin/shen-rust/              the REPL / CLI (`--served`, `--kernel-tests`)
examples/shen-cedar-authz/  Shen + Cedar integration (gate / verify / generate)
kernel/                     vendored ShenOSKernel-41.1 (klambda + conformance tests)
specs/                      backpressure specs in Shen sequent-calculus syntax
design/                     architecture + performance design notes
scripts/                    gates.sh (CI), benches, cross-port + warm benchmarks
```

## Performance

The reference target is the upstream `shen-cl` (SBCL) port. Two metrics, two
answers (paired interleaved runs, 2026-06-10, Apple M-series):

**One-shot** (`--kernel-tests`, boot + load + run + exit):

| Config | Wall | vs shen-cl |
|---|---:|---:|
| shen-cl (SBCL) | ≈ 1.0 s | 1× |
| shen-rust, bare | ≈ 3.0 s | **~3.0× off** |
| shen-rust + tc-cache (`SHEN_RUST_TC_CACHE=<dir>`, warm) | ≈ 1.0 s | **at parity / ahead** |

The bare gap is structural — the boxed-`Value` + interpreted-dispatch model,
not a single hot spot. A 2026-06-10 profiling round stripped the runtime's
own overheads (split-TLS heap access, direct-mapped call-target interning,
thin-LTO build) for ~18 % cumulative; what remains is the model, where each
local lever measures ≤ ~8 %. The tc-cache win is typecheck-verdict
memoization, not raw speed; it's off by default.

**Served** (long-lived process: load once, serve many evaluations) — the
funded direction, where the tiers stack:

| Tier | vs tree-walk loaded | vs VM loaded |
|---|---:|---:|
| bytecode VM (`--served`) | ~2.3× | 1× |
| **AOT overlay** (committed `.shen` → native) | ~5.3–11.7× | **~1.9–3.2×** |

On served spec code the overlay leaves the interpreter entirely (the
SBCL-shaped answer for that niche): `benches/authz_served.rs` measures
~3.1× over the VM-loaded arm. For long-running served processes,
`SHEN_RUST_GC=1` bounds the heap (see "Execution engine" above). Full history
and the GC / value-representation / JIT ladder live in `PERFORMANCE.md`,
`BENCHMARKS.md`, and `design/perf-*.md`.

## Development

```sh
./scripts/gates.sh    # full CI: fmt+clippy, build, test, shen-check, audits, kernel-tests
```

## License

[BSD-3-Clause](LICENSE) for the port code (© 2026 Reuben Brooks). The vendored
Shen kernel under `kernel/` is © 2010–2022 Mark Tarver and retains its own
BSD-3-Clause license (`kernel/LICENSE.txt`).
