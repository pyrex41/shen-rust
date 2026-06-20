#!/usr/bin/env bash
# Build ShenRust.xcframework (iOS device + simulator) from the shenffi crate.
# Output: <workspace>/target/ShenRust.xcframework — drag into an Xcode app,
# add the bridging header (shenffi.h) + ShenRust.swift, and you're done.
set -euo pipefail

cd "$(dirname "$0")/../.."          # workspace root (shen-rust/)
HDRS="crates/shenffi/include"

for tgt in aarch64-apple-ios aarch64-apple-ios-sim; do
  rustup target add "$tgt" >/dev/null 2>&1 || true
  echo "building shenffi for $tgt ..."
  cargo build --release -p shenffi --target "$tgt"
done

echo "packaging ShenRust.xcframework ..."
rm -rf target/ShenRust.xcframework
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libshenffi.a       -headers "$HDRS" \
  -library target/aarch64-apple-ios-sim/release/libshenffi.a   -headers "$HDRS" \
  -output target/ShenRust.xcframework

echo "done -> target/ShenRust.xcframework"
