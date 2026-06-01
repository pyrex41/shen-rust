# Handoff: Bytecode VM implementation (Phase 2) + current state

**Date**: 2026-05-28
**Branch**: `main`
**Plan of record**: `/Users/reuben/.claude/plans/abundant-splashing-orbit.md`
(also summarized in `PERFORMANCE.md` → "Roadmap to sub-2s")
**Companion design doc**: `design/runtime-execution-strategy.md`

This note hands off the in-flight performance work so another agent (or a
later session) can pick it up cleanly.

---

## 1. Performance state

`scripts/kernel-tests.sh` headline metric (warm, internal "run time"):

| Stage | Time | Notes |
|---|---|---|
| Original baseline | ~17.5s | before any work |
| After Phase 1a (committed) | **~5.5s** | current default (tree-walker) |
| VM opt-in (`SHEN_RUST_VM=1`) | ~5.9s | **slower** — see §4 |

shen-cl reference: ~1.0s. Target: sub-2s.

> Machine was under load during the last measurements (numbers drifted to
> 8–11s across the board); trust the *relative* ordering, not absolute
> values from those runs. Re-measure on a quiet machine.

All 134 kernel tests pass; all 8 gates green — in BOTH the default
(tree-walker) and `SHEN_RUST_VM=1` (bytecode) configurations.

---

## 2. Git state — IMPORTANT

**10 commits are landed** (each gates-green, each its own milestone):

```
09e6529 vm(B4b): inlined-primitive opcodes
8e55cf3 vm(B4a): self-tail-call lowering
e29c42c vm(B3c): multi-frame compiler — lambda/freeze with resolve_var upvals
d6ac249 vm(B3b): MakeClosure + LoadUpval
8f288e5 vm(B3a): ClosureKind::Bytecode variant + dispatch wiring
7fed2ab vm(B2): compiler + Jump/JumpFalse/LoadGlobal — special forms
22f6a4f vm(B1): bytecode VM skeleton
a3e1206 perf: free-variable analysis for lambda/freeze captures (Phase 1a+1d)
c6421a5 docs: update PERFORMANCE.md
e6e861f baseline: shen-rust at 17.5s → ~5.7s
```

**The working tree is dirty with TWO intertwined, uncommitted features**
(`git status`):
- `crates/shen-rust/src/interp/eval.rs`
- `crates/shen-rust/src/vm/exec.rs`
- `crates/shen-rust/src/vm/compiler.rs`
- `crates/shen-rust/src/error.rs`
- `crates/shen-rust/tests/budget_cancel.rs` (new, untracked)
- `design/` (this dir, untracked — the strategy doc + this handoff)

These are **not committed** because they mix two separate efforts that
should land as separate commits:

**(A) B5 — VM wired into `do_defun` (mine):**
- `eval.rs`: `do_defun` now tries `vm::compile_fn` and registers a
  `ClosureKind::Bytecode` on success, falling back to tree-walked
  `ClosureKind::Lambda` on compiler error. Gated by `vm_enabled()`.
- `vm_enabled()` is **opt-in** (`SHEN_RUST_VM=1`), NOT default — because
  the VM is currently slower (see §4).
- `eval.rs` `tail_apply`/`call_strict` + `aot/runtime.rs` `call_or_apply`
  dispatch the `Bytecode` arm into `vm::exec` (these parts ARE in the
  committed B3a).
- `vm/compiler.rs`: added `quote` lowering (`quote_to_value` → `LoadConst`).
- `vm/exec.rs`: locals/stack are `Vec<Value>` (a SmallVec experiment was
  measured *slower* and reverted — see the comment at the `type Locals`
  alias).

