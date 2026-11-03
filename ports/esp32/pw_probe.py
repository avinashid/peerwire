#!/usr/bin/env python3
"""The PC side of the firmware's LAN probe (NB-091): which paths the LAN passes between
this PC and a board running the probe. Every 2 s it sends a probe on UDP 20568 each to
Peerwire's LAN group, the mDNS group, 255.255.255.255 and the subnet's broadcast address,
and by unicast to the board and to every address it has heard a probe from. It prints the
first probe of each kind from each address, then a count of everything it heard.

Usage: pw_probe.py MY_IP BOARD_IP [SECS] [PREFIX]   (e.g. 192.168.1.10 192.168.1.13 60 24)
"""
import ipaddress
import socket
import sys
import time

PORT = 20568
GROUPS = {"mc239": "239.255.80.87", "mc224": "224.0.0.251"}


def main():
    me, board = sys.argv[1], sys.argv[2]
    secs = float(sys.argv[3]) if len(sys.argv) > 3 else 60
    prefix = int(sys.argv[4]) if len(sys.argv) > 4 else 24
    subnet = str(ipaddress.ip_network(f"{me}/{prefix}", strict=False).broadcast_address)
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    s.bind(("", PORT))
    for g in GROUPS.values():
        s.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton(g) + socket.inet_aton(me))
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(me))
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 0)
    s.settimeout(0.1)
    kinds = [*GROUPS.items(), ("bc255", "255.255.255.255"), ("bcnet", subnet)]
    print(f"probe on {me}:{PORT} for {secs:.0f} s, board {board}: {kinds}", flush=True)

    heard, failed, peers = {}, {}, {board}
    rnd, next_send, end = 0, 0.0, time.monotonic() + secs
    while time.monotonic() < end:
        if time.monotonic() >= next_send:
            next_send = time.monotonic() + 2
            rnd += 1
            for kind, to in kinds + [("uc", p) for p in sorted(peers)]:
                try:
                    s.sendto(f"PWPROBE {kind} {rnd} pc".encode(), (to, PORT))
                except OSError as e:
                    failed[kind] = failed.get(kind, 0) + 1
                    if failed[kind] == 1:
                        print(f"sending {kind} to {to}: {e}", flush=True)
        try:
            data, (src, _) = s.recvfrom(64)
        except socket.timeout:
            continue
        words = data.decode(errors="replace").split(" ")
        if len(words) < 2 or words[0] != "PWPROBE" or src == me:
            continue
        key = (src, words[1])
        heard[key] = heard.get(key, 0) + 1
        peers.add(src)
        if heard[key] == 1:
            print(f"heard {words[1]} from {src}: {' '.join(words)}", flush=True)

    print(f"{rnd} rounds sent, failed {failed}")
    for (src, kind), c in sorted(heard.items()):
        print(f"  from {src} {kind}: {c}")
    if not heard:
        print("  heard nothing")


if __name__ == "__main__":
    main()
