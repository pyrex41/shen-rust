#!/bin/bash
# Gate 10: codegen-equivalence examples for shen-cedar-authz.
#
# Runs the `equiv` example, and the `reaches_equiv` example if present,
# under shen-cedar-authz. Exits non-zero if any invoked example fails.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "shen-cedar-equiv: running equiv example"
cargo run -q -p shen-cedar-authz --example equiv

if [ -f examples/shen-cedar-authz/examples/reaches_equiv.rs ]; then
    echo "shen-cedar-equiv: running reaches_equiv example"
    cargo run -q -p shen-cedar-authz --example reaches_equiv
else
    echo "shen-cedar-equiv: reaches_equiv example not present, skipping"
fi

echo "RESULT: PASS"