**(B) Budget / cancellation (someone else's, in-progress):**
- `error.rs`: a `cancelled`-style `ShenError` constructor.
- `eval.rs`: `Interp` gained `remaining_steps`, `deadline`,
  `deadline_counter` fields + `set_budget`/`set_deadline`/`clear_budget`/
  `charge_step` methods. `eval_in`'s trampoline and `vm::exec`'s loop both
  call `charge_step()?` so long evaluations are cancelable.
- `tests/budget_cancel.rs`: new integration test for that feature.

**Recommended commit split** (use `git add -p` to separate the two within
`eval.rs`):
1. Commit (B) budget/cancellation first (it's orthogonal and self-contained).
2. Commit (A) as `vm(B5): wire bytecode into do_defun (opt-in via SHEN_RUST_VM)`.

Do NOT `git add -A` — it would fuse both features into one commit.

---

## 3. What the VM can do today (Phase 2: B1–B5)

Module: `crates/shen-rust/src/vm/` — `opcode.rs`, `bytecode.rs`,
`compiler.rs`, `exec.rs`. **30 unit tests, all passing**
(`cargo test -p shen-rust --lib vm::`).

Implemented and tested:
- **All KL special forms**: `if`, `let` (with shadowing), `cond`, `do`,
  `and`/`or` (short-circuit), `lambda`, `freeze`, `type`, `quote`.
- **Closures with upvalues**: multi-frame compiler, `resolve_var` walks
  the frame stack registering upvals; `MakeClosure` + `LoadUpval`.
  Two-level nested-lambda capture verified.
- **Self-tail-call**: `SelfTailCall` opcode does in-place arg rebind +
  `pc=0`; 100k-deep tail loop runs without Rust stack growth. Only fires
  in the outermost frame for the true self-call.
- **18 inlined-primitive opcodes** (`Add`/`Sub`/…/`Cons`/`Hd`/`Tl`/
  predicates) mirroring klcompile's `inlinable()` table, with a non-mutating
  `peek_shadowed` so a lexically-shadowed primitive name falls back to a
  normal call.
- **Dispatch**: `ClosureKind::Bytecode(Rc<BytecodeFn>, Vec<Value>)` is
  dispatched by `tail_apply`, `call_strict`, and `rt::call_or_apply`.

NOT yet supported by the VM compiler (callers fall back to tree-walker):
`trap-error`, `thaw`, nested `defun`. Cross-tier general `TailCall` (the
opcode exists but exec returns an error) — only `SelfTailCall` is wired.

---

## 4. The key finding: VM is correct but not yet faster

Measured: VM-enabled kernel-tests is ~5-6% **slower** than the
tree-walker default. Root causes:

1. **kernel-tests is AOT-dominated.** After boot, kernel functions are
   AOT-Rust (Native) and dispatch AOT→AOT through the `apply_direct`
   fn-pointer table. User-`defun` calls — the only place the VM runs —
   are a minority of total calls.
2. **Per-call allocation.** `vm::exec` allocates a `Vec<Value>` locals
   frame + a `Vec<Value>` operand stack on every call. The tree-walker's
   `Scope` COW shares the caller's buffer and allocates *nothing* in the
   common case. That per-call overhead exceeds the bytecode dispatch
   savings for this workload.
3. **SmallVec made it worse**, not better: `Value` is 24 bytes, so an
   8-slot inline frame is ~192 bytes to move/init per call. Reverted.

**Why this is expected to flip after Phase 3:** a tagged `Value(u64)`
(8 bytes vs 24) makes the per-call `Vec<Value>` frames ~3× cheaper to
allocate/move/drop, and makes all the Value clone/drop traffic cheaper.
The VM's structural win (flat integer slots, no alist scan, static
dispatch) should then dominate. **Recommend doing Phase 3 next, then
re-measuring the VM** before investing more in VM micro-optimization.

Alternative VM-side optimization if you want it sooner: a reused
locals/stack buffer pool (thread-local or passed through `exec`), to
kill the per-call `Vec` allocation. This is the single most likely lever
to make the VM beat the tree-walker independent of Phase 3.

---

## 5. Recommended next steps (priority order)

1. **Commit the two uncommitted features** (split as in §2). Get back to
   a clean tree.
2. **Phase 3 — tagged `Value(u64)`** (`/Users/reuben/.claude/plans/...`
   and `PERFORMANCE.md`). Biggest remaining lever: attacks the #1 profile
   leaf (`drop_in_place<Value>` ~800 samples) AND makes the VM's frames
   cheap. Generated AOT kernel code is insulated via `rt::` helpers, so
   only the helpers + ~30-50 hand-written match arms change. Phase it:
   sibling type → switch `rt::` helpers → convert matches file-by-file →
   retire the enum.
3. **Re-measure VM** with `SHEN_RUST_VM=1` after Phase 3. If it now wins,
   flip `vm_enabled()` back to default-on and do B6 (retire the 1 GB
   worker-thread stack in `bin/shen-rust/src/main.rs`, since
   `SelfTailCall` + a future cross-tier `TailCall` remove the deep-Rust-
   stack need) and B7 (decide whether klcompile should emit bytecode
   instead of Rust).
4. **Phase 4 — cons arena** (per-`Interp` bump allocator for cons cells).
5. Deferred Tier-A items (`Phase 1b` shen-cl overrides, `Phase 1c`
   factorise-defun) were dropped because the post-1a profile didn't
   justify them; revisit only if a profile says so.

---

## 6. How to verify anything

- Full gate suite: `bash scripts/gates.sh` (8 gates; must be ALL GREEN).
- VM unit tests: `cargo test --release -p shen-rust --lib vm::`.
- Timing: `cargo build --release -p shen-rust-bin` then 3 warm runs of
  `./target/release/shen-rust --kernel-tests` (read the final
  "run time: N secs" line). Compare default vs `SHEN_RUST_VM=1`.
- Hotspot profile (macOS): background the binary, then
  `/usr/bin/sample <pid> 8 1 -file /tmp/prof.txt -mayDie`; read the
  "Sort by top of stack" section (ignore `__ulock_wait` — that's the
  idle main thread joining the worker).
- Codegen changes require `scripts/codegen-kernel-aot.sh` to regenerate
  `crates/shen-rust/src/aot/kernel/*.rs`; Gate 6 byte-diffs them.

---

## 7. Gotchas learned this session

- The repo had **zero commits** at the start; `e6e861f` is the root.
  `.claude/` is gitignored.
- **mimalloc trap**: as a global allocator it gave ~20% but BREAKS the
  kernel-tests boot when built `default-features = false` (TLS init on the
  spawned 1 GB worker thread — `shen.initialise` silently undefined).
  With default features it works but is slower than no override. Currently
  no global allocator. Revisit in Phase 5 with `local_dynamic_tls` on.
- **opt-level**: release is `opt-level = 2` (sweet spot). opt-3 adds build
  time for no runtime gain (malloc-bound); opt-3+LTO is 8+ min builds.
- The kernel-tests suite is **71% type-checker** (`tc +` on
  `interpreter.shen` 6.6s + `c-minus.shen` 1.8s historically); it proves
  theorems about runtime-loaded user code, which is why making *runtime*
  code fast (the VM) matters even though the kernel itself is AOT'd.
