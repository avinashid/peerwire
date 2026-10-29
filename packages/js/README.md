# @peerwire/js

Peer-to-peer rooms for the browser and Node, with no server of our own:
peerwire-core compiled to a WebAssembly module with no imports
(`crates/peerwire-ffi`), a small JS host, WebRTC DataChannels as the datagram
transport and Nostr relays (or a BroadcastChannel) to find peers. No wasm-bindgen.

```
./packages/js/build.sh                         # -> packages/js/pkg/peerwire_ffi.wasm (~300 KB)
python3 -m http.server -d packages/js 8000     # any static server
```

## A chat in ten lines

<!-- readme-chat: test/readme.mjs runs this block in two browser tabs -->
```html
<ul id="log"></ul>
<form id="chat"><input id="text" autocomplete="off"><button>Send</button></form>
<script type="module">
  import { joinRoom } from '@peerwire/js';
  const room = await joinRoom({ appId: 'readme-chat', roomId: 'lobby' }); // signaling: 'nostr' to reach other machines
  const show = (who, text) => log.append(Object.assign(document.createElement('li'), { textContent: `${who}: ${text}` }));
  room.on('peer-join', (peer) => show(peer.slice(0, 6), 'joined'));
  room.on('message', (text, peer) => show(peer.slice(0, 6), text));
  chat.onsubmit = (e) => {
    e.preventDefault(); room.send(text.value);
    show('me', text.value); chat.reset();
  };
</script>
```

Open it in two tabs. With a bundler (Vite, webpack, esbuild) `@peerwire/js` resolves
from npm and the wasm is found next to the package. Without one, map it in the
page: `<script type="importmap">{"imports":{"@peerwire/js":"/path/to/packages/js/src/index.js"}}</script>`.

## joinRoom

```js
const room = await joinRoom({
  appId: 'my-app',          // rooms of different apps never meet
  roomId: randomRoomId(),   // the room's secret: share it to invite (see Closed rooms)
  signaling: 'local',       // 'local' (tabs of one browser, one Node process) | 'nostr' | custom factory
  presence: { name: 'Ana' }, // shared with every peer when it joins
  privacy: 'direct',        // or 'public-only': no LAN addresses in ICE (see Privacy below)
  fallback: 'auto',         // peers WebRTC can't link: 'auto' | 'peers' | 'off' (see Through any NAT)
});

room.on('peer-join', (peer, presence) => {});
room.on('peer-leave', (peer, reason) => {});    // 'left', 'closed', 'timeout', ...
room.on('presence', (peer, presence) => {});    // a peer called setPresence
room.on('message', (data, peer, { lane }) => {});

room.send('hi');                                // every joined peer
room.send({ x: 1, y: 2 }, { lane: 'unreliable' }); // latest-wins, one datagram at most
room.send(bytes, { to: peer });                 // one peer, or an array of them
room.setPresence({ name: 'Ana', typing: true });
room.peers; room.self; room.getPresence(peer); room.rtt(peer);
room.route(peer);                               // { kind: 'direct' (adopted?) | 'peer' (via) | 'signaling', rttMs }
room.retryDirect();                             // try WebRTC again with peers on a fallback (see Through any NAT)
const doc = room.doc('board');                  // a document every member holds (see Shared documents)
await room.leave();                             // peers see 'peer-leave' at once
```

- **Data:** strings arrive as strings, `Uint8Array`/`ArrayBuffer` as `Uint8Array`,
  anything else as JSON.
- **Lanes:** `reliable` (default: ordered, retransmitted), `unreliable` (no
  retransmits, for positions and cursors), `urgent` (reliable, each fragment sent
  twice, ordered only among urgent messages so it overtakes queued `reliable`
  data; up to 64 KiB encoded, and `send` throws a `RangeError` above that).
  Reliable sends past the send window queue up instead of failing.
- **Presence** travels over the encrypted links, not through signalling. A peer
  counts as joined once its presence has arrived, so `peer-join` always has it.
  A `peer-join` handler can send to the newcomer, to hand it the room's state, say:
  `room.send(state, { to: peer })` arrives after our own presence, so the newcomer
  already lists us. Send it on the `reliable` lane (the default): a peer drops what a
  member sends before its presence, and an `urgent` message can overtake it.
- **Closed rooms:** the roomId is the room's secret. Relays see only a hash of it
  (the topic); every hello, WebRTC offer and link carries an HMAC under a key
  derived from it (`sha256("peerwire/room-key/<appId>/<roomId>")`, as in Rust). A peer
  is dialled only on a hello that proves the key, offers that don't prove it get no
  answer (and so none of your addresses), and a link that doesn't prove it within
  10 s is dropped without seeing presence or messages. A name anyone can guess
  (`'lobby'`) makes a public lobby; for a closed room use at least 128 random bits:
  `randomRoomId()`. Members are trusted alike: anyone given the roomId can join.
- **Signalling:** `'nostr'` publishes ephemeral events to `wss://nos.lol`,
  `wss://relay.primal.net` and `wss://nostr.mom` (override with `relays`) and uses Google and Cloudflare STUN
  (override with `iceServers`). Relays see the room topic and the PeerIds in
  hellos, not the roomId; offers are sealed to the recipient. Each side sends one
  offer or answer with its candidates, once ICE gathering is complete or 300 ms
  after the first public (STUN) candidate (2 s at most), and any candidates found
  later in one `ice-batch`. Relays drop events, so an offer with no answer is sent
  again at 3 s and 6 s, and a repeated offer gets the same answer again.
  `transport.trace` reports each step (Net Lab shows it).
- **Node:** Node has no WebRTC. `signaling: 'local'` rooms then carry their
  datagrams over the BroadcastChannel (peers in one process or its workers). For
  real networks pass `RTCPeerConnection` from a WebRTC package such as
  node-datachannel's polyfill. Known issue: on Node 26.7, a script can hang in
  `process.exit()` on a busy machine while V8 is still optimising the wasm in the
  background; run it with `node --no-wasm-tier-up` (the gate runs its tests that way).
- **Identity:** `room.endpoint.secret()` returns 32 bytes; pass them back as
  `secret` to keep the same PeerId.

### Pull-style, for game engines

Hosts that handle callbacks poorly, such as Godot's web export through
`JavaScriptBridge`, can poll once a frame instead. Events are plain objects:
string ids, `ArrayBuffer` bytes, JSON values, and no class instances.

```js
const room = await joinRoom({ appId: 'my-game', roomId: 'match-1', queue: true });
// every frame:
for (const e of room.poll()) {   // or room.receive(): the next event, or null
  // { type: 'peer-join' | 'presence', peer, presence }
  // { type: 'peer-leave', peer, reason }
  // { type: 'message', peer, data, kind: 'text' | 'bytes' | 'json', lane }
}
room.sendTo('', 'x,y', 'unreliable');   // '' or null: everyone; else a peer id
```

`queue: true` keeps events from the moment you join. Without it, the queue starts
at the first `poll()` or `receive()`, so callback-only apps never build one up. A
started queue grows until you drain it. `demo/poll.html` runs this way.

Types are in `src/index.d.ts`.

## Shared documents

`room.doc(name)` opens a document that every member holds: maps, sets of items and logs. Writes reach every member
at once, members who join later or come back after a partition catch up, and members that have heard the same
writes hold the same document, whatever order they heard them in.

