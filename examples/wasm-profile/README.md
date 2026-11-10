# Lean wasm32 profile

Two Yggdrasil slices, built with `yggdrasil-build` for `wasm32-wasip1`, then
`wasm-opt -Oz`. Gzip is `-9`. Brotli is `-q 11`. Node times are the minimum
of five `compile + instantiate + wasi.start` runs
(`scripts/wasm-profile-boot.mjs`).

Release profile: `opt-level = 3`, thin LTO, `codegen-units = 1`.

## Fib

`examples/wasm-profile/fib` is ShenScript's published fib shake (54 kernel
defuns, `cannot-reach=eval`). Stdout is `fib 20 = 6765\n`.

| build | raw | gzip | brotli | Node total |
|---|---:|---:|---:|---:|
| before: default features, straight-line `shen.initialise` | 4,775,021 | 1,497,505 | 975,639 | 26.10 ms |
| lean, tables still compiled as code, `sha2` linked | 436,471 | 118,370 | 91,047 | 10.53 ms |
| after: tables as data, `sha256` feature off | 388,110 | 107,725 | 84,339 | 8.70 ms |

Fastest after run: compile 1.76 ms, instantiate 1.35 ms, run 5.59 ms.

`shen.initialise` went from 53,903 bytes of straight-line code to 18,678.
The arity list, the external-symbol list, and `shen.*lambdatable*` are
`rt::lit_list` / `rt::lambda_alist` over static tables. Host SHA-256 is a
default feature (`sha256`) and is left off when the manifest never mentions
it; that drops `sha2::sha256::compress256` (7,905 bytes).

## Catalyst profile slice

`examples/wasm-profile/catalyst` is a shake of `frontend/browser.shen` (the
browser entry: domain, profile types, profile command, profile boundary).
Yggdrasil on this port's ShenOSKernel 43.4 produced the same manifest as the
43.1 shake except `kernel-version` (`78` kernel defuns, `cannot-reach=eval`,
`needs-eval=false`). `driver.kl` is a harness, not part of the shake: it
prints `(ix.frontend.profile-command "Ada" "London" members true)`.

That call returns `[ix.profile "Ada" "London" members true]` on the native
lean binary, on every wasm build below, and from the ShenScript module
(`shen-reference/web/domain-runtime.mjs` in the source repo: 44,224 raw /
12,280 gzip / 10,893 brotli).

| build | raw | gzip | brotli | Node total |
|---|---:|---:|---:|---:|
| before: default features | 4,890,731 | 1,513,184 | 986,352 | 28.84 ms |
| lean, lambda forms still compiled as closures | 531,195 | 130,385 | 99,072 | 14.55 ms |
| after: lambda-form runs and literal spines as data | 507,946 | 128,010 | 97,761 | 16.71 ms |

Fastest after run: compile 2.09 ms, instantiate 1.38 ms, run 13.24 ms.
Fastest before run: compile 7.14 ms, instantiate 1.63 ms, run 20.07 ms.

The 43.4 initialiser is one `do` that mixes ordinary `set`s with about 40
`(shen.set-lambda-form-entry …)` steps. Consecutive runs of those steps
become one `rt::install_lambda_forms` table. `and` / `or` / `if` entries stay
compiled, because a curried `apply_direct` would not short-circuit. The
table cut is about 23 KB here; most of the lean module is the domain
program and boot, not the tables.

An earlier probe of this slice (wasm32-unknown-unknown, `opt-level = "z"`,
fat LTO, cedar-policy 4.11, before Cedar was optional) was 2,925,235 raw /
1,022,889 gzip / 707,833 brotli after wasm-opt. The before row above is the
same scaffold on the current wasip1 / opt-3 / thin-LTO / cedar 4.14 build.

## What the after bytes are

Twiggy on `wasm-opt -Oz --debuginfo` (the name subsection is not in the
stripped sizes above).

Fib, stripped 388,110:

| item | bytes |
|---|---:|
| `main` (boot and `install` inlined) | 82,316 |
| `shen.initialise` | 18,678 |
| `<f64 as Display>` | 13,951 |
| `dlmalloc` | 5,913 |
| `shen.change-pointer-value` | 5,231 |

Catalyst, stripped 507,946:

| item | bytes |
|---|---:|
| `main` (boot and `install` inlined) | 109,691 |
| `shen.initialise` | 23,475 |
| `ix.activate` | 22,609 |
| `<f64 as Display>` | 14,025 |
| `ix.audience-allows?` | 11,008 |
| `dlmalloc` | 5,913 |

No Cedar, no `eval_in`, no `sha2`.

## What still separates this from the ShenScript file

The JS module is 44 KB because the shaken program is data plus a small
runtime. The wasm module is an order of magnitude larger for three reasons
that are not the tables anymore:

- `main` is 80–110 KB. Thin LTO inlines `Interp::new`, primitive
  registration, and every AOT `install_*` into it. Outlining those calls
  does not remove the code.
- Each AOT defun is fatter than the same function in JS. `ix.activate`
  alone is 23 KB, about half the entire ShenScript file.
- Float formatting, `dlmalloc`, and the WASI imports have no counterpart
  in the JS artifact. `shen.initialise` is still ~19–23 KB of ordinary
  `set`s plus the few lambda forms that are real special forms.

`./scripts/wasm-profile.sh` rebuilds the fib fixture. The catalyst image
is `yggdrasil-build examples/wasm-profile/catalyst <outdir>`.
`YGGDRASIL_NO_LITERAL_TABLES=1` keeps the straight-line tables for the
comparison above.
