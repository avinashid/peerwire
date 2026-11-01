# Peerwire on ESP32 (prototype)

A feasibility prototype, not a supported target: the unchanged `peerwire::Node`
running on an ESP32-C3 under ESP-IDF, tested in Espressif's QEMU. N emulated
chips share one virtual LAN and link into a full mesh with the same handshake,
encryption and reliable lane as every other Peerwire node.

It is not built by the repo's workspace, CI or `scripts/gate.sh`: it has its own
`[workspace]` and its own pinned toolchain.

## What it does

`src/main.rs` reads its node number from the last flash sector (`run.sh` patches
it into each node's copy of the image), and takes from it:

- the address `10.62.0.<10+n>` on QEMU's emulated Ethernet (the OpenCores MAC);
- a fixed identity, **for testing only**: anyone can derive these keys.

It binds a `Node` with no STUN servers, then dials every lower-numbered node with
an invite built from that node's identity and address. Linked nodes send each
other a numbered reliable message every second. `Config` caps memory for a chip
with ~250 KB of free heap: 16 KiB messages, a 32 KiB send buffer and 8 links
(the defaults allow 1 MiB). Every 10 s each node logs free heap, the lowest free
heap so far, unused main-task stack and its links' RTTs.

`l2hub.mjs` carries Ethernet frames between the QEMU instances (`-netdev dgram`
on 127.0.0.1), as a hub on one LAN segment would. Nothing leaves the host.

## Results (1 Nov 2026, QEMU 9.2.2 esp-develop on a 4-core arm64 VM)

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

## What it would take to go further

- **Discovery.** Here the invites are built from fixed test identities. A real swarm
  needs real identities and local discovery (mDNS or a LAN broadcast), which Peerwire
  doesn't have yet. Nostr signalling needs TLS (rustls with ring), which hasn't been
  tried on ESP-IDF.
- **Wi-Fi.** A board would use Wi-Fi instead of the emulated Ethernet. ESP-IDF's
  Wi-Fi driver takes heap of its own, which these numbers don't include.
- **Memory caps.** With heavy traffic each link holds its send and reassembly buffers.
  For many links on a plain ESP32-C3, lower `max_message` and `max_send_buffer`
  further. An ESP32-S3 with PSRAM (the Arduino Nano ESP32) has room to spare.
- **Xtensa chips** (ESP32, ESP32-S3) need Espressif's Rust fork (`espup`); this
  prototype targets the RISC-V C3 on upstream nightly.

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

Why the nightly is pinned: from 2026-10-10 an ICE on esp-idf-svc's async traits
(rust-lang/rust#164093, fixed 2026-10-20), and from 2026-10-16 std no longer builds for
ESP-IDF against libc 0.2.191 (rust-lang/rust#164555, open).

## Running

QEMU: `qemu-riscv32-softmmu` from espressif/qemu releases, unpacked to
`~/.local/opt/esp-qemu`. On a host without SDL2 and libslirp, `apt-get download` them
(plus libpulse0, libsamplerate0, libxss1, libdecor-0-0, libsndfile1 and its codecs,
libasyncns0) and unpack them with `dpkg-deb -x` into `~/.local/opt/esp-qemu/sysroot`;
`run.sh` puts that on `LD_LIBRARY_PATH`.

```sh
./run.sh 4 60              # 4 nodes for 60 s; logs in out/node<n>.log
MSG_KIB=16 ./run.sh 4 45   # the same with 16 KiB messages
grep report out/node*.log
```
