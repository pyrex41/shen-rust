# Kernel provenance

## ShenOSKernel 43.1 (community `shen-sources`)

The 23 release files in this directory are the ShenOSKernel 43.1 artifact,
adopted from the verified upstream checkout used for this port upgrade.
`compiler.kl` is a pre-existing local Shen-CL backend retained as an explicit
port extra; it is not part of the 43.1 archive and is not in the Rust boot
list.

- Release tag: `shen-43.1`
- Release commit: `715824a52b55e9c81a9501df8548ff3edd8411f5`
- Release archive SHA-256: `900eedef44f440af78f9395ed06e689ce1604546fc309f5bc7c8bc84cc651920`
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
