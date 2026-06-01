# shen-rust Performance Handoff — Closing the ~5× Gap vs shen-cl

**Audience**: an engineer/agent picking up the performance effort cold.
**Date**: 2026-05-28
**Read-with**: `design/execution-engine-roadmap.md` (architecture), `design/value-representation.md` (why value-repr is a dead end), `design/runtime-execution-strategy.md` (original Path C/D framing), `PERFORMANCE.md` (tier history).

This document is self-contained: you should be able to execute from it without
the originating conversation. **Read §2 before doing anything** — it lists work
that has already been measured and rejected, so you don't repeat it.

---

## 1. Mission & current state

**Goal**: close the gap on `scripts/kernel-tests.sh` (the upstream Shen kernel
test suite). shen-cl (SBCL) runs it in **~1.0 s**; shen-rust runs it in
**~5.5–6.0 s** warm (the absolute number drifts ±5–12% with machine thermal
state — see §6). Target: **sub-2 s** (within ~2× of shen-cl).

**What's already done** (committed; do not redo):
- Tier-0 **AOT compile of the vendored kernel** to Rust (`crates/klcompile` →
  `crates/shen-rust/src/aot/kernel/*.rs`), with a direct fn-pointer dispatch
  table (`apply_direct`), inlined arithmetic/cons/predicate primitives, and a
  `SLOW_DEFUNS` skip list. This took the suite from ~17.5 s → ~5.7 s.
- A heavily-optimized **tree-walking interpreter** (`crates/shen-rust/src/
  interp/eval.rs`): `Scope` copy-on-write, locals-by-reference, `SmallVec` arg
  vectors, FNV + pointer-keyed symbol interning, single-allocation cons.
- A **bytecode VM** (`crates/shen-rust/src/vm/*`, commits B1–B4b): opcode set,
  stack-machine compiler with free-variable capture analysis, `SelfTailCall`,
  `MakeClosure`/upvalues, inlined-primitive opcodes. **It works (134/0
  kernel-tests) but is currently *slower* than the tree-walker** and is gated
  off by default (`SHEN_RUST_VM=1`). Fixing this is the core of the plan.

**Uncommitted working-tree changes** (from the investigation; keep or drop as
noted):
- `crates/shen-rust/src/cons.rs` (**new**) — `ConsCell` seam (a newtype over
  `Rc<(Value,Value)>` that `Value::Cons` now holds). **Keep** — it's a harmless
  abstraction boundary. (An earlier recycling-pool version was reverted; see §2.)
- `crates/shen-rust/src/value.rs` — `Value::Cons` now holds `ConsCell`; trivial.
- `crates/shen-rust/src/interp/eval.rs` + `aot/runtime.rs` — removed
  unnecessary per-call `Rc::clone`s on the dispatch fast paths (`call_or_apply`,
  `tail_apply`, `call_strict`). **Keep** — clean, correct (134/0), though
  measured at ~0% (see §2).
- `design/*.md` — the design docs referenced above.

Nothing is committed. The repo convention is **do not commit without explicit
direction.**

---

## 2. What has been EMPIRICALLY RULED OUT (do not repeat)

Three intuitive levers were implemented and measured. **All three are dead.**
This is the most important section: it redirects effort away from weeks of
dead-end work that the profile *appears* to justify but does not.

| Hypothesis | Predicted | Measured | Method |
|---|---|---|---|
| Cons-cell recycling pool (avoid malloc/free) | large | **~2.5%** | back-to-back A/B (`CAP=0` vs `CAP=64K`) |
| Remove per-call `Rc::clone` on dispatch | some | **~0%** | before/after |
| Eliminate **all** cons value-churn (leaked bump arena = the *ceiling* a perfect GC could reach: no drop, no free, no refcount, no spine walk) | ~32% | **~2.4%** | paired alternating A/B, 6 pairs |

