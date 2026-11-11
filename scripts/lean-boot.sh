#!/usr/bin/env bash
# Boot a lean slice and run fib, for every feature combination the
# initialiser can be compiled with. Native runs the regression test.
# wasm32-wasip1 runs the same example (jit excluded: Cranelift's JIT
# crate does not build for that target).
set -euo pipefail

cd "$(dirname "$0")/.."

if ! rustup target list --installed | grep -qx wasm32-wasip1; then
  rustup target add wasm32-wasip1
fi

native() {
  local label="$1"
  shift
  echo
  echo "==== native: $label ===="
  cargo test -p shen-rust --test lean_boot "$@"
}

wasm() {
  local label="$1"
  shift
  echo
  echo "==== wasm32-wasip1: $label ===="
  local out
  out="$(mktemp -d)"
  cargo build -p shen-rust --example lean_fib --target wasm32-wasip1 --target-dir "$out" "$@"
  local wasm
  wasm="$(find "$out/wasm32-wasip1" -name 'lean_fib.wasm' -type f | head -1)"
  if [[ -z "$wasm" ]]; then
    echo "lean_fib.wasm not produced"
    exit 1
  fi
  node scripts/wasm-profile-boot.mjs "$wasm" "$out/stdout.txt" >"$out/boot.json"
  python3 - "$out/stdout.txt" "$out/boot.json" <<'PY'
import json, pathlib, sys
stdout = pathlib.Path(sys.argv[1]).read_text()
boot = json.loads(pathlib.Path(sys.argv[2]).read_text())
if stdout != "fib 10 = 55\n":
    raise SystemExit(f"stdout {stdout!r}")
if boot.get("exit") not in (0, None):
    raise SystemExit(f"wasm exit {boot.get('exit')}")
print("fib 10 = 55")
PY
}

native "no-default-features" --no-default-features
native "interpreter" --no-default-features --features interpreter
native "kernel-aot" --no-default-features --features kernel-aot
native "jit" --no-default-features --features jit
native "default"

wasm "no-default-features" --no-default-features
wasm "interpreter" --no-default-features --features interpreter
wasm "kernel-aot" --no-default-features --features kernel-aot
wasm "default"
