# Examples

Every runnable example in the repo, what it shows, and the command that runs it. Commands run from the repo root.
Most work on one machine over loopback; the ones marked **public** use public STUN servers or Nostr relays.

## Rust programs

[`examples/rust/`](rust), built as examples of the [`peerwire`](../crates/peerwire) crate.

| Example | Shows |
|---|---|
| [echo.rs](rust/echo.rs) | `peerwire::Node`: a host prints a `pw:` invite, a joiner links to it, says hello and gets the echo |
| [echo_async.rs](rust/echo_async.rs) | the same with `peerwire::AsyncNode` on tokio |
| [lan_chat.rs](rust/lan_chat.rs) | chat on one LAN with no internet: copies in the same room find each other over UDP multicast (`LanSignaling`) |
| [room_chat.rs](rust/room_chat.rs) | the same rooms on the LAN and across the internet at once: `LanSignaling` and `NostrSignaling` in one `MultiSignaling` |
| [ble_chat.rs](rust/ble_chat.rs) | the same rooms over Bluetooth LE on Linux (`Builder::bluetooth` with the BlueZ driver) and the LAN, each link's path printed as it comes up and moves; `--no-udp` for the radio alone. See [Bluetooth](../docs/concepts/bluetooth.md#try-it) |
| [doctor.rs](rust/doctor.rs) | **public.** The doctor: what this network's NAT does (mapping, filtering, IPv6), router port mapping, and whether each Nostr relay and the LAN carrier work, as text or `--json`; with `room <name>`, meeting a peer and whether the link came up direct, relayed or not at all. Results go in [Connectivity in the field](../docs/project/field-connectivity.md) |
| [nostr_relay.rs](rust/nostr_relay.rs) | a minimal in-memory Nostr relay, for tests and LAN play without internet |

```sh
cargo run -p peerwire --example echo                                  # prints an invite, then echoes
cargo run -p peerwire --example echo -- pw:…                          # in another terminal: joins with that invite
cargo run -p peerwire --features tokio --example echo_async           # the same pair, async
cargo run -p peerwire --features tokio --example echo_async -- pw:…
cargo run -p peerwire --example lan_chat -- kitchen                   # on two or more machines on one LAN
cargo run -p peerwire --features nostr --example room_chat -- kitchen # on the LAN and over Nostr relays
cargo run -p peerwire --features ble-bluez --example ble_chat -- kitchen   # over Bluetooth and the LAN (Linux)
cargo run -p peerwire --features nostr --example doctor               # the network report (--json, --redact)
cargo run -p peerwire --features nostr --example doctor -- room <name>     # then meet a peer running the same
cargo run -p peerwire --features nostr --example nostr_relay -- 127.0.0.1:7447
```

## Core and simulator

Examples of [`peerwire-core`](../crates/peerwire-core/examples) (endpoints on real sockets) and
[`peerwire-sim`](../crates/peerwire-sim/examples) (real endpoints in the deterministic simulator, in virtual time).

