# shen-rust performance

> **Current state (2026-05-28):** `--kernel-tests` runs in **~5.7s warm**
> (134/0 passing, all 8 gates green), down from the 17.5s starting baseline
> documented below — **≈3.1× faster** through ten stacked surgical wins. The
> goal is now **sub-2s** (within 2× of shen-cl's ~1s). The path there is a
> bytecode VM + Value-representation overhaul + cons arena. The original
> profile and Tier roadmap from the handoff are preserved below for
> historical context.

## Status

**Done (committed at this baseline):**

| Optimization | Effect |
|---|---|
| Locals-by-reference + `Scope` COW (T1a/T1b) | killed the quadratic per-arg `locals.clone()` |
| Vec-indexed function/global tables (T2a) | replaced HashMap probes with SymId indexing |
| Single-allocation cons (`Value::Cons(Rc<(Value, Value)>)`) | halved per-cell allocations |
| `opt-level = 1 → 2` | ~15% on the kernel-test workload |
| **A1 — no-alloc dispatch** (skip `total` Vec rebuild when `partial` is empty) | ~31% — killed the #1 profile hotspot |
| FNV interner | ~5%, zero-risk |
| Zero-alloc call ABI (`rt::apply_named` / `apply_value` take `&[Value]`) | enabled the slice-arg path |
| Pointer-keyed `intern_static` (per-`Interner` cache) | ~15% — killed the residual SipHash on AOT call sites |
| `SmallVec<[Value; 4]>` `ArgVec` (T1d) | no-heap arg vectors for ≤4 args |
| Direct AOT fn-pointer table (`apply_direct` + `Interp::aot_direct`) | true SymId pre-intern: codegen emits `rt::apply_direct(interp, "name", &[..])` which hits a fn-pointer table at install time |
| klcompile `SLOW_DEFUNS` skip list | leaves ~4 known-slow boot-only defuns tree-walked (saves >5min of LLVM time) |

**Where the wall is.** Post-fix profile is led by:

- `drop_in_place<Value>` ≈ 857 samples — cons + value churn (intrinsic to refcounted `Rc<(Value, Value)>` traversal)
- Tree-walker: `eval_in` ≈ 376 + `lookup_local` ≈ 241 — the **type-checker** running runtime-built `freeze`/`lambda` continuations
- Diffuse allocation: captured envs (`locals.to_vec()` snapshots whole scope), per-call SmallVec spills

71% of the suite is `(tc +)` then `(load …)` on two user files (`interpreter.shen` 6.6s, `c-minus.shen` 1.8s). The hot loop is the Shen type-checker proving theorems about **runtime-defined** user code — those defuns/lambdas/freezes are *never* AOT-compiled and run through the tree-walker. `apply_direct` accelerates AOT-to-AOT but cannot help once a Lambda closure (tree-walked body) is invoked.

## Roadmap to sub-2s

**Phase 1 — surgical consolidation** (≈1–2 weeks):
- 1a. **Free-variable analysis** for `build_lambda`/`build_freeze` — capture only syms the body references, not the entire visible scope. Applies to both the tree-walker (`crates/shen-rust/src/interp/eval.rs`) and klcompile (`crates/klcompile/src/main.rs`). Shrinks captured envs and the subsequent `lookup_local` scans.
- 1b. **More shen-cl hot overrides** in `register_hot_overrides` (`crates/shen-rust/src/primitives.rs`): `shen.str->bytes` / `shen.bytes->string` / `shen.rfas-h` (reader O(N²)→O(N)), `shen.macroexpand-h` (`Rc::ptr_eq` fast path), `shen.analyse-symbol?` / `symbol?` / `variable?` (char-class checks). Priority by re-profile after 1a.
- 1c. **Pattern factorization** in klcompile (mirror shen-cl `factorise-defun` from `../shen-cl/src/overwrite.lsp`): group cascading `(cond ((and X …) …))` clauses sharing a leading test into a nested cond.
- 1d. Delete the dead `Control` enum left in `eval.rs` from an exploratory refactor.

**Phase 2 — bytecode VM** (the architectural play, multi-week):

Compile user defuns/lambdas/freezes to bytecode at definition time. This is the only way to escape the tree-walker tax for the dominant type-checker workload. shen-go's design reports 4–8× from the VM alone + 2–3× from numeric fast-paths.

- New module `crates/shen-rust/src/vm/{opcode, bytecode, compiler, exec}.rs`.
- ~22 opcodes mirroring shen-go (`../shen-go/kl/vm.go:10-34`): `LoadLocal`, `StoreLocal`, `LoadUpval`, `LoadConst`, `LoadGlobal`, `Jump(i16)`, `JumpFalse(i16)`, `Call(u8)`, `TailCall(u8)`, **`SelfTailCall(u8)`** (in-place loop, no Rust stack growth), `MakeClosure(u16, u8)`, plus fixnum-fast-path numeric ops (`Add`/`Sub`/`Mul`/`Lt`/`Le`/`Gt`/`Ge`/`Eq`/`Not`) and Shen-specific `Cons`/`Hd`/`Tl`/`IsCons`/`Truthy`.
- Flat per-call frames: `Vec<Value>` locals indexed by integer slot (parameters in `[0..arity)`, lets in `[arity..n_locals)`). Upvalues snapshot by value at closure creation.
- Compiler (per shen-go's `../shen-go/kl/compiler.go`): per-fn `locals: FxHashMap<SymId, u16>` + `upvals: Vec<UpvalInfo>` + `outer: Option<&Compiler>` chain. `resolve_var(sym)` returns `Local`/`Upval`/`Global`; nested lambdas register upvals upward.
- New `ClosureKind::Bytecode(Rc<BytecodeFn>, Vec<Value>)` variant in `crates/shen-rust/src/value.rs`. `rt::call_or_apply` dispatches it.
- Phased commits (B1 skeleton, B2 special forms, B3 closures, B4 self-tail + numeric, B5 wire into `do_defun`/`build_lambda`/`build_freeze` with `SHEN_RUST_NO_VM` fallback, B6 retire the 1 GB stack workaround, B7 AOT/VM split decision).

**Phase 3 — Value representation overhaul** (after VM lands):

Replace `enum Value` (24 bytes) with `struct Value(u64)` NaN-boxed (8 bytes). Halves memory traffic on every move/clone/drop, makes Int/Bool/Nil/Sym immediates zero-overhead, shrinks cons cells from 48-byte payload to 16-byte. Generated kernel code is **insulated** from this via `rt::` helpers (`hd`/`tl`/`is_cons`/etc.) — only the helpers and ~30–50 hand-written match arms change. Phased: define sibling type, switch `rt::` helpers, convert hand-written matches file-by-file, retire the enum.

**Phase 4 — cons arena:** Per-`Interp` bump arena for cons cells with inline refcount + freelist. Eliminates global allocator traffic on the dominant data type. Phased: `ConsArena` + `ConsRef`, switch `Value::cons`, per-`Interp` init thread-through, freelist + optional compaction.

**Phase 5 — validation + stretch:** revisit mimalloc (the prior attempt broke under SmallVec/`apply_direct` because of TLS init on the spawned 1 GB-stack thread — the VM retires that workaround), thin LTO, AOT pattern decision trees, `LoadGlobal` inline caches.

## Verification

Every commit must hold all 8 gates green (`scripts/gates.sh`) and be re-timed with 3 warm runs of `--kernel-tests` (median + variance). Hotspot validation via `/usr/bin/sample $PID 8 1` on the two heavy `(tc +)`/`(load …)` cases.

---

## Original handoff (preserved for historical reference)

Status at handoff: **all 134 kernel tests pass**, but shen-rust (release)
runs the kernel test suite in **~17.5 s vs shen-cl's ~1.0 s — ~17× slower**.
This document is the plan to close that gap. It's grounded in a real
profile (not guesses) plus an architecture survey of the other ports.

---

## 1. The profile says: we're allocation-bound, not logic-bound

Sampled `target/release/shen-rust --kernel-tests` for 12 s
(`sample <pid> 12 1`). Of 7879 samples on the worker thread, by
self-time:

| Bucket | ~Samples | ~% | What |
|---|--:|--:|---|
| malloc / free / memset / bzero | ~2400 | **~30%** | `_xzm_free` 991, `free` 247, mallocs 514, raw_vec alloc/dealloc 285, memset/bzero 292 |
| `Value::clone` + `Value::drop_in_place` + `Vec::clone` | ~1960 | **~25%** | Rc churn + recursive deep copies of `Value` |
| Hashing (`sip::Hasher`, `hash_one`, `DefaultHasher`) | ~615 | **~8%** | `HashMap<SymId,_>` probes per call |
| `Interner::intern` | 122 | ~1.5% | **runtime** string→SymId on the hot path |
| interpreter logic (`eval_in` 602, `apply` 133, `call_strict` 116, `tail_apply` 86, `eval_args` 52) | ~990 | ~13% | the actual tree-walk |
| real kernel work (`aot_*`, `shen_eq`, primitives) | rest | small | each AOT fn is a few % at most |

**Headline: ~63% of CPU is allocation + clone/drop + hashing — pure
overhead, not computation.** The interpreter logic itself is ~13%. We
don't have a "slow algorithm" problem; we have a "we allocate and copy
on every single step" problem.

### The single worst offender (quadratic, fixable today)

`crates/shen-rust/src/interp/eval.rs`:

```rust
pub type Locals = Vec<(SymId, Value)>;          // line 33

fn eval_args(&mut self, args: &[KlExpr], locals: &Locals) -> ShenResult<Vec<Value>> {
    for a in args {
        out.push(self.eval_in(a, locals.clone())?);   // line 278: clones ALL locals per arg
    }
}
```

`eval_in` takes `Locals` **by value** and `eval_args` clones the whole
locals vector once *per argument*. A call with N args in a scope with M
locals does O(N·M) `(SymId, Value)` clones — each `Value` clone is an Rc
bump or a deep copy. `eval_in` also does `expr.clone()` (line 150) and
`items.clone()` (line 167) on every node. This is most of the 25%
clone/drop bucket.

---

## 2. What the fast ports do (survey)

| Port | Host | KL execution | Values | Dispatch |
|---|---|---|---|---|
| **shen-cl** (fastest, most robust) | SBCL | KL → Lisp **source** → `compile` to native | CL native | direct Lisp call, no map |
| **shen-scheme** | Chez | KL → Scheme **source** → `eval`/JIT | Scheme native | direct call |
| **shen-go** (closest to us) | Go | KL → **bytecode VM** at define-time | pointer-tagged structs; **fixnums are synthesized pointers, never allocated** | `symbol.function` **direct slot**, no map |
| **Shen.java** (hraberg) | JVM | KL → JVM bytecode via ASM | numeric tag in a `long` | `invokedynamic` + `SwitchPoint` |
| **shen-rust** (us) | Rust | **tree-walk** (user code never compiled); kernel AOT'd to Rust but still routes through `apply_named` | `enum Value` with `Rc` everywhere | `HashMap<SymId,Value>` probe + `Rc<Closure>` clone per call |

Two reusable lessons:

1. **The fast ports don't interpret at steady state.** shen-cl/scheme
   emit host source and let the host compiler produce native code;
   shen-go compiles each `defun` to bytecode once. We tree-walk forever.
   Rust has no runtime compiler, so "emit host source" is off the table
   for user code — but **"compile the KL AST to a tree of Rust closures
   once at load time"** (Neil Mitchell's technique, ~33% over AST-walk on
   its own; near-bytecode speed) and **"compile to a bytecode VM"**
   (shen-go, cites 4–8× on tak/fib) both work for us.
2. **shen-go's two cheap structural wins map directly onto our profile:**
   - **Function table = direct slot, not a hashmap.** Each symbol carries
     its bound function; a call is one pointer deref. We hash a `SymId`
     into a `HashMap` on every call (the 8% hashing bucket).
   - **Fixnums are never boxed.** Small ints are synthesized pointers.
     We `Rc`-share/clone `Value::Int` like everything else.

The Shen wiki's three "easy" wins (native `hash`, `=` specialization,
bulk file I/O) are **already done** (Phase 8A/8B). The remaining gap is
structural.

