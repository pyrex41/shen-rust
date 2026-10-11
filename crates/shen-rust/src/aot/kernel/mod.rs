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

// `extension-type-annotations` is vendored + generated so the Gate 6 audit
// stays exhaustive, but deliberately NOT called from `install_all` — it is
// not part of the boot list in `interp::boot::KERNEL_FILES` (shen-scheme
// 0.50 does not bundle it either).
pub mod core;
pub mod declarations;
pub mod dict;
pub mod extension_expand_dynamic;
pub mod extension_features;
pub mod extension_launcher;
pub mod extension_namespaces;
pub mod extension_programmable_pattern_matching;
pub mod extension_type_annotations;
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
///
/// Order mirrors `interp::boot::KERNEL_FILES` (shen-scheme 0.50's
/// ShenOSKernel 42.2 file order, then the bundled extensions).
/// `extension_type_annotations` is intentionally absent — see above.
/// `core` must be called whenever `core.rs` exposes `pub fn install`
/// (`core.kl` has ~60 defuns; Gate 6 rejects omitting it).
pub fn install_all(interp: &mut Interp) {
    toplevel::install(interp);
    core::install(interp);
    sys::install(interp);
    dict::install(interp);
    sequent::install(interp);
    yacc::install(interp);
    reader::install(interp);
    prolog::install(interp);
    track::install(interp);
    load::install(interp);
    writer::install(interp);
    macros::install(interp);
    declarations::install(interp);
    types::install(interp);
    t_star::install(interp);
    init::install(interp);
    stlib::install(interp);
    extension_features::install(interp);
    extension_expand_dynamic::install(interp);
    extension_launcher::install(interp);
    extension_programmable_pattern_matching::install(interp);
    extension_namespaces::install(interp);
}
