#!/bin/bash
# Regenerate per-kernel-file AOT modules under
# crates/shen-rust/src/aot/kernel/. One Rust module per `.kl` file in
# kernel/klambda/; each module exposes `pub fn install(interp)` which
# registers every defun in the file as a native function.
#
# Module names: replace `-` with `_` in the basename (so
# `extension-launcher.kl` → `extension_launcher.rs`).
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build --quiet -p klcompile
KLCOMPILE="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')/debug/klcompile"

for f in kernel/klambda/*.kl; do
    base=$(basename "$f" .kl)
    mod=$(echo "$base" | tr '-' '_')
    out="crates/shen-rust/src/aot/kernel/${mod}.rs"
    "$KLCOMPILE" "$f" "$out"
done

rustfmt --quiet crates/shen-rust/src/aot/kernel/*.rs || true

echo "codegen-kernel-aot: regenerated $(ls crates/shen-rust/src/aot/kernel/*.rs | wc -l | tr -d ' ') modules"
