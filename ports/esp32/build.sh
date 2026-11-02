#!/usr/bin/env bash
# Builds the firmware and writes out/flash.bin, a full 4 MB flash image for
# Espressif's QEMU or a real board. Needs the toolchain in README.md.
# CHIP=esp32c3 (default, RISC-V, upstream nightly) or CHIP=esp32 (the original
# ESP32, as on an ESP32-WROOM-32: Xtensa, Espressif's `esp` toolchain from espup).
# Usage: [CHIP=chip] ./build.sh [cargo args]
set -euo pipefail
cd "$(dirname "$0")"

CHIP=${CHIP:-esp32c3}
case "$CHIP" in
  esp32c3) target=riscv32imc-esp-espidf; toolchain=() ;;
  esp32) target=xtensa-esp32-espidf; toolchain=(+esp) ;;
  *) echo "CHIP must be esp32c3 or esp32" >&2; exit 1 ;;
esac
var=CARGO_TARGET_$(echo "$target" | tr a-z- A-Z_)

export MCU=$CHIP
# ESP-IDF and its tools download once to ~/.espressif, shared by every checkout.
export ESP_IDF_TOOLS_INSTALL_DIR=global
export "${var}_LINKER=ldproxy"
export "${var}_RUSTFLAGS=--cfg espidf_time64"
# ESP-IDF's CMake build runs on ninja: `idf_tools.py install ninja` puts one under ~/.espressif (README.md).
export ESP_IDF_CMAKE_GENERATOR=Ninja
ninja_dir=$(dirname "$(ls ~/.espressif/tools/ninja/*/ninja 2>/dev/null | tail -n 1)")
[ -x "$ninja_dir/ninja" ] && export PATH="$ninja_dir:$PATH"

cargo "${toolchain[@]}" build --release --target "$target" -Z build-std=std,panic_abort "${@}"

mkdir -p out
elf=target/$target/release/peerwire-esp32
espflash save-image --chip "$CHIP" --flash-size 4mb --merge "$elf" out/flash.bin
ls -l "$elf" out/flash.bin
