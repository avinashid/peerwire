# peerwire-sync

Shared documents: maps, sets and logs that every member of a room holds and that converge. It is `room.doc(name)` of
`@peerwire/js` ([`packages/js/src/doc.js`](../../packages/js/src/doc.js)) for Rust, Godot and C hosts, with the same
semantics, the same caps and budgets, and the same frames, byte for byte, so a copy or a frame made by one reads in
the other.

- **Map:** a key shows its latest write. A set made elsewhere at the same time wins over a delete (add-wins).
- **Set:** items added and deleted by id.
- **Log:** keeps its newest `keep` entries by Lamport stamp.
- **Catch-up:** members that join later, or come back after a partition, catch up. One member at a time answers,
  the best routed first, in pieces acknowledged as they land.

Like `peerwire-core`, it is sans-IO and deterministic: the host passes time and messages in, and drains messages,
events and the next timer.

```rust
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
        Event::Overflow(peer) => node.disconnect(&peer),    // its link took nothing for too long
    }
}
// and docs.handle_timeout(now) once docs.poll_timeout() has passed.
```

## On the wire

A doc message is `DOC_TAG` (7, room.js's `T.Doc`) followed by the frame's JSON, on the reliable lane. `Node::send`
carries raw app bytes, so an app that opens docs tags its own messages too, and hands `receive` every reliable
message: `receive` takes the ones that start with 7, and gives the rest back. The Godot binding does this itself
(see [docs/GODOT.md](../../docs/GODOT.md)). Deflated frames (tag 8) are never asked for: this version's asks don't
carry `z`, so no member deflates for it.

**The JSON is V8's.** doc.js checks the object `JSON.parse` gives it, so this crate reads and writes JSON as V8 does:
- strings are UTF-16, lone surrogates included;
- numbers past f64 are `null`, and `-0` is `0`;
- numbers are written with `Number::toString`'s digits;
- keys keep V8's order, and a key given twice keeps its last value;
- any depth is read (a frame whose *value* nests deeper than 64 is dropped, as doc.js drops it).

## Stars

A Godot room is a star: each member links to the host alone, and doc.js sends a write only to its own members.
Opened with `Options { hub: true }`, the host sends what each member's frame added on to its other members, as an
answer with no number. It never sends a frame back to its source.

## Memory

Each member's messages wait, in order, until its link takes them. Past `OUTBOX_BYTES` (16 MiB) the member is
forgotten as if it left (`Event::Overflow`), and the host should close its link. It catches up when it joins again.

## The copy

`DocMut::snapshot` gives the doc as doc.js keeps it on a device: answer frames and a clock, as JSON. The host keeps
it, and `Options::copy` starts from it when the doc opens again. Sealing it at rest and pacing the saves, as
`store.js` does, are NB-006g's.

## Tests

- [`tests/golden.rs`](tests/golden.rs) replays sessions of doc.js members recorded by
  [`packages/js/test/rust_doc.mjs`](../../packages/js/test/rust_doc.mjs):
  - The Rust members must send the same frames, byte for byte, and hold the same docs.
  - The session reaches every cap and budget: 4 Mi characters, 10,000 entries, 1,024 writers and 8,192 ranges, the
    chars budget run dry, claims on every dot.
  - doc.js applies the frames of a Rust session (`rust.json`).
- The frame and JSON corpora check that every frame and text is kept or dropped as doc.js and V8 keep or drop it.
- The fuzzers in `src/fuzz.rs` feed mutated frames, JSON and copies.
- `peerwire-sim/tests/sync.rs` checks convergence on simulated links:
  - loss, duplicates and jitter;
  - partitions and a late joiner;
  - frames in any order, any number of times;
  - a star with a hub and a stalled member.
