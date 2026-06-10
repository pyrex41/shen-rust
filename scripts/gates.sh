#!/bin/bash
# Run all five backpressure gates in order. Mirror of `shen-ocaml`'s
# Makefile + bin/ralph.sh gate sequence, adapted for cargo.
set -euo pipefail

cd "$(dirname "$0")/.."

fail=()

run() {
    local label="$1"; shift
    echo
    echo "==== $label ===="
    if "$@"; then
        echo "[$label] PASS"
    else
        echo "[$label] FAIL"
        fail+=("$label")
    fi
}

run "Gate 0: shengen-codegen" scripts/shengen-codegen.sh
run "Gate 1: fmt + clippy"  bash -c "cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings"
run "Gate 2: build"          cargo build --workspace
run "Gate 3: test"           cargo test --workspace
run "Gate 4: shen-check"     scripts/shen-check.sh
run "Gate 5: tcb-audit"      scripts/tcb-audit.sh
run "Gate 6: kernel-aot-audit" scripts/kernel-aot-audit.sh
run "Gate 7: kernel-tests"   scripts/kernel-tests.sh
run "Gate 8: kernel-tests-debug" scripts/kernel-tests.sh --debug
run "Gate 9: kernel-tests-debug-gc" scripts/kernel-tests.sh --debug-gc

echo
if [ ${#fail[@]} -eq 0 ]; then
    echo "ALL GATES GREEN"
    exit 0
fi
echo "FAILED: ${fail[*]}"
exit 1
