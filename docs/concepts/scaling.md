# Scaling

## The cost of a mesh

In a full mesh, each member links with every other one and sends each of them its own copy of everything: `N − 1`
streams up and down per member. That's fine for chat, whiteboards and small games, up to a dozen or so active
members. Past that, a member's upload grows with the room: at 60 updates a second, every new player adds a stream to
everyone else's uplink.

Peerwire offers three ways past it, depending on what you're building:

| Shape | How | Where |
|---|---|---|
| **A star** | one host links with every client; clients link only with the host | Rust `Role::Host` / `Role::Client`, Godot (always a star) |
| **Islands and super-peers** | players split into islands of up to 12; a super-peer in each bundles its members' traffic | `peerwire-game` and the simulator; the arena demo in the browser |
| **Calls** | a mesh of up to 16 members (6 by default), each sending its media to the others | JavaScript `room.call()`: see [calls](../guides/calls.md) |

## Islands and super-peers

The game layer sorts players along a Hilbert curve, so players near each other in the game world land in the same
**island of at most 12**. Each island runs its own lockstep and has a **super-peer**, ranked by uplink and NAT type,
that bundles its members' inputs into one packet per member per tick.

- **Failover.** Members keep a hot-standby link to the next-ranked candidate and move to it after 2 s without
  bundles.
- **Overload control.** When the measured queueing delay passes 25 ms, the super-peer raises the update interval
  rather than stalling.
- **Desync detection.** Every peer computes a checksum of its island's state each tick. Peers whose checksum
  disagrees with a two-thirds quorum have desynced.
- **Determinism.** The game maths is fixed-point (`Q16_16`), with no floating point, so every peer computes the same
  state bit for bit on every CPU, natively and in WebAssembly.

### Measured, in the simulator

12-player islands at 60 Hz lockstep, 20 ms latency with 0–10 ms jitter, 1% loss, 20% of peers behind symmetric NATs,
and 250 ms router buffers:

| | Result |
|---|---|
| 512 players, 8 super-peers | lag p50/p99 **59/79 ms**, projectile p99 96 ms, 0.06% late events |
| Upload | 5.4 Mbit/s per super-peer; ~40 kbit/s per player |
| Super-peer on 10 Mbit/s | 96 players at p99 lag under 120 ms |
| Super-peer on 20 Mbit/s | 192 players at p99 lag under 120 ms |
| Super-peer killed mid-game | detected in 267 ms; no stalls; every event delivered |
| 4,096 players | per-node load stays flat: islands are independent |

Reproduce it with:

```sh
cargo run --release -p peerwire-sim --example arena -- --players 512 --supers 8
```

**Rule of thumb.** Players per super-peer ≈ `0.8 × uplink / (60 Hz × ~170 bytes × 8)`, which gives about 98 players
on 10 Mbit/s; the simulator measured 96. [Performance](../project/performance.md#3-multiplayer-games-measured-simulator)
has the model and the full results.

## What's built and what isn't

- **Built:** the deterministic maths, island partitioning and desync detection (`peerwire-game`), the full arena
  with super-peers, failover and overload control in the simulator (`peerwire-sim`), and the arena demo in the
  browser.
- **Not built yet:** super-peers and a mesh size limit in the `Node` and `joinRoom` APIs. Today, an app that wants
  islands runs them itself over Peerwire links, as the arena does. The same pattern is planned for large whiteboards,
  chat rooms and broadcasts.

[Status](../project/status.md) tracks what's planned.
