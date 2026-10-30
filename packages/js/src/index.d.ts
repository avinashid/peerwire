// Types for @peerwire/js.

/** A peer's id: its 32-byte Peerwire PeerId as 64 hex characters. */
export type PeerId = string;

/** Free-form data a peer shares with the room (a nickname, a colour); must survive JSON. */
export type Presence = any;

/** `reliable`: ordered, retransmitted. `unreliable`: latest-wins datagrams, one packet at most.
 * `urgent`: reliable, each fragment sent twice, ordered only among urgent messages (it overtakes queued
 * `reliable` data); arrives as `reliable`. Urgent messages are capped at 64 KiB encoded: `send` throws a
 * RangeError above that. */
export type LaneName = 'reliable' | 'unreliable' | 'urgent';

/** What `room.send` accepts. Strings arrive as strings, bytes as a Uint8Array, anything else as parsed JSON. */
export type RoomData = string | Uint8Array | ArrayBuffer | ArrayBufferView | number | boolean | null | object;

export interface SignalingAdapter {
  readonly id: string;
  /** Resolves once the adapter can publish (relays connected). */
  readonly ready?: Promise<unknown>;
  /** Called with each peer that announces itself in the room. */
  onHello: (peer: PeerId, meta?: unknown) => void;
  hello(meta?: unknown): void | Promise<void>;
  /** `lossy`: the sender resends what `body` carries if it is lost (a signalling-pipe batch), so it may go one way
   * rather than every way the adapter has (the Nostr adapter sends it through one relay, not each). */
  send(to: PeerId, body: unknown, options?: { lossy?: boolean }): void | Promise<void>;
  /** Milliseconds until the adapter can take one more message (0: it can now), for one that paces its sends, as the
   * Nostr adapter does. A signalling pipe waits this long, at most a second at a time, so its batches fill meanwhile.
   * Without it, the pipes send at their own rate, 6 batches a second with a burst of 12; an adapter that also ignores
   * `lossy` and sends to several relays puts all of that on each of them. */
  wait?(): number;
  /** `from` must be the sender's PeerId, authenticated by the adapter (as the Nostr adapter's sealed messages are;
   * BroadcastChannel is same-origin): WebRTC offers, answers, ICE candidates and restarts, retry requests, and the
   * signalling pipe and its stale drop all trust it. */
  onMessage(fn: (from: PeerId, body: any) => void): void;
  close(): void;
}

export interface SignalingContext {
  endpoint: Endpoint;
  peerwire: Peerwire;
  /** `${appId}/${roomId}` */
  room: string;
}

export interface JoinRoomOptions {
  /** Names your app; rooms of different apps never meet. */
  appId: string;
  /**
   * The room's name, and its only secret: peers prove they know it, so the topic
   * relays see lets no one in. A guessable name is a public lobby; for a closed
   * room use at least 128 random bits, e.g. `randomRoomId()`.
   */
  roomId: string;
  /**
   * How peers find each other.
   * - `'local'` (default): BroadcastChannel; tabs of one browser, or one Node process.
   * - `'nostr'`: public Nostr relays (`wss://nos.lol`, `wss://relay.primal.net`, `wss://nostr.mom`), across the internet.
   * - a factory returning your own adapter.
   */
  signaling?: 'local' | 'nostr' | ((ctx: SignalingContext) => SignalingAdapter);
  /** Shared with every peer when it joins; change it with `room.setPresence`. Default `{}`. `joinRoom` rejects
   * one over 1 MiB as JSON (RangeError) or one JSON can't encode, such as a function or a BigInt (TypeError). */
  presence?: Presence;
  /** Queue events for `room.poll()`/`room.receive()` from the start; otherwise the first call starts the queue. */
  queue?: boolean;
  /** 32 bytes restoring an identity from `room.endpoint.secret()`; a fresh one by default. */
  secret?: Uint8Array;
  /** Where to load `peerwire_ffi.wasm` from: a URL (in a browser, relative and root-relative ones such as a bundler's
   * `?url` import are fetched from the page's origin), a file path or `file:` URL (Node) or the bytes. Default: next to
   * this package. */
  wasm?: string | URL | BufferSource;
  /** Nostr relays, for `signaling: 'nostr'`. */
  relays?: string[];
  /** Default: none for `'local'`, Google and Cloudflare STUN otherwise. */
  iceServers?: RTCIceServer[];
  /** Which of your addresses peers see in ICE candidates. `'direct'` (default): all of them.
   * `'public-only'`: no LAN or loopback (`host`) candidates, only what STUN reports (and no local address in
   * a candidate's related address or the SDP's `c=` line either), so tabs of one
   * machine with no STUN server can't link. Relay-only is not available for rooms: use
   * `WebRtcTransport` with `privacy: 'relay-only'`.
   * Once the origin has a stored microphone or camera grant (it lasts across visits and covers every page of the
   * origin, even one that never asks), browsers list every network interface to connections made after the grant
   * (RFC 8828 mode 1). With `'direct'`, members then get each one's real address (LAN, hotspot and IPv6 ones),
   * which is what lets a phone and a laptop on its hotspot link directly. `'public-only'` withholds those, and that
   * direct path with them, but still sends each interface's public address as STUN sees it (the Wi-Fi's and the
   * mobile network's, or the real network's under a VPN). Voice and video apps ask for that permission anyway. */
  privacy?: 'direct' | 'public-only';
  /** What to do for a peer WebRTC can't link (symmetric NAT on both sides, no TURN):
   * - `'auto'` (default): dial it through every member linked to both of you, and over the
   *   signalling relays themselves, at once; the first path to answer wins. Signalling is
   *   slow (a relay round trip per batch, a few batches a second) but works on any network
   *   that reaches the relays.
   * - `'peers'`: through other members only.
   * - `'off'`: WebRTC only.
   * Every path carries the same end-to-end encrypted session. See `room.route(peer)`. */
  fallback?: 'auto' | 'peers' | 'off';
  /** How long a WebRTC link may take to open before the fallback starts. Default 8000 ms. */
  fallbackMs?: number;
  /** The same for a peer behind your own public IPv4 (its ICE candidates name a public address yours do), such
   * as a laptop on a phone's hotspot, where browsers find no direct path unless they show their LAN addresses.
   * Default 3000 ms (at most `fallbackMs`). ICE goes on checking: if the link's DataChannel opens later (two
   * desktops on one Wi-Fi), the link moves onto it and `route(peer)` says `'direct'` again. */
  sameNatFallbackMs?: number;
  /** While a peer is reached by fallback, the room tries WebRTC again with a new connection (see `retryDirect`),
   * and also when the page is granted the microphone or camera: browsers show their local addresses only to
   * connections made after such a grant, so with privacy `'direct'` the retry offers them to that peer. `false`
   * keeps a grant from making new connections (Net Lab's global room rejoins as `'public-only'` instead). Has no
   * effect with `'public-only'`, which never offers them. Default true. */
  retryOnGrant?: boolean;
  /** A WebRTC implementation for runtimes without one (Node: node-datachannel's polyfill).
   * Without WebRTC, `'local'` rooms send datagrams over the BroadcastChannel. */
  RTCPeerConnection?: typeof globalThis.RTCPeerConnection;
}

