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
//! to trace than one across 21 simultaneous modules).
//!
//! To regenerate: `scripts/codegen-kernel-aot.sh`.

use crate::interp::eval::Interp;

pub mod compiler;
pub mod core;
pub mod declarations;
pub mod dict;
pub mod extension_expand_dynamic;
pub mod extension_features;
pub mod extension_launcher;
// Opt-in extension (new in 41.2): vendored + generated so the Gate 6
// audit stays exhaustive, but deliberately NOT called from `install_all`
// — it is not part of the canonical 21-module boot list in
// `interp::boot::KERNEL_FILES`.
pub mod extension_programmable_pattern_matching;
pub mod init;
pub mod load;
pub mod macros;
pub mod prolog;
pub mod reader;
pub mod sequent;
pub mod stlib;
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
pub fn install_all(interp: &mut Interp) {
    core::install(interp);
    toplevel::install(interp);
    sys::install(interp);
    reader::install(interp);
    prolog::install(interp);
    load::install(interp);
    writer::install(interp);
    macros::install(interp);
    declarations::install(interp);
    types::install(interp);
    t_star::install(interp);
    sequent::install(interp);
    track::install(interp);
    dict::install(interp);
    compiler::install(interp);
    stlib::install(interp);
    init::install(interp);
    extension_features::install(interp);
    extension_expand_dynamic::install(interp);
    extension_launcher::install(interp);
    yacc::install(interp);
}
