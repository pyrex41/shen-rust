# Changelog

Notable changes to shen-rust. Earlier history is summarised in `STATUS.md`.

## Unreleased

## 0.1.2 — 2026-11-11

### Fixed

- **`value/or` is registered before `shen.initialise`.** Kernel 43.4's
  initialiser calls `bound?`. klcompile lowers that `trap-error` / `value`
  pair to `value/or`, and the same pass lowers `<-address` and `<-vector`
  traps to `<-address/or` and `<-vector/or`. Those three names are not
  kernel functions. On 0.1.1 they were installed in `register_hot_overrides`
  after `shen.initialise`, so a lean image (`default-features = false`, the
  shape `yggdrasil-build` emits for an eval-free shake) died at boot with
  `aot: undefined function value/or`. They are now part of
  `register_early_overrides`, which every boot path runs before
  `shen.initialise`. `boot_aot_slice` also installs the rest of the hot
  overrides before kernel top-level forms. A default native build is
  unchanged: the same overrides still shadow the AOT kernel after it is
  installed. Yggdrasil's `rust-aot-boot` step, which called
  `register_hot_overrides` before `boot_aot_slice`, is no longer required.

### Added

- **Lean-boot regression.** `cargo test -p shen-rust --test lean_boot`
  boots a slice whose initialiser and top-level form call `bound?` /
  `value/or` / `<-address/or` / `<-vector/or`, then runs `(fib 10)`.
  `./scripts/gates.sh` runs it with `--no-default-features`. CI job
  `lean boot` also runs it for `interpreter`, `kernel-aot`, `jit`, and
  the default features, and runs the `lean_fib` example on
  `wasm32-wasip1` for every combination except `jit`.

## 0.1.1 — 2026-11-10

First tagged release. The workspace version moves from `0.1.0` to `0.1.1`.
Nothing before this was tagged or published, so the notes below include
the kernel history that had been sitting under Unreleased, plus the lean
wasm feature gates from #35.

`0.1.1` is a patch, not `0.2.0`. The only version in the tree was `0.1.0`,
shared by every crate through `version.workspace = true`, and there is no
release workflow or crates.io publish. Cargo's pre-1.0 rule treats a
compatible change as a patch and a breaking change as a minor bump. #35
adds `cedar`, `kernel-aot`, and `interpreter`, and puts all three in
`default`, so a default native build is the same port as before.

### Added

- **Feature gates for a lean wasm link.** `cedar`, `kernel-aot`, and
  `interpreter` are Cargo features.
  `default = ["cedar", "kernel-aot", "interpreter"]`, so `cargo build`,
  `cargo test`, and `./scripts/gates.sh` still link Cedar, the precompiled
  full kernel, and the KL tree-walker. `jit` stays off by default and now
  implies `interpreter` and `kernel-aot`. `yggdrasil-build` emits
  `default-features = false` when the shake manifest says
  `cannot-reach=eval` or `needs-eval=false`, and adds `interpreter` only
  when eval is still reachable. `--full` keeps the default features.
- **Eval-free fib shake on `wasm32-wasip1`** (opt-level 3, thin LTO, then
  `wasm-opt -Oz`). Raw wasm ~4.8 MB → ~436 KB; brotli ~91 KB. Node
  compile + instantiate + start ~26 ms → ~10.5 ms. Measured on the
  committed fixture: 4,775,021 → 436,471 bytes raw, 975,639 → 91,047
  brotli, 26.10 → 10.53 ms total. See `examples/wasm-profile/README.md`.
- On `wasm32`, `*home-directory*` is `"/"` when the host has no process
  cwd. The wasm entry calls `run` directly and does not spawn the 1 GB
  thread.

### Changed

- `(port)` and `(release)` follow the crate version
  (`CARGO_PKG_VERSION`), so this release reports `0.1.1`. `*version*`
  stays the kernel version, `"43.4"`.
- **Kernel: ShenOSKernel 43.4.** The vendored klambda is the community
  `Shen-Language/shen-sources` release `shen-43.4` (commit
  `f34b307b490880eedcfe1f5e5d3cc24ca3d2602e`), `ShenOSKernel-43.4.tar.gz`
  SHA-256 `15e7972e6bf0cd65d37c2eba85750b8839d2dbadda41027eae0c88e5017a2756`.
  17 of 22 `.kl` files change (defuns 1117 -> 1126), including `stlib.kl`,
  which must move with the kernel. `kernel/tests/` gains `regressions/` and
  `extensions/expand-dynamic/`. `(version)` is `"43.4"`. The kernel AOT modules
  are regenerated with `scripts/codegen-kernel-aot.sh`.
- **Kernel: ShenOSKernel 43.2.** The vendored klambda is the community
  `Shen-Language/shen-sources` release `shen-43.2` (commit `e6947f507ebc`),
  `ShenOSKernel-43.2.tar.gz` SHA-256
  `23c71508f0de24785e1f127ea4fab440081904d81c463090bc642ee840417f32`.
  Against the previous `ec16a9f` import only `init.kl` (`*version*`) and
  `extension-type-annotations.kl` (renumbered generated names) differ;
  `kernel/tests/benchmark-coverage.shen` is added. `(version)` is `"43.2"`.
  The kernel AOT modules are regenerated with `scripts/codegen-kernel-aot.sh`.
- **Kernel: ShenOSKernel 43.1 at upstream `ec16a9f`.** The vendored klambda
  is now built from `Shen-Language/shen-sources` `ec16a9f`, the 43.1 release
  plus shen-sources#117: `shen.by-hypothesis` and
  `shen.search-user-datatypes` dereference the checked term once per call
  rather than once per hypothesis/datatype rule (b-typed: about -65%
  instructions). `ShenOSKernel-43.1-ec16a9f.tar.gz` SHA-256
  `060066a8f84d9a2f7d41cb3b37b986a8d4653279752b739b5063cd648d9baacd`;
  `(version)` stays `"43.1"`. The kernel AOT modules are regenerated with
  `scripts/codegen-kernel-aot.sh`. See `kernel/klambda/PROVENANCE.md`.
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
