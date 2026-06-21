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

## Embedding a tree-shaken Shen program (shen-cas)

The `cas/` directory holds a worked example: the **shen-cas computer algebra
system**, tree-shaken by [ratatoskr](../../ratatoskr) and embedded in the binary.

Pipeline:
1. Flatten shen-cas's modules into one `.shen` (strip its `(load …)` directives).
2. `ratatoskr shake cas-all.shen out/` → `kernel.kl` (only the kernel functions
   the CAS reaches — 298 KB vs the 749 KB full kernel) + `cas-all.kl`.
3. `include_str!` both into the crate; `shen_cas_boot` boots the slice via
   `boot_from_kl_source` (kernel + `shen.initialise`) then loads the CAS program.
4. `shen_cas_reduce` calls the CAS's own `parse-expr-string → reduce →
   pretty-expr` — no Shen-level `eval` required, so eval-stripping is fine.

```c
ShenCtx *shen_cas_boot(void);
char    *shen_cas_reduce(ShenCtx *ctx, const char *expr);  // "D[Sin[x],x]" -> "[Cos x]"
```

Verified Swift → Rust → CAS results (`swift/cas-demo.swift`):

```
D[Sin[x],x]  =>  [Cos x]
D[x^3,x]     =>  [Times [Power x 2] 3]
D[Exp[x],x]  =>  [Exp x]
6/4          =>  [3 / 2]
```

> The CAS reducer is deeply recursive and runs tree-walked, so call the FFI from
> a thread with a large stack (the demo uses a 512 MB `Thread`; the shen-rust CLI
> does the same). This is the same reason an app should call Shen off the main
> thread.

## Status

Verified: the full shen-rust + shenffi stack **compiles for `aarch64-apple-ios`
and `aarch64-apple-ios-sim`**, packages into an `.xcframework`, and the
Swift→Rust→Shen round-trip runs correctly on macOS. The default shen-rust build
has **no JIT** (the optional Cranelift JIT is off by default), so nothing relies
on runtime code generation.
