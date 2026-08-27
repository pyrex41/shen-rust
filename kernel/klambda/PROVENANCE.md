# Kernel provenance

## Shen 42.0 (S42), Mark Tarver

The 15 canonical `.kl` files are byte-identical copies of Mark Tarver's S42
archive (`KLambda/`).

Canonical source: `pyrex41/shen-upstream`, tag `s42-pristine-20260825`.
Original archive: https://www.shenlanguage.org/Download/S42.zip
Archive SHA-256: `30abdc7e5a1e27b7a20109c1ed141e4712885e31f24d9710d16415fbbd4dfb23`

Canonical files: `backend.kl`, `core.kl`, `declarations.kl`, `load.kl`,
`macros.kl`, `prolog.kl`, `reader.kl`, `sequent.kl`, `sys.kl`, `t-star.kl`,
`toplevel.kl`, `track.kl`, `types.kl`, `writer.kl`, and `yacc.kl`.

The `extension-*.kl` files are Shen port extensions and remain outside the
canonical inventory. `backend.kl` is generated for audit completeness but is
not booted, matching upstream's precompiled Common Lisp backend behavior.

Boot order follows S42 `install.lsp`; the standard library is loaded from
`kernel/stlib` sources after the kernel.
