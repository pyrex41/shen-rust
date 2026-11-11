# Lean wasm32 profile

Two Yggdrasil slices, built with `yggdrasil-build` for `wasm32-wasip1`, then
`wasm-opt -Oz`. Gzip is Python `gzip.compress(..., compresslevel=9, mtime=0)`.
Brotli is `-q 11`. Node times are the minimum of five
`compile + instantiate + wasi.start` runs (`scripts/wasm-profile-boot.mjs`).

The AOT-only scaffold uses `opt-level = "z"`, thin LTO, `codegen-units = 1`.
Rows marked opt 3 override that with `CARGO_PROFILE_RELEASE_OPT_LEVEL=3` so
each lowering can be compared without the size profile mixed in. `--full`
scaffolds stay at opt 3. This repo's workspace release profile stays at opt 3.

`./scripts/wasm-profile.sh` rebuilds one fixture (full and lean) and checks
wasm stdout against the native lean binary. `FIXTURE` selects the slice
(default `examples/wasm-profile/fib`).

## What changed, in order

The baseline is the lean image after the static arity / lambda-form tables:
registration still inlined into `main`, each defun still carrying its own
`format!` and `match` on `is_truthy`. That is the image behind the 110 KB
`main`, 23 KB `shen.initialise`, 23 KB `ix.activate`, and 11 KB
`ix.audience-allows?` figures. It was measured on the PR 35 branch after
that PR merged, so `main` itself is one step earlier (tables still code,
`sha2` still linked). Both are listed.

Each later row adds one lowering and keeps the earlier ones. Opt 3 until
the last row.

| step | fib raw | gzip | brotli | Node | catalyst raw | gzip | brotli | Node |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `main` (PR 35): tables as code, `sha2` linked | 436,471 | 118,370 | 91,047 | 10.53 ms | — | — | — | — |
| 0. tables as data, `sha256` off | 388,110 | 107,725 | 84,339 | 8.70 ms | 507,946 | 128,010 | 97,761 | 16.71 ms |
| 1a. outline `register_native` / `register_aot_direct` | 322,716 | 105,234 | 83,457 | 6.17 ms | 418,701 | 124,957 | 96,827 | 7.08 ms |
| 1b. one `install_fns` table | 318,238 | 103,727 | 82,838 | 6.08 ms | 410,459 | 122,577 | 95,397 | 7.25 ms |
| 2. shared helpers (`list_ref`, `headed_list`, arity, `cond`) | 308,927 | 102,101 | 81,476 | 6.06 ms | 371,613 | 116,305 | 91,474 | 7.00 ms |
| 3. literal `set` runs as `set_lits` | 293,521 | 100,203 | 80,727 | 5.93 ms | 357,396 | 114,798 | 90,722 | 6.81 ms |
| shipping: same as 3, `opt-level = "z"` | 242,510 | 91,368 | 76,477 | 6.10 ms | 287,293 | 100,282 | 83,096 | 7.09 ms |

The catalyst column starts at step 0 because that slice was not in the PR 35
tree. Its full-engine build on this host (wasip1, opt 3, thin LTO, Cedar
4.14) is 4,890,731 / 1,513,184 / 986,352, Node 28.84 ms. Fib's matching
full-engine build is 4,775,021 / 1,497,505 / 975,639, Node 26.10 ms.

Shipping boot, best of five:

| slice | compile | instantiate | run | total |
|---|---:|---:|---:|---:|
| fib | 0.83 ms | 1.45 ms | 3.81 ms | 6.10 ms |
| catalyst | 0.91 ms | 1.40 ms | 4.78 ms | 7.09 ms |

Stdout is unchanged: fib prints `fib 20 = 6765`, and the catalyst driver
prints `[ix.profile "Ada" "London" members true]`. Both match the native
`opt-level = "z"` binary. The same list is what ShenScript's
`domain-runtime.mjs` returns (44,224 raw / 12,280 gzip / 10,893 brotli).

## Why the AOT defuns were fat

