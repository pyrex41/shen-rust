# Runtime Execution Strategy for Dynamic KL Code

**Status**: Draft — for discussion  
**Date**: 2026-05  
**Authors**: Discussion between project maintainers  
**Related**: PERFORMANCE.md, ARCHITECTURE.md, crates/klcompile

---

## 1. Context and Motivation

shen-cedar currently has two execution paths for KL:

1. **AOT to Rust** (`klcompile` + `crates/shen-cedar/src/aot/kernel/`) — applied to the entire vendored kernel at build time. This path is already quite strong (direct calls via `apply_direct`, primitive inlining, `register_aot_direct` table, etc.).
2. **Tree-walking interpreter** (`Interp::eval` / `eval_in` in `crates/shen-cedar/src/interp/eval.rs`) — used for everything that arrives at runtime via `eval-kl`, `(load ...)`, the REPL, or Shen source loaded after boot (type checker, YACC, sequent calculus, prolog, reader extensions, user code, etc.).

Recent work (Scope borrow/owned, `SmallVec` argument vectors, direct AOT table, hot overrides) has improved the interpreter, but the fundamental model remains a tree walker over `Rc<Value>` with trampolined control flow.

The question under discussion is how (and how aggressively) to improve execution of *dynamically supplied* KL.

## 2. Why Do We Care About Dynamic Execution Speed?

This is the central scoping question.

**Arguments for caring less**:
- The kernel itself is now AOT-compiled.
- Purely interactive/REPL use is a small niche.
- Many real Shen programs spend the majority of their time in library/kernel code that can be AOT'd.

**Arguments for still caring (specific to this project)**:

- **Core algorithms that are not (and cannot easily be) fully AOT'd at build time**:
  - The type checker (`tc +` mode) lives in `t-star.kl` / `sequent.kl` / `types.kl`.
  - Shen-YACC and significant parts of the reader and macro system.
  - The integrated Prolog engine.
  These are large bodies of *Shen source* that flow through `eval-kl` even in non-interactive use.

- **Cedar value proposition**:
  The distinguishing feature of shen-cedar is first-class Cedar policy authoring and evaluation from within Shen. Dynamic generation, transformation, and evaluation of policies is inherently a *runtime* code execution problem. If this path is 5–10× slower than a mature port, the "use Shen to build and reason about Cedar policies" story is materially weakened.

- **Load-time and library loading**:
  `(load "foo.shen")` and `(eval ...)` go through the full Shen → KL → `eval-kl` pipeline. Any non-trivial Shen program or library pays this cost.

- **Project philosophy (backpressure / shengen)**:
  This project emphasizes robustness, formal methods, and a small trusted computing base. Having a high-performance "first-class" execution path only for statically known kernel code, while everything dynamic is second-class and slow, creates an uncomfortable split. It also means that interesting uses of the language (especially anything involving generated code) cannot easily benefit from the performance characteristics we advertise.

**Conclusion**: Even if raw REPL interactivity is a minor use case, dynamic KL execution speed is material to the Cedar integration story and to the credibility of shen-cedar as a serious host. We do not need to match shen-cl on every micro-benchmark, but we need a credible path to "good" dynamic performance without compromising robustness.

## 3. Evaluation of the Two Favored Paths

Two paths were identified as most interesting:

- **Path C**: Custom Bytecode VM (primary dynamic engine) + optional tier-up.
- **Path D**: KL lowered to WebAssembly at runtime (executed via Wasmtime / Wasmer), potentially as a tier or as the main dynamic path.

### 3.1 Path C — Custom Bytecode VM + Tiered Compilation

**Model**:
- At `defun` / `eval-kl` time (or after a hotness threshold), compile KL to a compact bytecode with flat per-call frames.
- The VM uses integer local slots, explicit tail-call opcodes (`OP_TAIL_CALL`, `OP_SELF_TAIL_CALL`), upvalue support for closures, and specialized arithmetic ops.
- Later, hot functions can be tiered up to a faster backend (Cranelift, copy-and-patch, or Wasm).

