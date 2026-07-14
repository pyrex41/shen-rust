# Standard library provenance

These are Mark Tarver's **S41.2 (2026-07-11 refresh)** standard-library
sources — the `Lib/StLib` tree of the refreshed Shen release. They are the
stdlib **source of truth** for this port, loaded at boot by
`crates/shen-rust/src/interp/boot.rs` (`load_stlib`), which runs upstream's
`install.shen`. This **retires the old community `stlib.kl` overlay**
(pre-compiled KLambda) that the port previously vendored under
`kernel/klambda/`.

## Why sources, not the compiled overlay

The community `ShenOSKernel-41.2` shipped the stdlib as a pre-compiled
`stlib.kl`. Installing those `defun`s bypassed the kernel's `load`/`define`
path, so stdlib functions ended up with the "unknown" arity `-1`: bare
`(fn filter)` and top-level `(filter …)` failed with
`fn: filter is undefined`. Loading the `.shen` sources through the real
`load` (as upstream's `install.shen` does) registers correct arities and
package namespacing, fixing that. Regression:
`crates/shen-rust/tests/library.rs::stdlib_loaded_from_source_has_real_arities`.

## Canonical source

- Repo: `pyrex41/shen-upstream` (the designated mirror of Tarver's uploads;
  formerly `pyrex41/shen-s41.1` — old URLs redirect)
- Tag: `s41.2-pristine-20260711`
- Commit: `11fc51bdf53a4dcb505adeec6ec8352754cbe50f`
- Path in mirror: `Lib/StLib/`

All files here are `diff`-verified byte-identical to that path at that tag
(22 files). Original upstream: the `Lib/StLib` directory of
`https://www.shenlanguage.org/Download/S41.2.zip` (Last-Modified
2026-07-11, zip sha256 `51becbf…3ee836`; see
`../klambda/PROVENANCE.md`).

## What is vendored

Exactly the files `install.shen` loads (plus `install.shen` and
`package-stlib.shen`): `Symbols/`, `Maths/` (incl. the `.dtype`
datatypes), `Lists/`, `Strings/`, `Vectors/`, `IO/`, `Tuples/`. The
StLib extras not referenced by `install.shen` (`Calendar/`, `Data/`,
`Maths/r.shen`, `Strings/regex.shen`, `Strings/smartmem.shen`) are **not**
vendored.

## Load mechanism

`load_stlib` rewrites `install.shen`'s relative `(load "…")` paths to
absolute paths under this directory (into a per-boot temp file) and loads
that, so no process-cwd mutation is needed and concurrent interpreter
boots stay re-entrant. The load runs under `*hush*` to keep the per-file
"…loaded"/type-signature chatter off the boot path.

Note: package-internal (non-exported) stdlib functions are namespaced —
e.g. `reduce` lives inside `(package list …)` and is reachable as
`list.reduce`, not bare `reduce`. Exported names (`filter`, `sq`, …) are
global. Because exported stdlib functions become **system functions**, a
user `(define …)` over one is refused by the kernel (as on shen-cl).

## Follow-up

`shen_boot_embedded` (shenffi, no-filesystem iOS path) does not yet bundle
these sources, so that path brings up the kernel without the stdlib —
embedding the StLib sources for the no-fs boot is future work.