`ix.activate` is one `cond` clause: a Shen pattern match lowered to KL as
nested `and` of `cons?`, `(= tag (hd …))`, and `(hd (tl (tl …)))`. The
generator used to emit a fresh walk and a `match` on `is_truthy` at every
test. That is why one function was 23 KB, about half the ShenScript file.

Lean emission, behind `CompileOptions::lean_emit` (off for
`CompileOptions::kernel()`, so Gate 6 stays a byte-for-byte kernel diff):

- `(hd (tl^n x))` becomes `rt::list_ref`. The error path lives in that
  helper.
- a proper list with a fixed head symbol becomes `rt::headed_list` (one
  walk, false on a short or untagged list, which is what the `and`
  short-circuit did).
- arity failures and `cond` fallthrough call `rt::arity_mismatch` and
  `rt::cond_failed` instead of a per-function `format!`.
- tail `if` / `and` / `or` / `cond` use `?`. The body is the whole `loop`,
  so returning the error is the same as `break Err`.

`and` / `or` / `if` stay special forms. A curried `apply_direct` would not
short-circuit. A bytecode encoding of cold defuns was weighed and skipped:
the lean image exists to drop the interpreter, and the bulk that looked
like "bytecode would help" was the duplicated pattern expansion above.

`shen.initialise` still had straight-line `(set NAME literal)` after the
lambda-form table. Runs of four or more become `rt::set_lits`. Sets whose
value is a call (`(shen.dict 20000)`) stay code.

## What the shipping bytes are

Twiggy on `wasm-opt -Oz --debuginfo`. The name subsection is not in the
stripped sizes above. These are the function bodies.

Catalyst, stripped 287,293 (`opt-level = "z"`):

| item | bytes | was (step 0, opt 3) |
|---|---:|---:|
| `<f64 as Display>` | 10,568 | 14,025 |
| `main` | 7,728 | 109,691 |
| `dlmalloc` | 5,913 | 5,913 |
| `ix.activate` | 5,521 | 22,609 |
| `ix.audience-allows?` | 3,418 | 11,008 |
| `shen.initialise` | 3,110 | 23,475 |

Fib, stripped 242,510:

| item | bytes | was (step 0, opt 3) |
|---|---:|---:|
| `<f64 as Display>` | 10,492 | 13,951 |
| `main` | 7,852 | 82,316 |
| `dlmalloc` | 5,913 | 5,913 |
| `shen.change-pointer-value` | 4,399 | 5,231 |

No Cedar, no `eval_in`, no `sha2`.

## What still separates this from the ShenScript file

The JS module is 44 KB because the shaken program is data plus a small
runtime. The shipping wasm module is 287 KB raw / 100 KB gzip.

The single functions that used to dominate are now ordinary: `main` is
8 KB, `shen.initialise` is 3 KB, `ix.activate` is 6 KB. What remains is
spread across the runtime and the other AOT bodies:

- `<f64 as Display>` is the largest function (about 11 KB). It is pulled
  in by `Value`'s debug formatting. Cutting it means a `Value` error path
  that does not format floats. Left in place; the win is real but it is
  the formatting Reuben expected to keep.
- `dlmalloc` is 6 KB and did not move.
- Heap growth, the symbol interner, and the WASI imports have no
  counterpart in the JS artifact.
- The rest is hundreds of small AOT functions (`shen.dict`,
  `shen.iter-vector`, `ix.join-rule`, and the kernel helpers the slice
  reached). Each is much smaller than before. A second bytecode VM for
  the cold ones would put an interpreter back into the image the shake
  asked to leave out.

## Fixtures

`examples/wasm-profile/fib` is ShenScript's published fib shake (54 kernel
defuns, `cannot-reach=eval`).

`examples/wasm-profile/catalyst` is a shake of `frontend/browser.shen`
(domain, profile types, profile command, profile boundary). Yggdrasil on
this port's ShenOSKernel 43.4 produced the same manifest as the 43.1 shake
except `kernel-version` (78 kernel defuns, `cannot-reach=eval`,
`needs-eval=false`). `driver.kl` is a harness, not part of the shake: it
writes `(ix.frontend.profile-command "Ada" "London" members true)` with
`write-byte`, because the shaken kernel has `shen.app` and not `pr`.
