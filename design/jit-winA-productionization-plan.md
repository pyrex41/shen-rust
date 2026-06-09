# Win A productionization plan — direct native call edges for tail/compute code

**Date**: 2026-06-08. **Target (user-decided)**: tail-recursive & compute-shaped
loaded functions in `--served` workloads, where the spike shows **3.17×** (mutual
tail) / **35×** (self-tail). **Explicitly NOT** the CPS type-checker / kernel-tests
3.55× metric — the Win A spike showed the direct-call edge buys only **1.14×** on
non-tail CPS (see `project_winA_spike_result` in memory; `benches/jit_winA_spike.rs`).

## What already exists (reuse, do not rebuild)

The J2 closure-JIT (gated `jit` feature, runtime `SHEN_RUST_JIT`) built most of the
machinery — it was falsified *for the type-checker*, not removed:

- `JitEngine` on `Interp` — owns the long-lived `JITModule`, body cache, compile-time
  diagnostics (`src/jit/mod.rs`).
- `JitClosure` / `JitEntry` ABI `(interp, errflag, captures, args, nargs) -> word`,
  `call_jit` Rust→native boundary, `pending_error` error ABI, `rtj_*` FFI helpers.
- `codegen::compile_closure_body` (`src/jit/codegen.rs`) — KL→Cranelift for a useful
  subset: `nil`/`bool`/fixnum literals, var resolution, `if`/`and`/`or`/`let`/`do`/
  `cond`, inline prims; per-body **bail** to interp on anything else.
- Tier-in via `install_jit` overriding the AOT direct table; differential-oracle
  discipline (`tests/vm_differential.rs` pattern) + `SHEN_RUST_JIT_STATS` counters,
  including `JIT_CALL_TO_JIT`/`_TO_OTHER` — the **"Slice C" ceiling**.
- J1 proved `return_call` self-tail TCO in real codegen (`compile_length_h`).
- The Win A spike (`benches/jit_winA_spike.rs`) proved **mutual** `return_call`
  direct edges work on aarch64 and beat the FFI edge 3.17× on tail shapes.

## The gap (what "fully implement" means here)

`codegen.rs` currently routes **every named/cross call through `rtj_apply_named`
(FFI)** and emits **no `return_call`** (codegen.rs:17–19). The missing pieces:

### Stage W1 — self-tail-call in codegen ✅ DONE (2026-06-08)
Emit `return_call` to the body's *own* `FuncId` for a named self-call in tail
position, instead of the `rtj_apply_named` FFI. Reuses J1's proven 3-arg→N-arg
`return_call` loop. Requires: tail-position tracking in `lower` + knowing the body's
own name/FuncId during lowering.
- **Kill-gate**: a self-tail loop (e.g. a user `sumto`/`count-down`) JIT'd,
  differential-oracle-equal to interp across a corpus, beats interp, `gates.sh` green.

**Done**: additive `lower_tail` + `*_tail` helpers (existing value-lowerers untouched
→ zero regression on the anonymous-closure path); `compile_named_self_tail` (declare
Tail-body FuncId first → lower → default-callconv entry trampoline); `named_self_tail`
map + `compile_named` + `jit_shim_w1_sumto` + `install_w1_sumto` (bail → the tree-walked
defun serves it). Oracle `tests/jit_winA_differential.rs` (base / deep-100k TCO / fixnum
→float overflow / error) + in-crate `w1_self_edge` (proves the self-edge adds `(0,0)` to
the FFI-call tallies = native `return_call`, not a hop). **Measured 11.65× over tree-walk**
on `shen-w1-sumto(200000)` through the real `rt::apply_direct` seam.
- **Soundness fix (adversarial review caught it)**: first impl passed self-tail args via
  `marshal`→a `stack_addr` into the dying frame (read by the callee with
  `MemFlags::trusted()`) — worked at arity-2/`opt=speed` but UB under the Tail ABI.
  Fixed to J1's shape: Tail body takes **scalar** params `(interp, errflag, captures,
  p0..)`, the self `return_call` passes scalar register operands, the entry trampoline
  unpacks `args_ptr` once. `captures_ptr` stays threaded (points outside the frame).
- **W2 follow-ups**: `and`/`or` in tail position still route the eval-arm self-call
  through FFI (no native edge); add a ≥3-arity self-tail differential case (the scalar
  fix makes it sound — prove it) when W2 compiles multiple arities generically.

### Stage W2 — direct cross-call edges (Slice C)
Compile a **group** of mutually/cross-referencing functions: declare all `FuncId`s
first, then define bodies (Cranelift allows forward refs to declared-undefined fns).
Resolve a callee statically via the Lisp-2 head symbol — if it names a function in
the JIT'd group, emit a direct `return_call` (tail) / `call` (non-tail) to its
`FuncId`; otherwise fall back to `rtj_apply_named`. Note the current
`compile_and_cache` finalizes per-body — W2 needs a **group compile** entry
(declare-all → define-all → one finalize).
- **Kill-gate**: even?/odd? + a compute kernel JIT'd with direct edges, oracle-equal,
  reproduces the spike's ~3× over the FFI-edge path on *real loaded* functions;
  `gates.sh` green.

### Stage W3 — tier-up policy for tail/compute, wired to `--served`
After load in served mode, identify tail/compute-shaped user `defun`s (heuristic:
self/mutually-recursive; fixnum/cons body; no bail forms) and group-compile them
(W2); cache; never re-JIT (cf. the VM's 1.2M-recompile bug). Leave non-tail/CPS and
kernel code on the interpreter.
- **Kill-gate**: a served compute workload shows end-to-end speedup; **no regression**
  on non-tail/CPS code or on `--kernel-tests` (JIT stays off there); `gates.sh` green.

### Stage W4 — GC roots in JIT frames (gate before GC Step 4)
Today collection is OFF (grow-only, Step 3) so W1–W3 are safe. Before GC Step 4 turns
collection on, ensure register-resident `Value`s are spilled at safepoints, or that
the conservative native-frame scan (sound per `gc_roots_aot_spike`) provably reaches
JIT frames. This is the #1 correctness gate (handoff §5.1) — orthogonal to W1–W3 but
blocking on Step 4 coexistence.

### Stage W5 — hardening
Differential-oracle extension for group compilation; x86 story (`return_call`
portability — keep codegen aarch64-gated, `jit` off-by-default to protect x86 CI);
Miri on the `rtj_*` helpers (can't Miri JIT'd code); `gates.sh` all-green; docs.

## Discipline
Every stage: differential-oracle-equal (Ok/Err + `shen_eq`) across a corpus, measured
paired min-of-N, gated behind `--features jit` + `SHEN_RUST_JIT`. `benches/
jit_winA_spike.rs` is the mechanism regression anchor; W2/W3 add real-function anchors.
Commits on `main` (perf work authorized; confirm scope), no Co-Authored-By trailer.
