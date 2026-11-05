# ESP32

> [!WARNING]
> This is a feasibility prototype, not a supported target. It has run in Espressif's QEMU emulator and on one
> ESP32-C3 board, and it doesn't keep its identity across reboots yet (see [Limits](#limits)).

The unchanged `peerwire::Node` runs under ESP-IDF on an ESP32-C3 and on the original ESP32, the chip in the
ESP32-WROOM-32 module. Emulated chips on one virtual LAN find each other with [LAN discovery](lan-discovery.md) and
link into a full mesh, with the same handshake, encryption and reliable lane as every other Peerwire node. The code
is in [`ports/esp32`](../../ports/esp32): the `peerwire-esp` crate.

## Start a project

`peerwire-esp` wraps what a board needs around the node: Wi-Fi, a clock set before the node binds, caps sized for the
chip, and signaling on the LAN and, optionally, over a Nostr relay. A
[`cargo generate`](https://github.com/cargo-generate/cargo-generate) template starts a project on it.

**Once, the toolchain:**

1. [Rust](https://rustup.rs), then `cargo install espup ldproxy espflash cargo-generate`. On Linux, espflash needs
   `libudev-dev` and `pkg-config` first.
2. `espup install`: Espressif's Rust (the `esp` toolchain, for the ESP32-C3 and the ESP32 alike) and its GCC. It
   writes `~/export-esp.sh`, which the project's `flash.sh` loads.
3. ESP-IDF's own prerequisites. On Debian or Ubuntu: `sudo apt install git wget flex bison gperf python3 python3-pip
   python3-venv ccache libffi-dev libssl-dev dfu-util libusb-1.0-0`; on macOS: `brew install dfu-util`. The first
   build fetches CMake and Ninja itself if you have none. On Linux, add yourself to the `dialout` group to reach the
   board's serial port.

**Then, a project:**

```sh
cargo generate --git https://github.com/avinashid/peerwire templates/esp32 --name my-board
cd my-board
./flash.sh
```

The template asks for the chip (`esp32c3` or `esp32`), your Wi-Fi network (2.4 GHz only) and its password, which it
keeps in `cfg.toml` (git ignores that file), and a room. `./flash.sh` builds the firmware, flashes the board on USB
and opens its log; it holds the chip's build settings (target, linker, `build-std`), so build with it, or with
`./flash.sh build`, not plain `cargo build`. The first build downloads ESP-IDF and its tools to `~/.espressif`,
several GB, and takes 10 to 20 minutes; later builds take seconds.

**Talk to it** from a PC on the same Wi-Fi, in the room you chose:

```sh
git clone https://github.com/avinashid/peerwire && cd peerwire
cargo run -p peerwire --example lan_chat -- esp32
```

Type a line, and the board answers `echo: …`. If `lan_chat` says `(sent to 0)`, the PC's firewall is likely dropping
the board's UDP; as `lan_chat`'s port is random, allow the board by address (`sudo ufw allow from 192.168.1.13 proto
udp`, with your board's address). With `--features nostr` the board also joins its room over a Nostr relay, so
`room_chat` on a PC anywhere links with it. The project's README says what else to check if it doesn't link.

| `peerwire-esp` | |
|---|---|
| `peerwire_esp::run(app)` | Starts ESP-IDF's logger and runs `app`; restarts the chip if it fails |
| `Board::connect(ssid, password)` | Joins Wi-Fi, sets the clock by SNTP (waiting up to 60 s) and keeps it in step; if it was set late, restarts the board so the node binds with it |
| `board.node()` | A node on the board's address, sized for the chip, with LAN signaling |
| `board.node_with_nostr(relay)` | The same, with a Nostr relay too (feature `nostr`) |
| `board.builder()` | The node's builder, to change before binding: an identity of your own, say |
| `peerwire_esp::idle(&node)` | The loop's sleep: until the node's next timer, 10 ms at most |

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

With Wi-Fi up before the node is built, seven boots gave seven different PeerIds, and every random draw the node
makes comes after Wi-Fi is up (NB62-2 in the threat model).
The [port's README](../../ports/esp32/README.md#on-a-board-nb-091-3-nov-2026) has the details and the flashing steps.

On 5 November 2026 the starter, as a user gets it, ran on the same board: a project generated from the template, on
`peerwire-esp` 0.1.1 from crates.io, built and flashed with `./flash.sh`, and echoed `lan_chat`'s lines:

| | |
|---|---|
| First `./flash.sh build`, ESP-IDF included | 11 min 6 s, no warnings; `~/.espressif` 5.5 GB |
| App image | 1,636,480 B |
| Clock set by SNTP | in 52 and 87 ms (two boots) |
| Reset to link with `lan_chat` | 8.4 s |
| A line to its echo | 51 ms |

The PC's firewall dropped the board's UDP until it was allowed by address, and when the board restarts with a new
identity, `lan_chat` counts the old one as a peer until its link times out, 30 s later.

### Rooms across the internet

On 4 November 2026 the same board joined its room over the LAN and the Nostr relay `wss://nostr.mom` at once, with
[`MultiSignaling`](../reference/rust.md#multisignaling), and linked directly with `room_chat` on the laptop and on a
cloud VM, through the home router's NAT. All three linked with each other and held for the whole 145 s run:

| | |
|---|---|
| App image (with TLS: rustls, ring and the webpki roots) | 2,397,856 B |
| Free heap with a bound `Node` / with 2 links | ~145 KB / 81–83 KB, lowest 63,244 B |
| Join to link | 2.9 s to the VM, 3.1 s to the laptop |
| RTT | ~70 ms to the VM, ~20 ms to the laptop |

The board joins a room over the relay only when its flash names one: `wifi.sh` writes a random name and prints it, and
`room.sh` changes it. Anyone who has the name can join from anywhere, link with the board directly, and see the home's
public address and the board's LAN address, so keep it to the people you'd let in. Without a name the board stays on
the LAN, in room `esp32`.

On ESP-IDF, `NostrSignaling` gives its relay thread a 32 KB stack and takes less than elsewhere, as an ESP32 has about
300 KB of heap: relay messages of up to 4 KiB, 32 events or 16 KiB waiting, 8 queued. Firmware of your own should run
the relay thread at the main task's priority, 1, with `ThreadSpawnConfiguration { priority: 1, .. }` around
`NostrSignaling::new`: ESP-IDF's default, 5, lets a relay that streams events hold off the node's loop on a one-core
chip. The [port's README](../../ports/esp32/README.md#rooms-across-the-internet-nb-095-4-nov-2026) has what it took.

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
  discovery and memory use, not privacy. A board makes every random draw after Wi-Fi is up, but a new identity each
  boot: keeping it needs encrypted storage, not done yet. Firmware of your own must keep that order: until Wi-Fi is
  started, ESP-IDF's random numbers are only pseudo-random, so start it, with modem sleep off, before you build a
  `Node`.
- **Clocks.** A board that can't reach an SNTP server can't link with a PC, since their clocks differ by more than a
  minute, and can't reach a Nostr relay, whose certificate it can't check.
- **One relay.** The board uses one Nostr relay, as each costs a thread and a TLS session, so peers elsewhere must use
  that relay too. If it is down, only the LAN is left.
- **Wi-Fi.** Only 2.4 GHz networks, as on any ESP32. The emulator's numbers above don't include the Wi-Fi driver's
  heap; the board's do.
- **Other chips.** The ESP32-S3 should build the same way with Espressif's Rust fork, but hasn't been tried.
