#!/usr/bin/env bash
# Writes Wi-Fi credentials into a copy of a flash image for a real board (NB-091):
# b"PWW", the SSID's length and the SSID, the password's length and the password, at
# 0x3FF040 in the last flash sector, where main.rs reads them. The board then joins
# that network instead of QEMU's Ethernet. The password comes from PW_WIFI_PASS or a
# prompt, never the command line. The output holds it in the clear: flash it, then
# delete it. ESP32 radios are 2.4 GHz only; WPA2 or WPA3, or an open network.
# Then room.sh names the board's room in OUT: PW_ROOM, a random name if unset, or none
# (the LAN only) if empty.
# Usage: [PW_WIFI_PASS=...] [PW_ROOM=...] ./wifi.sh SSID [IN=out/flash.bin] [OUT=out/board.bin]
set -euo pipefail
export LC_ALL=C

[ $# -ge 1 ] || { sed -n 's/^# Usage: //p' "$0" >&2; exit 1; }
ssid=$1
in=${2:-out/flash.bin}
out=${3:-out/board.bin}
if [ -n "${PW_WIFI_PASS+set}" ]; then
  pass=$PW_WIFI_PASS
else
  read -r -s -p "Password for $ssid (empty for an open network): " pass
  echo
fi
[ "${#ssid}" -ge 1 ] && [ "${#ssid}" -le 32 ] || { echo "the SSID must be 1 to 32 bytes" >&2; exit 1; }
[ "${#pass}" -eq 0 ] || { [ "${#pass}" -ge 8 ] && [ "${#pass}" -le 64 ]; } ||
  { echo "a WPA password is 8 to 64 bytes" >&2; exit 1; }
[ "$(stat -c %s "$in")" -eq $((4 << 20)) ] || { echo "$in is not a 4 MB image from build.sh" >&2; exit 1; }

(umask 077 && cp "$in" "$out")
# printf is a shell builtin here, so the password never shows in the process list.
{
  printf 'PWW'
  printf "\\$(printf '%03o' "${#ssid}")"
  printf '%s' "$ssid"
  printf "\\$(printf '%03o' "${#pass}")"
  printf '%s' "$pass"
} | dd of="$out" bs=1 seek=$((0x3FF040)) conv=notrunc status=none
echo "wrote $out for $ssid; delete it once the board is flashed"
"$(dirname "$0")/room.sh" "$out" ${PW_ROOM+"$PW_ROOM"}
