# Threat model

Scope of this document: `peerwire-core` (handshake, wire format, lanes, relay, NAT
traversal, STUN, invites, sealed messages), the C ABI (`peerwire-ffi`), the
JavaScript glue and transport (`packages/js`), the React hooks (`packages/react`),
and the Nostr / BroadcastChannel signaling adapters. It records what Peerwire
defends against, what it does not, and the findings of the first security review
(M5, NB-013) and of every review since.

Status: pre-1.0, no external audit. See [SECURITY.md](../SECURITY.md).

## What Peerwire protects

1. **Peer authentication.** A `PeerId` is an Ed25519 public key. A link is
   `Noise_IK_25519_ChaChaPoly_BLAKE2s`, whose static key is the X25519 form of
   that identity, and the initiator sends its Ed25519 PeerId inside HS1 bound to
   the handshake transcript. A completed handshake proves the remote holds the
   secret for the PeerId you dialled. No signaling relay, peer relay or NAT in
   the path can impersonate a peer or man-in-the-middle a link.
2. **Link confidentiality and integrity.** After the handshake, every data
   packet is AEAD-sealed (ChaCha20-Poly1305) under the session keys. A relay
   carrying a `Via` link sees only ciphertext.
3. **Replay of data packets.** Each link has a 2048-wide sliding nonce window
   (WireGuard-style); replayed or too-old data packets are dropped before
   decryption is trusted.
4. **Signaling payload secrecy.** Offers (which carry IP candidates) are sent as
   sealed messages (`Noise_X`): encrypted to the recipient PeerId and
   authenticating the sender's PeerId. A tampered or forged offer fails to open.

## Trust boundaries

| Boundary | Trusted? | Notes |
|---|---|---|
| Signaling relay (Nostr, BroadcastChannel) | No | May read, drop, reorder, replay or inject any signaling message. Cannot forge a sealed payload or impersonate a PeerId. |
| Peer relay (`FWD`/`RELAYED`) | No | Sees only ciphertext; rate-limited; forwards only to already-connected peers. |
| NAT / on-path network | No | Noise authenticates the endpoints; cannot MITM. Can drop/delay. |
| Remote peer | Partially | Authenticated, but may be malicious: send malformed frames, flood, probe. Parsers and resource caps are the defence. |
| Host application / C caller | Yes | The C ABI trusts its caller to pass valid pointers and lengths (see SEC-8). |

## Attacker capabilities considered

- Off-path attacker who can spoof source addresses but not observe replies.
- On-path attacker who observes and modifies datagrams.
- A malicious or curious signaling relay, or any party who learns a room name or
  a target PeerId.
- A connected, authenticated peer behaving maliciously.

Out of scope: compromise of the host OS or its CSPRNG; side channels in the
underlying crypto crates (`snow`, `ed25519-dalek`, `k256`); traffic analysis by a
global passive adversary; malicious application code above the API.

---

## Findings

Severity uses likelihood × impact for this software's threat model. Each finding
that needs a change is tracked as an NB card.

**Status as of main `41cc24a` (2026-10-26); the shared-doc rows (DOC-*) as of main
`f40cffa` (2026-10-28).**