export interface MessageInfo {
  lane: 'reliable' | 'unreliable';
  /** How it was sent: a string, bytes, or any other value as JSON. */
  kind: 'text' | 'bytes' | 'json';
}

/** A queued event from `room.poll()`/`room.receive()`: plain values only, for hosts such as Godot's JavaScriptBridge. */
export type RoomEvent =
  | { type: 'peer-join'; peer: PeerId; presence: Presence }
  | { type: 'presence'; peer: PeerId; presence: Presence }
  | { type: 'peer-leave'; peer: PeerId; reason: string }
  | { type: 'message'; peer: PeerId; data: string | ArrayBuffer | any; kind: 'text' | 'bytes' | 'json'; lane: 'reliable' | 'unreliable' };

export interface RoomEvents {
  /** A peer connected and shared its presence. */
  'peer-join': (peer: PeerId, presence: Presence) => void;
  /** A peer left, timed out or lost its connection. A peer that leaves (`leave()`, or its tab closing) shows
   * as 'left' or 'closed' at once; one that crashes or loses its network shows as 'timeout' after the idle timeout (30 s). */
  'peer-leave': (peer: PeerId, reason: 'left' | 'closed' | 'failed' | 'timeout' | 'replaced' | 'violation') => void;
  /** A joined peer changed its presence. */
  presence: (peer: PeerId, presence: Presence) => void;
  message: (data: any, peer: PeerId, info: MessageInfo) => void;
}

export interface SendOptions {
  /** One peer or several; default every joined peer. */
  to?: PeerId | PeerId[];
  /** Default `'reliable'`. A number must be 0, 1 or 2 (reliable, unreliable, urgent); anything else throws
   * TypeError. */
  lane?: LaneName | number;
}