**Robustness characteristics**:
- **Positive**: Single address space. Excellent interop with existing AOT code and the `DirectFn` table. We control the entire semantics. Easier to keep `Value`, `Foreign` (Cedar) objects, and the interpreter in the same world.
- **Negative**: Introduces a second execution engine for KL semantics. Any divergence between the VM and the tree-walker (or future tiers) is a bug. The VM itself becomes part of the TCB.

**Performance characteristics**:
- A well-designed bytecode VM following the shen-go approach can deliver 4–8× over a decent tree walker on its own (per their measurements). Combined with the AOT work already done, this would likely bring dynamic workloads into a much more acceptable range.
- Tier-up is where "very fast" lives, but is optional.

**Complexity & ownership**:
- We own the VM → high control, high responsibility.
- Reference implementation available in shen-go (`kl/{vm,compiler,types}.go` + design doc).

### 3.2 Path D — KL → WebAssembly

**Model**:
- At definition/evaluation time, lower KL to Wasm (text or binary).
- Execute using Wasmtime (or Wasmer). Use Cranelift for fast compilation or LLVM for higher code quality.
- Could be used as the primary dynamic engine or strictly as a hot tier.

**Robustness characteristics**:
- **Positive**: Wasm has a relatively well-specified semantics and a strong industry implementation (Wasmtime). The guest/host boundary is explicit, which can actually help compartmentalization. Sandboxing of dynamically generated Shen code (especially code that touches Cedar policies) is a genuine security property that aligns with the project's emphasis on robustness.
- **Negative**: Value representation bridging becomes painful (`Value`, `Cons` cells, `Foreign` Cedar handles, and closures must cross the boundary). This is often the dominant engineering cost in Wasm embeddings of dynamic languages. The TCB now includes the Wasm runtime. Debugging and observability across the boundary are worse. AOT ↔ Wasm interop is less seamless than pure-Rust AOT ↔ custom VM.

**Performance characteristics**:
- Can be excellent (near-native on hot code) once compiled.
- Compilation latency is higher than a custom lightweight bytecode compiler (though Cranelift is quite fast).
- The boundary tax on every host call (especially if Cedar objects are involved) can be significant unless careful shared-memory or handle-passing designs are used.

**Complexity & ownership**:
- Less code we write for the actual machine-code generation.
- More complexity in the embedding layer and FFI.

## 4. Recommended Direction (Draft Proposal)

**Primary recommendation**: Pursue a **hybrid** where a custom Bytecode VM is the main dynamic execution engine, with WebAssembly available as an *optional hot tier* for functions that justify the cost.

Rationale (robustness-first):

- The Bytecode VM gives us the largest single improvement over the current tree-walker with the best control over semantics and data representation.
- It keeps the majority of dynamic execution inside code we fully own and can reason about.
- Wasm becomes a performance *optimization* rather than a requirement, limiting its impact on the TCB and on the Cedar FFI story.
- We can still deliver the sandboxing benefit for the subset of workloads that need it (by routing specific functions or entire modules through Wasm when desired).
- This mirrors the successful tiered models used by mature dynamic language implementations while respecting that we already have a high-quality AOT tier.

**Tier structure (proposed)**:

| Tier | When used                  | Implementation     | Characteristics                     | Trust / TCB impact |
|------|----------------------------|--------------------|-------------------------------------|--------------------|
| 0    | Kernel (build time)        | AOT Rust (`klcompile`) | Maximum speed, direct calls        | Lowest (Rust code we compile ourselves) |
| 1    | General dynamic KL         | Custom Bytecode VM | Good speed, full control, seamless interop | Medium (new VM we own) |
| 2    | Hot functions (optional)   | Wasm (Wasmtime)    | Highest speed, sandboxing          | Adds Wasmtime to TCB for those functions |

## 5. High-Level Architecture Sketch

### 5.1 Shared Lowering Infrastructure

- Factor the existing `klcompile` lowering logic so it can target multiple backends:
  - Rust source (current)
  - Bytecode (new)
  - Wasm (future)
- Common passes: tail-call analysis, self-tail detection, closure conversion / upvalue analysis, pattern compilation, primitive specialization.

