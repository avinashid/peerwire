# Peerwire on ESP32 (prototype)

A feasibility prototype, not a supported target: the unchanged `peerwire::Node`
running under ESP-IDF on an ESP32-C3 and on the original ESP32 (the chip in the
ESP32-WROOM-32 module), tested in Espressif's QEMU and on one ESP32-C3 board on Wi-Fi
([On a board](#on-a-board-nb-091-3-nov-2026)). N emulated
chips share one virtual LAN, find each other by themselves (NB-064, LAN discovery)
and link into a full mesh with the same handshake, encryption and reliable lane as
every other Peerwire node.

It is not built by the repo's workspace, CI or `scripts/gate.sh`: it has its own
`[workspace]` and its own pinned toolchain.

## What it does

`src/main.rs` reads its node number and a 32-byte identity seed from the last flash
sector (`run.sh` patches them into each node's copy of the image), and takes:

- the address `10.62.0.<10+n>` on QEMU's emulated Ethernet (the OpenCores MAC);
- an identity from the seed, which `run.sh` draws fresh from the host's
  `/dev/urandom` for each node and each run.

It binds a `Node` to its own address with no STUN servers and `LanSignaling` (UDP
multicast on the LAN, and broadcast to its subnet: NB-092) as its carrier, and joins
one room. No node knows another's
key or address beforehand: they hear each other's room hellos and link. Linked
nodes send each other a numbered reliable message every second. `Config` caps memory for a chip
with ~250 KB of free heap: 16 KiB messages, a 32 KiB send buffer and 8 links
(the defaults allow 1 MiB). Every 10 s each node logs free heap, the lowest free
heap so far, unused main-task stack and its links' RTTs.

On a board's Wi-Fi it sets its clock by SNTP before binding the `Node`. Room hellos and
offers carry the sender's clock and are dropped more than a minute off the receiver's,
and a board's clock starts at 1970: unset, the board and a PC never linked (NB-091).
QEMU's chips all start at 1970, so they agree without it.

`l2hub.mjs` carries Ethernet frames between the QEMU instances (`-netdev dgram`
on 127.0.0.1), as a hub on one LAN segment would. Nothing leaves the host.

Two emulator-only workarounds:

- **Entropy.** QEMU's RNG gives every chip the same sequence: with identities generated
  on the chip, all of them came out the same PeerId. Hence the seed from the host. The
  seed fixes the identity only: the Node's other random draws (its endpoint and
  traversal seeds) still repeat on every chip, so two chips seal their room hellos with
  the same nonces and a listener can tell who is in the room. The emulator shows
  discovery and memory, not privacy. On a board the firmware brings Wi-Fi up, with
  modem sleep off, before it builds the Node, so every random draw comes after it
  (NB62-2), and each boot drew a different identity (NB-063). The identity isn't yet
  stored in encrypted NVS.
- **Multicast.** ESP-IDF's openeth driver sets no multicast filter and QEMU's
  OpenCores MAC drops multicast frames that miss it, so the firmware puts the MAC in
  promiscuous mode. ESP-IDF logs `failed to add mac filter` when lwIP joins the
  group; that's harmless here. A board on Wi-Fi has neither problem, though a router
  may not deliver multicast to it at all (see below).

## Results (1 Nov 2026, QEMU 9.2.2 esp-develop on a 4-core arm64 VM)

### LAN discovery (NB-064)

| | |
|---|---|
| App image | 1,094,688 B |
| A bound `Node` with `LanSignaling`, no links | 4,692 B (the carrier's 1.4 KB receive buffer and the room) |
| Heap per link, light traffic | ~4.3 KB at 7 links (8 nodes), ~6.4 KB at 3 links (4 nodes) |
| Join to full mesh | 4 nodes: 6 links in 2.1 s (most within 0.3 s). 8 nodes: 28 links in 2.0–2.7 s |

- **4 nodes, 30 s:** all 6 links up, each node at 3 links, messages flowing.
- **8 nodes, 60 s:** all 28 links up, each node at 7 links, 350–400 messages received by each.
  The only disconnects are `Replaced`: two room peers dial each other at once and the
  crossing link takes over, as on a PC. No timeouts, failed dials or crashes.
- The pairs that link at ~2 s missed the first hello and caught the next (hellos repeat
  at 2 s, 4 s, ... up to 30 s).

### The original ESP32 (NB-074, 2 Nov 2026)

The same code, built with `CHIP=esp32` for Xtensa (`xtensa-esp32-espidf`, Espressif's
`esp` toolchain 1.97.0.0) and run on QEMU's `esp32` machine. No source changes.

| | |
|---|---|
| App image | 1,054,720 B |
| Free heap at boot | 267,272 B |
| After Ethernet and the IP stack | 241,972 B |
| A bound `Node` with `LanSignaling`, no links | 4,708 B |
| Heap per link, light traffic | ~13.7 KB at 1 link, ~6.3 KB at 3 links |
| Main task stack | 24 KB configured, ~18 KB used at most |
| Join to link | 2 nodes: 382 ms. 4 nodes: 6 links in 0.6–1.4 s |

- **2 nodes, 40 s:** linked, messages flowing both ways for the whole run.
- **4 nodes, 50 s:** all 6 links up, each node at 3 links, 100 messages received by each in
  ~35 s. No disconnects, failed dials or crashes.
- **4 nodes with 16 KiB messages, 50 s:** the same; the lowest free heap was 98–129 KB.

The ESP32 starts with ~28 KB less free heap than the C3, and its per-link cost is about
the same. A board's Wi-Fi driver and a TLS connection for Nostr would take more (see below).

### Fixed invites (NB-062)

These ran before discovery, with fixed test identities and invites built from them.

| | |
|---|---|
| App image | 1,095,168 B: the whole firmware, including ESP-IDF, lwIP and the Ethernet driver |
| Free heap at boot | 295,324 B |
| After Ethernet and the IP stack | 270,392 B |
| A bound `Node`, no links | 2,776 B |
| Heap per link, light traffic | ~4.2 KB at 7 links (8 nodes), ~6 KB at 3 links (4 nodes) |
| Heap per link, 16 KiB messages every second | peaks of ~30–50 KB per link (free heap low point 118–182 KB at 3 links) |
| Main task stack | 24 KB configured, ~17 KB used at most |
| Dial to link | 2 nodes: 91 ms. 4 nodes: 61–130 ms. 8 nodes (28 links): median 269 ms, max 526 ms |

- **2 nodes, 40 s:** linked, messages flowing both ways for the whole run.
- **4 nodes, 45 s:** all 6 links up within ~0.9 s of boot, no drops, 100+ messages received by each; heap flat.
- **8 nodes, 60 s:** all 28 links up, each node at 7 links, no drops or failed dials.
- **4 nodes with 16 KiB messages, 45 s:** no send errors or drops.

Times are emulated: eight QEMU instances share four host cores, and RTTs (12–50 ms)
are the emulator and hub, not radio. Real-chip timing needs a board.

### On a board (NB-091, 3 Nov 2026)

An ESP32-C3 (rev v0.4, 4 MB flash, native USB only) on a home router's 2.4 GHz Wi-Fi,
in room `esp32` with `lan_chat` on a laptop on the same LAN (on the router's 5 GHz band).

| | |
|---|---|
| App image | 1,617,648 B, with the Wi-Fi driver and SNTP |
| Wi-Fi join | at 20 or 15 dBm, after at most one failed attempt; signal −55 to −63 dBm |
| Clock set by SNTP | 4.5 s after joining |
| Free heap with Wi-Fi up / with a bound `Node` | ~191 KB / ~185–187 KB (the `Node` took 4.5–5.9 KB) |
| Join to link with the laptop | 0.8 s |
| Heap per link | 13.4–14.8 KB at 1 link; lowest free heap 167,632 B |
| RTT to the laptop | 41–123 ms |

- **Result:** linked 0.8 s after joining and held the link for the whole ~90 s run. The
  laptop got messages #1–#89 with no gaps and the board got the laptop's. The one
  disconnect was a `Replaced` 3 ms in, when both sides dialled at once. No panics or resets.
- **Wi-Fi join.** One association attempt at full power got no answer from the AP
  (reason 4), so the firmware retries, stepping TX power 20 → 15 → 11 → 8.5 dBm and
  logging each failure's reason. Small C3 boards are known to fail association at full power.
- **Multicast.** The router never delivered multicast to the board, though broadcast and
  unicast worked both ways. `LanSignaling` now also sends to the subnet's broadcast
  address (NB-092); `NO_MULTICAST=1 ./run.sh` makes the emulator's hub do the same.
- **Clock.** Room hellos carry the sender's clock and are dropped more than a minute off.
  The board's clock started at 1970, so the board and the laptop each heard the other's
  hellos and dropped them. The firmware now sets the clock by SNTP before binding.
- **Entropy (NB-063).** With Wi-Fi up before the Node is built, 7 boots gave 7 different
  PeerIds (`cdefbd84e5cb`, `7123e58ee85f`, `43acb61bed00`, `afecca0f71e9`,
  `16c6673b8508`, `9f571feb83e4`, `1ee1be2aa889`). That shows the identity draw differs,
  and by reading every other draw (the endpoint and traversal seeds, the LAN carrier's
  tag and hash keys) comes after Wi-Fi is up too (NB62-2). The identity isn't kept
  across reboots yet.

### Rooms across the internet (NB-095, 4 Nov 2026)

On Wi-Fi, when the flash names a room, the firmware joins it over the LAN and over the
Nostr relay `wss://nostr.mom` at once (`MultiSignaling`), with STUN on, so peers
anywhere can join with `room_chat ROOM`. Without one it stays on the LAN, in room
`esp32`. The same board, in room `esp32` on both (before the name moved to flash), with
`room_chat esp32` on the laptop and on a cloud VM with a public address:

| | |
|---|---|
| App image | 2,397,856 B: rustls, ring and the webpki roots add ~780 KB |
| Free heap with a bound `Node` | ~145 KB: the `Node` and the relay's thread took 45 KB, 32 KB of it the thread's stack |
| Join to link | the VM 2.9 s (offers over the relay, then direct through the home router's NAT); the laptop 3.1 s |
| Free heap with 2 links | 81–83 KB for the whole run, lowest 63,244 B |
| Relay thread's stack | 14 KB of 32 KB used |
| RTT | the VM ~70 ms (630 ms in the first seconds), the laptop ~20 ms |

- **Result:** a full triangle, board↔laptop, board↔VM and laptop↔VM, all direct, with
  messages each way and no gaps for the whole 145 s run. No resets.
- **One socket.** ESP-IDF can't duplicate a socket (`TcpStream::try_clone` fails with
  ENOSYS), so at first every relay connection failed and was retried in silence. The
  relay's stream now owns its one socket, and the report shows the relays open.
- **Memory.** The relay inbox reserved room for 1024 events, 53 KB, from the start, and
  each event id remembered cost ~170 B: the lowest free heap was 3.9 KB with 2 links. On
  ESP-IDF `NostrSignaling` keeps 32 events, 128 ids and 8 queued.
- **Stack.** An ESP-IDF thread gets 4 KB unless it asks, and the TLS handshake
  overflowed it on every boot; the relay thread asks for 32 KB.
- **Building ring.** `build.sh` names the chip's GCC for ring's C, with `-D__ILP32__` on
  the C3 and `-mlongcalls` on the ESP32. ring draws its random numbers from
  `esp_fill_random` (feature `less-safe-getrandom-espidf`), after Wi-Fi is up.
- **SNTP.** One boot got no answer from its one SNTP server in 20 s; as the Node reads
  the clock when it is bound, it linked with no one. Asking four through lwIP's SNTP
  still took 15 s on one boot and over 50 s, silent, on two others, as lwIP gives each
  server 15 s. The firmware now asks the four itself, 2 s each, for up to 60 s, logs why
  each ask failed, then leaves lwIP's SNTP to keep the clock in step. The relay's TLS
  needs the clock too.
- **Restarts.** A boot that joined Wi-Fi but got no DHCP address stopped there; the
  firmware now restarts 5 s after any setup error.
- **Security review (NB95-1 to NB95-6).** One relay message of a few KB, parsed whole,
  could take more heap than the board had free (4.5 KB of empty tags took 192 KB). On
  ESP-IDF the carrier now takes messages of up to 4 KiB and holds 16 KiB of waiting
  events; on every target it drops a message of more than 64 JSON values, 2 objects or
  8 arrays unparsed (738 B of nested objects took 65.6 KB), and keeps its back-off when
  a frame over the cap closes the connection. A relay that
  stopped reading held every later publish; writes now time out after 5 s. The relay
  thread runs at the main task's priority, 1, not ESP-IDF's 5, which on the C3's one
  core could hold off the node's loop. The room is no longer fixed and public, the log
  escapes what members send, and the board forgets the address of a peer that left.

## What it would take to go further

- **Identity.** A board draws its key after Wi-Fi is up (NB-063) but makes a new one each
  boot. Keeping it needs encrypted NVS.
- **Clocks.** A board that can't reach an SNTP server can't link with a PC, whose clock
  differs by more than a minute (NB-093).
- **More relays.** The board uses one Nostr relay, as each costs a thread and a TLS
  session; peers elsewhere must use that relay too. If it is down, only the LAN is left.
- **Memory caps.** With heavy traffic each link holds its send and reassembly buffers.
  For many links on a plain ESP32-C3, lower `max_message` and `max_send_buffer`
  further. An ESP32-S3 with PSRAM (the Arduino Nano ESP32) has room to spare.
- **ESP32-S3** should build like the ESP32 with Espressif's Rust fork; not tried.

## Building

Everything installs under your home directory.

1. `rustup toolchain install nightly-2026-10-05 --profile minimal --component rust-src`
   (`rust-toolchain.toml` pins it), and `cargo install ldproxy`.
2. espflash 4.7 on `PATH` (release binary from esp-rs/espflash).
3. The first `./build.sh` downloads ESP-IDF v5.5.5, its RISC-V GCC, clang and CMake to
   `~/.espressif` (about 4.8 GB). Then add ninja:
   `IDF_TOOLS_PATH=~/.espressif IDF_PATH=~/.espressif/esp-idf/v5.5.5 ~/.espressif/python_env/idf5.5_py3.12_env/bin/python ~/.espressif/esp-idf/v5.5.5/tools/idf_tools.py install ninja`,
   remove the stale `CMakeCache.txt` and `CMakeFiles` under
   `target/riscv32imc-esp-espidf/release/build/esp-idf-sys/*/out/build`, and build again.
4. `./build.sh` writes `out/flash.bin`, a 4 MB image for QEMU or a board.

For the original ESP32 (`CHIP=esp32`), also install Espressif's Rust fork with
[espup](https://github.com/esp-rs/espup) 0.19: `espup install --targets esp32 --std`, then
source the `export-esp.sh` it writes (it sets `LIBCLANG_PATH`) and run
`CHIP=esp32 ./build.sh`. The first build downloads ESP-IDF's Xtensa GCC to `~/.espressif`.

Why the nightly is pinned: from 2026-10-10 an ICE on esp-idf-svc's async traits
(rust-lang/rust#164093, fixed 2026-10-20), and from 2026-10-16 std no longer builds for
ESP-IDF against libc 0.2.191 (rust-lang/rust#164555, open).

## Running

QEMU: `qemu-riscv32-softmmu` (and `qemu-xtensa-softmmu` for `CHIP=esp32`) from espressif/qemu releases, unpacked to
`~/.local/opt/esp-qemu`. On a host without SDL2 and libslirp, `apt-get download` them
(plus libpulse0, libsamplerate0, libxss1, libdecor-0-0, libsndfile1 and its codecs,
libasyncns0) and unpack them with `dpkg-deb -x` into `~/.local/opt/esp-qemu/sysroot`;
`run.sh` puts that on `LD_LIBRARY_PATH`.

```sh
./run.sh 4 60              # 4 nodes for 60 s; logs in out/node<n>.log
MSG_KIB=16 ./run.sh 4 45   # the same with 16 KiB messages
grep report out/node*.log
CHIP=esp32 ./run.sh 4 50   # the original ESP32
NO_MULTICAST=1 ./run.sh 2 40  # a LAN that drops multicast: the nodes link by broadcast
```

On a board (2.4 GHz Wi-Fi only), write the network and a room into a copy of the image,
flash it and delete the copy, which holds the password in the clear:

```sh
./wifi.sh "My Wi-Fi"     # asks for the password; writes out/board.bin, prints the room
espflash write-bin 0x0 out/board.bin && rm out/board.bin
espflash monitor
cargo run -p peerwire --features nostr --example room_chat -- ROOM   # on a PC anywhere
```

`wifi.sh` names a random room unless `PW_ROOM` names one; with `PW_ROOM=` (empty) the
board stays on the LAN, in room `esp32` (`lan_chat esp32` on a PC on the same LAN).
Anyone who has the room's name can join from anywhere, link with the board directly, and
see the home's public address and the board's LAN address. To rename a flashed board's
room, or remove it, without its Wi-Fi password, patch its config sector (`room.sh FILE
[ROOM]`; the sector holds the password in the clear, so delete it after):

```sh
espflash read-flash 0x3FF000 0x1000 cfg.bin
./room.sh cfg.bin        # a random room, printed; ./room.sh cfg.bin "" for none
espflash write-bin 0x3FF000 cfg.bin && rm cfg.bin
```

The PC's firewall must let in UDP from the LAN: port 20567 and `lan_chat`'s port, which
is random.