---

## 3. The plan, in priority order (each independently shippable)

Ordered by (measured impact ÷ effort). Tiers 1–2 are "make the
tree-walker stop wasting memory" and need **no architecture change** —
do these first, they're days not weeks and should roughly halve the
gap. Tier 3 is the real fix.

### Tier 1 — Stop allocating in the tree-walker (~30% + 25% buckets)

**T1a. Pass `Locals` by reference; stop cloning it per arg.**
`eval.rs`. Change `eval_in(&mut self, expr, locals: Locals)` →
`eval(&mut self, expr: &KlExpr, locals: &Locals)`. Kill the
`locals.clone()` in `eval_args` (line 278) and `step` (line 270). The
trampoline re-entry currently *mutates* `locals`, so you'll need a small
refactor: keep a single owned `Locals` in the driver loop and pass
`&Locals` down; only `let`/`lambda` extend it, and they can push/pop
instead of cloning. **Effort: M. Expected: large — this is the
quadratic-clone fix.**

**T1b. `Locals` as a push/pop scope stack, not a fresh `Vec` per frame.**
Lookups are already a reverse linear scan (`lookup_local`), so a single
growable stack with saved lengths works: `let` pushes one binding and
records the old length, restores on exit. Eliminates per-scope `Vec`
allocation. **Effort: M. Expected: large (kills much of the malloc
bucket).**

