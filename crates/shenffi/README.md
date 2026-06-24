# shenffi — Swift / C bindings for shen-rust

A thin C-ABI wrapper that embeds the [shen-rust](../shen-rust) interpreter in
non-Rust hosts — primarily **Swift on iOS/macOS**. shen-rust runs the Shen
kernel via AOT-compiled native code + a bytecode VM (no JIT in the default
build), so it's App Store-safe: it links into your app as a static library, no
runtime codegen.

## The C surface (`include/shenffi.h`)

```c
ShenCtx *shen_boot(const char *kernel_dir);   // NULL -> auto-locate kernel
char    *shen_eval(ShenCtx *ctx, const char *src);  // -> rendered result / "error: …"
void     shen_string_free(char *s);
void     shen_free(ShenCtx *ctx);
```

`shen_eval` runs a **Shen-level** expression through the kernel's own pipeline
(`read-from-string` → `head` → `eval`) and renders the result with `shen.app`,
so output matches the REPL.

## Swift wrapper (`swift/ShenRust.swift`)

```swift
let shen = try ShenRust(kernelDir: bundleKernelPath)   // nil to auto-locate
print(shen.eval("(map (/. X (* X X)) [1 2 3 4])"))     // -> [1 4 9 16]
```

## Build

**macOS (desktop / quick test):**
```sh
cargo build --release -p shenffi          # -> target/release/libshenffi.{a,dylib}
# round-trip demo:
cd crates/shenffi
swiftc -O -import-objc-header include/shenffi.h swift/ShenRust.swift swift/main.swift \
  -L ../../target/release -lshenffi -o /tmp/shenrust_demo
DYLD_LIBRARY_PATH=../../target/release /tmp/shenrust_demo
```

**iOS (device + simulator XCFramework):**
```sh
crates/shenffi/build-xcframework.sh        # -> target/ShenRust.xcframework
```
Then in Xcode: add `ShenRust.xcframework`, set `shenffi.h` as the bridging
header (or wrap in a module map), and drop in `swift/ShenRust.swift`.

## Kernel on iOS

`shen_boot(NULL)` auto-locates the kernel by walking the filesystem — fine on
desktop, not on a sandboxed device. Two device-friendly options:

1. **Bundle the `.kl` kernel** as an app resource and pass its directory path to
   `shen_boot(path)`.
2. **Embed a shaken `kernel.kl`** (produced by [ratatoskr](../../ratatoskr)) and
   boot from the in-memory source via shen-rust's `boot_from_kl_source` — no
   filesystem dependency. (Exposing that through the C ABI is a small addition;
   the current `shen_boot` takes a directory path.)

## Embedding a tree-shaken Shen program

`shen_boot_shaken(kernel_kl, prog_kl)` boots an arbitrary Ratatoskr-shaken slice
— a minimal `kernel.kl` (only the kernel functions the program reaches) plus an
optional program `.kl` — straight from in-memory source, no filesystem access.
This is how an app embeds a specific Shen program without the full kernel.

For a worked end-to-end example (the **shen-cas** computer-algebra system shaken
by [ratatoskr](../../ratatoskr), wrapped as a `CasEngine` + a `shen_cas_*` C ABI,
and driven from both an iced GUI and SwiftUI), see the **`cas-engine` crate in
the [shen-calc](../../../shen-calc) repo** — the app that consumes it. shenffi
itself stays program-agnostic.

## Status

Verified: the full shen-rust + shenffi stack **compiles for `aarch64-apple-ios`
and `aarch64-apple-ios-sim`**, packages into an `.xcframework`, and the
Swift→Rust→Shen round-trip runs correctly on macOS. The default shen-rust build
has **no JIT** (the optional Cranelift JIT is off by default), so nothing relies
on runtime code generation.
