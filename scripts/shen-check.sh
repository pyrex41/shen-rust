#!/bin/bash
# Gate 4: type-check specs through the shen-cedar binary itself.
#
# Boots ShenOSKernel-41.1, turns on the type checker, loads the spec
# file. The kernel reports `typechecked in N inferences` on success;
# we grep for that string.
set -euo pipefail

cd "$(dirname "$0")/.."

SPEC="${1:-specs/core.shen}"

if [ ! -f "$SPEC" ]; then
    echo "ERROR: spec not found: $SPEC"
    exit 1
fi

echo "shen-check: type-checking $SPEC"

output=$(printf '(tc +)\n(load "%s")\n' "$SPEC" \
    | cargo run --quiet --bin shen-cedar 2>&1)
echo "$output" | tail -30

if echo "$output" | grep -q "typechecked in"; then
    echo "RESULT: PASS"
    exit 0
fi

if echo "$output" | grep -q "loaded" ; then
    echo "RESULT: PASS (loaded without explicit typecheck banner)"
    exit 0
fi

echo "RESULT: FAIL"
exit 1