```js
const doc = room.doc('board');                   // the same name on every member is the same document
const meta = doc.map('meta');                    // one value per key
meta.set('title', 'Plans');                      // true; false, with a warning, if members have filled the doc
meta.get('title'); meta.has('title'); meta.delete('title');
const strokes = doc.set('strokes');              // items, each with its own id
const id = strokes.add({ points, color });       // the id; null if the doc is full
strokes.delete(id);                              // undo: delete your item
const chat = doc.log('messages', { keep: 500 }); // append-only, keeps the newest `keep` (1000 if not given)
chat.append({ text: 'hi' });
meta.entries();                                  // [{ key, value, from }], by key
strokes.entries(); chat.entries();               // [{ id, value, from }], oldest first, one order on every member
doc.on('change', ({ name, type, key, id, value, deleted, from, local }) => render());
doc.on('sync', ({ state, from, have, total }) => status()); // catching up: see below; doc.sync is the latest
doc.close();                                     // room.leave() closes the room's docs too
```

A late joiner sees the doc fill in, newest entries first, and can say where it stands meanwhile:

```js
const show = ({ state, from, have, total }) => {
  const percent = total ? Math.round((100 * have) / total) : 0;
  if (state === 'alone') line.textContent = doc.synced ? "You're the last one here" : "You're the only one here";
  else if (state === 'loading') line.textContent = `Loading from ${nameOf(from)}… ${percent}%`;
  else line.textContent = '';
};
show(doc.sync);
doc.on('sync', show);
```

A doc can be kept on each member's device, so it is back when that member comes back, even with nobody else online:

```js
const doc = room.doc('board', { persist: true }); // IndexedDB; in Node, a store (see "On this device")
await doc.loaded;                                  // 'loaded', 'empty', 'blocked' (storage refused) or 'off'
await doc.forget();                                // deletes this device's copy, e.g. on sign-out
```

- **What wins:**
  - A `set` replaces the values its writer has seen.
  - Two values set at the same time, where neither writer had seen the other's, are ordered by Lamport stamp and
    then by writer, the same on every member.
  - A `delete` removes the values its writer has seen, so a set made elsewhere at the same time survives it
    (add-wins, as in Yjs and Automerge).
  - Set items and log entries sort by Lamport stamp, then writer. A write made after seeing another sorts after
    it, which gives a whiteboard one z-order and a chat one order. No wall clock is involved.
- **Sync:**
  - Each write goes to every member as a delta on the reliable lane: the new entry and the ids of the values it
    replaces.
  - A doc asks every member when it opens, and each member that joins. The ask carries its context: for each
    writer, the ranges of writes it has seen.
  - It asks one member at a time, so a joiner is sent each entry about once (at most 1.00× the entries it holds
    in `test/doc_crdt.mjs`), not once per member.
  - A member is answered 8 times at once, then once per 15 s, and our asks of it go at the same rate. What the
    answers carry is capped too: 8 Mi characters of JSON text at once (two full docs), then 64 Ki a second. An
    answer's first piece takes one answer from that budget, and each piece its characters. An ask past that
    waits rather than being dropped, so a doc closed and opened again (a component that remounts) catches up
    every time: a small doc 8 times in a row at once, then each up to 15 s late.
  - Every frame stays under 512 KiB.
  - Doc frames have their own tags, so they never reach `'message'` or `poll()`. Versions without `room.doc()`
    (`@peerwire/js` 0.1.0 to 0.2.0) drop them.
- **Catching up:**
  - Members are asked in the order of their routes (`room.route()`): linked directly, then relayed by a member,
    then over signalling. When one linked directly joins, or a member's route becomes direct, while an answer
    comes over signalling, that member is asked at once, and the one over signalling again after it, for what is
    still missing. A member gives up its turn this way once until an answer of its ends: after that, the better
    routed one waits for the turn to pass.
  - An answer comes in pieces, the newest entries first (the top of a whiteboard), then the writes it knows are
    deleted. A piece is about 32 Ki characters of JSON over signalling or through a member, and 128 Ki linked
    directly; it is sized as it is cut, so an answer that a route upgrade moves gets bigger pieces. Each piece
    is applied as it lands and acknowledged, and at most 4 go unacknowledged, so an answer keeps the pace of its
    route, and live writes don't queue behind the whole doc.
  - The next member is asked once an answer has gone 5 s without a piece, or has taken 60 s while another
    member waits. The slow answer goes on, and the next ask carries what its pieces brought, so it is answered
    only with what is still missing.
  - Doc frames of 1 KiB or more go deflated (raw deflate, where the platform has `CompressionStream`) to members
    whose doc reads them. A whiteboard of 40 strokes of 1000 points, 508 K characters of JSON, reaches a late
    joiner in about 200 KB, 2.5 times less (`test/room_doc.mjs`). A frame that would deflate more than 16 times
    goes plain, and a member's frame is inflated only to 16 times its size and 1 MiB, so what a member sends
    can't make us read more than 16 times it. A room with no doc open drops deflated frames unread.
  - A member that doesn't have the doc open answers its asks with nothing, so it holds no one up.
  - In `test/doc_catchup.mjs`, over the signalling pipe through relays that take 10 events a second, a late
    joiner holds that whiteboard within 15 s of linking and its first strokes within 3 s (about 10 s and 0.2 s on
    a loaded 4-core host), and within 30 s and 5 s when the member it catches up from is a background tab,
    whose timers fire once a second (about 13 s and 1 s).
- **Where catching up stands:** `doc.sync` is `{ state, from, have, total }`, and `'sync'` fires when it changes.
  - `'alone'`: no other member to catch up from.
  - `'loading'`: catching up from member `from`, which has sent `have` of the `total` entries its answer carries
    (0 of 0 until its first piece lands). `have` may stop short of `total`: the last piece ends the answer.
  - `'synced'`: caught up. An answer ended from a member that had caught up itself, or every member has answered
    (none had: members that open the doc together hold all there is between them).
  - `doc.synced` is true once caught up, and stays true, also once the others have gone (`'alone'`). So an app
    can tell "you're the last one with the board" from "you're the first one here".
