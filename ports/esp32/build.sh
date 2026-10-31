#!/usr/bin/env bash
# Builds the ESP32-C3 firmware and writes out/flash.bin, a full 4 MB flash image
# for Espressif's QEMU or a real board. Needs the toolchain in README.md.
set -euo pipefail
cd "$(dirname "$0")"

export MCU=esp32c3
# ESP-IDF and its tools download once to ~/.espressif, shared by every checkout.
export ESP_IDF_TOOLS_INSTALL_DIR=global
export CARGO_TARGET_RISCV32IMC_ESP_ESPIDF_LINKER=ldproxy
export CARGO_TARGET_RISCV32IMC_ESP_ESPIDF_RUSTFLAGS="--cfg espidf_time64"
# ESP-IDF's CMake build runs on ninja: `idf_tools.py install ninja` puts one under ~/.espressif (README.md).
export ESP_IDF_CMAKE_GENERATOR=Ninja
ninja_dir=$(dirname "$(ls ~/.espressif/tools/ninja/*/ninja 2>/dev/null | tail -n 1)")
[ -x "$ninja_dir/ninja" ] && export PATH="$ninja_dir:$PATH"

cargo build --release --target riscv32imc-esp-espidf -Z build-std=std,panic_abort "$@"

mkdir -p out
elf=target/riscv32imc-esp-espidf/release/peerwire-esp32
espflash save-image --chip esp32c3 --flash-size 4mb --merge "$elf" out/flash.bin
ls -l "$elf" out/flash.bin