export declare class Room {
  /** Our own PeerId. */
  readonly self: PeerId;
  /** Joined peers, not counting us. */
  readonly peers: PeerId[];
  /** The Peerwire endpoint underneath, for stats, sealing and identity. */
  readonly endpoint: Endpoint;
  readonly signaling: SignalingAdapter;
  /** Subscribes; returns a function that unsubscribes. */
  on<E extends keyof RoomEvents>(event: E, fn: RoomEvents[E]): () => void;
  off<E extends keyof RoomEvents>(event: E, fn: RoomEvents[E]): void;
  /** Sends to every joined peer, or to `to`. Returns how many peers it was queued for.
   * Throws RangeError if the message can't fit the lane (reliable: 1 MiB; unreliable: one datagram; urgent:
   * 64 KiB), also while earlier messages wait for the send window, and then sends it to no one. */
  send(data: RoomData, options?: SendOptions): number;
  /** `send` with positional arguments; `peer` '' or null sends to everyone. */
  sendTo(peer: PeerId | '' | null, data: RoomData, lane?: LaneName | number): number;
  /** Every queued event, oldest first, and clears the queue. The first call starts queueing. */
  poll(): RoomEvent[];
  /** The next queued event, or null. The first call starts queueing. */
  receive(): RoomEvent | null;
  /** Our presence, or a joined peer's. */
  getPresence(peer?: PeerId): Presence | undefined;
  /** Throws RangeError for a presence over 1 MiB as JSON, TypeError for one JSON can't encode (`undefined`, a
   * function, a Symbol, a BigInt, a cycle), and keeps the old one. */
  setPresence(presence: Presence): void;
  /** How `peer` is reached: its own WebRTC link, a channel handed to `adoptChannel` (`adopted`), relayed by
   * member `via`, or over the signalling relays; null while it isn't linked. A `'peer'` or `'signaling'` route
   * can become `'direct'` later, with the same session (no `peer-leave` or `peer-join`): ICE goes on checking,
   * the room retries WebRTC (see `retryDirect`), or a channel is adopted. A `'signaling'` route also moves to a
   * member linked directly to both of you once that member is seen to relay both ways, and back if it goes; a
   * `'peer'` route to another such member once it is clearly faster (20 % and 15 ms, in 3 of 5 probe rounds 2 s
   * apart). `rttMs`: the round trip over this path, measured since the link moved there; null until then. */
  route(peer: PeerId): ({ kind: 'direct'; adopted?: true } | { kind: 'peer'; via: PeerId } | { kind: 'signaling' }) & { rttMs: number | null } | null;
  /** Tries WebRTC again, now, with `peer` (or every peer) the room reaches only by fallback: a new connection,
   * so one made after a network change, or a microphone or camera grant (privacy `'direct'`), offers the new
   * addresses. If its DataChannel opens, the link moves onto it and `route(peer)` says `'direct'`; if not, it
   * closes quietly and the link stays on its fallback. The room also retries on its own: 20 s after a peer goes
   * on a fallback, then 40, 80, 160 s, then every 5 min (each ±20 %), and on `online`, a new connection type
   * and a grant (see `retryOnGrant`). Retries take turns, one at a time per room, at least 2 s apart; only
   * the dialling side (the smaller id) offers, so the other side asks it. Returns how many peers are to be
   * retried. */
  retryDirect(peer?: PeerId): number;
  /** Moves `peer`'s link onto `dc`, an open DataChannel to it made outside the room (`pc` its connection; a
   * birthday punch's, say), when the room reaches `peer` only by fallback and the peer adopts its end too.
   * True once the channel carries the link; false, leaving the channel to the caller, if the link is direct
   * already or the peer didn't adopt within 5 s. When the channel closes, the link goes back to its fallback. */
  adoptChannel(peer: PeerId, pc: RTCPeerConnection, dc: RTCDataChannel): Promise<boolean>;
  /** The shared document `name` (1 to 128 characters), opened on the first call and the same object until it is
   * closed: the same name on every member is the same document. `options` count when it opens. Throws after
   * `leave()`, which closes the room's docs, TypeError for a `persist` it can't use or `relays: true` without Nostr
   * signalling, and RangeError for a name with `persist` or `relays` that isn't well-formed (a lone surrogate) or for
   * `relays: true` with a roomId that hasn't `randomRoomId()`'s shape. See {@link Doc}. */
  doc(name: string, options?: DocOptions): Doc;
  /** Smoothed round-trip time to a peer in ms, or null before the first sample. */
  rtt(peer: PeerId): number | null;
  stats(): EndpointStats;
  /** Tells every peer we're going, then closes the links, all before it returns (so it works from `pagehide`,
   * where it also runs); the promise resolves once docs kept on this device (`persist`) have saved, however long
   * the store takes (one that never settles holds it; the links are closed by then). Reliable messages not yet
   * acknowledged may not arrive: the goodbye can overtake them. */
  leave(): Promise<void>;
}

/** A map key, set item or log entry as `entries()` lists it. Values come back frozen. A value is member data: check
 * every field you render. `from` is the writer as the doc was told: a member relaying a catch-up can name any
 * writer, so use it to colour or group, not as proof. */
export interface DocMapEntry {
  readonly key: string;
  readonly value: unknown;
  readonly from: PeerId;
}
export interface DocItem {
  /** The item's id: opaque, the same on every member. */
  readonly id: string;
  readonly value: unknown;
  readonly from: PeerId;
}

/** What `doc.on('change')` reports: one event per key or item a write or a member's frame changed. */
export interface DocChange {
  /** The collection's name, as given to `doc.map()`, `doc.set()` or `doc.log()`. */
  name: string;
  type: 'map' | 'set' | 'log';
  /** A map's key. */
  key?: string;
  /** A set item's or log entry's id. */
  id?: string;
  /** What the key or item holds now; undefined once it is gone. */
  value: unknown;
  /** The key or item is gone: deleted, or trimmed from a log. */
  deleted?: true;
  /** The writer of the value now shown or, for a deletion, the member it came from (us for our own writes), or
   * 'relays' for one in a save joined from the relays (`relays: true`). */
  from: PeerId | 'relays';
  /** Our own write. */
  local: boolean;
}

/** Where a doc's catching up stands: `doc.sync`, and what `doc.on('sync')` reports when it changes. As good as the
 * members that told us: use it for a status line, not to decide what the doc holds. */
export interface DocSync {
  /** 'alone': no other member to catch up from. 'loading': catching up from `from`. 'synced': caught up (an
   * answer ended from a member that had caught up itself, or every member has answered). */
  readonly state: 'alone' | 'loading' | 'synced';
  /** The member being caught up from, while 'loading'; null when it waits to ask one, and in the other states. */
  readonly from: PeerId | null;
  /** Entries of `from`'s answer that have landed, newest first, of the `total` it carries (as it says): 0 of 0
   * until its first piece lands, and in the other states. `have` may stop short of `total`: the last piece ends
   * the answer. */
  readonly have: number;
  readonly total: number;
  /** Where the doc came from: 'members' once caught up from them; else 'relays' once it joined a save from the relays
   * (`relays: true`), as of `at`; null before either. */
  readonly source: 'members' | 'relays' | null;
  /** With `source` 'relays': when the newest save joined was made (ms since the epoch), as its saver claims it (one
   * more than 15 minutes past this device's clock counts as the time it was read). For display only ("as of
   * 10:42"). Null otherwise. */
  readonly at: number | null;
}