**The arena result is decisive.** `drop_in_place<Value>` is the single biggest
*profile self-sample* (~20%), but deleting that work entirely moves wall-clock
by ~2%. **Profile self-time ≠ wall-clock lever.** Conclusions:

- **A garbage collector or arena allocator will NOT close the gap.**
- **No `Value`-representation change will** (NaN-boxing, tagged pointers, 24→16 B
  shrink). Note also: immediates (`Int`/`Float`/`Sym`/`Bool`/`Nil`) are *already*
  unboxed in the enum, so NaN-boxing's headline win is already captured. See
  `value-representation.md` §6c for the full falsification.
- **Refcount traffic is not the bottleneck.**

Do not re-open any of these without new evidence.

---

## 3. Root cause (established by elimination + profiling)

The 5× is **distributed execution-model cost**, not a concentrated hotspot.
Every interpreted, dynamically-dispatched, boxed operation costs ~2–5× its SBCL
native+GC equivalent, and the type-checker performs millions of them.

**Why SBCL wins**: it native-compiles *every loaded `define`* — including the
`freeze`/`lambda` closures the type-checker builds at runtime from datatype
rules — and runs them on GC'd values. A continuation call is a native `funcall`.

**Why we lose**: we **tree-walk** that same dynamic code (`eval_in` over cloned
`Rc<Value>` `KlExpr`; linear-scan `lookup_local`) and apply continuations
through a **dynamic `Value::Closure` dispatch** path. The kernel itself is
AOT'd, but the *dominant workload is dynamic*: ~71% of the suite is `(tc +)`
then loading two `.shen` files; the heaviest single test is ~3.3 s (the
type-checker proving theorems about runtime-defined user code).

**Profile leaf breakdown** (worker thread ≈ 4152 samples at 1 ms; from
`/usr/bin/sample`, "Sort by top of stack"):

| Bucket | ~share | Symbols |
|---|---|---|
| Interpretation + dynamic dispatch | **~36%** | `eval_in` 391, `lookup_local` 219, `eval_args` 178, `collect_used_syms` 126, `tail_apply` 135, `apply_direct` 177, `is_truthy` 119, `call_or_apply` 96, `apply_named` 88 |
| Value create/destroy churn (NOT a lever — see §2) | ~32% | `drop_in_place<Value>` 814, free/malloc ~250, `Value::clone` 57 |
| The AOT'd Prolog/t\* algorithm itself | ~24% | `prolog::*`, `t_star::*`, `sys::*`, `yacc::*` (long tail) |

The type-checker is a **continuation-passing higher-order Prolog program**
(`t-star`/`sequent` running on the Prolog engine), which is why the dispatch
family and `eval_in` dominate. **Interpretation + dispatch (~36%) is the only
bucket with real, addressable headroom.**

---

## 4. THE PLAN

Backend ladder, all in **one address space on one `Rc<Value>` heap** (no
marshaling; Cedar `Foreign` handles pass through as ordinary `Value`s):

```
tree-walker  →  bytecode VM (rebuild)  →  Cranelift JIT (later, gated)
```

**LLVM and Wasm are not on the performance path** — see §5.

Execute in this order. Each stage gates the next.

### Stage D (DO FIRST) — measurement instrument

Nothing else is trustworthy without this. Machine variance (±5–12%) exceeds most
single wins.

- **D1. Type-checker microbench.** A binary/subcommand that boots the kernel,
  loads `interpreter.shen`, runs `(tc +)` — i.e. the ~3.3 s dominant path —
  *without* the rest of the kernel-test noise. Print an internal high-resolution
  timing (like the existing `run time:` line in `--kernel-tests`).
- **D2. Paired-A/B harness.** A script that, given two built binaries, runs them
  **alternately** N times (shared thermal state), and reports mean delta + min,
  not single runs. (Pattern used during investigation: alternate `bin-a` /
  `bin-b`, parse the final `run time:` line, compute paired stats in a small
  Python block.)
