<div align="center">

<img src="docs/assets/logo.svg" alt="" width="88" height="88">

# Peerwire

**A direct wire between peers.**

Peer-to-peer networking for games, whiteboards, meetings and chat.<br>
Encrypted, NAT-traversing and real-time, with no server to run and no relay to pay for.

[Website](https://avinashid.github.io/peerwire/) ·
[Documentation](https://avinashid.github.io/peerwire/docs/) ·
[Live demos](https://avinashid.github.io/peerwire/#demos) ·
[Changelog](CHANGELOG.md)

[![CI](https://github.com/avinashid/peerwire/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/avinashid/peerwire/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/@peerwire/js?label=%40peerwire%2Fjs)](https://www.npmjs.com/package/@peerwire/js)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Status: early development](https://img.shields.io/badge/status-early%20development-yellow.svg)](docs/project/status.md)

</div>

## What is Peerwire?

Peerwire connects apps directly to each other. Two apps that share a room name or an invite find each other, punch
through their NATs and open an end-to-end encrypted link. Nothing sits in between: no signaling server, no TURN
server, no backend of yours.

It's one Rust core that runs everywhere: natively, in browsers and Node as a WebAssembly module with no imports,
behind a C ABI, inside Godot 4, and as a prototype on ESP32 microcontrollers.

```js
import { joinRoom } from '@peerwire/js';

const room = await joinRoom({ appId: 'my-app', roomId: 'lobby', signaling: 'nostr' });
room.on('message', (data, peer) => console.log(peer.slice(0, 6), data));
room.send('hello'); // to every member, over a direct encrypted link
```

## Why Peerwire

- **No backend.** Peers meet over signaling they don't have to trust: public Nostr relays, an invite link or QR
  code, or multicast on the local network. What goes over it is sealed, so a relay can't read addresses, join a
  room it doesn't know the name of, or forge a link.
- **Through NATs.** STUN, simultaneous open, port prediction, and birthday and lockstep punching. When no direct
  path opens, a peer both sides are linked to relays the ciphertext. In simulation, 59 of the 64 pairings of eight
  NAT kinds connect directly.
- **Encrypted by identity.** A peer's ID *is* its Ed25519 public key, and every link is a one-round-trip Noise IK
  session. Dialling an ID authenticates whoever answers; relays and NATs see only ciphertext.
- **Built for real time.** Three lanes per link: unreliable for state, reliable for events (ordered, up to 1 MiB)
  and urgent for inputs (ahead of the queue, sent twice). Selective ACKs, tail-loss probes and delay-based
  congestion control keep p99 latency low under loss.
- **Shared state included.** `room.doc()` gives maps, sets and logs that every member holds and that converge, with
  catch-up for late joiners and optional persistence. Rust, Godot and C read and write the same documents.
- **More than messages.** Voice and video calls between room members, sealed offline messages to one peer, and
  building blocks for games: fixed-point maths, desync detection, and islands with super-peers (simulated at 512
  players).

## Quick start

**JavaScript and React**: browsers and Node 20+.

```sh
npm install @peerwire/js        # and @peerwire/react for the hooks
```

```tsx
import { RoomProvider, useSharedState } from '@peerwire/react';

export const App = () => (
  <RoomProvider appId="my-app" roomId="lobby">
    <Counter />
  </RoomProvider>
);

function Counter() {
  const [count, setCount] = useSharedState('count', 0); // the same value in every member's tab
  return <button onClick={() => setCount((c) => c + 1)}>{count}</button>;
}
```

**Rust**: Peerwire isn't on crates.io yet, so add it from git.

```toml
[dependencies]
peerwire = { git = "https://github.com/avinashid/peerwire", features = ["nostr", "tokio"] }
tokio = { version = "1", features = ["macros", "rt"] }
```

```rust
use peerwire::{AsyncNode, Event, Lane, Node, NostrSignaling};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let node = Node::builder().signaling(NostrSignaling::default()).bind("[::]:0")?;
    let mut node = AsyncNode::from_node(node)?;
    node.join_room("my-app", "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4"); // the room name is its password
    loop {
        match node.next_event().await {
            Event::Connected { peer, .. } => node.send(&peer, Lane::Reliable, b"hello")?,
            Event::Message { peer, data, .. } => println!("{peer}: {}", String::from_utf8_lossy(&data)),
            _ => {}
        }
    }
}
```

**Godot 4**: `PeerwireMultiplayerPeer` is a drop-in `MultiplayerPeer`, so `@rpc`, `MultiplayerSynchronizer` and
`MultiplayerSpawner` work unchanged.

```gdscript
var peer := PeerwireMultiplayerPeer.new()
peer.host_room("my-game", "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4")    # on the host
# peer.join_room("my-game", "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4")  # on each player
multiplayer.multiplayer_peer = peer
```

The [quick start](docs/getting-started/quick-start.md) walks through each of these, and
[installation](docs/getting-started/installation.md) covers the C ABI, Docker and building from source.

## Packages

| Package | What it is |
|---|---|
| [`@peerwire/js`](packages/js) | Browsers and Node: rooms over WebRTC, shared documents, calls, offline messages. The core as a ~350 KB WebAssembly module with no imports. |
| [`@peerwire/react`](packages/react) | React hooks: rooms, members, chat, shared and per-member state. TypeScript. |
| [`peerwire`](crates/peerwire) | Rust: `Node` and `AsyncNode` on a UDP socket, with STUN, rooms, NAT traversal and signaling over Nostr, LAN multicast or in-process. |
| [`peerwire-core`](crates/peerwire-core) | The sans-IO session core: identity, Noise IK, lanes, congestion control, peer relay, hole punching. |
| [`peerwire-sync`](crates/peerwire-sync) | Shared documents in Rust, frame for frame with `room.doc()`. |
| [`peerwire-godot`](bindings/godot) | Godot 4 GDExtension: `PeerwireMultiplayerPeer`. |
| [`peerwire-ffi`](crates/peerwire-ffi) | The C ABI behind the WebAssembly module and native bindings. |
| [`peerwire-peer`](crates/peerwire-peer) | An optional always-on peer that relays for the peers linked to it. Runs natively or in Docker. |
| [`peerwire-game`](crates/peerwire-game), [`peerwire-sim`](crates/peerwire-sim) | Game building blocks (fixed-point maths, islands, desync detection) and a deterministic network simulator. |

`@peerwire/js` 0.2.2 and `@peerwire/react` 0.1.1 are on npm. The Rust crates aren't on crates.io yet; use them from
git ([installation](docs/getting-started/installation.md)).

## Status

Peerwire is in **early development**. The APIs will change, and builds before 1.0 don't promise to interoperate.

Every part listed above works and is tested: in a deterministic simulator that models twelve NAT behaviours, on
loopback and local networks, in headless browsers, and over public Nostr relays. It hasn't yet been tested on real
mobile carrier-grade NATs. The [status page](docs/project/status.md) has the details, part by part, and the roadmap.

## Documentation

- **Get started:** [Introduction](docs/getting-started/introduction.md) ·
  [Installation](docs/getting-started/installation.md) · [Quick start](docs/getting-started/quick-start.md)
- **Guides:** [Rooms and invites](docs/guides/rooms.md) · [Sending messages](docs/guides/messaging.md) ·
  [Shared documents](docs/guides/shared-documents.md) · [Calls](docs/guides/calls.md) ·
  [Offline messages](docs/guides/offline-messages.md) · [React](docs/guides/react.md) · [Godot](docs/guides/godot.md) ·
  [LAN discovery](docs/guides/lan-discovery.md) · [Privacy](docs/guides/privacy.md)
- **Concepts:** [Architecture](docs/concepts/architecture.md) · [NAT traversal](docs/concepts/nat-traversal.md) ·
  [Protocol](docs/concepts/protocol.md) · [Security model](docs/concepts/security-model.md)
- **Reference:** [JavaScript](docs/reference/javascript.md) · [React](docs/reference/react.md) ·
  [Rust](docs/reference/rust.md) · [C ABI](docs/reference/c-abi.md)
- **Project:** [Performance](docs/project/performance.md) · [Comparison](docs/project/comparison.md) ·
  [Testing](docs/project/testing.md) · [Threat model](docs/project/threat-model.md)

All of it is also on the [website](https://avinashid.github.io/peerwire/docs/), with the
[live demos](https://avinashid.github.io/peerwire/#demos).

## Contributing

Issues and pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) explains how to build, test and run the
gate every change passes. Please report security issues privately, as [SECURITY.md](SECURITY.md) describes, rather
than in a public issue.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your
option. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this work,
as defined in the Apache-2.0 license, is dual licensed as above, without any additional terms or conditions.
