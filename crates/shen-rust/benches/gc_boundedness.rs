//! GC Step 4 boundedness demo — the "really useful" deliverable.
//!
//! A load-once / serve-many embedding runs the same request loop on two
//! interpreters (each on its own thread, so each gets its own TLS heap):
//!
//! * **control**: grow-only (the pre-Step-4 default) — heap footprint grows
//!   monotonically with request count;
//! * **gc**: request-mode collection (`SHEN_RUST_GC`) — footprint plateaus
//!   at a small multiple of the live set, no matter how many requests run.
//!
//! Both arms assert every request's result (a corrupt heap is a void
//! measurement, not a fast one — repo measurement discipline), and the
//! harness asserts the headline properties machine-checkably:
//! the GC arm's node count is FLAT across the back half of the run, and the
//! control arm's keeps growing past it.
//!
//! Run: `cargo run --release --bench gc_boundedness`

use std::time::Instant;

use shen_rust::interp::boot::boot;
use shen_rust::interp::eval::Interp;
use shen_rust::kl::parser::parse_one;
use shen_rust::value::Value;

const REQUESTS: usize = 20_000;
const SAMPLE_EVERY: usize = 1_000;

fn eval(interp: &mut Interp, src: &str) -> Value {
    let form = parse_one(src, &mut interp.symbols).unwrap_or_else(|e| panic!("parse {src:?}: {e}"));
    interp
        .eval(&form)
        .unwrap_or_else(|e| panic!("eval {src:?}: {e}"))
}

struct ArmResult {
    label: &'static str,
    samples: Vec<(usize, usize)>, // (request index, heap node count)
    final_nodes: usize,
    last_live: usize,
    collections: u64,
    wall: std::time::Duration,
}

/// Boot a kernel, then serve `REQUESTS` list-building/walking requests,
/// sampling heap footprint. Runs on a fresh thread = fresh TLS heap.
fn run_arm(label: &'static str) -> ArmResult {
    std::thread::Builder::new()
        .name(label.to_string())
        .stack_size(16 << 20)
        .spawn(move || {
            let mut interp = Interp::new();
            boot(&mut interp).expect("kernel boot");
            eval(
                &mut interp,
                "(defun range (N ACC) (if (= N 0) ACC (range (- N 1) (cons N ACC))))",
            );
            eval(
                &mut interp,
                "(defun rev (XS ACC) (if (cons? XS) (rev (tl XS) (cons (hd XS) ACC)) ACC))",
            );
            eval(
                &mut interp,
                "(defun sum (XS ACC) (if (cons? XS) (sum (tl XS) (+ (hd XS) ACC)) ACC))",
            );

            let t0 = Instant::now();
            let mut samples = Vec::new();
            for i in 0..REQUESTS {
                let v = eval(&mut interp, "(sum (rev (range 500 ()) ()) 0)");
                assert_eq!(v.as_int(), Some(125_250), "request {i}: corrupted result");
                if (i + 1) % SAMPLE_EVERY == 0 {
                    let (_, _, nodes) = interp.gc_stats();
                    samples.push((i + 1, nodes));
                }
            }
            let wall = t0.elapsed();
            let (collections, last_live, final_nodes) = interp.gc_stats();
            ArmResult {
                label,
                samples,
                final_nodes,
                last_live,
                collections,
                wall,
            }
        })
        .expect("spawn arm")
        .join()
        .expect("arm thread")
}

fn main() {
    let scan_supported = cfg!(all(
        target_arch = "aarch64",
        any(target_os = "macos", target_os = "linux")
    ));
    if !scan_supported {
        eprintln!("gc_boundedness: conservative scan unsupported here; nothing to demo");
        return;
    }

    // Control first (env unset), then the GC arm (env set). The variable is
    // read once per Interp::new on the arm's own thread.
    std::env::remove_var("SHEN_RUST_GC");
    let control = run_arm("control(grow-only)");
    std::env::set_var("SHEN_RUST_GC", "1");
    let gc = run_arm("gc(SHEN_RUST_GC=1)");

    println!("\n== gc_boundedness: {REQUESTS} served requests, sampled every {SAMPLE_EVERY} ==");
    for arm in [&control, &gc] {
        println!(
            "{:>20}: final {:>11} nodes ({:>6.1} MB), live {:>8}, {} collections, wall {:?}",
            arm.label,
            arm.final_nodes,
            (arm.final_nodes * 24) as f64 / 1e6,
            arm.last_live,
            arm.collections,
            arm.wall
        );
    }
    let halfway_gc = gc.samples[gc.samples.len() / 2].1;
    println!(
        "gc arm: nodes at request {}: {} | at request {}: {} (flat back half = bounded)",
        gc.samples[gc.samples.len() / 2].0,
        halfway_gc,
        gc.samples.last().unwrap().0,
        gc.final_nodes
    );

    // Machine-checked headline properties.
    assert!(gc.collections > 0, "gc arm never collected");
    assert!(
        gc.final_nodes <= halfway_gc + halfway_gc / 20,
        "gc arm footprint grew >5% across the back half: {} -> {}",
        halfway_gc,
        gc.final_nodes
    );
    assert!(
        control.final_nodes > 2 * gc.final_nodes,
        "control should grow well past the gc arm ({} vs {})",
        control.final_nodes,
        gc.final_nodes
    );
    println!("BOUNDEDNESS: PASS (gc arm flat, control unbounded)");
}