- **Acceptance**: harness can reliably resolve a 5% difference between two
  binaries.

### Stage A — Bytecode VM rebuild (primary lever)

The opcode set (`vm/opcode.rs`) and compiler (`vm/compiler.rs`) are sound. The
**execution model in `vm/exec.rs` is the problem**, plus the VM isn't wired into
the hot path. Do A1→A2 first and measure before A3.

- **A1 — Shared value stack.**
  *Defect*: `vm/exec.rs:47–51` allocates a fresh `locals: Vec<Value>` and
  `stack: Vec<Value>` on **every** `exec()` call. The tree-walker is
  allocation-free per call; this is the main reason the VM loses.
  *Fix*: one persistent `value_stack: Vec<Value>` owned by `Interp` (preferred
  over thread-local given the single-worker-thread model), reused across calls.
  A frame is a **base offset**; `locals[slot]` → `value_stack[base+slot]`;
  operands live above locals. Allocation only on rare stack growth. (This is the
  CPython/Lua/shen-go model.)
  *Acceptance*: VM unit tests still pass; no per-call heap alloc in `exec` (check
  with a profile or alloc counter).

- **A2 — In-VM call stack (kill the `interp.apply` bounce).**
  *Defect*: `vm/exec.rs` `Op::Call` (~line 107) drains args into a `Vec`, then
  calls `interp.apply(callee, args)` — full re-dispatch **and** a recursive Rust
  call (grows the Rust stack; this is why a 1 GB worker stack is needed).
  *Fix*: an explicit `Vec<Frame>` where `Frame = { bf: Rc<BytecodeFn>, base:
  usize, pc: usize, upvals: Rc<[Value]> }`. A `Call` to a **bytecode** callee
  pushes a frame and continues the same `loop` (no Rust recursion). `Return`
  pops the frame and resumes the caller. `Op::TailCall` *replaces* the current
  frame in place → true cross-function TCO. Calls to `Native`/AOT closures still
  go out to Rust (they're leaves; that's fine and unavoidable).
  *Bonus*: real TCO lets you **retire the 1 GB worker-stack hack** (search
  `bin/shen-rust/src/main.rs` for the thread stack size).
  *Acceptance*: 134/0 kernel-tests with `SHEN_RUST_VM=1`; deep mutual recursion
  doesn't grow the Rust stack.

- **GATE (kill-criterion)**: with A1+A2, measure the VM (`SHEN_RUST_VM=1`) vs
  the tree-walker on the Stage-D microbench. **If the VM does not beat the
  tree-walker by ≥20%, STOP and re-evaluate** — the execution-model hypothesis
  would itself be in question (hold it to the same bar as the dead hypotheses in
  §2). Do not proceed to A3 until this passes.

- **A3 — Wire the VM into the hot path.**
  *Defect*: `interp/eval.rs` `build_lambda` (~line 733) and `build_freeze`
  (~line 759) **always** build tree-walked `ClosureKind::Lambda`. `do_defun`
  (~line 807) uses the VM only under `SHEN_RUST_VM=1` (`vm_enabled()`, ~line
  896). The type-checker's hot continuations are exactly `freeze`/`lambda`, so
  today they never touch the VM.
  *Fix*: compile the body to a `BytecodeFn` at closure-creation time in
  `build_lambda`/`build_freeze`/`do_defun`, yielding `ClosureKind::Bytecode`.
  Once the gate passes, **remove the `SHEN_RUST_VM` flag** and make the VM the
  default for all dynamic code.
  *Acceptance*: 134/0 by default; microbench shows the win on the freeze/lambda
  path; differential gate (A5) green.

- **A4 — Compile-time captures.**
  `collect_used_syms`/`capture_used` (~126 leaf samples) run free-variable
  analysis at *every* closure creation at runtime. The compiler already computes
  free vars (commit `a3e1206`); emit `MakeClosure` with captured slots resolved
  at compile time so closure creation is a fixed-size copy.

