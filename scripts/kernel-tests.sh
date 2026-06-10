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
#
# Pass --debug-gc to run the debug suite with GC Step-4 collection forced
# aggressive (SHEN_RUST_GC with a small trigger floor): full mark/sweep
# cycles run under the live sentinel AND the debug poison-on-sweep (freed
# node words become 0xDEAD…), so a missed root or a heap-touching sweep Drop
# fails deterministically here instead of being silent release UB. On targets
# without the conservative scan the env var is refused (warning on stderr)
# and this degrades to a plain --debug run — still a valid suite run.
set -euo pipefail

cd "$(dirname "$0")/.."

case "${1:-}" in
--debug)
    cargo run --quiet --bin shen-rust -- --kernel-tests
    ;;
--debug-gc)
    SHEN_RUST_GC=100000 cargo run --quiet --bin shen-rust -- --kernel-tests
    ;;
*)
    cargo run --quiet --release --bin shen-rust -- --kernel-tests
    ;;
esac
