//! Integration test: parse every `.kl` file in the vendored kernel.
//! This is the meaningful proof that the parser is good enough for Phase 2.

use std::fs;
use std::path::PathBuf;

use shen_rust::kl::parser::parse_all;
use shen_rust::symbol::Interner;

fn kernel_klambda_dir() -> PathBuf {
    // Workspace root from the crate-local CARGO_MANIFEST_DIR:
    //   crates/shen-rust/Cargo.toml -> ../../kernel/klambda
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // workspace root
    p.push("kernel");
    p.push("klambda");
    p
}

#[test]
fn parses_every_kernel_file() {
    let dir = kernel_klambda_dir();
    let entries: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"))
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("kl"))
        .collect();

    assert!(
        !entries.is_empty(),
        "no .kl files found in {dir:?}; kernel not vendored?"
    );

    let mut total_forms = 0usize;
    let mut interner = Interner::new();
    for entry in &entries {
        let path = entry.path();
        let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let forms =
            parse_all(&src, &mut interner).unwrap_or_else(|e| panic!("parse {path:?}: {e}"));
        assert!(!forms.is_empty(), "no forms parsed from {path:?}");
        total_forms += forms.len();
    }

    // The ShenOSKernel-41.2 klambda set is around 3000+ top-level forms;
    // exact number changes between releases. We just want a sanity floor.
    assert!(
        total_forms > 1000,
        "only {total_forms} forms parsed across {} files — parser likely losing content",
        entries.len()
    );
}
