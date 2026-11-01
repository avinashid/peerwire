# Introduction

Peerwire is a networking toolkit for apps whose users talk to each other directly: multiplayer games, shared
whiteboards, meetings and chat. Two apps that share a room name or an invite find each other, punch through their
NATs and open an end-to-end encrypted link. There's no server of yours in between: no signaling server, no TURN
relay, no backend to run or pay for.

## What you get

- **Rooms.** Join a room by name and every other member connects to you. Or hand someone an invite string (a link
  or a QR code) and connect to exactly that peer.
- **Links that get through.** Peers learn their public address with STUN and dial each other at the same moment,
  so both NATs open. Harder NATs get port prediction, birthday punching and lockstep prediction. When no direct path
  opens, a peer that both sides are linked to forwards the encrypted traffic.
- **Encryption tied to identity.** A peer's ID is its public key. Every link is a Noise IK session, so dialling an
  ID proves you reached whoever holds that key. Relays and routers in the path see only ciphertext.
- **Real-time messaging.** Each link has three lanes: unreliable (latest state), reliable (ordered events, up to
  1 MiB) and urgent (inputs that must arrive first).
- **Shared documents.** Maps, sets and logs that every member of a room holds and that converge, with catch-up for
  people who join late.
- **Calls and offline messages** in the JavaScript package: audio and video between room members, and sealed
  messages left for a peer that's offline.

## Where it runs

The core is one Rust crate, `peerwire-core`, that never touches a socket, thread or clock itself. Thin hosts drive
it on each platform:

| Platform | Use |
|---|---|
| Browsers and Node 20+ | [`@peerwire/js`](../reference/javascript.md) (a ~350 KB WebAssembly module with no imports, over WebRTC DataChannels), and [`@peerwire/react`](../guides/react.md) |
| Linux, macOS, Windows | the [`peerwire`](../reference/rust.md) crate: `Node` (blocking) or `AsyncNode` (tokio) on a UDP socket |
| Godot 4.3+ (desktop) | [`PeerwireMultiplayerPeer`](../guides/godot.md), a GDExtension |
| Swift, Kotlin, Dart, C | the [C ABI](../reference/c-abi.md) (`pw_*`), as a native library |
| ESP32-C3 | a [prototype](../guides/esp32.md), run in Espressif's QEMU |
| Docker | an [always-on peer](../guides/always-on-peer.md) that relays for the peers linked to it |

Because every platform runs the same core, a browser, a Rust program and a Godot game can share a room.

## How two peers connect

```text
  Peer A                     signaling (untrusted)                     Peer B
    │   1. STUN: learn my public address and NAT type                    │
    │──── hello on the room's topic (sealed with the room key) ────────►│
    │◄─── hello ─────────────────────────────────────────────────────────│
    │──── offer sealed to B's key: my addresses, NAT type, my peers ───►│
    │◄─── offer sealed to A's key ───────────────────────────────────────│
    │                                                                    │
    │◄═══ 2. both dial every address at once: Noise IK handshake ══════►│
    │◄═══ 3. encrypted lanes (or relayed through a mutual peer) ═══════►│
```

1. **Learn your address.** STUN runs on the session's own socket, so the address it reports is the mapping the link
   will use. Asking two servers tells an open or cone NAT from a symmetric one.
2. **Find each other.** Peers announce themselves on a topic derived from the room name, over a publish/subscribe
   carrier: public Nostr relays, LAN multicast, a browser's BroadcastChannel, or in-process. The carrier sees hashes
   and sealed bytes, never addresses.
3. **Swap candidates privately.** Each peer seals its candidate addresses to the other's public key.
4. **Dial everything at once.** Both sides start the handshake on every candidate, and through any peer they share.
   The first path to answer wins. A relayed link keeps trying and moves to a direct path once one opens.

[NAT traversal](../concepts/nat-traversal.md) has the full story, and [architecture](../concepts/architecture.md)
shows how the pieces fit.

## When to use it

Peerwire fits apps where users are online together and talk to each other: games, collaborative editors,
whiteboards, calls, chat, local-network tools. It works best when latency matters and you'd rather not run servers.

It's a poor fit when you need a central authority (a server that decides who wins or what's true), durable storage
while every user is offline (a room's document can leave a sealed copy on the relays, but relays keep it only for a
while: see the [offline store design](../project/offline-store-design.md)), or guarantees about real mobile
carrier-grade NATs today: those haven't been tested yet. [Status](../project/status.md) lists what's tested and what isn't, and
[comparison](../project/comparison.md) sets Peerwire next to libraries like libp2p, PeerJS, Trystero and Yjs.

## Next steps

- [Install](installation.md) the package for your platform.
- Follow the [quick start](quick-start.md) to get two peers talking.
- Read [rooms and invites](../guides/rooms.md) to choose how your peers meet.
