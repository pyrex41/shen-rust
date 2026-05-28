#!/bin/bash
# Gate 5: TCB audit.
# Regenerate guard types into a scratch file and diff against the
# committed copy. Fails if shengen output drifts from what's checked in
# (someone hand-edited the generated file).
#
# Also rejects any file in src/generated/ other than the known set —
# the generated module is the forgery boundary, nothing else may live
# there.
set -euo pipefail

cd "$(dirname "$0")/.."

SPEC="specs/core.shen"
COMMITTED="crates/shen-cedar/src/generated/guard_types.rs"
SCRATCH="$(mktemp -t shengen-XXXXXX).rs"
trap "rm -f $SCRATCH" EXIT

echo "tcb-audit: regenerating from $SPEC"
cargo run --quiet --bin shengen-rust -- "$SPEC" "$SCRATCH"
rustfmt --quiet "$SCRATCH" || true

if ! diff -u "$COMMITTED" "$SCRATCH"; then
    echo "FAIL: $COMMITTED has drifted from spec regeneration."
    echo "      Run scripts/shengen-codegen.sh and re-commit."
    exit 1
fi

allowed=("guard_types.rs" "mod.rs")
for f in crates/shen-cedar/src/generated/*; do
    base=$(basename "$f")
    ok=false
    for a in "${allowed[@]}"; do
        if [ "$base" = "$a" ]; then ok=true; break; fi
    done
    if ! $ok; then
        echo "FAIL: unexpected file in generated/: $base"
        exit 1
    fi
done

echo "tcb-audit: OK"
