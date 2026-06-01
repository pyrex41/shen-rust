# shen-rust

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

## Execution engine

The same Shen semantics run on tiers chosen for the workload:

| Tier | When | Notes |
|---|---|---|
| **Tree-walker** | default | Allocation-light interpreter over the KL AST. Best for one-shot runs. |
| **AOT kernel** | build time | The 21 kernel KL files are compiled to Rust ahead of time by `crates/klcompile` — control flow lowered (self-tail-calls → loops; `if`/`let`/`cond` and ~18 primitives inlined). |
| **Bytecode VM** | `--served` / `SHEN_RUST_VM=1` | Runtime closures (`defun`/`lambda`/`freeze`) compile to bytecode. **~2.3× faster than the tree-walker on warm / served workloads** (load a theory once, serve many requests), where the compile cost amortizes. Not the bare default because a one-shot run can't amortize it. See `scripts/warm-bench.sh`. |
| **Cranelift JIT** | `--features jit`, `SHEN_RUST_JIT=1` | Experimental; native codegen for runtime closures. Wins on compute loops but was falsified for the type-checker's CPS continuations — kept gated/off. See `design/jit-productionization-plan.md`. |

Every tier is differentially tested against the tree-walker and held at 134/0.

## Shen + Cedar

Cedar is AWS's authorization-policy language. `examples/shen-cedar-authz` shows
three ways the `shen-rust` engine (in served / VM mode) and the `cedar-policy`
crate combine, natively, in one process:

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
drift. This is the "Shen as a formal-spec language for other code" line; see
`sb.toml` and the `sb:*` skills.

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

The reference target is the upstream `shen-cl` (SBCL) port. On the one-shot
`--kernel-tests` metric `shen-rust` is ~3.55× off SBCL — a structural gap from
the boxed-`Value` + interpreted-dispatch model, not a single hot spot (every
local lever has returned ≤ ~5 %). The bytecode VM wins ~2.3× on warm / served
workloads, which is why it's exposed via `--served`. Full history and the
GC / value-representation / JIT ladder live in `PERFORMANCE.md`, `BENCHMARKS.md`,
and `design/perf-*.md`.

## Development

```sh
./scripts/gates.sh    # full CI: fmt+clippy, build, test, shen-check, audits, kernel-tests
```

## License

[BSD-3-Clause](LICENSE) for the port code (© 2026 Reuben Brooks). The vendored
Shen kernel under `kernel/` is © 2010–2022 Mark Tarver and retains its own
BSD-3-Clause license (`kernel/LICENSE.txt`).
