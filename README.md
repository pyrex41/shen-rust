# shen-cedar

A port of the [Shen](https://shenlanguage.org/) language hosted in Rust,
with first-class [AWS Cedar](https://www.cedarpolicy.com/) integration.

> Status: Phase 0 scaffold. Nothing runs yet — see `STATUS.md`.

## Why "cedar"?

AWS Cedar policy language is intentionally not Turing-complete (no
closures, recursion, mutable state, or I/O) and therefore cannot host
Shen as a runtime. This project's name reflects the **distinguishing
feature** rather than the host: the host language is Rust, and the
project embeds the `cedar-policy` crate so that Shen programs can author,
evaluate, and (eventually) verify Cedar policies as first-class values.

## Layout

```
crates/shen-cedar/   The port itself (KL runtime, kernel boot, Cedar bridge)
crates/shengen-rust/ Codegen tool: Shen sequent-calc specs -> Rust guards
bin/shen-cedar/      REPL / CLI binary
kernel/              Vendored ShenOSKernel-41.1 (klambda + tests)
specs/               Backpressure specs in Shen sequent-calc syntax
```

## Build

```
cargo build
```

## License

BSD-3-Clause for the port code; kernel sources retain their upstream
license (`kernel/LICENSE.txt`).
