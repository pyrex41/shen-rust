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

# Do not leave core AOT uninstalled when codegen emitted an installer.
# 9f81b64 skipped core::install after empty S42 core.rs; 839cca1 regenerated
# a real module (~11k, pub fn install) and left the skip. backend is still
# excluded from install_all (not on the boot list).
core_rs="crates/shen-rust/src/aot/kernel/core.rs"
mod_rs="crates/shen-rust/src/aot/kernel/mod.rs"
core_kl="kernel/klambda/core.kl"
if grep -qE '^pub fn install\(' "$core_rs" && ! grep -qE '^[[:space:]]*core::install\(' "$mod_rs"; then
    echo "codegen-kernel-aot: FAIL — $core_rs has pub fn install but install_all omits core::install"
    exit 1
fi
if grep -qE '^\(defun[[:space:]]' "$core_kl" && ! grep -qE '^[[:space:]]*core::install\(' "$mod_rs"; then
    echo "codegen-kernel-aot: FAIL — $core_kl has defuns but install_all omits core::install"
    exit 1
fi

echo "codegen-kernel-aot: regenerated $(ls crates/shen-rust/src/aot/kernel/*.rs | wc -l | tr -d ' ') modules"