### 5.2 Bytecode VM Design Principles (inspired by shen-go)

- Flat per-call frames (`Nlocals` slots including parameters).
- Explicit `OP_SELF_TAIL_CALL` that rebinds the current frame and jumps.
- `OP_LOAD_UPVAL` for closures.
- Specialized arithmetic and predicate ops with fast paths.
- Constants pool per function.
- Clear separation between the VM execution loop and the rest of the runtime.

### 5.3 Value Representation and FFI

This is the hardest part for any multi-tier design.

Options under consideration:
- Keep a single `Value` representation and pass it by reference/handle into Wasm when needed (requires careful design of the host functions exposed to Wasm).
- Use a "Wasm-friendly" subset or view for values that cross the boundary.
- Accept some copying on the Wasm tier boundary for robustness and simplicity (at least initially).

### 5.4 TCO Across Tiers

- The Bytecode VM must support arbitrary tail recursion (via explicit opcodes + trampoline or loop).
- Cross-tier tail calls (AOT → VM, VM → Wasm, etc.) are probably best treated as non-tail at the boundary, with the understanding that this is rare in hot paths.

## 6. Robustness and TCB Considerations

Any new execution engine increases the trusted surface. This project already has strong mechanisms for this:

- `kernel-aot-audit.sh` (byte-diff of regenerated AOT modules)
- `tcb-audit.sh` for shengen output
- The six (now seven) gates

**Proposed additions for a new dynamic engine**:
- Differential testing: run the same KL fragments through the old tree-walker, the new VM, and (when enabled) Wasm, asserting observational equivalence.
- A regeneration + diff gate for any compiled bytecode or Wasm "bootstrap" artifacts, if we choose to pre-compile any.
- Clear documentation of which tiers are used for which categories of code (kernel vs. type checker vs. user code vs. policy generation).

## 7. Phased Implementation Proposal

**Phase 1 — Foundation (low risk)**
- Build a clean, well-tested Bytecode VM that can execute the existing kernel test suite and `eval_smoke` tests.
- Make it the default for `eval-kl` (behind a feature flag initially).
- Keep the tree-walker as a reference implementation for differential testing.

**Phase 2 — Performance & Polish**
- Flat frames, specialized ops, self-tail optimization, upvalues.
- Measure against current baseline and against shen-cl on realistic workloads (including `(tc +)`).

**Phase 3 — Optional Wasm Tier (only if justified)**
- Prototype lowering a subset of KL to Wasm.
- Implement a simple hotness-driven tier-up.
- Evaluate the Cedar FFI cost and sandboxing benefit.
- Decide whether to productize or keep as an experimental path.

**Phase 4 — Tooling & Gates**
- Add appropriate audit gates.
- Update documentation (PERFORMANCE.md, ARCHITECTURE.md, this document).

## 8. Open Questions

- Should the Bytecode VM be the *only* dynamic path long-term, or should we keep a tree-walker fallback for debugging / very simple cases?
- What is the hotness model for tier-up (call count? tracing? manual annotation)?
- How aggressively should we pursue shared lowering between the Rust AOT path and the new VM/Wasm paths?
- Do we want to expose tier selection or sandboxing controls to Shen code (e.g., `(cedar.with-sandboxed-policy-generator ...)` or similar)?
- Licensing / distribution implications of adding Wasmtime as an optional dependency.

## 9. References

- shen-go `thoughts/shen-go-compiler-design.md` and `kl/{vm,compiler,types}.go`
- Neil Mitchell's "Writing Fast Interpreters" (closure trees)
- Current `crates/shen-cedar/src/interp/eval.rs` and `aot/runtime.rs`
- shen-c `src/c/evaluator.c` (tail call lowering via `c.loop` / `c.recur` + `setjmp`)
- Wasmtime embedding documentation and Cranelift

---

**Next steps (proposed)**: Review this note, decide on scope (Path C as primary, Path D as optional tier), then produce a more detailed Bytecode VM design doc with opcode set and lowering rules before implementation begins.

This document should live in the repository and be updated as decisions are made.