//! Redefinition coherence for the two-table dispatch (env.functions +
//! aot_direct).
//!
//! THE invariant: `aot_direct[s]` is a pure fast-path mirror — it may hold
//! a fn pointer only while `env.functions[s]` holds the closure registered
//! together with it. A `(defun ...)` that rebinds a name must clear the
//! direct slot, or every AOT/kernel caller dispatching through
//! `rt::apply_direct` keeps the stale native forever while interpreted
//! callers see the new definition (a split-brain that exists for every
//! kernel AOT name, not just overlay installs).
//!
//! Tests A–D cover the `do_defun` seam (Stage 0A); Test E covers the
//! host-API `register_native` seam (Stage 0B).

use shen_rust::aot::generated;
use shen_rust::aot::runtime as rt;
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

fn fresh_with_defuns() -> Interp {
    let mut interp = Interp::new();
    run(
        &mut interp,
        "(defun fact (N) (if (= N 0) 1 (* N (fact (- N 1)))))",
    );
    run(&mut interp, "(defun double (X) (* X 2))");
    interp
}

/// Test A: after an AOT install, a `(defun ...)` redefinition must win on
/// BOTH dispatch paths — the interpreted App path (env.functions) and the
/// AOT fast path (`rt::apply_direct`) — and the direct slot must be empty.
#[test]
fn redefinition_wins_on_both_dispatch_paths() {
    let mut interp = fresh_with_defuns();
    generated::install(&mut interp);

    // Sanity: the AOT overlay serves both paths pre-redefinition.
    assert_eq!(run(&mut interp, "(double 21)").as_int(), Some(42));
    let direct = rt::apply_direct(&mut interp, "double", &[Value::int(21)]).unwrap();
    assert_eq!(direct.as_int(), Some(42));

    // Redefine over the AOT-installed name.
    run(&mut interp, "(defun double (X) (+ X 100))");

    let via_eval = run(&mut interp, "(double 5)");
    assert_eq!(
        via_eval.as_int(),
        Some(105),
        "interpreted path must see the redefinition"
    );

    let via_direct = rt::apply_direct(&mut interp, "double", &[Value::int(5)]).unwrap();
    assert_eq!(
        via_direct.as_int(),
        Some(105),
        "apply_direct path must see the redefinition (stale aot_direct slot)"
    );

    let sym = interp.intern("double");
    assert!(
        interp.get_aot_direct(sym).is_none(),
        "defun must clear the aot_direct slot"
    );
}

/// Test B: late-binding parity. Old AOT-compiled code still in flight
/// resolves its internal `apply_direct` call edges against the CURRENT
/// definition, exactly as tree-walked code resolves names per call.
/// `generated::aot_fact` recurses via `rt::apply_direct(interp, "fact", ..)`
/// (generated.rs:58), so calling the old compiled body after a redefinition
/// must route the recursive edge to the new definition.
#[test]
fn stale_aot_edge_sees_new_definition() {
    let mut interp = fresh_with_defuns();
    generated::install(&mut interp);
    assert_eq!(run(&mut interp, "(fact 5)").as_int(), Some(120));

    // Redefine fact to a constant function.
    run(&mut interp, "(defun fact (N) 99)");

    // Invoke the OLD compiled body directly: 5 * fact(4) where the inner
    // edge must late-bind to the new definition → 5 * 99 = 495.
    // (Pre-fix the direct slot still points at aot_fact, so the whole old
    // recursion runs: 120.)
    let held = generated::aot_fact(&mut interp, &[Value::int(5)]).unwrap();
    assert_eq!(
        held.as_int(),
        Some(495),
        "internal apply_direct edge of in-flight AOT code must late-bind"
    );
}

/// Test C: clear → reinstall round-trips. Clear-only invalidation is
/// sufficient (no versioning): re-running the overlay installer repopulates
/// both tables and the fast path serves again.
#[test]
fn clear_then_reinstall_roundtrips() {
    let mut interp = fresh_with_defuns();
    generated::install(&mut interp);
    run(&mut interp, "(defun double (X) (+ X 100))");
    assert_eq!(run(&mut interp, "(double 5)").as_int(), Some(105));

    // Re-overlay: AOT definition wins again on both paths.
    generated::install(&mut interp);
    assert_eq!(run(&mut interp, "(double 21)").as_int(), Some(42));
    let via_direct = rt::apply_direct(&mut interp, "double", &[Value::int(21)]).unwrap();
    assert_eq!(via_direct.as_int(), Some(42));

    let sym = interp.intern("double");
    assert!(
        interp.get_aot_direct(sym).is_some(),
        "reinstall must repopulate the direct slot"
    );
}

/// Test E: the host-API seam. A bare `register_native` (no paired
/// `register_aot_direct`) over an AOT-installed name must also win through
/// apply_direct — the rebind invalidates the direct slot, and the fast path
/// falls back to the env closure.
#[test]
fn register_native_override_wins_through_apply_direct() {
    let mut interp = fresh_with_defuns();
    generated::install(&mut interp);
    assert_eq!(run(&mut interp, "(double 21)").as_int(), Some(42));

    interp.register_native("double", 1, |_, args| {
        Ok(Value::int(args[0].as_int().unwrap() + 7))
    });

    let via_direct = rt::apply_direct(&mut interp, "double", &[Value::int(10)]).unwrap();
    assert_eq!(
        via_direct.as_int(),
        Some(17),
        "register_native must clear the stale direct slot"
    );
}

/// Test D: the split-brain is live TODAY for kernel AOT names. After a full
/// kernel boot (install_all populates ~1123 direct slots), a user
/// redefinition of a kernel fn must win through apply_direct — the path the
/// kernel's own AOT callers (prolog, typechecker) use.
#[test]
fn kernel_name_redefinition_wins_through_apply_direct() {
    let mut interp = Interp::new();
    let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.pop();
    dir.pop(); // workspace root
    let dir = dir.join("kernel").join("klambda");
    shen_rust::interp::boot::boot_with_kernel(&mut interp, &dir)
        .unwrap_or_else(|e| panic!("kernel boot failed: {e}"));

    // Kernel element? works via the direct path.
    let pre = rt::apply_direct(
        &mut interp,
        "element?",
        &[Value::int(1), Value::list([Value::int(1)])],
    )
    .unwrap();
    assert_eq!(pre.as_bool(), Some(true));

    // User redefines a kernel fn.
    run(&mut interp, "(defun element? (X L) reborn)");

    let via_eval = run(&mut interp, "(element? 1 ())");
    assert_eq!(
        via_eval.as_sym().map(|s| interp.resolve(s).to_string()),
        Some("reborn".to_string()),
        "interpreted path must see the kernel-name redefinition"
    );

    let via_direct = rt::apply_direct(
        &mut interp,
        "element?",
        &[Value::int(1), Value::list([Value::int(1)])],
    )
    .unwrap();
    assert_eq!(
        via_direct.as_sym().map(|s| interp.resolve(s).to_string()),
        Some("reborn".to_string()),
        "apply_direct must see the kernel-name redefinition (live split-brain)"
    );
}
