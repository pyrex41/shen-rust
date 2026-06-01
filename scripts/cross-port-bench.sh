#!/bin/bash
# Cross-port benchmark: time the upstream Shen kernel test suite against
# shen-rust (release) and shen-cl (interpreted). Records wall-clock so
# we can compare and iterate on perf.
#
# Both ports must be built. shen-cl lives one directory up at
# ../shen-cl/bin/sbcl/shen.
set -euo pipefail

cd "$(dirname "$0")/.."

SHEN_RUST_BIN="target/release/shen-rust"
SHEN_CL_BIN="../shen-cl/bin/sbcl/shen"
KERNEL_TESTS_INPUT="kernel/tests"

if [ ! -x "$SHEN_RUST_BIN" ]; then
    echo "Building shen-rust release..."
    cargo build --release --bin shen-rust
fi

echo
echo "==== shen-rust (release) ===="
{ time "$SHEN_RUST_BIN" --kernel-tests > /tmp/shen-rust-bench.log 2>&1; } 2> /tmp/shen-rust-bench.time
echo "Result: $(tail -1 /tmp/shen-rust-bench.log)"
cat /tmp/shen-rust-bench.time

if [ -x "$SHEN_CL_BIN" ]; then
    echo
    echo "==== shen-cl (interpreted) ===="
    # Drive shen-cl through stdin: cd into the tests dir, load runme.shen,
    # and exit. We can't override y-or-n? from outside, but the test
    # suite is normally non-interactive — failures should be 0 anyway.
    cat > /tmp/shen-cl-bench-input.shen <<EOF
(cd "$PWD/$KERNEL_TESTS_INPUT")
(load "runme.shen")
(cl.exit)
EOF
    { time "$SHEN_CL_BIN" < /tmp/shen-cl-bench-input.shen > /tmp/shen-cl-bench.log 2>&1; } 2> /tmp/shen-cl-bench.time
    echo "Last lines:"
    tail -5 /tmp/shen-cl-bench.log
    cat /tmp/shen-cl-bench.time
else
    echo "shen-cl not found at $SHEN_CL_BIN — skipping reference run."
fi
