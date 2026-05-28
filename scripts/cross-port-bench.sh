#!/bin/bash
# Cross-port benchmark: time the upstream Shen kernel test suite against
# shen-cedar (release) and shen-cl (interpreted). Records wall-clock so
# we can compare and iterate on perf.
#
# Both ports must be built. shen-cl lives one directory up at
# ../shen-cl/bin/sbcl/shen.
set -euo pipefail

cd "$(dirname "$0")/.."

SHEN_CEDAR_BIN="target/release/shen-cedar"
SHEN_CL_BIN="../shen-cl/bin/sbcl/shen"
KERNEL_TESTS_INPUT="kernel/tests"

if [ ! -x "$SHEN_CEDAR_BIN" ]; then
    echo "Building shen-cedar release..."
    cargo build --release --bin shen-cedar
fi

echo
echo "==== shen-cedar (release) ===="
{ time "$SHEN_CEDAR_BIN" --kernel-tests > /tmp/shen-cedar-bench.log 2>&1; } 2> /tmp/shen-cedar-bench.time
echo "Result: $(tail -1 /tmp/shen-cedar-bench.log)"
cat /tmp/shen-cedar-bench.time

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
