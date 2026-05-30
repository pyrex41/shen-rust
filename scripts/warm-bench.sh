#!/bin/bash
# Warm/served benchmark: tree-walker vs bytecode VM on a *repeated* execution
# workload (the lambda-calculus interpreter from interpreter.shen, served many
# times against a once-loaded, once-type-checked theory).
#
# This is the warm counterpart to scripts/cross-port-bench.sh. The one-shot
# kernel-tests metric never amortizes the VM's runtime compile cost, so the VM
# measures neutral there; this harness amortizes it (load once, serve N×) to
# test whether the VM's per-body execution win shows through at the suite level.
#
# The engine is process-global (`SHEN_CEDAR_VM` read once into a OnceCell), so
# we run two processes and INTERLEAVE them (tree, vm, tree, vm, …) across PAIRS
# rounds so both share thermal state. The bench itself reports a per-process
# min-of-8 internally; we take the min across rounds on top of that.
set -euo pipefail

cd "$(dirname "$0")/.."

PAIRS="${PAIRS:-5}"
BIN=$(ls target/release/deps/warm_typecheck-* 2>/dev/null | grep -v '\.d$' | head -1 || true)
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
    echo "Building warm_typecheck bench..."
    cargo build --release --bench warm_typecheck >/dev/null 2>&1
    BIN=$(ls target/release/deps/warm_typecheck-* | grep -v '\.d$' | head -1)
fi

# Extract the normalized "per-batch min X ms" number from a run's stdout log.
warm_min() { grep -E "warm exec" "$1" | sed -E 's/.*per-batch min ([0-9.]+) ms.*/\1/'; }

best_tree=""; best_vm=""
for r in $(seq 1 "$PAIRS"); do
    "$BIN"               >/tmp/warm-tree.log 2>/dev/null
    SHEN_CEDAR_VM=1 "$BIN" >/tmp/warm-vm.log   2>/dev/null
    t=$(warm_min /tmp/warm-tree.log); v=$(warm_min /tmp/warm-vm.log)
    printf "round %d:  tree %7.4f ms/batch   vm %7.4f ms/batch   (%.2fx)\n" "$r" "$t" "$v" \
        "$(echo "$t / $v" | bc -l)"
    if [ -z "$best_tree" ] || (( $(echo "$t < $best_tree" | bc -l) )); then best_tree=$t; fi
    if [ -z "$best_vm" ]   || (( $(echo "$v < $best_vm"   | bc -l) )); then best_vm=$v;   fi
done

echo
printf "BEST-OF-%d:  tree %7.4f ms/batch   vm %7.4f ms/batch   VM speedup %.2fx\n" \
    "$PAIRS" "$best_tree" "$best_vm" "$(echo "$best_tree / $best_vm" | bc -l)"
echo
echo "VM coverage (from last VM run):"
grep -E "VM coverage" /tmp/warm-vm.log || true