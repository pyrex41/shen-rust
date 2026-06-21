#!/usr/bin/env bash
# Build ShenRust.xcframework (iOS device + simulator + macOS) from the shenffi
# crate. Output: <workspace>/target/ShenRust.xcframework — drag into an Xcode app,
# add the bridging header (shenffi.h) + ShenRust.swift, and you're done.
#
# The macOS slice (aarch64-apple-darwin) lets a native macOS target embed the
# same CAS — and, unlike the iOS simulator, MLX/Metal runs there, so the
# on-device model is exercisable on an Apple-silicon Mac.
set -euo pipefail

cd "$(dirname "$0")/../.."          # workspace root (shen-rust/)
HDRS="crates/shenffi/include"

for tgt in aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin; do
  rustup target add "$tgt" >/dev/null 2>&1 || true
  echo "building shenffi for $tgt ..."
  cargo build --release -p shenffi --target "$tgt"
done

echo "packaging ShenRust.xcframework ..."
rm -rf target/ShenRust.xcframework
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libshenffi.a       -headers "$HDRS" \
  -library target/aarch64-apple-ios-sim/release/libshenffi.a   -headers "$HDRS" \
  -library target/aarch64-apple-darwin/release/libshenffi.a    -headers "$HDRS" \
  -output target/ShenRust.xcframework

echo "done -> target/ShenRust.xcframework"
