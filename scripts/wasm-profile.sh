#!/usr/bin/env bash
# Size and boot profile for a Yggdrasil-shaken wasm32 image.
#
# Fixture defaults to examples/wasm-profile/fib. Point FIXTURE at
# examples/wasm-profile/catalyst for the profile slice. WORK is the
# scratch directory.
#
# Builds the fixture twice:
#   full — yggdrasil-build --full (default shen-rust features, KL interpreter)
#   lean — yggdrasil-build        (no cedar, no kernel-aot, no interpreter
#          when the manifest says eval is unreachable)
# Then wasm-opt -Oz, raw/gzip/brotli sizes, a Node WASI boot, and a stdout
# check against the native lean binary.
#
# Requires: rustup target wasm32-wasip1, wasm-opt (binaryen), gzip, brotli, node.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
fixture="${FIXTURE:-$root/examples/wasm-profile/fib}"
work="${WORK:-$root/target/wasm-profile}"
mkdir -p "$work"

if ! rustup target list --installed | grep -qx wasm32-wasip1; then
  rustup target add wasm32-wasip1
fi

cargo build -p yggdrasil-build --release
builder="$root/target/release/yggdrasil-build"

scaffold() {
  local mode="$1" dir="$2"
  rm -rf "$dir"
  if [[ "$mode" == full ]]; then
    "$builder" --full "$fixture" "$dir"
  else
    "$builder" "$fixture" "$dir"
  fi
}

build_wasm() {
  local dir="$1"
  (cd "$dir" && cargo build --release --target wasm32-wasip1)
  local name
  name="$(basename "$dir")"
  local raw="$dir/target/wasm32-wasip1/release/${name}.wasm"
  wasm-opt -Oz --enable-bulk-memory --enable-multivalue --enable-simd \
    --enable-reference-types --enable-nontrapping-float-to-int \
    "$raw" -o "$dir/app.wasm"
  # Names survive in this copy so twiggy can attribute the same bodies.
  wasm-opt -Oz --debuginfo --enable-bulk-memory --enable-multivalue --enable-simd \
    --enable-reference-types --enable-nontrapping-float-to-int \
    "$raw" -o "$dir/app-named.wasm"
}

scaffold full "$work/full"
scaffold lean "$work/lean"
build_wasm "$work/full"
build_wasm "$work/lean"

# Native lean output is the oracle for this fixture (same AOT defuns).
(cd "$work/lean" && cargo build --release)
native_out="$work/lean-native.txt"
"$work/lean/target/release/lean" >"$native_out"

size_of() {
  local wasm="$1"
  local raw gzip brotli
  raw="$(wc -c <"$wasm" | tr -d ' ')"
  gzip="$(gzip -c -9 "$wasm" | wc -c | tr -d ' ')"
  brotli="$(brotli -c -q 11 "$wasm" | wc -c | tr -d ' ')"
  printf '%s %s %s\n' "$raw" "$gzip" "$brotli"
}

echo
echo "sizes after wasm-opt -Oz (raw gzip brotli bytes)"
printf 'full %s\n' "$(size_of "$work/full/app.wasm")"
printf 'lean %s\n' "$(size_of "$work/lean/app.wasm")"

node "$root/scripts/wasm-profile-boot.mjs" "$work/full/app.wasm" "$work/full-out.txt" | tee "$work/full-boot.txt"
node "$root/scripts/wasm-profile-boot.mjs" "$work/lean/app.wasm" "$work/lean-out.txt" | tee "$work/lean-boot.txt"

echo
echo "stdout vs native"
cmp "$native_out" "$work/lean-out.txt" && echo "lean wasm stdout matches native"
cmp "$native_out" "$work/full-out.txt" && echo "full wasm stdout matches native"

if command -v twiggy >/dev/null 2>&1; then
  echo
  echo "twiggy top (lean, names kept)"
  twiggy top -n 30 "$work/lean/app-named.wasm"
fi
