# shen-cedar — Cranelift JIT Productionization Plan (GC ladder rung 3 → ship)

**Date**: 2026-05-29. **Status**: plan, post-spike. The spike
(`benches/jit_spike.rs`, commit `128a906`) **passed** its kill-criterion — see
`design/jit-spike-handoff.md` §10 for the numbers. This doc is the staged plan to
turn that result into a shipped speedup, with a kill-gate at each stage (project
discipline: every rung is measured before the next is funded).

**Read first**: `design/jit-spike-handoff.md` (the whole thing, esp. §10 result +
§5 hard problems) and `design/perf-state-and-gc-ladder.md` (the ladder + the
honest scoreboard: ~3.55× off SBCL, gap is *dispatch*).

---

## 0. What the spike settled (so we don't re-litigate)

- Native code on the word `Value` beats the real AOT-direct dispatch path **3.46×**
  on `fib` (dispatch-bound), **35.67×** on a tail loop, **2.28×** even with a
  `cons` FFI per step. The lever is real.
- **`CallConv::Tail` + `return_call`** self-recursion works on aarch64/Cranelift
  0.132 (constant-stack 200k-deep). A **default-callconv host-entry trampoline**
  can `call` a tail-callconv callee (the verifier accepts it) — this is how Rust
  re-enters JIT'd code.
- The **`Value` word + `rt_cons` FFI** round-trips correctly (all engines built
  `shen_eq`-equal lists). Allocation is **not** a wall.

## 0.1 What the spike did NOT prove (these are work items below)

- Fixnum arith was **raw-word** (no tag-check / overflow-to-float guard).
- **GC roots in JIT frames is UNVERIFIED** — the spike ran the heap grow-only
  (collection off). This is the #1 gate before JIT and GC Step 4 coexist.
- Only **self** `return_call` was exercised — not cross-function / mutual tail
  calls, nor `apply` of a runtime closure value.
- Microbench on three isolated shapes — not a whole-suite multiplier.

---

## 1. The seam (concrete, verified against the tree)

| Thing | Location | Note |
|---|---|---|
| `Value(u64)` tags | `value.rs:185` | fixnum `000` (bits = `n<<3`), ptr `001`, sym `010`, nil `011`, bool `100`. |
| `ClosureKind` | `value.rs:113` | `Native(Rc<NativeFn>, Vec<Value>)` / `Lambda` / `Bytecode`. **Add `Jit`.** |
| Hot dispatch | `aot/runtime.rs:56` `call_or_apply` | full-arity match on `&c.kind`; add a `Jit` arm here. |
| Direct table | `interp/eval.rs:112` `aot_direct: Vec<Option<DirectFn>>` | `DirectFn = fn(&mut Interp,&[Value])->ShenResult<Value>` (eval.rs:44). `register_aot_direct`/`get_aot_direct`. |
| Runtime prim helpers | `aot/runtime.rs:151+` | `add/sub/mul/div/lt/.../cons/hd/tl/...` — the exact semantics JIT'd code must match; wrap as `extern "C"`. |
| AOT lowering | `crates/klcompile/src/main.rs` | `.kl` → Rust today; the JIT is an alternative lowering of the same forms. |
| Heap | `gc/{mod,heap,node}.rs` | non-moving mark-sweep; `Gc` is a tagged `u64` bit-compatible with `Value`; grow-only in Step 3. |
| Oracle pattern | `tests/vm_differential.rs` | copy this for the JIT differential test. |
| Measurement | `scripts/cross-port-bench.sh` (SBCL ratio), `scripts/gates.sh` | honest metric is the ratio to shen-cl. |

---

## 2. Staged plan (each stage kill-gated)

