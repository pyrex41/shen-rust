//! Pin generated guard types onto the kernel boot path.
//!
//! Calling each constructor here means that if `shengen-rust` ever emits
//! a different signature (param order, types, accessor names), this
//! module fails to compile — and since `interp::boot` imports it,
//! **Gate 2 (`cargo build`) catches the drift automatically**.
//!
//! This is the same pattern `shen-ocaml` uses with
//! `Runtime.Guard_types_link`.

use crate::generated::guard_types as gt;

/// Touch every generated constructor + a representative accessor. Called
/// once at boot to keep the symbols linked into the binary.
pub fn witness() {
    let _kl = gt::KlValue::new("dummy".to_string()).unwrap();
    let sym = gt::InternedSymbol::new("foo".to_string(), 0.0).unwrap();
    let fb = gt::FnBinding::new(sym.clone(), 1.0).unwrap();
    let _ = gt::ValBinding::new(sym.clone()).unwrap();
    let _ = gt::ResolvedArity::new(fb.clone(), 1.0).unwrap();
    let _ =
        gt::CheckedApplication::new(gt::ResolvedArity::new(fb.clone(), 1.0).unwrap(), 0.0).unwrap();
    let _ = gt::ValidKlAst::new("ast".to_string()).unwrap();
    let _ = gt::KernelLoaded::new(21.0).unwrap();
    // Accessor witness: if the generated accessor name changes, this fails.
    let _ = sym.name();
    let _ = sym.id();
    let _ = fb.arity();
}
