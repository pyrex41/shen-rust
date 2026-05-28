# shen-cedar Status

## 2026-05-27 — Phase 1 done, Phase 2 starting

### Phase 0 ✓

- Workspace scaffolded; kernel vendored (21 `.kl` + tests); toolchain
  pinned; Nix dev shell; stub docs.

### Phase 1 ✓

- KL runtime in `crates/shen-cedar/src/`:
  - `value.rs`: `Value` enum with `Rc`-shared sharing, interned `SymId`
    symbols, `Foreign(Rc<dyn Any>)` for Cedar handles, `shen_eq` with
    Int↔Float coercion.
  - `symbol.rs`: hand-rolled `Interner` (string ↔ id), pre-interned
    well-known ids.
  - `env.rs`: dual-namespace environment + property table.
  - `error.rs`: `ShenError` flowing as `Result<Value, ShenError>` through
    eval; mapped to `Value::Error` for `trap-error` handlers.
  - `kl/parser.rs`: s-expression reader for `.kl` files. Handles
    block/line comments, multi-line strings (literal bytes — no
    backslash escapes, matching kernel reader semantics), Shen-style
    symbols with `<`, `>`, `?`, `.`, `@`, etc.
  - `kl/ast.rs`: `KlExpr` with `App(Rc<[KlExpr]>)` for cheap clones in
    the trampoline.
  - `interp/eval.rs`: tree-walking evaluator with **trampoline-based
    TCO** — survives `(loop 50000 0)` style deep recursion. Special
    forms (`if`, `let`, `lambda`, `defun`, `cond`, `do`, `and`, `or`,
    `freeze`, `thaw`, `trap-error`, `quote`, `type`) dispatched
    in-flight; non-tail recursion handled by `eval_in` re-entry.
  - `primitives.rs`: 38 of the 46 KL primitives registered. Arithmetic
    with Int/Float promotion, comparison, equality, lists (cons/hd/tl/
    cons?), symbols (intern/symbol?/fn), strings (cn/pos/tlstr/n->string/
    string->n/str/string?), vectors (absvector/<-address/address->/
    absvector?/vector?), globals (set/value), errors (simple-error/
    error-to-string), I/O streams (open/close/read-byte/write-byte),
    meta (eval-kl/type/tc/tc?/get-time/hash/apply). `hash` uses native
    `DefaultHasher` (avoids the kernel's integer-overflow issue).

### Tests passing (36 total)

- 16 unit tests (parser, symbol, value modules).
- 19 eval smoke tests (`tests/eval_smoke.rs`): arithmetic, conditionals,
  recursion, **tail-recursive 50,000-iteration loop**, partial
  application, `trap-error`, `freeze`/`thaw`, `set`/`value`, absvector
  round-trip, predicates.
- 1 kernel parse integration test (`tests/parse_kernel.rs`): all 21
  vendored `.kl` files parse to **3000+ top-level forms** without error.

### Gates passing

- `cargo build --workspace` ✓
- `cargo test --workspace` ✓ (36/36)
- `cargo fmt --all -- --check` ✓
- `cargo clippy --workspace --all-targets -- -D warnings` ✓

### Phase 2 ✓

- `interp/boot.rs`: loads all 21 kernel `.kl` files in the same order as
  `shen-ocaml`, sets port metadata (`*version*` = "41.1",
  `*implementation*` = "shen-cedar", `*language*` = "Rust", etc.), wires
  `*stinput*`/`*stoutput*`/`*sterror*` to host stdio, sets
  `*home-directory*`, runs `shen.initialise`, then publishes
  `arity` + `shen.lambda-form` for every primitive on the kernel's
  `*property-vector*` so `(fn NAME)` resolves.
- Bug fixed: locals shadow the function namespace in head position
  (`(F x y)` where `F` is a lambda parameter — kernel relies on this for
  higher-order calls like `shen.simple-curry`).
- `eval-kl` updated: non-list/symbol values (numbers, strings,
  absvectors, closures…) are self-evaluating per KL semantics.
  Necessary because `shen.fn-print` returns a print-vector that the
  kernel's own `eval` pipeline passes back through `eval-kl`.
- `bin/shen-cedar/src/main.rs`: REPL that routes each input through
  `(eval EXPR)` — the kernel's full pipeline: macroexpand →
  find-types → process-applications → shen->kl → eval-kl.
  Print-vectors render as `(fn NAME)`; cons cells render as
  parenthesised lists.

### Verified expressions (post-boot, via kernel eval)

All from `shen-ocaml/STATUS.md` plus a couple extras:

- `(+ 1 1)` → `2`
- `(value *version*)` → `"41.1"`
- `(value *implementation*)` → `"shen-cedar"`
- `(let X 5 (+ X 1))` → `6`
- `(hd (cons 1 (cons 2 ())))` → `1`
- `(defun double (X) (* X 2))` then `(double 21)` → `42`
- `(trap-error (simple-error "boom") (lambda E (error-to-string E)))` → `"boom"`
- `(tc +)` → `true` (type checker toggles on)
- `(fn +)` → `<closure>` (resolves via `*property-vector*`)
- `(eval (cons + (cons 1 (cons 1 ()))))` → `2` (full kernel pipeline)

### Tests passing (46 total)

- 16 unit (parser, symbol, value)
- 19 eval smoke
- 1 parse_kernel integration
- 10 boot_kernel integration ← new in Phase 2

Boot takes ~2 s; the boot_kernel suite reboots per test (`fresh_booted`)
so the total wall time is ~20 s. Single-shot boot in the REPL is
sub-second.

### Stretch / deferred

- Kernel test suite under `kernel/tests/` (`kerneltests.shen`,
  `harness.shen`) — relies on `(load "file.shen")` reading Shen-syntax
  files plus an interactive `y-or-n?` failure handler. Same status as
  `shen-ocaml`'s STATUS lists for this item.

### Phase 3 ✓

- `cedar-policy = "4"` (currently resolves to 4.11.0) added as a workspace
  dependency.
- `src/cedar/types.rs`: named downcasters for `Policy`, `PolicySet`,
  `Schema`, `Entities`, `Request`, `Authorizer`, `EntityUid` over
  `Value::Foreign(Rc<dyn Any>)`.
- `src/cedar/primitives.rs`: 15 `cedar.*` primitives —
  - `parse-policy`, `parse-policy-set`, `parse-schema`, `parse-entities`
  - `make-entity-uid TYPE ID` (builder that side-steps the KL parser's
    inability to embed quotes inside strings; takes type and id as
    separate strings)
  - `entity-uid->string`
  - `make-request PRINCIPAL ACTION RESOURCE CONTEXT`
  - `is-authorized PSET ENTS REQ` → `Allow` / `Deny` symbol
  - `is-authorized-detailed` → `(DECISION REASONS ERRORS)`
  - `validate PSET SCHEMA` → list of validation error strings
  - `policy->string`, `policy-set->string`
  - `empty-entities`, `empty-policy-set`, `policy-set-add`
- Metadata for every `cedar.*` primitive is published on
  `*property-vector*` post-boot via `CEDAR_PRIMITIVES`, so the kernel's
  own `fn` resolves them and `process-applications` doesn't transform
  Cedar calls into "undefined function" errors.
- `tests/cedar_smoke.rs`: 6 end-to-end tests proving
  `permit(...)` → `Allow`, `forbid(...)` → `Deny`, empty pset → `Deny`,
  `policy->string` round-trip.
- Verified end-to-end in the REPL post-boot:
  ```
  (set p (cedar.parse-policy "permit(principal, action, resource);"))
  (set pset (cedar.policy-set-add (cedar.empty-policy-set) (value p)))
  (set req (cedar.make-request
              (cedar.make-entity-uid "User" "alice")
              (cedar.make-entity-uid "Action" "read")
              (cedar.make-entity-uid "Doc" "d1")
              ()))
  (cedar.is-authorized (value pset) (cedar.empty-entities) (value req))
  → Allow
  ```

### Test totals (52 total)

| Suite               | Tests | Notes                                  |
|---------------------|------:|----------------------------------------|
| Unit                | 16    | parser, symbol, value                  |
| `eval_smoke`        | 19    | KL evaluator end-to-end                |
| `parse_kernel`      | 1     | 21 kernel files, 3000+ forms           |
| `boot_kernel`       | 10    | full kernel boot + verified expressions|
| `cedar_smoke`       | 6     | Cedar bridge                           |

All gates green: `cargo build`, `cargo test`, `cargo fmt --check`,
`cargo clippy -D warnings`.

### Phase 4 ✓

- `specs/core.shen`: 14 sequent-calculus datatype definitions ported
  from `shen-ocaml/specs/core.shen`.
- `crates/shengen-rust/src/main.rs`: text-only parser (NOT a Shen
  interpreter) that compiles `(datatype …)` blocks to Rust structs with
  private fields + fallible `pub fn new(...) -> Result<Self, String>`
  constructors that enforce every `(... : verified)` premise. Output is
  rustfmt-cleaned. ~430 LOC.
- `crates/shen-cedar/src/generated/guard_types.rs`: generated code with
  one `pub struct` per datatype. Private fields + factory pattern: the
  forgery boundary is exactly this file.
- `crates/shen-cedar/src/interp/guard_types_link.rs`: witness module
  that exercises every generated constructor + a representative
  accessor, pulled onto the boot path so any shengen-output drift
  breaks **Gate 2 (`cargo build`)** automatically.
- `scripts/`:
  - `shengen-codegen.sh` — regenerate + rustfmt.
  - `shen-check.sh` — Gate 4 driver. Boots the kernel via the
    `shen-cedar` binary, runs `(tc +)`, then
    `(load "specs/core.shen")`. Passes when the kernel reports
    `typechecked in N inferences`.
  - `tcb-audit.sh` — Gate 5 driver. Re-runs shengen+rustfmt into a
    scratch file, `diff`s against the committed copy, and rejects any
    unexpected file in `src/generated/`.
  - `gates.sh` — runs all six gates in order.
- `sb.toml` — Shen-backpressure configuration pointing the harness at
  the scripts above.

### All six gates green

| # | Gate              | Mechanism                                                    |
|---|-------------------|--------------------------------------------------------------|
| 0 | `shengen-codegen` | Regenerate guards from `specs/core.shen` + rustfmt.          |
| 1 | `fmt + clippy`    | `cargo fmt --check` + `cargo clippy --all-targets -D warnings`. |
| 2 | `build`           | `cargo build --workspace`. The witness module forces this gate to encode the shengen signature contract. |
| 3 | `test`            | `cargo test --workspace` — 52 tests across 5 suites.         |
| 4 | `shen-check`      | The shen-cedar binary itself type-checks `specs/core.shen`. 14 datatypes verified in ~168 kernel inferences. |
| 5 | `tcb-audit`       | Re-run shengen + rustfmt, `diff` against committed output; reject any non-allowlisted file in `generated/`. |

Run them all with `scripts/gates.sh`.

## Phase milestones

- [x] Phase 0: Workspace scaffold
- [x] Phase 1: KL runtime
- [x] Phase 2: Boot ShenOSKernel-41.1, REPL up
- [x] Phase 3: Cedar primitives exposed to Shen
- [x] Phase 4: shengen-rust + 6 backpressure gates green

### Phase 5 ✓ (partial)

Code AOT (KL → Rust function bodies) shipped. shengen-cedar deferred —
redundant with the embedded `cedar-policy` library since Shen programs
can already author and evaluate policies in-process.

- `crates/klcompile/src/main.rs`: ~620 LOC. Reads a `.kl` file, parses
  with the shared `shen-cedar` parser, emits a Rust source file with
  one `pub fn aot_<NAME>(interp, args) -> ShenResult<Value>` per
  `(defun NAME ...)` plus a single `pub fn install(interp)` entry point
  to register them all on top of the tree-walked defuns.
- Special-form coverage: `if`, `let`, `cond`, `do`, `and`, `or`,
  `lambda`, `freeze`, `thaw`, `trap-error`, `quote`, `type`. Free
  symbols use innocent-symbol semantics. Lambda captures snapshot the
  full lexical env by-value.
- **Self-tail-call elimination**: bodies are wrapped in
  `#[allow(clippy::never_loop)] loop { ... }` and a separate
  `compile_tail` walker detects calls to the current function in tail
  position (then/else of `if`, action of every `cond` clause, last
  expression of `do`, RHS of `and`/`or`, body of `let`, `type`).
  Self-tail-calls reassign the function parameters and `continue`;
  terminal expressions `break Ok(value)`. The function parameters
  carry `#[allow(unused_mut)] let mut`.
- `crates/shen-cedar/src/aot/runtime.rs`: minimal runtime surface
  generated code calls into — `apply_named`, `apply_value`, `is_truthy`,
  `make_aot_closure`, `global_value`, `fn_value`.
- `crates/shen-cedar/src/aot/generated.rs`: the committed output of
  running klcompile on the smoke-test KL inputs. Regenerate with
  `cargo run -p klcompile -- INPUT.kl crates/shen-cedar/src/aot/generated.rs`.

### Phase 5 verification (4 tests in `tests/aot_smoke.rs`)

- `aot_factorial_matches_treewalker`: `(fact 10)` → `3628800` on both.
- `aot_loop_sum_matches_treewalker`: `(loop-sum 1000 0)` → `1000`,
  exercising the TCO path (would stack-overflow without it).
- `aot_double_matches_treewalker`: `(double 21)` → `42`.
- `aot_perf_smoke`: 5000 iterations of `(loop-sum 50 0)` — tree-walker
  ≈ 2.05 s, AOT ≈ 1.21 s → **~1.69× speedup in debug**. Release will
  compress both numbers (and likely widen the gap, since the AOT path
  benefits more from inlining).

The perf number is conservative: every primitive (`=`, `-`, `+`) still
goes through `rt::apply_named` env lookup. Inlining arithmetic and other
hot primitives is a clear next step.

### Final test totals (56 across 6 suites)

| Suite               | Tests |
|---------------------|------:|
| Unit (parser/symbol/value) | 16 |
| `eval_smoke`        | 19 |
| `parse_kernel`      | 1 |
| `boot_kernel`       | 10 |
| `cedar_smoke`       | 6 |
| `aot_smoke`         | 4 |

All six backpressure gates green (`scripts/gates.sh`).

## Open work (stretch)

- **Direct AOT-to-AOT calls** when the callee is known at compile time
  to be AOT'd. Skips the env hashmap on internal calls.

## 2026-05-27 — Phase 6: primitive inlining, full kernel AOT, kernel tests

### 6A — Hot-primitive inlining ✓

`klcompile` now emits direct calls to `rt::add`/`rt::sub`/`rt::mul`/
`rt::div`/`rt::lt`/`rt::gt`/`rt::lte`/`rt::gte`/`rt::eq`/`rt::cons`/
`rt::hd`/`rt::tl` plus the type predicates instead of routing through
`rt::apply_named` for these well-known names. `#[inline(always)]` on
each helper lets release-mode LLVM collapse the call entirely. On
`aot_perf_smoke` the speedup over the tree-walker jumped from **1.69×
to 48.02× in release mode** (5000 iterations of `(loop-sum 50 0)`).

### 6B — Full-kernel AOT ✓

Every `.kl` file in `kernel/klambda/` is compiled into a per-file Rust
module under `crates/shen-cedar/src/aot/kernel/`. 1128 total functions
across 21 modules; only the 4 top-of-file copyright strings are
skipped. `crate::aot::kernel::install_all(interp)` is wired into
`interp::boot::boot` after `register_all_metadata`, overriding each
tree-walked defun with its AOT closure while leaving the kernel's
property-vector setup intact.

Changes to `klcompile` to make 1128-function compilation work:
- `sanitize_ident` now hex-escapes non-alphanumeric chars (`?` →
  `_x3f_`, `->` → `_x2d__x3e_`) so distinct KL names (`string?` vs
  `string->n`) never collapse onto the same Rust ident.
- KL allows multiple `(defun NAME …)` for the same NAME; we pre-pass
  the form list and keep only the last occurrence so Rust doesn't see
  duplicate definitions.
- Lambda/freeze capture lists are sorted (HashSet iteration is
  nondeterministic).

New gate: `Gate 6: kernel-aot-audit` (`scripts/kernel-aot-audit.sh`).
Regenerates each per-file AOT module into a scratch dir and byte-diffs
against the committed copy — same TCB-audit pattern as Gate 5.

The generated AOT modules are excluded from `cargo fmt --check` via
`.rustfmt.toml` (12 MB of generated Rust makes rustfmt take ~5
minutes; klcompile output is already stable so byte diff is enough for
the audit).

`profile.release` changed from `codegen-units = 1` to `16` (with
`lto = "thin"` retained). codegen-units=1 was making `rustc` spend
10+ minutes (8.5 GB peak RSS) in a single LLVM unit on `stlib.rs`
alone; 16 brings the release build down to a few minutes.

### 6C — Kernel test runner ✓ (with known failures)

`bin/shen-cedar` learned a `--kernel-tests` subcommand
(`scripts/kernel-tests.sh`). Boots the kernel, overrides `y-or-n?` to
always answer yes, chdirs to `kernel/tests/`, loads `harness.shen`
first (so `*passed*`/`*failed*` get defined as `test-harness.*passed*`/
`test-harness.*failed*` by Shen's package macro), then registers a
native `reset` no-op override so that `kerneltests.shen`'s trailing
`(reset)` doesn't zero the counters before we can read them, then
loads `kerneltests.shen`. Spawned on a dedicated thread with a 1 GB
stack because the AOT-compiled reader / YACC parser hits deep
non-self-tail Rust recursion that overflows the default 8 MB.

**First result: 98 passed, 36 failed** (~73% pass rate). The 36
failures are real bugs in our port — mostly in the prolog-interpreter
report (`type error in rule 1 of my-occurs?`). Tracked as future work;
not blockers. The kernel itself boots and the existing 56 unit tests
still pass.

### 6D — Benchmarks ✓

`BENCHMARKS.md` written up with the `aot_perf_smoke` numbers (28.24×
over tree-walker on the inlined hot loop). `tests/aot_kernel_bench.rs`
added (`#[ignore]`) for non-trivial kernel-internal timing.

## 2026-05-27 — Phase 7: kernel tests passing

Started with 98 passed / 36 failed; now **134 passed / 0 failed**
(`scripts/kernel-tests.sh`).

### 7A — The root-cause bug: `Bool` vs `Sym("true")` ✓

Most of the 36 failures collapsed to a single cause. Our KL parser
interns `true` and `false` as `Value::Bool(true)` and `Value::Bool(false)`,
matching how the original .kl files write them. But the kernel's Shen
reader (used for `.shen` files via `(load ...)`) interns `true`/`false`
as the *symbols* `Sym("true")` and `Sym("false")` — via `(intern …)`
inside `shen.<atom>` (`kernel/klambda/reader.kl`). So when the kernel's
`boolean?` is defined as

```kl
(defun boolean? (V) (cond ((= true V) true) ((= false V) true) (true false)))
```

— compiled from sys.kl, where `true` becomes `Bool(true)` — and is
then handed a `Sym("true")` parsed from user .shen code, the `=`
comparison returns false. Cascade: `boolean?` returns false for the
literal `true` parsed from a .shen file, the type-checker's
`shen.primitive` rule for `boolean` fails, `shen.t*-rule-h` returns
false, and `shen.t*-rule` emits "type error in rule 1 of FN" for any
function whose body returns the literal `true`. That hit ~33 of the
36 failures (n-queens, search, c-minus, montague, l-int, proof,
quantifier machine, depth-first-search, secd, prolog-interp).

**Fix (`crates/shen-cedar/src/value.rs`)**: extend `shen_eq` with one
extra match arm that cross-equates `Bool(b)` against `Sym(k_true)` /
`Sym(k_false)` looked up from a `OnceLock<(SymId, SymId)>` populated
once by `Interp::new`. Single source change in `shen_eq` + a setter
call at interpreter boot. About 25 lines total.

### 7B — Lambda-application-via-`symbol?` (spreadsheet) ✓

`(str closure)` returned `"<closure>"`. The kernel's `symbol?` does
`(trap-error (shen.analyse-symbol? (str V)) (lambda E false))`, and
`shen.analyse-symbol?` checks first-char-alpha + rest-alphanums. With
`<` as a `shen.misc?` character and the rest alphanumeric,
`"<closure>"` passes. So `symbol?` returned true for closures, and the
spreadsheet test's `(or (number? V) (symbol? V) (string? V))` guard
treated lambdas as fixed values, never reducing them.

**Fix**: change `value_to_str` in `primitives.rs` to embed whitespace
(`"#<closure x>"`) — `shen.alphanums?` rejects space-containing
strings, so the symbol-parse path returns false and `symbol?` correctly
returns false for closures.

Bonus fix: whole-number floats were rendered without `.0` (`(* 5000 .8)`
displayed as `5000` instead of `5000.0`), so spreadsheet's expected
output `4000.0` looked like an int mismatch. Added `format_float` in
both `primitives.rs` (for `(str X)`) and `bin/shen-cedar/src/main.rs`
(for REPL display). One liner each.

### 7C — Parser: brackets + braces + REPL stack

Side-effect bugs that surfaced during 7A debugging:

- Our KL parser didn't handle Shen surface syntax (`[a b c]` cons-list,
  `[a | b]` improper-list, `{T}` type-signature). Files loaded through
  `(load …)` go through the kernel's Shen reader so this didn't break
  the kernel tests, but the REPL parsed `[Y | Z]` as the three atoms
  `[Y`, `|`, `Z]` and `(define foo {T --> U} …)` mangled the signature
  atoms. Added `parse_bracket_list` + standalone `{`/`}` tokenization
  in `crates/shen-cedar/src/kl/parser.rs`.
- REPL was on the default 8 MB thread stack, so any `(load …)` that
  hit deep AOT recursion (kernel reader / type-checker) overflowed.
  Bumped to 64 MB in `bin/shen-cedar/src/main.rs` `main`.

### 7D — Kernel-tests counter read

`kerneltests.shen` has a trailing `(reset)` that zeroes the counters.
Our runner now loads `harness.shen` first, then registers a native
`reset` no-op override (so the trailing `(reset)` is harmless), then
loads `kerneltests.shen`. Unbound `*failed*` (zero failures means it
was never `set!`'d) is now treated as 0 rather than -1.

### Final result

`scripts/kernel-tests.sh` exits 0 with **passed: 134, failed: 0** —
the full upstream Shen kernel test suite passes against shen-cedar.
56 unit tests still pass too. Added as Gate 7 in `scripts/gates.sh`.

## 2026-05-27 — Phase 8: performance (partial)

### 8A — Native hot-function overrides ✓

`register_hot_overrides` in `primitives.rs` installs native Rust for
the upstream call-frequency leaders: `element?` (12.6 M calls/suite),
`shen.pvar?` (7 M), `shen.lazyderef` (3.5 M), `fail` (3.4 M), plus
`value/or`, `<-address/or`, `read-file-as-bytelist`,
`read-file-as-string`. Pre-interned `shen.pvar`/`shen.-null-`/
`shen.fail!` symbol ids added to `WellKnown` so the hot path avoids
`interp.intern()` per call. ~12% wall-time improvement on the kernel
test suite (dev mode).

### 8B — Equality fast-path ✓

`shen_eq` short-circuits on `Rc::ptr_eq` for `Cons` and `Str`. The
kernel type-checker frequently compares a value to itself during
proof search; the reader produces shared sub-structures via cons
sharing. Both common cases now skip the deep walk.

### 8E — Cross-port benchmark ✓ (gap quantified, not closed)

`scripts/cross-port-bench.sh` runs the full kernel test suite against
both ports. Results:

- shen-cl (SBCL interpreted): **~1.0 s**
- shen-cedar (release): **~17.5 s** — 17× slower

See `BENCHMARKS.md` for the breakdown. Most of the gap is in the
tree-walker (user Shen code goes through `eval-kl` → tree-walked); the
remaining Phase 8 work (pre-interned SymIds at codegen time, SmallVec
args, direct AOT-to-AOT calls) would help here. Without changing
`Value` representation, the realistic target is ~5× of shen-cl rather
than parity — shen-cl benefits from decades of SBCL optimization.

### 8C, 8D — Deferred

The remaining codegen wins (`8C`: per-module SymId cache, SmallVec
args, direct AOT-to-AOT) and the slow-defun re-enable (`8D`) are
out of scope for this iteration and remain in the plan file.
