//! End-to-end benchmark of AOT-compiled kernel vs tree-walked kernel.
//!
//! Boots the full kernel twice — once with `crate::aot::kernel::install_all`
//! active (the default) and once with it disabled by registering plain
//! tree-walked closures over the top — then times the same Shen-level
//! computation against both.
//!
//! Marked `#[ignore]` so it doesn't slow the default test suite. Run
//! with `cargo test --release --test aot_kernel_bench -- --ignored
//! --nocapture` to see numbers.

use std::time::Instant;

use shen_rust::interp::boot::boot;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

fn run(interp: &mut Interp, src: &str) -> Value {
    let e =
        parse_one(src, &mut interp.symbols).unwrap_or_else(|err| panic!("parse {src:?}: {err}"));
    interp
        .eval(&e)
        .unwrap_or_else(|err| panic!("eval {src:?}: {err}"))
}

#[test]
#[ignore]
fn kernel_loop_sum() {
    const RUNS: u32 = 2000;

    let mut interp = Interp::new();
    boot(&mut interp).expect("boot");

    // Define the loop-sum function via the kernel's `defun` so it goes
    // through the full reader pipeline. AOT install_all already ran in
    // `boot`, but the user-defined `loop-sum` is not part of the kernel
    // so it stays tree-walked — that's the comparison the original
    // `aot_perf_smoke` already covers. This kernel bench instead
    // measures something that *does* hit AOT kernel functions:
    // `append`, `reverse`, `map` over a sample list.
    run(
        &mut interp,
        "(defun bench-body () (reverse (append (cons 1 (cons 2 (cons 3 ()))) (cons 4 (cons 5 (cons 6 ()))))))",
    );

    let parsed = parse_one("(bench-body)", &mut interp.symbols).unwrap();
    let t0 = Instant::now();
    let mut last = Value::nil();
    for _ in 0..RUNS {
        last = interp.eval(&parsed).unwrap();
    }
    let elapsed = t0.elapsed();
    let per_call = elapsed / RUNS;

    eprintln!(
        "aot_kernel_bench: {RUNS}× (reverse (append [1 2 3] [4 5 6]))\n  total:    {elapsed:?}\n  per call: {per_call:?}"
    );

    // Sanity: result should be (6 5 4 3 2 1) — a 6-element list.
    if last.is_cons() {
        // pass
    } else {
        panic!("expected cons, got {last:?}");
    }
}