- **On this device (`persist`):**
  - Opening loads the copy before members are asked, so the ask carries it and members send only what it lacks;
    their asks of us wait for it too. A member who was never there gets the doc from one that kept it, once that
    one is online: nothing holds a room's docs while nobody is in it.
  - The doc is saved when the page is hidden, on `doc.close()` and `room.leave()`, whose promises resolve once it
    is, and while it changes: a second after a change, one save at a time, and each save writes the whole doc, so
    the next waits for the last one's bytes at 256 KiB/s: 2 s for the whiteboard above, 48 s for a full doc. A page
    killed while visible can lose its last few seconds of writes here, more for a large doc; the members that heard
    them still have them.
  - `persist` is `true` for IndexedDB (browsers), or a store of bytes, `{ get(key), set(key, bytes), delete(key) }`,
    each returning a promise. `get` gives undefined only when there is no copy, and rejects when it can't tell.
    `memoryStore()` keeps copies in the process, and files take a few lines (keys are 64 hex characters). Make the
    store once and pass the same object each time, as a doc closed and opened again at once loads what the
    closing one saved only through the same store:

    ```js
    import { readFile, writeFile, rename, rm } from 'node:fs/promises';
    const files = (dir) => ({
      get: (key) => readFile(`${dir}/${key}`).then((b) => new Uint8Array(b), (e) => {
        if (e.code !== 'ENOENT') throw e; // unreadable isn't absent
      }),
      // Whole or not at all: a crash mid-write leaves the last copy.
      set: (key, bytes) => writeFile(`${dir}/${key}.tmp`, bytes)
        .then(() => rename(`${dir}/${key}.tmp`, `${dir}/${key}`)),
      delete: (key) => rm(`${dir}/${key}`, { force: true }),
    });
    const store = files('./docs');
    const doc = room.doc('board', { persist: store });
    ```
  - The copy is sealed with AES-256-GCM under a key derived from the room key, and kept under an id derived the
    same way, so the storage shows neither the app, the room, the doc nor what it holds. With `persist`, a doc name
    must be well-formed (no lone surrogate, else RangeError), as two such names would share a key.
  - A copy is about the size of the doc's JSON: 510 KB for the whiteboard above (`test/doc_catchup.mjs`), and
    12.1 MiB at most for a full doc of 3-byte characters (`test/doc_crdt.mjs`).
  - Storage that refuses (some private modes, a full disk) gives `'blocked'` or a failed save: one warning, and the
    doc goes on in memory; a failed save is tried again with the next change. A store that hasn't loaded the copy
    5 s after opening is `'blocked'` too: the doc asks and answers without it, and drops it if it comes later.
    `'blocked'` leaves the copy as it is: that opening never writes it, as it may hold what it couldn't read
    (`forget()` still deletes it). A copy that can't be read (too big, doesn't open, isn't JSON, isn't a doc)
    counts as none, with a warning saying why, and the next save replaces it; one that reads is checked as member
    data. A copy saved by another version of this package (a newer one) is left as it is: `'blocked'`, and the doc
    keeps none until a version that reads it opens it.
  - Two tabs of one browser in one room keep one copy of each doc, whichever saved last. Each catches up from the
    other while both are open, but a copy can miss the other tab's latest writes until they reach a member.
  - **What a copy keeps secret, and from whom:**
    - A copy is exactly as secret as the roomId. An app that keeps the roomId in the URL (history, bookmarks) or in
      `localStorage` gives it to anyone who can read the device's browser profile, and with it the copy. The seal
      protects a store taken on its own: a copied IndexedDB, a file from a file store, a backup.
    - It is authentic only against someone without the roomId: any member can make a copy that opens, as it can
      send an answer, so a copy is checked as member data.
    - A copy's id is the same on every device for one app, room and doc: two stores show they hold the same doc,
      not which one.
    - Its size and the time of its last save show. Someone who can write the storage can put an older copy back;
      the doc then catches up from the members, and never writes again as the writer the copy names, so no write
      is made twice. `doc.forget()` deletes the record, but storage can keep old bytes on disk or in backups.
- **Logs:** give `keep` the same value on every member, and call `doc.log()` right after `room.doc()`. A member
  that keeps fewer entries never trims another's log, but one that keeps more can miss the older entries another
  has trimmed, once it catches up from that member, directly or through a member that did.
- **Limits:**
  - Values are JSON: at most 64 KiB as JSON text and nested at most 64 deep. They come back frozen.
  - A doc holds 10,000 entries and 4 MiB of JSON text.
  - A value or key over its limit (keys: 256 characters; names: 128), a value that isn't JSON, or a write after
    `close()` throws: these are the app's own mistakes.
  - A doc that members have filled refuses writes with a warning, as every member refuses a write that doesn't
    fit. Deleting frees room.
- **Member data:**
  - Members are trusted alike, as in the room itself: any member can overwrite, delete or fill a doc.
  - What a member sends is checked and capped before it reaches you. A malformed frame is dropped whole.
  - A member can make the room ignore another member's writes, by claiming it has seen them before they are
    written. A doc that hears such a claim on its own writes moves on to new ones, 16 times; past that, it skips
    every write from the lowest claimed to the highest, its writes are lost at the members that took a claim it
    hasn't heard, and once every write is claimed they return false or null with a warning, until the doc is
    closed and opened again.
  - Every member takes a Lamport stamp however far ahead, as refusing one would split the room. So a member can
    move the stamps to 2^53 - 1 at once; from then on, values set at the same time, and set items and log
    entries, are ordered by writer, and a log keeps the highest writers' entries. A later set still wins.
  - A member can ask for the whole doc as often as it likes, and its answers can name any writes deleted, which
    we pass on to the other members, one range for each range of it that deleted something here. What it is sent,
    and what we pass on for it (counted once for each member), is at most 8 Mi characters of JSON text at once and
    64 Ki a second after, plus the send that went past that (theoretical, from those budgets; UTF-8 takes 1 to 3
    bytes a character). Past it, the others hear of those deletions from that member or when they next catch up.
    The budgets are per member while it stays: a member that leaves and joins again, or joins under a new id,
    starts with whole ones.
  - `from` is the writer as the doc was told. A member relaying a catch-up can name any writer, so use `from` to
    colour or group, not as proof of who wrote something. Check every field you render, as with messages.
  - Where catching up stands is as good as the members that told us: a member can say it had caught up when it
    hadn't, or carry fewer entries than it says, so a doc can show `'synced'` early or `total` too high. Use
    `doc.sync` for a status line, not to decide what the doc holds; entries that come later still show.
- **Not yet:** collaborative text (character sequences), and docs in Rust and Godot.

## Using with React

[`@peerwire/react`](../react/README.md) does all of this for you, with TypeScript types: `<RoomProvider>`, and
hooks for members, messages, shared state and per-peer values that check what other members send. To write your
own hook instead, join in an effect and leave in its cleanup, so the room lives as long as the component:

```jsx
import { useEffect, useState } from 'react';
import { joinRoom } from '@peerwire/js';

export function useRoom(appId, roomId) {
  const [room, setRoom] = useState(null);
  const [peers, setPeers] = useState([]);
  const [messages, setMessages] = useState([]);
  const [error, setError] = useState(null);
  useEffect(() => {
    let gone = false, joined = null;
    joinRoom({ appId, roomId }).then((r) => {
      if (gone) return r.leave(); // cleaned up before the join resolved
      joined = r;
      const sync = () => setPeers([...r.peers]);
      r.on('peer-join', sync);
      r.on('peer-leave', sync);
      r.on('message', (data, peer) =>
        typeof data === 'string' && setMessages((m) => [...m.slice(-99), { peer, data }]));
      setRoom(r);
      sync();
    }, (e) => gone || setError(e));
    return () => { gone = true; joined?.leave(); setRoom(null); setPeers([]); setMessages([]); };
  }, [appId, roomId]);
  return { room, peers, messages, error };
}

function Chat() {
  const { room, peers, messages, error } = useRoom('my-app', 'lobby');
  if (error) return <p>{error.message}</p>;
  if (!room) return <p>joining…</p>;
  return (
    <>
      <p>{peers.length} peers</p>
      <ul>{messages.map((m, i) => <li key={i}>{m.peer.slice(0, 6)}: {m.data}</li>)}</ul>
      <button onClick={() => room.send('hi')}>Say hi</button>
    </>
  );
}
```