/** One value per key. A `set` replaces the values its writer has seen; of values set at the same time, the one
 * with the later Lamport stamp shows, then the higher writer. A `delete` removes the values its writer has seen,
 * so a set made elsewhere at the same time wins. */
export interface DocMap {
  /** True; false, with a warning, when members have filled the doc or claimed every write of ours (see the README).
   * Throws RangeError for a key over 256 characters or a value over 64 KiB as JSON (or nested over 64 deep),
   * TypeError for a value that isn't JSON. */
  set(key: string, value: unknown): boolean;
  get(key: string): unknown;
  has(key: string): boolean;
  /** False when the key holds nothing. */
  delete(key: string): boolean;
  /** By key. The same array until the map changes. */
  entries(): readonly DocMapEntry[];
}

/** Items, each with its own id; `delete(id)` removes one (undo: delete your item). */
export interface DocSet {
  /** The new item's id; null, with a warning, when `DocMap.set` would return false. Throws as it does. */
  add(value: unknown): string | null;
  get(id: string): unknown;
  has(id: string): boolean;
  /** False when no such item is held. */
  delete(id: string): boolean;
  /** Oldest first, by Lamport stamp then writer: the same order on every member (a whiteboard's z-order). */
  entries(): readonly DocItem[];
}

/** Append-only, keeping the newest `keep` entries. */
export interface DocLog {
  /** The new entry's id; null, with a warning, when `DocMap.set` would return false. Throws as it does. */
  append(value: unknown): string | null;
  get(id: string): unknown;
  /** Oldest first, by Lamport stamp then writer: an entry appended after seeing another sorts after it. */
  entries(): readonly DocItem[];
}

/** Where a doc kept on this device keeps its copy: a store of bytes by key, e.g. over files. `get` gives back what
 * `set` stored, undefined only when there is none, and rejects when it can't tell; `set` stores all of it or
 * nothing (over files: a temp file, then a rename). */
export interface DocStore {
  get(key: string): Promise<Uint8Array | ArrayBuffer | undefined>;
  set(key: string, bytes: Uint8Array): Promise<void>;
  delete(key: string): Promise<void>;
}

export interface DocOptions {
  /** Keeps a copy of the doc on this device, loaded when it opens, before members are asked: `true` for IndexedDB
   * (browsers; TypeError where there is none), or a {@link DocStore}: make it once and pass the same object each
   * time, as a doc closed and opened again at once loads what the closing one saved only through the same store.
   * Saved when the page is hidden, on `close()` and `leave()`, and while the doc changes, one save at a time: a
   * second after a change, and after the last save by its size at 256 KiB/s. A save the store refuses (a full
   * disk) is tried again with the next change, a second after, then 2, 4 … up to 64 s apart while it keeps
   * refusing, until one succeeds; hiding the page, `close()` and `leave()` still save at once. The copy is sealed
   * under a key from the room key, so the storage shows neither the room, the doc nor what it holds; anyone who
   * knows the roomId can read it, as they could join the room. */
  persist?: boolean | DocStore;
  /** Keeps a sealed copy of the doc on the Nostr relays the room signals over (`signaling: 'nostr'`), so a member that
   * opens it while no other member is online still gets it: from the relays, then live from members as ever.
   *
   * **Use a roomId from `randomRoomId()`.** Anyone with the roomId can read this copy at any time, from any relay,
   * without joining the room and with no member online: nobody sees it read. Relays serve stored events to anyone,
   * and mirrors and archives may keep them, whatever expiry or deletion asks, so anything saved can be copied and
   * kept, and deleting from the doc doesn't remove old saves. There is no revoking a link: only a new roomId cuts off
   * someone who once had it. Its key is also a stored test for a guessed roomId. So `relays: true` throws RangeError
   * for a roomId that hasn't `randomRoomId()`'s shape (22 or more base64url characters); the check catches mistakes,
   * not a guessable roomId of that shape. Anyone with the roomId can also overwrite or wipe the copy: members
   * restore it at their next save, and it is best effort, the copies on members' devices (`persist`) being the main
   * store. Relays keep it 30 days after its last save (opening the doc within a week of that saves it again), or
   * less.
   *
   * Saved by one member at a time, behind signalling: a while after the doc changes (10 s without a change, then up
   * to 30 s more, at most once in 5 minutes, after reading what the relays hold), and by the last member online
   * when the page hides. At most 8 pieces of 44 KiB deflated (a few MB of JSON); a larger doc keeps none, with a
   * warning. Throws TypeError without Nostr signalling, or where there is no `CompressionStream('deflate-raw')`. */
  relays?: boolean;
}

/** A shared document from `room.doc(name)`: maps, sets and logs that every member holds and that converge. Writes
 * reach every member at once on the reliable lane; members that join later, or come back after a partition, catch
 * up, from the best-routed member first, in pieces, the newest entries first. Limits: 10,000 entries and 4 MiB of
 * JSON text per doc; names 128 characters, keys 256, values 64 KiB as JSON. Members are trusted alike: any member
 * can overwrite, delete or fill a doc. */