- **A5 — Fallback + differential gate.**
  Any KL the compiler can't lower (some `trap-error`/`thaw` shapes) must fall
  back to `ClosureKind::Lambda` (tree-walker). Keep the tree-walker as a
  **reference oracle**: a test that runs the same KL fragments through both
  engines and asserts observational equivalence. Add to `scripts/gates.sh`.

- **A6 — Polish & validate.** Per-op `interp.charge_step()` for budget/deadline
  cancellation must be preserved (already in `exec`). Compiler must emit a
  max-operand-stack-depth alongside `n_locals`. Miri on any new `unsafe` (the
  shared stack should be safe / bounds-checkable).

**Expected outcome of Stage A: ~2–3× overall.**

### Stage B — Cranelift JIT (only if Stage A wins and more is needed)

Re-profile after Stage A. Pursue this **only** if materially short of target and
a hot, compute-bound core remains.

- **Backend: Cranelift** (Wasmtime's codegen engine; Rust-native, fast compile).
  Lower `BytecodeFn` (or a shared KL IR) → Cranelift IR → native, in-process.
- **Why it fits**: single address space. JIT'd code calls the Rust runtime
  (`rt::*`, `primitives::*`, `interp.apply`) as **direct calls** and manipulates
  `Rc<Value>`/`Foreign` with **zero marshaling** (same heap; `Rc` is
  non-moving). This is the structural advantage Wasm gives up (§5).
- **Integration**: add `ClosureKind::Jit(...)` to `value.rs`; tier-up swaps a
  hot `ClosureKind::Bytecode` → `Jit`. Dispatch calls the native pointer.
- **Hard parts**: tier-up trigger (start with a simple call-count threshold);
  tail calls in native code (trampoline or Cranelift tail-call support); compile
  only fully-lowerable functions, fall back to bytecode otherwise (no deopt v1);
  differential gate against the VM/tree-walker (JIT becomes TCB).
- **Expected outcome: approach ~1.5×.** Parity (~1.0×) with SBCL would require
  native-compiling essentially all dynamic code with a mature optimizer + GC —
  not a near-term goal.

---

## 5. LLVM and Wasm — verdict (both OFF the perf path)

**LLVM (e.g. `inkwell`) — rejected for the dynamic/JIT path.**
1. We already get LLVM where it helps: `klcompile` emits Rust → `rustc`→LLVM
   compiles the static kernel.
2. Wrong tool for a JIT — LLVM optimizes AOT throughput, not compile *latency*;
   you'd stall on first call. (Cranelift exists precisely because LLVM is too
   slow for JIT.)
3. Large C++ dependency = TCB bloat, against the project's small-trusted-base
   ethos.
- *Only conceivable use*: AOT "compile loaded `.shen` → Rust → dylib → dlopen,"
  but that reuses the `klcompile` pipeline (not LLVM-as-library), can't help
  truly-dynamic `eval-kl`, and dlopen-per-load is heavy. **Parked.**

**Wasm (Wasmtime) — rejected as a speed engine; keep only as a future *sandbox*
feature.**
1. **Host-boundary marshaling tax is fatal here**: the type-checker is
   continuation-heavy and host-heap-entangled (constant callbacks into the Rust
   runtime + shared `Rc<Value>`/`Foreign` handles). Wasm wins on compute-bound,
   self-contained code; this is the opposite.
2. **Strictly dominated by Cranelift**: Wasmtime *uses Cranelift internally*, so
   Wasm = Cranelift codegen **+** a boundary tax = worse than calling Cranelift
   directly in-process (Stage B).
3. The only genuine reason to add Wasm later is **security**: sandboxed
   evaluation of untrusted, dynamically-generated Cedar policies. That's a
   feature with its own justification, **decoupled from performance.**

---

## 6. Measurement discipline (non-negotiable)

