# Kernel provenance

## Shen kernel — S41.2 (2026-07-11 refresh), Mark Tarver

The 15 core `.kl` files below are byte-identical copies of the `KLambda/`
directory of Mark Tarver's 2026-07-11 S41.2 upload.

**Canonical source** — `pyrex41/shen-s41.1`, the designated mirror of
Tarver's uploads (a private GitHub repo):

- Tag: `s41.2-pristine-20260711`
- Commit: `11fc51bdf53a4dcb505adeec6ec8352754cbe50f`
- Path in mirror: `KLambda/` (`diff`-verified byte-identical to the files
  vendored here)

**Original upstream** (secondary — what the mirror imported):

- URL: https://www.shenlanguage.org/Download/S41.2.zip
- Upstream `Last-Modified`: 2026-07-11
- Zip SHA-256: `51becbfd60fa8c93c3f8ae5b20b948eaa84c4b1d14ad2f5d2a056002a53ee836`

Vendored files (byte-identical to both the mirror tag and the zip's
`KLambda/`):

    backend.kl   core.kl      declarations.kl  load.kl    macros.kl
    prolog.kl    reader.kl    sequent.kl       sys.kl     t-star.kl
    toplevel.kl  track.kl     types.kl         writer.kl  yacc.kl

The zip's `KLambda/trace.lsp` is a Common-Lisp helper, not a `.kl` module,
and is not vendored.

### ⚠️ Version number reused for a restructured kernel

Upstream **reused the "41.2" version string** for a kernel that is
**restructured and of a different lineage** from the community
`ShenOSKernel-41.2` (github.com/Shen-Language/shen-sources, tag
`shen-41.2`) that this port previously vendored. `*version*` is still
`"41.2"` — that is faithful to upstream — but the module set is not the
same release. Treat "S41.2 (2026-07-11 refresh)" as the precise identity.

Delta vs the community `ShenOSKernel-41.2`:

- **Removed** `dict.kl`, `compiler.kl`, `init.kl`:
  - `put`/`get` no longer use a dict layer; they store pointer lists on
    the property vector directly (`shen.change-pointer-value` /
    `shen.remove-pointer`, in `sys.kl`). No native dict primitives are
    needed.
  - `compiler.kl` was a shen-cl build artifact (the KLambda image of the
    CL compiler), never part of the release zip.
  - `init.kl`'s content moved into `declarations.kl` and `toplevel.kl`.
    There is **no `shen.initialise` function** any more: `declarations.kl`
    sets `*property-vector*`, the arity table (`shen.initialise-arity-table`),
    and the lambda table (`shen.build-lambda-table`) via **top-level forms
    evaluated during file load**. `toplevel.kl` provides
    `shen.initialise_environment` (per-REPL-iteration counter reset).
- **Added** `backend.kl` — a `cl.*` KLambda→Common-Lisp backend. Upstream
  loads it as the precompiled `backend.lsp` (its `.kl`→Lisp compiler), and
  it is **not** in `install.lsp`'s `.kl` boot list. It is dead weight for a
  Rust port, so it is vendored + AOT-generated (for audit completeness)
  but **not booted** — see `crates/shen-rust/src/interp/boot.rs`
  (`KERNEL_FILES`) and `crates/shen-rust/src/aot/kernel/mod.rs`.

### Boot order

`KERNEL_FILES` follows upstream **`install.lsp`** (the reference SBCL
port's runtime loader), NOT `Sources/make.shen` (the bootstrap-generation
order). Order is load-bearing now: `declarations.kl` and `macros.kl` must
precede `types.kl` (whose 161 top-level `(declare …)` forms invoke the
type checker), and `types.kl` loads last.

## Standard library — now loaded from source (`stlib.kl` retired)

Tarver's refresh no longer ships the standard library as a kernel `.kl`;
it lives under `Lib/StLib/` as separately-loaded Shen sources. This port
now follows upstream: the community `ShenOSKernel-41.2` `stlib.kl` overlay
has been **removed**, and the stdlib loads from the vendored S-lineage
sources under `../stlib/` at boot (`boot::load_stlib` runs upstream's
`install.shen`). See `../stlib/PROVENANCE.md`. This also fixes the
arity `-1` quirk the pre-compiled overlay caused (`(fn filter)` failing).

## Extensions (retained, community additions)

`extension-features.kl`, `extension-expand-dynamic.kl`, and
`extension-launcher.kl` are community additions (not upstream Tarver
files), retained and booted on top of the kernel. `extension-launcher.kl`
provides the Ratatoskr stage-1 launcher CLI. They reference no removed
kernel symbol (`extension-launcher` only reads the `*hush*` global, still
present). `extension-programmable-pattern-matching.kl` is vendored +
AOT-generated for audit completeness but not booted.

## Regeneration

The AOT kernel modules in `crates/shen-rust/src/aot/kernel/*.rs` are
generated from these files via `scripts/codegen-kernel-aot.sh` and
byte-frozen by the Gate 6 audit (`scripts/kernel-aot-audit.sh`). After any
change here, regenerate and re-commit.
