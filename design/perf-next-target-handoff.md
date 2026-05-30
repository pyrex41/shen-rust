# shen-cedar — Perf next-target handoff (post-J2)

**Date**: 2026-05-30. **Standalone.** Read after `design/jit-productionization-plan.md`
§5 (the J2 closure-JIT falsification) and `design/perf-state-and-gc-ladder.md`
(the ladder + scoreboard). This doc picks the next perf target *by impact* given
that the two biggest execution-engine bets are now both measured non-winners.

---

## 0. The meta-finding that reframes everything

**Swapping the per-body execution engine does not move the kernel-tests metric.**
Both attempts, measured A/B on the *same* one-shot `--kernel-tests` workload the
cross-port scoreboard uses (paired, min-of-3, all 134/0):

| Engine (vs tree-walk baseline ≈ 3.58 s) | kernel-tests | Verdict |
|---|---|---|
| **Bytecode VM** (`SHEN_CEDAR_VM=1`, compiles closures **and** defuns) | ≈ 3.87 s | **neutral / slightly slower** |
| **Closure JIT** (`SHEN_CEDAR_JIT=1`, J2 Slice A) | ≈ 4.13 s | **−15 % regression** |

The VM's "1.3–4× on user code" (memory) is on closure-heavy *micro*-benchmarks;
on the kernel-tests aggregate it washes out. The JIT loses outright (J2 §5: 0 %
JIT→JIT edges, 73 % bail, FFI tax on tiny call-heavy CPS continuations).

**Why neither wins — two compounding structural reasons:**
1. **The cost is the distributed boxed-dispatch model, not the dispatch mechanism
   per body.** kernel-tests runs millions of tiny, allocation-light, call-dominated
   operations over boxed `Value`s. Re-encoding *how* one body dispatches (opcodes
   vs tree-walk vs native) doesn't change the per-op boxed-value work that
   dominates. This is the "distributed execution-model cost" the scoreboard already
   named — now confirmed by elimination from both sides.
2. **The metric is one-shot, so runtime compilation can't amortize.** kernel-tests
   boots, runs once (~3.6 s), exits. The VM and JIT both *compile at runtime*; that
   compile cost (cheap for the JIT — 0.07 s — but the VM's is in-line on the hot
   path) is paid once and offset against an execution win that, for non-compute
   bodies, is small or negative. SBCL pre-compiles ahead of time and pays neither.

**Consequence:** the "only the JIT closes the dispatch gap" thesis (memory) is
falsified *for this workload shape*. The JIT closes it for **compute loops** (the
spike's fib/sumto, J1's `shen.length-h`) — which kernel-tests is not made of.

---

## 1. What is now DEAD (measured; don't re-open without new evidence)

Adds two rows to `perf-state-and-gc-ladder.md` §3:
- **Bytecode VM as a kernel-tests lever** — neutral/slightly slower (above).
- **Closure-value JIT** — −15 % (`jit-productionization-plan.md` §5).

Already-dead (prior): cons recycling (~2.5 %), GC reclamation (~2.4 %), per-call
`Rc::clone` (~0 %), faster `lookup_local` (no win), eliminating cons churn (~2.4 %).

The pattern: **every lever that leaves the boxed-`Value` + interpreted-dispatch
model in place has returned ≤ ~5 %.** The structural 3.55× is not hiding in a
mechanism we can swap — it is the model.

---

## 2. Next targets, ranked by impact (honest estimates)

### A. (Highest info-value) Fresh leaf profile + metric re-grounding — DO THIS FIRST
Every lever to date was funded off the 2026-05-29 profile. The two biggest bets off
it both failed. **Re-profile `--kernel-tests` (release, `/usr/bin/sample $PID`) now**
— post-Step-3 word-`Value`, post-J2 — and answer two questions that decide the
whole next phase:
1. **Where does the time actually go now?** Is it still the `eval_in`/`lookup_local`/
   `eval_args` tree-walk cluster, or has the word-`Value` flip shifted it to
   allocation (`Gc` node alloc/`drop`) or to specific kernel prims? A new lever, if
   one exists, is only findable here.
