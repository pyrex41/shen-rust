# Kernel provenance

## ShenOSKernel-41.2 (everything except `compiler.kl`)

- Release tag: `shen-41.2`
- URL: https://github.com/Shen-Language/shen-sources/releases/tag/shen-41.2
- Source zip SHA-256: `49f1b85d02348d9b3ebc461570c5c56cc066270ab81e35d5257625fb9d17fe82`

All 21 `.kl` files other than `compiler.kl` are **byte-identical** copies of
the `klambda/` directory in that release zip (verified with `diff -r`).
This includes `extension-programmable-pattern-matching.kl`, new in 41.2.
It is vendored for completeness but is **opt-in**: it is not part of the
canonical 21-module boot list in `crates/shen-rust/src/interp/boot.rs`.

The conformance suite under `kernel/tests/` (including the 41.2
`tests/extensions/` harness) and `kernel/README.md` come byte-identical
from the same release zip. `kernel/LICENSE.txt` is unchanged from the
release.

## `compiler.kl` caveat

`compiler.kl` is **not part of the upstream release zip** — it is a
generated artifact of the shen-cl build (the KLambda image of Shen's
compiler). The vendored copy was produced from a fresh 41.2 shen-cl
build at `ShenOSKernel-41.2/klambda/compiler.kl`:

- SHA-256: `ed30a08be4c8916b1e844d437fb9d65a36476e1a99419340b5523fc81a7c3e44`

Because it is generated rather than released, it has no upstream zip to
be byte-compared against; the hash above pins the exact copy in use.

## Regeneration

The AOT kernel modules in `crates/shen-rust/src/aot/kernel/*.rs` are
generated from these files via `scripts/codegen-kernel-aot.sh` and
byte-frozen by the Gate 6 audit (`scripts/kernel-aot-audit.sh`). After
any change here, regenerate and re-commit.
