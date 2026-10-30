# Keeping a room's data while nobody is online (`peerwire-store`, NB-006g)

Status: design, 2026-10-30, revised after security's review (NB-006g-1 to -10). No code
yet. Measured on main `dbf0f7c`.

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
`relays: true` throws at open unless the roomId has `randomRoomId()`'s shape (at least
22 base64url characters): on a guessable roomId the relay copy is readable by anyone,
offline and for good (see Threats). Our mistakes throw, as elsewhere in `doc.js`.

**Keys.** All under the room key, with `e` the epoch as 4 bytes big-endian (0 for now),
`b` the bank and `i` the slot as one byte each, and `doc` as UTF-8 (a doc name with a
lone surrogate keeps no relay copy, as `store.js` already rules for the device copy):
- seal: `HMAC(room key, 'pw-snap-key' || doc || e)`;
- slot: `d = hex(HMAC(room key, 'pw-snap-id' || doc || e || b || i))`;
- signer: `pw_nostr_keypair(seed)`, `seed = HMAC(room key, 'pw-snap-sk' || doc || e)`.
  If the seed isn't a valid secret key (about 2^-128), use `HMAC(seed, 'pw-snap-sk')`
  and repeat: deterministic, so every member gets the same key. The wasm export takes
  a seed already; only its JS wrapper draws one at random.

The labels are prefix-free among themselves and against `pw-hello`, `pw-rtc-offer`,
`pw-link`, `pw-store-key`, `pw-store-id` and Rust's `pw-offer`; they join the label
lists in `proof.js` and in Rust. The epoch lets members later move the relay copy to
new keys and let the old slots expire, without a format change (its own card).
Every member signs with the same key: BIP-340 signatures with fresh aux bytes, so no
nonce is reused.

**Pieces and saves.** A save is the snapshot's frames, grouped so that each piece
deflates to at most 44 KB (and inflates to at most 1 MiB):
`{ v: 1, s, i, n, c, f }`: the save id `s` (8 random bytes, hex), piece `i` of `n`, the
Lamport clock `c`, the frames `f`. Deflated and sealed as the device copy is, with
`VERSION || d` as additional data: a piece opens only with the `d` of the slot it was
read from, and its `i` must equal that slot's.
- **Joining needs no manifest; writing does.** Each piece goes through `restore()` and
  `parseFrame(m, null)` as member data, so a mix of pieces holds no state a member
  couldn't send. But slot `i` of two saves holds unrelated entries, and the dead
  ranges sit in the last piece, so a reader that joins a save torn mid-write, then
  saves, publishes a doc missing whole pieces, or with deleted entries live again
  (security's loopback probe on `doc.js`: 194 of 470 entries lost; 20 of 20 deletions
  undone). So readers join only **complete** saves: all `n` pieces of one `s`,
  gathered from any relays. A complete save is a whole state, so joining every
  complete save found is the CRDT join and safe. Pieces of incomplete saves are skipped.
  Complete means every piece of `s` states the same `n`, with 1 ≤ `n` ≤ 8 and `i` < `n`,
  and all `n` are there; copies of one (`s`, `i`) from several relays may all be joined.
- **Two banks.** Slots come in two banks of 8 (`b` = 0, 1), 16 events per doc per relay
  at most. On each relay, a save writes the bank that doesn't hold **that relay's**
  newest complete save, so one complete save always stands there while the other is
  being written. Saves are ranked by `created_at` (the latest of their pieces' on that
  relay), never by `c`, which anyone with the roomId could set to 2^53 − 1 to pin the
  choice. So one save's pieces may sit in different banks on different relays; readers
  gather by `s`. An empty piece (`n: 0`, which no complete save can include) goes only
  into a slot of the bank being written that the read found holding another save's
  piece, so a one-piece save costs one event, not eight.
- **Cap.** 8 pieces per save (about 350 KB deflated, 1 to 2 MB of JSON for typical
  docs).
  A larger doc keeps no relay copy and warns once; its members still serve it.

