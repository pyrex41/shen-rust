#!/bin/bash
# Gate 6: kernel-AOT audit.
# Regenerate the per-kernel-file AOT modules into a scratch directory
# and diff each against its committed copy. Fails if any module has
# drifted from what klcompile would produce now (someone hand-edited a
# generated file, or klcompile changed without regenerating).
#
# Also rejects any file in crates/shen-rust/src/aot/kernel/ that isn't
# either mod.rs or a regenerable `<basename>.rs`.
set -euo pipefail

cd "$(dirname "$0")/.."

KERNEL_AOT_DIR="crates/shen-rust/src/aot/kernel"
SCRATCH=$(mktemp -d -t kernel-aot-XXXXXX)
trap "rm -rf $SCRATCH" EXIT

cargo build --quiet -p klcompile
KLCOMPILE="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import sys,json; print(json.load(sys.stdin)["target_directory"])')/debug/klcompile"

echo "kernel-aot-audit: regenerating to $SCRATCH"
for f in kernel/klambda/*.kl; do
    base=$(basename "$f" .kl)
    mod=$(echo "$base" | tr '-' '_')
    "$KLCOMPILE" "$f" "$SCRATCH/${mod}.rs"
done
rustfmt --quiet "$SCRATCH"/*.rs || true

drift=0
for fresh in "$SCRATCH"/*.rs; do
    base=$(basename "$fresh")
    committed="$KERNEL_AOT_DIR/$base"
    if [ ! -f "$committed" ]; then
        echo "FAIL: missing committed module $committed"
        drift=1
        continue
    fi
    if ! diff -u "$committed" "$fresh" >/dev/null; then
        echo "FAIL: $committed has drifted from klcompile output."
        diff -u "$committed" "$fresh" | head -40
        drift=1
    fi
done

# Reject any extra files. The only allowed non-regenerated file is mod.rs.
for f in "$KERNEL_AOT_DIR"/*.rs; do
    base=$(basename "$f")
    if [ "$base" = "mod.rs" ]; then continue; fi
    if [ ! -f "$SCRATCH/$base" ]; then
        echo "FAIL: unexpected file in $KERNEL_AOT_DIR: $base"
        drift=1
    fi
done

if [ $drift -ne 0 ]; then
    echo "kernel-aot-audit: FAIL — run scripts/codegen-kernel-aot.sh and re-commit."
    exit 1
fi

echo "kernel-aot-audit: OK"