### Stage J1 — Tier-in mechanism + ONE hand-written JIT'd kernel function
**Goal**: prove the *integration*, end to end, on the real interpreter — before
building a general code generator.
- Add `ClosureKind::Jit(JitFn, Vec<Value>)` (`JitFn` = a finalized
  `extern "C"` code ptr + arity + the traceable shadow-capture vec, mirroring
  `Native`'s §5 capture list). Trace its captures in `Closure::gc_edges`.
- Own a process-wide `JITModule` + a **code cache keyed by function identity**
  (never re-JIT a body — cf. the VM's 1.2M-recompile bug, memory). The module
  must outlive every finalized pointer.
- Wire `call_or_apply` (`aot/runtime.rs:56`) to call the `Jit` arm with the same
  zero-alloc borrowed-slice convention as `Native`.
- Hand-write Cranelift IR for **one** hot, allocation-light kernel function
  (pick from a fresh `sample` of kernel-tests — a `shen.` continuation or a
  `prolog`/`t_star` leaf), emitting **fixnum tag-checks + `checked_add`-style
  overflow guards** and `extern "C"` calls to `rt::*` for everything else.
- **Gate**: `cargo test` + the new JIT differential oracle green for that
  function; `scripts/cross-port-bench.sh` shows **no regression** (ideally a
  blip of improvement). If integration is uglier than the spike implies, stop
  and reassess here — cheap to bail.

### Stage J2 — A code generator (klcompile-style lowering to Cranelift)
**Goal**: lower an arbitrary supported KL/AOT-IR function shape to Cranelift, not
by hand.
- Reuse klcompile's lowering structure: it already maps KL forms to the `rt::*`
  helper calls; emit Cranelift IR for the same forms instead of Rust source.
- Cover: fixnum arith (inlined, guarded), `cons/hd/tl/=/<...` (inline tag tests +
  `rt::` fallback), `if`/`cond`/`let`/`do`, calls (`call`), and **cross-function
  tail calls via `return_call_indirect`** (the mutual-recursion case the spike did
  not exercise — validate it here on a mutually-recursive pair).
- `apply` of a non-JIT callee (runtime closure, partial, over-application) →
  `rt_apply` FFI back into `Interp::apply`.
- Unsupported forms (`trap-error`, `thaw`, etc.) → **don't JIT that function**
  (fall back to AOT/tree-walk), exactly as the VM bails today.
- **Gate**: JIT differential oracle over a broad corpus = `shen_eq`-equal +
  matching Ok/Err to the interpreter; pick the JIT'd set by a `sample`-driven hot
  list; measure the SBCL ratio. **Kill-criterion**: a *material* whole-suite move
  (the spike's 3.46× is the dispatch-component ceiling, not the suite multiplier —
  set the bar from the profile's dispatch fraction, e.g. target sub-2.5×).

### Stage J3 — The GC-roots-in-JIT-frames gate (BEFORE Step 4)
**Goal**: make JIT'd code safe under a *collecting* heap.
- The spike ran grow-only. JIT frames are ordinary native frames, so the §6g
  conservative native-stack scan (`design/perf-state-and-gc-ladder.md` §6g, the
  passed `gc_roots_aot_spike`) *should* reach them — **but a register-resident
  `Value` not spilled at a safepoint is invisible to a stack scan.**
- Build a spike (extend `gc_roots_aot_spike` / `jit_spike`): JIT'd code holding a
  live heap `Value` in a register across an `rt_*` call that triggers collection,
  asserting the object survives. Decide the spill story:
  - simplest sound option: **spill live `Value`s to scannable stack slots across
    any call that can collect** (Cranelift can be coaxed via the calling
    convention / explicit stack slots), or register JIT code regions + use the
    conservative scan with the membership table (`Heap::is_heap_ptr`).
- **Gate / kill-criterion**: 0 corruption across a collection-stress run with
  JIT'd allocators (like the 10,999-collection `gc_roots_aot_spike`). **JIT and
  GC Step 4 must not both be on until this passes.** (This is also the second
  independent argument against a moving GC.)

### Stage J4 — Tier-up policy + ship
- Decide *what* to JIT and *when*: simplest is compile-on-install for a fixed hot
  set (offline-ish, like AOT). A call-count runtime trigger is a later refinement.
- Final `scripts/gates.sh` ALL GREEN (134/0 both engine modes, workspace tests,
  kernel-aot-audit, fmt, clippy `-D warnings`); Miri on the Rust `rt_*` helpers
  (Miri can't run JIT'd machine code — cover the helpers under Miri, the JIT path
  under the differential oracle). Re-measure the SBCL ratio cool + loaded.

---

## 3. Risks / decisions to make early
- **Overflow semantics**: Shen promotes fixnum overflow to float. Guarded inline
  path must match `rt::add` etc. exactly (`aot/runtime.rs:151`). The guard is a
  predicted branch; it will shave some of the spike's 3.46×.
- **`apply`-heavy hot path**: the type-checker's hottest callees are runtime CPS
  closures (`make_aot_closure` → `Native`). JIT'ing *those* (closure values, not
  statically-named fns) is where the real kernel-tests win is — design the `Jit`
  closure value so these can be JIT'd, not just named `defun`s.
- **Code/memory growth**: cache finalized code; never re-JIT (memory: the VM bug).
- **Cranelift as a runtime dep**: J1+ moves `cranelift-*` from dev- to a real
  dependency (feature-gate it so non-JIT builds stay lean). Mind build time /
  binary size (the crate already tunes opt-level for build cost).

## 4. Definition of done
A measured, gates-green reduction in the `scripts/cross-port-bench.sh` ratio to
shen-cl/SBCL, with the JIT differential oracle green, the J3 safepoint gate
passed, and `benches/jit_spike.rs` retained as the microbench regression anchor.