**T1c. Don't `clone()` the AST while walking.** `eval_in` does
`expr.clone()` (line 150) and `items.clone()` (line 167) so it can own
`current` for trampolining. `KlExpr::App` is `Rc<[KlExpr]>` so the clone
is "only" an Rc bump + enum copy, but it's per-node. Hold `&KlExpr` and
make the trampoline carry an `Rc<[KlExpr]>` slice index instead of a
cloned node. **Effort: M. Expected: medium.**

**T1d. `SmallVec<[Value; 4]>` for argument vectors.** `eval_args`
returns `Vec<Value>`; ~90% of calls have ≤4 args. Add the `smallvec`
crate, return `SmallVec<[Value;4]>`. Also applies to the AOT codegen
(`klcompile` emits `vec![...]`). **Effort: S. Expected: medium (trims
the raw_vec alloc/dealloc 285-sample slice).**

### Tier 2 — Stop hashing and runtime-interning (~8% + 1.5% buckets)

**T2a. Function table + globals as `Vec<Option<Value>>` indexed by
`SymId`, not `HashMap`.** `SymId` is a dense, sequential `u32`
(`symbol.rs`: `SymId(self.names.len() as u32)`), so array indexing is
valid and O(1) with no hashing. Change `env.rs` `functions` / `globals`
from `HashMap<SymId, Value>` to `Vec<Option<Value>>` that grows with the
interner. This is shen-go's "direct slot" idea and directly removes the
8% hashing bucket. Keep `properties` as a map (the `(SymId,SymId)` key is
sparse). **Effort: S–M. Expected: medium-large.**

