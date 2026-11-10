# Lean wasm32 profile

A Yggdrasil-shaken fib slice, built with `yggdrasil-build` for
`wasm32-wasip1`, then `wasm-opt -Oz`. The fixture is ShenScript's published
fib shake (`cannot-reach=eval`, `needs-eval=false`, 54 kernel defuns, 14 KB
of `kernel.kl`). The catalyst-finder profile slice was not readable from
this environment (the PR returned 404); this program is the same shape: a
pure eval-free slice whose stdout is `fib 20 = 6765`.

Rebuild:

```sh
./scripts/wasm-profile.sh
```

The script scaffolds two images from this fixture.

- **before / `--full`** — default `shen-rust` features. Boot parses the
  shaken KL and evaluates non-defun forms with the tree-walker. Cedar, the
  precompiled full kernel, and the interpreter are all available to the
  linker.
- **after / default** — `default-features = false`. `cannot-reach=eval`
  selects `boot_aot_slice`: AOT defuns only, no KL parse, no Cedar, no
  precompiled kernel, no tree-walker.

Both release profiles are `opt-level = 3`, thin LTO, `codegen-units = 1`.
Sizes below are after `wasm-opt -Oz` (bulk memory, multivalue, simd,
reference types, nontrapping float-to-int). Gzip is `-9`, brotli is `-q 11`.

The stock generated `main` spawns a 1 GB thread. WASI returns
`Unsupported` for that spawn, so the process never reaches
`set_home_directory`. The wasm entry calls `run` directly (`#[cfg(target_arch = "wasm32")]`).
The before column uses that same entry on the unmodified engine (commit
`c0cd646`) so the size diff is the link, not the thread. The thread itself
was about 5 KB.

`std::env::current_dir` succeeds under Node's default WASI preview1, so
this host does not need the `"/"` fallback. On `wasm32` the `Err` arm is
`"/"` only — the `getcwd: …` format string is not in the lean module — so
a host that does fail `current_dir` still boots.

## Sizes

| build | raw | gzip | brotli |
|---|---:|---:|---:|
| before (default features, tree-walked KL) | 4,775,021 | 1,497,505 | 975,639 |
| after (`cannot-reach=eval`, no default features) | 436,471 | 118,370 | 91,047 |

About 11× smaller raw, 13× smaller gzip. `panic = "abort"` did not move
the lean binary (wasm-opt had already dropped the unwind tables).

ShenScript's self-contained JS for a slice of this kind is about 44 KB raw
/ 12.3 KB gzip. The lean wasm module is still ~10× that gzip.

## Boot in Node

`scripts/wasm-profile-boot.mjs` compiles, instantiates, and `wasi.start`s
the module. Minimum of five runs, milliseconds:

| build | compile | instantiate | run | total |
|---|---:|---:|---:|---:|
| before | 7.38 | 1.63 | 17.09 | 26.10 |
| after | 1.10 | 1.34 | 8.09 | 10.53 |

Stdout of both wasm runs and of the native lean binary is the same bytes:
`fib 20 = 6765\n`.

## What the bytes were

Twiggy on `wasm-opt -Oz --debuginfo` (function bodies match the stripped
module; the name subsection is extra and is not in the size table above).

**Before.** Cedar is the module.

| group | bytes in the named module |
|---|---:|
| `cedar-policy` + lalrpop | 2,930,674 |
| `regex` (pulled in by Cedar) | 579,688 |
| tree-walker (`interp::eval`) | 34,907 |
| bytecode VM | 33,229 |
| KL parser | 11,744 |

The largest single function is a `cedar::primitives::register_all` closure
(269,134 bytes), then Cedar's LALRPOP `reduce` functions (~74 KB each) and
`regex::Regex::new` (67,699). The precompiled full kernel (`aot::kernel`,
~140k lines) is not in the binary: nothing in this scaffold calls
`install_all`, and thin LTO drops it. It still costs a wasm compile of
that crate unless the `kernel-aot` feature is off.

**After.** The shaken program and the Rust runtime.

| item | shallow bytes | share of the 436,471 stripped module |
|---|---:|---:|
| `main` (boot inlined) | 83,483 | 19.1% |
| `shen.initialise` | 53,903 | 12.3% |
| `<f64 as Display>::fmt` | 13,951 | 3.2% |
| `sha2::sha256::compress256` | 7,905 | 1.8% |
| `dlmalloc` | 5,913 | 1.4% |
| `shen.change-pointer-value` | 5,231 | 1.2% |

No `cedar` symbols, no `eval_in`, no `aot::kernel`. `sha2` stays because
`register_hot_overrides` always installs the host SHA-256. That is about
8 KB, not the gap to ShenScript.

## Why this is not yet a 44 KB module

- The shaken `shen.initialise` alone is 54 KB of wasm. It is one straight-line
  function: the arity table and the lambda table, compiled as code. ShenScript
  emits that as data. The whole JS file is smaller than this one function.
- `main` holds another ~83 KB of inlined boot (interner, apply, property
  setup). That is the engine, not the fib.
- Formatting (`Display` for floats), `dlmalloc`, and the WASI shims do not
  exist in the JS artifact. `panic = "abort"` does not remove them.
- Host SHA-256 (`sha2`) is still linked for `shen.x.sha256-octets-host`.

Getting near the ShenScript size means treating `shen.initialise`'s tables
as data rather than code, and shipping a smaller runtime than the current
`Value` / interner / `apply` path. Feature flags will not close that gap.
