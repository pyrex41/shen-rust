//! AOT overlay install: swap loaded defuns' function cells for
//! klcompile-emitted native fns, verified.
//!
//! The overlay split (proven in benches/normal_form_aot.rs and
//! benches/authz_served.rs): a normal `.shen` load runs first so every
//! side effect is live — datatypes, declares, macros, output — then the
//! overlay swaps the loaded defuns' cells for compiled code
//! (`register_native` + `register_aot_direct`, in that order). The
//! overlay is a pure speed swap: on any mismatch the loaded engine keeps
//! serving, never an error.
//!
//! ## Soundness constraints (what the hash check does and does not cover)
//!
//! - **Source identity**: `install_if_match` compares an FNV-1a hash of
//!   the live `.shen` source text (concatenated in load order) against
//!   the hash recorded at generation time, plus a digest of the kernel
//!   `.kl` sources (the kernel reader/translator shapes the generated
//!   code) and the `KLCOMPILE_FORMAT` string (bumped on codegen changes).
//! - **Macro environment**: generation reads the `.shen` file through a
//!   fresh kernel boot. An artifact is valid only if read-time
//!   macroexpansion at serve time matches generation time — load
//!   overlaid files BEFORE any user macro-definers. (tc-cache-style
//!   session-chain keying is the upgrade path if a real user breaks
//!   this.)
//! - **Two readers**: generation reads via the kernel reader
//!   (`bootstrap`); hosts may serve via the Rust kl::parser + kernel
//!   eval on the same bytes. The per-artifact shen_eq differential gate
//!   (benches) covers this axis.
//! - **Redefinition**: a later `(defun ...)` over an overlaid name wins
//!   on both dispatch paths (do_defun clears the direct slot; see
//!   tests/aot_redefine_coherence.rs).

use std::path::Path;

use crate::error::{ShenError, ShenResult};
use crate::interp::eval::Interp;

/// Artifact format tag. Bump when klcompile's emitted-code contract
/// changes (calling convention, runtime helper surface, manifest shape)
/// so stale committed artifacts silently fall back to the loaded engine
/// instead of installing.
pub const OVERLAY_FORMAT: &str = "shen-rust-aot-overlay-1";

/// A generated overlay module's self-description, emitted by klcompile
/// when `CompileOptions.emit_manifest` is set (external/overlay configs
/// only — kernel AOT modules stay manifest-free, byte-frozen).
pub struct OverlayModule {
    /// Human-readable provenance (the generation source label).
    pub label: &'static str,
    /// `KLCOMPILE_FORMAT` recorded at generation time.
    pub format: &'static str,
    /// FNV-1a over the `.shen` source bytes, concatenated in load order.
    pub source_fnv: u64,
    /// Digest of the kernel `.kl` sources the generating boot used.
    pub kernel_fnv: u64,
    /// (kl-name, arity) for every compiled defun, emission order.
    pub compiled: &'static [(&'static str, usize)],
    /// The generated `install` fn: registers every compiled defun in
    /// both dispatch tables (native first, then direct).
    pub install: fn(&mut Interp),
}

/// What an install attempt did.
pub struct OverlayReceipt {
    /// True if the overlay was installed over the loaded defuns.
    pub installed: bool,
    /// Number of names the module covers.
    pub names: usize,
    /// Per-name precheck failures (empty when installed).
    pub mismatches: Vec<String>,
}

impl Interp {
    /// Install an overlay after verifying every compiled name is
    /// actually loaded with the expected arity (i.e. the `.shen` load
    /// this overlay was generated from really happened). All-or-nothing:
    /// any mismatch means the loaded source differs from generation
    /// time, so nothing is installed. `strict` turns that into an error;
    /// otherwise the receipt reports the mismatches and the loaded
    /// engine keeps serving.
    pub fn install_overlay(
        &mut self,
        module: &OverlayModule,
        strict: bool,
    ) -> ShenResult<OverlayReceipt> {
        let mut mismatches = Vec::new();
        for (name, arity) in module.compiled {
            let sym = self.intern(name);
            match self.env.get_fn(sym).and_then(|v| v.as_closure()) {
                None => mismatches.push(format!("{name}: not loaded")),
                Some(c) if c.arity != *arity => {
                    mismatches.push(format!("{name}: loaded arity {} != {arity}", c.arity))
                }
                Some(_) => {}
            }
        }
        if !mismatches.is_empty() {
            if strict {
                return Err(ShenError::new(format!(
                    "install_overlay {}: {}",
                    module.label,
                    mismatches.join("; ")
                )));
            }
            return Ok(OverlayReceipt {
                installed: false,
                names: module.compiled.len(),
                mismatches,
            });
        }
        (module.install)(self);
        Ok(OverlayReceipt {
            installed: true,
            names: module.compiled.len(),
            mismatches,
        })
    }

    /// Install the overlay iff the artifact matches the live world:
    /// format supported, `live_src` (the `.shen` source text this
    /// session loaded, concatenated in load order) hashes to the
    /// generation-time value, and the kernel digest matches. On any
    /// mismatch: false, nothing installed, the loaded engine keeps
    /// serving — the overlay is a pure speed swap and must never turn a
    /// working load into an error.
    pub fn install_overlay_if_match(
        &mut self,
        module: &OverlayModule,
        live_src: &str,
        kernel_dir: &Path,
    ) -> bool {
        if module.format != OVERLAY_FORMAT {
            return false;
        }
        if fnv64(live_src.as_bytes()) != module.source_fnv {
            return false;
        }
        if kernel_digest(kernel_dir) != module.kernel_fnv {
            return false;
        }
        self.install_overlay(module, false)
            .map(|r| r.installed)
            .unwrap_or(false)
    }
}

/// FNV-1a 64-bit.
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Digest of a kernel directory: every `.kl` file, sorted by name,
/// name + content hashed. One-time ~MBs of IO per install attempt
/// (installs happen once per process). The kernel is disk-loaded
/// (`SHEN_KERNEL_DIR`-overridable), so this must be computed at runtime
/// against the directory the session actually booted from.
pub fn kernel_digest(kernel_dir: &Path) -> u64 {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(kernel_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "kl"))
        .collect();
    files.sort();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for f in &files {
        if let Some(name) = f.file_name().and_then(|n| n.to_str()) {
            mix(name.as_bytes());
        }
        mix(&fnv64(&std::fs::read(f).unwrap_or_default()).to_le_bytes());
    }
    h
}