export interface Doc {
  readonly name: string;
  /** Where catching up stands now; `'sync'` fires when it changes. */
  readonly sync: DocSync;
  /** True once the doc has caught up, and from then on, also once the other members have gone ('alone'). */
  readonly synced: boolean;
  /** What opening found on this device: 'loaded' (its copy is in the doc), 'empty' (none, or one that couldn't be
   * read, which the next save replaces), 'blocked' (the storage refused, as in some private modes, hadn't loaded
   * the copy within 5 s, or holds one from another version of this package: the copy is left as it is, and the doc
   * goes on without one, with a warning) or 'off' (no `persist`). */
  readonly loaded: Promise<'loaded' | 'empty' | 'blocked' | 'off'>;
  map(name: string): DocMap;
  set(name: string): DocSet;
  /** `keep`: the newest entries kept, 1 to 10,000 (default 1000). Give every member the same, right after
   * `room.doc()`. Calling again with another `keep` trims to it. */
  log(name: string, options?: { keep?: number }): DocLog;
  /** Subscribes; returns a function that unsubscribes. A handler that throws is reported on its own. */
  on(event: 'change', fn: (change: DocChange) => void): () => void;
  on(event: 'sync', fn: (sync: DocSync) => void): () => void;
  off(event: 'change', fn: (change: DocChange) => void): void;
  off(event: 'sync', fn: (sync: DocSync) => void): void;
  /** Deletes the copy this device keeps (`persist`), and keeps none until the doc is opened with `persist` again.
   * With `relays: true`, also wipes the copy on the relays, best effort, **for every member** until another member
   * saves (a relay or archive may still keep what it copied), and saves none there from now on. What the doc holds
   * stays. Resolves once the copy is gone and the relays have answered; rejects if the storage fails. */
  forget(): Promise<void>;
  /** Stops syncing and forgets the doc; `room.doc(name)` later opens it afresh and catches up. Writes after it
   * throw. A doc kept on this device saves first: the promise resolves once the store has the copy, however long
   * that takes (closed while a store takes more than 5 s to load the copy, it resolves then, saving nothing). */
  close(): Promise<void>;
}

/** Joins a room: `const room = await joinRoom({ appId: 'my-app', roomId: 'lobby' })`. */
export declare function joinRoom(options: JoinRoomOptions): Promise<Room>;

/** 128 random bits, base64url (22 characters): a roomId nobody can guess. */
export declare function randomRoomId(): string;

/** A {@link DocStore} in this process's memory, for Node and tests: its copies go with the process. */
export declare function memoryStore(): DocStore;

export declare const DEFAULT_ICE_SERVERS: RTCIceServer[];
export declare const DEFAULT_RELAYS: string[];
export declare const RECONNECT_MS: number;
export declare const MAX_RECONNECT_MS: number;

// Lower layers.

export declare const Lane: { readonly Reliable: 0; readonly Unreliable: 1; readonly Urgent: 2 };
export declare const EventKind: {
  readonly Connected: 1;
  readonly Message: 2;
  readonly Disconnected: 3;
  readonly ConnectFailed: 4;
  readonly PathChanged: 5;
  readonly AddrObserved: 6;
};

export type Path = { direct: number } | { via: PeerId };

export type EndpointEvent =
  | { type: 'connected'; peer: PeerId; initiator: boolean; path: Path }
  | { type: 'message'; peer: PeerId; lane: number; data: Uint8Array }
  | { type: 'disconnected'; peer: PeerId; reason: 'timeout' | 'closed' | 'replaced' | 'violation' }
  | { type: 'connectFailed'; peer: PeerId }
  | { type: 'pathChanged'; peer: PeerId; path: Path }
  | { type: 'addrObserved'; peer: PeerId; addr: number }
  | { type: 'unknown'; kind: number };

export interface EndpointStats {
  txPackets: number;
  rxPackets: number;
  rxMalformed: number;
  rxUnknownSession: number;
  rxAuthFailed: number;
  rxReplayed: number;
  rxHandshakeDropped: number;
  retransmits: number;
  unreliableDropped: number;
  roams: number;
  relayed: number;
  relayDropped: number;
  punchesSent: number;
  punchSucceeded: number;
  punchFailed: number;
  directFallbacks: number;
}

export interface Peerwire {
  nostrKeypair(): { secret: Uint8Array; pubkey: Uint8Array };
  nostrSign(secret: Uint8Array, msg32: Uint8Array): Uint8Array;
  endpoint(options?: { secret?: Uint8Array | null; shareAddresses?: boolean; relayOnly?: boolean }): Endpoint;
}

/** Loads the module from a URL (fetched; in a browser, relative and root-relative ones too), an absolute path or
 * `file:` URL (read from disk, in Node only) or its bytes. A fetch with an HTTP error status rejects with it. */
export declare function loadPeerwire(source: string | URL | BufferSource): Promise<Peerwire>;

/** Every method that takes a peer throws a TypeError unless it is 32 bytes or 64 lowercase hex
 * characters: ids from signalling are untrusted and never reach wasm memory unchecked. */
