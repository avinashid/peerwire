# Keeping a room's data while nobody is online (`peerwire-store`, NB-006g)

Status: design, 2026-10-30. No code yet. Measured on main `dbf0f7c`.

## The gap

`room.doc(name, { persist })` (NB-006f) keeps a whole copy of a doc on every member's
device, and a member that comes back serves it to the others (NB-006e). So a doc is
lost only while **no member that holds it is online**. Two cases are left:

1. **A new device joins an empty room.** Someone shares a whiteboard link, its author
   closes the tab, a friend opens the link: the board is empty until the author comes
   back. This is the case users see.
2. **Edits outlive their writer's session.** A edits while B is away, A leaves, B comes
   back: B sees the doc as it was until A is online again.

Both need a holder that is online when the members aren't. With no server of our own,
that holder is a public Nostr relay (route 1). Route 2, pieces spread over the members,
can't fill the gap: its holders are the members, offline at exactly that moment.

## Measurements

**Snapshot size.** `Doc.snapshot()` (the frames the device copy already keeps), then
deflate-raw, the AES-256-GCM seal (1 + 12 + 16 bytes), then base64 for a Nostr event's
`content`. Measured in Node with `doc.js` at `dbf0f7c`, four writers:

| doc | JSON | deflated | on a relay | 60 KB pieces |
|---|---|---|---|---|
| whiteboard: 2000 strokes of 20 points, 30% erased (1374 live) | 357 KB | 111 KB | 148 KB | 3 |
| chat: a log keeping 1000 of 3000 messages | 128 KB | 20 KB | 27 KB | 1 |
| worst case: 60 random 45 KB values | 3.5 MB | 2.6 MB | 3.5 MB | 59 |

Deflate took 4 to 94 ms and the seal under 5 ms. The device copy today is sealed but
not deflated; deflate takes a typical doc to 16 to 31% of its JSON.

**Relays.** The three in `DEFAULT_RELAYS` (nos.lol, relay.primal.net, nostr.mom) all run
strfry 1.1.3 and list NIP-40 (expiration). None publishes a retention policy in NIP-11.
`max_message_length` is 128 KiB on nos.lol and nostr.mom and 1 MB on primal.
- **Size.** A kind-30078 event of 60 KB was taken by nos.lol and nostr.mom, and one of
  100 KB refused by both ("invalid: event too large: 102846"): strfry's default event
  limit of 64 KiB. So a piece carries at most 60 KB of base64, 44 KB sealed.
- **Retention.** The two kind-30078 probes Protocol Zero wrote on 2026-10-17 (nos.lol,
  relay.primal.net; damus and nostr.band refused theirs) were both still served at
  305.5 h (12.7 days), read back on 2026-10-30. Two samples: re-check at +30 days.
- **Traffic.** Four test events in all (60 and 100 KB, two relays), random content, a
  throwaway key, expiring after 2 days, and two reads of the old probes. No rate-limit
  probing: a relay that sees a burst may ban the IP (damus bans this host).

**Erasure code.** The p0 PoC's pure-TypeScript GF(2^8) code (`erasure.ts`), decoding
from parity shards (its slowest case), on this host:

| data | k of n | shard | stored | encode | decode |
|---|---|---|---|---|---|
| 27 KB (chat) | 4 of 8 | 6.8 KB | 2.0x | 10 ms | 5 ms |
| 148 KB (whiteboard) | 4 of 8 | 37 KB | 2.0x | 55 ms | 45 ms |
| 148 KB | 8 of 12 | 18.5 KB | 1.5x | 53 ms | 174 ms |
| 1 MB | 8 of 12 | 128 KB | 1.5x | 423 ms | 693 ms |

## Recommendation

**Route 1: a sealed snapshot of the doc on the relays the room already signals over.**
Opt-in, `room.doc(name, { persist, relays: true })`, JS first, then Rust (NB-006b).

- **Pieces.** The snapshot's frames, grouped so that each piece deflates to at most
  44 KB: `{ v: 1, i, n, c, f }` (piece `i` of `n`, the Lamport clock `c`, frames `f`),
  deflated and sealed as the device copy is, under its own key
  `HMAC(room key, 'pw-snap-key' || doc)`, with `VERSION || d` as additional data.
- **Pieces need no manifest.** Each frame is a piece of state joined into the doc
  (delta-state CRDT), applied in any order, any number of times. A reader that gets
  piece 1 of one save and piece 2 of the next holds a valid doc, missing at most what
  the absent piece held, which the next member online sends. That is how catch-up
  already applies an answer's pieces as they land.
