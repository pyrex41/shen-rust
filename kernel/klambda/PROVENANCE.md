# Kernel provenance

## ShenOSKernel 42.2 (community `shen-sources`) — current

Every `.kl` file in this directory is a byte-identical copy of `klambda/`
from the ShenOSKernel 42.2 release, the kernel that upstream
[shen-scheme 0.50](https://github.com/tizoc/shen-scheme/tree/v0.50) ships.

- Repository: https://github.com/Shen-Language/shen-sources
- Tag: `shen-42.2`, commit `73507e069fe147aefb728d632c4ce40bc6a7b314`
- Release asset: `ShenOSKernel-42.2.tar.gz`, SHA-256
  `1d02273003654d34ec020ae67cb214ecec410dee3350419ff807d82b652678d8`
  (the checksum pinned in shen-scheme 0.50's Makefile)
- `(version)` is `"42.2"` (set by `shen.initialise` in `init.kl`)

`klambda/` is a generated release artifact (it is not tracked in the
`shen-sources` git tree), so the tarball is the source of truth for these
files; `diff -r` against the extracted tarball's `klambda/` is clean.
`../tests/`, `../stlib/` (from `lib/stlib/`), `../README.md` and
`../LICENSE.txt` come from the same tarball and also match the `shen-42.2`
git tree.

Vendored files (23):

    core.kl          declarations.kl  dict.kl          init.kl
    load.kl          macros.kl        prolog.kl        reader.kl
    sequent.kl       stlib.kl         sys.kl           t-star.kl
    toplevel.kl      track.kl         types.kl         writer.kl
    yacc.kl
    extension-expand-dynamic.kl            extension-features.kl
    extension-launcher.kl                  extension-namespaces.kl
    extension-programmable-pattern-matching.kl
    extension-type-annotations.kl

### Lineage choice

Option (a) of pyrex41/shen-rust#28 (umbrella pyrex41/bifrost#26,
pyrex41/yggdrasil#32): adopt the community ShenOSKernel 42.2 **wholesale**,
replacing Mark Tarver's S42 lineage (below). This matches shen-scheme 0.50
byte-for-byte, at the cost of retiring the Tarver-lineage loader work.

### Boot (see `crates/shen-rust/src/interp/boot.rs`)

The 42.2 `.kl` files contain only `defun`s, so load order is cosmetic again;
`KERNEL_FILES` uses shen-scheme 0.50's `*shen-files*` order. Initialisation
is explicit, as in shen-scheme 0.50's `*init-code*`:

1. `(shen.initialise)` (`init.kl`) after every file is loaded — sets
   `*property-vector*` (a `shen.dict`, `dict.kl`), the arity and lambda
   tables, signed functions and `*version*`.
2. Primitive arity / lambda-form metadata, AOT install, native overrides.
3. `shen.x.programmable-pattern-matching.initialise`,
   `shen.x.namespaces.initialise`, then `stlib.initialise` (the precompiled
   standard library's arities, lambda forms, macros, synonyms, datatypes,
   types and sources).

Booted extensions: `features`, `launcher`, `programmable-pattern-matching`
and `namespaces` (shen-scheme 0.50's set), plus `expand-dynamic` (retained
from this port's previous boot list). `extension-type-annotations.kl` is
vendored and AOT-generated for audit completeness but not booted (shen-scheme
0.50 does not bundle it). `shen.x.features.initialise` is not called: this
port provides `shen.x.features.current` natively.

### Port-upgrade audit (`doc/port-upgrades.md`, 42.0–42.2)

- 42.0 `update-lambda-table` / lambda-form invalidation: shen-rust keeps no
  cache of callable forms; `fn` is the kernel's own (`reader.kl`) lookup of
  `shen.lambda-form` on `*property-vector*`, which `update-lambda-table`
  `unput`s for zero/unknown arity.
- 42.1 source-form handlers / `shen.macroexpand-h` 4th argument /
  `shen.try-parse`: shen-rust overrides neither the reader, `macroexpand`
  nor the read loop (only byte-level stream primitives, `pr`, and
  `read-file-as-bytelist`/`read-file-as-string`, which return the same
  values as the kernel's), so the kernel's 42.1 read loop runs unchanged,
  including input at EOF without a trailing newline.
- 42.2 `extension-namespaces.kl`: vendored from the 42.2 release (the
  regenerated version).

## Regeneration

The AOT kernel modules in `crates/shen-rust/src/aot/kernel/*.rs` are
generated from these files via `scripts/codegen-kernel-aot.sh` and
byte-frozen by the Gate 6 audit (`scripts/kernel-aot-audit.sh`). After any
change here, regenerate and re-commit. The four `stlib.initialise-*` defuns
(large literal tables) are left tree-walked (`klcompile::KERNEL_SLOW_DEFUNS`).

---

## History: Shen 42.0 (S42), Mark Tarver — superseded

Vendored until the 42.2 upgrade. The 15 canonical `.kl` files were
byte-identical copies of Mark Tarver's S42 archive (`KLambda/`).

Canonical source: `pyrex41/shen-upstream`, tag `s42-pristine-20260825`.
Original archive: https://www.shenlanguage.org/Download/S42.zip
Archive SHA-256: `30abdc7e5a1e27b7a20109c1ed141e4712885e31f24d9710d16415fbbd4dfb23`

Canonical files: `backend.kl`, `core.kl`, `declarations.kl`, `load.kl`,
`macros.kl`, `prolog.kl`, `reader.kl`, `sequent.kl`, `sys.kl`, `t-star.kl`,
`toplevel.kl`, `track.kl`, `types.kl`, `writer.kl`, and `yacc.kl`.

That lineage has no `init.kl`/`shen.initialise`, `dict.kl` or `stlib.kl`:
`declarations.kl` and `types.kl` initialised the kernel through top-level
forms during load (so boot order followed S42 `install.lsp`), `put`/`get`
stored pointer lists directly on `*property-vector*`, and the standard
library was loaded from Tarver's `Lib/StLib` Shen sources. `backend.kl` (a
`cl.*` KLambda→Common-Lisp backend) was vendored but not booted. Before S42
the port vendored Tarver's S41.2 refresh (`s41.2-pristine-20260711`), and
before that the community ShenOSKernel-41.2.
