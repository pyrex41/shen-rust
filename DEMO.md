# shen-rust — an executable tour

*2026-06-10T22:21:37Z by Showboat 0.6.1*
<!-- showboat-id: 2276776a-fb7c-439a-8512-fc2b997a3107 -->

[shen-rust](https://github.com/pyrex41/shen-rust) is a port of the
[Shen](https://shenlanguage.org/) programming language to Rust — a functional
language with pattern matching, an integrated logic engine, and an optional
sequent-calculus type system — plus first-class
[AWS Cedar](https://www.cedarpolicy.com/) authorization integration.

This document is **executable proof**: every code block below was run against
the repo, and its captured output is what the command actually printed.
Re-verify the whole tour at any time with
[showboat](https://github.com/simonw/showboat): `showboat verify DEMO.md`.
(Requires Rust ≥ 1.85; the GC demo block expects aarch64 macOS/Linux.)

## Build and conformance

One workspace, plain cargo. The build AOT-compiles the entire vendored
ShenOSKernel-41.2 (21 KLambda files) to Rust via `crates/klcompile`.

```bash
cargo build --release --quiet --bin shen-rust && echo build ok
```

```output
build ok
```

The proof that this is Shen, not a dialect: the upstream kernel test suite,
end to end (boots the kernel, loads the official harness, runs all 134 tests).

```bash
./target/release/shen-rust --kernel-tests 2>&1 | grep 'passed:'
```

```output
kernel-tests: passed: 134, failed: 0
```

## The language

The REPL reads from stdin, so it pipes. Pattern-matching function definitions,
big-integer-free honest semantics (fixnums promote to floats on overflow,
matching the other ports), partial application, higher-order functions:

```bash
printf '(define fact 0 -> 1 N -> (* N (fact (- N 1))))\n(fact 20)\n(map (* 2) [1 2 3 4 5])\n(let Add3 (+ 3) (Add3 39))\n' | ./target/release/shen-rust

```

```output
shen-rust booting kernel… ready.

Shen 41.2, ©2021–2026 Mark Tarver  (shen-rust 0.1.0, Rust)

(0-) (fn fact)
(0-) 2432902008176640000
(0-) (2 4 6 8 10)
(0-) 42
(0-) 
```

Shen's signature feature: the optional type system, a sequent-calculus theorem
prover that runs *in* the language. `(tc +)` turns it on; the `{...}` signature
is then statically verified — the prover type-checks the polymorphic `mymap`
before admitting it:

```bash
printf '(tc +)\n(define mymap\n  {(A --> B) --> (list A) --> (list B)}\n  F [] -> []\n  F [X | Xs] -> [(F X) | (mymap F Xs)])\n(mymap (+ 1) [1 2 3])\n' | ./target/release/shen-rust 2>&1 | tail -4

```

```output
(0-) true
(0-) ...  ...  ...  (fn mymap)
(0-) (2 3 4)
(0-) 
```

## Execution tiers

The same semantics run on tiers chosen for the workload: a tree-walking
interpreter (default, best one-shot), the AOT-compiled kernel (always on), a
bytecode VM for load-once/serve-many processes (`--served` / `SHEN_RUST_VM=1`,
~2.3× warm), an opt-in per-file AOT overlay (~3× over the VM on served spec
code), and a gated experimental Cranelift JIT. Every tier is differentially
tested against the tree-walker. Same program, two engines, same answer:

```bash
RUN='(define sum-to 0 Acc -> Acc N Acc -> (sum-to (- N 1) (+ N Acc)))\n(sum-to 1000000 0)\n'
printf "$RUN" | ./target/release/shen-rust 2>&1 | tail -2 | head -1
printf "$RUN" | SHEN_RUST_VM=1 ./target/release/shen-rust 2>&1 | tail -2 | head -1

```

```output
(0-) 500000500000
(0-) 500000500000
```

## Memory: a real GC

`Value` is a word-sized `Copy` tagged `u64` over a non-moving mark-sweep heap.
Collection is opt-in (`SHEN_RUST_GC=1`) and runs at interpreter safepoints with
hybrid roots (precise interpreter tables + a conservative native-stack scan).
The machine-checked boundedness demo serves 20,000 requests twice — grow-only
vs GC — and asserts the GC arm's footprint is flat while the control's grows
without bound (it measures ~480 MB grow-only vs ~26 MB flat, wall-neutral; the
exact node counts vary slightly run to run, the PASS assertions do not):

```bash
cargo bench --bench gc_boundedness 2>&1 | tail -1
```

```output
BOUNDEDNESS: PASS (gc arm flat, control unbounded)
```

## Cedar as first-class Shen values

The engine embeds AWS's `cedar-policy` crate and exposes ~15 `cedar.*`
primitives, so Shen *programs* author and evaluate authorization policy
directly — here, parse a policy, build a request, and ask the live Cedar
authorizer for a decision, entirely from the Shen REPL:

```bash
printf '(cedar.is-authorized\n  (cedar.policy-set-add (cedar.empty-policy-set)\n    (cedar.parse-policy "permit(principal, action, resource);"))\n  (cedar.empty-entities)\n  (cedar.make-request (cedar.make-entity-uid "User" "alice")\n                      (cedar.make-entity-uid "Action" "read")\n                      (cedar.make-entity-uid "Doc" "d1") ()))\n' | ./target/release/shen-rust 2>&1 | tail -2 | head -1

```

```output
(0-) ...  ...  ...  ...  ...  ...  Allow
```

The flagship integration example inverts the relationship: Shen's logic engine
reasons *about* a Cedar policy set. It resolves Cedar's `in` hierarchy over the
role DAG and finds a dead (shadowed) permit and a forbid/permit overlap that
Cedar's per-request evaluator cannot surface — then cross-checks its verdict
against the live authorizer:

```bash
cargo run --quiet --release -p shen-cedar-authz --example verify 2>&1
```

```output
policies: strict-validated against schema ✓
booting served Shen VM… ready (AOT overlay).

policy0 forbid  principal=in Role::"Staff"           resource=any
policy1 permit  principal=in Role::"Analyst"         resource=== ShenCap::"pure"
policy2 forbid  principal=any                        resource=== ShenCap::"io"
policy3 permit  principal=in Role::"Admin"           resource=any
policy4 permit  principal=in Role::"Manager"         resource=== ShenCap::"pure"

Shen-computed interactions (hierarchy-aware: `in` resolved over the role DAG):
  ⚠ policy1 is DEAD — shadowed by forbid policy0  (via Analyst in Staff — string-equality would miss this)
  ⚠ policy3 OVERLAPS forbid policy2 — forbid wins on the intersection

Cross-check (live Cedar): alice(Analyst∈Staff) · pure => DENY ✓ confirms p1 is dead

Shen reasoned over 5 policies, flagged 2 interaction(s).
```

## Shen as a spec language (shengen)

`crates/shengen-rust` compiles sequent-calculus specs in `specs/` into Rust
guard types with private fields and fallible constructors, so a spec change
breaks `cargo build` instead of drifting silently. The same specs are also
re-type-checked by the engine itself — the prover runs over the spec on every
CI pass:

```bash
./scripts/shen-check.sh 2>&1 | grep -E 'typechecked|RESULT'
```

```output
typechecked in 168 inferences
RESULT: PASS
```

## Performance and verification

Measured against the reference `shen-cl` (SBCL) port, paired and interleaved
on the same machine: ~3× off bare on a one-shot kernel-tests run (down from
17× at first conformance), at parity with a warm typecheck cache, and ahead on
served workloads (VM ~2.3×, AOT overlay ~3× over that). Every number — and
every experiment that *failed* — is documented in `PERFORMANCE.md` and
`BENCHMARKS.md`, reproducible from `scripts/` and `benches/`.

The repo holds itself to ten CI gates (`scripts/gates.sh`): fmt+clippy,
build, full test suite, the engine re-type-checking its own specs, two TCB
audits, and the kernel suite three ways — release, debug (heap-reentrancy
sentinel live), and debug with the GC forced aggressive under poison-on-sweep.
The collector and value layers are additionally verified under miri.

*This document: `showboat verify DEMO.md` re-runs every block above and fails
on any drift.*

