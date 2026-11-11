//! Regression: a lean boot (`boot_aot_slice`, no default features) must
//! register `value/or` before `shen.initialise`. Kernel 43.4's initialiser
//! calls `bound?`, and klcompile lowers that to `value/or`.

use shen_rust::interp::lean_fib;

#[cfg(feature = "interpreter")]
use shen_rust::interp::boot::boot_from_kl_source;
#[cfg(feature = "interpreter")]
use shen_rust::interp::eval::Interp;

#[test]
fn lean_boot_runs_fib() {
    assert_eq!(lean_fib::fib10().expect("lean boot"), 55);
}

/// Tree-walk boot (`interpreter` only, the needs-eval scaffold). The KL
/// initialiser calls `value/or` by name. That primitive has to exist
/// before `shen.initialise`, not only after the later hot-override pass.
#[cfg(feature = "interpreter")]
#[test]
fn kl_initialise_reaches_value_or() {
    let mut interp = Interp::new();
    let src = "(defun shen.initialise () (value/or not-bound-symbol (freeze 7)))";
    boot_from_kl_source(&mut interp, src, None).expect("boot_from_kl_source");
    let sym = interp.intern("shen.initialise");
    let f = interp.env.get_fn(sym).cloned().expect("shen.initialise");
    let v = interp.apply(f, vec![]).expect("re-run initialise");
    assert_eq!(v.as_int(), Some(7), "{v:?}");
}