export declare class Endpoint {
  readonly peerId: Uint8Array;
  readonly id: PeerId;
  /** Called with (transport handle, datagram) for each datagram to send. */
  onTransmit: (addr: number, bytes: Uint8Array) => void;
  onEvent: (event: EndpointEvent) => void;
  /** Called at the end of a pump that handled incoming datagrams: ACKs may have freed send
   * window, so a sender that got -3 (would block) can try again. Room flushes its outbox here. */
  onDrain: (() => void) | null;
  /** Datagrams lost because `onTransmit` threw; the endpoint keeps running and retransmits. */
  readonly transmitErrors: number;
  /** The 32-byte identity secret; null after `close()`. */
  secret(): Uint8Array | null;
  /** Throws `Error('connect: endpoint closed')` after `close()`. */
  connect(addr: number, peer: PeerId | Uint8Array): number;
  /** Throws `Error('connectVia: endpoint closed')` after `close()`. */
  connectVia(relay: PeerId | Uint8Array, peer: PeerId | Uint8Array): number;
  /** A direct address for a relayed (or dialling) peer: punches to it and moves the link there once the peer
   * answers from it. Does nothing for a peer linked directly or unknown, while relay-only for one linked
   * through a relay or being dialled with one (nothing is sent to `addr`), or after `close()`. */
  addCandidate(addr: number, peer: PeerId | Uint8Array): void;
  /** Marks `addr` as a slow address of the linked `peer` (a signalling pipe), or clears the mark (`null`). Paths
   * rank slow address, then relay, then direct. On it the link moves to a relay offered with `addRelay` once a
   * probe through it is answered, and goes back to it ('pathChanged') when that relay's link goes, the peer's
   * traffic comes over it again, the relay carries nothing for 15 s, or data sent through it goes unacknowledged
   * for 5 s. Then the link holds that relay off for a minute, twice as long each time it stalls the link again (up
   * to 16 minutes). Does nothing for a peer not linked, or after `close()`. */
  setSlow(peer: PeerId | Uint8Array, addr: number | null): void;
  /** Offers `relay`, a peer linked directly, to carry the link to `peer` while that link is on its slow address
   * (`setSlow`): a sealed probe goes through `relay` every half second for 5 s, and the link moves there
   * ('pathChanged') once `peer` answers through it, which it does only if it would use the relay too. The first
   * probe goes out after the current task, with any others offered in it. On a link relayed already, the relay
   * is raced against the one it is on (a probe round every 2 s, 5 rounds), and the end with the smaller
   * PeerId moves the link to it once it is clearly faster; the other follows. A relay that leaves the link's data
   * unacknowledged for 5 s, on a link with no slow address, is held off as `setSlow` describes and raced at once,
   * and either end moves the link to the relay that wins. Does nothing while relay-only, for a link that is
   * direct, for a relay the link holds off (see `setSlow`), or after `close()`. */
  addRelay(relay: PeerId | Uint8Array, peer: PeerId | Uint8Array): void;
  /** Ends the session with `peer` and sends it a sealed close at once (pumps), so it sees 'disconnected'
   * (reason 'closed') now rather than after its idle timeout. No local event for `peer`, though links it relayed
   * go back to their slow addresses ('pathChanged'); does nothing after `close()`. */
  disconnect(peer: PeerId | Uint8Array): void;
  /** Relayed links never punch to a direct path, dials through a relay alone (given the relay before any
   * direct address of their own, or with a relay when this is turned on) send nothing direct, relays forward
   * without our address, and a direct handshake is answered only from a peer we are dialling directly or already
   * have a direct link with. A direct dial later given a relay keeps its direct addresses and may complete direct,
   * until the next `setRelayOnly(true)`: every such call, even with relay-only already on, sends each dial that has
   * a relay through it alone. */
  setRelayOnly(on: boolean): void;
  /** After `close()`, the counters as they were when it closed. */
  stats(): EndpointStats;
  /** seal, open and rttMs return null, and isConnected false, after `close()`. */
  seal(peer: PeerId | Uint8Array, bytes: Uint8Array): Uint8Array | null;
  open(bytes: Uint8Array): { from: PeerId; data: Uint8Array } | null;
  isConnected(peer: PeerId | Uint8Array): boolean;
  rttMs(peer: PeerId | Uint8Array): number | null;
  /** Round-trip time over the path the link to `peer` is on now, measured since it moved there; null until
   * measured, or after `close()`. */
  pathRttMs(peer: PeerId | Uint8Array): number | null;
  /** 0, or a negative PW_ERR code (-1 not connected, -2 too large: over 1100 B unreliable, 64 KiB urgent,
   * 1 MiB reliable; -3 would block).
   * Throws `Error('send: endpoint closed')` after `close()`: that is a bug in the app, not a lost packet. */
  send(peer: PeerId | Uint8Array, data: string | Uint8Array, lane?: number): number;
  /** send() for a caller that tries the same message again after -3: a message over 16 KiB stays
   * in wasm memory between tries instead of being copied in each time. `data` is matched by
   * identity and must not change between tries; one message is kept at a time. */
  sendRetry(peer: PeerId | Uint8Array, data: string | Uint8Array, lane?: number): number;
  /** A datagram that arrived on handle `addr`. `peer`: the only peer that sends on that handle (its DataChannel,
   * its signalling pipe), if there is one: a packet there for another peer's link, or a handshake from anyone else,
   * is then dropped unread, so a peer that relays for others can't move their links onto its own channel. Ignored
   * after `close()`: late datagrams are normal. */
  receive(addr: number, bytes: Uint8Array, peer?: PeerId | Uint8Array | null): void;
  pump(): void;
  /** Frees the endpoint; a second call does nothing. Close the transport first, so that no
   * datagram arrives for a closed endpoint (`room.leave()` does this). */
  close(): void;
  /** True once `close()` has run. */
  readonly closed: boolean;
}

