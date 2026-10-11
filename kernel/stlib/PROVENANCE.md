# Standard library provenance

## ShenOSKernel 42.2 `lib/stlib/` — current

These files are byte-identical copies of `lib/stlib/` from the ShenOSKernel
42.2 release (`Shen-Language/shen-sources`, tag `shen-42.2`, commit
`73507e069fe147aefb728d632c4ce40bc6a7b314`; `ShenOSKernel-42.2.tar.gz`
SHA-256 `1d02273003654d34ec020ae67cb214ecec410dee3350419ff807d82b652678d8`).
They are the sources that release's `make-stlib.shen` compiles into
`../klambda/stlib.kl`.

They are **not loaded at boot**. Like shen-scheme 0.50, shen-rust boots the
precompiled `../klambda/stlib.kl` and calls `stlib.initialise` (see
`../klambda/PROVENANCE.md`). The sources are vendored so the library that
`stlib.kl` was built from is in-tree and consistent with it; running
`install.shen` from this directory reloads it from source.

## History: Tarver S42 `Lib/StLib` — superseded

Previously this directory held Mark Tarver's Shen 42.0 archive `Lib/StLib/`
(`pyrex41/shen-upstream`, tag `s42-pristine-20260825`, S42.zip SHA-256
`30abdc7e5a1e27b7a20109c1ed141e4712885e31f24d9710d16415fbbd4dfb23`), loaded
at boot by `boot::load_stlib`, which ran `install.shen` with rewritten
paths. That loader was retired with the move to ShenOSKernel 42.2.