**When to read.** On opening, alongside the device copy, from every relay:
`{ kinds: [30078], authors: [the signer], '#d': [the 16 slots] }`, closed at EOSE or
after a timeout. Bounds, since anyone with the roomId can write the slots and a relay
can send anything:
- keep only events of kind 30078 whose `pubkey` is the signer and whose `d` is a slot;
  at most one per slot per relay, the newest; skip an event id already read from
  another relay;
- refuse `content` over 60 KiB of base64 before decoding it (as `nostr.js` does with
  `MAX_CONTENT`); inflate with `room.js`'s streaming `inflate(bytes, max)` at 1 MiB per
  piece, and at most `MAX_STORED` over one read;
- a piece that doesn't open counts as absent; a piece of another `VERSION` means that
  bank on that relay is left as it is and never saved over, as `store.js` does. (A
  reader can't tell a real one from a relay that flipped the clear VERSION byte, so
  it is decided per relay: we write the other bank there, and stop on that relay only
  if both banks hold one.)
A read is **complete** on a relay when it reached EOSE within the timeout and no bound
cut it short (`MAX_STORED` included); a read that found only incomplete pieces found
"nothing". Pieces are joined with a distinct origin: their changes, deletions included, are
reported `from: 'relays'`, never as our own. `sync` gains a source: from a member, or
from relays as of the save's time (the saver's claim, for display only).

**When to save.** One saver per room at a time:
- After a change, wait for 10 s of quiet plus a random 0 to 30 s. The member whose
  timer fires first sends its members a small doc frame `{ d, t: 5, s }` ("saving").
  A `doc.js` or Rust member without the store drops `t: 5` as a malformed frame,
  whole, as it does today.
- A `t: 5` defers our save to at most 5 minutes after it first deferred it; later ones
  don't push it further, and we honour at most one per member per 5 minutes. When the
  deferral ends we read as usual: if that member's save `s` isn't complete on the
  relays that answered, we ignore its `t: 5`s until it joins again, and save. So a
  member that announces saves it never makes can't stop the repair of a wipe.
- A save starts only after a read that was complete, found a complete save or nothing,
  and joined it, whether the doc is `synced` or `'alone'` (so an author drawing alone
  saves). It goes only to relays whose read was complete, and only if the join left us
  something the newest complete save lacks, or that save expires within 7 days (so
  opening a doc daily keeps its copy alive).
- Pieces go one a second per relay through `nostr.js`'s per-relay buckets, holds and
  bans, behind signalling, so they never starve a join or a pipe.
