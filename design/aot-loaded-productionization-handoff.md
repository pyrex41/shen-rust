# Handoff: AOT-Native Loaded Shen — Productionization Planning

**For: a fresh ultracode planning session.** Everything below is measured,
committed evidence — do not re-derive it; plan from it. The deliverable of
that session is a staged, kill-gated productionization plan for **Lever B:
making loaded `.shen` defuns execute as klcompile-emitted native Rust** in
served/hot workloads.

## 1. The thesis, and what it is NOT

Loaded user code today executes as tree-walked `ClosureKind::Lambda` (or VM
bytecode under `--served`). The proven lever: after a *normal* load (all
side effects live — datatypes, declares, macros, output), swap the loaded
defuns' function cells for klcompile-emitted native fns.

It is **NOT** a lever for cold `--kernel-tests`. Phase 0 (2026-06-09,
ultracode run `wf_8fead85f-c9b`) measured that wall as 64% typecheck CPS
dispatch + 26% eval-kl side-effect forms; loaded defuns are
defined-then-barely-run (`do_defun` ≈ 0.5ms). That wall is owned by the
shipped tc-cache (§4). Do not re-target kernel-tests; the served/hot shape
is the metric.

## 2. Evidence base (all on main, 2026-06-09)

| Commit | What | Numbers |
|---|---|---|
| `fc6a642` | `benches/aot_vs_vm_vs_treewalk.rs` — synthetic 3-way, klcompile-emitted code verbatim, recursion through boxed `apply_direct` (verified non-devirtualizable) | AOT/VM **3.4–5.6×**, AOT/tree 13–23× (fib / sumto self-tail / cons-walk) |
| `f0395da` | `benches/normal_form_aot.rs` + `benches/gen/` — **real loaded code**: `(load "interpreter.shen")` for real, normal-form run hot, AOT installed over loaded defuns, `shen_eq`-verified identical | AOT **5.04–5.23× over tree-walk loaded**, **1.92–2.15× over VM loaded** |
| `e8b9f2e`+`089fd7d`+`b52c7e0` | tc-cache: typecheck-verdict memoization + nesting-sound keying | cold kernel-tests 4.07→1.48s (2.75×), beats shen-cl 2.0s |
| `1d2b2c0` | Win A W1: JIT native self-tail `return_call` codegen (jit-gated) | 11.65× vs tree-walk |

Generation pipeline proven end-to-end in `f0395da`:
`(bootstrap "file.shen")` → `file.kl` → `cargo run --release -p klcompile --
file.kl out.rs` (10/10 defuns compiled on interpreter.shen) → `install()`
over the loaded defuns → zero semantic divergence.

**Falsified — do not re-litigate without new evidence:**
- Caching/replaying `shen->kl` translations and `shen.compile-prolog`
  codegen: built (3-stream tc_cache, in git history of
  `crates/shen-rust/src/interp/tc_cache.rs`), measured **wall-neutral**.
- Write-set capture for `shen.process-datatype`: non-starter — kernel `put`
  mutates the property-vector dict **in place** via `address->`/`vec_set`,
  bypassing `Env`; registry values include closures.
- Native codegen for the type-checker's CPS shape (J2): −15%, ceiling 0%.
- The "~2× boxed AOT ceiling": a shen-go figure, wrong for this port.

## 3. Architecture facts the plan must respect

- **Two-table dispatch.** `env.functions` (closure values) + `aot_direct`
  (`Vec<Option<DirectFn>>` by SymId, `interp/eval.rs:112`). Kernel/AOT code
  calls via `rt::apply_direct` (`aot/runtime.rs:36`) hitting the raw
  fn-pointer table. Any override must hit **both** tables —
  `register_native` then `register_aot_direct`, in that order.
- **THE BLOCKER: no `clear_aot_direct`.** A user redefinition after an AOT
  install leaves the stale native dispatching via `apply_direct` forever.
  Fix (do_defun must clear/restore the direct slot for overridden names) is
  a hard prerequisite with its own differential test. This is the
  two-table coherence trap; the normal-form bench sidesteps it only because
  nothing redefines.
- **klcompile** (`crates/klcompile/src/main.rs`): single-file bin,
  `<input.kl> <output.rs>`; inlines arith/cons/hd/tl to `rt::` helpers,
  calls via `apply_direct`, lambda/freeze via `rt::make_aot_closure`;
  `SLOW_DEFUNS` is a kernel-name-only skip list (generalize to size/shape
  budget). Phase-1 refactor to lib+CLI must keep kernel AOT **byte-stable**
  (`scripts/kernel-aot-audit.sh` is a gate).
- **`bootstrap` loses datatypes** (their effects fire at read-time
  macroexpansion; the .kl carries only defuns). Hence the overlay split:
  normal load first (semantics), then swap defun cells (speed). Proven in
  `f0395da`.