- **StrictMode** mounts, cleans up and mounts again in development, so the first join
  can resolve after its cleanup has run. The `gone` check leaves that room at once;
  without it the page stays in the room twice and the other members see a second peer
  for as long as the page is open.
- **Dependencies:** list the options' values (`appId`, `roomId`), not an options object
  made on each render, or every render leaves and joins again. Change presence with
  `room.setPresence`, not by joining again.
- **Client only:** a room needs the browser (WebRTC, BroadcastChannel). Effects don't run
  during server rendering, so the hook is safe in a page that is rendered on the server
  too. In Next.js, use it in a client component (`'use client'`). Importing
  `@peerwire/js` touches no browser globals.
- **The wasm:** bundlers find it next to the package; Vite does in dev and in a build.
  To choose the file yourself, pass its URL as `wasm`:
  `import wasmUrl from '@peerwire/js/peerwire_ffi.wasm?url'` (Vite), then
  `joinRoom({ appId, roomId, wasm: wasmUrl })`. Root-relative URLs work too, such as
  `wasm: '/peerwire_ffi.wasm'` for a copy in Vite's `public/`.
- **Check what a member sent:** a message is whatever a member chose to send: text,
  bytes or any JSON. Check its type before you store or render it. React throws on an
  object child, and unmounting the app runs the cleanup, which leaves the room. The
  hook keeps strings only, and only the last 100, so a member who floods the room
  can't grow the list without bound.
- **Messages in state** suit a chat. For a game, handle `message` in a ref or a store
  outside React, and render from it each frame.

## Files

| File | What it is |
|---|---|
| `src/index.js` | The package entry: `joinRoom` and everything below. |
| `src/room.js` | `joinRoom` and `Room`: mesh, presence, message framing, backpressure. |
| `src/doc.js` | `room.doc()`: shared documents (maps, sets, logs), a delta-state CRDT, catch-up, limits on member data. |
| `src/proof.js` | Room key and HMAC proofs (hello, offer, link), compatible with Rust's; `randomRoomId()`. |
| `src/peerwire.js` | Loads the module; `Endpoint` (connect, send, events, stats, seal/open) and Nostr key helpers. Browser and Node. |
| `src/webrtc.js` | `WebRtcTransport`: one negotiated, unordered, no-retransmit DataChannel per peer. `BroadcastSignaling` for tabs of one browser. |
| `src/nostr.js` | `NostrSignaling` over public relays: ephemeral events, a throwaway key per session, payloads sealed to the recipient's PeerId. |
| `src/local.js` | `LocalTransport`: datagrams over the BroadcastChannel, for Node and tests. |
| `src/index.d.ts` | TypeScript types. |
| `demo/index.html` | Mesh chat on the low-level API: every tab links to every other. |
| `demo/chat.html` | Chat on `joinRoom`, mobile-friendly: a room per link (`#<room>`; New room makes one with `randomRoomId()`), an invite link with a copy button and a QR code (`demo/qr.js`, a small encoder), nicknames in presence (remembered in localStorage), members with their route (direct, relayed by a member, or over signalling) and RTT, a typing indicator on the unreliable lane that goes after 3 s, and messages on the reliable lane (up to 2000 characters). What members send is shown as text only (`textContent`), never as HTML. Nostr by default; `?signal=local` for tabs of one browser. A member sees messages sent while it is in the room. |
| `demo/poll.html` | Pull-style room: `poll()` and `sendTo()` once a frame, dots you move with the arrow keys. |
| `demo/arena.html` | Super-peer lockstep arena: `?role=super` in one tab, players in others; arrow keys move. |
| `demo/netlab.html` | Net Lab, a connection diagnostics page (not a game), mobile-friendly: join a room by any name you type (`office test` is the room `office-test`, shared as a `#room=` link, with recent rooms and a "make private" suffix), the global room, or a private one shared as a `#key=` link, and every member links to every other. An overview row sums up the room (peers, average and worst ping, packet loss, routes, WebRTC traffic, packets). Per peer: ping measured on the route in use (latest, min/avg/max, jitter, loss, with a sparkline), core and ICE round trips, WebRTC traffic, ICE restarts and port guesses, the route (direct, relayed by a member, or over signalling), the ICE candidate pair (IPv4/IPv6, UDP/TCP), the offer/answer exchange (times, resends, the addresses each carried), ICE connectivity checks sent, answered and received with a verdict on which way packets are blocked, time to link and the other side's NAT. Also this device's public addresses, IPv6 and a NAT mapping estimate, the signalling relays' state, chat, an event log with warnings and errors, and a JSON report. `?relays=` other Nostr relays; `?rtc=off` fails this tab's WebRTC links. `demo/crew-server.mjs` serves it at `/`. |
| `demo/crew.html` | Crew, a small Among Us-like game: the host is authoritative and relays for every player, so a player needs a link to the host only. Network panel: per-peer RTT and route (ICE path, or the fallback). `?turn=turn:HOST:PORT&tu=USER&tp=PASS` adds a TURN server; `?rtc=off` fails this tab's WebRTC links (testing the fallback). `demo/crew-server.mjs` serves it for hosting. |

Signalling is untrusted by design: whatever carries the offers, the Peerwire Noise
handshake over the DataChannel proves each side holds its PeerId.

## Privacy

A WebRTC peer sees the IP addresses in your ICE candidates and, once linked, the
one your packets come from. `new WebRtcTransport(ep, sig, { privacy })` picks what
it is given:

- `'direct'` (default): every candidate.
- `'public-only'`: no `host` candidates (LAN, loopback, mDNS `.local` names), in
  trickled ICE and in the SDP. Only what STUN reports goes out, so two tabs on one
  machine with no STUN server can't link this way. No other local address goes either:
  a candidate's related address (its local socket, which browsers show once a page has
  mic or camera permission) is cleared, and so is the default address that gathering
  writes into the SDP's `c=` line and m-line port.
- `'relay-only'` with `relay: <peer id>`: WebRTC runs with the relay only (also
  public-only); offers from anyone else are ignored and `dial()` to them throws.
  Reach other peers with `endpoint.connectVia(relay, peer)`: the endpoint is
  relay-only (`Endpoint.setRelayOnly`), so those links never punch to a direct
  path, and the relay is asked not to pass on your address. The relay sees both
  ends. There is no TURN: a peer not linked to the relay can't be reached.

**With microphone or camera permission.** Until a page has it, browsers hide its local
addresses (behind mDNS names; Android Chrome offers only its mobile-data interface).
Connections made after the grant list every network interface (RFC 8828 mode 1), which
affects every voice or video app. The grant is stored per origin, so it also applies on
later visits and to pages of that origin that never ask:
- `'direct'`: members get each interface's real address, LAN, hotspot and IPv6 ones.
  That is also what lets a phone and a laptop on its hotspot link directly. In one field
  test (October 2026: Android Chrome on mobile data, a laptop on its hotspot) the pair
  linked direct in 1.2 s, at about 10 ms, when both pages had microphone permission, and
  over the signalling relays in 5 to 6 s, at about 600 ms, when they didn't.