- `created_at` is the later of now and the newest `created_at` read for that slot **on
  that relay**, plus 1, ignoring events more than 15 minutes past our clock: a clock
  that runs ahead (or a writer on purpose) can't pin a slot, and one relay serving a
  `created_at` a year ahead (`nostr.js` doesn't check signatures) can't fail our
  saves to the others. If an ignored event sits in a slot we must write, the save
  fails on that relay only. A refused write fails the save on that relay.
- At most one save per doc every 5 minutes per member, and only the last member online
  (`'alone'`) makes the best-effort save when the page hides.
- Every piece expires (NIP-40) 30 days after its save.

**Forgetting.** `forget()` also wipes the slots, best effort: an empty piece in each,
then a NIP-09 deletion. Its docs say that this wipes the room's relay copy for every
member until another member saves (today `forget()` touches only this device), and
that copies already archived may still be kept. The `relays` option's docs say "use
`randomRoomId()`": the shape check catches mistakes, not intent.

**Cost.** The whiteboard above: 3 pieces to each of 3 relays, 450 KB up per save,
about one save per room every 5 minutes while it changes; a read before each, and
150 KB down per relay for a joiner.

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

- **The roomId becomes a permanent, offline read key** (NB-006g-1). Relays serve stored
  events to anyone (a `REQ` for kind 30078 with no authors lists every snapshot), and
  mirrors and archives copy them; NIP-40 expiry and replacement bind only relays that
  honour them. So:
  - anything saved can be copied by anyone and kept; anyone who ever had the link can
    read it later, without joining the room, with no member online and no trace in
    the room; deleting from the doc doesn't remove old saves;
  - there is no revoking: the only way to cut off a past link-holder is a new roomId
    (or, later, a new epoch that old link-holders can't derive, which needs a key
    they don't have);
  - the signer's pubkey is a stored guessing oracle: anyone can compute it for a
    guessed (app, roomId, doc) and match it offline against what relays hold.
    Signalling's room tag is the same kind of oracle, but only on ephemeral events
    while members signal. Hence the random-roomId rule above.
  The `relays` option's docs (`index.d.ts`, README) say this plainly.
- **Who can write.** Anyone with the roomId can overwrite the slots, save frames that
  mark every dot dead, or delete the slots (NIP-09, with the shared signer): the
  doc's existing trust model, where members are trusted alike to write. Joining only
  complete saves, two banks, and a read-then-join before each save mean honest members
  restore what an overwrite removed from the relay, not what it deleted in the doc;
  the `t: 5` rule (NB-006g-8) keeps a member that announces saves it never makes from
  stopping that repair.
- **Availability is best effort** against anyone with the roomId: they can stop the
  relay copy for good, by putting another VERSION's pieces in both banks on every
  relay or by overwriting it over and over. Device copies stay the main store.
- **Rollback and withholding.** A relay can serve an old save or none. Joining is
  monotone, so an old complete save can't undo a newer one from another relay; a
  joiner that only reaches relays holding old saves sees an old doc until a member
  comes online. Reading every relay limits this, and `sync` says the copy's age.
- **Metadata.** Anyone who reads the relay sees one stable key and up to 16 slots per
  doc, piece sizes and save times; the relay also sees the saver's IP. They don't see
  the room, the doc's name or its contents. Signalling already shows a relay a stable
  tag per room and members' IPs (SEC-5 is the same for a peer's tag), so the new
  exposure is the edit rhythm and the doc's size. Padding pieces to 60 KB would hide
  the size for 2x the bytes, and is left off.
- **Reading is bounded** (NB-006g-3): a piece that inflates 1000x, a 1 MB event, or a
  relay that ignores the filter, all stop at the bounds under "When to read".
- **Relay abuse** (NB-006g-4). 16 events of 60 KB per doc per relay, about one save per
  room every 5 minutes, through signalling's per-relay buckets. A relay that refuses
  for its rate limit pauses as signalling does (NB-006h); a ban costs that relay for
  signalling too, for every device behind the same IP.
- **`created_at` pinning** (NB-006g-5) and **one relay deciding for all** (NB-006g-9):
  handled by the per-relay rules for the bank, `created_at` and VERSION above.
- **Retention isn't promised.** A doc nobody opens for 30 days loses its relay copy by
  design, and a relay may drop it sooner. Device copies are the primary store.

## Not chosen

- **A user-run always-on member** (NB-015, Docker): the most reliable holder, and it
  fits "no server of the developer's own" when the user runs it. It complements route
  1 rather than replacing it.
- **Mainline DHT (BEP 44)**: 1000-byte values, too small for a doc.
- **One fresh key per save**: unlinkable saves, but events never replace each other, so
  a relay would keep every save and serve old ones alongside.
- **Warning, not refusing, on a guessable roomId**: a console warning is easy to miss,
  and what it warns of can't be undone.

## Next cards

1. **NB-006l: relay snapshot of `room.doc` in JS**, as above, with security's NB-006g-3
   to -10 as requirements: security review (parsing and the threats above); tests
   against a local strfry-like fake relay (torn saves, both banks, a relay that took
   only part of a save, a bomb piece, a relay that ignores the filter, one serving
   `created_at` a year ahead, another VERSION in one bank, a member sending `t: 5`
   without saving), plus one minimal live check on our own test room.
2. **NB-006b** (in progress) reads and writes the same pieces in Rust, and drops `t: 5`
   until then.
3. **NB-019 rescoped**: one-to-one offline messages to a PeerId over relays.
4. **Epoch rotation**: moving a relay copy to a new epoch, its own card, later.
5. Re-read the retention probes at +30 days (2026-11-16).