| Example | Shows |
|---|---|
| [udp_pair.rs](../crates/peerwire-core/examples/udp_pair.rs) | two endpoints on loopback UDP driven by a plain loop: handshake time and message rate |
| [stun_probe.rs](../crates/peerwire-core/examples/stun_probe.rs) | **public.** Asks STUN servers (`peerwire`'s, or `host:port` arguments) for this machine's public address, to tell a cone NAT from a symmetric one, and one that maps per destination IP from one that maps per IP and port |
| [arena.rs](../crates/peerwire-sim/examples/arena.rs) | a large arena over super-peers: Hilbert islands of up to 12 players, lockstep, confirmation lag and bandwidth per role |
| [bulk.rs](../crates/peerwire-sim/examples/bulk.rs) | a bulk reliable transfer on a long, fast link, with urgent messages overtaking it |
| [cc_fair.rs](../crates/peerwire-sim/examples/cc_fair.rs) | two bulk flows sharing one uplink, and how fairly congestion control splits it |
| [docs.rs](../crates/peerwire-sim/examples/docs.rs) | shared documents at size: the numbers in [performance](../docs/project/performance.md) |

```sh
cargo run --release -p peerwire-core --example udp_pair -- 10000 unreliable   # [messages] [reliable|unreliable]
cargo run -p peerwire-core --example stun_probe
cargo run --release -p peerwire-sim --example arena -- --players 512 --supers 8
cargo run --release -p peerwire-sim --example bulk                             # --loss 0.01 --cpu 1 for variants
cargo run --release -p peerwire-sim --example cc_fair -- --buffer 1            # or --buffer 50 --loss 0.01
cargo run --release -p peerwire-sim --example docs
```

## Browser demos

In [`packages/js/demo`](../packages/js/demo). Build the wasm module once, serve `packages/js`, and open the pages:

```sh
./packages/js/build.sh
python3 -m http.server -d packages/js 8000 --bind 127.0.0.1   # then open http://127.0.0.1:8000/demo/
```

| Page | Shows | Signalling |
|---|---|---|
| [index.html](../packages/js/demo/index.html) | mesh chat on the low-level API (`loadPeerwire`): every tab links to every other | tabs of one browser; `?signal=nostr` for **public** relays |
| [chat.html](../packages/js/demo/chat.html) | chat on `joinRoom`: a room per invite link or QR code, nicknames, members with their routes, a typing indicator | **public** Nostr relays; `?signal=local` for tabs of one browser |
| [meet.html](../packages/js/demo/meet.html) | video calls of up to 6 browsers (`room.call`): camera and microphone pickers, a grid of tiles, mute | **public** Nostr relays; `?signal=local` for tabs of one browser |
| [poll.html](../packages/js/demo/poll.html) | a pull-style room: `poll()` and `sendTo()` once a frame, dots you move with the arrow keys | tabs of one browser |
| [arena.html](../packages/js/demo/arena.html) | super-peer lockstep: `?role=super` in one tab, players in others, arrow keys move | tabs of one browser; `?signal=nostr` for **public** relays |
| [netlab.html](../packages/js/demo/netlab.html) | Net Lab, connection diagnostics: ping, loss, routes, ICE candidate pairs and NAT estimates, per peer | **public** Nostr relays; `?signal=local` for tabs of one browser |
| [crew.html](../packages/js/demo/crew.html) | Crew, a small Among Us-like game with an authoritative host that relays for every player | **public** Nostr relays; `?signal=local` for tabs of one browser |

`node packages/js/demo/crew-server.mjs [port] [host]` serves the same files for hosting Net Lab and Crew elsewhere
(`/` opens Net Lab). [The JS package's README](../packages/js/README.md) describes each demo's options.

## React

[`packages/react/example`](../packages/react/example): a Vite, React and TypeScript app with presence, members, a
shared counter, chat and live cursors on `@peerwire/react`.

```sh
./packages/js/build.sh
cd packages/react && npm ci && npx vite example   # open the printed URL in two tabs
```

The room is in the URL (`?room=`). Members are tabs of one browser until you add `&signal=nostr` (**public**).

## Godot

[`examples/godot`](godot): Godot 4's own multiplayer (`@rpc`, `MultiplayerSynchronizer`, `MultiplayerSpawner`) over
Peerwire. One instance hosts, the others join by invite or by room name. Needs Godot 4.3 or newer.

```sh
cargo build --release -p peerwire-godot
godot --path examples/godot                # twice: Host in one window, paste the invite and Join in the other
GODOT=godot ./examples/godot/test.sh       # headless on loopback: invite, room and relay-peer runs; exits 0 on success
```

[The Godot guide](../docs/guides/godot.md) covers the binding and the demo's options.

## Always-on peer and the demos in Docker

[`docker/compose.yml`](../docker/compose.yml) runs the [always-on peer](../docs/guides/always-on-peer.md) and serves
the browser demos at `http://127.0.0.1:7780`, both on this machine only.

```sh
docker compose -f docker/compose.yml up -d --build
docker compose -f docker/compose.yml down
```

## ESP32

[`templates/esp32`](../templates/esp32): a project to start from, for an ESP32-C3 or ESP32 board. It joins your
Wi-Fi and a room, and answers what peers send it; `lan_chat` on a PC on the same Wi-Fi talks to it.

```sh
cargo generate --git https://github.com/avinashid/peerwire templates/esp32
cd my-board && ./flash.sh   # builds, flashes the board on USB and opens its log
```

[`ports/esp32`](../ports/esp32): the `peerwire-esp` crate the template uses, and its `mesh` example, the port's
test: emulated chips in Espressif's QEMU on one virtual LAN find each other and link into a full mesh. It has its own
toolchain (a pinned nightly, ESP-IDF, QEMU); its [README](../ports/esp32/README.md) sets it up.

```sh
cd ports/esp32
./build.sh                 # out/flash.bin
./run.sh 4 60              # 4 chips for 60 s; logs in out/node<n>.log
grep report out/node*.log
```