- `'public-only'`: none of those, and so not that direct path either. Each interface's
  public address as STUN sees it still goes (the Wi-Fi's and the mobile network's, or
  the real network's under a VPN).

## Trying it

- Same browser: open `demo/index.html` or the arena pages in several tabs.
- Chat: open `demo/chat.html`, start a room, and send its invite link to another browser or scan its QR code with a
  phone. Add `?signal=local` to keep it to tabs of one browser.
- Different machines: add `?signal=nostr&room=<something only you know>` to every
  page. Peers meet through `wss://nos.lol`, `wss://relay.primal.net` and `wss://nostr.mom` and use
  Google's public STUN. Peers behind symmetric NATs on both sides still meet: see
  below.

## Through any NAT

WebRTC can't link two peers that are both behind symmetric NAT (common on mobile
hotspots and carrier NAT) unless a TURN server is configured. Rather than requiring
one, `joinRoom` falls back per peer, on its own, with no setup on either side:

1. **Direct WebRTC**, as always (`room.route(peer).kind === 'direct'`). Two things help
   it through more NATs:
   - **Port prediction.** A browser behind a NAT that hands out ports in sequence sees it
     from its two STUN servers' answers (`natFromSdp`: one IPv4 address, two ports 1 to 8
     apart). Its offer, answer or late candidates then carry `nat: { ip, next, step }`. A
     peer that isn't behind a symmetric NAT itself adds 32 candidates at the ports around
     `next` (`predictedCandidates`), so its checks open its NAT to the port the other's
     will come from. Peers that don't know the field ignore it.
   - **ICE restart.** When a link that was open loses ICE (a Wi-Fi to mobile switch, a
     hotspot dropping), it isn't dropped: after 2 s of `disconnected`, or at once on
     `failed`, the dialling side renegotiates with new ICE credentials, so ICE gathers the
     new network's addresses and checks again; the other side asks it to. The DataChannel
     and the Peerwire session over it carry on, so the app sees no leave. At most 3 tries
     a minute (2, 4, 8 s apart), whether or not ICE connects in between; while it stays
     down, the next waits for the minute to free. `online` and `navigator.connection`
     changes restart links whose ICE is down at once. Each restart is an offer and an
     answer over signalling, within the peer's publish budget (6 at once, then 1 per 5 s;
     the candidates of an answer it refused wait with it).
2. **Through another member.** When a link isn't open `fallbackMs` (8 s) after its
   offer, or its ICE fails, the dialling side dials the peer again through every member
   it has a direct link to, including members that link later. The core forwards for
   linked peers (`connect_via`, 256 KiB/s per peer) and sees only ciphertext (`'peer'`).
   - **Behind one NAT.** A peer whose ICE candidates name one of your public IPv4
     addresses (a laptop on a phone's hotspot) gets `sameNatFallbackMs` (3 s) instead.
     Browsers find no direct path there unless they show their LAN addresses: mDNS hides
     them, and Android Chrome lists only its mobile-data interface. Waiting the full 8 s
     rarely helps, so such a pair links over signalling about 3 to 4 s after hearing
     each other (the handshake crosses the relays), not 9 s. Strangers behind one
     carrier NAT match too, and seldom link direct either. The transport traces
     `same-nat`.
3. **Over the signalling relays.** If no member has answered within 1 s, or at once
   when there is no other member to ask, Peerwire datagrams also go as signalling
   messages (a *pipe*). On Nostr each batch is sealed to the peer again
   (`'signaling'`). This works wherever the relays are reachable. It is slow, though: a
   relay round trip per batch, 40 ms batching (a batch's worth goes at once), and at most
   6 batches a second for all of a client's pipes together, each through one relay (see
   below). That is within what public relays allow one client. A background tab fires its timers once a second at most: a
   pipe timer that fires that late sends every batch the rate allows by then, and until
   one fires on time again, pipes send what each task queued at its end, not 40 ms later.
   Each relay connection is paced on its own: at most 4 events a second (a burst of 8),
   the rest waiting on its queue for up to 10 s. While no relay can take one, a pipe
   holds its batch, which fills meanwhile. Relays don't publish their rate (NIP-11 has no
   field for it), so each relay's is learnt from its refusals. One that refuses an event
   for its rate (`rate-limited:`) halves its rate, which grows back by one event a second
   every 5 s. It is held for 1 s (doubling to 30 s while it refuses again before taking
   one), and is sent the event again, 3 times in all. A relay isn't held while no other is
   free, so a join or a pipe always has a relay to go through. Each pipe batch goes
   through one relay, the relays taking turns (one that holds us for its rate, banned us,
   isn't open, or is slowed by its refusals so that it couldn't take the batch within
   1 s is skipped), and every other signal through each. A pipe loads each relay with a
   share and moves at about their rates together, up to its own 6 batches a second,
   while they keep pace; a relay slowed by its refusals gives its turns to the others. A batch is lost when its relay drops it or the peer doesn't hear that relay,
   like a datagram on any path, and the core sends what it carried again: taking turns
   keeps that to one batch in three when one of three relays drops them all. The last
   batches a room sends as it leaves go through every relay, as nothing could send them
   again. A relay that bans us (`blocked:`, `banned`) gets nothing for 30 s, doubling up
   to 10 min; the ban starts over at 30 s once the relay has gone 10 min without one.

The core keeps the first path that completes the handshake, and the room's proofs,
presence and messages run unchanged on it. A pipe opens only for a peer whose hello
proved the room key. `fallback: 'peers'` skips step 3; `'off'` keeps WebRTC only.
TURN in `iceServers` still works and is faster than step 3 when you have a server.

**A member beats signalling.** A link over signalling moves to a member that links
directly to both of you later (it joined, or its own WebRTC links opened). The room
offers every such member, the core sends a sealed probe through it, and the link moves
there (`'peer'`) once the far end answers through the member, which it does only if it
would use that member too: one stray packet never moves it. If the member leaves, the
link goes back to signalling at once (after 15 s of silence if it vanishes without a
word), then on to another member that answers. It is the same link throughout, with no
`'peer-leave'` or `'peer-join'`. A member never relays onto its own pipe.

