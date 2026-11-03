# JavaScript API

`@peerwire/js` is Peerwire for the browser and Node: the core compiled to a WebAssembly module with no imports
(`crates/peerwire-ffi`), a small JS host around it, WebRTC DataChannels to carry datagrams, and Nostr relays or a
BroadcastChannel to find peers. This page lists every export, its options and what it returns. The types are in
[`packages/js/src/index.d.ts`](../../packages/js/src/index.d.ts); this page follows them and the code.

For how to use it, see the guides: [rooms](../guides/rooms.md), [messages](../guides/messaging.md),
[shared documents](../guides/shared-documents.md), [calls](../guides/calls.md),
[offline messages](../guides/offline-messages.md) and [React](../guides/react.md).

```sh
npm install @peerwire/js
```

| | |
|---|---|
| Version | 0.2.1 (`packages/js/package.json`) |
| Runtimes | Browsers; Node 20 or later (`engines`), with no WebRTC unless you pass one |
| Module | ES module, `sideEffects: false`; importing it touches no browser globals |
| wasm | `pkg/peerwire_ffi.wasm`, 356,363 bytes on main (as `packages/js/build.sh` builds it and the gate reports it), no imports |

**Exports.** For apps: [`joinRoom`](#joinroom), [`Room`](#room), [`randomRoomId`](#randomroomid),
[`memoryStore`](#memorystore), [`openInbox`](#openinbox) and the [constants](#constants). The lower layers, which
`joinRoom` is built on: [`loadPeerwire`](#loadpeerwire), [`Endpoint`](#endpoint),
[`WebRtcTransport`](#webrtctransport), [`LocalTransport`](#localtransport),
[`BroadcastSignaling`](#broadcastsignaling), [`NostrSignaling`](#nostrsignaling) and the [helpers](#helpers).

## joinRoom

```ts
function joinRoom(options: JoinRoomOptions): Promise<Room>
```

```js
import { joinRoom, randomRoomId } from '@peerwire/js';

const room = await joinRoom({ appId: 'my-app', roomId: randomRoomId(), signaling: 'nostr', presence: { name: 'Ana' } });
```

Joins a room, and resolves once the signalling is ready and our first hello has gone out. Peers that give the same
`appId` and `roomId` find each other, link, and show up as [`'peer-join'`](#events).

| Option | Type | Default | What it does |
|---|---|---|---|
| `appId` | `string` | required | Names your app. Rooms of different apps never meet. |
| `roomId` | `string` | required | The room's name, and its only secret: see [Closed rooms](#closed-rooms). |
| `signaling` | `'local'` \| `'nostr'` \| factory | `'local'` | How peers find each other. `'local'`: a BroadcastChannel, for tabs of one browser or one Node process. `'nostr'`: public Nostr relays, across the internet. A function `(ctx: SignalingContext) => SignalingAdapter` brings your own ([`SignalingAdapter`](#signalingadapter)). |
| `presence` | JSON value | `{}` | Shared with every peer when it joins; change it with `room.setPresence`. |
| `queue` | `boolean` | `false` | Queue events for `room.poll()`/`room.receive()` from the start. Otherwise the first call starts the queue. |
| `secret` | `Uint8Array` (32 bytes) | a fresh identity | Restores an identity saved from `room.endpoint.secret()`, so the PeerId stays the same. |
| `wasm` | `string` \| `URL` \| `BufferSource` | next to the package | Where to load `peerwire_ffi.wasm` from: a URL (in a browser, relative and root-relative ones too, such as a bundler's `?url` import), a file path or `file:` URL in Node, or the bytes. |
| `relays` | `string[]` | [`DEFAULT_RELAYS`](#constants) | Nostr relays, for `signaling: 'nostr'`. |
| `iceServers` | `RTCIceServer[]` | none for `'local'`; else [`DEFAULT_ICE_SERVERS`](#constants) | STUN and TURN servers for WebRTC. |
| `privacy` | `'direct'` \| `'public-only'` | `'direct'` | Which of your addresses peers see: see [Privacy](#privacy). |
| `fallback` | `'auto'` \| `'peers'` \| `'off'` | `'auto'` | What to do for a peer WebRTC can't link: see [Through any NAT](#through-any-nat). |
| `fallbackMs` | `number` | 8000 | How long a WebRTC link may take to open before the fallback starts (`RTC_FALLBACK_MS` in `webrtc.js`). |
| `sameNatFallbackMs` | `number` | 3000, at most `fallbackMs` | The same, for a peer behind your own public IPv4 (`RTC_SAME_NAT_FALLBACK_MS`). |
| `retryOnGrant` | `boolean` | `true` | With privacy `'direct'`, try WebRTC again with peers on a fallback when the page is granted the microphone or camera. No effect with `'public-only'`. |
| `RTCPeerConnection` | constructor | `globalThis.RTCPeerConnection` | A WebRTC implementation for runtimes without one (Node: node-datachannel's polyfill). |

**Throws** (the promise rejects):

- `TypeError` with no `appId` or `roomId`, a `privacy` or `fallback` not listed, or an unknown `signaling`.
- `RangeError` for a `presence` over 1 MiB as JSON; `TypeError` for one JSON can't encode (a function, a BigInt).
- `Error` when the runtime has no WebRTC and `signaling` isn't `'local'` (pass `RTCPeerConnection`).
- The wasm load's error, such as an HTTP error status for its URL.

**Data.** Strings arrive as strings, `Uint8Array`/`ArrayBuffer` as `Uint8Array`, anything else as JSON. A member's
JSON, in a message or its presence, can nest about 500000 levels deep in 1 MiB, and some engines' `JSON.stringify`
throws past a few thousand (Node 22's does): check its depth before you stringify it.

**Signalling.** `'nostr'` publishes ephemeral events (kind 25059) to the relays and uses Google and Cloudflare STUN.
Relays see the room topic and the PeerIds in hellos, not the roomId; offers are sealed to the recipient. Each side
sends one offer or answer with its candidates, once ICE gathering is complete or 300 ms after the first public
(STUN) candidate (2 s at most), and any candidates found later in one `ice-batch`. Relays drop events, so an offer
with no answer is sent again at 3 s and 6 s, and a repeated offer gets the same answer again. With `'local'`,
candidates trickle. Signalling is untrusted by design: whatever carries the offers, the Peerwire Noise handshake over
the DataChannel proves each side holds its PeerId.

**Node.** Node has no WebRTC. `signaling: 'local'` rooms then carry their datagrams over the BroadcastChannel (peers
in one process or its workers). For real networks pass `RTCPeerConnection` from a WebRTC package such as
node-datachannel's polyfill. Known issue: on Node 26.7, a script can hang in `process.exit()` on a busy machine while
V8 is still optimising the wasm in the background; run it with `node --no-wasm-tier-up` (the gate runs its tests
that way).

### Closed rooms

The roomId is the room's secret. Relays see only a hash of it (the topic). Every hello, WebRTC offer and link
carries an HMAC under a key derived from it (`sha256("peerwire/room-key/<appId>/<roomId>")`, as in Rust):

- a peer is dialled only on a hello that proves the key;
- offers that don't prove it get no answer, and so none of your addresses;
- a link that doesn't prove it within 10 s (`PROOF_MS` in `room.js`) is dropped without seeing presence or messages.

A name anyone can guess (`'lobby'`) makes a public lobby. For a closed room use at least 128 random bits:
[`randomRoomId()`](#randomroomid). Members are trusted alike: anyone given the roomId can join. See
[Rooms and invites](../guides/rooms.md).

## Room

What `joinRoom` resolves to. One per join; `leave()` ends it.

| Property | Type | |
|---|---|---|
| `self` | `PeerId` | Our own PeerId: 64 lowercase hex characters. |
| `peers` | `PeerId[]` | Joined peers, not counting us. |
| `endpoint` | [`Endpoint`](#endpoint) | The Peerwire endpoint underneath, for stats, sealing and identity. |
| `signaling` | [`SignalingAdapter`](#signalingadapter) | The signalling in use. |

### Events

```ts ignore
room.on(event, fn): () => void   // returns a function that unsubscribes
room.off(event, fn): void
```

| Event | Arguments | When |
|---|---|---|
| `'peer-join'` | `(peer, presence)` | A peer linked, proved the room key and shared its presence. A peer counts as joined once its presence has arrived, so `presence` is always there. |
| `'peer-leave'` | `(peer, reason)` | A joined peer is gone. `reason` is `'left'` or `'closed'` at once when it leaves (`leave()`, or its tab closing); `'timeout'` after the idle timeout (30 s) when it crashes or loses its network; `'failed'` when its link failed with nothing else to try; `'replaced'` or `'violation'` as the core reports them. |
| `'presence'` | `(peer, presence)` | A joined peer called `setPresence`. |
| `'message'` | `(data, peer, { lane, kind })` | A joined peer sent something. `lane` is `'reliable'` or `'unreliable'` (urgent arrives as reliable); `kind` is `'text'`, `'bytes'` or `'json'`. |

Presence travels over the encrypted links, not through signalling. A `'peer-join'` handler can send to the
newcomer, to hand it the room's state, say: `room.send(state, { to: peer })` arrives after our own presence, so the
newcomer already lists us. Send it on the `reliable` lane (the default): a peer drops what a member sends before its
presence, and an `urgent` message can overtake it.

### send

```ts ignore
room.send(data: RoomData, options?: { to?: PeerId | PeerId[]; lane?: LaneName | number }): number
```

Sends to every joined peer, or to `to` (peers not joined are skipped). Returns how many peers it was queued for.
Reliable sends past the send window queue up in the room instead of failing.

| Lane | Number | Delivery | Largest message |
|---|---|---|---|
| `'reliable'` (default) | 0 | Ordered, retransmitted | 1 MiB |
| `'unreliable'` | 1 | No retransmits, latest-wins: for positions and cursors | One datagram: 1100 bytes |
| `'urgent'` | 2 | Reliable, each fragment sent twice, ordered only among urgent messages, so it overtakes queued `reliable` data; arrives as `reliable` | 64 KiB |

The largest message is `MAX_FRAME` in `room.js`, and it counts the message as encoded plus 1 byte of framing.

**Throws:** `RangeError` for a message too large for its lane, before it is sent to anyone; `TypeError` for a lane
that isn't one of those names or numbers; `Error` after `leave()`.

See [Sending messages](../guides/messaging.md) for choosing a lane.

### sendTo

```ts ignore
room.sendTo(peer: PeerId | '' | null, data: RoomData, lane?: LaneName | number): number
```

`send` with positional arguments, for hosts that pass plain values. `''` or `null` sends to everyone.

### poll and receive

```ts ignore
room.poll(): RoomEvent[]          // every queued event, oldest first, and clears the queue
room.receive(): RoomEvent | null  // the next queued event, or null
```

The pull-style API, for hosts that handle callbacks poorly, such as Godot's web export through
`JavaScriptBridge`. Events are plain objects: string ids, `ArrayBuffer` bytes, JSON values, no class instances.

```js
const room = await joinRoom({ appId: 'my-game', roomId: 'match-1', queue: true });
// every frame:
for (const e of room.poll()) {
  // { type: 'peer-join' | 'presence', peer, presence }
  // { type: 'peer-leave', peer, reason }
  // { type: 'message', peer, data, kind: 'text' | 'bytes' | 'json', lane }
}
room.sendTo('', 'x,y', 'unreliable');
```

`queue: true` keeps events from the moment you join. Without it, the queue starts at the first `poll()` or
`receive()`, so callback-only apps never build one up. A started queue grows until you drain it.
`packages/js/demo/poll.html` runs this way.

### Presence

```ts ignore
room.getPresence(peer?: PeerId): Presence | undefined   // ours with no argument
room.setPresence(presence: Presence): void
```

`setPresence` sends the new presence to every peer, who see `'presence'`. It throws `RangeError` for one over 1 MiB as
JSON and `TypeError` for one JSON can't encode (`undefined`, a function, a Symbol, a BigInt, a cycle), and keeps the
old one.

### route

```ts ignore
room.route(peer: PeerId):
  | (({ kind: 'direct'; adopted?: true } | { kind: 'peer'; via: PeerId } | { kind: 'signaling' })
      & { rttMs: number | null })
  | null
```

How `peer` is reached: its own WebRTC link (`'direct'`), a channel handed to [`adoptChannel`](#adoptchannel)
(`adopted`), relayed by member `via` (`'peer'`), or over the signalling relays (`'signaling'`); null while it isn't
linked. `rttMs` is the round trip over this path, measured since the link moved there; null until then.

A `'peer'` or `'signaling'` route can become `'direct'` later, with the same session (no `'peer-leave'` or
`'peer-join'`). A `'signaling'` route moves to a member linked directly to both of you once that member is seen to
relay both ways, and back if it goes. A `'peer'` route moves to another such member once it is clearly faster (20 %
and 15 ms, in 3 of 5 probe rounds 2 s apart). See [Through any NAT](#through-any-nat).

### retryDirect

```ts ignore
room.retryDirect(peer?: PeerId): number
```

Tries WebRTC again, now, with `peer` (or every peer) the room reaches only by fallback, on a new connection, so one
made after a network change or a microphone or camera grant offers the new addresses. If its DataChannel opens, the
link moves onto it; if not, it closes quietly and the link stays on its fallback. Returns how many peers are to be
retried. The room also retries on its own: see [WebRTC is tried again](#webrtc-is-tried-again).

### adoptChannel

```ts ignore
room.adoptChannel(peer: PeerId, pc: RTCPeerConnection, dc: RTCDataChannel): Promise<boolean>
```

Moves `peer`'s link onto `dc`, an open DataChannel to it made outside the room (`pc` its connection; a browser
birthday punch's, `src/birthday.js`, say), when the room reaches `peer` only by fallback. Both sides call it. True
once the channel carries the link; false, leaving the channel to the caller, if the link is direct already or the
peer didn't adopt within 5 s. When the channel closes, the link goes back to its fallback. See
[A direct channel found later](#a-direct-channel-found-later).

### rtt and stats

```ts ignore
room.rtt(peer: PeerId): number | null   // smoothed round trip in ms; null before the first sample
room.stats(): EndpointStats             // the endpoint's counters: see Endpoint
```

### doc

```ts ignore
room.doc(name: string, options?: DocOptions): Doc
```

The shared document `name`: see [Doc](#doc-1).

### call

```ts ignore
room.call(stream: MediaStream | null, options?: { max?: number }): Call
```

Joins the room's voice and video call: see [Call](#call-1).

### leave

```ts ignore
room.leave(): Promise<void>
```

Tells every peer we're going, then closes the links, all before it returns, so it works from `pagehide` (where the
room also runs it). Peers see `'peer-leave'` at once. Reliable messages not yet acknowledged may not arrive: the
goodbye can overtake them. It closes the room's docs and leaves its call. The promise resolves once docs kept on
this device (`persist`) have saved, however long the store takes.

## Doc

```ts ignore
room.doc(name: string, options?: DocOptions): Doc
```

A document every member of the room holds: maps, sets of items and logs. Writes reach every member at once on the
reliable lane; members that join later, or come back after a partition, catch up; members that have heard the same
writes hold the same document. `room.doc(name)` opens it on the first call and returns the same object until it is
closed. The same name on every member is the same document. `options` count when it opens. How it converges,
catches up and keeps copies: [Shared documents](../guides/shared-documents.md).

**Throws:** after `leave()`; `TypeError` for a `persist` it can't use, or `relays: true` without `'nostr'`
signalling or without `CompressionStream('deflate-raw')`; `RangeError` for a name that isn't 1 to 128 characters,
for a name with `persist` or `relays` that isn't well-formed (a lone surrogate), or for `relays: true` with a roomId
that hasn't `randomRoomId()`'s shape (22 or more base64url characters).

| Option | Type | Default | What it does |
|---|---|---|---|
| `persist` | `boolean` \| [`DocStore`](#docstore) | off | Keeps a sealed copy on this device, loaded when the doc opens: `true` for IndexedDB (browsers; `TypeError` where there is none), or a store. Make a store once and pass the same object each time. |
| `relays` | `boolean` | `false` | Keeps a sealed copy on the room's Nostr relays, for a member who opens the doc while no other member is online. **Anyone with the roomId can read it, for good**: read [Keep a copy on the relays](../guides/shared-documents.md#keep-a-copy-on-the-relays) first, and use `randomRoomId()`. |

| Property or method | |
|---|---|
| `name: string` | The doc's name. |
| `sync: DocSync` | Where catching up stands now: see [DocSync](#docsync). |
| `synced: boolean` | True once the doc has caught up, and from then on, also once the other members have gone. |
| `loaded: Promise<'loaded' \| 'empty' \| 'blocked' \| 'off'>` | What opening found on this device: `'loaded'` (its copy is in the doc), `'empty'` (none, or one that couldn't be read, which the next save replaces), `'blocked'` (the storage refused, hadn't loaded the copy within 5 s, or holds one from another version of this package: the copy is left as it is), `'off'` (no `persist`). |
| `map(name): DocMap` | The map `name`: one value per key. |
| `set(name): DocSet` | The set `name`: items, each with its own id. |
| `log(name, { keep }?): DocLog` | The log `name`: append-only, keeping the newest `keep` entries (1 to 10,000, default 1000). Give every member the same `keep`, right after `room.doc()`. Calling again with another `keep` trims to it. |
| `on('change', fn)`, `on('sync', fn)` | Subscribes; returns a function that unsubscribes. A handler that throws is reported on its own. |
| `off('change' \| 'sync', fn)` | Unsubscribes. |
| `forget(): Promise<void>` | Deletes the copy this device keeps, and keeps none until the doc is opened with `persist` again. With `relays: true`, also wipes the relay copy, best effort, for every member until another member saves. What the doc holds stays. Rejects if the storage fails. |
| `close(): Promise<void>` | Stops syncing and forgets the doc; `room.doc(name)` later opens it afresh and catches up. Writes after it throw. A doc kept on this device saves first, and the promise resolves once the store has the copy (closed while a store takes more than 5 s to load the copy, it resolves then, saving nothing). |

### DocMap

| Method | Returns |
|---|---|
| `set(key, value)` | `true`; `false`, with a warning, when members have filled the doc or claimed every write of ours. |
| `get(key)` | The value, or `undefined`. |
| `has(key)` | Whether the key holds a value. |
| `delete(key)` | `false` when the key holds nothing. |
| `entries()` | `readonly { key, value, from }[]`, by key. The same array until the map changes. |

A `set` replaces the values its writer has seen. Of values set at the same time, the one with the later Lamport stamp
shows, then the higher writer. A `delete` removes the values its writer has seen, so a set made elsewhere at the same
time wins.

### DocSet

| Method | Returns |
|---|---|
| `add(value)` | The new item's id (opaque, the same on every member); `null`, with a warning, when `DocMap.set` would return `false`. |
| `get(id)`, `has(id)` | The item's value or `undefined`; whether it is held. |
| `delete(id)` | `false` when no such item is held. Undo is deleting your item. |
| `entries()` | `readonly { id, value, from }[]`, oldest first, by Lamport stamp then writer: the same order on every member (a whiteboard's z-order). |

### DocLog

| Method | Returns |
|---|---|
| `append(value)` | The new entry's id; `null`, with a warning, when `DocMap.set` would return `false`. |
| `get(id)` | The entry's value, or `undefined`. |
| `entries()` | `readonly { id, value, from }[]`, oldest first, by Lamport stamp then writer: an entry appended after seeing another sorts after it. |

**Writes throw** `RangeError` for a key over 256 characters or a value over 64 KiB as JSON or nested over 64 deep,
`TypeError` for a value that isn't JSON, and `Error` after `close()`: these are the app's own mistakes. Values come
back frozen.

### DocChange

What `doc.on('change', fn)` reports: one event per key or item that a write or a member's frame changed.

| Field | |
|---|---|
| `name` | The collection's name, as given to `map()`, `set()` or `log()`. |
| `type` | `'map'`, `'set'` or `'log'`. |
| `key` / `id` | A map's key, or a set item's or log entry's id. |
| `value` | What the key or item holds now; `undefined` once it is gone. |
| `deleted` | `true` when the key or item is gone: deleted, or trimmed from a log. |
| `from` | The writer of the value now shown or, for a deletion, the member it came from (us for our own writes), or `'relays'` for one in a save joined from the relays. |
| `local` | `true` for our own write. |

`from` is the writer as the doc was told: a member relaying a catch-up can name any writer, so use it to colour or
group, not as proof. A value is member data: check every field you render.

### DocSync

`doc.sync`, and what `doc.on('sync', fn)` reports when it changes.

| Field | |
|---|---|
| `state` | `'alone'`: no other member to catch up from. `'loading'`: catching up from `from`. `'synced'`: caught up. |
| `from` | The member being caught up from, while `'loading'`; `null` when it waits to ask one, and in the other states. |
| `have`, `total` | Entries of `from`'s answer that have landed, newest first, of the `total` it says it carries: 0 of 0 until its first piece lands. `have` may stop short of `total`: the last piece ends the answer. |
| `source` | `'members'` once caught up from them; else `'relays'` once the doc joined a save from the relays; `null` before either. |
| `at` | With `source` `'relays'`: when that save was made, in ms, as its saver claims (one more than 15 minutes past this device's clock counts as the time it was read). For display only. `null` otherwise. |

It is as good as the members that told us: use it for a status line, not to decide what the doc holds.

### DocStore

```ts
interface DocStore {
  get(key: string): Promise<Uint8Array | ArrayBuffer | undefined>;
  set(key: string, bytes: Uint8Array): Promise<void>;
  delete(key: string): Promise<void>;
}
```

Where a doc (or an inbox) kept on this device keeps its copy, for `persist`. `get` gives back what `set` stored,
`undefined` only when there is none, and rejects when it can't tell. `set` stores all of it or nothing (over files:
a temp file, then a rename). Keys are 64 hex characters. A file store is in
[Keep a copy on the device](../guides/shared-documents.md#keep-a-copy-on-the-device).

### Doc limits

From `packages/js/src/doc.js`; `crates/peerwire-sync` has the same.

| Limit | Value | Constant |
|---|---|---|
| Doc and collection names | 1 to 128 UTF-16 code units | `MAX_NAME_LENGTH` |
| Map keys | 256 characters | `MAX_KEY_LENGTH` |
| A value | 64 KiB as JSON text, nested 64 deep | `MAX_VALUE_LENGTH`, `MAX_VALUE_DEPTH` |
| Entries in a doc, all collections | 10,000 | `MAX_ENTRIES` |
| A doc's JSON text | 4 MiB | `MAX_DOC_LENGTH` |
| A log's `keep` | 1 to 10,000, default 1000 | `DEFAULT_KEEP` |
| A doc frame | 512 KiB as UTF-8 | `MESSAGE_BYTES` |

A doc that members have filled refuses writes with a warning (`false` or `null`), as every member refuses a write
that doesn't fit. Deleting frees room.

## Call

```ts ignore
room.call(stream: MediaStream | null, options?: { max?: number }): Call
```

Joins the room's voice and video call with `stream` (`null` only receives). Every member in the call sends its
stream to every other (a mesh), each over a WebRTC connection of its own, signalled over the room's links. The app
gets the stream (`getUserMedia`); the library never asks for the camera or microphone. How it works and what it
costs: [Calls](../guides/calls.md).

**Throws:** while our call is open (one call per room) and after `leave()`; `TypeError` where WebRTC has no media
(Node) or for a stream that isn't a `MediaStream`; `RangeError` for a `max` that isn't 2 to 16.

| Option | Type | Default | |
|---|---|---|---|
| `max` | `number` | 6 (`DEFAULT_MAX` in `call.js`) | The most in the call, us included: 2 to 16 (`MAX_CALL`). A member with no seat for a joiner tells it the call is full; the joiner goes only if its own count of who is in agrees. |

| Property or method | |
|---|---|
| `peers: PeerId[]` | The peers in the call with us, failed ones too, until `'ended'`. |
| `left: boolean` | True after `leave()`, `'full'`, or the room's `leave()`. Nothing fires after it but `'full'`. |
| `on(event, fn)`, `off(event, fn)` | Subscribe (returns a function that unsubscribes) and unsubscribe. |
| `state(peer)` | `'connecting'`, `'connected'`, `'failed'`, or `null` for a peer not in the call with us. |
| `stream(peer)` | The peer's `MediaStream` once `'stream'` has fired, else `null`. |
| `getStats(peer)` | The peer's connection stats (`RTCStatsReport`: inbound-rtp's `framesDecoded`, say), or `null` for a peer not linked or failed. |
| `setStream(stream)` | Sends `stream`'s first audio and first video track from now on, or nothing for a kind it lacks (`null`: nothing at all). Replaces tracks; never renegotiates. Throws after leaving. |
| `leave()` | Leaves the call and tells the members; the room stays. |

| Event | Arguments | When |
|---|---|---|
| `'state'` | `(peer, state, reason?)` | A peer's media connection: `'connecting'` once its link is made (and again while an ICE restart runs), `'connected'`, or `'failed'` with a `reason`. A failed peer is not tried again until it or we join again. |
| `'stream'` | `(peer, stream)` | Once per peer, when its description is applied: one audio and one video track, whatever it sends. The tracks stay the same when it changes what it sends; they are silent or black while it sends nothing. |
| `'ended'` | `(peer, reason)` | A peer in `call.peers` is out: it left the call (`'left'`) or the room (`'gone'`). |
| `'full'` | `(max)` | The call had no seat for us: we are out of it (`call.left`), and `room.call()` may try again. |

| Failure `reason` | |
|---|---|
| `'no-path'` | It never connected within 20 s (`CONNECT_MS`): no direct path between the two, and no TURN server in `iceServers`. |
| `'lost'` | It connected, then ICE failed and 2 restarts in a minute (`MAX_RESTARTS`, `RESTART_WINDOW_MS`) didn't bring it back. |
| `'error'` | Its description couldn't be applied. |

## openInbox

```ts
function openInbox(options: InboxOptions): Promise<Inbox>
```

```js
import { openInbox } from '@peerwire/js';

const inbox = await openInbox({ appId: 'my-app', secret, persist: true });
```

Opens an inbox: messages to one peer, by its PeerId, while it is offline. Each is sealed to the recipient and left on
Nostr relays, and read the next time the recipient opens its inbox. No room is needed, nor anyone online. How it
works and what a relay sees: [Offline messages](../guides/offline-messages.md).

| Option | Type | Default | |
|---|---|---|---|
| `appId` | `string` | required | A message is read only in the app it was left for. |
| `secret` | `Uint8Array` (32 bytes) | a fresh identity | The identity whose inbox it is (`room.endpoint.secret()`). A fresh one can only send, as nobody knows its PeerId. |
| `relays` | `string[]` | [`DEFAULT_RELAYS`](#constants) | Relays to leave and read messages on. |
| `persist` | `boolean` \| [`DocStore`](#docstore) | off | Remembers which messages were delivered, across sessions: `true` for IndexedDB, or a store. Needs `secret`; sealed under a key from it. Without it, a message still on the relays is delivered again in each new session. |
| `wasm` | `string` \| `URL` \| `BufferSource` | next to the package | As for `joinRoom`. |

| Property or method | |
|---|---|
| `id: PeerId` | Our PeerId: give it to those who may write to us. |
| `on('message', fn)` | `fn(data, from, { kind, at, id })` for each message, once, oldest first. The first listener starts reading: at once, then every minute (`CHECK_MS`); nothing is read while nobody listens. Returns a function that removes it. |
| `off('message', fn)` | Removes a listener. |
| `send(to, data, { ttl }?)` | Leaves `data` (a string, bytes or any other value as JSON) for `to`, for `ttl` whole seconds: 1 to 604,800 (7 days, the default). Resolves to the number of relays that took it, once each has answered. |
| `check()` | Reads the relays now; resolves to the number of messages delivered. |
| `close()` | Stops reading and closes the relay connections. |

**`send` throws** `RangeError` for a message over 16 KiB, a JSON value nested over 64 deep, or a `ttl` out of range;
`TypeError` for a `to` that isn't a PeerId; `Error` after `close()`.

| Message info | |
|---|---|
| `kind` | `'text'`, `'bytes'` or `'json'`: how it was sent. |
| `at` | When it was sent, in ms, by the sender's clock. |
| `id` | 32 hex characters, from the sender and the id it gave the message: the same if the message comes again, so an app that keeps ids can drop it. |

`from` is the sender's PeerId, authenticated: it sealed the message, and nobody else can read it. Anyone can leave a
message for any PeerId: check `from`, and every field you render.

## randomRoomId

```ts
function randomRoomId(): string
```

128 random bits as base64url: 22 characters. A roomId nobody can guess, for a [closed room](#closed-rooms), and the
shape `room.doc(name, { relays: true })` requires.

## memoryStore

```ts
function memoryStore(): DocStore
```

A [`DocStore`](#docstore) in this process's memory, for Node and tests. Its copies go with the process.

## Constants

| Constant | Value | Source |
|---|---|---|
| `DEFAULT_RELAYS` | `['wss://nos.lol', 'wss://relay.primal.net', 'wss://nostr.mom']` | `nostr.js` |
| `DEFAULT_ICE_SERVERS` | `[{ urls: 'stun:stun.l.google.com:19302' }, { urls: 'stun:stun.cloudflare.com:3478' }]` | `room.js` |
| `RECONNECT_MS` | 1000: a closed relay socket is dialled again after it, doubling | `nostr.js` |
| `MAX_RECONNECT_MS` | 30000: the longest wait between those dials | `nostr.js` |
| `MAX_MESSAGE` | 16384: the largest message `inbox.send` takes, in bytes | `inbox.js` |
| `MAX_TTL_S` | 604800: the longest a message stays on the relays, in seconds (7 days) | `inbox.js` |
| `Lane` | `{ Reliable: 0, Unreliable: 1, Urgent: 2 }` | `peerwire.js` |
| `EventKind` | `{ Connected: 1, Message: 2, Disconnected: 3, ConnectFailed: 4, PathChanged: 5, AddrObserved: 6 }` | `peerwire.js` |

## Through any NAT

WebRTC can't link two peers that are both behind symmetric NAT (common on mobile hotspots and carrier NAT) unless a
TURN server is configured. Rather than requiring one, `joinRoom` falls back per peer, on its own, with no setup on
either side. Every path carries the same end-to-end encrypted session. `fallback: 'peers'` skips step 3; `'off'`
keeps WebRTC only. TURN in `iceServers` still works, and is faster than step 3 when you have a server. The core keeps
the first path that completes the handshake, and the room's proofs, presence and messages run unchanged on it. See
also [NAT traversal](../concepts/nat-traversal.md).

1. **Direct WebRTC** (`route(peer).kind === 'direct'`). Two things help it through more NATs:
   - **Port prediction.** A browser behind a NAT that hands out ports in sequence sees it from its two STUN servers'
     answers (`natFromSdp`: one IPv4 address, two ports 1 to 8 apart). Its offer, answer or late candidates then
     carry `nat: { ip, next, step }`. A peer that isn't behind a symmetric NAT itself adds 32 candidates at the ports
     around `next` (`predictedCandidates`), so its checks open its NAT to the port the other's will come from. Peers
     that don't know the field ignore it.
   - **ICE restart.** When a link that was open loses ICE (a Wi-Fi to mobile switch, a hotspot dropping), it isn't
     dropped: after 2 s of `disconnected`, or at once on `failed`, the dialling side renegotiates with new ICE
     credentials; the other side asks it to. The DataChannel and the Peerwire session over it carry on, so the app
     sees no leave. At most 3 tries a minute (2, 4, 8 s apart), whether or not ICE connects in between; while it
     stays down, the next waits for the minute to free. `online` and `navigator.connection` changes restart links
     whose ICE is down at once. Each restart is an offer and an answer over signalling, within the peer's publish
     budget (6 at once, then 1 per 5 s; the candidates of an answer it refused wait with it).
   - **IPv6.** IPv6 has no NAT, so two phones on mobile data that both have it usually link directly, past the
     carrier NAT their IPv4 sits behind. Browsers gather the candidates themselves; nothing is filtered. In a field
     test (November 2026: two Android Chrome phones on Jio mobile data, one without microphone permission) the pair
     linked direct over IPv6 1.2 s after joining, at 31 ms ICE RTT. See
     [IPv6](../concepts/nat-traversal.md#ipv6-dual-stack).
2. **Through another member** (`'peer'`). When a link isn't open `fallbackMs` (8 s) after its offer, or its ICE
   fails, the dialling side dials the peer again through every member it has a direct link to, including members
   that link later. The core forwards for linked peers (`connect_via`, 256 KiB/s per peer) and sees only ciphertext.
   - **Behind one NAT.** A peer whose ICE candidates name one of your public IPv4 addresses (a laptop on a phone's
     hotspot) gets `sameNatFallbackMs` (3 s) instead. Browsers find no direct path there unless they show their LAN
     addresses: mDNS hides them, and Android Chrome lists only its mobile-data interface. So such a pair links over
     signalling about 3 to 4 s after hearing each other, not 9 s. Strangers behind one carrier NAT match too, and
     seldom link direct either. The transport traces `same-nat`.
3. **Over the signalling relays** (`'signaling'`). If no member has answered within 1 s, or at once when there is no
   other member to ask, Peerwire datagrams also go as signalling messages (a *pipe*). On Nostr each batch is sealed
   to the peer again. This works wherever the relays are reachable. A pipe opens only for a peer whose hello proved
   the room key. See [Pipes over Nostr](#pipes-over-nostr) for its pace.

**A member beats signalling.** A link over signalling moves to a member that links directly to both of you later
(it joined, or its own WebRTC links opened). The room offers every such member, the core sends a sealed probe through
it, and the link moves there (`'peer'`) once the far end answers through the member, which it does only if it would
use that member too: one stray packet never moves it. If the member leaves, the link goes back to signalling at once
(after 15 s of silence if it vanishes without a word), then on to another member that answers. It is the same link
throughout, with no `'peer-leave'` or `'peer-join'`. A member never relays onto its own pipe.

**Direct still wins.** ICE goes on checking after the fallback starts. If the link's own DataChannel opens later
(two desktops on one Wi-Fi resolve each other's mDNS names), the link moves onto it, and `route(peer)` says
`'direct'` again:

- A relayed link punches over the channel and moves there once the punch is answered.
- A link over signalling sends its datagrams over the channel. The far end's core, hearing them there, moves the link
  onto it, and then ours does. The room sends one at once (an empty reliable frame), so an idle link moves too.
  Datagrams that still come over signalling after that were sent before the channel opened; they are dropped, so
  the link doesn't move back.

### WebRTC is tried again

While a peer is on a fallback, the room retries WebRTC with a new connection and leaves the working link alone. If the
new DataChannel opens, the link moves onto it: `route(peer)` becomes `'direct'`, the same session, with no
`'peer-leave'` or `'peer-join'`. A retry that doesn't open within `fallbackMs` closes quietly. The room retries:

- **on a backoff:** 20 s after the peer went on a fallback, then 40, 80 and 160 s, then every 5 min, each ±20 %;
- **after a network change:** `online`, or a new `navigator.connection.type` (a new speed estimate doesn't count);
- **after a microphone or camera grant**, with privacy `'direct'`: browsers show their local addresses only to
  connections made after one, so a laptop on a phone's hotspot can then link to the phone directly (when both pages
  have the permission). `retryOnGrant: false` turns this off. With `'public-only'`, retries offer no local address;
- **when the app asks:** `room.retryDirect(peer?)`.

Only the dialling side (the smaller id) offers; the other side asks it to. Each retry is an offer and an answer over
the signalling relays, so a room keeps them rare: one at a time, at least 2 s apart, peers in turn, and a peer's
requests count once per 20 s. A 20-peer room whose retries all fail makes about one per peer per 5 min, a few a
minute at most (`test/retry_backoff.mjs`). On the answering side, a peer's retry offers make at most one new
connection per 2 s.

### A direct channel found later

A DataChannel to the peer opened outside the room (a browser birthday punch's, `src/birthday.js`, as Net Lab runs
one) can take a fallback link over. Both sides call `room.adoptChannel(peer, pc, dc)`. Before anything moves, the two
ends agree over the channel: each sends a one-byte marker until it hears the other's, so a link never moves onto a
channel only one side reads. A link over signalling then sends its datagrams over the channel at once. A relayed link
moves once the endpoint's punch over the channel is answered (`endpoint.addCandidate`). `route(peer)` then says
`{ kind: 'direct', adopted: true }`. When the channel closes, a link over signalling goes back at once; a relayed one
goes back to its member after the endpoint's 15 s direct timeout.

### Pipes over Nostr

A pipe is slow: a relay round trip per batch, 40 ms batching (a batch's worth goes at once), and at most 6 batches a
second for all of a client's pipes together (`PIPE_FLUSH_MS`, `PIPE_RATE` in `webrtc.js`). That is within what
public relays allow one client. A background tab fires its timers once a second at most: a pipe timer that fires
that late sends every batch the rate allows by then, and until one fires on time again, pipes send what each task
queued at its end.

- **Pacing.** Each relay connection is paced on its own: at most 4 events a second (a burst of 8), the rest waiting
  on its queue for up to 10 s (`RELAY_RATE`, `RELAY_QUEUE_MS` in `nostr.js`). While no relay can take one, a pipe
  holds its batch, which fills meanwhile. Relays don't publish their rate (NIP-11 has no field for it), so each
  relay's is learnt from its refusals. One that refuses an event for its rate (`rate-limited:`) halves its rate,
  which grows back by one event a second every 5 s. It is held for 1 s (doubling to 30 s while it refuses again
  before taking one), and is sent the event again, 3 times in all. A relay isn't held while no other is free, so a
  join or a pipe always has a relay to go through.
- **Turns.** Each pipe batch goes through one relay, the relays taking turns, and every other signal through each.
  A relay that holds us for its rate, banned us, isn't open, or is slowed by its refusals so that it couldn't take
  the batch within 1 s, is skipped. So a pipe moves at about the relays' rates together, up to its own 6 batches a
  second. A batch is lost when its relay drops it or the peer doesn't hear that relay, like a datagram on any path,
  and the core sends what it carried again: taking turns keeps that to one batch in three when one of three relays
  drops them all.
- **Silent relays.** A relay that has answered nothing for 5 s since we sent it an event (relays answer each one
  with an OK within a round trip) is skipped too, while any relay that answers is left, and still sent every other
  signal; its next answer gives it its turns back. A socket that has heard nothing at all for 10 s since a signal went
  through it is closed and dialled again, and the new one takes no batch until its relay answers. The same relay is
  redialled after 20 s, then 30 s, and at most 3 times until it answers within 5 s again on a socket dialled 30 s ago
  or more, as relays ban an IP that opens too many connections.
- **Reconnects.** A socket that closes is dialled again after 1 s, doubling to 30 s (`RECONNECT_MS`,
  `MAX_RECONNECT_MS`), and back to 1 s only once a socket has stayed open 30 s: a relay that takes each connection
  and drops it is dialled twice a minute at most. Each time a relay opens again the room says hello through every
  relay. Going online, a new connection type or the page showing again redials at once and starts over (a
  `navigator.connection` 'change' that is only a new speed estimate redials nothing); going online redials every
  relay.
- **Bans.** A relay that bans us (`blocked:`, `banned`) gets nothing for 30 s, doubling up to 10 min; the ban starts
  over at 30 s once the relay has gone 10 min without one.
- **Leaving.** The last batches a room sends as it leaves go through every relay, as nothing could send them again.

**What the fallback reveals.** Every member the dialler is linked to learns that the two peers couldn't link
directly. The member that relays sees the timing and volume of their traffic, never its content; it can also move
the link onto its own path, which gives it no more power than dropping the traffic would. On a pipe, the Nostr relays
see a steady stream between two tags for the whole session, where WebRTC sends them only a few signalling events.
They see no content and, unlike WebRTC, the peer learns no IP address.

## Privacy

A WebRTC peer sees the IP addresses in your ICE candidates and, once linked, the one your packets come from.
`joinRoom`'s `privacy` takes the first two of these modes, and applies to the room's calls too; `WebRtcTransport`
takes all three. See also [Privacy](../guides/privacy.md).

| Mode | What peers get |
|---|---|
| `'direct'` (default) | Every candidate. |
| `'public-only'` | No `host` candidates (LAN, loopback, mDNS `.local` names), in trickled ICE and in the SDP: only what STUN reports, so two tabs on one machine with no STUN server can't link this way. No other local address either: a candidate's related address (its local socket, which browsers show once a page has microphone or camera permission) is cleared, and so is the default address gathering writes into the SDP's `c=` line and m-line port. |
| `'relay-only'` with `relay: <peer id>` (`WebRtcTransport` only) | WebRTC runs with the relay only (and public-only); offers from anyone else are ignored and `dial()` to them throws. Reach other peers with `endpoint.connectVia(relay, peer)`: the endpoint is relay-only (`Endpoint.setRelayOnly`), so those links never punch to a direct path, and the relay is asked not to pass on your address. The relay sees both ends. There is no TURN: a peer not linked to the relay can't be reached. |

**With microphone or camera permission.** Until a page has it, browsers hide its local addresses (behind mDNS names;
Android Chrome offers only its mobile-data interface). Connections made after the grant list every network interface
(RFC 8828 mode 1), which affects every voice or video app. The grant is stored per origin, so it also applies on
later visits and to pages of that origin that never ask.

- `'direct'`: members get each interface's real address: LAN, hotspot and IPv6 ones. That is also what lets a phone
  and a laptop on its hotspot link directly. In one field test (October 2026: Android Chrome on mobile data, a laptop
  on its hotspot) the pair linked direct in 1.2 s, at about 10 ms, when both pages had microphone permission, and
  over the signalling relays in 5 to 6 s, at about 600 ms, when they didn't.
- `'public-only'`: none of those, and so not that direct path either. Each interface's public address as STUN sees it
  still goes (the Wi-Fi's and the mobile network's, or the real network's under a VPN). On IPv6 that public address
  is the device's own, as there is no NAT: the same one `'direct'` sends as a host candidate.

## loadPeerwire

```ts
function loadPeerwire(source: string | URL | BufferSource): Promise<Peerwire>
```

Loads the wasm module from a URL (fetched; in a browser, relative and root-relative ones too), an absolute path or
`file:` URL (read from disk, in Node only), or its bytes. A fetch with an HTTP error status rejects with it.
`joinRoom` and `openInbox` load it once per source and share it.

| `Peerwire` method | |
|---|---|
| `endpoint({ secret?, shareAddresses?, relayOnly? }?)` | A new [`Endpoint`](#endpoint). `secret`: 32 bytes restoring an identity (default `null`, a fresh one). `shareAddresses` (default `false`): tell peers the addresses we see; leave it off where addresses are local handles, as in the browser. `relayOnly` (default `false`): see `setRelayOnly`. |
| `nostrKeypair()` | `{ secret, pubkey }`: a fresh Nostr (BIP-340) key pair, 32 bytes each. |
| `nostrSign(secret, msg32)` | A 64-byte BIP-340 signature of a 32-byte message. |

## Endpoint

A Peerwire endpoint: one identity, its sessions, and the core's timers, over transport *handles* (numbers the
transport gives each peer's channel) rather than IP addresses. `joinRoom` makes one; use it directly to build your
own transport. Every method that takes a peer throws `TypeError` unless it is 32 bytes or 64 lowercase hex
characters: ids from signalling are untrusted and never reach wasm memory unchecked.

| Member | |
|---|---|
| `peerId: Uint8Array`, `id: PeerId` | Our identity, as bytes and as hex. |
| `onTransmit: (addr, bytes) => void` | Called with (transport handle, datagram) for each datagram to send. |
| `onEvent: (event) => void` | Called with each [`EndpointEvent`](#endpointevent). |
| `onDrain: (() => void) \| null` | Called at the end of a pump that handled incoming datagrams: ACKs may have freed send window, so a sender that got -3 can try again. |
| `transmitErrors: number` | Datagrams lost because `onTransmit` threw; the endpoint keeps running and retransmits. |
| `secret()` | The 32-byte identity secret; `null` after `close()`. |
| `connect(addr, peer)` | Dials `peer` at transport handle `addr`. |
| `connectVia(relay, peer)` | Dials `peer` through `relay`, a peer linked already. |
| `addCandidate(addr, peer)` | A direct address for a relayed (or dialling) peer: punches to it and moves the link there once the peer answers from it. |
| `setSlow(peer, addr \| null)` | Marks `addr` as a slow address of the linked `peer` (a signalling pipe), or clears the mark. |
| `addRelay(relay, peer)` | Offers `relay`, a peer linked directly, to carry the link to `peer` while that link is on its slow address, or to race the relay it is on. |
| `disconnect(peer)` | Ends the session and sends the peer a sealed close at once, so it sees `'disconnected'` (`'closed'`) now rather than after its idle timeout. |
| `setRelayOnly(on)` | Relayed links never punch to a direct path, and relays forward without our address. |
| `send(peer, data, lane?)` | 0, or a negative `PW_ERR` code: -1 not connected, -2 too large (over 1100 B unreliable, 64 KiB urgent, 1 MiB reliable), -3 would block. Throws after `close()`. |
| `sendRetry(peer, data, lane?)` | `send` for a caller that tries the same message again after -3: a message over 16 KiB stays in wasm memory between tries. `data` is matched by identity and must not change. |
| `receive(addr, bytes, peer?)` | A datagram that arrived on handle `addr`. `peer`: the only peer that sends on that handle, if there is one; then a packet there for another peer's link, or a handshake from anyone else, is dropped unread. Ignored after `close()`. |
| `pump()` | Drains the core's outgoing datagrams and events, and re-arms its timer. |
| `seal(peer, bytes)`, `open(bytes)` | Seal bytes to a PeerId, and open what was sealed to us (`{ from, data }`); `null` on failure or after `close()`. |
| `isConnected(peer)`, `rttMs(peer)`, `pathRttMs(peer)` | Whether a session is up; its smoothed RTT; the RTT over the path it is on now, measured since it moved there. |
| `stats()` | [`EndpointStats`](#endpointstats); after `close()`, the counters as they were. |
| `close()`, `closed` | Frees the endpoint (a second call does nothing); `closed` is true after. Close the transport first. |

`setSlow`, `addRelay`, `addCandidate`, `disconnect` and `setRelayOnly` have more to their rules than fits here: their
comments in `index.d.ts` are the reference. The numbers in them (a probe every half second for 5 s, a relay held off
for a minute and up to 16 minutes, 5 rounds 2 s apart) are the core's.

### EndpointEvent

```ts
type Path = { direct: number } | { via: PeerId };
type EndpointEvent =
  | { type: 'connected'; peer: PeerId; initiator: boolean; path: Path }
  | { type: 'message'; peer: PeerId; lane: number; data: Uint8Array }
  | { type: 'disconnected'; peer: PeerId; reason: 'timeout' | 'closed' | 'replaced' | 'violation' }
  | { type: 'connectFailed'; peer: PeerId }
  | { type: 'pathChanged'; peer: PeerId; path: Path }
  | { type: 'addrObserved'; peer: PeerId; addr: number }
  | { type: 'unknown'; kind: number };
```

`decodeEvent(bytes)` turns an event record from the module into one of these.

### EndpointStats

Counters, as `endpoint.stats()` and `room.stats()` return them: `txPackets`, `rxPackets`, `rxMalformed`,
`rxUnknownSession`, `rxAuthFailed`, `rxReplayed`, `rxHandshakeDropped`, `retransmits`, `unreliableDropped`, `roams`,
`relayed`, `relayDropped`, `punchesSent`, `punchSucceeded`, `punchFailed`, `directFallbacks`.

## WebRtcTransport

```ts ignore
new WebRtcTransport(endpoint: Endpoint, signaling: SignalingAdapter, options?)
```

One negotiated, unordered, no-retransmit DataChannel per peer, signalled through `signaling`. `joinRoom` builds one;
build your own for relay-only privacy or a custom room.

| Option | Default | |
|---|---|---|
| `iceServers` | `[]` | STUN and TURN servers. |
| `onPeer(peer, state, handle?)` | none | `'open'`, `'closed'` (with the closed link's endpoint handle), `'failed'` (the channel isn't open `fallbackMs` after its offer or answer, or ICE failed), `'carrier'` (an adopted channel started or stopped carrying the link), `'retry-failed'` (a retry's connection didn't open), `'retry'` (the other side asks us, the dialling side, for one). |
| `fallbackMs` | 8000 | |
| `sameNatFallbackMs` | 3000, at most `fallbackMs` | |
| `loss` | 0 | Simulated loss: the chance each datagram is dropped, for tests. |
| `trickle` | `true` | Send candidates as found; `false` sends one offer or answer with them (what Nostr rooms do). |
| `privacy` | `'direct'` | `'direct'`, `'public-only'` or `'relay-only'` (needs `relay`): see [Privacy](#privacy). |
| `relay` | `null` | The relay's PeerId, for `'relay-only'`. |
| `offerProof` | `null` | `{ sign(to), check(from, p) }`: offers carry `p: await sign(to)`, and an offer is answered only if `check(from, p)` holds. |
| `pipes` | `false` | Lets the transport carry datagrams as signalling messages (a pipe) for peers the room allows. `joinRoom` turns it on for `fallback: 'auto'` and opens the pipes itself (see [Through any NAT](#through-any-nat)). Off, pipe batches that arrive are refused (`badSignals`). |
| `RTCPeerConnection` | `globalThis.RTCPeerConnection` | |

**Throws** `Error` for an unknown `privacy`, `TypeError` for `'relay-only'` without a valid `relay`.

| Member | |
|---|---|
| `dial(peer, { retry }?)`, `has(peer)`, `drop(peer)`, `close()` | The [`Transport`](#transport) methods. |
| `networkChanged()` | Links that were open but whose ICE isn't connected restart ICE now, rather than after the 2 s grace. `joinRoom` calls it on `online` and on a `navigator.connection` change. |
| `adopt(peer, pc, dc, { pipe }?)` | Carries `peer`'s link over `dc` once the far end adopts its end too. Resolves to the channel's handle, or -1 if the far end didn't adopt within 5 s or `dc` closed first. `room.adoptChannel` uses it. |
| `carrier(peer)` | The adopted `{ pc, dc }` carrying `peer`'s link now, or `null`. |
| `retry(peer)` | As the dialling side, dials `peer` again with a new connection while its link runs by other means. `false` if the link is open or a retry is under way. |
| `askRetry(peer)` | Asks `peer`, the dialling side, for a retry. |
| `trace` | `(peer, what, detail) => void` or `null`: each link's signalling, retries, ICE states, port guesses, `same-nat` and `throttled`. Net Lab shows it. |

| Counter | |
|---|---|
| `dropped` | Datagrams dropped by `loss`. |
| `congested` | Datagrams dropped because the DataChannel had over 1 MiB queued (`MAX_BUFFERED`) or `send` threw. |
| `badSignals` | Signals refused: a `from` that isn't a PeerId, an empty message, an offer whose proof fails or whose SDP doesn't apply, a second retry connection within 2 s, an `ice-batch` that isn't a list of at most 32, a pipe batch from a peer not allowed one (or with `pipes` off) or that isn't a list of at most 64 base64 datagrams. |
| `resent` | Offers sent again for want of an answer, and answers sent again for a repeated offer. |
| `throttled` | Offers, answers and restart asks not sent because the peer's publish budget was spent: 6 at once, then 1 per 5 s per peer. |
| `restarts` | ICE restarts begun as the dialling side: at most 3 per link a minute. |
| `retried` | Retries begun as the dialling side. |

## LocalTransport

```ts ignore
new LocalTransport(endpoint: Endpoint, signaling: BroadcastSignaling)
```

Datagrams over the BroadcastChannel, for Node and tests: peers in one process or its workers, or tabs of one
browser. `joinRoom` uses it for `'local'` rooms when the runtime has no WebRTC. It has the
[`Transport`](#transport) methods.

## BroadcastSignaling

```ts ignore
new BroadcastSignaling(id: string, room?: string)
```

A [`SignalingAdapter`](#signalingadapter) over a BroadcastChannel, for tabs of one browser or one Node process.
`id` is our PeerId; `joinRoom` names the channel `peerwire/<appId>/<roomId>`.

## NostrSignaling

```ts ignore
new NostrSignaling(pw: Peerwire, endpoint: Endpoint, options: { room: string; relays?: string[] })
```

A [`SignalingAdapter`](#signalingadapter) over public Nostr relays (default [`DEFAULT_RELAYS`](#constants)):
ephemeral events, a throwaway Nostr key per session, payloads sealed to the recipient's PeerId. `room` is
`<appId>/<roomId>`; relays see only its hash. See [Pipes over Nostr](#pipes-over-nostr) for its pacing.

| Member | |
|---|---|
| `ready: Promise<void>` | Resolves once it can publish. |
| `send(to, body, { lossy }?)` | `lossy`: through one relay, the relays taking turns, not each. |
| `wait()` | Milliseconds until the relay the next lossy event goes to could take it (0: now). |
| `onReconnect: (url) => void` | A relay opened again after its socket closed or heard nothing. |
| `stats` | `published`, `received`, `rejected` (events refused), `relaysOpen`, `reconnects`, `backoffs` (relays held for their rate or stopped for a ban), `resent`, `unsent` (dropped from a relay's queue past 64 waiting or 10 s), `redials`, `lastRejection`. |

It also has `hello`, `onHello`, `onMessage` and `close`, as every adapter does. It remembers 4096 event ids
(`MAX_SEEN`) to drop copies.

## SignalingAdapter

```ts
interface SignalingAdapter {
  readonly id: string;
  readonly ready?: Promise<unknown>;
  onHello: (peer: PeerId, meta?: unknown) => void;
  hello(meta?: unknown): void | Promise<void>;
  send(to: PeerId, body: unknown, options?: { lossy?: boolean }): void | Promise<void>;
  wait?(): number;
  onMessage(fn: (from: PeerId, body: any) => void): void;
  close(): void;
}
```

What `joinRoom({ signaling: (ctx) => adapter })` needs. `ctx` is `{ endpoint, peerwire, room }`, with `room` as
`<appId>/<roomId>`.

- `onHello` is called with each peer that announces itself; `hello` announces us.
- `onMessage`'s `from` **must** be the sender's PeerId, authenticated by the adapter (as the Nostr adapter's sealed
  messages are; BroadcastChannel is same-origin): offers, answers, candidates, restarts, retry requests and the
  signalling pipe all trust it.
- `lossy` marks a signalling-pipe batch, which the sender resends if it is lost, so it may go one way rather than
  every way the adapter has.
- `wait()`, for an adapter that paces its sends: milliseconds until it can take one more (0: now). A pipe waits this
  long, at most a second at a time. Without it, the pipes send at their own rate, 6 batches a second with a burst of
  12.

## Transport

```ts
interface Transport {
  dial(peer: PeerId): void | Promise<void>;
  has(peer: PeerId): boolean;
  drop(peer: PeerId): void;
  close(): void;
}
```

## Helpers

| Function | |
|---|---|
| `toHex(bytes)`, `fromHex(hex)` | Bytes to lowercase hex and back. |
| `isPeerId(id)` | Whether `id` is a PeerId as text: 64 lowercase hex characters. Check ids from signalling with it. |
| `peerBytes(id)` | A PeerId as 32 bytes; throws `TypeError` unless `id` is 32 bytes or 64 lowercase hex characters. |
| `decodeEvent(bytes)` | An [`EndpointEvent`](#endpointevent) from the module's event record. |
| `withoutHostCandidates(sdp)` | The SDP without `host` candidates (LAN, loopback, mDNS). |
| `candidateSummary(sdp)` | `{ host4, host6, mdns, srflx4, srflx6, prflx, relay }`: an SDP's candidates by type and family. |
| `natFromSdp(sdp)` | `{ type: 'symmetric' \| 'cone' \| 'unknown', ip?, next?, step? }`: the NAT as the server-reflexive candidates show it, with a prediction for a sequential symmetric one. |
| `predictedCandidates(nat, sdpMid?)` | ICE candidates at the ports around a peer's prediction, for `addIceCandidate`; `[]` unless it is an IPv4 address and ports in range. |

## Source files and demos

| File | What it is |
|---|---|
| `src/index.js` | The package entry: `joinRoom` and everything below. |
| `src/room.js` | `joinRoom` and `Room`: mesh, presence, message framing, backpressure. |
| `src/doc.js` | `room.doc()`: shared documents (maps, sets, logs), a delta-state CRDT, catch-up, limits on member data. |
| `src/store.js` | A doc's sealed copy on this device (`persist`); `memoryStore()`. |
| `src/relays.js` | A doc's sealed copy on the Nostr relays (`relays: true`). |
| `src/inbox.js` | `openInbox`: offline messages to one PeerId, sealed and left on the Nostr relays. |
| `src/call.js` | `room.call()`: a room's voice and video call, a mesh of WebRTC media connections signalled over the room's links. |
| `src/proof.js` | Room key and HMAC proofs (hello, offer, link), compatible with Rust's; `randomRoomId()`. |
| `src/peerwire.js` | Loads the module; `Endpoint` (connect, send, events, stats, seal/open) and Nostr key helpers. Browser and Node. |
| `src/webrtc.js` | `WebRtcTransport`: one negotiated, unordered, no-retransmit DataChannel per peer. `BroadcastSignaling` for tabs of one browser. |
| `src/retry.js` | When a room tries WebRTC again with the peers it reaches only by fallback. |
| `src/birthday.js` | A browser birthday punch, for two peers that can't punch directly; Net Lab runs it. |
| `src/nostr.js` | `NostrSignaling` over public relays: ephemeral events, a throwaway key per session, payloads sealed to the recipient's PeerId. |
| `src/local.js` | `LocalTransport`: datagrams over the BroadcastChannel, for Node and tests. |
| `src/index.d.ts` | TypeScript types. |

The demos are in `packages/js/demo/`: build the wasm with `packages/js/build.sh`, then serve `packages/js` with any
static server (`python3 -m http.server -d packages/js 8000`).

| Demo | What it is |
|---|---|
| `demo/index.html` | Mesh chat on the low-level API: every tab links to every other. |
| `demo/chat.html` | Chat on `joinRoom`, mobile-friendly: a room per link (`#<room>`; New room makes one with `randomRoomId()`), an invite link with a copy button and a QR code, nicknames in presence, members with their route and RTT, a typing indicator on the unreliable lane, and messages on the reliable lane (up to 2000 characters), shown as text only. Nostr by default; `?signal=local` for tabs of one browser. |
| `demo/meet.html` | A video call of up to 6 browsers (`room.call`): turn the camera on, join, and send the invite link. `?signal=local` for tabs of one browser. |
| `demo/poll.html` | Pull-style room: `poll()` and `sendTo()` once a frame, dots you move with the arrow keys. |
| `demo/arena.html` | Super-peer lockstep arena: `?role=super` in one tab, players in others; arrow keys move. |
| `demo/netlab.html` | Net Lab, a connection diagnostics page: join a room by name or a private `#key=` link; per peer, ping on the route in use, ICE round trips, WebRTC traffic, restarts and port guesses, the route and candidate pair, the offer/answer exchange and ICE checks with a verdict on which way packets are blocked, and the other side's NAT; this device's public addresses and NAT mapping; the relays' state; chat, an event log and a JSON report. `?relays=` other Nostr relays; `?rtc=off` fails this tab's WebRTC links. |
| `demo/crew.html` | Crew, a small Among Us-like game: the host is authoritative and relays for every player. `?turn=turn:HOST:PORT&tu=USER&tp=PASS` adds a TURN server; `?rtc=off` fails this tab's WebRTC links. `demo/crew-server.mjs` serves it. |

Add `?signal=nostr&room=<something only you know>` to a demo page to reach other machines through the default relays
and Google's STUN.

## Tests

Run these with Node and the Playwright install on this host, from the repo root as `node packages/js/test/<name>`.
Those marked (Chromium) open headless browser tabs on loopback. None touches a public relay but `nostr.mjs`.

- `node_pair.mjs`: two endpoints in Node, checks the glue.
- `wasm_source.mjs`: where `loadPeerwire` reads the module from in Node: an absolute path or `file:` URL from disk, an
  `http:` URL by fetch (a 404 rejects with its status).
- `privacy.mjs`: host-candidate filtering, and a relay-only endpoint linking through a relay and refusing a direct
  dial.
- `room_node.mjs`: `joinRoom` with three peers in Node: presence, message types, lanes, targeted sends,
  backpressure, leaving, a late joiner and the pull-style `poll()`/`receive()`/`sendTo()` path.
- `peer_id.mjs`: untrusted peer ids. A malformed or over-long id throws a `TypeError` before any copy into wasm
  memory, and every signalling layer drops a `from` that isn't 64 lowercase hex characters.
- `nostr_bounds.mjs`: `NostrSignaling` on fake sockets: the de-duplication set stays at 4096 ids under 12,000
  events, malformed events don't throw, base64 round-trips bodies up to 1 MB; each relay's pacing, holds and bans;
  queues and `close()`; pipe batches taking turns, silent relays and redials; stored events (`query()`, `store()`);
  the pacing on a monotonic clock. No relays.
- `inbox.mjs`: `openInbox` on fake strfry-like relays and a fake clock: delivery once, what relays and
  non-recipients see, dropped messages, floods, the 4096 kept, the sealed state; end to end over `NostrSignaling`
  with fake sockets. No relays.
- `call.mjs`: calls with a fake `RTCPeerConnection` that negotiates like a browser's: every call frame cap and budget,
  privacy, offers and answers, `setStream`, ICE restarts, `'no-path'`, `'lost'` and `'error'`, seats and `'full'`;
  then real Node rooms. No relays.
- `rust_hello.mjs`: a Rust or Godot member's room hello is the same base64 that `sealRoom` makes, and a `Room`
  doesn't dial a native peer. No relays.
- `room_proof.mjs`: an attacker that knows only the room topic gets no dial, no answer and, on a link it dials
  itself, nothing but the link proof before it is dropped; the hello proof matches Rust's.
- `fuzz.mjs`: seeded random and mutated input to the JS parsers (Nostr events and relay messages, hex and peer ids,
  room hello proofs, room frames and shared document frames): nothing throws out of a handler of untrusted input and
  no promise is left rejected.
- `webrtc_signals.mjs` (Chromium): malformed offers leave no link, refused signals are counted in `badSignals`, and
  without trickle a lost first offer or answer still links (in about 3.2 s).
- `nat_predict.mjs`: NAT type and prediction from server-reflexive candidates, the 32 predicted candidates, and a
  peer's prediction refused unless it is an IPv4 address and ports in range.
- `ice_restart.mjs` (Chromium): ICE restarts by either side keep the DataChannel; the 2 s grace, `failed` at once,
  at most 3 a minute; `networkChanged()`.
- `restart_budget.mjs`: a peer can't keep us publishing to the signalling relays: 3 restarts, 6 answers to 200 copies
  of an offer, the rest counted in `throttled`.
- `pump_rearm.mjs`, `endpoint_closed.mjs`, `scratch_buffers.mjs`: the endpoint's timer survives a transport that
  throws; no method calls into wasm after `close()`; `send` and `receive` reuse their buffers.
- `room_drain.mjs`, `room_leave.mjs`, `room_welcome.mjs`: 6 MiB through a room over the send window; leaving reaches
  peers before `leave()` returns; a `'peer-join'` handler's message reaches the newcomer.
- `doc_crdt.mjs`: shared documents on fake rooms, seeded: convergence, intent, limits, frames, budgets, pieces, turns
  and routes, `'sync'`, Lamport stamps far ahead, claims on future writes, the copy on this device.
- `room_doc.mjs`: `room.doc()` with members in a real `'local'` room, deflated frames, and the sealed copy on this
  device and its file store.
- `doc_catchup.mjs`: a late joiner's catch-up of a whiteboard-sized board (509 KB) direct, over the pipe through fake
  rate-limited relays, from a background tab, and with a relay half-open, against time bars.
- `doc_store.mjs` (Chromium): `persist: true` in IndexedDB.
- `doc_relays.mjs`: `relays: true` on relays in the process that keep events as strfry does.
- `pipe_batches.mjs`, `room_fallback.mjs`, `room_adopt.mjs`, `room_retry.mjs`, `retry_backoff.mjs`,
  `room_same_nat.mjs`: the fallback paths of [Through any NAT](#through-any-nat).
- `qr.mjs`: the chat demo's QR encoder against known answers.
- `retry_direct.mjs`, `netlab*.mjs`, `crew.mjs`, `chat.mjs`, `meet.mjs`, `poll.mjs`, `leave.mjs`, `browser.mjs`,
  `arena.mjs` (Chromium): the demos and browser paths.
- `call_browser.mjs` (Chromium): calls with Chromium's fake camera and microphone, 2 to 4 tabs, a full call,
  `'public-only'` with no STUN, and a receive-only member.
- `readme.mjs` (Chromium): runs the chat from `packages/js/README.md` in two tabs.
- `wasm_url.mjs` (Chromium): a root-relative `wasm` URL, as a bundler gives.
- `room_throttled.mjs --ab=<other packages/js>`: a benchmark, not a test.
- `nostr.mjs`: two isolated browser profiles meet over public Nostr relays and connect. Run it only when needed.

What each test checks in full is in its header comment, and in [Testing](../project/testing.md).