- **Cache keying is a solved problem**: reuse tc-cache's machinery
  (`interp/tc_cache.rs`) — kernel-seeded content-hash chain, enclosing-load
  digests, gensym pinning (start/end/per-entry). An AOT artifact cache
  keyed the same way inherits the same soundness story.
- **GC**: collection is grow-only (Step 3). AOT-installed closures and any
  cached Values are future Step-4 roots (already flagged in tc_cache docs).

## 4. Adjacent shipped state (don't collide)

- tc-cache (`SHEN_RUST_TC_CACHE=<dir>`, off by default) owns load-time
  typecheck cost. An AOT overlay composes with it: tc-cache makes the load
  fast, the overlay makes the loaded code fast.
- `--served` VM: shipped, 2.33× warm; loaded-code baseline under it is the
  65–71ms/batch column in `f0395da`.
- Win A JIT (W1 shipped, W2 = cross-call edges pending,
  `design/jit-winA-productionization-plan.md`).

## 5. The strategic question the plan MUST resolve

**AOT overlay vs Win A JIT W2 for the served use case — or a boundary
between them.** Considerations: JIT warm-up amortizes in long-lived served
processes and is already partially shipped, but is feature-gated
(`jit`, off-by-default for x86 CI) and bails on many shapes; AOT wins
cold-start, has no runtime-compile dependency, and covers whole files of
named defuns — but a *dynamic* AOT path needs rustc at runtime (dylib,
unstable ABI) or build-time manifests. A plausible boundary: AOT for
file-known named defuns (build-time or cached artifacts), JIT for
runtime-created closures. The plan should argue one with numbers
(`normal_form_aot` can grow a JIT arm: `SHEN_RUST_JIT=1`).

**Target workload**: the shen-cedar served shape —
`examples/shen-cedar-authz` (gate/verify/generate; served VM + ShenHost)
loads spec/policy `.shen` and serves many evaluations. The plan should
define an authz-shaped bench alongside `normal_form_aot`.

## 6. Artifact-strategy options to weigh (original plan §3/§5, updated)

1. **Build-time manifest (static)**: known `.shen` files → generated Rust
   modules compiled into the binary; loader checks content hash, installs
   on match. No runtime rustc. Cheapest, serves shen-cedar now.
2. **Dynamic dylib cache**: on miss, normal-load + emit + `rustc` to a
   cache dylib keyed by tc-cache-style chain; on hit, dlopen + install via
   C-ABI installer; handles must outlive Interp. Powerful, heavy, opt-in.
3. **JIT-only** (no AOT artifacts): fund W2 instead. Zero artifact
   machinery; pays warm-up; gated feature.

## 7. Kill-gates to bake into whatever plan emerges

- Differential identity: loaded-engine vs AOT results `shen_eq`-equal on a
  corpus (extend `normal_form_aot`'s in-process check into a test).
- Redefinition coherence: redefine an AOT-overlaid fn → new definition
  wins through BOTH dispatch paths (this is the `clear_aot_direct` test).
- `scripts/gates.sh` all green; kernel AOT byte-stable after the klcompile
  refactor (Gate 6).
- Measured ≥1.5× on a real served workload vs the VM baseline before any
  default-on or product wiring; 134/0 untouched on the bare path.

## 8. Session gotchas (cost real debug cycles)

- The binary is package **`shen-rust-bin`**; `cargo build -p shen-rust`
  builds only the lib — stale-binary silently no-ops your change.
- `benches/*.rs` are auto-discovered targets: generated/included modules go
  in `benches/gen/`.
- Generated-code lint posture: carry klcompile's `#![allow(...)]` set
  (incl. `clippy::clone_on_copy`); Gate 1 is strict.
- Deep typecheck/recursion work needs the 1GB worker-stack thread pattern.
- Timing methodology: paired/interleaved process runs, min-of-N; the box
  drifts ~10-25% under load. `{ TIMEFORMAT=%R; time ...; }` (the
  `/usr/bin/time`-stderr-swallowing trap bit twice).
- Repo: `includeCoAuthoredBy: false` — no Co-Authored-By trailer.

## 9. Suggested fan-out shape for the planning session

Phase A (parallel read-only investigations): (1) klcompile lib+CLI refactor
surface + SLOW_DEFUNS budget design; (2) overlay install seams + the
`clear_aot_direct`/do_defun coherence fix design; (3) artifact strategy
deep-dive (§6 options costed against the shen-cedar deploy reality);
(4) JIT-W2-vs-AOT boundary analysis (read
`design/jit-winA-productionization-plan.md` + J2 memory); (5) served-bench
definition over `examples/shen-cedar-authz`.
Phase B: adversarial review of each (the W1 review caught real UB — keep
that bar). Phase C: judge-panel synthesis into a staged plan with the §7
kill-gates, ending in a scope decision the user confirms before any build.
