#!/usr/bin/env bash
# Runs N nodes (default 2) of out/flash.bin in Espressif's QEMU for SECS seconds
# (default 60), all on one virtual LAN through l2hub.mjs. Node n's serial log is
# out/node<n>.log. MSG_KIB=16 pads each node's once-a-second message to every
# peer to 16 KiB (at most the firmware's 16 KiB cap). Each node gets 32 fresh random
# bytes from the host for its identity: QEMU's RNG gives every chip the same sequence.
# CHIP=esp32 runs an image built with CHIP=esp32 on QEMU's esp32 (Xtensa) machine.
# NO_MULTICAST=1 makes the LAN drop multicast, as some home routers do to an ESP32
# (NB-091): the nodes must then find each other by broadcast.
# Usage: [CHIP=chip] [MSG_KIB=k] [NO_MULTICAST=1] ./run.sh [N] [SECS]
set -euo pipefail
cd "$(dirname "$0")"

N=${1:-2}
SECS=${2:-60}
MSG_KIB=${MSG_KIB:-0}
CHIP=${CHIP:-esp32c3}
case "$CHIP" in
  esp32c3) arch=riscv32 ;;
  esp32) arch=xtensa ;;
  *) echo "CHIP must be esp32c3 or esp32" >&2; exit 1 ;;
esac
QEMU=${ESP_QEMU:-$HOME/.local/opt/esp-qemu/qemu/bin/qemu-system-$arch}
# QEMU's SDL2/slirp libraries, unpacked user-locally (README.md).
LIBS=${ESP_QEMU_LIBS:-$HOME/.local/opt/esp-qemu/sysroot/usr/lib/aarch64-linux-gnu}
export LD_LIBRARY_PATH=$LIBS:$LIBS/pulseaudio${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
HUB=${HUB_PORT:-6200}

node l2hub.mjs "$HUB" "$(seq -s, $((HUB + 1)) $((HUB + N)))" > out/hub.log 2>&1 &
hub=$!
pids=()
for n in $(seq 1 "$N"); do
  img=out/node$n.bin
  cp out/flash.bin "$img"
  # Where main.rs reads them: b"PWN", the node number and the message size, a byte
  # each, then the identity seed.
  { printf "PWN\\$(printf '%03o' "$n")\\$(printf '%03o' "$MSG_KIB")"; head -c 32 /dev/urandom; } |
    dd of="$img" bs=1 seek=$((0x3FF000)) conv=notrunc status=none
  timeout "$SECS" "$QEMU" -machine "$CHIP" -display none -monitor none -serial "file:out/node$n.log" \
    -drive "file=$img,if=mtd,format=raw" \
    -netdev "dgram,id=lan,local.type=inet,local.host=127.0.0.1,local.port=$((HUB + n)),remote.type=inet,remote.host=127.0.0.1,remote.port=$HUB" \
    -net "nic,netdev=lan,model=open_eth,macaddr=02:62:00:00:00:$(printf '%02x' "$n")" &
  pids+=($!)
done
wait "${pids[@]}" || true
kill "$hub"
wait "$hub" || true
cat out/hub.log