export declare function decodeEvent(bytes: Uint8Array): EndpointEvent;
export declare function toHex(bytes: Uint8Array): string;
/** Removes `host` candidates (LAN, loopback, mDNS) from an SDP. */
export declare function withoutHostCandidates(sdp: string): string;
/** Counts an SDP's ICE candidates by type and family; `mdns` are host candidates named `*.local`. */
export declare function candidateSummary(sdp: string): { host4: number; host6: number; mdns: number; srflx4: number; srflx6: number; prflx: number; relay: number };
/** A NAT as the server-reflexive candidates in an SDP show it; a sequential symmetric one comes with its prediction. */
export interface NatInfo {
  type: 'symmetric' | 'cone' | 'unknown';
  /** With `next` and `step`: the NAT's public IPv4 address, its likely next port, and the step between ports. */
  ip?: string;
  next?: number;
  step?: number;
}
/** What the server-reflexive candidates in an SDP (or candidate lines) say about the NAT. */
export declare function natFromSdp(sdp: string): NatInfo;
/** ICE candidates at the ports around a peer's prediction, to add with `addIceCandidate`; `[]` if it doesn't check out. */
export declare function predictedCandidates(nat: { ip: string; next: number; step: number }, sdpMid?: string): RTCIceCandidateInit[];
export declare function fromHex(hex: string): Uint8Array;
/** Whether `id` is a PeerId as text: 64 lowercase hex characters. Check ids from signalling with it. */
export declare function isPeerId(id: unknown): id is PeerId;
/** A PeerId as 32 bytes; throws a TypeError unless `id` is 32 bytes or 64 lowercase hex characters. */
export declare function peerBytes(id: PeerId | Uint8Array): Uint8Array;

export interface Transport {
  dial(peer: PeerId): void | Promise<void>;
  has(peer: PeerId): boolean;
  drop(peer: PeerId): void;
  close(): void;
}

export declare class WebRtcTransport implements Transport {
  constructor(
    endpoint: Endpoint,
    signaling: SignalingAdapter,
    options?: {
      iceServers?: RTCIceServer[];
      /** `'failed'`: the DataChannel isn't open `fallbackMs` after its offer or answer (`sameNatFallbackMs` for a
       * peer behind our public IPv4), or ICE failed; ICE goes on checking, and a channel that opens later takes
       * over a link the endpoint made by other means (a relayed one punches over it; a pipe's datagrams go over it,
       * and the far end's core moves the link there). `'carrier'`: an adopted channel started or stopped carrying
       * the peer's link (see `adopt`). With `'closed'`, `handle` is the closed link's endpoint handle.
       * `'retry-failed'`: a retry's connection (see `retry`) didn't open; `'retry'`: the other side asks us, the
       * dialling side, for one. */
      onPeer?: (peer: PeerId, state: 'open' | 'closed' | 'failed' | 'carrier' | 'retry-failed' | 'retry', handle?: number) => void;
      /** Default 8000 ms. */
      fallbackMs?: number;
      /** Default 3000 ms, at most `fallbackMs`. */
      sameNatFallbackMs?: number;
      loss?: number;
      trickle?: boolean;
      /** `'relay-only'` needs `relay`: WebRTC then runs only with that peer. */
      privacy?: 'direct' | 'public-only' | 'relay-only';
      relay?: PeerId;
      /** Offers carry `p: await sign(to)`; an offer is answered only if `check(from, p)` holds. */
      offerProof?: { sign(to: PeerId): Promise<string>; check(from: PeerId, p: unknown): Promise<boolean> };
      RTCPeerConnection?: typeof globalThis.RTCPeerConnection;
    },
  );
  /** Datagrams dropped by the simulated `loss` option. */
  readonly dropped: number;
  /** Datagrams dropped because the DataChannel had over 1 MiB queued or `send` threw. */
  readonly congested: number;
  /**
   * Signals refused: a `from` that isn't a PeerId or an empty message, an offer whose `offerProof` fails or whose
   * SDP doesn't apply, a retry offer that would make a peer's second new connection within 2 s, an `ice-batch`
   * that isn't a list of at most 32, or one that failed unexpectedly. Answers and
   * ICE candidates that don't apply are not counted (copies and races are normal). The WebRTC counterpart of
   * NostrSignaling's `stats.rejected`.
   */
  readonly badSignals: number;
  /** Offers sent again for want of an answer, and answers sent again for a repeated offer. */
  readonly resent: number;
  /**
   * Offers, answers, copies of either and `restart` asks not sent because the peer's publish budget was spent:
   * 6 at once, then 1 per 5 s per peer, so no peer can keep us publishing to the signalling relays. The ICE
   * candidates of an answer not sent wait with it, until a copy of its offer finds budget.
   */
  readonly throttled: number;
  /**
   * ICE restarts begun as the dialling side (a link that was open lost ICE). At most 3 per link a minute, whether
   * or not ICE reconnects in between (while it stays down, the next is made once the minute frees); the other
   * side's ask is heeded only while our own ICE is down too.
   */
  readonly restarts: number;
  /** Retries begun as the dialling side (see `retry`). */
  readonly retried: number;
  /**
   * Diagnostics: called for each link's signalling (`offer-sent`, `offer-received`, `answer-sent`, `answer-received`,
   * `ice-batch-sent`, `ice-batch-received`, with `candidateSummary` counts, `resend: true` on a copy and
   * `restart: true` for an ICE restart, `retry: true` for a retry's offer), retries (`retry` with `dialer`,
   * `retry-failed`), ICE (`ice-state` with `state`, `candidate-error` with the STUN server `url`,
   * `code`, `text`, `address`, `ice-restart` with `n`, `reason`, `dialer`), port prediction (`guesses` with the
   * peer's `ip`, `next`, `step` and the `n` candidates added), `same-nat` (the peer's candidates name our
   * public IPv4 `ip`, so the link falls back after `ms`) and `throttled` (a message of `kind` not sent: see
   * `throttled`).
   */
  trace: ((peer: PeerId, what: string, detail: Record<string, unknown>) => void) | null;
  /**
   * The network changed: links that were open but whose ICE isn't connected restart ICE now, rather than after
   * the 2 s grace. `joinRoom` calls it on `online` and on a `navigator.connection` change.
   */
  networkChanged(): void;
  /**
   * Carries `peer`'s link over `dc`, an open DataChannel to it made outside the transport, once the far end
   * adopts its end too (each sends a one-byte marker until it hears the other's). With `pipe`, the peer's pipe
   * datagrams go over `dc` instead of signalling; otherwise `dc` gets its own handle, for
   * `endpoint.addCandidate`. Resolves to that handle, or -1 (the channel left to the caller) if the far end
   * didn't adopt within 5 s or `dc` closed first. Once carrying, `dc` closing or `pc` failing ends it, as do
   * `drop(peer)`, `close()` and a newer `adopt(peer)`; ending it closes `pc`. `room.adoptChannel` uses it.
   */
  adopt(peer: PeerId, pc: RTCPeerConnection, dc: RTCDataChannel, options?: { pipe?: boolean }): Promise<number>;
  /** The adopted channel carrying `peer`'s link now, or null. */
  carrier(peer: PeerId): { pc: RTCPeerConnection; dc: RTCDataChannel } | null;
  /**
   * As the dialling side, dials `peer` again with a new RTCPeerConnection while its link runs by other means:
   * the link's own connection, which never opened, is closed, and the offer says `retry`, so the other side
   * drops its own (only one that never opened). If the new DataChannel opens, the link moves onto it; if not,
   * it closes quietly (`'retry-failed'`, never `'failed'`). False if the link is open or a retry is under way.
   */
  retry(peer: PeerId): boolean;
  /** Asks `peer`, the dialling side, for a retry (it gets `onPeer(peer, 'retry')`). */
  askRetry(peer: PeerId): void;
  dial(peer: PeerId, options?: { retry?: boolean }): Promise<void>;
  has(peer: PeerId): boolean;
  drop(peer: PeerId): void;
  close(): void;
}

