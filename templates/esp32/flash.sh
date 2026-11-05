#!/usr/bin/env bash
# Builds the firmware for the {{chip}}, flashes the board on USB and opens its log (Ctrl+C leaves it).
#
#   ./flash.sh                     build, flash and open the log
#   ./flash.sh --features nostr    the same, with cargo's options
#   ./flash.sh build [options]     build only
#
# The chip's build settings are here, so plain `cargo build` builds for this PC, and fails. The first build
# downloads ESP-IDF and its tools to ~/.espressif, once.
set -eu
cd "$(dirname "$0")"

# espup's libclang and the chip's GCC, unless this shell has them already.
if [ -z "${LIBCLANG_PATH-}" ] && [ -f ~/export-esp.sh ]; then . ~/export-esp.sh; fi

target={% if chip == "esp32" %}xtensa-esp32-espidf{% else %}riscv32imc-esp-espidf{% endif %}
t=CARGO_TARGET_$(echo "$target" | tr a-z- A-Z_)
export CARGO_BUILD_TARGET=$target CARGO_UNSTABLE_BUILD_STD=std,panic_abort
export "${t}_LINKER=ldproxy" "${t}_RUSTFLAGS=--cfg espidf_time64" "${t}_RUNNER=espflash flash --monitor"
export ESP_IDF_TOOLS_INSTALL_DIR=${ESP_IDF_TOOLS_INSTALL_DIR:-global}
# ring (the `nostr` feature) compiles C with the chip's GCC.
case $target in
    riscv32imc-esp-espidf)
        export CC_riscv32imc_esp_espidf=riscv32-esp-elf-gcc CFLAGS_riscv32imc_esp_espidf=-D__ILP32__ ;;
    xtensa-esp32-espidf)
        export CC_xtensa_esp32_espidf=xtensa-esp32-elf-gcc CFLAGS_xtensa_esp32_espidf=-mlongcalls ;;
esac

cmd=run
if [ "${1-}" = build ]; then cmd=build; shift; fi
exec cargo "$cmd" --release "$@"
