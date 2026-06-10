#!/bin/bash
# Run the upstream Shen kernel test suite against shen-rust.
# Boots the kernel, overrides y-or-n? to always answer yes, then loads
# kernel/tests/runme.shen which in turn runs harness.shen + kerneltests.shen.
# Exits non-zero if *failed* > 0.
#
# Pass --debug to run the unoptimized dev profile instead (~50s vs ~3s): used
# by gates.sh as the heap-reentrancy tripwire — the debug build carries the
# HEAP_BORROWS sentinel (value.rs split-TLS note), so a future funnel-reentry
# bug panics deterministically here instead of being silent UB in release.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ "${1:-}" = "--debug" ]; then
    cargo run --quiet --bin shen-rust -- --kernel-tests
else
    cargo run --quiet --release --bin shen-rust -- --kernel-tests
fi
