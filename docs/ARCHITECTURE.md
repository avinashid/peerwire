# Architecture

## Principles

1. **No developer-owned servers.** Every function, including signaling, relaying, storage and super-peers, can be
   served by peers. Optional helpers (a Docker "always-on peer") are ordinary peers that happen to stay online.
2. **Sans-IO core.** `peerwire-core` never touches sockets, threads or clocks. A host feeds it datagrams and the
   time, and drains datagrams, events and the next deadline. The core is therefore:
   - deterministic, which is what lets `peerwire-sim` replay any scenario exactly;
   - portable to wasm with zero imports.
3. **Identity is a key.** A `PeerId` is an Ed25519 public key. Dialling a PeerId runs Noise IK against that key,
   so no channel in between (signaling relay, peer relay, NAT) can impersonate a peer.
4. **Signaling is untrusted.** Signaling adapters only carry offers. A tampered offer fails the handshake.

## Layers

```
 app ───────────────────────────────────────────────────────────────────────
  peerwire (facade, planned): Node/Room API, pub/sub, presence, adapters registry
  ├─ peerwire-sync   CRDT documents: maps, lists, text, presence (planned; JS has room.doc(): maps, sets, logs)
  ├─ peerwire-game   fixed-point maths, Hilbert islands, desync detection, lockstep/rollback helpers
  ├─ peerwire-media  WebRTC media signaling, mesh / peer-SFU / broadcast tree topologies (planned; JS has
                     room.call(): a mesh of up to 6, signalled over the room's links)
  └─ peerwire-store  offline store-and-forward (planned; JS has room.doc's relay copy, and openInbox(): sealed
                     messages to one PeerId left on Nostr relays)
 peerwire-core ───────────────────────────────────────────────────────────────
  Endpoint: sessions, Noise IK, lanes (unreliable / reliable / urgent), ACK + congestion control,
            tail-loss probe, keepalive, roaming, peer relay (FWD/RELAYED), hole punching, STUN codec, invites
 hosts / transports ────────────────────────────────────────────────────────
  native UDP (std / tokio)   WebRTC DataChannel (browser, packages/js)   C ABI (peerwire-ffi)
 signaling adapters ────────────────────────────────────────────────────────
  invite link / QR · BroadcastChannel · Nostr relays · mDNS (planned) · DHT (planned)
```

## Connecting two peers

1. **Learn the peer.** A gets B's PeerId and candidate addresses from an invite (`pw:…`), or from an
   adapter (a Nostr note sealed to B's key, a BroadcastChannel message, mDNS).
2. **Dial every path at once.** `connect_with` sends the Noise handshake down every known path: direct
   addresses, and through any mutual peer that can relay.
3. **Handshake.** It takes one RTT. When both sides dial at once, the dial from the smaller PeerId wins, so
   exactly one link results.

For native nodes the `peerwire` crate automates steps 1 and 2: STUN for the public address, rooms and sealed offers
over a signaling carrier, and simultaneous dials. See [NAT-TRAVERSAL.md](NAT-TRAVERSAL.md).
4. **Upgrade to direct.** If the link is relayed, it keeps punching candidates (with port spraying for
   sequential NATs). It moves to a direct path only after an echoed PUNCH_ACK.
5. **Rank paths.** Paths rank slow address, then relay, then direct. A host marks an address slow
   (`set_slow`: a browser room's signalling pipe) and offers relays for it (`add_relay`). The link then
   sends a sealed PUNCH through each relay and moves to the first whose PUNCH_ACK comes back through it. The far
   end answers only if it would use that relay too. When the relay goes, the link goes back to the slow address,
   as it does when data sent through the relay goes unacknowledged for 5 s; then it holds that relay off for a minute,
   twice as long on each repeat. A browser host names the peer whose channel each datagram came on
   (`handle_datagram_from`), so a member can't send another peer's packets on its own channel to move their link
   onto it; on UDP a link still roams to wherever its peer's newest packets come from (probes, and packets older
   than the newest over its path, move nothing).
6. **Take the fastest path of a kind.** A link races the other paths it has of its own kind: its other direct
   addresses (LAN or public, IPv4 or IPv6, when addresses are network addresses) or, while relayed, the other
   relays offered for it or seen carrying the peer's probes. A round sends a sealed PUNCH down each, its own path
   too, at once, every 2 s for 5 rounds, again whenever the path in use carries nothing for 12 s, and once more
   20 s after a move a race won, so that a move won on lost probes goes back. A path at least 15 ms and a fifth
   faster in 3 rounds, or answering when the path in use doesn't, is taken by the end with the smaller PeerId, at
   most once in 20 s, and the other end follows its packets there. A relay that leaves the link's data
   unacknowledged for 5 s, with no slow address to go back to, is held off as in step 5 and raced at once without
   being probed; either end leaves it for the path that wins. A peer answers a probe through a relay only if it
   would follow there. A direct path never gives way to a relay this way, nor a relay to the slow address.
   `path_rtt` (`pw_path_rtt_us`, JS `route(peer).rttMs`) is the RTT over the path in use.

## Lanes

| Lane | Guarantee | Use for |
|---|---|---|
| Unreliable | at most once, ≤ 1100 B, drop-oldest | positions, cursors, voice-activity |
| Reliable | ordered, exactly once, ≤ 1 MiB (fragmented) | chat, document ops, events |
| Urgent | reliable and ordered in its own sequence, ahead of queued Reliable data; every transmission sent twice, 4 ms apart; ≤ 64 KiB | inputs, hits, anything where p99 matters more than bytes |

## Scaling past a mesh: super-peers and islands

The game layer partitions players into islands of at most 12 along a Hilbert curve. Each island has a super-peer
(chosen by uplink and NAT type) that bundles inputs for its members.

- **Failover:** members keep a hot-standby link to the next-ranked candidate and migrate after 2 s without
  bundles.
- **Overload control:** when the measured queue delay exceeds 25 ms, the update interval is raised.

The same pattern (super-peer fan-out with failover) is planned for large whiteboards, chat rooms and broadcasts.

Details and measurements: [PERFORMANCE.md](PERFORMANCE.md).
