# shen-rust architecture

How the port is layered, and the Rust-specific design decisions. The blueprint
mirrors the other Shen ports (`shen-cl`, `shen-go`, `shen-ocaml`); the
interesting parts here are the value representation, the execution tiers, and
the Cedar integration.

## Layering

```
bin/shen-rust                REPL / CLI  (--served, --kernel-tests)
        |
        v
crates/shen-rust             engine: runtime + evaluator + kernel boot + Cedar
        |   value.rs         word-sized tagged Value(u64)
        |   symbol.rs        SymId interner (dense u32)
        |   env.rs           dual namespace (function / global), property table
        |   primitives.rs    KL primitives + native hot-fn overrides
        |   kl/              s-expression parser + KlExpr AST
        |   interp/          tree-walking eval + kernel boot
        |   vm/              bytecode VM (opcode / compiler / exec / stats)
        |   jit/             experimental Cranelift JIT (feature = "jit")
        |   aot/             generated kernel modules + AOT runtime helpers
        |   gc/              non-moving mark-sweep heap + collector
        |                    (collection opt-in via SHEN_RUST_GC)
        |   cedar/           cedar-policy bridge → first-class cedar.* values
        v
cedar-policy crate           (also used directly from examples/shen-cedar-authz)

crates/klcompile             build-time KL → Rust AOT compiler for the kernel
crates/shengen-rust          Shen sequent-calc specs → Rust guard types
```

## Runtime value (`value.rs`)

`Value` is a word-sized `struct Value(u64)` — a tagged 64-bit word, not an
`Rc`-shared enum. Immediates (Int, Float, Sym, Bool, Nil) live inline with no
allocation; compound values (cons, string, vector, closure, stream, and
`Foreign` host handles) carry a heap reference behind the tag, managed by the
`gc/` heap — a non-moving mark-sweep collector with `Copy` handles. Flipping
the old 24-byte `Rc`-enum to a `Copy` word (GC Step 3) cut per-op memory
traffic and unblocked native codegen; collection (Step 4) is **opt-in** via
`SHEN_RUST_GC`: allocation never collects — the interpreter collects at its
depth-0 safepoints with hybrid roots (precise interpreter tables + a
conservative native-stack scan), bounding the heap for long-running served
embeddings, while the grow-only default protects one-shot latency. Hand-written
`match` sites and the AOT `rt::` helpers are the only code that knows the bit
layout; everything else goes through accessors.

## Execution tiers

The same KL semantics run on whichever tier fits (all differentially tested
against the tree-walker, all 134/0):

1. **Tree-walker** (`interp/eval.rs`) — default. Walks the `KlExpr` AST with an
   explicit **trampoline** for tail calls (Rust has no guaranteed TCO and the
   kernel is heavily tail-recursive). `let`/`lambda` push/pop a scope stack
   rather than cloning locals.
2. **AOT kernel** (`aot/`, `klcompile`) — the 21 kernel files are compiled to
   Rust at build time: self-tail-calls become loops, `if`/`let`/`cond` and ~18
   primitives are inlined, the rest route through `rt::apply_*`. Installed over
   the tree-walked defuns at boot, preserving function-cell late binding.
3. **Bytecode VM** (`vm/`) — runtime closures (`defun`/`lambda`/`freeze`)
   compile to bytecode with integer-slot locals and a static jump table. Enabled
   per-process via `--served` / `SHEN_RUST_VM=1`; wins ~2.3× once compile cost
   amortizes over a served session.
4. **Cranelift JIT** (`jit/`) — experimental native codegen for runtime
   closures, feature-gated and off by default.

## Error handling

`eval` returns `Result<Value, ShenError>`; `ShenError` carries the message
string (plus a cancelled-vs-normal kind for evaluation budgets/deadlines), and
`trap-error` re-presents it to the user handler as a `Value` error object:
`match eval(body) { Ok(v) => v, Err(e) => call(handler, error_value(e)) }`.
Deep non-tail recursion in the AOT reader / type-checker runs on a large-stack
worker thread (1 GB for `--kernel-tests`, 64 MB for the REPL).

## Cedar integration (`cedar/`)

Two surfaces:

1. **First-class `cedar.*` values.** The `cedar-policy` crate is embedded; Cedar
   `Policy` / `PolicySet` / `Schema` / `Entities` / `Request` / `Authorizer` /
   `EntityUid` are wrapped as `Value::Foreign` host handles, and ~15 `cedar.*`
   primitives (`parse-policy`, `parse-policy-set`, `parse-schema`,
   `parse-entities`, `make-entity-uid`, `make-request`, `is-authorized`,
   `is-authorized-detailed`, `validate`, `policy->string`, …) let Shen programs
   manipulate them directly. Metadata is published on the kernel
   `*property-vector*` so `(fn cedar.foo)` resolves.
2. **Rust-side patterns** (`examples/shen-cedar-authz`) — gate / verify /
   generate, driving the engine and `cedar-policy` together from Rust.

## Backpressure (`shengen-rust`)

`specs/*.shen` hold sequent-calculus types. `shengen-rust` parses them and emits
`src/generated/guard_types.rs` (private fields + fallible constructors that
enforce each `: verified` premise). A witness module on the boot path makes any
shengen-output drift break `cargo build`; the gates also re-type-check the specs
with the engine itself (`shen-check`) and TCB-audit the generated output.
