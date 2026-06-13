#!/bin/bash
# Code-coverage wrapper for the shen-rust workspace.
#
# Runs `cargo llvm-cov` over the workspace (unit + the port-authored
# integration suites: cli_launcher, primitives_coverage, io_coverage,
# error_robustness, reader_fuzz, library, plus the pre-existing
# differential/smoke suites). This measures the PORT-AUTHORED test surface,
# NOT the canonical kernel certification suite — the kernel suite runs as the
# separate `kernel-tests` gate (scripts/kernel-tests.sh) and exercises the
# vendored kernel end to end through the binary, which llvm-cov instruments
# poorly. Keep the two tiers distinct (see README "Two test tiers").
#
# This tool is OPTIONAL: if `cargo-llvm-cov` is not installed it SKIPS
# gracefully (exit 0) with an install hint, so it never blocks the gates.
#
# Usage:
#   scripts/coverage.sh            # text summary to stdout
#   scripts/coverage.sh --html     # also write an HTML report under target/llvm-cov
#   scripts/coverage.sh --lcov     # also write lcov.info (for CI upload)
set -euo pipefail

cd "$(dirname "$0")/.."

if ! cargo llvm-cov --version >/dev/null 2>&1; then
    echo "coverage: cargo-llvm-cov not installed — skipping."
    echo "  install with: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview"
    exit 0
fi

extra=()
case "${1:-}" in
--html)
    extra=(--html)
    echo "coverage: writing HTML report to target/llvm-cov/html/"
    ;;
--lcov)
    extra=(--lcov --output-path lcov.info)
    echo "coverage: writing lcov.info"
    ;;
"")
    : # text summary (default)
    ;;
*)
    echo "coverage: unknown option ${1}; use --html or --lcov" >&2
    exit 2
    ;;
esac

# `--workspace` covers all crates; the integration tests under
# crates/shen-rust/tests are included automatically.
cargo llvm-cov --workspace "${extra[@]}"