The investigation repeatedly produced misleading single-run numbers because
machine thermal/run variance (~5–12%) exceeds most single optimizations.

- **Always paired A/B**: build both variants, alternate runs, report mean delta
  + min. Never compare two numbers taken minutes apart.
- **Use the Stage-D microbench**, not full `--kernel-tests`, to resolve small
  effects.
- Settle/quiesce the machine before sampling; expect the first run to be cold.
- Profiling: `./target/release/shen-rust --kernel-tests & ; /usr/bin/sample
  <pid> 7 -file /tmp/prof.txt`. The useful section is "Sort by top of stack"
  (self/leaf weights), **not** the inclusive call graph (which over-weights
  dispatch glue near the root).
- Build cost: release build of the binary is ~1–2 min; a `Value`/cons-type
  change additionally forces AOT regen unless it funnels through `Value::cons`
  (the `ConsCell` seam keeps cons changes regen-free).

---

## 7. File map

| Path | Role |
|---|---|
| `crates/shen-rust/src/vm/exec.rs` | **Bytecode dispatch loop** — A1/A2 happen here. Per-call `Vec` frames (47–51), `Call`→`interp.apply` bounce (~107). |
| `crates/shen-rust/src/vm/opcode.rs` | `Op` enum (stack machine, inlined prims, `SelfTailCall`, `TailCall`, `MakeClosure`). |
| `crates/shen-rust/src/vm/compiler.rs` | KL → bytecode; free-var capture analysis. A4 lives here. |
| `crates/shen-rust/src/vm/bytecode.rs` | `BytecodeFn` (code, consts, arity, n_locals). Add max-stack. |
| `crates/shen-rust/src/interp/eval.rs` | Tree-walker + dispatch. `build_lambda` (733), `build_freeze` (759), `do_defun` (807), `vm_enabled` (896), `tail_apply` (482), `call_strict` (547), `apply` (567). A3 wires here. |
| `crates/shen-rust/src/aot/runtime.rs` | AOT runtime: `call_or_apply` (56), `apply_direct`, `apply_named`, inlined prims (`add`/`sub`/…). |
| `crates/shen-rust/src/value.rs` | `Value` enum + `ClosureKind` (Native/Lambda/Bytecode; add `Jit` in B). |
| `crates/shen-rust/src/cons.rs` | `ConsCell` seam (keep). |
| `crates/klcompile/src/main.rs` | Build-time KL→Rust AOT compiler (Tier 0). Source of the lowering you may want to share with the VM backend. |
| `bin/shen-rust/src/main.rs` | Entry; spawns the 1 GB worker stack (retire after A2's TCO). |
| `scripts/kernel-tests.sh` | The benchmark/correctness suite (134 tests). |
| `scripts/gates.sh` | All 8 CI gates (fmt+clippy, build, test, shen-check, tcb-audit, kernel-aot-audit, kernel-tests). Must stay green. |

---

## 8. Definition of done / gates

Every change must keep: `cargo fmt --all --check`; `cargo clippy --workspace
--all-targets -- -D warnings`; `cargo test --workspace`;
`scripts/kernel-aot-audit.sh`; `scripts/kernel-tests.sh` → **134 passed, 0
failed**; Miri clean on any new `unsafe`. Add the A5 differential gate.

**Sequencing recap:** D → A1+A2 → **GATE (≥20% or stop)** → A3 → A4–A6 →
re-profile → (only if needed) B. LLVM/Wasm: not on this path.

## 9. Open questions for the implementer

- `value_stack` on `Interp` vs thread-local? (Single-worker model favors
  `Interp`-owned.)
- Tier-up trigger for Stage B: call-count vs tracing? (Start: call-count.)
- Factor `klcompile`'s lowering so the Rust-AOT and bytecode backends share a
  front end, before A3, to avoid two divergent compilers?
- Should the differential oracle run in CI on a fixed KL corpus, or randomized?
