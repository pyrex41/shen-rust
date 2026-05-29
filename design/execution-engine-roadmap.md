# Execution-Engine Roadmap: Bytecode VM, JIT, and the LLVM/Wasm Verdict

**Status**: Plan — for decision
**Date**: 2026-05
**Supersedes** the Path-C/Path-D evaluation in `runtime-execution-strategy.md` §3–4 with measured data.
**Related**: PERFORMANCE.md, value-representation.md, `crates/shen-cedar/src/vm/*`, `crates/klcompile`

---

## 0. What the measurements already settled

We are ~5× slower than shen-cl (SBCL) on `--kernel-tests`. Three "obvious"
levers were implemented and measured, and **all three are dead**:

| Lever | Predicted | Measured | Status |
|---|---|---|---|
| Cons recycling pool | large | ~2.5% | ❌ |
| Remove per-call `Rc::clone` on dispatch | some | ~0% | ❌ |
| Eliminate **all** cons value-churn (leaked arena = GC ceiling) | ~32% | ~2.4% | ❌ |

The arena spike is decisive: `drop_in_place<Value>` is ~20% of profile
*self-samples* but only ~2% of *wall-clock* — so **a GC or arena will not close
the gap, and neither will any `Value`-representation change.**

By elimination, the 5× is **distributed execution-model cost**: every
interpreted, dynamically-dispatched, boxed operation costs ~2–5× its SBCL
native+GC equivalent, across millions of operations. SBCL native-compiles every
loaded `define` — including the `freeze`/`lambda` closures the type-checker
builds at runtime — and runs them on GC'd values. We tree-walk that same dynamic
code (`eval_in` over `Rc<Value>` `KlExpr`, linear `lookup_local`) and dispatch
continuations through a dynamic `Value::Closure` path.

**The only lever with real headroom is the execution model.** Biggest
addressable chunk (leaf profile): interpretation + dispatch ≈ **36%**
(`eval_in`/`lookup_local`/`eval_args`/`collect_used_syms` ≈ 22% + the `apply`
family ≈ 14%).

## 1. Honest targets

- **Bytecode VM, done right**: plausibly **~2–3×** overall (turns the ~36%
  interpret+dispatch into something much cheaper; also speeds the AOT functions
  that call back into dynamic code).
- **+ JIT (Cranelift)** on hot functions: approach **~1.5×**, maybe better on
  compute-heavy code.
- **~1.0× parity with SBCL**: would require native-compiling essentially all
  dynamic code with a mature optimizer + GC. Not a realistic near-term goal.

There is no weekend win. This is a multi-week engine effort, staged so each
stage is validated before the next is funded.

## 2. Tier model

| Tier | Code it runs | Backend | Status |
|---|---|---|---|
| **T0** | Vendored kernel (build time) | AOT → Rust (`klcompile`) → rustc/LLVM | **done** |
| **T1** | All dynamic KL (`eval-kl`, `defun`, `lambda`, `freeze`, loaded `.shen`) | **Bytecode VM** | exists (B1–B4b) but slower than tree-walker — **rebuild (Part A)** |
| **T2** | Hot T1 functions (call-count threshold) | **Cranelift JIT** | **future, gated on T1 winning (Part B)** |
| — | Tree-walker | — | demoted to **reference oracle + fallback** for KL the compiler can't lower |

LLVM and Wasm are **not** tiers — see Part C.

---

## Part A — Bytecode VM rebuild (the actual lever)

The opcode set and compiler (`vm/{opcode,compiler}.rs`: stack machine, inlined
prims, `SelfTailCall`, `MakeClosure`/upvals, free-var capture analysis from
commit `a3e1206`) are sound. The **execution model in `vm/exec.rs` is what's
wrong**, plus it isn't wired into the hot path. Three fixes, in order:

### A1 — Shared value stack (kill per-call `Vec`)
*Defect:* `exec.rs:47–51` allocates a fresh `locals: Vec` and `stack: Vec` per
call. With `Value` at 24 B and the tree-walker being allocation-free (`Scope`
COW + `SmallVec`), this is why the VM loses.

*Fix:* one persistent `value_stack: Vec<Value>` owned by `Interp` (or a
thread-local), reused across all calls. A frame is a **base offset**; locals are
`value_stack[base .. base+n_locals]`, operands live above. `LoadLocal(slot)` →
`value_stack[base+slot]`. Allocation happens only on the rare stack grow. This
is the CPython/Lua/shen-go model.

