# Changelog

Notable changes to Peerwire's packages. The npm packages follow [semantic versioning](https://semver.org); before 1.0,
a minor version may break the API. The Rust crates aren't released yet: use them from git, and pin a commit.

Every security finding behind a fix below is in the [threat model](docs/project/threat-model.md), with its severity
and the commit that fixed it.

## Unreleased

### Rust crates and Godot

- **LAN discovery:** `LanSignaling` runs rooms over UDP multicast on the local network, with no internet.
- **ESP32:** `peerwire` builds for ESP-IDF; a prototype runs on emulated ESP32-C3s.
- **Shared documents:** `peerwire-sync` gives shared documents in Rust, wire-compatible with `room.doc()`, in the
  Godot binding (`open_doc`) and the C ABI.
- **Harder NATs:**
  - birthday punching and lockstep prediction, so 59 of 64 simulated NAT pairings connect directly;
  - dual-stack IPv4 and IPv6 candidates;
  - relay peers: an always-on `peerwire-peer` kept linked and used when a punch fails.
- **Faster paths:** a link races its other paths and takes one that's clearly faster.
- **Bulk transfer:** a loss at the end of a burst is repaired as a tail loss (RACK-TLP), not a timeout: 22% faster
  at 1% loss and 100 ms. The receive window is 4096 fragments, so `reliable_window` can go up to 4096 on long, fast
  links; a peer can now make a link hold about 5.7 MB (2.3 MB before).
- **Security:**
  - room hellos are sealed as in JavaScript;
  - handshake cookies under load, and a per-address cap on half-open handshakes;
  - an HS1 must be exactly the right length;
  - incoming offers spend a budget before they're decrypted.
- **Godot:** a hub catches up a stalled member afresh instead of disconnecting it; getters for each link's route and
  address.

## `@peerwire/js` 0.2.1 (2026-11-03)

**Fixed**

- `persist`: a save on a connection the browser has just closed, as it does while it clears the site's data, opens
  the store again, once, instead of failing.
- `persist`: an open the browser aborts is tried once more a moment later; an open it refuses still fails at once.

## `@peerwire/js` 0.2.0 (2026-11-01)

**Added**

- **Shared documents:** `room.doc(name)` gives maps, sets and logs that every member holds and that converge.
  - Late joiners catch up, in pieces sized for slow routes.
  - `persist` keeps an encrypted copy on the device; a failing store backs off up to 64 s.
  - `relays: true` keeps a sealed copy on the Nostr relays for members who join while nobody else is online.
- **Calls:** `room.call()` gives audio and video between room members, as a mesh of up to 16 (6 by default),
  signalled over the room's own links.
- **Offline messages:** `openInbox()` sends messages to one PeerId, sealed and left on Nostr relays until it comes
  online.
- **Fallback routes** for peers WebRTC can't link:
  - through another member linked to both, then over the signaling relays themselves (`fallback`, `room.route(peer)`);
  - the room keeps retrying WebRTC while a link runs on a fallback (`room.retryDirect()`);
  - it moves to a member that links to both as soon as one does.
- **Harder NATs in the browser:** ICE restarts, port prediction and birthday punching.
- **Demos:** Net Lab (what your network allows, link by link), a chat with invite links and QR codes, and a meeting
  room of up to 6.

**Changed**

- Room hellos are sealed with AES-256-GCM under a key from the room name, so a relay can't read the PeerIds in a room.
- Default Nostr relays are now `wss://nos.lol`, `wss://relay.primal.net` and `wss://nostr.mom`.
- Nostr signaling:
  - survives a network switch;
  - paces its sends and spreads batches across relays;
  - redials a relay that goes silent, backing off after repeated failures.
- `loadPeerwire` fetches the wasm by URL in browsers, so bundlers' asset URLs (Vite's `?url`) work.

**Fixed**

- A message a `peer-join` handler sends to the newcomer now reaches it.
- `room.send` throws `RangeError` for a message over its lane's limit, even while the outbox is full.
- `'public-only'` strips local addresses from every SDP form browsers produce.
- Many hardening fixes from security reviews, among them bounds on what members can make a peer store or compute.

## `@peerwire/react` 0.1.1 (2026-11-01)

- A shared write that doesn't fit what members can store is dropped with a warning instead of throwing.
- Catch-up messages are cut to 512 KiB, so they always fit a reliable message.
- A member's value nested more than 64 levels deep is dropped before it's stringified.
- `usePeerwireRoom` fails the join with an error if the presence changes while it's joining.

## `@peerwire/react` 0.1.0 (2026-10-24)

First release. React hooks over `@peerwire/js`, in TypeScript: `RoomProvider`, `useRoom`, `usePeers`, `usePresence`,
`useMessages`, `useRoomEvent`, `useSharedState`, `usePeerState` and `usePeerwireRoom`.

## `@peerwire/js` 0.1.1 (2026-10-23)

**Fixed**

- A room's presence waits for the link's own room proof, so a join under load is no longer one-sided (one member
  listing the other but not the reverse).

## `@peerwire/js` 0.1.0 (2026-10-21)

First release.

- The Peerwire core as a ~320 KB WebAssembly module with no imports.
- `joinRoom`:
  - WebRTC DataChannels, with Nostr or BroadcastChannel signaling;
  - presence, and the reliable, unreliable and urgent lanes;
  - closed rooms proved with a key from the room name;
  - a pull-style API for game engines.
- Privacy modes for the transport: `'direct'`, `'public-only'` and `'relay-only'`.
- The lower layers: `loadPeerwire`, `WebRtcTransport` and `NostrSignaling`.
- TypeScript types.
