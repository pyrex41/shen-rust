# shen-cedar-authz — Shen + Cedar, natively in Rust

Worked examples of combining the **Shen engine** (this port, run in its
**served / VM mode** — the `--served` embedding where the bytecode VM's
per-body win amortizes) with **AWS Cedar** (`cedar-policy`), in one Rust
process. Three integration shapes, sharing the `ShenHost` embedding lib
(`src/lib.rs`):

| Example | Direction | What it shows |
|---|---|---|
| `gate` | Cedar **gates** Shen | Each request `(principal, action, resource, shen-src)` is authorized by Cedar; only on `Allow` does the served VM evaluate. |
| `verify` | Shen reasons **about** Cedar | Shen-authored, hierarchy-aware analysis of a `PolicySet`: detects dead (shadowed) permits and partial conflicts that Cedar's per-request evaluator won't surface. The verifier lives in `spec/verify.shen`. |
| `generate` | Cedar generated **from** Shen | `spec/authz.shen` is the source of truth; the VM computes the transitive grant closure; the host renders + validates Cedar permits. |

```sh
cargo run -p shen-cedar-authz --example gate
cargo run -p shen-cedar-authz --example verify
cargo run -p shen-cedar-authz --example generate
```

## Hardening (vs the original prototypes)

- **Cedar schema + strict validation.** `authz.cedarschema` is the contract;
  every example strict-validates its policy set (`Validator` /
  `ValidationMode::Strict`), validates entities (`Entities::from_json_str` with
  the schema), and builds schema-checked requests (`Request::new(.., Some(&schema))`).
- **`verify` resolves `in` over the role DAG**, not as string equality. The
  Shen verifier carries a `reaches` membership-closure predicate, so
  `forbid(principal in Role::"Staff")` correctly shadows
  `permit(principal in Role::"Analyst")` when `Analyst in Staff`. Each static
  finding is cross-checked against the live Cedar authorizer.
- **`generate` reads the committed spec file** `spec/authz.shen` (the source of
  truth), strict-validates the generated policy, and writes it out as a build
  artifact.

## AOT overlay (opt-in)

`verify` and `generate` serve their spec code as **klcompile-emitted native
Rust**: after the normal load, `ShenHost::install_aot_overlay` swaps the loaded
defuns for the committed compiled modules (`examples/gen/{verify,authz}_aot.rs`)
— but only when the artifact's manifest matches the live spec source and the
booted kernel (source FNV + kernel digest); any mismatch silently leaves the VM
serving. `generate` additionally asserts the generated Cedar artifact is
byte-identical on both engines. The served measurement behind this is
`crates/shen-rust/benches/authz_served.rs` (~3.1× over the VM on the
verification sweep). Regenerate after editing a spec file:

```sh
scripts/codegen-shen-aot.sh examples/shen-cedar-authz/examples/gen/verify_aot.rs examples/shen-cedar-authz/spec/verify.shen
scripts/codegen-shen-aot.sh examples/shen-cedar-authz/examples/gen/authz_aot.rs examples/shen-cedar-authz/spec/authz.shen
```

`gate` is deliberately **not** overlaid: its Shen cost is fresh-source kernel
`eval` (the reader), which the overlay doesn't touch.

## Scope / caveats

Still illustrative, not a product: scopes use a simplified set model
(`any`/`in`/`eq`; `Is`-typed scopes are widened to `any`), the verifier's
DAG/entities are example data, and there is no context-based policy or HTTP
front. The `ShenHost` lib is the reusable piece.