### A2 — In-VM call stack (kill the `interp.apply` bounce + Rust recursion)
*Defect:* `exec.rs:107` `Op::Call` drains args into a `Vec`, then calls
`interp.apply(callee, args)` — a recursive Rust call through the full dynamic
dispatch path. So VM→VM calls grow the Rust stack (hence the 1 GB worker stack)
and re-pay dispatch.

*Fix:* an explicit frame stack `Vec<Frame>` where `Frame = { bf: Rc<BytecodeFn>,
base: usize, pc: usize, upvals: Rc<[Value]> }`. `Call` to a **bytecode** callee
pushes a frame and continues the same `loop` (no Rust recursion). `Return` pops
the frame and resumes the caller. `TailCall` *replaces* the current frame in
place (true cross-function TCO → **retires the 1 GB stack hack**). Calls to
`Native`/AOT closures still go out to Rust (they're leaves and unavoidable), but
those are direct calls, not re-dispatch.

### A3 — Wire the VM into the hot path
*Defect:* `do_defun` uses the VM only under `SHEN_CEDAR_VM=1`; `build_lambda`
/`build_freeze` (`eval.rs:733/759`) **always** build tree-walked
`ClosureKind::Lambda`. The type-checker's hot continuations are exactly
`freeze`/`lambda` — so today they never touch the VM.

*Fix:* compile the body to a `BytecodeFn` at closure-creation time in
`build_lambda`/`build_freeze`/`do_defun`, producing `ClosureKind::Bytecode`.
Once A1+A2 make the VM win, **remove the `SHEN_CEDAR_VM` flag** and make it the
default for all dynamic code. Keep `ClosureKind::Lambda` only as the fallback.

### A4 — Compile-time captures (kill `collect_used_syms`)
`collect_used_syms`/`capture_used` (~126 leaf samples) run free-variable
analysis at *every* closure creation. The compiler already computes free vars;
emit `MakeClosure` with the captured slots resolved at compile time so closure
creation is a fixed-size copy, no runtime scan.

### A5 — Fallback + differential safety
Any KL the compiler can't yet lower (e.g. odd `trap-error`/`thaw` shapes) falls
back to the tree-walker. The tree-walker becomes a **reference oracle**: a
differential gate runs the same KL fragments through both and asserts
observational equivalence (already proposed in runtime-execution-strategy.md §6).

### A6 — Validation & kill-criterion
- All 8 gates green, `--kernel-tests` 134/0, Miri on any new `unsafe` (the
  shared stack should be safe; frame indexing is bounds-checkable).
- **Paired A/B** (Part D) on the type-checker microbench.
- **Kill-criterion**: if, after A1+A2+A3, the VM does not beat the tree-walker
  by **≥ 20%** on the type-checker workload, stop and re-evaluate — the model
  hypothesis would itself be in doubt (given how many hypotheses have died,
  hold this one to the same standard).

### A7 — Risks
- Budget/deadline charging: keep `interp.charge_step()` per op (already present).
- Cedar `Foreign` handles: pass through as ordinary `Value` — no boundary, no
  marshaling (this is the structural advantage that Wasm gives up; see Part C).
- Operand-stack depth / locals sizing computed by the compiler (already tracked
  as `n_locals`); add a max-stack field.

---

## Part B — JIT (Cranelift), Tier 2, gated on Part A

**Only pursue after the VM is the default and profiled, and only if the
workload still needs more.**

- **Backend: Cranelift** (the Wasmtime codegen engine; Rust-native, designed for
  *fast* compilation). Lower `BytecodeFn` (or the shared KL IR) → Cranelift IR →
  native, in-process.
- **Why it fits here**: single address space. JIT'd code calls the existing Rust
  runtime (`rt::add`, `primitives::*`, `interp.apply`) as **direct function
  calls**, and manipulates `Rc<Value>`/`Foreign` **with zero marshaling** — the
  same heap. No GC to coordinate with (Rc is non-moving).
- **Integration**: a `ClosureKind::Jit(JitFn)` variant; tier-up swaps a hot
  `ClosureKind::Bytecode` → `Jit`. Dispatch calls the native pointer directly.
- **Hard parts**: (1) which functions to compile (call-count/trace threshold);
  (2) tail calls in native code (trampoline, or Cranelift's tail-call
  support); (3) only compile fully-lowerable functions — fall back to bytecode
  otherwise (no deopt machinery v1); (4) the JIT's correctness becomes TCB →
  differential gate against the VM/tree-walker.
- **Honest cost**: this is the path toward ~1.5×, but it is a large, ongoing
  component. Cranelift is a reasonable dependency (Rust, no C++), far lighter
  than LLVM.

---

## Part C — Verdict on LLVM and Wasm

### LLVM (e.g. via `inkwell`) — **rejected for the dynamic path**

1. **We already get LLVM where it helps.** `klcompile` emits Rust; `rustc`
   lowers it through LLVM. The *static* kernel already has LLVM-grade codegen.
   LLVM adds nothing there that we don't have.
2. **Wrong tool for a JIT.** LLVM optimizes for AOT throughput, not compile
   *latency*; JIT-compiling hot functions through LLVM would stall on first call.
   Cranelift exists precisely because LLVM is too slow for this.
3. **TCB / dependency cost.** A large C++ toolchain dependency is antithetical to
   this project's small-trusted-base ethos.
- *Only conceivable use*: an ahead-of-time "compile loaded user `.shen` files →
  Rust → dylib → `dlopen`" path. But that reuses the existing `klcompile`
  pipeline (not LLVM-as-a-library), cannot help truly-dynamic `eval-kl`, and
  `dlopen`-per-load is operationally heavy. **Parked, not planned.**

### Wasm (Wasmtime) — **rejected as a speed engine; keep only as a sandbox feature**

1. **The host-boundary tax is fatal for this workload.** The type-checker is
   continuation-heavy and host-heap-entangled: it constantly calls back into the
   Rust runtime (primitives, `apply`, `cons`) and threads shared
   `Rc<Value>`/`Foreign` Cedar handles. Every such interaction crosses the
   guest/host boundary with marshaling. Wasm wins on *compute-bound,
   self-contained* code; this is the opposite.
2. **Strictly dominated by Part B.** Wasmtime *uses Cranelift internally*. Going
   through Wasm gives you Cranelift's codegen **plus** a boundary tax — worse
   than calling Cranelift directly in-process (Part B), which keeps the shared
   heap.
3. **The one real reason to add it later is security, not speed**: sandboxed
   evaluation of *untrusted, dynamically-generated Cedar policies* — an isolation
   property aligned with the robustness ethos, where the boundary cost buys
   containment. **Decouple that from the perf roadmap**; it is a feature with its
   own justification, not a tier.

**Summary:** for performance, the backend ladder is **tree-walker → bytecode VM →
Cranelift JIT**, all in one address space on one `Value` heap. LLVM and Wasm do
not earn a place on the perf path.

---

## Part D — Measurement discipline (learned the hard way)

The machine has ~5–12% thermal/run variance — larger than most single wins.
Rules for every change from here:

- **Paired A/B**: build both variants, alternate runs (shared thermal state),
  report mean delta + min, not single runs.
- **Focused microbench**, not full `--kernel-tests`: a tight harness that loads
  `interpreter.shen` + runs `(tc +)` (the ~3.3 s dominant path), min-of-N, so a
  5% effect is visible. **Build this first** — it's the instrument every
  subsequent change depends on.
- Quiesce + settle before sampling; record absolute machine state.

## Part E — Sequencing & decision gates

1. **D first**: build the type-checker microbench + paired-A/B harness.
2. **A1 + A2** (shared stack + in-VM frames). Measure vs tree-walker on the
   microbench.
3. **A3** (wire into lambda/freeze/defun; drop the flag) — only if A1+A2 pass the
   ≥20% kill-criterion.
4. **A4–A6** polish; retire the 1 GB stack hack via real TCO; differential gate.
5. **Re-profile.** If still materially short of goal *and* a hot, compute-bound
   core remains, **then** scope **Part B (Cranelift)**.
6. **LLVM / Wasm**: not on this path. Revisit Wasm only if/when *sandboxing
   untrusted policy code* becomes a product requirement.

## Open questions

- Shared value stack on `Interp` vs thread-local? (Worker-thread model suggests
  `Interp`-owned is cleanest.)
- Tier-up trigger for B: call-count vs tracing? (Start: simple call-count.)
- How much of `klcompile`'s lowering can be shared between the Rust-AOT and
  bytecode backends to avoid two divergent compilers? (Worth a factoring pass
  before A3.)