**T2b. Pre-intern SymIds at codegen / parse time.** `apply_named` and
the AOT output call `interp.intern("name")` at runtime (122 samples).
In `klcompile`, emit a per-module `static SYMS: OnceLock<…>` populated in
`install`, and an `apply_named_id(interp, SymId, &[Value])` variant that
skips interning. For the tree-walker, the parser already hands back
`SymId`s, so this is mostly an AOT-path fix. **Effort: S. Expected:
small-medium.**

### Tier 3 — Compile the AST (the structural fix; pick ONE)

This is what actually closes the gap to single-digit-× of shen-cl. Both
options eliminate the repeated special-form dispatch (`step`'s big match)
and let arguments/locals live in flat slots.

**T3a. Closure-tree compilation (recommended first — lower risk).**
Walk each KL function body once at install time and produce a
`Box<dyn Fn(&mut Interp, &mut Frame) -> ShenResult<Value>>` tree (a
"compiled thunk" per node). Variables become integer slot indices into a
flat `Frame`, resolved at compile time. Subsequent calls invoke the
closure tree — an indirect call the branch predictor loves — instead of
re-matching the AST. Neil Mitchell measured ~33% over AST-walking and
near-bytecode speed; combined with Tier 1/2 it should land us in the
~4–6× range. **Effort: L. Expected: large.** Reference:
neilmitchell.blogspot.com/2020/04/writing-fast-interpreter.html

**T3b. Bytecode VM (shen-go's choice — higher ceiling, more work).**
Compile each `defun` to a flat `Vec<Instr>` with slot-indexed locals,
a stack machine, compile-time-resolved special forms, arithmetic
intrinsic opcodes, and `OP_TAIL_CALL`/`OP_SELF_TAIL_CALL` for TCO without
growing the Rust stack. shen-go cites 4–8× over its old tree-walker. See
`/Users/reuben/projects/shen/shen-go/kl/{vm.go,compiler.go}` and
`shen-go/thoughts/shen-go-compiler-design.md` for a working blueprint in
a sibling compiled-host port. **Effort: XL. Expected: largest.**

Note: we already have `klcompile` (KL→Rust source, AOT) for the *kernel*.
That's the shen-cl strategy but at build time. It only helps the kernel,
not user code typed at the REPL or `(load)`ed — those still tree-walk.
T3a/T3b are what make *user* code fast. (You could also extend klcompile
to AOT user code on `(load)` by shelling out to `rustc` + `dlopen`, but
that's a heavy, fragile path; closure-tree/bytecode is self-contained.)

### Tier 4 — Value representation (deferred; biggest blast radius)

**T4. Tagged-pointer / fixnum-no-alloc `Value`.** shen-go synthesizes
small-int pointers so arithmetic never allocates; we `Rc`-everything.
Re-representing `Value` (NaN-boxing, or `enum` with an inline small-int
fast path that never touches `Rc`) would cut the remaining alloc/clone
cost, but it touches every file that matches on `Value`. **Do this last,
only if Tiers 1–3 leave us short of target.** Don't open this Pandora's
box first.

---

## 4. Suggested sequencing

1. **T1a + T1b together** (locals by-ref + scope stack) — one focused PR.
   Re-profile. This should be the biggest single jump and validates the
   "we're allocation-bound" thesis.
2. **T2a** (Vec function table) + **T1d** (SmallVec args) — a second PR.
3. Re-benchmark with `scripts/cross-port-bench.sh`. If we're under ~6×,
   that may be "good enough" to declare victory for now.
4. If not, **T3a** (closure-tree). This is the project that gets us to
   single-digit-× and is the natural long-term architecture.
5. **T2b**, **T3b**, **T4** only as needed.

After each PR: `scripts/gates.sh` must stay green (all 7 gates, incl.
kernel-tests = 134/0), and `cargo test --release --test aot_smoke`
must still show the loop-sum speedup as a regression guard.

---

## 5. Reproducing the profile

```bash
cargo build --release --bin shen-rust
./target/release/shen-rust --kernel-tests >/tmp/run.log 2>&1 &
sample $! 12 1 -file /tmp/sc.txt          # 12s @ 1ms
# Read the "Sort by top of stack" section of /tmp/sc.txt for self-time.
```

`scripts/cross-port-bench.sh` runs the same workload against shen-cl for
the head-to-head number. shen-cl lives at
`../shen-cl/bin/sbcl/shen`.

---

## 6. Key files

| Concern | File |
|---|---|
| Tree-walker (Tier 1/3 target) | `crates/shen-rust/src/interp/eval.rs` |
| `Locals`, `eval_in`, `eval_args`, `step`, `tail_apply` | same, lines 33 / 149 / 275 / 196 / ~280 |
| Env (Tier 2a target: HashMap→Vec) | `crates/shen-rust/src/env.rs` |
| Symbol interner (dense SymId) | `crates/shen-rust/src/symbol.rs` |
| `Value` (Tier 4 target) + `shen_eq` | `crates/shen-rust/src/value.rs` |
| AOT codegen (Tier 1d/2b target) | `crates/klcompile/src/main.rs` |
| AOT runtime (`apply_named`, inline helpers) | `crates/shen-rust/src/aot/runtime.rs` |
| Native hot-fn overrides (done, Phase 8A) | `crates/shen-rust/src/primitives.rs` `register_hot_overrides` |
| Benchmark harness | `scripts/cross-port-bench.sh`, `tests/aot_smoke.rs` |
| shen-go blueprint (bytecode VM) | `../shen-go/kl/{vm,compiler,types,eval}.go`, `../shen-go/thoughts/` |
| shen-cl blueprint (KL→native) | `../shen-cl/src/{compiler.shen,primitives.lsp}` |
