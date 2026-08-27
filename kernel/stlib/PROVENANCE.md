# Standard library provenance

These files are sourced from Mark Tarver's Shen 42.0 archive, `Lib/StLib/`.
Canonical source: `pyrex41/shen-upstream`, tag `s42-pristine-20260825`.
Original archive: https://www.shenlanguage.org/Download/S42.zip
Archive SHA-256: `30abdc7e5a1e27b7a20109c1ed141e4712885e31f24d9710d16415fbbd4dfb23`

The complete S42 `Lib/StLib` tree is vendored so `install.shen` and optional
library modules retain upstream behavior. `boot::load_stlib` rewrites the
relative paths in `install.shen` and loads the sources after the kernel.
