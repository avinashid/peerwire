# Shared documents

A shared document is state that every member of a room holds: maps, sets of items and logs. Writes reach every
member at once; members who join later, or come back after a partition, catch up; and members that have heard the
same writes hold the same document, whatever order they heard them in. It suits a whiteboard, a chat history, a
to-do list or a game lobby's settings.

It is `room.doc()` in JavaScript, [`peerwire-sync`](../../crates/peerwire-sync/README.md) in Rust, and `open_doc` in
[Godot](godot.md#shared-documents). All three have the same maps, sets and logs, the same limits and the same frames,
byte for byte. The API is in the [JavaScript reference](../reference/javascript.md#doc-1).

## Open a doc

```js
import { joinRoom } from '@peerwire/js';

const room = await joinRoom({ appId: 'my-app', roomId: 'doc-demo' }); // open it in two tabs
const doc = room.doc('board');                 // the same name on every member is the same document
doc.on('change', ({ name, key, id, value, deleted, from, local }) => console.log(name, key ?? id, value));
doc.map('meta').set('title', 'Plans');         // every member sees it
```

A doc holds three kinds of collection, each by name:

```js
const meta = doc.map('meta');                    // one value per key
meta.set('title', 'Plans');                      // true; false, with a warning, if members have filled the doc
meta.get('title'); meta.has('title'); meta.delete('title');

const strokes = doc.set('strokes');              // items, each with its own id
const id = strokes.add({ points: [1, 2, 3, 4], color: 'red' }); // the id; null if the doc is full
strokes.delete(id);                              // undo: delete your item

const chat = doc.log('messages', { keep: 500 }); // append-only, keeps the newest `keep` (1000 if not given)
chat.append({ text: 'hi' });

meta.entries();                                  // [{ key, value, from }], by key
strokes.entries(); chat.entries();               // [{ id, value, from }], oldest first, one order on every member
doc.close();                                     // room.leave() closes the room's docs too
```

Values are JSON and come back frozen. `'change'` fires once for each key or item a write or a member's frame
changed, with `local: true` for our own writes. Give a log the same `keep` on every member, and call `doc.log()`
right after `room.doc()`: a member that keeps fewer entries never trims another's log, but one that keeps more can
miss the older entries another has trimmed, once it catches up from that member.

## What wins

- A `set` replaces the values its writer has seen.
- Two values set at the same time, where neither writer had seen the other's, are ordered by Lamport stamp and then
  by writer, the same on every member.
- A `delete` removes the values its writer has seen, so a set made elsewhere at the same time survives it (add-wins,
  as in Yjs and Automerge).
- Set items and log entries sort by Lamport stamp, then writer. A write made after seeing another sorts after it,
  which gives a whiteboard one z-order and a chat one order. No wall clock is involved.

## Show where catching up stands

A late joiner sees the doc fill in, newest entries first. `doc.sync` says where it stands, and `'sync'` fires when it
changes:

```js
const show = ({ state, from, have, total }) => {
  const percent = total ? Math.round((100 * have) / total) : 0;
  if (state === 'alone') line.textContent = doc.synced ? "You're the last one here" : "You're the only one here";
  else if (state === 'loading') line.textContent = `Loading from ${from.slice(0, 6)}… ${percent}%`;
  else line.textContent = '';
};
show(doc.sync);
doc.on('sync', show);
```

- `'alone'`: no other member to catch up from.
- `'loading'`: catching up from member `from`, which has sent `have` of the `total` entries its answer carries (0 of 0
  until its first piece lands). `have` may stop short of `total`: the last piece ends the answer.
- `'synced'`: caught up. An answer ended from a member that had caught up itself, or every member has answered (none
  had: members that open the doc together hold all there is between them).

`doc.synced` is true once caught up, and stays true, also once the others have gone (`'alone'`). So an app can tell
"you're the last one with the board" from "you're the first one here". `doc.sync` is as good as the members that
told us: a member can say it had caught up when it hadn't, or carry fewer entries than it says. Use it for a status
line, not to decide what the doc holds.

## How catching up works

- **Writes** go to every member as deltas on the reliable lane: the new entry and the ids of the values it replaces.
- **Asks.** A doc asks every member when it opens, and each member that joins. The ask carries its context: for each
  writer, the ranges of writes it has seen. It asks one member at a time, so a joiner is sent each entry about once
  (at most 1.00× the entries it holds, in `packages/js/test/doc_crdt.mjs`), not once per member.
- **Order.** Members are asked in the order of their routes (`room.route()`): linked directly, then relayed by a
  member, then over signalling. When one linked directly joins, or a member's route becomes direct, while an answer
  comes over signalling, that member is asked at once, and the one over signalling again after it, for what is still
  missing.
- **Pieces.** An answer comes in pieces, the newest entries first (the top of a whiteboard), then the writes it
  knows are deleted. A piece is about 32 Ki characters of JSON over signalling or through a member, and 128 Ki linked
  directly. Each piece is applied as it lands and acknowledged, and at most 4 go unacknowledged, so an answer keeps
  the pace of its route, and live writes don't queue behind the whole doc.
- **Slow answers.** The next member is asked once an answer has gone 5 s without a piece, or has taken 60 s while
  another member waits. The slow answer goes on, and the next ask carries what its pieces brought.
- **Compression.** Doc frames of 1 KiB or more go deflated (where the platform has `CompressionStream`) to members
  whose doc reads them. A whiteboard of 40 strokes of 1000 points, 508 K characters of JSON, reaches a late joiner in
  about 200 KB, 2.5 times less (`packages/js/test/room_doc.mjs`).
- **Time.** In `packages/js/test/doc_catchup.mjs`, over the signalling pipe through relays that take 10 events a
  second, a late joiner holds that whiteboard within 15 s of linking and its first strokes within 3 s (about 10 s
  and 0.2 s on a loaded 4-core host), and within 30 s and 5 s when the member it catches up from is a background tab.
- **Budgets.** A member is answered 8 times at once, then once per 15 s, and our asks of it go at the same rate. What
  the answers carry is capped too: 8 Mi characters of JSON text at once (two full docs), then 64 Ki a second. An ask
  past that waits rather than being dropped, so a doc closed and opened again (a component that remounts) catches up
  every time: a small doc 8 times in a row at once, then each up to 15 s late.
- A member that doesn't have the doc open answers its asks with nothing, so it holds no one up. Doc frames have their
  own tags, so they never reach `'message'` or `poll()`.

## Keep a copy on the device

```js
const doc = room.doc('board', { persist: true }); // IndexedDB in a browser
await doc.loaded;                                  // 'loaded', 'empty', 'blocked' (storage refused) or 'off'
await doc.forget();                                // deletes this device's copy, e.g. on sign-out
```

With `persist`, the doc is back when that member comes back, even with nobody else online. Opening loads the copy
before members are asked, so the ask carries it and members send only what it lacks. A member who was never there
gets the doc from one that kept it, once that one is online: nothing holds a room's docs while nobody is in it (for
that, see the [relays](#keep-a-copy-on-the-relays)).

In Node, or to choose the storage, pass a store of bytes: `{ get(key), set(key, bytes), delete(key) }`, each
returning a promise. `memoryStore()` keeps copies in the process. Files take a few lines (keys are 64 hex
characters). Make the store once and pass the same object each time: a doc closed and opened again at once loads
what the closing one saved only through the same store.

```js
import { readFile, writeFile, rename, rm } from 'node:fs/promises';

const files = (dir) => ({
  get: (key) => readFile(`${dir}/${key}`).then((b) => new Uint8Array(b), (e) => {
    if (e.code !== 'ENOENT') throw e; // unreadable isn't absent
  }),
  // Whole or not at all: a crash mid-write leaves the last copy.
  set: (key, bytes) => writeFile(`${dir}/${key}.tmp`, bytes).then(() => rename(`${dir}/${key}.tmp`, `${dir}/${key}`)),
  delete: (key) => rm(`${dir}/${key}`, { force: true }),
});
const store = files('./docs');
const doc = room.doc('board', { persist: store });
```

- **When it saves.** When the page is hidden, on `doc.close()` and `room.leave()` (whose promises resolve once it
  is), and while it changes: a second after a change, one save at a time. Each save writes the whole doc, so the next
  waits for the last one's bytes at 256 KiB/s: 2 s for the whiteboard above, 48 s for a full doc. A page killed while
  visible can lose its last few seconds of writes, more for a large doc; the members that heard them still have them.
- **Size.** A copy is about the size of the doc's JSON: 510 KB for the whiteboard above
  (`packages/js/test/doc_catchup.mjs`), and 12.1 MiB at most for a full doc of 3-byte characters
  (`packages/js/test/doc_crdt.mjs`).
- **Storage that refuses** (some private modes, a full disk) gives `'blocked'` or a failed save: one warning, and the
  doc goes on in memory. A failed save is tried again with the next change, a second after the failure, then 2, 4 …
  up to 64 s apart while the store keeps failing; hiding the page, `close()` and `leave()` still save at once. A store
  that hasn't loaded the copy 5 s after opening is `'blocked'` too. `'blocked'` leaves the copy as it is: that
  opening never writes it (`forget()` still deletes it). A copy that can't be read counts as none, with a warning
  saying why, and the next save replaces it. A copy saved by another (newer) version of the package is left as it is.
- **Two tabs** of one browser in one room keep one copy of each doc, whichever saved last. Each catches up from the
  other while both are open.
- **Sealed.** The copy is sealed with AES-256-GCM under a key derived from the room key, and kept under an id derived
  the same way, so the storage shows neither the app, the room, the doc nor what it holds. With `persist`, a doc name
  must be well-formed (no lone surrogate, else `RangeError`), as two such names would share a key.
- **As secret as the roomId.** An app that keeps the roomId in the URL (history, bookmarks) or in `localStorage` gives
  it to anyone who can read the device's browser profile, and with it the copy. The seal protects a store taken on its
  own: a copied IndexedDB, a file from a file store, a backup. Any member can make a copy that opens, so a copy is
  checked as member data. A copy's id is the same on every device for one app, room and doc. Its size and the time of
  its last save show. Someone who can write the storage can put an older copy back; the doc then catches up from the
  members, and never writes again as the writer the copy names, so no write is made twice. `doc.forget()` deletes
  the record, but storage can keep old bytes on disk or in backups.

## Keep a copy on the relays

```js
const room = await joinRoom({ appId: 'my-app', roomId, signaling: 'nostr' }); // roomId from randomRoomId(), shared
const doc = room.doc('board', { persist: true, relays: true });
doc.on('sync', ({ source, at }) => {
  if (source === 'relays') line.textContent = `As saved at ${new Date(at).toLocaleTimeString()}`;
});
```

With `relays: true`, the doc keeps a sealed copy on the room's Nostr relays, so someone who opens it while no member
is online still gets it: from the relays, then live from members as ever. Read this first:

- **Anyone with the roomId can read this copy, for good, and nobody sees it.** Relays serve stored events to anyone,
  and mirrors and archives copy them, whatever expiry or deletion asks. So anyone who ever had the link can read the
  doc later, without joining the room, with no member online, and deleting from the doc doesn't remove old saves.
  There is no revoking a link: only a new roomId cuts off someone who once had it. The copy's key is also a stored
  test for a guessed roomId. So `relays: true` throws `RangeError` unless the roomId has `randomRoomId()`'s shape (22
  or more base64url characters): **use `randomRoomId()`**. The check catches mistakes, not a guessable roomId of that
  shape.
- **Anyone with the roomId can also write it:** overwrite it, mark everything deleted, or wipe it, as members can the
  doc itself. Members put back what an overwrite took off the relays (not what it deleted in the doc) at their next
  save, but someone set on it can keep the relay copy away for good. It is best effort: members' devices (`persist`)
  are the main store.
- **What the relays hold:** up to 16 events per doc on each (two banks of 8 slots), kind 30078, all signed by one key
  derived from the room key, each a piece of the doc deflated and sealed with AES-256-GCM. A relay sees one stable
  key per doc, up to 16 slots, the pieces' sizes and save times, and the IP of each member that saves or reads it. Not
  the room, the doc's name or what it holds.
- **Reading:** opening reads every relay (until its EOSE, 10 s at most) beside the device copy, and joins every save
  found whole; a save torn mid-write is skipped. What it brings is checked as member data, and a deletion it brings
  says `from: 'relays'`. A read is bounded whatever a relay sends: 60 Ki characters of content an event, a piece
  inflated to 1 MiB at most, and a whole read to 32 MiB.
- **Saving:** one member at a time, behind signalling. A while after the doc changes (10 s without a change, then a
  random 0 to 30 s, at most once in 5 minutes), a member reads, joins what it finds, and writes only to the relays
  whose copy lacks something it holds or expires within 7 days. Each relay keeps its newest whole save while the next
  goes into its other bank, the pieces one a second. A save no relay took goes again 5 minutes on. The last member
  online also saves when the page hides (at most once a minute); from a closing tab that rarely finishes.
- **Size:** at most 8 pieces of 44 KiB deflated each; a whiteboard of 1400 strokes of 20 points takes 4
  (`packages/js/test/doc_relays.mjs`). A larger doc keeps no relay copy, with one warning; its members still serve it.
- **Retention:** each piece expires (NIP-40) 30 days after its save, and opening the doc within a week of that saves
  it again. Relays promise nothing: one may drop it sooner.
- **`doc.forget()`** also wipes the relay copy, best effort, for every member until another one saves: an empty piece
  in each slot, then a NIP-09 deletion. Copies a relay or archive already made may stay.
- It needs `signaling: 'nostr'` (`TypeError` otherwise) and `CompressionStream('deflate-raw')`. Members without
  `relays: true` drop the frame that says a member is saving.

The design behind it is in [Offline store design](../project/offline-store-design.md).

## Limits

| | |
|---|---|
| A value | 64 KiB as JSON text, nested at most 64 deep |
| A doc | 10,000 entries and 4 MiB of JSON text |
| Names and keys | names 1 to 128 characters, keys 256 |
| A log's `keep` | 1 to 10,000 (default 1000) |

A value or key over its limit, a value that isn't JSON, or a write after `close()` throws: these are the app's own
mistakes. A doc that members have filled refuses writes with a warning (`false` or `null`), as every member refuses
a write that doesn't fit. Deleting frees room. Every frame stays under 512 KiB.

## Trusting members

Members are trusted alike, as in the room itself: any member can overwrite, delete or fill a doc. What a member sends
is checked and capped before it reaches you, and a malformed frame is dropped whole. Beyond that:

- `from` is the writer as the doc was told. A member relaying a catch-up can name any writer, so use `from` to colour
  or group, not as proof of who wrote something. Check every field you render, as with messages.
- A member can make the room ignore another member's writes, by claiming it has seen them before they are written. A
  doc that hears such a claim on its own writes moves on to new ones, 16 times; past that, its writes are lost at the
  members that took a claim it hasn't heard, and once every write is claimed they return `false` or `null` with a
  warning, until the doc is closed and opened again.
- Every member takes a Lamport stamp however far ahead, as refusing one would split the room. So a member can move
  the stamps to 2^53 - 1 at once; from then on, values set at the same time, and set items and log entries, are
  ordered by writer, and a log keeps the highest writers' entries. A later set still wins.
- A member can ask for the whole doc as often as it likes. What it is sent, and what we pass on for it, is at most 8
  Mi characters of JSON text at once and 64 Ki a second after (theoretical, from those budgets; UTF-8 takes 1 to 3
  bytes a character). The budgets are per member while it stays: one that leaves and joins again, or joins under a
  new id, starts with whole ones.

The full analysis is the DOC section of the [threat model](../project/threat-model.md).

## In Rust

[`peerwire-sync`](../../crates/peerwire-sync/README.md) is the same document for Rust, sans-IO like the core: the host
passes time and messages in, and drains messages, events and the next timer. Beside a `peerwire::Node`:

```rust
# use peerwire::{Lane, Node, SendError};
# use peerwire_sync::{Instant, PeerId, State};
# fn render(_: String, _: String, _: Option<&str>) {}
# fn status(_: State) {}
# let mut node = Node::builder().bind("127.0.0.1:0")?;
# let (me, peer, seed, now) = (PeerId([1; 32]), PeerId([2; 32]), [7; 32], Instant::from_millis(0));
# let message = b"\x01app bytes".to_vec();
use peerwire_sync::{Docs, Event, Options, Route, DOC_TAG};

let mut docs = Docs::new(me, seed);                        // seed: 32 bytes from the OS
let board = docs.open(now, "board", Options::default())?;  // the same name on every member is the same doc
docs.joined(now, peer, Route::Direct);                     // a member joined the room
docs.doc(board).unwrap().map_set("meta", "title", r#""Plans""#)?;  // values are JSON text

// The host loop, beside the node's:
if !docs.receive(now, peer, &message) { /* not a doc message: the app's */ }
while let Some((peer, m)) = docs.poll_transmit() {
    if node.send(&peer, Lane::Reliable, &m) == Err(SendError::WouldBlock) {
        docs.retry(peer, m);                               // it goes first next time
    }
}
while let Some(e) = docs.poll_event() {
    match e {
        Event::Change(doc, c) => render(c.collection(), c.key(), c.value()),
        Event::Sync(doc, s) => status(s.state),             // Alone, Loading or Synced
        Event::Overflow(peer) => node.disconnect(&peer),    // its link stalled, or it flooded a hub
    }
}
// and docs.handle_timeout(now) once docs.poll_timeout() has passed.
# Ok::<(), Box<dyn std::error::Error>>(())
```

- A doc message is `DOC_TAG` (7) followed by the frame's JSON, on the reliable lane. `Node::send` carries raw app
  bytes, so an app that opens docs tags its own messages too, and hands `receive` every reliable message: `receive`
  takes the ones that start with 7, and gives the rest back.
- `docs.joined`, `docs.rerouted` and `docs.left` tell it about members and their routes (`Route::Direct`, `Peer`,
  `Signaling`), which set the order it asks them in.
- **Keeping a copy:** `DocMut::snapshot()` gives the doc as `doc.js` keeps it on a device. Keep it, and open the doc
  with `Options { copy: Some(&bytes), ..Default::default() }` next time: members send only what it lacks. Sealing it
  at rest is the host's job. The crate keeps no copy on the relays.
- **Stars:** opened with `Options { hub: true }`, a member passes what each member's frame added on to the others,
  for rooms where members link to one host alone (a Godot room). Each member's messages wait until its link takes
  them, up to `OUTBOX_BYTES` (16 MiB); past that it is caught up afresh, and `Event::Overflow` says when to close a
  link that has taken nothing for 30 s.
- Writes return `Result`: `Error::Name`, `Key`, `Json`, `Value`, `Keep` or `Copy` where `doc.js` throws, and
  `Ok(false)` or `Ok(None)` where it returns `false` or `null`.

Its README has the wire format, the JSON rules it shares with V8, and the memory bounds. The crate's types are in the
[Rust reference](../reference/rust.md).

## In Godot

`PeerwireMultiplayerPeer.open_doc(name)` opens the same document in a Godot room, with signals for changes and sync,
and `snapshot()` / `open_doc_from(name, copy)` to keep a copy: see [Godot: shared documents](godot.md#shared-documents).
A Godot room is a star around its host, which passes on what each joiner writes. Browsers and Godot don't share a doc
live, as a browser doesn't dial a native peer, but a copy or a frame made by one reads in the other.

## Not yet

Collaborative text (character sequences), copies on the relays from Rust and Godot, and moving a relay copy to new
keys (to cut off old links without a new roomId).
