# Kernel provenance

## ShenOSKernel 43.1 (community `shen-sources`)

The files in this directory are the ShenOSKernel 43.1 release artifact,
adopted wholesale from the verified upstream checkout used for this port
upgrade.

- Release tag: `shen-43.1`
- Release commit: `715824a52b55e9c81a9501df8548ff3edd8411f5`
- Release archive SHA-256: `900eedef44f440af78f9395ed06e689ce1604546fc309f5bc7c8bc84cc651920`
- `(version)` is `"43.1"`, set by `shen.initialise` in `init.kl`.

The 43.1 port contract exposes the public character stream functions
`char-stream?`, `read-unit-string`, and `write-string`. The Rust host keeps
its byte stream behavior and supplies the callable metadata required by the
kernel; character-stream operations report unsupported behavior where the
host has no character stream.

The kernel's normal initialization remains explicit: all KL files are loaded,
then `shen.initialise`, primitive metadata, AOT functions, native overrides,
extension initializers, and `stlib.initialise` are run by the Rust boot path.
