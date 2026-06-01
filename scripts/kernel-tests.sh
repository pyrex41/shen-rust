#!/bin/bash
# Run the upstream Shen kernel test suite against shen-rust.
# Boots the kernel, overrides y-or-n? to always answer yes, then loads
# kernel/tests/runme.shen which in turn runs harness.shen + kerneltests.shen.
# Exits non-zero if *failed* > 0.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo run --quiet --release --bin shen-rust -- --kernel-tests
