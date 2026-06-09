//! klcompile CLI: compile a `.kl` file to a Rust module.
//!
//! Thin wrapper over the `klcompile` library (see src/lib.rs). This is
//! the kernel-AOT generation tool: it runs with the kernel options
//! (`crate::` imports, the `//!`+`#![allow]` header, the legacy
//! known-slow skip list) so `scripts/codegen-kernel-aot.sh` and the
//! Gate-6 audit produce byte-identical output to the historical
//! single-file binary. Overlay/bench generation paths call
//! `klcompile::compile_kl` with `CompileOptions::external` instead.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use klcompile::{compile_kl, CompileOptions};

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().collect();
    // `--external [label]` switches to the external-crate configuration
    // (shen_rust:: imports, plain // header, body-size budget) used for
    // overlay/bench modules generated outside shen-rust. The kernel path
    // (no flag, exactly two args) is byte-frozen behind Gate 6.
    let external = args.iter().position(|a| a == "--external");
    if let Some(i) = external {
        args.remove(i);
    }
    if args.len() != 3 && !(external.is_some() && args.len() == 4) {
        eprintln!("Usage: klcompile [--external] <input.kl> <output.rs> [source-label]");
        return ExitCode::from(2);
    }
    let input = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);

    let src = match fs::read_to_string(&input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("klcompile: read {input:?}: {e}");
            return ExitCode::from(1);
        }
    };

    // Kernel options by default; the source label embeds the input path
    // verbatim, exactly as the historical `emit_header(&input)` did.
    let label = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| input.display().to_string());
    let opts = if external.is_some() {
        CompileOptions::external(label)
    } else {
        CompileOptions::kernel(label)
    };
    let (module, report) = match compile_kl(&src, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("klcompile: {} {input:?}", e);
            return ExitCode::from(1);
        }
    };

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ok();
        }
    }
    if let Err(e) = fs::write(&output, module) {
        eprintln!("klcompile: write {output:?}: {e}");
        return ExitCode::from(1);
    }
    eprintln!(
        "klcompile: {} compiled, {} skipped → {}",
        report.compiled.len(),
        report.skipped.len(),
        output.display()
    );
    for s in &report.skipped {
        eprintln!("  skipped: {s}");
    }
    ExitCode::SUCCESS
}
