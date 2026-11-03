# Architecture

## Principles

1. **No developer-owned servers.** Every function, including signaling, relaying, storage and super-peers, can be
   served by peers. Optional helpers, such as the [always-on peer](../guides/always-on-peer.md), are ordinary peers
   that happen to stay online.
2. **A sans-IO core.** `peerwire-core` never touches sockets, threads or clocks. A host feeds it datagrams and the
   time, and drains datagrams, events and the next deadline. So the core is deterministic, which lets
   `peerwire-sim` replay any scenario exactly, and it compiles to WebAssembly with no imports.
3. **Identity is a key.** A `PeerId` is an Ed25519 public key. Dialling a PeerId runs Noise IK against that key, so
   nothing in between (a signaling relay, a peer relay, a NAT) can pose as a peer.
4. **Signaling is untrusted.** Carriers only move sealed hellos and offers. A tampered offer is dropped, and a
   forged one can't complete a handshake.

## Layers

```text
 ┌──────────────────────────────────── your app ───────────────────────────────────┐
 │ game · whiteboard · meeting · chat                                              │
 ├──────────────────────────────── building blocks ────────────────────────────────┤
 │ peerwire-sync    shared documents: maps, sets, logs (the frames of room.doc())  │
 │ peerwire-game    fixed-point maths, Hilbert islands, desync detection           │
 │ JS: room.doc() · room.call() · openInbox()                                      │
 ├─────────────────────────────── hosts and facades ───────────────────────────────┤
 │ peerwire         Node / AsyncNode: UDP socket, STUN, rooms, NAT traversal       │
 │ peerwire-godot   PeerwireMultiplayerPeer (GDExtension)                          │
 │ peerwire-ffi     C ABI → WebAssembly (browsers, Node) and native libraries      │
 │ @peerwire/js     the wasm with WebRTC DataChannels, rooms, signaling            │
 │ peerwire-sim     deterministic network and NAT simulator                        │
 ├──────────────────────────── peerwire-core (sans-IO) ────────────────────────────┤
 │ Endpoint: sessions · Noise IK · lanes · ACKs and congestion control ·           │
 │ tail-loss probes · keepalive · roaming · peer relay · hole punching · invites   │
 ├───────────────────────────── signaling (untrusted) ─────────────────────────────┤
 │ invite / QR · Nostr relays · LAN multicast · BroadcastChannel · in-process      │
 └─────────────────────────────────────────────────────────────────────────────────┘
```

| Crate or package | Role |
|---|---|
| `peerwire-core` | The session state machine: identity, handshake, lanes, reliability, relay, punching, STUN codec, sealed boxes, invites. No I/O. |
| `peerwire` | The native host: `Node` (blocking) and `AsyncNode` (tokio) on a UDP socket, plus STUN, rooms, signaling carriers, NAT traversal and router port mapping. |
| `peerwire-ffi` | A C ABI over the core's `Endpoint`, built as a zero-import wasm module and as native libraries. |
| `@peerwire/js`, `@peerwire/react` | The browser and Node host: the wasm module over WebRTC DataChannels, rooms, shared documents, calls, offline messages; React hooks on top. |
| `peerwire-godot` | Godot's `MultiplayerPeer` over `peerwire`. |
| `peerwire-sync` | Shared documents in Rust, wire-compatible with `room.doc()`. |
| `peerwire-game` | Deterministic building blocks for multiplayer games. |
| `peerwire-sim` | A simulator that runs real endpoints over simulated latency, loss, uplinks and NATs, in virtual time. |
| `peerwire-peer` | The always-on peer, as a binary. |

[Rust crates](../reference/rust.md) describes each crate's API.

## The sans-IO core

`peerwire_core::Endpoint<A>` is a pure state machine, generic over the address type `A` (a socket address natively,
a channel handle in the browser, a simulated address in the simulator). It owns no socket, thread or clock, and
its random number generator is seeded once when it's made. A host drives it with five calls:

```text
  in                                          out
  ──                                          ───
  datagram ─► handle_datagram(now, from, bytes)
  timer    ─► handle_timeout(now)
                 poll_transmit(now) ─► (destination, bytes) ─► socket
                 poll_event()       ─► Event                ─► app
                 poll_timeout()     ─► when to wake next
```

Because time and I/O come from outside:

- **The simulator replays any scenario bit for bit**, covering latency, jitter, loss, router buffers, uplink caps and
  twelve NAT behaviours.
- **The WebAssembly build needs no imports**, and the same binary runs in browsers and in Node.
- **Every platform behaves the same**, because there's only one implementation of the protocol.

## A link's life

1. **Learn the peer.** A gets B's PeerId and candidate addresses from an invite, or from a sealed offer over a
   signaling carrier. For native nodes, `peerwire` does this with STUN, room hellos and offers: see
   [NAT traversal](nat-traversal.md).
2. **Dial every path at once.** The handshake goes down every known path: direct addresses, and through any mutual
   peer that can relay.
3. **Handshake.** It takes one round trip. When both sides dial at once, as hole punching needs, the dial from the
   smaller PeerId wins, so exactly one link results.
4. **Upgrade to direct.** A relayed link keeps punching its candidates and moves to a direct path only after an
   echoed `PUNCH_ACK`.
5. **Rank paths.** Paths rank slow address, then relay, then direct. A host marks an address slow (`set_slow`: a
   browser room's signaling pipe) and offers relays for it (`add_relay`). The link sends a sealed `PUNCH` through
   each relay and moves to the first whose `PUNCH_ACK` comes back through it; the far end answers only if it would
   use that relay too. When the relay goes, or data sent through it goes unacknowledged for 5 s, the link goes back
   to the slow address and holds that relay off for a minute, twice as long on each repeat. A browser host names the
   peer whose channel each datagram came on (`handle_datagram_from`), so a member can't move another peer's link
   onto its own channel. On UDP, a link roams to wherever its peer's newest packets come from; probes, and packets
   older than the newest over its path, move nothing.
6. **Take the fastest path of a kind.** A link races the other paths it has of the same kind: its other direct
   addresses (LAN or public, IPv4 or IPv6) or, while relayed, the other relays offered for it. A round sends a sealed
   `PUNCH` down each, its own path too, every 2 s for 5 rounds, again whenever the path in use carries nothing for
   12 s, and once more 20 s after a move. A path at least 15 ms and a fifth faster in 3 rounds, or answering when
   the path in use doesn't, is taken by the end with the smaller PeerId, at most once in 20 s, and the other end
   follows. A direct path never gives way to a relay this way, nor a relay to the slow address.
7. **Stay alive.** A keepalive goes out every 10 s, and a link is dropped after 30 s of silence.

## Lanes

Each link has an unreliable, a reliable and an urgent lane: see [sending messages](../guides/messaging.md) for how to
use them and [protocol](protocol.md#lanes-and-reliability) for how reliability works.

## Beyond a mesh

A full mesh costs each peer a stream per other member. The game layer splits large games into islands, each with a
super-peer: see [scaling](scaling.md).
