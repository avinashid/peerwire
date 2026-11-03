# Status and roadmap

Peerwire is in **early development**. The APIs will change between releases, builds before 1.0 don't promise to
interoperate, and nothing has had an external security audit. This page says what works, how it's tested, and
what's next. It's updated as parts land; the [changelog](../../CHANGELOG.md) has the history.

## Releases

| Package | Released | On `main` |
|---|---|---|
| `@peerwire/js` | 0.2.1 on npm (3 Nov 2026) | 0.2.1 |
| `@peerwire/react` | 0.1.1 on npm (1 Nov 2026) | 0.1.1 |
| Rust crates | not on crates.io; use them from git | 0.1.0 |
| Godot extension | built from source | |

## What works

| Part | Status | Tested |
|---|---|---|
| Session core: identity, Noise IK, lanes, congestion control, peer relay, hole punching, invites | ✅ | unit tests, fuzzing, the simulator |
| `Node` / `AsyncNode`: UDP host, STUN, rooms, NAT traversal, router port mapping | ✅ | loopback, the simulator, public Nostr relays and STUN (opt-in) |
| Signaling: Nostr relays, in-process (`MemoryHub`) | ✅ | a local relay; public relays (opt-in) |
| LAN discovery (`LanSignaling`), on PCs and ESP32 | ✅ | loopback, Docker networks, 8 emulated ESP32-C3s, 4 emulated ESP32s |
| JavaScript package: rooms over WebRTC, Nostr and BroadcastChannel signaling, fallback routes | ✅ | Node, headless Chromium, local fake relays |
| Shared documents in JS (`room.doc()`): maps, sets, logs, catch-up, persistence, relay copies | ✅ | Node and browsers, against local fake relays |
| Shared documents in Rust, Godot and the C ABI (`peerwire-sync`) | ✅ | against `room.doc()`'s own frames |
| Calls in JS (`room.call()`): audio and video, a mesh of up to 16 | ✅ | Node, and headless Chromium with fake media devices |
| Offline messages in JS (`openInbox()`) | ✅ | local fake relays |
| React hooks (`@peerwire/react`) | ✅ | jsdom, and a Vite example |
| Godot 4 `PeerwireMultiplayerPeer` (desktop) | ✅ | headless Godot 4.7 |
| C ABI and the WebAssembly module (~350 KB, no imports) | ✅ | size and import checks on every build |
| Always-on peer (`peerwire-peer`) in Docker | ✅ | Docker on loopback |
| Game layer: fixed-point maths, Hilbert islands, desync detection; arena simulation | ✅ | unit tests, the simulator |
| ESP32-C3 and ESP32 (ESP32-WROOM-32) | 🧪 prototype | Espressif's QEMU only |

## How far it's been tested

- **NAT traversal** is verified in a deterministic simulator that models twelve NAT behaviours. In a room of two with
  signaling only, 59 of the 64 pairings of the first eight connect directly, in 0.36–2.43 s. The other five are a
  random-port symmetric NAT against another symmetric one, which no hole punch can link
  ([the odds](../concepts/nat-traversal.md#two-symmetric-nats)); a third peer in the room, or a relay peer, connects
  them. The other four kinds, symmetric NATs that filter by IP only or map one port per IP, link with every cone NAT,
  and the one that maps per IP and filters by IP with every symmetric NAT too, by a birthday punch on both sides
  (116 of all 144 pairings connect directly).
- **Real networks:** public Nostr relays and STUN, from one host; and field tests that linked an Android phone on
  mobile data with a laptop on its hotspot, and two Android phones on mobile data directly over IPv6.
- **Not tested yet:** real mobile carrier-grade NATs at scale, real ESP32 boards, and Godot on Android, iOS or the
  web.

Every change runs a gate of about a hundred checks before merge: Rust tests and lints, the WebAssembly size budget,
Node and headless-browser tests, a debug fuzz run, and CI's Node list on Node 22. [Testing](testing.md) has the
details.

## Roadmap

Near term:

- Publish the Rust crates on crates.io.
- An outer tag on signaling messages, so junk floods can't stop two members linking (NB-065).
- Real entropy and stored keys on ESP32 boards (NB-063).

Planned:

- Super-peers and a mesh size limit in the `Node` and `joinRoom` APIs, as the game layer has today
  ([scaling](../concepts/scaling.md)).
- DHT signaling, as one more carrier.
- `peerwire-media` (audio and video outside the browser) and `peerwire-store` (store-and-forward while nobody is
  online: see the [offline store design](offline-store-design.md)).
- Godot on Android, iOS and the web; higher-level mobile bindings over the C ABI.

Ideas and requests are welcome as [GitHub issues](https://github.com/avinashid/peerwire/issues).