- **Addressing.** Kind 30078 (NIP-78 app data, addressable), one slot per piece:
  `d = hex(HMAC(room key, 'pw-snap-id' || doc || i))`, signed with the key
  `pw_nostr_keypair(HMAC(room key, 'pw-snap-sk' || doc))`: the wasm export takes a
  seed already, only its JS wrapper draws one at random. Every member derives the same
  key, so a save replaces the last one slot by slot and a doc
  takes at most `n` events on a relay. Readers ask each relay once:
  `{ kinds: [30078], authors: [that key], '#d': [slots 0..15] }`.
- **Cap.** 16 pieces (about 700 KB deflated, about 2 to 4 MB of JSON for typical docs).
  A larger doc keeps no relay copy and warns once; its members still serve it.
- **When to save.** Only once the doc is `synced`. After a change, wait for 10 s of
  quiet plus a random 0 to 30 s, then read the relay copy, join it, and write only if
  that added something of ours: at most one save per doc every 5 minutes per member,
  plus a best-effort save when the page hides. Pieces go one a second per relay, and
  slots freed by a smaller doc get an empty piece. Every piece expires (NIP-40) 30
  days after its save, and each save renews it.
- **When to read.** On opening, alongside the device copy, from every relay. Each
  piece is checked as member data and joined; `sync` gains a source: from a member,
  or from relays as of the snapshot's save time.
- **Cost.** The whiteboard above: 3 pieces to each of 3 relays, 450 KB up per save at
  most once per 5 minutes while it changes, and 150 KB down per relay for a joiner.

**Route 2 isn't for docs.** Its holders are offline exactly when they're needed; a full
copy on each member (NB-006f) already serves the doc from any one member, where k of n
needs k of them; and on relays, a 3-piece doc copied whole to 3 relays costs 3x of
about 150 KB, which erasure coding would cut to 1.5x at the price of 45 to 700 ms per
decode in TypeScript and a cliff below k relays. Keep it for large payloads later.

**NB-019 (offline delivery), scoped down.** Chat in a room is a `doc.log`, so with
route 1 a member coming back gets the messages sent while it was away, from the
relays or any member. What is left for NB-019 is a message to one peer outside a
room, to its PeerId: sealed to the recipient and left on relays (Protocol Zero's dead
drop, proven live), with erasure only for payloads over one piece.

## Threats

- **Who can read and write.** Anyone with the roomId, as for the room and the device
  copy: a guessable roomId keeps nothing secret. Others see random bytes and can't
  sign as the slot key. A member, or anyone who ever knew the roomId, can overwrite
  the slots or save frames that mark every dot dead: the doc's existing trust model
  (members are trusted alike to write). Read-then-join before each save means honest
  members restore what an overwrite removed from the relay, not what it deleted in
  the doc.
- **Rollback and withholding.** A relay can serve an old save or none. Joining
  is monotone, so an old piece can't undo a newer one from another relay; a joiner
  that only reaches relays holding old saves sees an old doc until a member comes
  online. Reading every relay limits this, and `sync` says the copy's age.
- **Metadata.** A relay sees one stable key and up to 16 slots per doc, piece sizes,
  save times and the saver's IP. It doesn't see the room, the doc's name or contents.
  Signalling already shows a relay a stable tag per room and members' IPs (SEC-5 is the
  same for a peer's tag), so the new exposure is the edit rhythm and the doc's size.
  Padding pieces to 60 KB would hide the size for 2x the bytes, and is left off.
- **Relay abuse.** Bounded by the cap: 16 events of 60 KB per doc per relay, at most
  one save per member per 5 minutes. A relay that refuses for its rate limit pauses as
  signalling does (NB-006h); a ban costs that relay for signalling too.
- **Retention isn't promised.** A doc untouched for 30 days loses its relay copy by
  design, and a relay may drop it sooner. Device copies are the primary store.

## Not chosen

- **A user-run always-on member** (NB-015, Docker): the most reliable holder, and it
  fits "no server of the developer's own" when the user runs it. It complements route
  1 rather than replacing it.
- **Mainline DHT (BEP 44)**: 1000-byte values, too small for a doc.
- **One fresh key per save**: unlinkable saves, but events never replace each other, so
  a relay would keep every save and serve old ones alongside.

## Next cards

1. **NB-006l: relay snapshot of `room.doc` in JS**, as above: backend; security review
   (parsing and the threats above); a test against a local strfry-like fake relay, plus
   one minimal live check on our own test room.
2. **NB-006b** (in progress) reads and writes the same pieces in Rust.
3. **NB-019 rescoped**: one-to-one offline messages to a PeerId over relays.
4. Re-read the retention probes at +30 days (2026-11-16).
