# shen-cedar Architecture

Blueprint mirrors `shen-ocaml/ARCHITECTURE.md`; this document tracks
Rust-specific adaptations and Cedar integration design.

## Layering

```
bin/shen-cedar           REPL / CLI
        |
        v
crates/shen-cedar        runtime + KL evaluator + kernel boot + Cedar bridge
        |    |
        |    +-- value.rs       Shen runtime value (Rc-shared enum)
        |    +-- symbol.rs      SymId interner
        |    +-- env.rs         Dual namespace
        |    +-- primitives.rs  ~46 KL primitives
        |    +-- kl/            S-expression parser + AST
        |    +-- interp/        Tree-walking eval + kernel boot
        |    +-- cedar/         cedar-policy bridge + Shen wrappers
        |    +-- generated/     shengen-rust output (Phase 4)
        v
cedar-policy crate
```

## Runtime value (`value.rs`, Phase 1)

`Value` is a tagged enum using `Rc` for shared sub-values. Single-threaded
runtime, so `Rc` rather than `Arc`. Symbols are interned `SymId` from day
one (the optimization `shen-ocaml` left as future work).

## Tail-call strategy

Rust does not guarantee TCO; the kernel is heavily tail-recursive.
The evaluator uses an explicit trampoline: `eval_app` returns
`Step::Done(v)` or `Step::Tail(closure, args)` and the outer loop drives
it. This is correct for arbitrary recursion depth.

## Error handling

`eval` returns `Result<Value, ShenError>` where `ShenError` carries a
`Value` payload. `trap-error` becomes
`match eval(body) { Ok(v) => v, Err(e) => call(handler, e) }`.

## Cedar integration (`cedar/`, Phase 3)

`cedar-policy` crate is embedded directly. Cedar values (Policy,
PolicySet, Schema, Entities, Request, Response) are wrapped as
`Value::Foreign(Rc<dyn Any>)` carrying a Cedar type tag. Primitives
exposed to Shen:

| Shen | Cedar |
|---|---|
| `(cedar.parse-policy STR)` | `Policy::from_str` |
| `(cedar.parse-policy-set STR)` | `PolicySet::from_str` |
| `(cedar.parse-schema STR)` | `Schema::from_str` |
| `(cedar.parse-entities STR)` | `Entities::from_json_str` |
| `(cedar.is-authorized PSET ENTS REQ)` | `Authorizer::is_authorized` |
| `(cedar.validate PSET SCHEMA)` | `Validator::validate` |
| `(cedar.policy->string POLICY)` | `Policy::to_string` |

## Backpressure (Phase 4)

`specs/core.shen` holds sequent-calculus types. `shengen-rust` parses
those and emits `crates/shen-cedar/src/generated/guard_types.rs` with
private fields + public constructors. The five gates: `cargo fmt`,
`cargo build`, `cargo test`, `(tc +)` over specs, TCB audit.