2. **Is the one-shot metric the right metric?** It structurally penalizes runtime
   compilation (VM/JIT) by never amortizing. The warm answer is **already known**:
   `cargo run --release --bench vm_vs_treewalk` (the paired warm harness) shows the
   VM **1.3–4× on user-code bodies** — so the kernel-tests "neutral" above is a
   *one-shot metric artifact*, not a VM verdict. The VM genuinely wins when compile
   cost amortizes. **So the real open question is a goal/product decision, not a
   measurement: is the target workload one-shot (the cross-port ratio) or
   warm/served (a long-running type-checker / REPL / batch)?** If warm/served, the
   VM is an *already-built, already-134/0, already-1.3–4×* win — shipping it is the
   highest-impact move available and costs only a default flip + the warm metric.
   This decision is the user's; it gates whether B/C/D below are even the right
   axis.

### B. (Concrete, GREENLIT, but low *speed* impact) GC Step 4 — collection ON + roots
The only remaining greenlit ladder rung (`perf-state-and-gc-ladder.md`; both spikes
passed: mark-sweep + Copy word + shadow-stack = 3.34× on lists; AOT-frame
conservative scan + aarch64 reg-flush sound, 10 999 collections / 0 corruption).
- **Speed impact on kernel-tests: small (~2–3 %)** — reclamation is the falsified
  lever. Its real value is **memory** (today's heap is grow-only → leaks all
  garbage; fine for a 6 s process, blocking for big-list / long-running workloads)
  and **completing the GC ladder** so the heap is production-viable.
- **The J3 gate folds in here for free now**: GC-roots-in-JIT-frames (the spike's
  open question) is moot while the JIT is paused — but if Step 4 lands, the precise
  shadow-stack + conservative-AOT hybrid is what J-anything would reuse.
- Do this if the goal is "finish the GC story / unblock big workloads," **not** if
  the goal is the SBCL kernel-tests ratio.

### C. (Low ceiling) Re-aim the JIT at compute-shaped targets
The JIT machinery (committed, `ClosureKind::Jit` + `BodyTranslator`, off by default)
is correct and reusable. Its proven win is compute loops. Candidate: a fixed hot-set
of **self-recursive numeric/list kernel leaves** (the J1 `shen.length-h` shape) JIT'd
**with inline raw-word arith + `return_call`** (not FFI-per-op). But the profile caps
this: the AOT-kernel-leaf cluster was only ~250 of ~1790 samples → **even a perfect
JIT here is ≤ ~10–15 %**, and it needs the inline-arith codegen J2 Slice A skipped.
Only worth it if (A) re-profiling shows a *specific* hot compute leaf dominating.

### D. (Strategic) Re-examine the goal / architecture
If A confirms the cost is irreducibly the boxed-dispatch model on one-shot loads,
then closing 3.55× → ~1× may require what `arch_execution_engine.md` rejected
(AOT-native-compiling loaded code, à la SBCL) — a different project. Worth an
explicit decision: **is the target the one-shot ratio, or a warm/served ratio?**
The answer changes everything above.

---

## 3. Recommendation

**The next move is a decision, not a build: pick the target workload (§2.A.2).**
The engine-swap avenue is exhausted *for the one-shot metric* (VM neutral, JIT
−15 %) but the VM already wins 1.3–4× *warm*. So:

- **If the goal is the one-shot cross-port ratio:** engine swaps are closed. The
  3.55× is the boxed-dispatch model itself; the only honest paths are GC Step 4
  (finish the ladder; ~2–3 % speed but real memory value, **B**) or a goal/arch
  rethink (**D**) — AOT-native-compiling loaded code is the SBCL-shaped answer the
  arch doc rejected; revisit only with eyes open.
- **If the goal is a warm/served workload:** **ship the VM** — flip
  `SHEN_CEDAR_VM` to default-on, gate it behind a fresh warm cross-port harness
  (type-check a corpus N× in one process), confirm 134/0 + the 1.3–4× holds at the
  suite level, and retire the JIT-for-closures line for good.

Either way, **re-profile first** (§2.A.1) so the rung after this is funded off
fresh evidence, not this doc's priors. Do not fund another per-body engine swap
for the one-shot metric — that question is answered.

## 4. State / anchors
- J2 closure-JIT committed off-by-default (`1cf0672`): `SHEN_CEDAR_JIT=1` +
  `SHEN_CEDAR_JIT_STATS=1` for the diagnostics; `tests/jit_closure_differential.rs`
  is the oracle; `benches/jit_spike.rs` records where the JIT *does* win.
- VM: `SHEN_CEDAR_VM=1`, `tests/vm_differential.rs` is its oracle.
- Measure with `scripts/cross-port-bench.sh` (the honest SBCL ratio) — but see §2.A
  on whether one-shot is the right shape. Gates: `scripts/gates.sh` (134/0 both
  engine modes + fmt/clippy/kernel-aot-audit).
