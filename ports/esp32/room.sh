#!/usr/bin/env bash
# Names the room a board on Wi-Fi joins (NB-095, NB95-4): b"PWR", the name's length and
# the name, at 0x3FF0C0 in the last flash sector, where main.rs reads it. With a room the
# board joins it on the LAN and over the Nostr relay wss://relay.primal.net, so `room_chat ROOM`
# anywhere on the internet links with it; without one it stays on the LAN, in room
# "esp32". Anyone who knows the name can join from anywhere, link with the board directly
# and see its home's public address and LAN address: so the default is a random name. An
# empty ROOM ("") removes the record.
# FILE is a 4 MB image from build.sh or wifi.sh, or the 4 KB sector read from a board,
# which keeps the board's Wi-Fi record (and holds its password in the clear: delete it
# once written back):
#   espflash read-flash --port /dev/ttyACM0 0x3FF000 0x1000 cfg.bin
#   ./room.sh cfg.bin && espflash write-bin --port /dev/ttyACM0 0x3FF000 cfg.bin && rm cfg.bin
# Usage: ./room.sh FILE [ROOM]
set -euo pipefail
export LC_ALL=C

[ $# -ge 1 ] || { sed -n 's/^# Usage: //p' "$0" >&2; exit 1; }
file=$1
case $(stat -c %s "$file") in
  $((4 << 20))) at=$((0x3FF0C0)) ;;
  4096) at=$((0xC0)) ;;
  *) echo "$file is neither a 4 MB image nor a 4 KB config sector" >&2; exit 1 ;;
esac
if [ $# -ge 2 ]; then
  room=$2
else
  room=esp32-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
fi
[ -z "$room" ] || [[ $room =~ ^[[:graph:]]{1,64}$ ]] ||
  { echo "a room name is 1 to 64 printable ASCII bytes, no spaces" >&2; exit 1; }

# Erased flash (0xff) first, so no byte of a longer name stays behind.
head -c 68 /dev/zero | tr '\0' '\377' | dd of="$file" bs=1 seek=$at conv=notrunc status=none
if [ -z "$room" ]; then
  echo "$file: no room, so the board stays on the LAN in room esp32"
  exit 0
fi
{
  printf 'PWR'
  printf "\\$(printf '%03o' "${#room}")"
  printf '%s' "$room"
} | dd of="$file" bs=1 seek=$at conv=notrunc status=none
echo "$file: room $room; join it from a PC with: room_chat $room"
