# shen-cedar — JIT Stage J1 Implementation Handoff (tier-in + first JIT'd kernel fn)

**Date**: 2026-05-29. **Standalone** — a fresh agent can execute this cold.
**This is Stage J1 of `design/jit-productionization-plan.md`** (read that for the
full staged arc). The spike that greenlit it is committed (`128a906`,
`benches/jit_spike.rs`); its result is in `design/jit-spike-handoff.md` §10.

**Read first, in order:**
1. `design/jit-productionization-plan.md` — the staged plan + the seam table. J1
   is the first row.
2. `design/jit-spike-handoff.md` §10 — what the spike proved/didn't. **The spike's
   `benches/jit_spike.rs` is your working Cranelift reference — lift the
   `JITModule` setup, the `CallConv::Tail`/`return_call` tail body, the
   default-callconv entry trampoline, the `rt_cons` symbol registration, and the
   `finalize_definitions`/`get_finalized_function`/`transmute` dance directly from
   it. It compiles and runs today.**
3. `design/perf-state-and-gc-ladder.md` §10/scoreboard — the honest metric is the
   `scripts/cross-port-bench.sh` ratio to shen-cl/SBCL (currently ~3.55×).

---

## 1. J1 goal and kill-gate

**Goal: prove the *integration* end-to-end on the real interpreter — one hot
kernel function actually executing as JIT'd native code in a normal kernel-tests
run — BEFORE building a general code generator (that's J2).**

**Kill-gate (stop/reassess if not met):**
- `scripts/gates.sh` ALL GREEN (134/0 both engine modes, workspace tests,
  kernel-aot-audit, fmt, clippy `-D warnings`) with the JIT path active.
- A new **JIT differential test**: the chosen function, run JIT'd, returns
  `shen_eq`-equal results *and* matching `Ok`/`Err` to the interpreted version
  across a corpus.
- `scripts/cross-port-bench.sh` shows **no regression** (a blip of improvement is
  the hoped-for signal, but J1's bar is "integrates cleanly + correct", not a big
  number — one function won't move the suite).

If the integration turns out uglier than the spike implies, **bail here** — it's
the cheap place to learn that.

---

## 2. The seam (verified file:line anchors, this tree)

| Thing | Location | What you do with it |
|---|---|---|
| `Value(u64)` tags | `crates/shen-cedar/src/value.rs:185` | fixnum `000` (bits `n<<3`), ptr `001`, sym `010`, nil `011`, bool `100`. `as_int` = `(bits as i64) >> 3`. `#[repr(transparent)]` + `Copy` → `transmute` to/from `i64` is sound (the spike does this: `w2v`/`v2w`). |
| `Value::to_gc`/`from_gc` | `value.rs:211/217` | `pub(crate)` — fine if your JIT engine lives *in* the crate (it must, see §3). |
| Direct-call table | `interp/eval.rs:112` `aot_direct: Vec<Option<DirectFn>>` | **Tier-in point for J1.** `DirectFn = fn(&mut Interp,&[Value])->ShenResult<Value>` (`eval.rs:44`). `register_aot_direct(name, f)` (`eval.rs:337`), `get_aot_direct(sym)` (`eval.rs:348`). |
| `call_or_apply` | `aot/runtime.rs:56` | the hot dispatch; on the direct-table path it already routes named calls to `aot_direct`. **J1 needs no change here** (you register a shim DirectFn). A `ClosureKind::Jit` arm here is **J2** (JIT'ing closure *values*). |
| `rt::` prim helpers | `aot/runtime.rs:151+` | `add/sub/mul/div/lt/gt/lte/gte/eq/cons/hd/tl/is_cons/...` — the **exact semantics** your JIT must match (incl. fixnum→float overflow in `add`/`sub`/`mul`). Wrap these as `extern "C"`. |
| `make_aot_closure` / CPS continuations | `aot/runtime.rs:102` | the type-checker's hottest callees are these `Native` closures. **Not J1** — but pick your J1 function knowing the *real* win is JIT'ing these (J2). |
| GC heap | `gc/{mod,heap,node}.rs` | non-moving, **grow-only in Step 3** (collection OFF). J1 inherits this → **J1 has zero GC-root exposure** if you JIT a *named* fn (no captured `Value`s); the safepoint problem is J3. Keep it that way. |
| klcompile | `crates/klcompile/src/main.rs` | how `.kl` lowers to Rust today; J2's general codegen mirrors it. Read for the form→`rt::` mapping when you get to J2. |
| Differential oracle pattern | `tests/vm_differential.rs` | copy its structure for the J1 JIT oracle. |
| Spike (working Cranelift) | `crates/shen-cedar/benches/jit_spike.rs` | lift the engine scaffolding. |

---

## 3. The J1 build, step by step

### 3a. Make Cranelift a real (feature-gated) dependency
Today `cranelift-*` are **dev-deps** (bench only). J1 needs them in the crate.
Add a `jit` cargo feature and put `cranelift-codegen/-frontend/-jit/-module` +
`cranelift-native` under it (optional deps). Gate all JIT code with
`#[cfg(feature = "jit")]`. Rationale: non-JIT builds stay lean; gates can run
both with and without the feature; binary-size/build-time stays opt-in. Decide
whether the default `shen-cedar` binary enables `jit` (probably yes once J1 is
green, but land it off-by-default first so gates are bisectable).

### 3b. A `JitEngine` that owns the module + code cache
New module `crates/shen-cedar/src/jit/` (feature-gated). It owns:
- the long-lived `JITModule` (**must outlive every finalized code pointer** — a
  dropped `JITModule` frees the code → UB; this is why the engine lives in the
  crate / on `Interp`, not in a bench `main`),
- a cache `name → finalized fn ptr` so a body is **never re-JIT'd** (memory: the
  VM's 1.2M-recompile bug — do not repeat it),
- the `extern "C"` runtime symbols registered via `JITBuilder::symbol(...)`.

Store it as `Interp { jit: Option<Box<jit::JitEngine>> }` (feature-gated field),
populated during boot (`interp/boot.rs`, near `aot::kernel::install_all`).

Lift `JitEngine::new()` from the spike's `Jit::new()` (settings → `cranelift_native`
isa → `JITBuilder::with_isa` → register symbols → `JITModule::new`).

### 3c. The runtime FFI surface (`extern "C"` over `rt::`)
JIT'd code inlines fixnum arith + tag tests and FFIs for everything else. Define
`extern "C"` wrappers (in `jit/` or `aot/runtime.rs`) over the existing `rt::`
semantics, e.g.:
```
extern "C" fn rtj_cons(h: u64, t: u64) -> u64               // = rt::cons
extern "C" fn rtj_hd(interp: *mut Interp, v: u64) -> u64     // = rt::hd, but see error ABI
extern "C" fn rtj_apply(interp: *mut Interp, f: u64, args: *const u64, n: usize) -> u64
extern "C" fn rtj_add(a: u64, b: u64) -> u64                 // = rt::add (incl. overflow→float)
... etc, only for the ops your chosen function uses.
```
Keep them `#[inline(never)]`, stable ABI, `u64` words + `*mut Interp`.

**Error ABI — decide this in J1 (real kernel fns can error):**
- Recommended general answer: a **`pending_error: Option<ShenError>` slot on
  `Interp`** (or thread-local). An `rtj_*` helper that fails sets the slot and
  returns a sentinel word (e.g. `nil`); the **Rust entry shim (§3e) checks the
  slot after the JIT call** and converts to `Err`. JIT'd code treats helper
  results normally; you do *not* need to thread `Result` through Cranelift.
- Cheaper J1-only alternative: pick a **total** first function (no error path) so
  the ABI is pure `word→word`, then add the pending-error slot before J1 is
  "done". Either is fine; the pending-error slot is the one you'll keep.

### 3d. Hand-write Cranelift IR for ONE chosen kernel function
**Pick the function from a fresh profile**, not a guess:
- `sample` the kernel-tests worker thread (or use the existing profiling notes in
  `design/perf-state-and-gc-ladder.md` §3). Choose a **hot, allocation-light,
  fixnum/list leaf** in `core`/`prolog`/`t_star` — small body, called a lot,
  ideally self-recursive (so you also exercise `return_call`). Good shapes:
  list-length/`shen.`-counter style, a numeric comparison leaf, a `cons`-walk.
- Emit it with the spike's patterns: fixnum ops as raw-word `iadd`/`isub` **but
  now guarded** — a fixnum tag-check (`band` with `0b111`, branch to a slow
  `rtj_*` path if not fixnum) and an **overflow guard** matching `rt::add`'s
  `checked_add`→float fallback (`iadd_coflow`/`icmp` or call `rtj_add` on the
  overflow edge). The unguarded raw-word path was a *spike* shortcut; production
  must match `rt::` semantics exactly or the differential oracle will (correctly)
  fail.
- Tail self-calls via `CallConv::Tail` + `return_call` + the default-callconv
  entry trampoline (proven in the spike). Cross-function tail calls
  (`return_call_indirect`) are J2 — for J1 a single self-recursive fn is enough.

### 3e. Tier in via the existing direct table (no `ClosureKind` change)
Register a **Rust shim `DirectFn`** for the chosen name:
```
fn jit_shim_<name>(interp: &mut Interp, args: &[Value]) -> ShenResult<Value> {
    let code: extern "C" fn(*mut Interp, *const u64, usize) -> u64 = /* from engine cache */;
    let w = code(interp as *mut _, args.as_ptr() as *const u64, args.len());
    if let Some(e) = interp.take_pending_error() { return Err(e); }   // see §3c
    Ok(/* w2v */ unsafe { transmute(w) })
}
```
Install it with `interp.register_aot_direct("<name>", jit_shim_<name>)` during
boot, **after** `aot::kernel::install_all` (so it overrides the AOT version for
that one name), behind `#[cfg(feature="jit")]` + an env guard
(e.g. `SHEN_CEDAR_JIT=1`) so it's trivially A/B-toggleable like the VM
(`SHEN_CEDAR_VM`). This reuses the exact hot path the kernel already takes
(`apply_direct` → `get_aot_direct` → indirect call) — that's the whole point of
tiering in here.

### 3f. JIT differential oracle
New `tests/jit_differential.rs` (pattern from `tests/vm_differential.rs`): for a
corpus of inputs to the chosen function, assert the JIT'd result is `shen_eq`-equal
**and** Ok/Err-matching to the interpreted (AOT/tree-walk) result. Wire it into
gates.

---

## 4. Gates, measurement, conventions
- **Gate**: `scripts/gates.sh` green **with `--features jit` and the env guard on**,
  and still green with it off. Miri can't run JIT'd machine code → cover the
  `rtj_*` Rust helpers under Miri; cover the JIT path with the differential oracle.
- **Measure**: `scripts/cross-port-bench.sh`, paired/alternating/min-of-N, same
  machine state, assert 134/0 (a broken run gives a *void* fast time). J1 bar:
  **no regression**.
- **Repo conventions**: commits on `main` for perf work (user-authorized; **confirm
  scope before committing**). `includeCoAuthoredBy:false` → **no Co-Authored-By
  trailer**. New `unsafe` (code-ptr transmutes, FFI): dedicated tests + Miri on the
  Rust side.

## 5. Explicit boundaries — what J1 is NOT
- **Not** a general code generator — that's **J2** (klcompile-style lowering,
  arbitrary forms, `return_call_indirect`, JIT'ing closure values via
  `ClosureKind::Jit`).
- **Not** turning GC collection on — that's GC **Step 4**. J1 stays grow-only and,
  by JIT'ing a *named, capture-free* fn, has **zero GC-root exposure**. The
  roots-in-JIT-frames safepoint problem is **J3** and gates Step 4, not J1.
- **Not** "JIT the hottest thing" (the CPS `Native` closures) — that needs the
  closure-value path (J2). J1 proves the plumbing on one named fn.
- **Not** a suite-level speedup claim — one function won't move 134 tests; J1's
  deliverable is *correct, clean integration* + the toggle + the oracle.

## 6. First move
Lift `JitEngine` from the spike, feature-gate it, wire the `pending_error` slot +
`rtj_*` helpers for a **total** numeric/list leaf, JIT that one function, register
the shim behind `SHEN_CEDAR_JIT=1`, get the differential oracle green, then run
`scripts/cross-port-bench.sh` with the toggle on vs off and confirm no regression.
Report the diff and the toggle-on/off ratio before expanding to a second function.
