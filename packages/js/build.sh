#!/bin/sh
# Builds the browser module into packages/js/pkg/ (size-optimised, no wasm-bindgen needed).
set -e
cd "$(dirname "$0")/../.."
export CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
  CARGO_PROFILE_RELEASE_STRIP=true CARGO_PROFILE_RELEASE_PANIC=abort
mkdir -p packages/js/pkg
cargo build --release -p peerwire-ffi --target wasm32-unknown-unknown --target-dir target/web
cp target/web/wasm32-unknown-unknown/release/peerwire_ffi.wasm packages/js/pkg/
# The same with Bluetooth (pw_ble_*, for BluetoothTransport): AES-GCM, HMAC-SHA256 and Ed25519 signatures make it
# about 50 KB larger, so only a page that uses Bluetooth loads it.
cargo build --release -p peerwire-ffi --features ble --target wasm32-unknown-unknown --target-dir target/web
cp target/web/wasm32-unknown-unknown/release/peerwire_ffi.wasm packages/js/pkg/peerwire_ffi_ble.wasm
ls -l packages/js/pkg/peerwire_ffi.wasm packages/js/pkg/peerwire_ffi_ble.wasm
