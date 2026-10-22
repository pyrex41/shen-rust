# Kernel provenance

## ShenOSKernel 43.1 (community `shen-sources`)

The 23 release files in this directory are ShenOSKernel 43.1 as built at
upstream `Shen-Language/shen-sources` master `ec16a9f`: the 43.1 release plus
the type checker's deref-once change (shen-sources#117), which adds the
`shen.by-hypothesis-h` and `shen.search-user-datatypes-h` predicates.
`compiler.kl` is a pre-existing local Shen-CL backend retained as an explicit
port extra; it is not part of the 43.1 archive and is not in the Rust boot
list.

- Upstream commit: `ec16a9f` (merge of shen-sources#117), on top of tag
  `shen-43.1` (`715824a52b55e9c81a9501df8548ff3edd8411f5`)
- Archive: `ShenOSKernel-43.1-ec16a9f.tar.gz`, unmodified upstream sources
  built with the repo's `make klambda`/`make release`, from
  https://github.com/pyrex41/shen-sources/releases/tag/upstream-ec16a9f
- Archive SHA-256: `060066a8f84d9a2f7d41cb3b37b986a8d4653279752b739b5063cd648d9baacd`
- Previous import: the `shen-43.1` release archive, SHA-256
  `900eedef44f440af78f9395ed06e689ce1604546fc309f5bc7c8bc84cc651920`
- `(version)` is `"43.1"`, set by `shen.initialise` in `init.kl`.

The 43.1 port contract exposes the public character stream functions
`char-stream?`, `read-unit-string`, and `write-string`. The Rust host keeps
its byte stream behavior and supplies the callable metadata required by the
kernel; character-stream operations report unsupported behavior where the
host has no character stream.

The kernel's normal initialization remains explicit: the official 23 KL files
are loaded, then `shen.initialise`, primitive metadata, AOT functions, native
overrides, extension initializers, and `stlib.initialise` are run by the Rust
boot path.
