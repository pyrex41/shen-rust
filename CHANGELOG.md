# Changelog

Notable changes to shen-rust. Earlier history is summarised in `STATUS.md`.

## Unreleased

### Changed

- **Kernel: ShenOSKernel 42.2** (pyrex41/shen-rust#28). The vendored kernel
  is now the community ShenOSKernel 42.2 from `Shen-Language/shen-sources`
  (tag `shen-42.2`, commit `73507e069fe147aefb728d632c4ce40bc6a7b314`,
  `ShenOSKernel-42.2.tar.gz` SHA-256
  `1d02273003654d34ec020ae67cb214ecec410dee3350419ff807d82b652678d8`), the
  kernel upstream shen-scheme 0.50 ships, replacing Mark Tarver's S42 lineage
  (`s42-pristine-20260825`). `(version)` is `"42.2"`. `kernel/klambda/`,
  `kernel/tests/`, `kernel/stlib/` (42.2 `lib/stlib/`) and `kernel/README.md`
  are byte-identical to the release. See `kernel/klambda/PROVENANCE.md`.
- Boot follows shen-scheme 0.50: load the defun-only `.kl` files (including
  `dict.kl`, `init.kl` and the precompiled `stlib.kl`), call
  `shen.initialise`, install AOT code and native overrides, then run
  `shen.x.programmable-pattern-matching.initialise`,
  `shen.x.namespaces.initialise` and `stlib.initialise`. The programmable
  pattern matching and namespaces extensions are now booted; the
  type-annotations extension is vendored but not booted.
- The standard library is booted from `stlib.kl` instead of loading the
  `kernel/stlib/` Shen sources (`boot::load_stlib` removed).
- The kernel's own `dict.kl`-based `put`/`get`/`unput` are used. The native
  Tarver pointer-list versions are only installed over a kernel that defines
  `shen.change-pointer-value` (e.g. a shaken Tarver-lineage slice).
- The kernel conformance suite is the 42.2 one: 143 tests.
- `shenffi`'s embedded (no-filesystem) boot embeds the same file set and runs
  the same post-boot initialisers.

### Removed

- `backend.kl` (Tarver's `cl.*` backend; never booted) and its AOT module.
