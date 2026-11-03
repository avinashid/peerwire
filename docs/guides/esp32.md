# ESP32

> [!WARNING]
> This is a feasibility prototype, not a supported target. It has run in Espressif's QEMU emulator and on one
> ESP32-C3 board, and it doesn't keep its identity across reboots yet (see [Limits](#limits)).

The unchanged `peerwire::Node` runs under ESP-IDF on an ESP32-C3 and on the original ESP32, the chip in the
ESP32-WROOM-32 module. Emulated chips on one virtual LAN find each other with [LAN discovery](lan-discovery.md) and
link into a full mesh, with the same handshake, encryption and reliable lane as every other Peerwire node. The code
is in [`ports/esp32`](../../ports/esp32).

## What it shows

Each chip binds a `Node` to its own address, with no STUN servers and `LanSignaling` as its carrier, and joins one
room. No chip knows another's key or address beforehand. Linked chips send each other a numbered reliable message
every second, and log their free heap and link RTTs every 10 s.

Measured on 1 November 2026, with QEMU 9.2.2 (esp-develop) on a 4-core arm64 VM:

| | |
|---|---|
| App image (the whole firmware: ESP-IDF, lwIP, the Ethernet driver and Peerwire) | 1,094,688 B |
| Free heap at boot / after Ethernet and the IP stack | 295,324 B / 270,392 B |
| A bound `Node` with `LanSignaling`, no links | 4,692 B |
| Heap per link, light traffic | ~4.3 KB at 7 links, ~6.4 KB at 3 links |
| 8 chips, 60 s | 28 of 28 links; 350–400 messages received by each; no timeouts, failed dials or crashes |

Times in the emulator aren't radio times: eight QEMU instances share four host cores, and the 12–50 ms RTTs are the
emulator and its virtual hub. `Config` is set for a chip with ~250 KB of free heap: 16 KiB messages, a 32 KiB send
buffer and at most 8 links.

The original ESP32 (Xtensa, as on an ESP32-WROOM-32) builds from the same code with no changes. Measured on
2 November 2026, same emulator and host:

| | |
|---|---|
| App image | 1,054,720 B |
| Free heap at boot / after Ethernet and the IP stack | 267,272 B / 241,972 B |
| A bound `Node` with `LanSignaling`, no links | 4,708 B |
| Heap per link, light traffic | ~6.3 KB at 3 links |
| 4 chips, 50 s | 6 of 6 links, 0.6–1.4 s after joining; no timeouts, failed dials or crashes |
| 4 chips, 16 KiB messages | the same, with the lowest free heap at 98–129 KB |

## On a board

On 3 November 2026 an ESP32-C3 board on a home router's 2.4 GHz Wi-Fi linked with `lan_chat` on a laptop on the same
LAN, 0.8 s after joining the room, and held the link for the whole ~90 s run with no lost messages:

| | |
|---|---|
| App image (with the Wi-Fi driver and SNTP) | 1,617,648 B |
| Free heap with Wi-Fi up / with a bound `Node` | ~191 KB / ~185–187 KB |
| Heap per link | 13.4–14.8 KB at 1 link; lowest free heap 167,632 B |
| RTT to the laptop | 41–123 ms |

Getting there took three fixes, each now in the firmware or the library:

- **Wi-Fi join.** The board got no answer to its association request at full transmit power. The firmware retries,
  stepping the power down from 20 to 8.5 dBm; the board joined at 20 or 15 dBm.
- **Multicast.** The router never delivered multicast to the board, though broadcast worked. `LanSignaling` now also
  sends to the LAN's broadcast address ([LAN discovery](lan-discovery.md)).
- **Clock.** A board's clock starts at 1970, and room hellos more than a minute off are dropped, so the board and the
  laptop ignored each other. The firmware sets the clock by SNTP before it joins.

With Wi-Fi up first, the chip's random number generator draws real entropy: seven boots gave seven different
PeerIds. The [port's README](../../ports/esp32/README.md#on-a-board-nb-091-3-nov-2026) has the details and the
flashing steps.

## Building and running

The port has its own workspace and pinned toolchain (Rust nightly for RISC-V, Espressif's `esp` Rust toolchain for
Xtensa, ESP-IDF, Espressif's QEMU), so the
repository's normal build and CI don't touch it. Once the toolchain is installed:

```sh
cd ports/esp32
./build.sh            # builds the firmware image
./run.sh 8 60         # 8 emulated chips for 60 s, on a virtual LAN on 127.0.0.1
CHIP=esp32 ./build.sh # the same for the original ESP32 (ESP32-WROOM-32)
CHIP=esp32 ./run.sh 4 50
```

Each chip's log goes to `out/node<n>.log`. The [port's README](../../ports/esp32/README.md) lists the toolchain
versions, how to install them, and every result in detail.

## Limits

- **Entropy.** QEMU gives every chip the same random sequence, so `run.sh` gives each one an identity seed from the
  host. That fixes the identity only: the node's other random draws still repeat on every chip, and two emulated
  chips seal their room hellos with the same nonces, so a listener can tell who's in the room. The emulator shows
  discovery and memory use, not privacy. A board draws real entropy, but makes a new identity each boot: keeping it
  needs encrypted storage, not done yet.
- **Clocks.** A board that can't reach an SNTP server can't link with a PC, since their clocks differ by more than a
  minute.
- **LAN only.** `LanSignaling` reaches the local network. Reaching peers elsewhere needs Nostr signaling, which needs
  TLS on ESP-IDF; that hasn't been tried.
- **Wi-Fi.** Only 2.4 GHz networks, as on any ESP32. The emulator's numbers above don't include the Wi-Fi driver's
  heap; the board's do.
- **Other chips.** The ESP32-S3 should build the same way with Espressif's Rust fork, but hasn't been tried.