**Direct still wins.** ICE goes on checking after the fallback starts. If the link's own
DataChannel opens later (two desktops on one Wi-Fi resolve each other's mDNS names), the
link moves onto it, and `room.route(peer)` says `'direct'` again:
- **A relayed link** punches over the channel and moves there once the punch is answered.
- **A link over signalling** sends its datagrams over the channel. The far end's core, hearing
  them there, moves the link onto it, and then ours does: an authenticated datagram from a
  new address moves a direct path. The room sends one at once (an empty reliable frame),
  so an idle link moves too, not at the next keepalive. Datagrams still queued for
  signalling when the channel opens go over the channel instead. Those that still come over
  signalling after that were sent before it opened. They are dropped, so the link doesn't
  move back.

**WebRTC is tried again.** While a peer is on a fallback, the room retries WebRTC with a
new connection and leaves the working link alone. If the new DataChannel opens, the link
moves onto it as above: `route(peer)` becomes `'direct'`, the same session, with no
`peer-leave` or `peer-join`. A retry that doesn't open within `fallbackMs` closes quietly;
the fallback and the peer stay as they are. The room retries:
- **on a backoff:** 20 s after the peer went on a fallback, then 40, 80 and 160 s, then every
  5 min, each ±20 %;
- **after a network change:** `online`, or a new `navigator.connection.type` (a new speed
  estimate doesn't count);
- **after a microphone or camera grant**, with privacy `'direct'`: browsers show their local
  addresses (mDNS hides them, Android Chrome lists only mobile data) only to connections
  made after one, so a laptop on a phone's hotspot can then link to the phone directly
  (when both pages have the permission; see Privacy for what members then see).
  `retryOnGrant: false` turns this off. With `'public-only'`, retries offer no local
  address, as first dials don't;
- **when the app asks:** `room.retryDirect(peer?)`, for every peer on a fallback or one.

Only the dialling side (the smaller id) offers; the other side asks it to. Each retry is
an offer and an answer over the signalling relays, so a room keeps them rare: one at a time,
at least 2 s apart, peers in turn, and a peer's requests count once per 20 s. A 20-peer room
whose retries all fail makes about one per peer per 5 min, a few a minute at most. On the
answering side, a peer's retry offers make at most one new connection per 2 s.

**A direct channel found later.** A DataChannel to the peer opened outside the room (a
browser birthday punch's, `src/birthday.js`, as Net Lab runs one) can take a fallback
link over. Both sides call `room.adoptChannel(peer, pc, dc)`. Before anything moves, the
two ends agree over the channel: each sends a one-byte marker until it hears the
other's, so a link never moves onto a channel only one side reads. A link over
signalling then sends its datagrams over the channel at once. A relayed link moves once
the endpoint's punch over the channel is answered (`endpoint.addCandidate`).
`room.route(peer)` then says `{ kind: 'direct', adopted: true }`. It resolves `false`,
and leaves the channel to the caller, if the link is direct already or the peer didn't
adopt within 5 s. When the channel closes, a link over signalling goes back at once.
A relayed one goes back to its member after the endpoint's 15 s direct timeout.

What the fallback reveals:
- **To the other members.** Every member the dialler is linked to learns that the two
  peers couldn't link directly. The member that relays sees the timing and volume of
  their traffic, never its content. A relaying member can also move the link onto its
  own path, which gives it no more power than dropping the traffic would.
- **To the public relays.** On a pipe, the Nostr relays see a steady stream between two
  tags for the whole session, where WebRTC sends them only a few signalling events.
  They see no content and, unlike WebRTC, the peer learns no IP address.

## Tests

Run these with Node and the Playwright install on this host:

- `node packages/js/test/node_pair.mjs`: two endpoints in Node, checks the glue.
- `node packages/js/test/wasm_source.mjs`: where `loadPeerwire` reads the module from in Node: an
  absolute path or `file:` URL from disk, an `http:` URL by fetch (a 404 rejects with its status).
- `node packages/js/test/privacy.mjs`: host-candidate filtering, and a relay-only
  endpoint linking through a relay and refusing a direct dial.
- `node packages/js/test/room_node.mjs`: `joinRoom` with three peers in Node: presence,
  message types, lanes, targeted sends, backpressure, leaving, a late joiner and
  the pull-style `poll()`/`receive()`/`sendTo()` path.
- `node packages/js/test/peer_id.mjs`: untrusted peer ids. A malformed or over-long id
  throws a `TypeError` before any copy into wasm memory, and every signalling layer
  drops a `from` that isn't 64 lowercase hex characters.
- `node packages/js/test/nostr_bounds.mjs`: a flood of 12,000 distinct Nostr events
  keeps the de-duplication set at 4096 ids, malformed events don't throw, and base64
  round-trips bodies up to 1 MB. Each relay is paced on its own; one that refuses for its
  rate is held 1 s (2 s if it refuses again before taking one) at half its rate and sent
  what it refused again, in order. With every relay held, the hold that ends first is cut
  short, as it is when the one free relay closes. A ban stops a relay for 30 s, doubling.
  Queues drop their oldest past 64 events or 10 s, and `close()` sends what waits at once,
  but not to a relay that holds or banned us (a pipe batch waiting on a held one goes
  through a free one). The pacing runs on a monotonic clock: the
  wall clock set back 60 s stalls nothing.
  Pipe batches go through one relay each, the relays taking turns, also one that can't
  send yet but can within 1 s; one held for its rate, banned, closed or slowed behind its
  backlog is skipped, and with none left the one free soonest takes it. One refused for
  its rate goes again through the next relay. Sent as `wait()` allows, pipe batches move
  at three relays' rates, other events at one's, and with relays of different rates each
  takes about an equal share. No relays.
- `node packages/js/test/room_proof.mjs`: an attacker that knows only the room topic
  gets no dial, no answer to its offers and, on a link it dials itself, nothing but
  the link proof before it is dropped; the hello proof matches Rust's; a room that signs
  its own link proof after checking the peer's still joins on both sides, and one whose
  link is lost before that proof is sent leaves the peer unproven, with no `peer-join`.
- `node packages/js/test/fuzz.mjs`: seeded random and mutated input to the JS parsers (Nostr
  events and relay messages, hex and peer ids, room hello proofs, room frames and shared document
  frames): nothing throws out of a handler of untrusted input and no promise is left rejected.
- `node packages/js/test/webrtc_signals.mjs` (Chromium, loopback): malformed offers leave no
  link and no unhandled rejection, 300 random signals throw nothing, real offers are still
  answered, and each refused signal is counted in `WebRtcTransport.badSignals` (as Nostr's
  `stats.rejected` counts refused events). Without trickle, a lost first offer or first
  answer still links two transports (in about 3.2 s), candidates that overtake their
  description are held, and gathering stops 300 ms after the first public candidate.
- `node packages/js/test/nat_predict.mjs`: NAT type and prediction from server-reflexive
  candidates, the 32 predicted candidates, and a peer's prediction refused unless it is an
  IPv4 address and ports in range (nothing can be spliced into a candidate line).
- `node packages/js/test/ice_restart.mjs` (Chromium, loopback): an ICE restart by either side
  renews the ICE credentials while the DataChannel stays open and datagrams flow; the
  other side's ask is ignored while the dialling side's own ICE is connected; ICE
  `disconnected` restarts after 2 s, `failed` at once; at most 3 a minute, ICE reconnecting
  or not; `networkChanged()` touches only links whose ICE is down; an offer's prediction adds
  32 candidates, a malformed one none.
- `node packages/js/test/restart_budget.mjs`: a peer can't keep us publishing to the
  signalling relays. A peer looping `restart` asks (answering each, ICE reconnecting) gets
  3 restarts; an offer and 200 copies of it, or 50 new offers from a linked peer, get 6
  answers (the per-peer budget: 6 at once, then 1 per 5 s; the rest counted in
  `throttled`), and only those answers' candidates (`ice` or `ice-batch`); a copy of the
  offer releases a held answer, made again with the candidates found meanwhile; a copy of
  a new offer never gets the last one's answer; a genuine ICE failure still restarts, on
  either side; a minute after the 3rd restart a 4th goes through, by itself if ICE is
  still down.
- `node packages/js/test/pump_rearm.mjs`: a transport that throws or refuses to send (a full
  DataChannel) costs datagrams, never the endpoint's timer: a reliable message sent during
  the outage still arrives after it, by retransmit.
- `node packages/js/test/endpoint_closed.mjs`: after `close()` no `Endpoint` method calls
  into wasm: a late datagram is dropped, `send`/`connect*` throw, queries return null.
- `node packages/js/test/scratch_buffers.mjs`: `send` and `receive` reuse the endpoint's
  buffers: messages arrive intact back to back and after wasm memory grows, with no
  allocation per message up to 16 KiB.
- `node packages/js/test/room_drain.mjs`: 6 MiB through a Room at once, over the send
  window: the outbox is flushed as ACKs arrive (`Endpoint.onDrain`), the timer is a fallback.
- `node packages/js/test/room_leave.mjs`: a room leaving with a full send window reaches
  its peers at once, before `leave()` returns; with the Bye lost, the core's close still does;
  the `pagehide` handler holds the page 20 ms after it, only if it had peers.
- `node packages/js/test/doc_crdt.mjs`: shared documents on fake rooms, seeded. Members converge after frames
  arrive in random order and twice (no catch-up needed), after drops healed by joining again, and after a
  partition heals; a late joiner is sent each entry about once. It also checks:
  - intent: a later set wins, a set beats a delete it didn't see, a reply sorts after what it saw;
  - one `'change'` per change, also for a map whose name ends where another's key begins, and frozen values;
  - limits, including a full doc;
  - frames: every one under 512 KiB;
  - 20,000 mutated frames, none of which throws;
  - answers per member: 8 at once, then one per 15 s, and an ask past that waits; what they carry to a member
    that asks for a 2 Mi-character doc every second for a minute and acknowledges every piece (4 whole sends at
    once, then pieces as the budget allows, within a piece of the 8 Mi + 64 Ki/s bound);
  - answers in pieces: a whiteboard-sized doc (40 strokes of 1000 points) in 20 pieces over signalling and 4
    linked directly, newest first, each applied as it lands, at most 4 unacknowledged; acknowledgements of a
    piece not sent, sent twice or for another ask send nothing; a route that becomes direct mid-answer gets
    bigger pieces;
  - a slow answer taken up, not started again: the next member is asked 5 s after the last piece, not the ask,
    and the joiner is sent each entry once; a member trickling its answer holds the turn 60 s at most, and one
    that stalled holds it only until another can be asked; an older ask's answer ends nothing, also for a doc
    opened again during it;
  - routes: members linked directly asked first, then relayed, then over signalling, and a member that comes,
    or whose route becomes direct, during an answer over signalling takes the turn, once until that answer ends
    however often the better routed member is queued again;
  - `'sync'`: alone, two members opening together, a joiner loading with `have` up to `total` and synced once a
    caught-up member's answer ends, alone again once everyone has gone, and loading with a member that never
    answers;
  - a doc closed and opened 12 times while a member stays catches up every time;
  - a member that never answers is asked once, however often it prompts us, until it answers;
  - Lamport stamps moved far ahead (by 2^32 four times, or to 2^53 - 1): a member cut off meanwhile and one that
    joins after still converge, and so do a member's writes after a jump sent to it alone;
  - claims on a member's future writes, also past its 16 moves: an entry forged on its next write is never held
    and the doc stays whole, 12000 separate claims take one range of its context and new members are still heard,
    and with every write claimed its writes warn and return false or null;
  - a catch-up that deletes 3000 items is passed on as one range per writer; a member that sends us 9000 items of
    its own, then deletes them, three times, has us pass on at most what it sent (12 other members), and with one
    range per item, what we pass on for it stops at its 8 Mi budget and goes on once the budget is back;
  - what one member frame costs: 16384 copies of a dead range over 9000 entries, 16384 entries into a log
    that keeps 5000, and 16000 fresh writers into a full context, each timed against a frame of 16384 items
    (within 3 to 5 times it);
  - the copy on this device: a doc opened again is back as closed (entries, context and clock) and a member never
    there gets it; opened again among members, its ask carries the copy and only what it lacks comes, and what
    is written or heard while the copy loads is kept and saved; a doc closed while its copy loads saves both, and
    one opened again at once gets them; one save a second while a small doc changes, none for a frame that
    changes nothing; one save at a time (10 s of writes behind a held save call it once, then save once; the page
    hidden or the doc closed meanwhile saves as it ends); a full doc changed every 100 ms saves every 48 s, its
    12.1 MiB at 256 KiB/s; `forget()`; a reload that draws an incarnation in the copy draws again, and one on a copy
    rolled back uses no write twice; a copy of another version is left as it is; copies that are off in 11 ways
    are dropped whole, 5000 mutated ones never throw; a store that fails warns once, and that opening never writes
    the copy (nor does one closed during a load that fails), while a failed save is tried again with the next
    change; a store that never loads: at 5 s the doc asks and answers, saves nothing, closes without waiting, and
    drops a copy that comes later; full docs' copies take at most 36 frames of the 64 a copy may hold.
- `node packages/js/test/room_doc.mjs`: `room.doc()` with three members in a real `'local'` room: they converge, a
  doc opened late and a fourth member catch up, doc frames never show as messages, writes outlive their writer,
  and leaving closes docs. A whiteboard-sized doc reaches a late joiner deflated (2.5 times less), members that
  don't have a doc open answer its asks with nothing, a write that deflates 900 times goes plain, and deflated
  frames are taken in order with plain ones, one past 16 times its size or 1 MiB, or not deflate, or from a
  member gone meanwhile, dropped; a room with no doc open drops them unread. A doc kept on this device is back
  when its member comes back on the same secret and reaches a member never there; its copy is sealed (neither key
  nor bytes show the room, the doc or what it holds), another room's doc of that name and another doc's key can't
  read it, records sealed without the version or the key as additional data don't open, one of another version
  is left as it is, each refusal says why, a name with a lone surrogate is refused, `forget()` deletes it, a save
  gives the bytes it stored, and the file store above works, also when it can't read the copy (`'blocked'`, and
  nothing written over it).
- `node packages/js/test/doc_catchup.mjs`: a late joiner's catch-up of a whiteboard-sized board (509 KB) over a
  direct link, the signalling pipe through fake rate-limited relays, from a background tab, both at once, from a
  member that kept the board on its device and comes back after nobody was online, and with two holders, one
  direct and one over the pipe (one whole answer, from the direct one); then over the pipe through relays that
  take 3 events a second, then 2, which refuse part of every join's burst, and with the joiner deaf to one of the
  three relays, which loses the batches sent through it. It prints the time to the whole board, the frames,
  datagrams, retransmits, pipe batches and relay events, and checks the bars: 15 s (30 s from a background tab
  over the pipe, 60 s with the joiner deaf to a relay), first strokes within 3 s (5 s from a background tab over
  the pipe), and nothing while nobody is online.
- `node packages/js/test/pipe_batches.mjs`: a pipe's batches on a fake clock: datagrams queued together go 40
  ms later as one, a batch's worth at once, 6 a second after a burst of 12, and with timers held to once a second
  (a background tab), the batches a late timer finds go together, and what a task queued goes at its end, until
  a timer fires on time again. Batches go lossy (through one relay), the last one at `close()` not.
- `node packages/js/test/room_welcome.mjs`: a message a `peer-join` handler sends to the
  newcomer reaches it, also when the newcomer's presence came while its link proof was
  being checked (forced in 20 joins); 200 two-member joins and 30 third members lose none.
- `node packages/js/test/room_throttled.mjs --ab=<other packages/js>`: benchmark, not a test.
  Room send throughput when the sender's timers are throttled like a background tab's.
- `node packages/js/test/crew.mjs [tabs=4] [shot-prefix]`: plays `demo/crew.html` in four tabs: a kill,
  a report, a meeting with chat, the impostor voted out, then a second round won on tasks.
  `RTC_OFF=all` fails every WebRTC link, so the game runs over the signalling fallback;
  `RTC_OFF=1` fails tab 1's links only.
- `node packages/js/test/netlab_rooms.mjs [shot-prefix]`: Net Lab's named rooms: a typed name opens the room id
  it shows and goes in the address bar as `#room=`, the room link brings another tab into it (linked, pinging),
  another name is another room, the room is listed under recent rooms, and the report never holds its name.
- `node packages/js/test/netlab.mjs [tabs=3] [shot-prefix]`: opens `demo/netlab.html` in phone-sized tabs
  joined by one `#key=` link: every tab links to every other, pings come back, chat arrives, the
  report holds every peer and not the room key, and the page doesn't scroll sideways. `RTC_OFF=<i>`
  fails tab i's WebRTC links: it is still reached, by fallback, and pinged.
- `node packages/js/test/room_fallback.mjs`: a fake RTCPeerConnection blocks chosen
  pairs. A blocked pair links through a third member, or over signalling when there is
  none (a 100 KB message arrives whole), and stays apart with `fallback: 'peers'`/`'off'`.
  1,000 proven peers that each send one datagram over signalling leave at most 256 pipes.
- `node packages/js/test/room_adopt.mjs`: `room.adoptChannel` with the same fake. A link
  over signalling, or relayed by a member, moves onto a channel both ends adopt: 100 KB
  arrives with nothing over signalling, or the member relays nothing more. When the
  channel closes, the link goes back to its fallback route and carries on. A channel
  only one end adopts is refused after 5 s, and a direct link isn't moved.
- `node packages/js/test/room_retry.mjs`: retries on the same fake, whose first attempts fail. A
  pair over signalling, or relayed by a member, goes direct on the backoff's retry, the same session
  (one `connected`, no leave), with data over the channel and none over the pipe. A failed retry
  closes quietly and the next, twice as late, goes direct. The larger id's `retryDirect()` asks the
  dialling side. A `'public-only'` retry carries no host candidate. A microphone grant retries in a
  `'direct'` room only (not with `retryOnGrant: false`); so do `online` and a new connection type.
  20 proven retry offers 50 ms apart make one connection and one answer (a copy of that offer is
  answered again), and so do 2 to an answering side whose last retry link failed and went; the
  larger id ignores a request, and a retry whose `createOffer` throws fails
  quietly so the next one goes direct.
- `node packages/js/test/retry_backoff.mjs`: the retry schedule on a fake clock: the backoff and its
  jitter, one retry at a time, 2 s apart, peers in turn, requests once per 20 s; 20 failing peers
  for an hour stay within a few retries a minute.
- `node packages/js/test/retry_direct.mjs`: in Chromium, two tabs whose first connection gets no
  candidates link over signalling, then go direct on the retry with no leave; with the microphone
  granted and `'public-only'`, no signal, retries included, names a local address.
- `node packages/js/test/room_same_nat.mjs`: members behind one public IPv4, on a fake whose
  descriptions name it (or trickle it 1 s late, on either side) and whose ICE never
  connects. The pair traces `same-nat`, falls back 3 s after the offer, not 8, and links
  over signalling; different public IPv4s still take 8 s. When the pair's DataChannel
  opens after the fallback, a link over signalling (pipe batches 300 ms late) or relayed
  by a member goes direct and stays direct, with nothing more over the pipe. A pair that
  opens at once never falls back.
- `node packages/js/test/netlab_carry.mjs [shot-prefix]`: Net Lab in two tabs that
  link over signalling (`?rtc=off`). A real loopback channel between them, handed in as
  a birthday punch's winner, carries the room link: the route goes direct, the log
  gives the channel's address, and chat goes over it. When it closes, the link goes back
  to signalling. Net Lab's transport hook passes a closed link's handle on to the room.
- `node packages/js/test/netlab_mic.mjs [shot-prefix]`: Net Lab's "Use microphone" button, with fake
  media and the browser's mDNS hiding on. Before, the local addresses are `.local`; after a grant the
  report says `granted`, the network test lists real local addresses and reads the same NAT, and the
  pair, over signalling until then, retries WebRTC without rejoining (the same ids, no leave): the other
  tab sees `mic` and real local addresses on the new connection, which links direct. The global
  room, with the grant stored before the tabs open or given while they're in it, gives the other tab
  no local address and still links, and a birthday punch's offer carries no candidate. Pressed in the
  global room, the button rejoins before it re-tests the network, so a tab joining meanwhile hears no
  local address. A refused prompt reports `denied` and doesn't rejoin.
- `node packages/js/test/netlab_punch_close.mjs`: a Net Lab birthday punch that ends (the peer's abort)
  while its certificate, fleet, spray connection or answer is still being set up closes the connections
  made after it ended, and its card keeps the reason it ended, not a stale error. A second offer or
  answer from the peer is ignored: one fleet, all of it closed at the end.
- `node packages/js/test/readme.mjs [shot-prefix]`: runs the README chat above in two
  tabs and checks a message gets across.
- `node packages/js/test/wasm_url.mjs [shot-prefix]`: two tabs join with a root-relative `wasm`
  (`'/pkg/peerwire_ffi.wasm'`, the kind of URL a bundler's `?url` import gives) and chat; relative and
  absolute URLs load, and a 404 rejects with its status.
- `node packages/js/test/chat.mjs [shot dir]`: the chat demo in two tabs: members and routes, nicknames,
  the typing indicator, messages shown as text only, frames that aren't messages dropped, leaving and New room.
- `node packages/js/test/qr.mjs`: the chat demo's QR encoder against known answers.
- `node packages/js/test/poll.mjs [tabs] [shot.png]`: the pull-style demo in tabs;
  every tab must see every other, and a closed tab must leave.
- `node packages/js/test/leave.mjs [--runs=10] [--stress=N] [--crash=1]`: closes a streaming
  tab and times the others' peer-leave (under 5 s each run); a crashed tab may take the 30 s
  idle timeout. `--stress=N` adds N busy loops for a loaded host.
- `node packages/js/test/doc_store.mjs`: `room.doc(name, { persist: true })` in IndexedDB: a doc written, left and
  reloaded is back, in one sealed record, in one database and store of constant names; a write just before the
  page is hidden is saved then and outlives a crash; `forget()` deletes the record; once the browser closes the
  connection (site data cleared), the next save opens it again; with IndexedDB refusing, the doc says `'blocked'`,
  warns once and syncs.
- `node packages/js/test/browser.mjs [tabs] [shot.png]`: mesh of tabs, RTT and throughput.
- `node packages/js/test/arena.mjs [players] [secs] [shot.png|-]`: the arena. `LOSS=0.1`
  drops datagrams. Checks that every island agrees on its lockstep state.
- `node packages/js/test/nostr.mjs`: two isolated browser profiles meet over public
  Nostr relays and connect.
