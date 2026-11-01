# How Peerwire compares

This is an honest, feature-level comparison based on each project's documented design. It is not a benchmark. Where
Peerwire is ahead only on the roadmap, the table says *planned*.

| | Peerwire | GunJS | Yjs + y-webrtc | PeerJS | libp2p | Trystero | Hyperswarm |
|---|---|---|---|---|---|---|---|
| Core language | Rust (wasm, C ABI, JS) | JS | JS | JS | Go, Rust, JS, … | JS | JS (Node) |
| Runs in browsers | yes, ~350 KB wasm | yes | yes | yes | yes (limited transports) | yes | no |
| Runs natively / mobile | yes (Rust, C ABI, Godot; ESP32 prototype) | Node | Node | no | yes | no | Node, Bare |
| Needs a server you run | **no** (uses public Nostr relays and STUN, which you can swap) | relay peers in practice | signaling server | PeerServer | bootstrap nodes | **no** | no (public DHT) |
| Signaling | invite/QR, BroadcastChannel, Nostr, LAN multicast; DHT planned | relay peers | WebSocket server | PeerServer | DHT, rendezvous | Nostr, BitTorrent, MQTT, IPFS | DHT |
| Hard NAT (symmetric/CGNAT) | hole punch + **peer relay**, no TURN | through relay peers | TURN needed | TURN needed | circuit relay v2 + DCUtR | TURN needed | hole punch + relay |
| E2E encryption, authenticated peer IDs | yes (Noise IK, ID = public key) | opt-in (SEA) | transport only | transport only | yes (Noise/TLS) | transport only | yes (Noise) |
| Unreliable / real-time lane | **yes** (unreliable, reliable, urgent) | no | no | via DataChannel options | stream-based | no | stream-based |
| Game netcode (lockstep, islands, failover) | **yes** | no | no | no | no | no | no |
| Shared state / CRDT | yes: maps, sets, logs (`room.doc()`, `peerwire-sync`); no rich text | **yes** (graph CRDT) | **yes** (best-in-class CRDT) | no | no | no | via Hypercore |
| Offline delivery | partial: sealed messages to one peer (`openInbox()`) and a sealed document copy on relays; store-and-forward planned | yes (relay peers) | via providers | no | no | no | via Hypercore |
| Audio/video signaling | yes, in the browser (`room.call()`, a mesh of up to 16) | no | no | **yes** | no | yes | no |
| Deterministic network simulator | **yes** | no | no | no | partial (testground) | no | no |

## Where Peerwire aims to win

- **Truly zero backend, including hard NATs.**
  - Other browser libraries fall back to a TURN server (cost) or to relay peers that someone has to run.
  - In Peerwire, any connected peer can relay for others, with rate limits, and the link upgrades to direct when a
    hole punch succeeds.
- **Real-time first.**
  - GunJS and Yjs sync state over reliable streams.
  - Peerwire has an unreliable lane, an urgent lane and tail-loss probes, so games and live cursors stay smooth
    under loss.
- **One core everywhere.**
  - The same Rust core runs in browsers, on servers and desktops, in Godot, and on embedded hosts.
  - Bugs get fixed once.
- **Measured, reproducible performance.** Every claim in [Performance](performance.md) comes from a command
  you can run, or shows its formula.

## Where others are ahead today

- **Yjs:** a far more mature CRDT, with rich text and editor bindings.
  - Peerwire's documents are maps, sets and logs; for rich text, Yjs over Peerwire links is the better fit.
- **GunJS:** an existing graph database with a large ecosystem.
- **libp2p:** many transports, a large production deployment, and a formal spec.
- **Trystero:** more signaling strategies (BitTorrent, MQTT, IPFS) and a smaller API.
- **Maturity:** every library here has more production use than Peerwire, which is pre-1.0 and unaudited.