| Finding | Status |
|---|---|
| SEC-1 (High) | **Fixed** in NB-021 (`ccc3b79`), with its review findings N21-1 and N21-2 |
| SEC-2 (Medium) | **Fixed in JS** (NB-022): hello body encrypted to the room; open in Rust/Godot (NB-022b, before a native or Godot release) |
| SEC-3 (Medium) | Fixed for rooms (NB-031 Rust, NB-037 JS); open for a bare `WebRtcTransport` |
| SEC-4…SEC-8 | Open (NB-024…NB-028) |
| SEC-9…SEC-11 | Met by NB-020/NB-030 and `docs/GODOT.md` (Security); NB-029 audit and N29-1 fix |
| SEC-12 (Info) | Accepted, no card |
| N38-1…N38-3 (port mapping) | **Fixed** in NB-038 (`71d83b2`) and NB-041 (`0dfdbc9`) |
| NPM-1 (Low) | **Fixed** by `4cffbf9` (merged `14f463a`): no port mapping under `RelayOnly` |
| NPM-2 (Low) | **Fixed** in `docs/GODOT.md` (`9be92d5`); the flood risk itself is SEC-4 |
| NPM-3 (Low) | **Fixed** by `6f947f3`: a map request in flight at stop gets one delete |
| NB44-1 (Medium) | **Fixed** by `40e42db` (per-IP punch cap), re-checked 2026-10-23 ([below](#hard-nat-punching-nb-044-nb-045-nb-046-nb-048--nb44-1nb48-2)) |
| NB48-1 (Medium) | **Fixed** by `d1b2d25` (fleet checks one public address; Net Lab consent and cooldown), re-checked 2026-10-23 |
| NB48-2 (Low) | **Fixed** by `d1b2d25` (public spray IP) and `695b38b` (NB48b-3); the IP isn't matched to the peer's signalling (accepted) |
| NB44-2, NB44-3 (Low) | **Fixed** by `a16072e` (NB-044b: authenticated routes, fd reserve), re-checked 2026-10-23; residuals NB44c-1, NB44c-2 |
| NB48b-2 (Low) | **Fixed** by `de9efea` (the busy slot is held per peer), re-checked 2026-10-24 |
| NB48b-1, NB48b-3 (Low) | **Fixed** by `de9efea` (the answer is stripped) and `695b38b` (Chrome's line model, checked after apply), re-checked 2026-10-24 |
| NB48b-4, NB48b-7 (Low) | **Fixed** by `4b6c0f7`, `46a5da3` and `4c37bb7` (relay-only dials through a relay stay relayed, also when switched on mid-dial), re-checked 2026-10-24 |
| NB44c-1, NB44c-2, NB46-1 (Low) | Open, same section |
| NB45-I1, NB44-I2, NB46-I1, NB44b-I1 (Info) | Optional hardening or accepted, same section; NB44-I2 folds into NB44c-2 |
| NB48b-5, NB48b-6 (Info) | **Fixed** by `08425b8` (a stale link's close keeps the peer) and `2a00ecb` (CARRY replies bounded), re-checked 2026-10-24 |
| MIC-1, MIC-3 (Low) | **Fixed** by `d78d046`, `877d02c` and `6303bd8` (Net Lab's global room and punch offers; `public-only` in `@peerwire/js` 0.1.1 and earlier), reviewed 2026-10-24; residual MIC-4 ([below](#bounds-and-residuals-from-reviews-nb-014nb-043)) |
| MIC-2 (Info) | **Fixed** by `9282d36` (a punch that ended closes what it opened) and `691dec0` (one offer and one answer per punch, which also closes a Low NB48-1 bypass), reviewed 2026-10-24 |
| MIC-4 (Low) | **Fixed** by `134fed3` (the button rejoins before it re-tests the network), reviewed 2026-10-24 |
| MIC-5 (Info) | Open, same section |
| FL1-1 (Info), FL1-2 (Low) | **Fixed** by `4b131ee` (at most one new connection per peer per 2 s from retry offers; only the dialling side acts on a `retry` ask), reviewed 2026-10-24, [WebRTC retries](#webrtc-retries-fast-lane-1--fl1-1-fl1-2) |
| FL2-1 (Low), FL2-2 (Low) | **Fixed** by `63e5b72` and `ddae245`, merged `8314aa8` (a member relay that stalls data hands the link back to the pipe and is held off; a datagram names the peer whose channel it came on), reviewed 2026-10-24; residuals R1, R2, [Member relays for pipe links](#member-relays-for-pipe-links-fast-lane-2--fl2-1-fl2-2) |
| FL2F-1 (Info) | Open, same section |
| RH-2 (Medium), RH-1, RH-3, RH-4 (Low) | **Fixed** by `78c4ebe` and `8126119`, merged `dec1554`, before `@peerwire/react` 0.1.0 was published (deep `accept` checks required, per-key clocks bounded by wall time, per-member quotas, catch-up by digest), reviewed 2026-10-24, [`@peerwire/react` hooks](#peerwirereact-hooks-nb-049--rh-1rh-6) |
| RH-5 (Low), catch-up message size (Low), RH-6 (Info), N1 (nit) | **Fixed** by `12974c4`, merged `41cc24a`, in `@peerwire/react` 0.1.1 (a write members' data leaves no room for is dropped with a warning; catch-up messages capped at 512 KiB; versions kept instead of entries; a newer version an asker holds isn't sent), reviewed 2026-10-26, same section |
| OUT-1 (Low) | Open, `@peerwire/js` 0.2.0 and main, [JS endpoint and rooms](#bounds-and-residuals-from-reviews-nb-014nb-043) |
| RX-1 (Low), RX-2 (Info) | **Fixed** by `b0d354b` (the README's React example keeps string messages only, and only the last 100), reviewed 2026-10-24, [Demos](#bounds-and-residuals-from-reviews-nb-014nb-043) |
| DOC-1 (Medium), DOC-2, DOC-3 (Low) | **Fixed** by `979cdae`, `eaa2a22` and `a4fddfd`, with DOC-2a and DOC-3a by `d6b148c`, merged `c30ab0f`, before `room.doc()` was published (stamps taken however far ahead; no entry on our own writer that we didn't write; a frame costs about one pass over what it names), reviewed 2026-10-27, [Shared docs](#shared-docs-roomdoc-nb-006--doc-1doc-5) |
| DOC-2b (Low) | **Fixed** by `8b7946c`, merged `45c7cfd` (a catch-up's deletions passed on one range per range of it, charged to the sender's budget), reviewed 2026-10-27; residuals DOC-2c and I6, same section |
| DOC-4, DOC-5 (Low) | **Fixed** by `f40cffa` (the merge of `ff51ce6`): a member over signalling is paused for a better routed one once per answer; a room with no doc open drops deflated doc frames unread. Reviewed 2026-10-28; residual I8, same section |
| Earlier reviews' bounds and residuals | Recorded in [Bounds from reviews](#bounds-and-residuals-from-reviews-nb-014nb-043) |

### SEC-1 — Unvalidated peer-id length corrupts the wasm heap (High) — NB-021

**Status: fixed** in NB-021 (merged `ccc3b79`). Every fixed-size copy into wasm
memory checks its length first (`peerBytes`, `exact`), every variable-size one
takes bytes only (`bytesOf`), and the signaling adapters (`nostr.js`, `webrtc.js`,
`local.js`, `room.js`) drop a `from` that is not 64 lowercase hex characters. The
reproduction below now throws a `TypeError` and the module stays healthy
(`packages/js/test/peer_id.mjs`, in the gate). The C ABI peer-length argument is
a separate defence-in-depth card.

`packages/js/peerwire.js` `Endpoint.setPeer(id)` copies `id` into the fixed 32-byte
`peerBuf` with `mem().set(..., this.peerBuf)` and performs no length check;
`seal()` and `open()`/`put()` have the same shape. An over-long id writes past the
buffer into adjacent wasm linear memory, corrupting other endpoint state and the
allocator; the module then traps (`unreachable`) or behaves unpredictably.

This is reachable from untrusted input: the demos and `webrtc.js` pass a
signaling peer identifier (`from` of a Nostr/BroadcastChannel `hello`, or a `to`
for `seal`) straight into `dial()` → `seal()` → `setPeer()`. A hostile relay or
peer that publishes a `hello` with an over-long `from` can drive the overflow in
any client that auto-dials newcomers (as `demo/demo.js` and `demo/arena.js` do).

**Reproduction.** Load the wasm, call `ep.seal('ab'.repeat(4096), ...)`: the copy
runs past `peerBuf` and a subsequent `nb.endpoint()` allocation traps the module.

**Fix.** Validate length at the boundary before every wasm copy:
- `setPeer`/`seal`/`open`: require a 32-byte id (or a 64-hex-char string) and
  reject otherwise;
- in `nostr.js`/`webrtc.js`, validate that a signaling `from`/`to` is 64 lowercase
  hex chars before using it as a PeerId.
Defence in depth: the C ABI could take an explicit peer-length argument, but the
JS-side check is the necessary fix.

The review of the fix found two more unbounded inputs, both fixed in the same
merge:

- **N21-1 (Medium) — fixed.** `webrtc.js` passed DataChannel messages to
  `new Uint8Array(e.data)`. `binaryType = 'arraybuffer'` covers binary messages
  only, so a peer could send a *string* such as `"2147483648"`, which became a
  length: one 10-byte message made the tab allocate 2 GB and aborted the module
  in `pw_alloc` (`"100000000"` grew wasm memory to ~101 MB, never returned). It
  needed only an open DataChannel, before the Noise handshake. Now `onmessage`
  accepts only an `ArrayBuffer` of at most `MAX_DATAGRAM` (1232) bytes, and
  `Endpoint.receive` drops anything larger before allocating, which also covers
  `local.js` and custom transports.
- **N21-2 (Low) — fixed.** `nostr.js` decoded and opened event content of any
  size, so a hostile relay could push multi-MB events, each costing about twice
  its size in wasm memory. Content that is not a string or is over 64 KiB is now
  skipped before decoding.

### SEC-2 — Room `hello` discloses the long-term PeerId in cleartext (Medium) — NB-022

`nostr.js` `hello()` publishes `{ t:'hello', from:<full PeerId>, meta }` as plain
base64 under a tag derived only from the room name. Anyone who knows the room name
(the room tag is `sha256("peerwire/room/<room>")`, computable by anyone with the
name) can read every member's long-term Ed25519 PeerId, correlate it with the
ephemeral Nostr pubkey, the `created_at` timestamp and the relay set, and track
that identity across rooms and time. This defeats the "fresh key per session"
unlinkability the adapter otherwise aims for.

**Fix.** Don't broadcast the raw PeerId in a room-wide, unencrypted hello. Options,
in order of preference: (a) gossip presence only to peers already known
out-of-band and seal it; (b) include only an ephemeral, per-session handle in the
hello and exchange the real PeerId inside the sealed channel; (c) at minimum
document that room membership reveals every member's identity to anyone who knows
the room name, so room names must be treated as secrets.

**Status: fixed in JS (NB-022); open in Rust/Godot (NB-022b).** Two earlier steps
narrowed it: NB-031 (Rust) and NB-037 (JS) derive a room key, separate from the
topic, from the name, and a hello is acted on only with its HMAC proof, so someone
without the name can no longer get dialled. NB-022 then encrypts the JS hello body
(AES-GCM, key `sha256("peerwire/room-enc/<room>")`, random 12-byte nonce), so a
relay or kind-25059 subscriber without the room name can't read the PeerId. It
still sees that a hello was posted (room tag, fixed ~304-char body), the ephemeral
pubkey and `created_at`, and via the inbox tag (SEC-5) a stable per-PeerId handle.
Freshness comes from the NB-037 proof (`meta.ts`, 60 s): a replayed hello is
refused once it is older than that. The residual is inherent: a peer that holds the
room name (a legitimate member, or anyone who learns it) can read and forge hellos
and learns the PeerIds of everyone in the room, the same bar as joining. Option (c)
is also done: the docs say private room names need at least 128 random bits
(`randomRoomId()`), and a guessable name is a public lobby.

**Still open (NB-022b):** Rust/Godot hellos are still plaintext. Godot `join_room`
uses the Rust `NostrSignaling` with the default public relays, so every native or
Godot room member publishes its PeerId in the clear to any kind-25059 subscriber;
this must be fixed before a native or Godot release (NB22-1). JS and Rust currently
drop each other's hellos, but only because JS can't open a plaintext one: Rust marks
its hello `meta.nb` while `room.js` checks `meta.pw`, so once Rust encrypts, JS
would dial Rust peers unless the marker is reconciled (NB22-2).

### SEC-3 — Sealed-offer probe reveals IP candidates to anyone who knows your PeerId (Medium) — NB-023

Because a valid offer only requires the target's PeerId (a public value), any
party who learns a PeerId can seal an offer to it over a public relay and receive
a WebRTC *answer* — which contains the target's ICE candidates (local and
server-reflexive IP addresses). The Noise handshake still protects against
impersonation, but the metadata (presence + IP) leaks before any mutual
authentication of intent. Combined with SEC-2, a passive observer of a room can
deanonymize every member's IP.

**Fix.** Treat an inbound offer as a request, not a command: rate-limit answers
per source, and gate candidate disclosure on some proof the peer belongs in the
room (a room-scoped shared secret, or an allow-list the app supplies). Document
the residual "known PeerId ⇒ discoverable IP" exposure for apps that publish
PeerIds widely.

**Status: fixed for rooms, open elsewhere (NB-023).** In Rust (NB-031), room
offers are acted on only with a verified room-key MAC, and a plain offer only
from a peer we invited. In JS (NB-037), `joinRoom` gives `WebRtcTransport` an
`offerProof`, and an offer without a valid room proof gets no answer, so no
candidates. A `WebRtcTransport` built directly without `offerProof` still answers
any sealed offer, so the finding stays open for that use.

### SEC-4 — Half-open handshake table exhaustion (Medium) — NB-024

`Config::max_pending_handshakes` (default 4096) caps half-open responder links,
and the cap is checked before the expensive Noise build, so CPU per flood packet
is bounded once the table is full and memory is bounded (~a few MB, entries expire
after `handshake_timeout`, 10 s). However, there is no address-validation cookie:
an attacker sending ~4096 HS1s per 10 s with spoofed or varied `(sender_idx,
ephemeral)` keeps the table saturated, so legitimate inbound handshakes are
dropped for the duration — a connection-establishment denial of service against a
public/always-on peer. HS2 is roughly HS1-sized, so reflection amplification is
~1×.

**Fix (hardening).** Add a stateless retry/cookie step (QUIC Retry / WireGuard
cookie style): on the first HS1 from an unvalidated address, reply with a keyed
cookie and allocate no state until the address echoes it. This moves the cost to
address-spoofers and keeps the table for validated peers. Until then, operators of
always-on peers should size `max_pending_handshakes` and `handshake_timeout` for
their exposure.

### SEC-5 — Deterministic, unsalted Nostr peer tag enables presence correlation (Low–Medium) — NB-025

The per-peer tag is `sha256("peerwire/peer/<PeerId>")`, a fixed function of the
public PeerId. Anyone who knows a target's PeerId can compute the tag and
subscribe to it on the public relays to learn whenever that identity is addressed,
and to correlate activity across sessions and rooms. (SEC-2 hands out the PeerIds
to room-name holders, and to anyone in Rust/Godot rooms until NB-022b; even
without it, PeerIds are shared in invites.)

**Fix.** Derive the routing tag from a rotating, room- or epoch-scoped secret
rather than from the bare PeerId, so the tag is unlinkable to anyone who only
knows the public PeerId. Accept that two peers must share that secret (they share
a room already).

### SEC-6 — Sealed signaling messages have no replay protection (Low) — NB-026

`sealed::open` intentionally provides no anti-replay (documented in `sealed.rs`:
"callers that care put a timestamp or nonce inside"). `nostr.js` adds none beyond
a per-session `seen` set of event ids, which does not survive a reconnect and does
not stop a relay from re-delivering an old sealed offer with a fresh event id. An
observer can replay a captured sealed offer later; the effect is limited (it
triggers a redundant answer / ICE churn, feeding SEC-3) but non-zero. A re-sent
retry offer (`retry: true`, Fast lane 1) also closes the recipient's pending connection
to that peer, one that never opened, and so cancels an upgrade; the fallback link
carries on.

**Fix.** Put a `created_at`/nonce inside the sealed body and reject stale or
duplicate (sender, nonce) at the adapter. The Nostr `created_at` is unauthenticated
(it is outside the sealed blob) and must not be trusted for this.

### SEC-7 — No in-session rekeying / forward secrecy refresh (Low) — NB-027

A link keeps one set of transport keys for its whole lifetime (nonce is a u64
counter; exhaustion is not a practical concern). There is no periodic rekey, so
compromise of a live session's keys exposes all traffic on that session, past
(if recorded) and future, until it is torn down. A reconnect performs a fresh
handshake with new ephemeral keys, which bounds exposure in practice.

**Fix (future).** Offer an optional rekey (new ephemeral DH) on a byte/time
threshold for long-lived sessions. Document the current property meanwhile.

### SEC-8 — C ABI trusts caller pointers and lengths (Low, by contract) — NB-028

Several `pw_*` functions dereference the endpoint handle and in/out buffers
without null or length checks (e.g. `pw_secret`, `pw_send`, `pw_stats`), and
`bytes(ptr, len)` with a null `ptr` and non-zero `len` is undefined behaviour.
This is the documented C-ABI contract and the JS glue always upholds it, but a
native (Dart/Swift/C) caller that violates it gets memory unsafety rather than an
error code.

**Fix.** Document the contract explicitly in the header/readme, and add null
checks on the handle and on buffers where `len > 0` as cheap defence in depth.

### SEC-12 — An invite holder can confirm a guessed link by naming it as a relay (Info) — no card

Since NB-033 an invite holder the node never dialled learns none of its linked
PeerIds. But relay fallback dials a waiting sender `Via` every relay it listed
that the node is linked to, when the offer arrives (`on_offer`) or when that link
comes up later (`handle_event`, `Connected`). So a holder can list a guessed
PeerId X as a relay: if a relayed handshake from the node arrives through X, the
node is linked to X.

This predates NB-033 (it only extends the check to links that come up later) and
is inherent to relay fallback. It is bounded: at most 8 guesses (`MAX_RELAYS`) per
offer acted on, under the offer rate limits; each guess answers only "linked to X
or not" and enumerates nothing; and X must actually relay to the holder, so X has
to be linked to it. No fix planned; a stronger design (naming only links that
agreed to relay) would be a separate card.

### Router port mapping (NB-038, Godot switch) — N38-1…N38-3, NPM-1…NPM-3

`Builder::port_mapping(true)` (Godot: `set_port_mapping(true)`), off by default,
asks the LAN gateway (PCP, then NAT-PMP, then UPnP-IGD) to forward the node's UDP
port. The mapped address is offered like a public one. This changes the attack
surface in three ways:

- **Unsolicited reachability.** While mapped, anyone on the internet can send to
  the port, with no NAT filtering in front. It reaches only the Noise endpoint,
  but that endpoint has no handshake-flood cookies yet, so SEC-4 applies directly.
  It also shows up in port scans. `docs/GODOT.md` says this and advises asking
  the player first (NPM-2).
- **The gateway and the LAN are trusted for the address only.** Any device on the
  LAN can answer the requests. A rogue responder can report a false address,
  which peers would then dial. It can't get into a session: no one finishes a
  handshake without the peer's key. Accepted residual.
- **Leftover mappings.** After a crash the router keeps the mapping until its
  lease ends (2 h), or until someone deletes it if the router only allows
  permanent UPnP leases. Documented.

Findings:

- **N38-1 (fixed, NB-038 `5b52eb5`).** A dripping or stalled UPnP gateway could
  hold the mapping thread and the delete on drop. Now each HTTP exchange has one
  3 s deadline, and drop waits at most 7 s.
- **N38-2 (fixed, NB-038 `5b52eb5`).** A PCP or NAT-PMP answer with lifetime 0
  was taken as a permanent mapping; it is now a refusal.
- **N38-3 (fixed, NB-041 `bf769e9`).** A UPnP chain stopped half-way kept
  sending; the worker now checks stop before each HTTP exchange.
- **NPM-1 (Low, fixed by `4cffbf9`, merged `14f463a`).** Under `Privacy::RelayOnly` the mapped
  address is never given out, yet the port was still opened. Now a relay-only
  node asks the router nothing (state `RelayOnly`). Switching to relay-only
  deletes the mapping without blocking, and switching out maps again after the
  old delete has gone out. A Godot relay-only host still maps, because it gives
  out its public addresses as under `PublicOnly`.
- **NPM-2 (Low, fixed in `docs/GODOT.md`, `9be92d5`).** The doc said the port
  "reaches only the Noise endpoint" without the SEC-4 flood caveat.
- **NPM-3 (Low, fixed by `6f947f3`).** `Mapper::stop()` deleted only a lease it
  held. A PCP or NAT-PMP map request still unanswered when the worker stopped
  (drop right after bind, or `set_privacy(RelayOnly)` right after a map request)
  got no delete, so the router's late grant stayed open for its lease (2 h). Now
  a stop with no lease and a map request out sends one lifetime-0 delete: PCP
  with the same nonce and no suggested address (RFC 6887 §15), NAT-PMP with
  external port 0 (RFC 6886 §3.4). With a lease held, only `release()` deletes.

### Hard-NAT punching (NB-044, NB-045, NB-046, NB-048) — NB44-1…NB48-2

Reviewed on 2026-10-23: native `backend/NB-045-lockstep-prediction` (`3a6c8b8`,
NB-044 birthday punch and NB-045 lockstep) and browser
`ui/NB-048-browser-birthday` (`5ee351e`, NB-046 ICE restart and port prediction,
NB-047 Net Lab rooms, NB-048 browser birthday punch). Mechanisms and costs are in
`docs/NAT-TRAVERSAL.md` (Birthday punching, Lockstep prediction, Reflection).

These features make a node send many packets to an address a remote party names.
The security question is who can name that address and how much traffic follows.

| Trigger | Who can trigger it | Traffic per try | Limits |
|---|---|---|---|
| Native spray (an offer saying `Symmetric` with no prediction, to a cone node) | A room member, an invite holder, or a peer the node dialled | Up to 2048 HS1s of 133 B to ports ≥ 1024 of the offer's first address (public IPv4): 272,916 B over 3.8 s | 5 min per peer; per node, a burst of 4, then 1 per 30 s |
| Native open (an offer saying `Cone`, to a random-port symmetric node) | Same | 256 sockets for 10 s, each sending 1 HS1 to the offer's address (about 34 KB) | Same budget; at most 512 extra sockets open at once |
| Native lockstep sweep (both sides predict) | Same | Up to 96 HS1s (about 13 KB) to predicted ports of the offer's IP, and up to 96 of the node's own NAT ports | Same budget |
| Browser port guesses (NB-046) | A linked or dialling WebRTC peer | 32 extra remote candidates per new `{ ip, next, step }` | Chrome's ICE pacing (about 16 checks/s per connection) |
| Browser fleet and spray (NB-048, Net Lab only) | Any member of the Net Lab room, including the open global room | Fleet: 256 connections checking every candidate in the peer's offer. Spray: 1024 checks to the IP the peer names | 70 s per run, no cooldown |

The core stayed sans-IO and deterministic: `Endpoint::spray`, `sweep` and `opener`
use the endpoint's seeded RNG and the `now` the host passes in. Sockets, the 5 ms
poll and routing live in `peerwire::Node`. `fuzz.rs` checks that a spray visits each
port at most once and respects `MAX_SPRAY`.

**NB44-1 — The birthday spray is a reflection vector with about 270× byte
amplification (Medium).** A sealed offer costs the sender well under 1 KB. When the
node acts on it, the node sends 272,916 B to ports of an IP the sender chose. The IP
only has to be public IPv4 and dialable, and it isn't checked against anything the
peer proved. The 5-minute cooldown is per PeerId, and a room-key holder can make up
any number of PeerIds, so only the node budget limits it: 4 sprays, all at one IP if
the sender wishes.

- Repro (unit test on the branch, `punch_case`): 4 offers from 4 PeerIds naming
  `198.51.100.9` make one cone node send 8,208 datagrams (1,091,664 B) to that IP
  within 6 s; 8 PeerIds send no more (budget).
- Sustained, the limit is about 9 KB/s per node, towards any victims. Every cone
  member of a public room can be recruited, one offer each.
- Fix: a cooldown per destination IP across all PeerIds (for example, one planned
  punch per IP at a time and at most 2 per 5 min), so all four sprays can't land on
  one victim. Correct the "no amplification beyond the count" line in
  NAT-TRAVERSAL.md. Test: 4 PeerIds offering one IP start 1 spray.
- **Status: fixed** by `40e42db` (NB-044b). `start_plans` allows one punch per
  destination IP at a time (20 s) and at most 2 per 5 min across all PeerIds. The
  check runs before the node token, and covers spray, sweep and open. Re-check probe:
  4 or 30 PeerIds offering one IP every 1 s or every 20 s for 15 min. Per 5-min
  window, that IP got at most 4,096 spray probes (2 sprays, 545,832 B) from a cone
  node, or 2 fleets (512 HS1s) from a symmetric one. Before the fix it was about 14
  sprays (3.8 MB) per 5 min.
- Residual: the cap is per node. The first spray is still about 270× the offer, and
  N room members recruited with one offer each still reflect N times the
  per-node bound at one victim. Recruiting needs the room key or an invite.

**NB44b-I1 — The per-IP cap can be spent by a room member (Info, accepted).** A
member can name a real peer's public IP in offers from minted PeerIds, which uses
up that IP's 2 slots. Birthday punches towards it then stop for 5 min and fall back
to the relay. Two genuine peers behind one carrier-NAT IP get at most 2 punches per
5 min between them. A plan the cap blocks is dropped, not deferred, until that
peer's next offer. These are availability costs only, and the cap is worth them.

**NB44-2 — Extra sockets take any sender as a route, before authentication
(Low).** `Node::read_extra` makes the first extra socket that hears from an address
that address's route, and refreshes the socket's `last_recv` on every datagram. It
does this before `Endpoint::handle_datagram` has authenticated anything.

- Repro (node test on the branch): junk from about 57,000 loopback source ports to
  one extra socket left 24,496 entries in `routes`. The socket stayed open 1.2 s past
  `until`, and stays open for as long as junk keeps arriving.
- Who can reach an extra socket:
  - the offer's address, through a port-restricted NAT;
  - any host on the LAN;
  - the internet, if the node is bound dual-stack on a host with global IPv6 and
    no inbound firewall (the ports can be found by a UDP scan).
- A sender that can spoof the source address of a peer the node already talks to
  can move the node's egress to that peer onto the extra socket, which breaks the
  path through the peer's NAT.
- The punch target alone gets an HS1 from every socket's NAT mapping. One datagram
  per socket per minute (under `ROUTE_IDLE`) then keeps all of them open
  indefinitely: up to 512 fds (NB44-3) and the 5 ms poll (NB44-I2) for as long as it
  likes. This is why NB44-2 should land before native builds ship.
- Fix: set a route only for an address the endpoint has accepted, that is a session
  path or dial target equal to `from` after `handle_datagram`. Refresh `last_recv`
  only from that address, and cap `routes` at `MAX_EXTRA_SOCKETS`. Test: junk from N
  ports leaves `routes` empty, and the sockets close at `until`.
- **Status: fixed** by `a16072e` (NB-044b). `read_extra` adopts a route, and refreshes
  `last_recv`, only when `Endpoint::handle_datagram_direct` reports that the datagram
  reached a session whose direct path is `from`; `routes` is capped at `MAX_ROUTES`
  (512). Re-check: junk, a bad-tag Data packet, or anything from another address
  makes no route (test `birthday_sockets_ignore_unauthenticated_floods`), and a
  punched link still adopts its route. Residuals: NB44c-1 and NB44c-2 below.

**NB44c-1 — Two unauthenticated packets still count as "authenticated on this
path" (Low).** `on_packet` returns the session index from two early returns that
run no crypto: an HS1 matching `half_open` by (sender, ephemeral), and an HS2 for a
link that is already live. So `handle_datagram_direct` reports true for either one
when it comes from the peer's current direct address.

- Repro (two in-memory endpoints, linked): an HS2 of `0x12`, any sender, our receiver
  index and 48 junk bytes, from the peer's address, gives true, and `rx_auth_failed`
  does not move. Replaying the peer's HS1 from its address while the link is
  half-open also gives true. The wrong index or another address gives false.
- It needs the peer's exact address and either our receiver index (in clear in every
  Data packet to us) or a captured HS1: an on-path observer, or a spoofer who has
  seen one packet. If such a packet reaches an extra socket, the peer's address
  becomes a route there (our egress to it moves) and the socket is held another
  `ROUTE_IDLE` each time.
- Fix: report true only when this datagram passed crypto (set a flag after
  `read_message` in `on_hs1`, `on_hs2` and `on_data`; the duplicate returns leave it
  unset). Test: both packets above give false.

**NB44c-2 — Any linked identity can hold extra sockets open (Low).** `open_extra`
does not record `Birthday.peer`, so a valid handshake from any identity that knows
our PeerId and can reach an extra socket makes a route, and the socket stays open
while that session sends once a minute. `on_data` also moves a session's path on
every authenticated packet from a new source port, so one linked peer cycling
through N source ports holds N sockets (up to 512) and the 5 ms poll (NB44-I2).

- Repro (loopback node test): node A opens 4 extra sockets for a punch to T, and an
  unrelated node C links to one. One route, and the socket is still open after
  `until`.
- The NB44-3 reserve keeps the process 64 fds clear, so the cost is held fds and
  wake-ups, not `EMFILE`.
- Fix: keep the punch's peer in `Extra`, have `handle_datagram_direct` return the
  authenticated session's `PeerId`, adopt a route only for that peer, and keep one
  route per peer. Fold NB44-I2 in: drop the 5 ms poll once every open extra socket
  is a route. Card NB-044c.

**NB44-3 — Local resources a remote peer can spend (Low).**
- **File descriptors.** An offer saying `Cone` makes a random-port symmetric node
  open 256 sockets for 10 s, up to 512 at once. The default soft `RLIMIT_NOFILE` is
  256 for macOS apps (the launchd default) and 1024 on Linux desktops.
  `open_extra` stops at the first failed bind, so the process can sit at its limit
  for 10 s: other opens in the host
  fail with `EMFILE` (a Godot resource load, the Nostr websocket reconnect), and the
  punch silently opens fewer sockets.
- **Ports.** Each try takes 256 ports on a carrier NAT, and many carrier NATs give a
  subscriber only 512 to 2048. The NAT keeps them for its UDP timeout after the
  sockets close.
- **Connection tracking.** A spray adds up to 2048 entries to the cone side's
  router (already in NAT-TRAVERSAL.md).
- Fix:
  - On the first failed bind, close a reserve (for example 64 of the sockets just
    opened).
  - Run one open at a time (256).
  - Give open plans their own budget of 1 per 2 min, longer than common UDP mapping
    timeouts.
  - The NB44-1 per-IP cooldown covers repeats.
  - Document in GODOT.md that macOS hosts (and mobile ones, check the limit) should
    raise `RLIMIT_NOFILE`.
- Found by reading the code; no probe was run.
- **Status: fixed** (file descriptors) by `a16072e` (NB-044b). A failed bind closes up
  to `FD_RESERVE` (64) of the sockets that punch opened, and GODOT.md has the
  `RLIMIT_NOFILE` note. Re-check under `ulimit -n 256`: a punch opened 188 sockets
  and left 64 fds free, a second punch at the same time still left 64, the main
  socket kept working, and every extra socket closed at `until` with nothing leaked
  (the same at 128: 60 opened, 64 free). At 256 a punch gets fewer sockets, so it
  may not link. Ports and connection tracking stay bounded by `MAX_EXTRA_SOCKETS`,
  the NB44-1 per-IP cap and the node's punch bucket (accepted).

**NB45-I1 — Sweep ports can be below 1024 (Info).** The sender's prediction (`next`,
`step`) chooses the ports of a lockstep sweep and of port guesses. Unlike the spray,
they aren't kept at 1024 and up, so 96 HS1s can reach well-known ports (53, 123,
161) of the offered IP. An HS1 is not a valid request for those protocols, so
nothing is amplified. Fix (optional): filter ports below 1024 as the spray does.

**NB44-I2 — The 5 ms wake-up lasts as long as a punched link (Info, not
security).** `Node::next_timeout` is capped at 5 ms while any extra socket is open,
and a route socket stays open while its link lives, so a link made by a birthday
punch keeps the host waking 200 times a second. This costs battery on phones.
Suggested: wait on the extra sockets' readiness, or poll less often once only routes
are left.

**NB48-1 — Net Lab: any room member can make a visitor's browser run a 256-connection
fleet against addresses of the member's choosing, with no click, repeatedly
(Medium).** The trigger is two messages: `{t:'bday', step:'start',
yourRole:'hard'}`, then `{t:'bday', step:'offer', sdp}`. With them, `onBirthday`
generates a certificate and calls `openFleet` with `FLEET_SIZE` (256), applying the
attacker's offer, candidates included, to every connection. Every connection then
runs ICE checks to every candidate in that offer.

- Repro (`packages/js/test/sec_fleet_probe.mjs`, Chromium, loopback; scratch only):
  an offer with 20 injected candidates drew 16 STUN checks/s from 1 connection,
  515/s from 32 and 2,095/s from 128. That scales linearly, so about 4,000/s from
  256, for the 70 s timeout. The candidates can name any IP, including the
  visitor's LAN.
- There is no cooldown: `step:'abort'` closes the fleet, and a new `start` opens
  another at once. Each round also costs the visitor 256 connections and a key
  generation, which is heavy on a phone.
- The global room is open to anyone who loads Net Lab.
- Fix:
  - Library: `openFleet` strips the offer's candidates and takes one validated
    public IPv4 address for the easy side, added to each connection with
    `addIceCandidate`, so the fleet checks one address, as the native open does.
    Cap `size` at `FLEET_SIZE`.
  - Net Lab: a remote `start` runs only after a local click. One fleet at a time,
    one per peer per 5 min, and `abort` doesn't reset the wait.
  - Test: an offer with N candidates leaves at most one remote candidate on each
    fleet connection.
- **Status: fixed** by `d1b2d25` (NB-048b).
  - `openFleet` strips the offer's candidates, caps `size` at 256, and adds the
    easy side's `addr` only if it is public IPv4.
  - A remote `start` only sets `invited`; `tryBirthday` runs on a local click only.
  - The cooldown is one punch per peer per 5 min, kept in merged state so a remote
    `start` can't reset it.
- Re-check probe (Chromium, loopback, scratch):

  | Hostile offer | Result |
  |---|---|
  | 20 injected candidates plus `c=`/`m=` at a listener | 0 remote candidates and 0 checks per connection |
  | A last candidate line without its CRLF | Chrome rejects the SDP |
  | `a=CANDIDATE:` in upper case | Not parsed as a candidate |

- Residual (accepted): once the visitor accepts, a peer still chooses the one public
  `ip:port` the fleet checks. Measured with one candidate per connection: 474
  checks/s from 32 connections, 1,912/s from 128 and 2,705/s from 256; 39,288
  checks in 30 s from 256. That is one click per peer per 5 min, public IPv4 only.

**NB48b-1 — The easy side still checks the candidates in the peer's answer (Low).**
`openSpray().applyAnswer` passes the answer SDP to `setRemoteDescription` without
`stripCandidates`, so a peer playing the hard side can list any addresses, LAN and
loopback included.
- Repro (re-check probe): an answer with 20 loopback candidates gave the easy
  connection 20 remote candidates. It sent 127 checks in 8 s and hit all 20 ports.
- This needs the visitor's click to accept, and it is one connection.
- Fix: `setRemoteDescription({ type: 'answer', sdp: stripCandidates(answerSdp) })`.
  Our own fleet's answer carries no candidates, so nothing else changes.
- Test: an answer with N candidates leaves only the spray candidates.
- **Status: fix applied** by `de9efea`, re-checked 2026-10-24. Answers with CRLF,
  LF-only, upper-case or tab-separated candidates drew 0 remote candidates and 0
  checks in Chromium. The strip itself could be bypassed, though: NB48b-3, fixed by
  `695b38b`.

**NB48b-2 — "One fleet at a time" is cleared by its own refusal (Low).**
- The bug: `closeBday` always sets `bdayBusy = false`. Both the busy refusal and the
  cooldown refusal in `tryBirthday` go through `bdayFail` → `closeBday`, so refusing
  a second punch frees the slot, and the next click starts it.
- Repro (Net Lab, three tabs, scratch): `punch(0)`, then `punch(1)` twice, left two
  punches running at once in one tab. Every two clicks on another peer add one more.
- Fix: track the busy peer (`bdayPeer`) and clear it only in that peer's
  `closeBday`. Or reset it only when a `ctx` was closed.
- Test: the third click leaves one punch running.
- **Status: fixed** by `de9efea`, re-checked 2026-10-24. `bdayBusy` holds the peer
  whose punch has the slot, and only that peer's `closeBday` frees it. Every way a
  punch ends reaches that call: the timer, `abort`, an answer with no public IP, an
  error, a win. The 70 s timer is armed with the slot, so the slot can't stick.

**NB48b-3 — `stripCandidates` misses some candidate lines Chrome parses (Low).**
`stripCandidates` finds candidate lines with a regex whose idea of a line isn't
Chrome's: Chrome's SDP parser splits only on `\n`. Some line-ending variants
survive the strip and are still parsed as candidates.
- Repro (re-check probe, loopback listeners): a hostile answer with 5 loopback
  candidates written that way, through `applyAnswer`, gave the easy connection 5
  remote candidates and 129 checks in 8 s.
- Reach:
  - the answer path, which leaves NB48b-1 open;
  - by code reading, the offer path into `openFleet`, which brings NB48-1's LAN
    reach back across the fleet.
  - Both still need the visitor's click, at most once per peer per 5 min, so the
    no-click NB48-1 stays fixed.
- Fix: strip on Chrome's line model,
  `sdp.split('\n').filter((l) => !/^\s*a=candidate:/i.test(l)).join('\n')`.
  After `setRemoteDescription`, fail if `pc.remoteDescription.sdp` (Chrome's own
  re-serialisation) still lists a candidate.
- Test: stripping SDPs with every line ending, and a stray `\r` or U+2028 inside a
  candidate line, leaves no `\n`-delimited candidate line. In Chromium, the fleet
  has 0 remote candidates and the easy side only the sprayed ones.
- **Status: fixed** by `695b38b`, re-checked 2026-10-24.
  - `stripCandidates` splits on `\n` only, as Chrome does, and drops every line that
    starts (after any whitespace) with `a=candidate:`, in any case.
  - `openFleet` and `applyAnswer` then check Chrome's own remote description. If a
    candidate is still there, they close every connection they opened and fail.
  - Re-check probe (Chromium, loopback listeners counting STUN checks): 29
    line-ending, prefix and case variants, each with one candidate aimed at a
    listener. Unstripped, Chrome checked 4 of them (41 checks each in 2.5 s) and
    rejected or ignored the rest. Through the fixed `openFleet` and `applyAnswer`:
    0 checks for all 29, and the easy side had only its sprayed candidates. The
    variants the strip leaves alone make Chrome reject the whole description, and
    both calls then fail and close.
  - Residual (Info, accepted): the after-apply check is a backstop for a future
    parser change. On the fleet it runs before the answer, so ICE never starts (0
    checks). On the easy side, which offers, ICE starts as soon as the answer is
    applied: with candidates forced past the strip, exactly one check left before the
    close, for 1, 5 or 20 candidates.

**A punched channel carries the room link (`fd660de`, `cf49f72`; reviewed
2026-10-24).** A birthday punch that works hands its DataChannel to the room
(`room.adoptChannel`). A link over signalling then sends its pipe datagrams over the
channel, and the core sees no new address. A link relayed by a member gets a new
handle, and `pw_add_candidate` lets the core punch to it and move there. The review
found nothing that weakens the session:
- The channel's far end is bound by DTLS to the SDP that came over the authenticated
  room link. Everything it sends goes to `ep.receive` and must authenticate. Probe:
  101 junk datagrams over a carried channel were all rejected (99 malformed, 2 unknown
  session), and the link kept delivering.
- The one-byte CARRY/CARRIED agreement settles only on a binary message of at most
  `MAX_DATAGRAM` bytes. A string, an oversized or an empty buffer doesn't settle it,
  and a one-sided adopt is refused after 5 s. A CARRY gets one reply on the same
  channel, so there is no amplification (but see NB48b-6).
- There is one carrier per peer, and a newer adopt ends the old one. It ends with the
  peer (`lost` → `drop`) and with the room (`close`), which closes its connection.
- No address is shared beyond what the punch already exchanged, and both visitors
  clicked to punch.

**NB48b-4 — Relay-only: a pending relayed dial sends its handshake direct (Low).**
With relay-only on and a dial through a relay still pending, `pw_add_candidate` (or
`pw_connect`) for that peer sends the handshake straight to the new address. That
reveals our address to whoever is there, which relay-only promises not to do.
- Cause: `connect_with` keeps only Via targets when the *current call* names one.
  A Direct-only call made while dialling is added to the existing relayed dial.
- Repro (FFI, scratch): a and b link to r, `pw_set_relay_only(a, 1)`, then
  `pw_connect_via(a, r, b)`, then `pw_add_candidate(a, 11, b)` before routing. a sent
  3 HS1 to handle 11. With the link live first, it sent 0.
- Reach: the host only. Native traversal under `RelayOnly` dials others only through
  the relay, and JS rooms refuse relay-only.
- Fix: while dialling, skip Direct targets when relay-only and the dial has a relay.
- Test: the repro sends 0 datagrams to the candidate in both states.
- **Status: fixed** by `4b6c0f7` and `46a5da3`, re-checked 2026-10-24.
  - `4b6c0f7`: a dial through a relay takes no Direct path added while relay-only
    (`add_candidates`, `connect`, `connect_with`). Tests in the core and the C ABI.
  - `46a5da3`: turning relay-only on while such a dial is pending drops its Direct
    targets, port guesses and spray, and the dial ignores a handshake answer that
    arrives at our own address (NB48b-7). It completes through the relay only.
  - Probes: candidates, guesses and sweeps on a relay-only relayed dial send nothing
    direct, and a simultaneous direct handshake from the peer is dropped.
  - Residual (Info, accepted): a *direct* dial made while relay-only that later also
    names a relay keeps its direct target, so it can still go live direct. As for an
    incoming HS1, the peer's answer is then taken at our address whatever address it
    comes from (`4c37bb7`). Dialling a peer directly gives up relay-only for that
    peer: the host chose to send its handshake there.

**NB48b-7 — Relay-only: a relayed dial takes the peer's answer at our own address
(Low).** Found in the NB48b-4 re-check. The handshake answer (HS2) set the link's path
to wherever it arrived from, with no relay-only check, unlike an incoming HS1. So the
peer being dialled, if it already knew or guessed our address and our NAT let it in,
could answer there: the link went live direct and our traffic confirmed the address.
- Fix: a relay-only dial through a relay drops an HS2 that arrives direct, before
  reading it, and waits for the relayed answer.
- **Status: fixed** by `46a5da3`, with a core test (an answer to an HS1 sent direct
  before the switch is dropped; the link comes up through the relay). `4c37bb7` keys
  the check on the dial instead of its relay, the same rule as an incoming HS1: under
  relay-only, an HS2 at our own address is taken only by a dial with a direct target.
  Re-checked 2026-10-24: a relayed dial, also one switched to relay-only with the
  peer's direct answer in flight, still drops it and completes through the relay.

**NB48b-5 — A route reads `direct` while a relayed link waits to fall back (Info).**
After a carried channel that replaced a relayed link closes, the core keeps the dead
handle until its 15 s direct timeout, and `room.route` reports plain `direct` (probe:
from 50 ms to 15 035 ms). In that window, an old WebRTC link's `closed` event drops the
peer, and the peer counts as a relay for others. Only the peer itself or the network
can cause this.
- Fix: `rtcClosed` drops the peer only if the closed link's handle is its current
  path, or ex-carrier handles stay marked until the path changes.
- **Status: fixed** by `08425b8` (the first fix), re-checked 2026-10-24. The transport
  passes the closed link's handle, and a close that isn't the peer's path now only
  forgets the WebRTC link. The transport keeps one WebRTC link per peer and detaches
  `onclose` from links it drops, so this never closes a live link. Probe: the peer's
  current link closing still drops it.
  - Residual (Info, accepted): in the same window `rtcFailed` can still pick the
    peer as a relay for another member's fallback. That dial fails through it; the
    other relays and the pipe still run.

**NB48b-6 — CARRY replies are unbounded (Info).** Every CARRY is answered with a
CARRIED, sent without the `MAX_BUFFERED` check. A peer can fill its own channel's send
queue with one-byte replies (probe: 500 in, 500 out).
- Fix: answer at most once per `CARRY_EVERY_MS`, through the congestion check.
- **Status: fixed** by `2a00ecb`, re-checked 2026-10-24. Each adopted channel answers
  at most one CARRY per `CARRY_EVERY_MS`, and every marker goes through `dcSend`.
  Probes: 500 CARRY at once got 1 reply (none with the buffer full); 193 spread over
  2 s got 10.

**NB48-2 — The easy side sprays whatever IPv4 address the peer names (Low).** In Net
Lab's `answer` step, `data.ip` goes to `applyAnswer` after `sprayCandidates` checks
only its format. That sends 1024 ICE checks, with retransmits, at about 16/s to
ports 1024 and up of any IPv4 address, including the visitor's own router or LAN
hosts, or loopback. Fix: accept only a public IPv4 address (not RFC 1918, loopback,
100.64/10, link-local or multicast) that matches a server-reflexive address in the
peer's own signalling.
- **Status: fixed** by `d1b2d25` and `695b38b`.
  - Fixed: `isPublicV4` refuses non-public spray IPs, and the spray needs the
    visitor's click.
  - Left as is: the IP isn't matched to anything the peer proved, since the peer
    writes all of that signalling anyway.
  - The LAN reach that remained through the answer's own candidates (NB48b-1, then
    NB48b-3) is closed by `695b38b`.

**NB46-1 — A peer can keep the dialling side publishing offers (Low).** A `restart`
signal makes the dialling side create and publish an ICE-restart offer, resent up to
twice. `MAX_RESTARTS` (3) starts over every time ICE reconnects. So a peer that
answers each restart and lets ICE reconnect can make us publish about 3 offers
(1–2 KB of SDP each) per round, without end.

Independently, and already true on main, every distinct offer from a linked peer is
answered, and every copy of the last offer gets the answer resent, with no per-peer
limit. The publishes land on our Nostr relays, which rate-limit and ban busy IPs:
relay.damus.io banned a shared hotspot IP in testing (see `86119f4`), and `2d40bc6`
dropped it from the defaults for that. Losing the relays cuts signalling for the
whole room.

Fix: honour `restart` only while our own ICE is not `connected` or `completed`, and
allow at most 3 restarts per minute per link whether or not ICE reconnects. Put the
answers and resends we publish to one peer under a budget (for example a burst of 6,
then 1 per 5 s). Found by reading the code; no probe was run.

**NB46-I1 — Port guesses aim at any IPv4 address (Info).** `checkPrediction` checks
only the format of `nat.ip`. It doesn't have to be the peer's server-reflexive IP, or
public. Each new `{ ip, next, step }` adds 32 candidates, and every `ice-batch` can
carry a new one. Remote candidates per link have no total cap, only 32 per message
(this predates NB-046). The bound is Chrome's ICE pacing, which is WebRTC's own
security model for remote candidates (RFC 8826), but the cost is cheap to tighten.
Fix: require `nat.ip` to match one of the peer's server-reflexive addresses, take one
prediction per description, and cap remote candidates per link (for example at 128).

### WebRTC retries (Fast lane 1) — FL1-1, FL1-2

While a link runs on a fallback (a member relay or the signalling pipe), the room tries
WebRTC again with a new RTCPeerConnection (`9acde3d`…`0e984ea`, `retry.js`): on a
backoff (20 s doubling to 5 min), after a network change, after a microphone or camera
grant (privacy `'direct'`, `retryOnGrant`) and on `room.retryDirect()`. Only the
dialling side (the smaller id) offers; the other asks it with a `retry` signal. Reviewed
2026-10-24 by reading the code, with two scratch repros:
- A retry offer (`retry: true`) must prove the room key even on an existing link. It
  replaces only an answering link that never opened, and closing that link clears its
  `onclose` first. The fallback carrying the session (pipe, member relay or adopted
  channel) and the session are untouched, and a link whose channel opened is never
  replaced. Over Nostr `from` is bound by the seal, so a member can't retry in another's
  name. A relay re-sending an old retry offer (SEC-6) can close a pending connection, as
  dropping signals could.
- The room acts on a `retry` ask only for a linked peer on a fallback, once per 20 s per
  peer. Its own retries go one at a time, at least 2 s apart, and a failing one holds the
  slot for about 10 s (gathering plus `fallbackMs`). So asks raise one room to about 6
  retries a minute, each an offer, an answer and late candidates.
- Retries describe the connection with the same `description()`, host filter and
  `outgoing()` as a first dial, so `public-only` gives out no local address (MIC-1,
  MIC-3). MIC-5 is unchanged.
- The new `Nudge` room frame (tag 6, empty) goes only to proven peers. Rooms on main and
  in 0.1.1 drop unknown tags after the proof.
- A pipe's queued datagrams go over the channel that now carries the link (the live
  adopted one, else the peer's own open one), in the same order as `onTransmit`. The core
  seals them either way.

**FL1-2 — Retry offers are answered without limit, each with a new connection (Low).**
This is NB46-1's per-peer gap for retries. While our link to a peer hasn't opened, each
proven retry offer from that peer with a new description closes the link, builds a new
RTCPeerConnection (gathering from the STUN servers) and publishes an answer. The room's
budget limits only the retries we begin. Repro (scratch, `room_retry.mjs` harness): a
member sent 20 proven retry offers 100 ms apart to a pair on the pipe. The other built
20 connections and published 20 answers, and the link stayed on the pipe. Fix: take at
most one new retry offer per peer per ~2 s, checked after the proof (a copy still gets
its answer resent), plus NB46-1's answer budget. A prototype of the floor made 1
connection and 1 answer.
**Fixed** by `4b131ee` (merged `a3f40d4`). After the proof, and after the check that
ignores an offer for a link we dial or one that opened, a retry offer that would make a
new connection (it replaces our link, or there is none) is taken at most once per peer
per `WebRtcTransport.retryAnswerMs` (2 s). The rest are counted in `badSignals` and leave
the current link alone. A copy of the current offer gets its answer resent, with no new
connection. Every other way a peer gets a new connection on the answering side is
already bounded, so a peer can keep at most about one connection gathering, made at most
once per 2 s, and each new one closes the last. A legitimate retry the floor refuses is
taken on its resend 3 s later, while the dialler's link waits 8 s (`fallbackMs`).
`room_retry.mjs` (test 10): 20 offers made 1 connection and 1 answer, and 19 were
refused. Still open: NB46-1's answer budget for restart offers on an open link, and a
relay replaying a proven retry offer (SEC-6), which can take the 2 s slot. Test gap
FL-T1 (Info): test 10 starts while the answering side still holds its failed link, so
it didn't reach the no-link case. Closed by `e451fc4` (merged `147537d`): in test 13,
2 proven retry offers within the floor to a side with no link made 1 connection and 1
answer, and 1 was refused.

**FL1-1 — The answering side acts on a `retry` ask (Info).** The larger id queues the
asker and asks it back, though only the dialling side should act on an ask. A
legitimate dialler never asks, and the ask goes back only to its sender, once per 20 s,
so nothing is amplified. Repro: one ask to the larger id on a pipe pair made it ask back
once, and the dialler retried once. Fix: act on `retry` only when `self < peer`; with
it, neither happened. **Fixed** by `4b131ee`: the room acts on a `retry` ask only when
`self < peer` (`room_retry.mjs` test 11: no ask back, no retry).

### Member relays for pipe links (Fast lane 2) — FL2-1, FL2-2

A link on the signalling pipe moves to a member that later links directly to both ends
(`03d3616`, `1d6a3dd`). The room marks the pipe as the link's slow address
(`Endpoint::set_slow`) and offers every proven direct member (`add_relay`). The core
probes with a sealed PUNCH through the member and moves on the PUNCH_ACK that comes back
through it. The far end answers only if it would use that member too. The link goes back
to the pipe when the member's link goes, when the peer's newer traffic comes over the
pipe, or after 15 s of relay silence. Reviewed 2026-10-24 by reading the code, with two
scratch core repros; the follow-up fixes (`63e5b72`, `eb20605`, `ddae245`, merged
`8314aa8`) were re-checked the same day with one more:
- Only the two ends can move a link. Every move or return needs an inner packet sealed
  under their session and passing its replay window; a relay can only forward, withhold
  or delay it. A move needs the prober's random token on a relay it offered. A return to
  the pipe needs a non-probe packet over the pipe with a nonce above everything that came
  through the relay, so withheld or replayed pipe packets move nothing. Signalling relays
  can't inject on the pipe (`from` is sealed).
- A Bye now also ends the core session, so links the leaver relayed go back at once. A
  Bye is acted on only as a message on the leaver's own session, so nobody else can send
  one, and a replay fails the nonce window.
- Probes are 1:1 to the peer, through at most 8 members, every 500 ms for 5 s.
  Relay-only moves nothing, answers no probe and never falls back to a pipe. No new
  frames: a 0.1.x peer ignores a relayed PUNCH, and the link stays on the pipe.
- A relay no longer forwards to a peer it reaches only over its own pipe. This closes an
  older drain, where one member made another spend the 3 messages/s budget all its pipes
  share; the asker has its own pipe. Relays on 0.1.x still forward.
- After a move, the member stays the link's fallback relay. If the pipe is silent for
  15 s, the link goes back to that member without a probe; with a 10 s keepalive, that
  means the pipe is down. Since `63e5b72` the fallback also needs that member linked
  directly, not over its own pipe, and not held off (FL2-1). Before, it needed only a
  link of any kind (review question Q4).

**FL2-1 — A member that drops only data holds a moved link until it fails (Low).** On a
member relay, `last_path` is kept fresh by any packet, and the peer sends through the
member too. A member that passes probes, acks and pings but drops larger packets
triggers neither rule, so reliable data stalls until the link fails as a whole. Before
Fast lane 2 a member couldn't take a pipe link. Repro (scratch core test): the member
dropped what it relayed to b above 200 bytes. a's 400-byte message never arrived, the
link stayed on the member, and a dropped it as timed out at 70 s. Fix: on a relay with a
slow address, go back to the pipe once reliable data has waited about 5 s unacked, and
hold that member off for the link for a while: no offer, no answer to its probes, and no
fallback to it.
**Fixed** by `63e5b72` (tests in `eb20605`, merged `8314aa8`). On a member relay, for a
link with a slow address to go back to, data unacknowledged for `RELAY_STALL` (5 s,
timed from the move at the earliest) sends the link back to the slow address and resends
what is in flight there at once. That member is then held off for the link for
`RELAY_HOLD` (60 s), doubled for each repeat up to 16 times. A link holds at most
`MAX_RELAYS` (8) members off, and a new hold replaces the one that ends first. A held
member isn't offered, its probes aren't answered, and neither the relay-to-relay roam nor
the fallback moves the link to it. A link that moves to a member pings through it at
once, so both ends have a non-probe packet through it and follow each other back. The
core test `a_relay_that_stalls_the_data_hands_the_link_back_and_is_held_off` is the
repro's case: the link goes back to the pipe `RELAY_STALL` after the data went out, the
data arrives there, and the member isn't taken again until its hold runs out. Native UDP
links have no slow address, so the rule doesn't apply to them. Residuals:
- **R1 (Info, accepted):** the rule watches reliable and urgent data in flight. A member
  that drops only large unreliable datagrams, or delays everything by just under 5 s,
  keeps the link. The unreliable lane promises no delivery, so the ends can't tell those
  drops from loss.
- **R2 (Info, accepted):** holds are per link and at most 8. More than 8 members acting
  together can stall the link in turn, each new hold pushing out an older one and its
  doubling. The link can then keep cycling between the pipe and a stalling member, each
  round delaying its reliable data by up to `RELAY_STALL`. The data still arrives over
  the pipe.

**FL2-2 — A member offered as a relay can roam a pipe link onto its own channel (Low).**
The peer's sealed probes, acks and data for a pipe link pass through every member
offered for it. A member can keep one and send it raw on its own channel. The core takes
an authenticated packet from a new direct address as a roam (NB43-I1), and a relayed
PUNCH_ACK shares the direct punch token. The link then goes to the member, the room
reads `'direct'` and drops the slow mark, and nothing brings the link back. So NB43-I1
now reaches any pipe link whose two ends share a direct member. Repro (scratch core
test): one probe taken from the member's relay queue and sent from the member's address
moved the link from the pipe to the member. Fix: in JS every handle belongs to one peer
(a WebRTC link, a pipe or an adopted channel). Pass that peer to the core with each
datagram, and drop a data packet for another peer's session before any roam, which also
closes NB43-I1 in JS. Give relay probes their own token. Rust UDP sets no slow address
and is unchanged.
**Fixed** by `63e5b72` (core) and `ddae245` (FFI and JS), merged `8314aa8`.
`Endpoint::handle_datagram_from` takes the peer whose channel a datagram came on, and
drops a data packet, HS1 or HS2 meant for another peer before decryption and before any
roam (`Stats::rx_wrong_peer`). `pw_handle_datagram_from` exposes it, and
`Endpoint.receive(addr, bytes, peer)` uses it. Every JS channel passes its peer: WebRTC
data channels, pipes (the authenticated sender of the sealed signal), adopted channels
and `LocalTransport`. Relay probes carry their own token (the direct one inverted), so a
relayed PUNCH_ACK never counts as a direct punch answer. This also closes NB43-I1 in JS.
A native UDP socket serves every peer, so it still uses `handle_datagram`, and NB43-I1
stays as recorded there.

**FL2F-1 — The relay-to-relay roam doesn't need a direct link to the new member
(Info).** Found in the follow-up re-check. The other moves onto a member relay (a probe's
answer, the fallback) need that member linked directly. The roam from one member relay
to another (`endpoint.rs:1518`) also takes a newer packet through a member reached only
over our pipe to it, such as a 0.1.x member, which still forwards over pipes. The link
then runs over that pipe's shared budget until a newer packet through the other member
moves it back. A held member is still refused. Repro (scratch core test): with a's link
to c on its slow address, a packet relayed by c moved a's link from d to c, and later
back to d. Fix: take that roam only when the packet came over a direct link
(`if fast_from`), as the probe answer does; with it the core library tests pass. Open,
suggested with Fast lane 3.

### Shared docs, `room.doc()` (NB-006) — DOC-1…DOC-5

`room.doc(name)` (`packages/js/src/doc.js`; room tag 7, and 8 when deflated) is a shared
document of maps, sets and logs that every member holds. It is a delta-state CRDT: each
write goes to every member as a delta, and a member that joins, or a doc opened again,
asks one member at a time with its context and is answered with what it lacks. Members
are trusted alike to write, as in the room: any member can overwrite, delete or fill a
doc. What the doc defends is each member's app and resources:
- Every frame is checked field by field and dropped whole if anything is off.
- State lives in Maps.
- Names, keys, values, entries, characters, writers and context ranges are capped
  (`MAX_*`), and frames stay under 512 KiB.
- What a member can have us send is budgeted per member:
  - answers and asks: 8 at once, then one per 15 s;
  - the characters our answers carry, and the deletions its answers have us pass on to
    the others: 8 Mi at once, then 64 Ki a second.

Reviewed by reading the code, with Node probes on fake networks of docs (no wasm):
- NB-006 at `8ebea00`, then its fold-ins `d798c79` and `d6b148c` (merged `c30ab0f`,
  2026-10-27);
- NB-006c at `8b7946c` (merged `45c7cfd`, 2026-10-27);
- NB-006e at `558f53c` and its fixes `ff51ce6` (merged `f40cffa`, 2026-10-28), by reading
  the code and one mutant run of `doc_crdt.mjs`.

**DOC-1 — A Lamport stamp far ahead was dropped for good (Medium).** NB-006 refused an
entry whose stamp was more than 2^32 past our clock. Deltas aren't repeated, and an
answer didn't resend it, so the entry never came again:
- one such delta made a joiner miss later writes;
- a few cut it off from everything newer;
- two sent to one member alone muted that member at everyone else.

**Fixed** by `979cdae`: every member takes any stamp up to 2^53 − 1. Residual: a member
can move the stamps to 2^53 − 1 in one frame. From then on, values set at the same time,
set items and log entries are ordered by writer (README, "Member data").

**DOC-2 — An entry forged on our own next write corrupted the doc (Low).** Past
`MAX_INCARNATIONS` (16) moves to a new writer, a member's answer could hold an entry on
the dot our next write would take. The write then landed on it, which left the doc's
index and length wrong, and the app's `delete()` threw a TypeError. **Fixed** by
`eaa2a22`:
- an entry on our own writer that we didn't write is never held;
- writes skip dots our context has;
- once every dot is claimed, writes return false or null with a warning until the doc
  is opened again.

**DOC-2a (Low)** was found in the re-check of `eaa2a22`: each forged dot was passed on to
every other member as its own range, 3.7× what the forger sent with 6 other members and
7.5× with 12. **Fixed** by `d6b148c`: one span per frame, from the lowest forged dot to
the highest (0.01×).

**DOC-2b — A catch-up's deletions were passed on one range per entry (Low).** An answer
that names a writer's dots dead in one range had us send each other member one range
for each entry it deleted here:
- a 620-byte answer sent about 110 KB to each member (1000×);
- a member's own items, deleted in cycles, sent 3.5× (6 others) to 7.0× (12).

**Fixed** by `8b7946c` (merged `45c7cfd`):
- each range of the answer that deleted entries goes on as one range, from the lowest
  dot it deleted to the highest;
- what a member's answers have us pass on is charged to its characters budget, once for
  each member it goes to;
- claims on our own writer's future dots are held as one span (I5), so our writer takes
  at most two ranges of the context; the span is reset with each frame (I4).

**DOC-3 — Frames that cost far more CPU than their size (Low).** On a loaded host:
- `kill()` walked each copy of a range: a 246 KB frame took 3.0 s;
- a full context churned in `makeRoom()`: a 1 MiB frame took 0.49 s;
- trimming a log scanned per entry: a 490 KB frame took 0.27 s.

**Fixed** by `a4fddfd`: dead ranges are merged per writer before the walk, idle writers
are kept in insertion order, and logs keep a heap, so a frame costs about one pass over
what it names (17–82 ms for the same cases).

**DOC-3a (Low)** was found in the re-check: with 1024 live writers and a full context,
`makeRoom()` scanned every writer once per range recorded (a 295 KB frame of separate
ranges, about 0.4 s). **Fixed** by `d6b148c`, which evicts in batches (0.3–2.1× an empty
context).

**NB6-L1 — A doc opened again didn't catch up (lead's review).** In NB-006 as first
written, catching up with a member was a budget per membership. So a doc closed and
opened again, such as a component that remounts, wasn't caught up.

The first fix, `26b7540`, made catching up a rate per member, and cleared what we had
sent a member for each new incarnation of its doc. The asker picks its incarnation, so
security's note showed that any member could then be sent the whole doc in every
answer: theoretical, about 96 MB at once and 0.8 MB/s from each member.

**Fixed** by `3564acf`: what answers carry has a budget of its own per member, the
characters budget above. A doc that remounts catches up every time: a small doc 8 times
in a row at once, then each up to 15 s late.

**DOC-4 — A better routed member could pause a slower one's answer on every turn
(Low).** NB-006e (`558f53c`) asks members by route: linked directly, then relayed, then
over signalling. An answer coming over signalling is paused when a better routed member
is queued, and its member is asked again after. Nothing limited how often:
- a member linked directly that was queued again during each of the slower member's
  turns paused that answer every cycle;
- its asks budget refills at one per 15 s, faster than a 60 s turn passes;
- so the slower member delivered at most 4 pieces a cycle, about 128 Ki characters a
  minute, instead of its answer.

**Fixed** by `ff51ce6` (merged `f40cffa`): a member is paused this way once until an
answer of its ends. After that, the better routed member waits for the turn to pass (5 s
without a piece, or 60 s with another waiting). The doc_crdt test: on `558f53c`, the
member over signalling was asked 8 times over 7 turns, and 67 entries were sent for 44;
on the fix, it was asked twice, and 47 were sent for 44.

Test gap (Info), closed: the test pinned "once", not "again after the answer ends", so a
mutant that never clears the flag passed `doc_crdt.mjs` (its cost would be time only: a
route that becomes direct later waits for the turn to pass). A second doc_crdt test, "paused
again once its answer ended", has the member's next answer give the turn up again: asked
`a,c,a,a,c,a` on the fix, and `a,c,a,a,c` (a failure) with that mutant.

**DOC-5 — Rooms with no doc open inflated deflated doc frames (Low).** NB-006e's `onDoc`
stopped returning early with no doc open. So a room that never opens a doc (a game, a
chat) inflated every deflated doc frame (tag 8) to as much as 16× its size, and parsed
it. **Fixed** by `ff51ce6` (merged `f40cffa`): with no doc open, a deflated frame is
dropped unread. A member deflates only for a doc whose ask said it reads deflated
frames, so such a frame is for a doc we closed. Plain frames are still read at 1×,
because an ask for a doc we don't have open gets an empty answer, to its sender only.
The room_doc test reads 1 frame on the fix and 2 on `558f53c`.

**NB-006e, checked with nothing found:**
- **Answers in pieces.** An answer goes in pieces (32 Ki characters over signalling or
  through a member, 128 Ki linked directly), at most 4 unacknowledged.
  - A MORE moves only the window of the answer it names, and never past what was sent.
  - Each piece is charged its characters before it goes, and an answer's first piece
    takes one answer unit, so the bound is the one above.
  - A piece is never empty unless it ends the answer.
- **The turn.** A member that trickles pieces holds our turn at most 60 s per ask while
  another member waits. One that stalls gives the turn up 5 s after its last piece, and
  keeps it only while no other member can be asked.
- **Inflating.** Inflate stops at min(1 MiB, 16× the compressed size), fed in 1 KiB
  slices, so at most one slice's output is held past the cap. A truncated or junk stream
  is dropped. Frames stay in order per member both ways, and one inflated after its
  member left is dropped.
- **The pipe.** Its budget is unchanged: 3 batches a second shared, burst 6.
  - A full batch goes at once when a token is free.
  - A pipe timer that fires 250 ms or more late (a background tab) moves flushes to the
    end of the task, until one fires on time.
  - Out of tokens, the pipe waits on a timer.
- **Sync status.** `doc.sync` is as good as the members that told us (README).

**Info:**
- **I1–I3** (NB-006), addressed by `11cf2e6`:
  - change notes were keyed so that map `a` key `b\0c` and map `a\0b` key `c` gave one
    event;
  - the README now says that versions without `room.doc()` (0.1.0 to 0.2.0) drop doc
    frames;
  - a member that let our ask time out is asked again only once it answers, so one that
    never answers holds the others' turns up once.
- **I4, I5** (NB-006c), fixed by `8b7946c`, under DOC-2b.
- **I7** (NB-006e), fixed by `ff51ce6`: an answer under way held the entries the asker
  lacked. A member that never acknowledged kept the doc as it was at its ask, deleted
  values included, one copy per member. The answer now holds ids, and each is looked up
  as its piece is cut.

**Residuals (accepted; the README's "Member data" and "Logs" say them):**
- **DOC-2c (Info):** a member that keeps a log longer than another can miss older
  entries the other trimmed, once it catches up from that member, directly or through a
  member that did (README `72548e2`). Give `keep` the same value on every member.
- **I6 (Info):** the budgets are per member while it stays. A member that leaves and
  joins again, or joins under a new id, starts with whole ones, so N ids get N budgets
  (README `72548e2`).
- **I8 (Info):** a member can make us inflate and parse up to 16× what it sends in
  deflated doc frames (`DEFLATE_RATIO`). That is capped at 1 MiB a frame, plus at most
  one 1 KiB slice's output past the cap, and holds only in rooms with a doc open
  (DOC-5). A frame that would deflate more goes plain, by design.
- **Member trust.** Any member can:
  - claim another member's next writes, so the room ignores them; the claimed member
    moves to a new writer, 16 times;
  - move the stamps to 2^53 − 1;
  - name any writer when it relays a catch-up (`from` is as good as the member that
    told us);
  - make `doc.sync` show `'synced'` early, or `total` too high.

### `@peerwire/react` hooks (NB-049) — RH-1…RH-6

`@peerwire/react` (`packages/react`, 0.1.0 published from `dec1554`) wraps a room in
React hooks. Its store (`store.ts`, one per room) keeps shared keys (one value per key
for the whole room, last writer wins, replayed to each member that joins) and per-peer
values (each member's latest value per key, such as a cursor). Both travel as room
messages with a `$peerwire` field. Reviewed 2026-10-24 at `d369c8d` and re-checked at
`8126119` by reading the code, with Node probes on fake rooms, the real hooks on jsdom,
and two loopback rooms.

**Trust statement.** Every member is trusted alike to write, as in the room itself: any
member can overwrite any shared key, write well-typed nonsense, or show members different
values by sending a write to some of them only (`to`). What the store defends is each
member's app and resources. A member must not be able to crash another's app, make its
own writes fail, use up the caps for everyone, or make catch-up costly. Shared values
outlive their writer and every member replays them to each joiner, so a bad value there
repeats on every join, where a bad chat message (RX-1) happens once.

What holds:
- **Message checks.** Keys are at most 256 characters and values at most 64 KiB as JSON
  (`MAX_VALUE_LENGTH`). Clocks and sequence numbers must be safe integers of at least 1.
  A writer named in a catch-up must be a PeerId. Catch-up arrays hold at most 1024
  entries. State lives in `Map`s, so keys such as `__proto__` are harmless, and the member
  list and per-peer views the hooks return are frozen.
- **Attribution.** Per-peer values, also inside a catch-up, are charged to the peer that
  sent the message, never to a claimed id. A shared value's writer `w` can be claimed in
  a catch-up, on purpose: that's how a joiner gets a value whose writer has left. A
  claimed writer matters only in a tie, and a member can win any key outright with a
  later clock. So `w` must never be shown to users as "who wrote it" (nothing does today).
- **Checks.** An `accept` check that throws counts as failed. A shared value that fails
  the check reads as `initial`, the same on every member.
- **Internal messages.** `useMessages` and `useRoomEvent` hide `$peerwire` messages. Code
  that listens to the room directly sees them.

**RH-2 — The default shared-state check was one level deep (Medium).** Without `accept`,
`useSharedState(key, initial)` checked only that a value had the same top-level kind as
`initial`, and a `null` initial took anything. A member wrote `[{"text":{"x":1}}]` to an
array key. Every app that rendered `t.text` threw while rendering; with no error
boundary the app unmounted and its cleanup left the room. The value persisted, so each
joiner got it in catch-up and crashed the same way, until the key was written again or
every member holding it had left. Reproduced with the real hooks on jsdom. **Fixed** by
`78c4ebe` and `8126119`: without `accept`, `initial` must be a string, number or boolean
(a TypeScript overload, and a `TypeError` at render otherwise). Arrays, objects and
`null` need `accept`. Member data in `useMessages`, `usePeerState` and `usePeers` is
typed `unknown` unless checked, so TypeScript flags rendering an unchecked value. The
README tells apps to check every field they render, however deep, and to put an error
boundary inside `RoomProvider`; the example has one. `test/hooks.mjs` covers the nested
case. What is left is the app's to get right: an `accept` shallower than what the app
renders. An error boundary catches errors in render and effects, not in event handlers.

**RH-1 — One far-future clock froze every shared key (Low).** The store kept one clock
for all keys and raised it to any member's clock. One write with a clock of
`Number.MAX_SAFE_INTEGER`, to any key, made every later `setShared` throw "clock
exhausted", for every key, on every member. Catch-up carried it to joiners, so it
outlived the writer, and a setter called in an effect unmounted the app. **Fixed** by
`78c4ebe` before the first publish, because the clock is part of the messages between
versions. Each key has its own hybrid clock: a write is stamped
`max(Date.now(), current + 1)`, and a member's clock is taken only up to `Date.now()` +
`MAX_CLOCK_DRIFT` (5 min), in writes and in catch-up versions alike. Nothing throws for a
clock any more. A bound relative to our own last clock was rejected: a joiner starts at
0, and members would stop agreeing. Residual:
- **Clock skew near the bound (Info, accepted).** When honest members' clocks differ by
  δ, a write stamped within δ of `now + MAX_CLOCK_DRIFT` is taken by the members whose
  clocks are ahead and ignored by those behind. No write is sent twice, so they disagree
  on that key until a write that every member takes beats that stamp. Writes from the
  members that took it qualify about δ later; writes from the others only once their own
  clocks pass the stamp, up to `MAX_CLOCK_DRIFT` + δ later. A member can do this on
  purpose by stamping near the bound, which gives it no more than a write sent to some
  members only. A member whose clock runs more than `MAX_CLOCK_DRIFT` ahead has all its
  shared writes ignored (the README says so).

**RH-3 — All members shared the caps, first come, first served (Low).** One 82 KB message
with 1024 junk keys meant no member kept any member's new shared key, even after the
writer left. One member could take every per-peer slot. A local write past the cap was
kept and sent, but every other member dropped it, so the writer silently disagreed with
the room. **Fixed** by `78c4ebe`. Per-peer values have a quota per member, ours included:
`MAX_PEER_KEYS` (64) and `MAX_PEER_LENGTH` (256 KiB), charged to the sender and freed
when it leaves. Shared state holds 1024 keys and `MAX_STORE_LENGTH` (1 MiB) in all. When
it is full, a key a mounted hook reads, or our own write, pushes out the oldest keys no
hook reads; a local write that still doesn't fit is dropped with a warning (RH-5).
Accepted: what is pushed out is local, so members can hold different unread keys; a
joiner gets the newest version any member still holds. A hook that mounts later for a
key that was pushed out reads `initial` until the key is written again, while others hold
a value (a route change or a virtualized list can do this). A member can force it by
filling the unread space, which is no more than overwriting the key, as it can anyway.
"Oldest" is insertion order, so long-held app data goes before fresh junk.

**RH-4 — Each join cost every member its whole state twice (Low).** On a join both sides
sent everything and asked, and the answer to an ask sent everything again: 100 KB of
shared state cost 216 KB to one joiner (2.16×). At the old 16 MiB cap that was up to
32 MiB per member per join, more with repeated asks, and minutes of the pipes' shared
budget. **Fixed** by `78c4ebe` (a change to the catch-up messages, before the first
publish): catch-up by digest. A join sends the versions held (`[key, clock, writer]`)
and asks. The answer carries only the values the asker's digest lacks, plus our
per-peer values, and never one already sent to that member. It asks back when the asker
holds a version we lack or hasn't answered us yet, which covers a first message lost to
the room's welcome race (on `@peerwire/js` 0.1.1). A store answers at most 4 asks per
member while it stays, and keeps at most 1024 of the versions a member told it. Per
membership, a member can now draw from us at most 1 MiB of values once, 4 digests (up to
1024 versions, about 350 KB each) and our per-peer values (256 KiB) once: about 2.5 MB,
where it was 4 × 16 MiB. A 100 KB store now costs 1.18× per joiner. Every member still
sends its state to each joiner, which is slow over a relayed link or the pipe; the
README says to keep shared state small.

**Fixed in 0.1.1.** `@peerwire/react` 0.1.0 was published with these four. `12974c4`
(merged `41cc24a`, `@peerwire/react` 0.1.1) fixes all of them, re-checked 2026-10-26 by
reading the code, with Node probes on `dist/store.js` built from that commit. None
changed the messages:
- **RH-5 (Low): member data could still make our own shared write throw.** In an app
  whose hooks read more than 16 shared keys (`MAX_STORE_LENGTH / MAX_VALUE_LENGTH`),
  members can fill the keys it reads with 64 KiB values. Then our write to a new key, or
  one that grows a value, has nothing it may push out, and in 0.1.0 `setShared` throws
  `RangeError`. Called from an effect, that shows the error boundary's fallback, or
  unmounts the app without one. From an event handler it is an uncaught error and the
  write is lost. It is RH-1's kind (member data causing a throw in app code), narrower.
  **Fixed**: a write that doesn't fit is dropped with a `console.warn`, as every member
  drops a member's write that doesn't fit, and `setShared` returns `false`. The store
  works out what it would push out before it pushes anything, so a dropped write sends
  nothing and changes nothing. It still throws, before any change, for the app's own
  mistakes: a value over 64 KiB as JSON or a key over 256 characters. An app on 0.1.0
  that reads many shared keys can catch the `RangeError` around its setter calls, or
  upgrade. Residual (Info): the warning comes once per dropped call, so an app that
  writes on every frame to a store members have filled logs on every frame; warning once
  per key is planned for 0.1.2.
- **Catch-up message size (Low, via OUT-1).** In 0.1.0 a catch-up message holds at most
  64 values but has no byte cap, so near a full store one can pass the reliable lane's
  1 MiB (non-ASCII text takes up to 3 bytes per character). The store splits a message
  when the room throws `RangeError`, but the room doesn't throw when that peer's outbox
  holds frames (OUT-1, below): it drops the message silently, and the store has marked
  those values as sent. The joiner then lacks them from us until they are written again;
  another member's answer may still bring them. **Fixed**: each catch-up message, digests
  included, is cut at 512 KiB (`CHUNK_BYTES`), counting 3 bytes per UTF-16 code unit of
  each entry as JSON. UTF-8 takes at most that, since `JSON.stringify` escapes lone
  surrogates, so a message stays under 512 KiB plus its envelope, and one entry alone (a
  64 KiB value under a 256-character key) under about 200 KiB. A probe with worst-case
  `€` keys and values, measured as the room frames it, found 394,963 bytes at most,
  against the 1,048,576 limit. The split on `RangeError` stays as a fallback. OUT-1
  itself is still open in `room.js`.
- **RH-6 (Info): catch-up records kept replaced values alive.** For each member the store
  kept the entries it last sent it (`sentShared`, `sentLive`). When a key was written
  again, the old value stayed in memory while that member stayed: at most one old value
  per key per member, up to about 1 MiB of shared and 256 KiB of per-peer values for each
  member, beyond the store itself, if keys kept being rewritten between joins. **Fixed**:
  the records hold each entry's version (clock and writer, or sequence number) instead,
  which compares the same, because a write never replaces an entry with an equal version.
  A key pushed out of the store is removed from every member's record, as before.
- **N1 (nit, cost only):** an asker whose digest already held a newer version of a key
  still got our older value, which it ignored, because `tell()` recorded the asker's
  version only when it wasn't newer than ours. **Fixed**: `tell()` records it either way,
  so the answer carries only that key's version. It still asks back only for a digest
  version we lack, and still keeps at most 1024 told versions per member.

### Bounds and residuals from reviews (NB-014…NB-043)

This section records what each earlier review established about resource bounds,
and what it accepted. Values are from main `7b716a2`.

**Core sessions**
- **Receive memory per link.**
  - Reliable lane: `RECV_WINDOW` 1024 fragments of `FRAGMENT` 1100 B.
  - Urgent lane: `URGENT_RECV_WINDOW` 64 fragments, with messages capped at
    `MAX_URGENT` (64 KiB).
  - Fragments longer than `FRAGMENT`, and reassembly past `max_message` (1 MiB by
    default), are refused.
  - The bound tested in `fuzz.rs` is about 2.31 MB per link (2.17 MB reliable, 0.13
    MB urgent). `docs/DOCKER.md` sizes `PEERWIRE_MAX_PEERS` from it: 256 peers can pin
    about 0.6 GB.
- **Retransmission.**
  - RTO is 20 ms to 5 s; backoff `rto·2^min(tries-1, 8)`, at most 10 s.
  - After `MAX_TRIES` (16) the link is dead.
  - ACKs report a 64-fragment SACK bitmap. Since PERF-10, a fragment beyond the
    bitmap's reach doesn't time out while the hole below it is repaired.
- **Congestion control.**
  - A loss halves the window (`LOSS_CUT`).
  - Queueing delay above 25 ms cuts it to 0.85 (`DELAY_CUT`).
  - A loss with under 5 ms of queueing cuts it to 0.8 (`RANDOM_LOSS_CUT`, Veno);
    PERF-10 deepens that to 0.5 on a timeout in the same round trip.
  - The minimum window is `MIN_CWND` (4).
  - Congestion state is per link, so a peer that games it affects only its own link.
- **Links.**
  - `max_links` is unlimited by default, and the `peerwire-peer` server sets it to
    256.
  - Half-open handshakes don't count towards `max_links`.
  - An HS1 is refused before the DH when half-open handshakes reach
    `max_pending_handshakes` (4096) or links reach `max_links`.
  - An unannounced responder past the cap is dropped at its first data packet.
  - Handshake floods are still SEC-4 (cookies, NB-024).
- **Relay.**
  - 256 KiB/s with a 64 KiB burst per peer, and a 256-packet queue.
  - The total (`relay_total_rate`) is unlimited by default, and `peerwire-peer`
    sets it to 8 MiB/s.

**JS endpoint and rooms**
- **Lifetime.**
  - `leave()` sends `Bye` to each linked peer on the unreliable lane, then
    disconnects them.
  - On `pagehide` it waits at most 20 ms for that.
  - A received `Bye` is inside the Noise session, so it only ever drops the
    sender's own link. Rooms accept it before the room proof, which is harmless for
    the same reason.
- **Sends.** `sendRetry` keeps at most one message above the 16 KiB scratch buffer
  between tries. The room's outbox has no cap. It holds only what the local
  application sends, so that is application policy.
  - **OUT-1 (Low, open; `@peerwire/js` 0.2.0 and main):** `room.send()` throws
    `RangeError` for a message too large for its lane only when that peer's outbox is
    empty. When it holds frames (more than the 4 MiB send buffer unacknowledged to that
    peer: a slow or relayed link, or a burst), `write()` queues the frame unchecked and
    `flush()` later drops it as too large. The caller sees no error, the peer never gets
    the message, and later messages still arrive. Found in the `@peerwire/react` review:
    its catch-up split relied on the throw in 0.1.0 (capped by size in 0.1.1, so it no
    longer does), and `setLive`'s move from the unreliable to the reliable lane still
    does. Reproduced with two loopback rooms: a 1 MiB + 1 KiB send threw with an empty
    outbox, and with 9 frames queued it didn't throw and never arrived. Fix: in
    `write()`, check the frame against the lane's limit before queueing and throw the
    same `RangeError`; for a broadcast, check once before writing to any member, so a
    throw doesn't leave the message sent to some members only.
- **Signals.**
  - Malformed or unproven signals are counted in `badSignals` (a counter only, with
    no action).
  - An `ice` or `ice-batch` message holds at most 32 candidates, and at most 32 are
    held before the description.
  - Residuals:
    - No total cap on remote candidates per link (NB46-I1).
    - No cap on signals held while an offer's proof is checked (one async check, so
      the window is short).
- **Fallback pipes (NB-043).**
  - `MAX_PIPE_PEERS` 256, evicted oldest first. Since `a283a01` an evicted permission
    drops its pipe unless a session uses it (NB43-1).
  - Outgoing pipes share 3 messages/s (burst 6), in batches of 16 KiB.
  - Each pipe's queue is 64 KiB, dropping the oldest.
  - Incoming batches are capped at 64 items but not rate-limited.
  - Residuals:
    - **NB43-I1:** a relaying member can roam a link onto its own pipe, because a
      direct-to-direct roam needs no path validation. Since Fast lane 2 this reached
      pipe links too (FL2-2). **Fixed in JS** by `63e5b72` and `ddae245`: each datagram
      names the peer whose channel it came on, and a packet for another peer is dropped
      before any roam. Still open on native UDP, where one socket serves every peer.
    - **NB43-I2:** an insider holding the room key can churn the pipe permissions,
      take the shared budget, and send 64 HS1s per batch. Card NB-043b; it supports
      SEC-4.
    - **NB43-I3:** the Nostr backoff never decayed; fixed in `a283a01`.
    - **NB43-I4:** relays see session-long timing and volume of piped links, and
      every direct member learns that a pair failed WebRTC.

**Signalling**
- **JS Nostr.**
  - Content is capped at 64 KiB.
  - 4096 event ids are remembered (oldest forgotten), and each must be 64 hex
    characters.
  - A relay that rate-limits us is paused from 30 s, doubling to 10 min, and the
    pause starts over after 10 quiet minutes.
  - Closed sockets are redialled from 1 s, doubling to 30 s.
  - Residual: JS checks neither the event id hash nor the signature, so a relay can
    present one event under many ids. Offers are sealed and hellos proven, and
    replays fall under SEC-6, so the cost is duplicate work. Rust checks the id.
- **Rust Nostr.**
  - Websocket messages and frames are capped at 128 KiB.
  - The inbox holds 1024 events, then drops.
  - 4096 event ids are remembered.
  - At most 64 publishes are queued while a relay is down.

**Addresses we give out**
- **NB-041.** Offers to a relay peer name no links. Residual: under `Direct`, a relay
  peer sees our LAN addresses; `PublicOnly` hides them.
- **NB-042.**
  - **NB42-I1 and NB42-I2:** mapped `::ffff:` addresses compare as IPv4, and NAT64
    `64:ff9b::/96` is judged by its IPv4; fixed in NB-042b (`1529b54`).
  - **NB42-I3:** Godot always binds `[::]`, so a host with global IPv6 and no inbound
    firewall is reachable unsolicited. That makes SEC-4 more urgent.
  - **Residual NB42b-I1:** `is_public` doesn't unwrap NAT64, so a NAT64 address
    embedding a private IPv4 still gets a guess window.
  - **Residual NB42b-I2:** the local-use prefix `64:ff9b:1::/48` isn't covered.
- **`73aa866` (private interface addresses).** A socket bound to any interface also
  offers up to 3 RFC 1918 interface addresses. Container and VM bridges are skipped
  by name, but VPN tunnels (`wg*`, `tun*`, `utun*`, `ppp*`) are not. So under
  `Direct` a peer can learn, for example, a corporate VPN address. Suggested: skip
  those interface names too.
- **MIC-3 (Low, privacy): `public-only` in `@peerwire/js` 0.1.1 and earlier gave out
  local addresses.**
  - **What:** once a browser shows a page's local addresses (the page has microphone
    or camera permission), a server-reflexive candidate's related address (`raddr`,
    `rport`) is the local socket it was sent from. So each interface with a STUN
    mapping gave away its local address, in the description and in trickled and late
    candidates. When no STUN answer came before the description was taken (on Nostr,
    within `GATHER_MAX_MS`, 2 s), gathering had also written a host address into `c=`,
    the m-line port and `a=rtcp:`.
  - **Who:** `joinRoom({ privacy: 'public-only' })`, and `WebRtcTransport` under
    `'public-only'` or `'relay-only'`. An app with camera or microphone permission
    (voice, video, meetings) that chose `public-only` gave every peer that got its
    description (in a room, every member) its LAN, VPN tunnel and container bridge
    addresses.
  - **Fixed** by `6303bd8`: `withoutHostCandidates` and the transport's sent
    candidates clear the related address to `0.0.0.0 0`, and `c=`, the m-line port and
    `a=rtcp:` go back to `0.0.0.0` and `9`, as before gathering. ICE reads none of
    these. The clearing doesn't rely on the runtime hiding anything, so it also holds
    in runtimes that never hide local addresses (we haven't checked which ones fill
    `raddr` without a permission). Suggested: the next npm release notes tell apps on
    0.1.1 that use `public-only` to upgrade.
  - **Not covered:** `'direct'` gives out every local address the runtime shows. ICE
    checks leave from local sockets, so a peer on the same network can still see a
    local address as a peer-reflexive candidate.
- **MIC-5 (Info):** `withoutHostCandidates` splits on `\r\n` only, so an SDP with bare
  `\n` line ends comes out unchanged, host candidates and all. Browsers write `\r\n`;
  a custom `RTCPeerConnection` (Node, native) might not. Suggested: split on `\n` as
  `stripCandidates` does (NB48b-3), and match `typ host` before any `\r`.

**Demos**
- No CSP is sent by `crew-server.mjs`, which serves Net Lab and Crew. Only the Docker
  examples' nginx config sets one (`script-src 'self' 'wasm-unsafe-eval'`,
  `connect-src 'self' wss:`, `frame-ancestors 'none'`). Net Lab escapes everything a
  peer sends before it reaches `innerHTML` (checked for NB-047 and NB-048), so a CSP
  is defence in depth. Suggested: send the same CSP from `crew-server.mjs`.
- Net Lab caps its event log at 500, the DOM log at 300 and chat at 200. The
  per-peer route history has no cap.
- **README examples (`9e4640b`, reviewed 2026-10-24).** "Using with React" in
  `packages/js/README.md` joins in an effect and leaves in its cleanup. A room `message`
  is whatever the member chose to send: text, bytes or any JSON.
  - **RX-1 (Low):** the example's hook stored every message and rendered `{m.data}`. A
    member who sent an object made React throw while rendering, which unmounted the
    app, and the cleanup then left the room. So one broadcast object took every other
    member running the example out of the room. Reproduced on loopback (Vite dev server,
    BroadcastChannel). **Fixed** by `b0d354b`: the hook keeps string messages only, and
    a bullet tells apps to check a message's type before they store or render it.
  - **RX-2 (Info):** the message list had no cap, so a member flooding the room grew it
    without bound. **Fixed** by `b0d354b`: the hook keeps the last 100.
  - The ten-line chat writes messages with `textContent`, so an object only shows as
    text.
- **Microphone permission (Net Lab's "Use microphone" button, `e450baf`).** Chrome
  hides a page's local addresses (mDNS names, the default route only) until the
  origin has microphone or camera permission. After that, every new connection lists
  every interface (RFC 8828 mode 1). The button asks only for that permission and
  stops the tracks at once: nothing keeps the stream or uses the audio (checked in
  Chromium). Presence carries a `mic` boolean, to room members only.
  - **MIC-1 (Low, privacy):** after a grant, new links give every room member the
    device's real local addresses (LAN, hotspot, VPN tunnel, container bridges). Each
    interface's STUN mapping can also show the public address of a network other than
    the default route, such as the connection under a VPN, or mobile data while on
    Wi-Fi. In the global room that is anyone. The grant is stored per origin, so
    later visits and the other demos on that origin do the same without a tap.
    `privacy: 'public-only'` drops the local (`host`) candidates, and since `6303bd8`
    every other local address too (MIC-3), but still sends each interface's public
    mapping. Any app that asks for the microphone or camera (voice, video) gets the
    same under `privacy: 'direct'`, the default. **Fixed** for Net Lab by `d78d046`:
    the global room is joined with `privacy: 'public-only'` while the microphone is
    allowed, also after a grant made in another tab, in the site settings or while
    joining. The button and its text say that named and private rooms get the local
    addresses until the permission is removed in the site settings. `877d02c` sends a
    birthday punch's offer without candidates (the hard side never used them), and
    `6303bd8` takes the default address out of both punch descriptions. Residual MIC-4.
  - **MIC-2 (Info):** a birthday punch closed while its `openFleet`, `openSpray` or
    certificate is still pending (a rejoin, its timeout, the peer's abort) throws
    when that resolves. The connections it just opened are left open, and the
    one-fleet slot is already free. Nothing is sent. **Fixed** by `9282d36`: after
    each await of a punch's setup, a punch that has ended closes what it just opened
    and returns. `691dec0` takes one offer per punch on the hard side and one answer
    on the easy side, marked before the first await. Before, each further offer from
    the peer in a punch the user had accepted opened another fleet of up to
    `FLEET_SIZE` connections, and closing the punch closed only the last: a Low
    bypass of NB48-1's one fleet at a time.
  - **MIC-4 (Low, privacy):** in the global room, the button re-tests the network
    after the grant (`reprobe()`: 2.5 s after the first STUN answer, at most 8 s) and
    only then rejoins with `public-only`; `guardGlobal` leaves it to the button. A
    member who links in that window gets the real local addresses. In Chromium, with
    a 2.5 s re-test, a tab that joined during it got all four of the other tab's local
    addresses, and none with the rejoin first. **Fixed** by `134fed3`: the button
    rejoins as soon as `getUserMedia` resolves, then re-tests. Leaving the old room
    closes its connections synchronously, so a description still gathering there is
    never sent. Accepted residual: the grant is stored before `getUserMedia`
    resolves, so during the device's open time a member joining the old room could
    still be described with the local addresses, if gathering also finishes in it.
  - **Retries (Fast lane 1, `9acde3d`, `d888522`; reviewed 2026-10-24).** When the
    page gets microphone or camera permission, a `'direct'` room now retries WebRTC
    with each peer it reaches by fallback (`retryOnGrant`, default true). So those peers
    get the local addresses within seconds too, not only peers that link later.
    `'public-only'` retries leave them out, as first dials do (`retry_direct.mjs`
    checks this in Chromium with a stored grant). In named and private rooms, Net Lab's
    button now retries (`room.retryDirect()`) instead of rejoining. Only peers on a
    fallback get a new connection, and ids stay the same. The global room joins with
    `retryOnGrant: false` and still rejoins first. MIC-4's accepted window also covers
    a retry begun in it (by the backoff, a network change or the peer's ask). Leaving
    the old room closes such a retry, as it closes a first dial, unless gathering
    finishes first.

---

## Invite strings (`pw:`)

An invite (`crates/peerwire-core/src/invite.rs`) is `pw:` + unpadded base64url of
`version | peer(32) | addrs | relays`. It carries **no secret**: the PeerId is a
public key and the addresses are routing hints. Security properties:

- **Confidentiality: none, by design.** Anyone who sees an invite learns the
  PeerId and candidate IP addresses of the inviter. An invite posted publicly is
  a standing version of SEC-3 (IP disclosure) and SEC-2 (identity disclosure):
  treat invite links as semi-private, and prefer LAN/relay candidates over a
  home IP when sharing broadly.
- **Integrity: not self-authenticating, but safe.** An invite is unsigned, so a
  carrier can swap the addresses or relays. This does not break authentication:
  the handshake still runs Noise IK to the PeerId in the invite, so a swapped
  address only reaches the real peer or fails. A carrier that swaps the *PeerId*
  produces an invite to a different identity — the recipient connects to whoever
  the carrier chose, exactly as if the carrier had sent its own invite. Invites
  therefore inherit the trust of the channel they arrive on; a QR shown in person
  or a sealed signaling message is trustworthy, a plaintext forum post is not.
- **Parsing is safe.** `from_bytes` is a bounds-checked reader, refuses more than
  16 entries, rejects trailing bytes, and validates each PeerId is a real Ed25519 point.
  Reviewed, no issues; covered going forward by fuzzing (NB-014).
- **Canonical.** Each invite has exactly one link: base64url decoding refuses a last
  symbol with its unused low bits set, and a count over 16 is refused rather than
  clamped. Nothing keys on the link string today, but a future dedupe, cache or
  allow-list on it cannot be bypassed with a lookalike. `fuzz_invites` asserts
  `to_link(parse(s)) == s` for every accepted `s`.

No new finding; the exposure is documented so apps set expectations for where an
invite may be posted.

## Godot binding (NB-020, planned — security requirements)

The coming `bindings/godot` crate exposes `PeerwireMultiplayerPeer`
(`MultiplayerPeerExtension`) so Godot's high-level multiplayer — `@rpc`,
`MultiplayerSynchronizer`, `MultiplayerSpawner` — runs over Peerwire. Peerwire
delivers *authenticated, confidential bytes* to that layer, but everything Godot
does *above* the `MultiplayerPeer` is outside Peerwire's guarantees. This section
sets the requirements the binding must meet; they are tracked as SEC-9…SEC-11
(proposed follow-ups on NB-020, owner @peerwire-backend).

### SEC-9 — Untrusted packets crossing into GDScript (High, design) — NB-029

Godot's high-level multiplayer deserializes incoming packets into Variants and
dispatches `@rpc` methods by name. Variant decoding of `Object`/`Callable`/
`RID` from the network is a known remote-code-execution surface (object
instantiation, resource loading). Because Peerwire is pure P2P, **every connected
peer is a remote attacker** at this layer.

**Requirements.**
- The binding must configure the `SceneMultiplayer` with
  `allow_object_decoding = false` (Godot's default, but assert it) so peers
  cannot ship encoded `Object`s.
- Document loudly that `@rpc("any_peer")` methods receive attacker-controlled
  arguments and must validate them and check `multiplayer.get_remote_sender_id()`.
- Surface Peerwire's per-lane size caps to the `MultiplayerPeer` so an oversized or
  malformed Godot packet is rejected at the boundary, not deep in a decoder.
- A Rust panic in the cdylib must not unwind into Godot: catch/abort at the FFI
  edge (godot-rust convention), so a malformed packet can't turn a panic into UB.

### SEC-10 — Godot peer-ID spoofing and mapping collisions (High, design) — NB-029

Godot peer IDs are 32-bit ints; Peerwire PeerIds are 256-bit Ed25519 keys. The
binding derives a stable non-zero Godot ID from each PeerId. A 32-bit space makes
collisions reachable (birthday bound ~2^16 peers, but an attacker can *grind*
PeerIds offline to target a specific victim's Godot ID).

**Requirements.**
- The PeerId → Godot-ID map is authoritative per session and **collisions must be
  rejected**, not silently merged: a joiner whose derived ID clashes with a live
  peer is renumbered or refused, never allowed to inherit another peer's ID.
- `_get_packet_peer()` must return the Godot ID of the *authenticated Peerwire
  source* of that packet only; it must never be derivable from packet contents an
  attacker controls.
- Host is Godot ID 1: the binding must bind ID 1 to the actual host identity and
  reject any joiner that tries to present as 1.
- Grinding resistance: either widen the effective namespace (keep the full PeerId
  as the identity and treat the 32-bit ID as a non-security-bearing handle the app
  must not use for authorization), or document that `is_multiplayer_authority()`
  and peer-ID checks are **not** a security boundary — the PeerId is.

### SEC-11 — Host authority over RPCs in a serverless mesh (Medium, design) — NB-029

Godot's model assumes peer 1 is a trusted server/authority. Peerwire has no
inherent authority: a peer can send any packet to any peer, and
`MultiplayerSpawner`/`MultiplayerSynchronizer` let an "authority" spawn/despawn
nodes and set properties on others' scene trees. A malicious peer that the app
treats as an authority corrupts every client.

**Requirements.**
- `docs/GODOT.md` must state that Godot's authority model is only as trustworthy
  as the app's topology; in an open mesh, `@rpc("authority")` is not a security
  control.
- Recommend a pattern: designate the host as the only synchronizer/spawner
  authority and have it validate inputs, or use `peerwire-game`'s deterministic
  lockstep (where every peer recomputes state and desync is detected) instead of
  trusting an authority's state pushes.
- The binding should not auto-relay packets between peers on behalf of the Godot
  layer unless the app opts in (Peerwire relay is already gated and rate-limited;
  don't bypass it).

**Status of SEC-9…SEC-11 as of `ccc3b79`.** The binding shipped (NB-020) and was
hardened (NB-030). `docs/GODOT.md` (Security) now states each requirement: keep
`allow_object_decoding` off (the demo asserts it), treat `@rpc("any_peer")`
arguments as attacker-controlled, ids are labels (the host renumbers collisions,
only the dialled host is 1, authorize by `get_peer_key`), and Godot's authority
is only as trustworthy as the peer made authority (use the host, or lockstep on
`peerwire-game`). SEC-11 is a design property: it stays a documented residual, not
a bug. NB-037's room-key proof does not change it. It only limits who can join a
room to holders of the room key, and any of them can still be malicious. Godot's
`server_relay` is on by default; `docs/GODOT.md` names its cost, how to turn it
off, and that a relayed packet's sender id is the host's word (the host can forge
it).

**NB-029 audit (`72c7e57`, `bindings/godot/src/{peer,session}.rs`).** Each
requirement against the code:

- SEC-9: the demo asserts `allow_object_decoding` is off (`main.gd`).
  `get_max_packet_size()` is the current mode's cap, `put` refuses larger, and
  received packets wait in a queue capped at 4096 packets and 16 MiB. gdext 0.5.5
  runs every `#[func]` and virtual method under `catch_unwind`
  (`godot-core::private::handle_panic`), so a panic is logged as a Godot error
  and never unwinds into the engine; no panic is reachable from peer input in
  `session.rs`.
- SEC-10: `derive_id` never yields 0 or 1, and `IdMap::assign` moves a colliding
  key to the next derived id (`colliding_ids_are_moved`). A packet's peer is
  looked up from the authenticated Peerwire sender; a frame from a peer with no id
  is dropped. A joiner takes ASSIGN only from the host it dialled, only once,
  and never 1; a host ignores ASSIGN (`spoofed_frames_are_dropped`).
- SEC-11: documented, as above; nothing in the binding forwards packets itself.
- **N29-1 (Low) — fixed.** The data frame's mode byte is the sender's word and was
  not checked against the lane the message arrived on: a peer could label
  unreliable-lane packets `RELIABLE`, and `get_packet_mode()` reported a
  guarantee the packet never had. Repro: `on_message(host, Lane::Unreliable,
  encode_data(Reliable, ..))` queued a `RELIABLE` packet. Fix: a frame whose
  `mode.lane()` differs from the delivering lane is dropped
  (`spoofed_frames_are_dropped`, which fails with the check removed).
- **Residual.** An admitted joiner can fill the 4096-packet queue every frame,
  and Godot decodes everything it is handed: a CPU cost bounded by `max_peers`
  and each link's bandwidth, not a memory one. The host's remedy is
  `disconnect_peer` (and a ban by key); per-peer rate limits are app policy.

## Relation to the Protocol Zero red-team notes

Peerwire was bootstrapped from the Protocol Zero SDK, whose adversarial notes
(`p0/protocol-zero/REDTEAM.md`) cover a related dead-drop/signaling stack. The
mappings that carry over and are already handled in Peerwire:

- **A1 sender forgery → handled.** Peerwire signaling uses the authenticated sealed
  form (`Noise_X`, sender PeerId bound), the equivalent of P0's
  `sealAuthenticated`. A forged/anonymous seal fails to open under a claimed
  identity (`sealed.rs` test).
- **A8 packet injection/replay → handled.** AEAD per data packet + the 2048 nonce
  window; nonce recorded only after authentication (SEC/THREAT items above).
- **A7 stale/replayed signaling → partially: SEC-6.** Peerwire has no in-band
  signaling-replay guard; carry P0's lesson (bind a fresh session id) into the fix.
- **A11 metadata / A12 forward secrecy → OPEN here too: SEC-2/3/5 and SEC-7.** The
  same open problems (relay-visible timing/IP, static-key exposure) apply; Peerwire's
  per-session Nostr key and rotating-tag ideas mirror P0's mitigations.
- **A5 spam / A13 Sybil → relevant to the signaling layer**; the half-open cap
  (SEC-4) and relay rate limiting are the Peerwire analogues. A PoW-gated first
  contact (as in P0 A5) is a candidate hardening if public rooms are abused.

## Residual risks accepted for v0.1

- Traffic analysis by a global passive adversary (packet sizes/timing) is not
  addressed.
- Metadata at the signaling layer (who talks to whom, when) is minimized but not
  hidden from a relay that correlates tags and timing.
- The security of the underlying crypto crates is assumed.

## Review method

Manual review of every parser and the handshake/relay/signaling state machines in
the commit at `cf9cbb0`, plus targeted Node reproduction for SEC-1. Continuous
coverage of the parsers is tracked under fuzzing (NB-014).

Later reviews (each change touching crypto, the wire format, relaying, signalling,
traversal or the FFI) read the diff against the threat model above and reproduce
each finding on loopback, in the simulator or with unit-level probes in a scratch
checkout. The probes stay out of the tree unless the fix adopts them as tests. The
hard-NAT review (2026-10-23) used a `punch_case` probe for NB44-1, a `Node` probe
for NB44-2 and a Chromium fleet probe for NB48-1.
