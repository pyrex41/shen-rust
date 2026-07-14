//! AOT-compiled kernel.
//!
//! `klcompile` emits one module here per `.kl` file in `kernel/klambda/`.
//! Each module exposes `pub fn install(interp: &mut Interp)` which
//! registers every defun in that file as a native function on `interp`,
//! overriding the tree-walked version installed during kernel boot.
//!
//! `install_all` calls every per-file installer in the same fixed order
//! `interp::boot::KERNEL_FILES` loads the source files. Order matters
//! only for diagnostic purposes (a panic in `install_macros` is easier
//! to trace than one across many simultaneous modules).
//!
//! To regenerate: `scripts/codegen-kernel-aot.sh`.

use crate::interp::eval::Interp;

// `backend` (the `cl.*` Common-Lisp backend) and the programmable
// pattern-matching extension are vendored + generated so the Gate 6
// audit stays exhaustive, but deliberately NOT called from `install_all`
// — neither is part of the boot list in `interp::boot::KERNEL_FILES`.
pub mod backend;
pub mod core;
pub mod declarations;
pub mod extension_expand_dynamic;
pub mod extension_features;
pub mod extension_launcher;
pub mod extension_programmable_pattern_matching;
pub mod load;
pub mod macros;
pub mod prolog;
pub mod reader;
pub mod sequent;
pub mod sys;
pub mod t_star;
pub mod toplevel;
pub mod track;
pub mod types;
pub mod writer;
pub mod yacc;

/// Register every AOT-compiled kernel function on `interp`. Call after
/// `shen.initialise` and `register_all_metadata`, so these overwrite the
/// tree-walked versions while leaving property-vector setup intact.
///
/// Order mirrors `interp::boot::KERNEL_FILES` (upstream `install.lsp`
/// runtime order for the Tarver S41.2 refresh, then the stdlib overlay
/// and extensions). `backend` is intentionally absent — see above.
pub fn install_all(interp: &mut Interp) {
    sys::install(interp);
    writer::install(interp);
    core::install(interp);
    reader::install(interp);
    declarations::install(interp);
    toplevel::install(interp);
    macros::install(interp);
    load::install(interp);
    prolog::install(interp);
    sequent::install(interp);
    track::install(interp);
    t_star::install(interp);
    yacc::install(interp);
    types::install(interp);
    extension_features::install(interp);
    extension_expand_dynamic::install(interp);
    extension_launcher::install(interp);
}
