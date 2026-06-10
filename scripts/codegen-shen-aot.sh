#!/bin/bash
# Generate an AOT overlay module from .shen source files via the
# canonical recipe: fresh kernel boot → (bootstrap F) per input →
# concatenate the .kl in load order → klcompile external config with an
# overlay manifest → rustfmt. The fresh boot is load-bearing: gensym'd
# locals in the bootstrap output depend on session state, so generating
# from a dirty session produces byte-different (semantically identical)
# output. Commit the module; the manifest's SOURCE_FNV/KERNEL_FNV let
# Interp::install_overlay_if_match silently fall back to the loaded
# engine when the artifact no longer matches the live sources.
#
# Usage: scripts/codegen-shen-aot.sh <out.rs> <in1.shen> [in2.shen ...]
set -euo pipefail

cd "$(dirname "$0")/.."

if [ $# -lt 2 ]; then
    echo "Usage: scripts/codegen-shen-aot.sh <out.rs> <in1.shen> [in2.shen ...]" >&2
    exit 2
fi

cargo build --quiet --release --bin shen-rust
BIN="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')/release/shen-rust"

"$BIN" --aot-gen "$@"
rustfmt --quiet "$1"
echo "codegen-shen-aot: $1"
