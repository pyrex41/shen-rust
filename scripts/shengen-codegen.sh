#!/bin/bash
# Regenerate Rust guard types from Shen specs.
set -euo pipefail

cd "$(dirname "$0")/.."

SPEC="${1:-specs/core.shen}"
OUTPUT="${2:-crates/shen-rust/src/generated/guard_types.rs}"

echo "shengen-codegen: $SPEC -> $OUTPUT"
cargo run --quiet --bin shengen-rust -- "$SPEC" "$OUTPUT"
rustfmt --quiet "$OUTPUT" || true