export declare class LocalTransport implements Transport {
  constructor(endpoint: Endpoint, signaling: BroadcastSignaling);
  dial(peer: PeerId): void;
  has(peer: PeerId): boolean;
  drop(peer: PeerId): void;
  close(): void;
}

export declare class BroadcastSignaling implements SignalingAdapter {
  constructor(id: string, room?: string);
  readonly id: string;
  onHello: (peer: PeerId, meta?: unknown) => void;
  hello(meta?: unknown): void;
  send(to: PeerId, body: unknown): void;
  onMessage(fn: (from: PeerId, body: any) => void): void;
  close(): void;
}

export declare class NostrSignaling implements SignalingAdapter {
  constructor(pw: Peerwire, endpoint: Endpoint, options: { room: string; relays?: string[] });
  readonly id: string;
  readonly ready: Promise<void>;
  /**
   * `relaysOpen` counts sockets open now; `reconnects` counts relays that opened again after a close; `backoffs`,
   * relays held for their rate or stopped for a ban; `resent`, events sent to a relay again after it refused them
   * for its rate; `unsent`, events dropped from a relay's queue (past 64 waiting, or waiting over 10 s).
   */
  readonly stats: { published: number; received: number; rejected: number; relaysOpen: number; reconnects: number; backoffs: number; resent: number; unsent: number; lastRejection?: string };
  onHello: (peer: PeerId, meta?: unknown) => void;
  /** A relay opened again after its socket closed (it is redialled on its own, and at once on a network change or the page showing again; going online redials every relay). */
  onReconnect: (url: string) => void;
  hello(meta?: unknown): Promise<void>;
  /** `lossy`: through one relay, the relays taking turns (one that holds us for its rate, has banned us or isn't open
   * is skipped, and so is one slowed behind what waits for it and, while any relay that answers is left, one that has
   * answered nothing for 5 s since it was sent an event), not each. */
  send(to: PeerId, body: unknown, options?: { lossy?: boolean }): Promise<void>;
  /** Milliseconds until the relay the next lossy event goes to could take it (0: it can now). Each relay is paced on
   * its own, so lossy sends that wait this long move at about the relays' rates together. */
  wait(): number;
  onMessage(fn: (from: PeerId, body: any) => void): void;
  close(): void;
}
