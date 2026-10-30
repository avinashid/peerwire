// Shared documents: state that every member of a room holds and that converges, made with
// room.doc(name). Members that join later, or come back after a partition, catch up.
//
//   const doc = room.doc('board');
//   const meta = doc.map('meta');                    // meta.set('title', 'Plans'); meta.get('title')
//   const strokes = doc.set('strokes');              // const id = strokes.add(stroke); strokes.delete(id)
//   const chat = doc.log('messages', { keep: 500 }); // chat.append({ text })
//   doc.on('change', (ev) => render(ev));
//   doc.on('sync', ({ state, from, have, total }) => status(state)); // 'alone', 'loading' or 'synced'
//
// Every write is a dot, (writer, n). The writer is our PeerId and a random incarnation, one per room.doc()
// call, so a reloaded page never reuses a dot; n counts the writer's writes. A doc holds its live entries and
// its context: for each writer, the ranges of dots it has seen. A dot it has seen and doesn't hold is dead
// (replaced by a later set of its key, deleted, or trimmed from a log), so a deletion leaves no tombstone, and
// a dead entry that comes again, from a member that hasn't heard, is known and ignored.
//
// A map key holds the values written to it that no later write has seen: a set replaces the values its
// writer has seen, and a delete removes them, so a set made elsewhere at the same time wins over a delete
// (add-wins, as in Yjs and Automerge). Of values set at the same time, the one with the later Lamport stamp
// shows, then the higher writer. A set item is one dot; delete(id) removes it. A log keeps its newest `keep`
// entries by (Lamport stamp, writer). Lamport stamps order writes, never wall time.
//
// Each write goes to every member as a delta: the new entry and the dots it replaces. A doc opened, and each
// member that joins, is asked with our context, one member at a time and the best routed first (linked
// directly, then relayed by a member, then over signalling), so a joiner gets the doc once rather than from
// every member: the answer holds the entries we lack and the dots the member knows are dead. Every frame is a
// piece of state joined into ours (delta-state CRDTs, Almeida et al. 2018), so frames apply in any order and
// any number of times, and every member that has heard the same frames holds the same doc. So an answer goes
// in pieces sized to the route, the newest entries first, each applied as it lands and acknowledged, a few
// unacknowledged at a time; the next member is asked once an answer goes ASK_WAIT_MS without a piece, and its
// ask, carrying what the pieces brought, is answered with only what is still missing. 'sync' says where this
// stands: 'alone', 'loading' from a member with `have` of `total` entries, or 'synced'.
//
// A doc opened with a store (room.doc(name, { persist })) keeps a copy on this device: its entries and context,
// saved while it changes, paced by the copy's size (SAVE_MS, SAVE_RATE) or, while the store fails, slower with
// each failure in a row (SAVE_DOUBLINGS), and when it closes. Opening loads the copy before the first ask, so
// the ask carries it and members send only what it lacks, and our answers wait for it, LOAD_MS at most. The
// copy joins the doc as an answer from ourselves would, checked as member data, so a copy that is off anywhere
// is dropped whole. An opening that couldn't read the copy (the store failed or was too slow, or the copy is
// another version's) never writes it: it goes on without one.
//
// Members are trusted alike to write, as in the room itself: any member can overwrite, delete or fill a doc,
// and a member relaying a catch-up can claim any writer, so `from` is as good as the member that told us.
// What the doc defends is each member's app and resources. Frames are checked field by field and dropped
// whole if anything is off; state lives in Maps; names, keys, values, entries, characters, writers and context
// ranges are capped, and so is what a member can have us send to catch up: answers and asks at a rate
// (CATCHUP_BURST at once, then one per CATCHUP_EVERY_MS), and what our answers to it carry, with the deletions
// its answers have us send on to the others, CATCHUP_CHARS at once, then CATCHUP_CHARS_PER_S, as an asker can
// always claim to lack the whole doc and an answer can always name dots dead.
//
// Every member takes any stamp up to 2^53 - 1, as refusing one far ahead would split the room: no member is a
// reference for how far ahead is plausible, and a refused entry never comes again. So a member can move the
// stamps to 2^53 - 1 in one frame; from then on, values set at the same time, and set items and log entries,
// are ordered by writer, and a log keeps the highest writers' entries. A later set still wins, as it replaces
// what it has seen. A member could claim dots another member hasn't written yet, so that the room ignores its
// next writes: a doc that sees such a claim on its own dots moves to a new incarnation, at most
// MAX_INCARNATIONS times. Past that it writes after the dots it knows are claimed; a write on a dot claimed
// where it hasn't heard is lost at the members that took the claim, and once every dot is claimed, writes are
// refused with a warning until the doc is opened again. It holds the claims on its dots as one span, from the
// lowest to the highest (none of those dots is its yet), so they take one range of its context however many.

export const MAX_NAME_LENGTH = 128; // a doc's or a collection's name, in UTF-16 code units
export const MAX_KEY_LENGTH = 256; // a map key
export const MAX_VALUE_LENGTH = 64 * 1024; // a value as JSON text
export const MAX_VALUE_DEPTH = 64; // how deeply a value's arrays and objects nest
export const MAX_ENTRIES = 10000; // live entries in a doc, all collections
export const MAX_DOC_LENGTH = 4 * 1024 * 1024; // what a doc's values, keys and names take as JSON text
export const MAX_WRITERS = 1024; // writers a doc's context holds
export const MAX_RANGES = 8192; // ranges of dots a doc's context holds
const FORGET_RANGES = 1024; // ranges a full context frees at once, when no writer is idle
export const DEFAULT_KEEP = 1000; // entries a log keeps unless doc.log() says otherwise
export const MESSAGE_BYTES = 512 * 1024; // the most a doc frame takes as UTF-8, half room.send's 1 MiB
// Catching up with a member: the answers it gets and the asks we send it each come from a budget of
// CATCHUP_BURST that refills one per CATCHUP_EVERY_MS, and what our answers carry, as JSON text, from one of
// CATCHUP_CHARS (two full docs) that refills CATCHUP_CHARS_PER_S: an answer's first piece takes an answer,
// and each piece its characters. An answer or ask past a budget waits for it rather than being dropped, so a
// doc closed and opened again (a component that remounts) catches up every time, and an asker can't have us
// send more, whatever it claims to lack or acknowledges: a piece past the chars budget waits for it, and an
// acknowledgement only lets the answer asked for go on. The deletions a member's answers have us send on to the
// others come from its CATCHUP_CHARS too, once for each member they go to; past it they aren't sent on, and
// the others hear of them from that member or when they next catch up.
const CATCHUP_BURST = 8;
const CATCHUP_EVERY_MS = 15000;
const CATCHUP_CHARS = 2 * MAX_DOC_LENGTH;
const CATCHUP_CHARS_PER_S = 64 * 1024;
// Each budget: [burst, ms one unit takes to refill].
const BUDGETS = {
  answers: [CATCHUP_BURST, CATCHUP_EVERY_MS],
  asks: [CATCHUP_BURST, CATCHUP_EVERY_MS],
  chars: [CATCHUP_CHARS, 1000 / CATCHUP_CHARS_PER_S],
};
// An answer goes in pieces, its newest entries first and then the dots it knows are dead: each piece about
// PIECE_CHARS of JSON text for the route to the asker (one entry at least), so a slow route gets small ones
// and a route that changes, pieces to fit it. The asker acknowledges each piece it applies (MORE), and at most
// WINDOW go unacknowledged, so the answer keeps pace with the route and live writes never wait behind all of it.
const PIECE_CHARS = { direct: 128 * 1024, peer: 32 * 1024, signaling: 32 * 1024 };
const WINDOW = 4;
// Members are asked in the order of their routes: linked directly, relayed by a member, over signalling.
const RANK = { direct: 0, peer: 1, signaling: 2 };
// The turn of the member asked passes on once its answer has gone ASK_WAIT_MS without a piece (it stalled) or
// taken TURN_MS, if another member is waiting to be asked. Its answer goes on all the same.
const ASK_WAIT_MS = 5000;
const TURN_MS = 60000;
// Incarnations a doc moves through when members claim its future dots.
const MAX_INCARNATIONS = 16;
// A doc kept on this device is saved SAVE_MS after it changes, one save at a time, each the whole doc: so the
// next waits SAVE_MS, or the copy's bytes at SAVE_RATE if longer, after the one before ends (2 s for a 510 KB
// whiteboard, 48 s for a full doc of 12 MiB; one costs about 5 ms a MiB of main thread). After a save that
// failed (a full disk), the next waits SAVE_MS, doubled for each failure in a row before it, SAVE_DOUBLINGS
// times at most (1, 2, 4 ... 64 s), until one succeeds: a store that keeps failing has a full doc built and
// sealed once a minute, not once a second. Closing and hiding the page save at once all the same. Its copy is
// answer frames, as pack() cuts them: a full doc takes about 30, and a copy of more than STORED_FRAMES isn't
// loaded. A store that hasn't loaded the copy LOAD_MS after opening is taken as stuck: the doc goes on without it.
export const SAVE_MS = 1000;
export const SAVE_RATE = 256 * 1024; // bytes a second
const SAVE_DOUBLINGS = 6;
export const LOAD_MS = 5000;
export const STORED_FRAMES = 64;
// The longest array a frame may hold: entries, dead ranges, writers.
const FRAME_ITEMS = 16384;
const MAX_STAMP = Number.MAX_SAFE_INTEGER;
const DELTA = 1;
const ASK = 2;
const ANSWER = 3;
const MORE = 4;
// "Saving": a member is writing the doc's copy on the relays, save `s` (relays.js). Only a doc kept on the relays
// reads it; any other drops it as malformed.
const SAVING = 5;
const SAVE_ID = /^[0-9a-f]{16}$/;
const TYPES = { m: 'map', s: 'set', l: 'log' };
const WRITER = /^[0-9a-f]{72}$/;
// Estimated UTF-8 bytes in a frame: 3 per character of JSON text, and room for numbers and punctuation (a
// range is at most [1023,9007199254740991,9007199254740991], 41 bytes).
const WRITER_BYTES = 80;
const RANGE_BYTES = 48;
const ENTRY_BYTES = 96;

const count = (n) => Number.isSafeInteger(n) && n >= 1;
const index = (n) => Number.isSafeInteger(n) && n >= 0;
const peerOf = (w) => w.slice(0, 64);
// Whether entry a sorts after entry b: the later stamp, then the higher writer, then the later dot.
const after = (a, b) => a.c > b.c || (a.c === b.c && (a.w > b.w || (a.w === b.w && a.n > b.n)));
const byStamp = (a, b) => (after(a, b) ? 1 : after(b, a) ? -1 : 0);

// A binary heap of entries, the one sorting first on top: a log's, so trimming one costs log n.
function heapPush(h, e) {
  let i = h.push(e) - 1;
  while (i > 0) {
    const p = (i - 1) >> 1;
    if (!after(h[p], e)) break;
    h[i] = h[p];
    i = p;
  }
  h[i] = e;
}

function heapPop(h) {
  const top = h[0];
  const last = h.pop();
  if (h.length) {
    let i = 0;
    for (let c = 1; c < h.length; c = 2 * i + 1) {
      if (c + 1 < h.length && after(h[c], h[c + 1])) c++;
      if (!after(last, h[c])) break;
      h[i] = h[c];
      i = c;
    }
    h[i] = last;
  }
  return top;
}
const validName = (s) => typeof s === 'string' && s.length >= 1 && s.length <= MAX_NAME_LENGTH;
const validKey = (k) => typeof k === 'string' && k.length <= MAX_KEY_LENGTH;
function collection(name) {
  if (!validName(name)) throw new RangeError(`a collection name is 1 to ${MAX_NAME_LENGTH} characters`);
  return name;
}
// A string's length as JSON text: control characters take up to 6.
const jsonLength = (s) => JSON.stringify(s).length;

// Freezes a value parsed from JSON, unless it nests deeper than MAX_VALUE_DEPTH: then false. A loop, not
// recursion, so no value can overflow the stack.
function freezeJson(v) {
  if (v === null || typeof v !== 'object') return true;
  const stack = [[v, 1]];
  while (stack.length) {
    const [o, depth] = stack.pop();
    if (depth > MAX_VALUE_DEPTH) return false;
    Object.freeze(o);
    for (const k in o) {
      const x = o[k];
      if (x !== null && typeof x === 'object') stack.push([x, depth + 1]);
    }
  }
  return true;
}

// Our own value as members will see it (JSON gives back a copy), frozen, and its length as JSON text.
// Throws for a value that isn't JSON or is past the limits: those are the app's own mistakes.
function asJson(value) {
  const text = JSON.stringify(value);
  if (text === undefined) throw new TypeError('a doc value must be a JSON value');
  if (text.length > MAX_VALUE_LENGTH) throw new RangeError(`value of ${text.length} characters as JSON; at most ${MAX_VALUE_LENGTH}`);
  const v = JSON.parse(text);
  if (!freezeJson(v)) throw new RangeError(`value nested deeper than ${MAX_VALUE_DEPTH}`);
  return { v, len: text.length };
}

// Ranges of dots, per writer: sorted, disjoint and never adjacent ([1, 4] and [5, 9] are kept as [1, 9]).
// The index of the first range in `list` that ends at or after n.
function seek(list, n) {
  let lo = 0;
  let hi = list.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (list[mid][1] < n) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

class Context {
  constructor() {
    this.writers = new Map(); // writer -> ranges
    this.size = 0; // ranges in all
  }

  has(w, n) {
    const l = this.writers.get(w);
    if (!l) return false;
    const i = seek(l, n);
    return i < l.length && l[i][0] <= n;
  }

  // Whether every dot from a to b has been seen.
  covers(w, a, b) {
    const l = this.writers.get(w);
    if (!l) return false;
    const i = seek(l, a);
    return i < l.length && l[i][0] <= a && l[i][1] >= b;
  }

  // Whether adding [a, b] would join a range of w's rather than start one.
  touches(w, a, b) {
    const l = this.writers.get(w);
    if (!l) return false;
    const i = seek(l, a - 1);
    return i < l.length && l[i][0] <= b + 1;
  }

  add(w, a, b) {
    let l = this.writers.get(w);
    if (!l) this.writers.set(w, (l = []));
    const i = seek(l, a - 1);
    let j = i;
    let lo = a;
    let hi = b;
    for (; j < l.length && l[j][0] <= b + 1; j++) {
      if (l[j][0] < lo) lo = l[j][0];
      if (l[j][1] > hi) hi = l[j][1];
    }
    l.splice(i, j - i, [lo, hi]);
    this.size += 1 - (j - i);
  }

  drop(w) {
    const l = this.writers.get(w);
    if (!l) return;
    this.size -= l.length;
    this.writers.delete(w);
  }

  // The first of w's dots from n on that hasn't been seen.
  next(w, n) {
    const l = this.writers.get(w);
    if (!l) return n;
    const i = seek(l, n);
    return i < l.length && l[i][0] <= n ? l[i][1] + 1 : n;
  }

  // A context of `ranges` ([w, a, b] in any order), added in order so each lands at the end of its writer's.
  static of(ranges) {
    const c = new Context();
    const sorted = [...ranges].sort(([v, a], [w, b]) => (v < w ? -1 : v > w ? 1 : a - b));
    for (const [w, a, b] of sorted) c.add(w, a, b);
    return c;
  }

  // Drops w's highest range.
  dropLast(w) {
    const l = this.writers.get(w);
    if (!l) return;
    l.pop();
    this.size--;
    if (!l.length) this.writers.delete(w);
  }
}

// A frame from `peer`, checked: null if any field is off. Entries come back with their length as JSON text.
// An ask carries its number `q`, and `z` if its doc reads deflated frames; a piece of an answer, the ask's `q`,
// its own number `i`, `n` the entries the whole answer carries, and on the last, `end` and `s` if the member
// had caught up itself. A MORE acknowledges piece `i` of the answer to ask `q`.
function parseFrame(m, peer) {
  if (m === null || typeof m !== 'object' || Array.isArray(m)) return null;
  const { t, ws } = m;
  if (t === MORE) return count(m.q) && index(m.i) ? { t, q: m.q, i: m.i } : null;
  if (t !== DELTA && t !== ASK && t !== ANSWER) return null;
  if (!Array.isArray(ws) || ws.length > FRAME_ITEMS || !ws.every((w) => typeof w === 'string' && WRITER.test(w))) return null;
  if (t === ASK && (!count(m.q) || (m.z !== undefined && m.z !== 1))) return null;
  const writer = (i) => (Number.isInteger(i) && i >= 0 && i < ws.length ? ws[i] : null);
  const ranges = [];
  const list = t === ASK ? m.r : m.x;
  if (list !== undefined) {
    if (!Array.isArray(list) || list.length > (t === ASK ? MAX_RANGES : FRAME_ITEMS)) return null;
    for (const r of list) {
      if (!Array.isArray(r) || r.length !== 3) return null;
      const w = writer(r[0]);
      if (!w || !count(r[1]) || !count(r[2]) || r[1] > r[2]) return null;
      ranges.push([w, r[1], r[2]]);
    }
  }
  if (t === ASK) return { t, ranges, q: m.q, z: m.z === 1 };
  if (t === ANSWER && m.q !== undefined && (!count(m.q) || !index(m.i) || !index(m.n))) return null;
  if (t === ANSWER && m.s !== undefined && (m.s !== 1 || m.end !== 1)) return null;
  const entries = [];
  if (m.e !== undefined) {
    if (!Array.isArray(m.e) || m.e.length > FRAME_ITEMS) return null;
    for (const x of m.e) {
      if (!Array.isArray(x) || x.length !== 7) return null;
      const [i, n, c, type, coll, k, v] = x;
      const w = writer(i);
      if (!w || !count(n) || !count(c) || typeof type !== 'string' || !Object.hasOwn(TYPES, type) || !validName(coll)) return null;
      if (type === 'm' ? !validKey(k) : k !== null) return null;
      // A delta carries only its sender's writes.
      if (t === DELTA && peerOf(w) !== peer) return null;
      const text = JSON.stringify(v);
      if (text === undefined || text.length > MAX_VALUE_LENGTH || !freezeJson(v)) return null;
      entries.push({ w, n, c, t: type, coll, k, v, len: text.length + jsonLength(k) + jsonLength(coll) });
    }
  }
  if (m.end !== undefined && m.end !== 1) return null;
  const f = { t, entries, ranges, end: m.end === 1 };
  return t === ANSWER && m.q !== undefined ? Object.assign(f, { q: m.q, i: m.i, n: m.n, s: m.s === 1 }) : f;
}

// The frames of a copy (`f`: the one on this device, or a piece of the one on the relays), each checked as a member's
// answer to no ask, for doc `name`: null if any is off.
export function copyFrames(name, f) {
  const frames = [];
  for (const m of f) {
    const x = m?.d === name && m.t === ANSWER && m.q === undefined ? parseFrame(m, null) : null;
    if (!x) return null;
    frames.push(x);
  }
  return frames;
}

// The answer to `m`, a member's ask for a doc we don't have open: nothing, so that the asker counts us as
// answered rather than waiting for us. Null if `m` isn't an ask. About the ask's size, and to its sender only.
export function unopened(m) {
  if (m?.t !== ASK || !validName(m.d) || !count(m.q)) return null;
  return { d: m.d, t: ANSWER, ws: [], q: m.q, i: 0, n: 0, end: 1 };
}

// One doc, shared by every member that opens the same name. `host` is the room's side:
// { self, peers() (joined members), send(peer, message, deflate) (`deflate`: the member reads deflated frames),
// after(ms, fn) -> cancel, now() (ms), closed(doc), incarnation(), route(peer) ('direct', 'peer' or
// 'signaling'), deflate (we read deflated frames), store (the copy on this device, or none: load() -> the copy
// saved, null if there is none or it can't be read, or false for one of another version, to leave as it is;
// save(copy) -> the bytes it wrote, `copy` may be a promise of it, or of null: nothing to save; forget(); each
// a promise that rejects if
// the storage fails, and each taking effect in the order called, as a doc closed and opened again at once loads
// what the closing one saved), relays (the copy on the relays, a RelayCopy, or none), wall() (ms since the epoch,
// for the relays; Date.now() if not given) }.
export class Doc {
  constructor(name, host) {
    if (!validName(name)) throw new RangeError(`a doc name is 1 to ${MAX_NAME_LENGTH} characters`);
    this.name = name;
    this.host = host;
    // The copy on the relays: the doc tells it of changes (not those it joined itself: `fromRelays`, while it does)
    // and members' saves, and it joins whole saves from the relays (see joinRelays).
    this.relays = host.relays ?? null;
    this.fromRelays = false;
    this.relaySeen = 0; // `version` as the copy on the relays last heard of it
    this.entries = new Map(); // id -> entry { id, w, n, c, t, coll, k, v, len }
    this.byWriter = new Map(); // writer -> Map(n -> entry)
    this.ctx = new Context();
    this.colls = new Map(); // type + name -> collection { t, name, keys | items, keep, list }
    this.keeps = new Map(); // log name -> keep, as doc.log() said
    this.length = 0; // what the entries take as JSON text
    this.clock = 0; // the highest Lamport stamp seen
    this.incarnations = 0;
    this.newIncarnation();
    this.handlers = new Set();
    this.syncHandlers = new Set();
    this.touched = new Map(); // changes during one frame or write: collection -> ref -> { coll, ref, before }
    this.idle = new Set(); // writers the context holds with no live entry, longest idle first
    this.members = new Map(); // peer -> catch-up state, see member()
    this.queue = []; // members to ask, in turn
    this.asking = null; // the member whose turn it is, see nextAsk
    this.cancelWait = null;
    // Our asks are numbered from the incarnation's random bits, so a doc opened again never takes an answer to
    // the asks of the one before it for its own.
    this.askNo = (Number.parseInt(this.writer.slice(64, 72), 16) || 0) * 2 ** 20;
    // Caught up: an answer from a member that had caught up itself ended, or every member had answered.
    this.synced = false;
    this.sync = Object.freeze(this.status());
    this.closed = false;
    // While a frame is applied: a claim on our own dots, the span of them its entries took, a sender's write
    // we count as seen and don't hold, and the dead ranges a catch-up has us send on.
    this.claimed = false;
    this.claim = null; // the lowest and highest of our dots that a frame's entries took
    this.unheard = false;
    this.dropped = null;
    // The copy on this device: what opening found ('loaded', 'empty', 'blocked' or 'off'), whether it is still
    // loading, the doc's changes (`version`, counted by insert, remove and record) as of the last save, the wait
    // for the next save, and the last save or deletion asked of the store (each runs after the one before).
    this.store = host.store ?? null;
    this.rangeCap = MAX_RANGES; // ranges the context may hold: more while a copy joins (see restore)
    this.loaded = Promise.resolve('off');
    this.loading = false;
    this.loadEnd = null; // resolves `loaded`
    this.loadWait = null; // cancels the LOAD_MS deadline
    this.version = 0;
    this.savedVersion = 0;
    this.saveWait = null;
    this.saving = Promise.resolve();
    this.inFlight = false; // a save is with the store
    this.saveSoon = false; // a save asked at once (the page hidden) while one was in flight: goes as it ends
    this.nextSave = 0; // host.now() before which no paced save starts (see saved)
    this.saveFails = 0; // saves that failed since the last that succeeded: each doubles the wait (see saved)
    this.forgotten = false;
    // This opening couldn't read the copy (the store failed or took LOAD_MS, or the copy is another version's):
    // it leaves the copy as it is and keeps none.
    this.blocked = false;
    this.storeWarned = false;
    this.closingCopy = null; // closed while the copy loaded: gives the save close() queued its copy (see load)
  }

  // Collections, made on first use. Calling again with the same name gives the same contents.

  map(name) {
    return new DocMap(this, collection(name));
  }

  set(name) {
    return new DocSet(this, collection(name));
  }

  // `keep`: how many entries the log holds, the newest; give every member the same, right after room.doc().
  log(name, { keep = DEFAULT_KEEP } = {}) {
    if (!Number.isSafeInteger(keep) || keep < 1 || keep > MAX_ENTRIES) throw new RangeError(`keep is 1 to ${MAX_ENTRIES}`);
    this.keeps.set(collection(name), keep);
    const coll = this.colls.get('l' + name);
    if (coll) {
      coll.keep = keep;
      this.trim(coll);
      this.flush(this.host.self, true);
    }
    return new DocLog(this, name);
  }

  // 'change': a key or item changed. 'sync': where catching up stands changed (see status).
  on(event, fn) {
    const set = event === 'change' ? this.handlers : event === 'sync' ? this.syncHandlers : null;
    if (!set) throw new TypeError(`a doc's events are 'change' and 'sync', not ${String(event)}`);
    set.add(fn);
    return () => this.off(event, fn);
  }

  off(event, fn) {
    if (event === 'change') this.handlers.delete(fn);
    else if (event === 'sync') this.syncHandlers.delete(fn);
  }

  // Stops syncing and forgets the doc: room.doc(name) later opens it afresh and catches up again. A doc kept on
  // this device is saved first, at once, after any save in flight (one closed while its copy loads, once that is
  // joined in: see load); the promise resolves once that is done.
  close() {
    if (this.closed) return this.saving;
    if (this.loading && !this.forgotten) {
      // Saved in its turn now, so a doc opened again at once loads it, with the copy that load() gives it. Past
      // LOAD_MS that is nothing, and closing waits no more for a store that is stuck.
      const copy = new Promise((r) => (this.closingCopy = r));
      const store = this.store;
      const saved = (async () => store.save(copy))().catch((e) => this.storeFailed(e));
      this.saving = this.loaded.then(() => (this.blocked ? undefined : saved));
    } else {
      this.write();
    }
    this.closed = true;
    this.relays?.close();
    this.saveWait?.();
    this.saveWait = null;
    this.cancelWait?.();
    this.queue = [];
    this.asking = null;
    this.handlers.clear();
    this.syncHandlers.clear();
    if (!this.loading) this.clear();
    for (const st of this.members.values()) {
      st.answerWait?.();
      st.askWait?.();
    }
    this.members.clear();
    this.host.closed?.(this);
    return this.saving;
  }

  clear() {
    this.entries.clear();
    this.byWriter.clear();
    this.colls.clear();
  }

  // Deletes the copy this device keeps and keeps none from now on, until the doc is opened with a store again;
  // and wipes the copy on the relays (for every member, until one saves again), best effort, saving none there
  // either. What the doc holds stays. Resolves once the copy is gone and the relays have answered; rejects if the
  // storage fails.
  forget() {
    const wipe = this.relays?.forget() ?? null;
    if (!this.store) return wipe ?? Promise.resolve();
    this.forgotten = true;
    this.saveWait?.();
    this.saveWait = null;
    const store = this.store;
    const gone = (async () => store.forget())();
    this.saving = gone.catch(() => {});
    return wipe ? Promise.all([gone, wipe]).then(() => {}) : gone;
  }

  // The room's side.

  // Loads the copy this device keeps, if any, then asks every member: the doc was just opened.
  open() {
    if (this.store) {
      this.loading = true;
      this.loaded = new Promise((r) => (this.loadEnd = r));
      // A store that never answers would hold our asks and answers for good.
      this.loadWait = this.host.after(LOAD_MS, () => {
        this.storeFailed(new Error(`the store hasn't loaded it in ${LOAD_MS} ms`));
        this.loadDone('blocked');
      });
      this.load();
    }
    this.relays?.open(this);
    for (const p of this.host.peers()) this.startAsk(p);
    this.emitSync();
  }

  // The copy, joined into the doc before the first ask (see nextAsk), so the ask carries it and members send only
  // what it lacks, and before our first answer (see serve), so answers carry it. What it found: 'loaded', 'empty'
  // (none, or one that couldn't be read or was off) or 'blocked' (the storage failed, private browsing say, or
  // holds a copy of another version: see loadDone).
  async load() {
    let found = 'empty';
    try {
      const copy = await this.store.load();
      if (!this.loading) return; // past LOAD_MS: the doc went on without it
      // With nothing written or heard while it loaded, the doc is now the copy: no need to save it again.
      const unchanged = this.version === this.savedVersion;
      if (copy === false) {
        found = 'blocked';
      } else if (copy != null && this.restore(copy)) {
        found = 'loaded';
        if (unchanged) this.savedVersion = this.version;
      } else if (copy != null) {
        console.warn(`@peerwire/js: doc ${JSON.stringify(this.name)}: the copy on this device isn't a doc this version reads; the next save replaces it`);
      }
    } catch (e) {
      found = 'blocked';
      this.storeFailed(e);
    }
    this.loadDone(found);
  }

  // The load ended, with what it found, or 'blocked' at LOAD_MS (a copy that comes after is dropped). Blocked, the
  // opening couldn't read the copy, so it never writes it: it may hold what this device kept, or a newer release's
  // copy. forget() still deletes it.
  loadDone(found) {
    if (!this.loading) return;
    this.loading = false;
    this.loadWait?.();
    this.loadWait = null;
    if (found === 'blocked') this.blocked = true;
    this.loadEnd(found);
    // Closed while it loaded: what was written meanwhile is saved with the copy (see close), then the doc forgets
    // it all.
    if (this.closed) {
      this.closingCopy?.(this.version !== this.savedVersion && !this.blocked ? this.snapshot() : null);
      this.clear();
      return;
    }
    this.nextAsk(false);
    for (const [p, st] of this.members) if (st.theirs) this.serve(p, st);
    // Writes and frames while it loaded weren't saved, so a copy of part of the doc never replaced it.
    if (this.version !== this.savedVersion) this.dirty();
  }

  joined(peer) {
    if (this.closed) return;
    this.relays?.joined(peer);
    this.startAsk(peer);
  }

  left(peer) {
    const st = this.members.get(peer);
    st?.answerWait?.();
    st?.askWait?.();
    this.members.delete(peer);
    this.queue = this.queue.filter((p) => p !== peer);
    if (this.asking === peer) this.nextAsk(true);
    this.emitSync();
  }

  // A member's route changed: one better routed than the member whose turn it is may take the turn.
  rerouted() {
    if (!this.closed) this.better();
  }

  // A doc frame from member `peer`, already parsed from JSON. One that is malformed is dropped.
  receive(peer, m) {
    if (this.closed) return;
    if (m?.t === SAVING) {
      if (this.relays && typeof m.s === 'string' && SAVE_ID.test(m.s)) this.relays.saving(peer, m.s);
      return;
    }
    const f = parseFrame(m, peer);
    if (!f) return;
    if (f.t === ASK) return this.onAsk(peer, f);
    if (f.t === MORE) return this.onMore(peer, f);
    this.join(f, peer);
    if (f.q !== undefined) this.landed(peer, f);
  }

  // Joins frame `f`, a delta or an answer from member `peer`, or from a copy (`peer` null): the one on this device,
  // whose changes come from ourselves, or on the relays (`origin` 'relays').
  join(f, peer, origin) {
    this.claimed = false;
    this.claim = null;
    this.unheard = false;
    this.dropped = f.t === ANSWER && peer ? [] : null;
    this.apply(f, peer);
    // Entries on our own dots, from the lowest to the highest: all above n, as [1, n] is in our context, so the
    // span claims nothing a member couldn't claim with one dead range, and it is sent on as one.
    if (this.claim) {
      const [a, b] = this.claim;
      this.claim = null;
      this.record(this.writer, a, b);
      this.dropped?.push([this.writer, a, b]);
    }
    if (this.claimed) this.newIncarnation();
    this.flush(origin ?? peer ?? this.host.self, false);
    // Entries a catch-up told us are dead go from our other members too, as if we deleted them: they may
    // have missed the deletion as we did (a partition), or never been told (a member's claim sent to some).
    // One range for each range of the answer that dropped any (see kill), while the sender's budget allows.
    if (this.dropped?.length) this.forward(peer, this.dropped);
    this.dropped = null;
    // A write of the sender's own that we count as seen yet don't hold: it was deleted before it reached us,
    // or a member claimed the sender's dots ahead of it. Our ask shows the sender our context.
    if (this.unheard) this.startAsk(peer);
  }

  // A piece of an answer, applied: if it answers our latest ask of the member, it is acknowledged (unless a
  // better routed member took the turn), and the last ends the ask.
  landed(peer, f) {
    const st = this.members.get(peer);
    const a = st?.ask;
    if (!a || f.q !== a.q) return;
    a.got += f.entries.length;
    a.n = f.n;
    a.last = this.now();
    if (!f.end) {
      if (!a.paused) this.host.send(peer, { d: this.name, t: MORE, q: a.q, i: f.i }, st.z);
      return this.emitSync();
    }
    st.ask = null;
    st.answered = true;
    st.preempted = false;
    if (f.s) this.synced = true;
    if (this.asking === peer) this.nextAsk(true);
    // A member whose turn passed as it stalled is asked again once it has answered, if it was due an ask meanwhile.
    if (st.stalled) {
      st.stalled = false;
      if (st.askAgain) {
        st.askAgain = false;
        this.startAsk(peer);
      }
    }
    this.emitSync();
  }

  // Local writes.

  writeMap(name, key, value) {
    this.writable();
    if (!validKey(key)) throw new RangeError(`a map key is a string of at most ${MAX_KEY_LENGTH} characters`);
    const { v, len } = asJson(value);
    const old = [...(this.colls.get('m' + name)?.keys.get(key) ?? [])];
    const cost = len + jsonLength(key) + jsonLength(name);
    if (!this.fits(cost, old)) return false;
    const e = this.mint('m', name, key, v, cost);
    for (const o of old) this.remove(o);
    this.flush(this.host.self, true);
    this.broadcast([e], old.map((o) => [o.w, o.n, o.n]));
    return true;
  }

  deleteKey(name, key) {
    this.writable();
    const old = this.colls.get('m' + name)?.keys.get(key);
    if (!old) return false;
    const dead = old.map((o) => [o.w, o.n, o.n]);
    for (const o of [...old]) this.remove(o);
    this.flush(this.host.self, true);
    this.broadcast([], dead);
    return true;
  }

  add(t, name, value) {
    this.writable();
    const { v, len } = asJson(value);
    const cost = len + jsonLength(null) + jsonLength(name);
    if (!this.fits(cost, [])) return null;
    const e = this.mint(t, name, null, v, cost);
    if (t === 'l') this.trim(this.colls.get('l' + name));
    this.flush(this.host.self, true);
    this.broadcast([e], []);
    return e.id;
  }

  deleteItem(name, id) {
    this.writable();
    const e = this.colls.get('s' + name)?.items.get(id);
    if (!e) return false;
    this.remove(e);
    this.flush(this.host.self, true);
    this.broadcast([], [[e.w, e.n, e.n]]);
    return true;
  }

  writable() {
    if (this.closed) throw new Error('doc closed');
  }

  // Whether an entry of `cost` fits in place of `old`. A doc that members filled refuses our write with a
  // warning, as every member refuses one that doesn't fit, so member data can't make the app's writes throw.
  // So does one with no dot left to write on: members claimed them all, and the doc has moved on as often as
  // it may (MAX_INCARNATIONS).
  fits(cost, old) {
    const entries = this.entries.size - old.length + 1;
    const length = this.length - old.reduce((s, o) => s + o.len, 0) + cost;
    if (this.ctx.next(this.writer, this.n + 1) > MAX_STAMP) {
      console.warn(`@peerwire/js: doc ${JSON.stringify(this.name)}: members claimed every write of ours; the write was dropped (close the doc and open it again)`);
      return false;
    }
    if (entries <= MAX_ENTRIES && length <= MAX_DOC_LENGTH) return true;
    console.warn(`@peerwire/js: doc ${JSON.stringify(this.name)} is full (${MAX_ENTRIES} entries or ${MAX_DOC_LENGTH} characters as JSON); the write was dropped`);
    return false;
  }

  // A new entry of ours, held and counted in our context, on the next dot we haven't seen: a member may have
  // claimed some (see put).
  mint(t, name, k, v, len) {
    const n = (this.n = this.ctx.next(this.writer, this.n + 1));
    const c = this.clock < MAX_STAMP ? ++this.clock : MAX_STAMP;
    this.ctx.add(this.writer, n, n);
    const e = { id: `${this.writer}.${n}`, w: this.writer, n, c, t, coll: name, k, v, len };
    this.insert(e);
    return e;
  }

  // A writer for our next writes that isn't in `taken` (our context, or a copy's writers: see restore), drawn
  // again a few times if it is (one chance in 2^32 for each writer of ours there).
  newIncarnation(taken = this.ctx.writers) {
    if (this.incarnations >= MAX_INCARNATIONS) return;
    this.incarnations++;
    let w;
    for (let i = 0; i < 4 && (w === undefined || taken.has(w)); i++) {
      w = this.host.self + (this.host.incarnation?.() ?? [...crypto.getRandomValues(new Uint8Array(4))].map((b) => b.toString(16).padStart(2, '0')).join(''));
    }
    this.writer = w;
    this.n = 0;
  }

  // State.

  // Joins frame `f` into the doc. Its entries and dead ranges never share a dot, so their order doesn't
  // matter, and 'change' compares each key or item before and after the whole frame.
  // A member's frame costs about one pass over what it names: dead ranges are merged per writer first, so a
  // writer's entries are walked once.
  apply(f, peer) {
    for (const x of f.entries) this.put(x, f.t === DELTA && peer);
    for (const [w, l] of Context.of(f.ranges).writers) this.kill(w, l);
  }

  put(x, sender) {
    const id = `${x.w}.${x.n}`;
    if (this.entries.has(id)) return;
    if (this.ctx.has(x.w, x.n)) {
      if (sender) this.unheard = true;
      return;
    }
    // An entry on our own writer that we didn't write (a member claiming our next dot): never held, as our
    // write would then take its id. Its dot counts as seen and dead, so our answers kill it where it went.
    // The frame's claims are taken as one span (see receive), so its entries cost one range, not one each.
    if (x.w === this.writer) {
      this.claimed = true;
      const s = this.claim;
      if (!s) this.claim = [x.n, x.n];
      else if (x.n < s[0]) s[0] = x.n;
      else if (x.n > s[1]) s[1] = x.n;
      return;
    }
    if (this.entries.size >= MAX_ENTRIES || this.length + x.len > MAX_DOC_LENGTH) return;
    if (!this.record(x.w, x.n, x.n)) return;
    if (x.c > this.clock) this.clock = x.c;
    const e = { id, w: x.w, n: x.n, c: x.c, t: x.t, coll: x.coll, k: x.k, v: x.v, len: x.len };
    this.insert(e);
    if (e.t === 'l') this.trim(this.colls.get('l' + e.coll));
  }

  // The dots in ranges `l` of writer w (sorted and apart) are dead: entries of theirs we hold go, except a
  // log's, which only its own `keep` trims (members may keep different lengths). Walks the fewer of the dots
  // named and the entries held. In a catch-up, each range that dropped entries is sent on from the lowest of
  // their dots to the highest (see receive): one range, not one per entry, and the answer named all of it dead.
  kill(w, l) {
    if (w === this.writer && l[l.length - 1][1] > this.n) this.claimed = true;
    const held = this.byWriter.get(w);
    if (held) {
      const spans = new Map(); // index in l -> the lowest and highest dot it dropped
      const drop = (i, e) => {
        this.remove(e);
        const s = spans.get(i);
        if (!s) spans.set(i, [e.n, e.n]);
        else if (e.n < s[0]) s[0] = e.n;
        else if (e.n > s[1]) s[1] = e.n;
      };
      let dots = 0;
      for (const [a, b] of l) dots += b - a + 1;
      if (dots < held.size) {
        for (let i = 0; i < l.length; i++) {
          for (let n = l[i][0]; n <= l[i][1]; n++) {
            const e = held.get(n);
            if (e && e.t !== 'l') drop(i, e);
          }
        }
      } else {
        for (const [n, e] of [...held]) {
          const i = seek(l, n);
          if (i < l.length && l[i][0] <= n && e.t !== 'l') drop(i, e);
        }
      }
      if (this.dropped) for (const [a, b] of spans.values()) this.dropped.push([w, a, b]);
    }
    for (const [a, b] of l) this.record(w, a, b);
  }

  // Counts dots a to b of w as seen, if the context has room or can make it. False if not. Our own dots past n
  // aren't ours yet, so a claim on them joins the claims before it in one span, from the lowest to the highest:
  // that claims no dot a member couldn't with one range, and our writer holds two ranges at most.
  record(w, a, b) {
    if (this.ctx.covers(w, a, b)) return true;
    if (w === this.writer && b > this.n) {
      const l = this.ctx.writers.get(w);
      const i = l ? seek(l, this.n + 1) : 0;
      if (l && i < l.length) {
        a = Math.min(a, l[i][0]);
        b = Math.max(b, l[l.length - 1][1]);
      }
    }
    if (!this.room(w, a, b)) this.makeRoom(w);
    if (!this.room(w, a, b)) return false;
    this.ctx.add(w, a, b);
    this.version++;
    if (!this.byWriter.has(w)) this.idle.add(w);
    return true;
  }

  room(w, a, b) {
    const writers = this.ctx.writers.has(w) || this.ctx.writers.size < MAX_WRITERS;
    return writers && (this.ctx.size < this.rangeCap || this.ctx.touches(w, a, b));
  }

  // A full context forgets what matters least: first the writers with no live entry (longest idle first, kept
  // in `idle` so finding them costs nothing), then the highest ranges of the writers with the most, most
  // first (never ours: those are claims, see record), each down to one range, until FORGET_RANGES are free: so
  // the writers are sorted about once per that many ranges recorded, not scanned for each. A forgotten dead dot
  // can come back from a member that missed its deletion; nothing else is lost.
  makeRoom(keep) {
    for (const w of this.idle) {
      if (this.ctx.writers.size < MAX_WRITERS && this.ctx.size < MAX_RANGES) return;
      if (w === keep || w === this.writer) continue;
      this.ctx.drop(w);
      this.idle.delete(w);
    }
    if (this.ctx.size < MAX_RANGES) return;
    const most = [...this.ctx.writers].filter(([w, l]) => w !== this.writer && l.length > 1).sort((x, y) => y[1].length - x[1].length);
    for (const [w, l] of most) {
      while (l.length > 1 && this.ctx.size > MAX_RANGES - FORGET_RANGES) this.ctx.dropLast(w);
      if (this.ctx.size <= MAX_RANGES - FORGET_RANGES) return;
    }
  }

  collection(t, name, create) {
    let coll = this.colls.get(t + name);
    if (!coll && create) {
      coll = { t, name, keys: t === 'm' ? new Map() : null, items: t === 'm' ? null : new Map(), heap: t === 'l' ? [] : null, keep: this.keeps.get(name) ?? DEFAULT_KEEP, list: null };
      this.colls.set(t + name, coll);
    }
    return coll;
  }

  insert(e) {
    const coll = this.collection(e.t, e.coll, true);
    this.touch(coll, e.t === 'm' ? e.k : e.id);
    this.entries.set(e.id, e);
    let held = this.byWriter.get(e.w);
    if (!held) {
      this.byWriter.set(e.w, (held = new Map()));
      this.idle.delete(e.w);
    }
    held.set(e.n, e);
    this.length += e.len;
    this.version++;
    if (e.t === 'm') {
      const list = coll.keys.get(e.k);
      if (list) list.push(e);
      else coll.keys.set(e.k, [e]);
    } else {
      coll.items.set(e.id, e);
      if (e.t === 'l') heapPush(coll.heap, e);
    }
    coll.list = null;
  }

  remove(e) {
    const coll = this.colls.get(e.t + e.coll);
    this.touch(coll, e.t === 'm' ? e.k : e.id);
    this.entries.delete(e.id);
    const held = this.byWriter.get(e.w);
    held.delete(e.n);
    if (!held.size) {
      this.byWriter.delete(e.w);
      if (this.ctx.writers.has(e.w)) this.idle.add(e.w);
    }
    this.length -= e.len;
    this.version++;
    if (e.t === 'm') {
      const list = coll.keys.get(e.k);
      list.splice(list.indexOf(e), 1);
      if (!list.length) coll.keys.delete(e.k);
    } else {
      coll.items.delete(e.id);
    }
    coll.list = null;
    if (!coll.keys?.size && !coll.items?.size) this.colls.delete(e.t + e.coll);
  }

  // Trims a log to its newest `keep` entries. Trimming is the only way a log's entries go, so its heap holds
  // exactly its items, oldest on top.
  trim(coll) {
    while (coll && coll.items.size > coll.keep) this.remove(heapPop(coll.heap));
  }

  // What a map key shows: of its values, the one that sorts last.
  static shown(list) {
    let top;
    for (const e of list ?? []) if (!top || after(e, top)) top = e;
    return top;
  }

  current(coll, ref) {
    const c = this.colls.get(coll.t + coll.name);
    if (!c) return undefined;
    return coll.t === 'm' ? Doc.shown(c.keys.get(ref)) : c.items.get(ref);
  }

  // Notes a key or item about to change, with what it showed before. Keyed by collection, then ref, as a name and
  // a key may hold any character.
  touch(coll, ref) {
    let refs = this.touched.get(coll.t + coll.name);
    if (!refs) this.touched.set(coll.t + coll.name, (refs = new Map()));
    if (!refs.has(ref)) refs.set(ref, { coll: { t: coll.t, name: coll.name }, ref, before: this.current(coll, ref) });
  }

  // Emits 'change' for what a frame or write changed, now that all of it is in, and saves a doc kept on this
  // device soon if it changed, and on the relays (unless it came from there).
  flush(from, local) {
    if (this.version !== this.savedVersion) this.dirty();
    if (this.version !== this.relaySeen) {
      this.relaySeen = this.version;
      if (!this.fromRelays) this.relays?.changed();
    }
    const touched = [...this.touched.values()].flatMap((refs) => [...refs.values()]);
    this.touched.clear();
    for (const { coll, ref, before } of touched) {
      const now = this.current(coll, ref);
      if (now === before) continue;
      const ev = { name: coll.name, type: TYPES[coll.t], [coll.t === 'm' ? 'key' : 'id']: ref, value: now?.v, from: now ? peerOf(now.w) : from, local };
      if (!now) ev.deleted = true;
      for (const fn of this.handlers) {
        try {
          fn(ev);
        } catch (e) {
          // A throwing handler must not stop the others or the sync: report it on its own.
          setTimeout(() => {
            throw e;
          });
        }
      }
    }
  }

  // Reads.

  get(t, name, ref) {
    const coll = this.colls.get(t + name);
    if (!coll) return undefined;
    return (t === 'm' ? Doc.shown(coll.keys.get(ref)) : coll.items.get(ref))?.v;
  }

  list(t, name) {
    const coll = this.colls.get(t + name);
    if (!coll) return EMPTY;
    if (!coll.list) {
      if (t === 'm') {
        const keys = [...coll.keys.keys()].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
        coll.list = Object.freeze(keys.map((key) => {
          const e = Doc.shown(coll.keys.get(key));
          return Object.freeze({ key, value: e.v, from: peerOf(e.w) });
        }));
      } else {
        coll.list = Object.freeze([...coll.items.values()].sort(byStamp).map((e) => Object.freeze({ id: e.id, value: e.v, from: peerOf(e.w) })));
      }
    }
    return coll.list;
  }

  // Sending.

  // Sends entries and dead ranges to every member but `except`, as one delta or a few.
  broadcast(entries, ranges, except) {
    const frames = this.pack(DELTA, entries, ranges, false);
    for (const p of this.host.peers()) if (p !== except) for (const f of frames) this.host.send(p, f, this.members.get(p)?.z);
  }

  // Sends dead ranges member `peer`'s answer named on to our other members, unless its `chars` budget is empty,
  // and charges it their JSON text once for each member: what a member's answers can have us send the others
  // comes from the budget of what its asks can have us send it.
  forward(peer, ranges) {
    const st = this.member(peer);
    const now = this.now();
    if (this.early(st, 'chars', now) > 0) return;
    const to = this.host.peers().filter((p) => p !== peer);
    const frames = this.pack(DELTA, [], ranges, false);
    const chars = frames.reduce((s, f) => s + JSON.stringify(f).length, 0);
    this.spend(st, 'chars', chars * to.length, now);
    for (const p of to) for (const f of frames) this.host.send(p, f, this.members.get(p)?.z);
  }

  // Frames of kind `t` holding `entries` and dead `ranges`, each under MESSAGE_BYTES as UTF-8, with `end`
  // on the last if asked.
  pack(t, entries, ranges, end) {
    const frames = [];
    let f;
    let bytes;
    let index;
    const start = () => {
      f = { d: this.name, t, ws: [] };
      index = new Map();
      bytes = 3 * jsonLength(this.name) + 64;
    };
    const writer = (w) => {
      let i = index.get(w);
      if (i === undefined) {
        index.set(w, (i = f.ws.length));
        f.ws.push(w);
        bytes += WRITER_BYTES;
      }
      return i;
    };
    const place = (field, w, size) => {
      const n = (f.e?.length ?? 0) + (f.x?.length ?? 0);
      if (n && (n >= FRAME_ITEMS || bytes + size + (index.has(w) ? 0 : WRITER_BYTES) > MESSAGE_BYTES)) {
        frames.push(f);
        start();
      }
      bytes += size;
      return (f[field] ??= []);
    };
    start();
    for (const e of entries) place('e', e.w, ENTRY_BYTES + 3 * e.len).push([writer(e.w), e.n, e.c, e.t, e.coll, e.k, e.v]);
    for (const [w, a, b] of ranges) place('x', w, RANGE_BYTES).push([writer(w), a, b]);
    frames.push(f);
    if (end) f.end = 1;
    return frames;
  }

  // A member's catch-up: when each of its budgets (BUDGETS) is whole again; its latest ask while it waits for
  // them (`theirs`, numbered `want`), our answer under way (`out`, see answerOf) and the wait for the budgets;
  // whether it reads deflated frames (`z`); our latest ask of it (`ask`) until its answer ends, the wait for our
  // budget to ask it, whether it has answered one, whether its turn passed as it stalled (`stalled`) and
  // another ask is due once it answers, and whether a better routed member took its turn since its last answer
  // ended (`preempted`, see better).
  member(peer) {
    let st = this.members.get(peer);
    if (!st) this.members.set(peer, (st = { answers: 0, asks: 0, chars: 0, theirs: null, want: 0, out: null, answerWait: null, z: false, ask: null, askWait: null, answered: false, stalled: false, askAgain: false, preempted: false }));
    return st;
  }

  now() {
    return this.host.now?.() ?? performance.now();
  }

  // How long until the member's budget `kind` has one unit to spend; at most 0 if it has now. A budget is kept
  // as the time it is whole again, so nothing accumulates rounding.
  early(st, kind, now) {
    const [burst, every] = BUDGETS[kind];
    return Math.max(st[kind], now) - now - (burst - 1) * every;
  }

  // Spends `n` units of it. An answer's characters, or what we send on for the member, may take it past empty:
  // the next answer waits until it's back.
  spend(st, kind, n, now) {
    st[kind] = Math.max(st[kind], now) + n * BUDGETS[kind][1];
  }

  // Runs `fn` in `ms` if the member stays; `wait` ('answerWait' or 'askWait') holds it, one of each at a time.
  later(peer, st, wait, ms, fn) {
    st[wait] ??= this.host.after(Math.ceil(ms), () => {
      st[wait] = null;
      if (!this.closed && this.members.get(peer) === st) fn();
    });
  }

  // Catching up.

  // Queues an ask of `peer`. One whose turn passed as it stalled isn't asked again until it answers, so a member
  // that never does holds the others' turns up once, not each time it prompts us.
  startAsk(peer) {
    const st = this.member(peer);
    if (st.stalled) return void (st.askAgain = true);
    if (this.asking === peer || this.queue.includes(peer)) return;
    const now = this.now();
    const wait = this.early(st, 'asks', now);
    if (wait > 0) return this.later(peer, st, 'askWait', wait, () => this.startAsk(peer));
    this.spend(st, 'asks', 1, now);
    this.queue.push(peer);
    if (!this.asking) this.nextAsk(false);
    else this.better();
  }

  // How a member is reached, as a rank (RANK): 0 linked directly or with a host that doesn't say, 1 relayed by a
  // member, 2 over signalling.
  rank(peer) {
    return RANK[this.host.route?.(peer)] ?? 0;
  }

  // Gives the turn to the next member in the queue, the best routed (of those, the first queued), and asks it;
  // `done` ends the turn under way. None is asked while the copy on this device loads (see load).
  nextAsk(done) {
    if (done) {
      this.cancelWait?.();
      this.cancelWait = null;
      this.asking = null;
    }
    if (this.asking || this.closed || this.loading) return;
    const peers = new Set(this.host.peers());
    this.queue = this.queue.filter((p) => peers.has(p));
    if (this.queue.length) {
      let k = 0;
      for (let j = 1; j < this.queue.length; j++) if (this.rank(this.queue[j]) < this.rank(this.queue[k])) k = j;
      const [p] = this.queue.splice(k, 1);
      const st = this.member(p);
      const now = this.now();
      this.asking = p;
      st.ask = { q: ++this.askNo, at: now, last: now, got: 0, n: 0, paused: false };
      this.host.send(p, this.askFrame(st.ask.q), st.z);
      this.watch(p, st.ask);
    }
    this.emitSync();
  }

  // Watches the turn of member `p`, whose answer to `ask` is under way: it passes on once the answer has gone
  // ASK_WAIT_MS without a piece (the member stalled) or taken TURN_MS, if another member is waiting to be asked.
  // A member that stalled with none waiting keeps the turn, until one is (see better).
  watch(p, ask) {
    const due = Math.min(ask.last + ASK_WAIT_MS, this.queue.length ? ask.at + TURN_MS : Infinity);
    this.cancelWait = this.host.after(Math.ceil(due - this.now()), () => {
      this.cancelWait = null;
      const st = this.members.get(p);
      if (this.closed || this.asking !== p || st?.ask !== ask) return;
      const now = this.now();
      if (now - ask.last >= ASK_WAIT_MS) st.stalled = true;
      else if (!this.queue.length || now - ask.at < TURN_MS) return this.watch(p, ask);
      if (this.queue.length) this.nextAsk(true);
    });
  }

  // With a member waiting to be asked, the turn passes on if the member whose turn it is stalled or has had
  // TURN_MS, or the one waiting is better routed (linked directly, while the answer comes over signalling, say):
  // then the answer under way is paused, and its member asked again after, with what we hold by then.
  // A member is paused so once until an answer of its ends (`preempted`): one better routed, queued again on each
  // of its turns, waits for the turn to pass instead.
  better() {
    const p = this.asking;
    if (!p || !this.queue.length) return;
    const st = this.member(p);
    if (st.stalled || this.now() - st.ask.at >= TURN_MS) return this.nextAsk(true);
    const r = this.rank(p);
    if (st.preempted || !this.queue.some((x) => this.rank(x) < r)) return;
    st.ask.paused = true;
    st.preempted = true;
    this.queue.push(p);
    this.nextAsk(true);
  }

  // Where catching up stands: 'alone' with no member to catch up from, 'synced' once caught up (see synced),
  // else 'loading' from the member whose turn it is, with `have` of the `total` entries its answer carries
  // (as it says; 0 of 0 until its first piece lands). And where the doc came from: 'members' once caught up,
  // else 'relays' once it joined a save on the relays, `at` (ms) the time its saver claims, or null.
  status() {
    const peers = this.host.peers().length;
    const source = this.synced ? 'members' : this.relays?.at != null ? 'relays' : null;
    const at = source === 'relays' ? this.relays.at : null;
    if (!peers || this.synced) return { state: peers ? 'synced' : 'alone', from: null, have: 0, total: 0, source, at };
    const a = this.asking ? this.members.get(this.asking)?.ask : null;
    return { state: 'loading', from: this.asking, have: a ? Math.min(a.got, a.n) : 0, total: a?.n ?? 0, source, at };
  }

  // Emits 'sync' if where catching up stands changed. Every member having answered counts as caught up: none
  // had caught up itself (they opened the doc together), so what they hold is all there is.
  emitSync() {
    if (this.closed) return;
    const peers = this.host.peers();
    if (!this.synced && peers.length && peers.every((p) => this.members.get(p)?.answered)) this.synced = true;
    const s = this.status();
    const o = this.sync;
    if (s.state === o.state && s.from === o.from && s.have === o.have && s.total === o.total && s.source === o.source && s.at === o.at) return;
    this.sync = Object.freeze(s);
    for (const fn of this.syncHandlers) {
      try {
        fn(this.sync);
      } catch (e) {
        setTimeout(() => {
          throw e;
        });
      }
    }
    this.relays?.synced();
  }

  // Our context, so the member answers with what we lack, and `q` for the answer to name. Under MESSAGE_BYTES at
  // the caps; were it not, a shorter one only asks for more.
  askFrame(q) {
    const ws = [];
    const r = [];
    let bytes = 3 * jsonLength(this.name) + 64 + WRITER_BYTES;
    for (const [w, l] of this.ctx.writers) {
      if (bytes + WRITER_BYTES + RANGE_BYTES > MESSAGE_BYTES) break;
      const i = ws.push(w) - 1;
      bytes += WRITER_BYTES;
      for (const [a, b] of l) {
        if (bytes + RANGE_BYTES > MESSAGE_BYTES || r.length >= MAX_RANGES) break;
        r.push([i, a, b]);
        bytes += RANGE_BYTES;
      }
    }
    const f = { d: this.name, t: ASK, ws, r, q };
    if (this.host.deflate) f.z = 1;
    return f;
  }

  onAsk(peer, f) {
    if (f.ranges.some(([w, , b]) => w === this.writer && b > this.n)) this.newIncarnation();
    const st = this.member(peer);
    st.z = f.z;
    // Answered now or, past the member's budget, once it allows: only its latest ask waits, in place of the answer
    // under way.
    st.theirs = Context.of(f.ranges);
    st.want = f.q;
    st.out = null;
    this.serve(peer, st);
    // It has seen dots we haven't: ask it back.
    if (f.ranges.some(([w, a, b]) => !this.ctx.covers(w, a, b))) this.startAsk(peer);
  }

  // Starts the answer to the member's latest ask once its answers budget allows, then sends pieces while the
  // answer's window and the chars budget allow; past a budget, it waits for it. A wait may find the ask answered
  // already, when a later one came as the budget did. While the copy on this device loads, the ask waits for it.
  serve(peer, st) {
    if (this.loading) return;
    const now = this.now();
    if (st.theirs) {
      const wait = this.early(st, 'answers', now);
      if (wait > 0) return this.later(peer, st, 'answerWait', wait, () => this.serve(peer, st));
      this.spend(st, 'answers', 1, now);
      st.out = this.answerOf(st.theirs, st.want);
      st.theirs = null;
    }
    const o = st.out;
    while (o && st.out === o && o.next - o.acked - 1 < WINDOW) {
      const wait = this.early(st, 'chars', this.now());
      if (wait > 0) return this.later(peer, st, 'answerWait', wait, () => this.serve(peer, st));
      this.piece(peer, st, o);
    }
  }

  // An answer to ask `q`, whose context is `theirs`: the ids of the entries it lacks, the newest first (ids, so
  // an answer that waits holds none deleted meanwhile), then every dot we know is dead. Pieces are numbered from
  // 0 (`next`, the next one), and acknowledged up to `acked`.
  answerOf(theirs, q) {
    const list = [];
    for (const e of this.entries.values()) if (!theirs.has(e.w, e.n)) list.push(e);
    list.sort((a, b) => byStamp(b, a));
    return { q, list: list.map((e) => e.id), i: 0, dead: this.dead(), j: 0, next: 0, acked: -1 };
  }

  // Sends answer `o`'s next piece: its next entries still held, then its dead ranges, about the route's
  // PIECE_CHARS as the chars budget counts them (one entry at least, in two frames if MESSAGE_BYTES can't hold
  // it), charged to the member's budget. The last has `end`, and `s` if we have caught up.
  piece(peer, st, o) {
    const size = PIECE_CHARS[this.host.route?.(peer)] ?? PIECE_CHARS.direct;
    const entries = [];
    const ranges = [];
    let chars = 0;
    for (; o.i < o.list.length; o.i++) {
      const e = this.entries.get(o.list[o.i]);
      // Gone since the ask (a dot is never used again): the member hears of that as the others did.
      if (!e) continue;
      if (chars && chars + ENTRY_BYTES + e.len > size) break;
      entries.push(e);
      chars += ENTRY_BYTES + e.len;
    }
    for (; o.i === o.list.length && o.j < o.dead.length && (!chars || chars + RANGE_BYTES <= size); o.j++) {
      ranges.push(o.dead[o.j]);
      chars += RANGE_BYTES;
    }
    const end = o.i === o.list.length && o.j === o.dead.length;
    if (end) st.out = null;
    this.spend(st, 'chars', chars, this.now());
    const frames = this.pack(ANSWER, entries, ranges, end);
    for (const f of frames) {
      Object.assign(f, { q: o.q, i: o.next++, n: o.list.length });
      if (f.end && this.synced) f.s = 1;
    }
    for (const f of frames) this.host.send(peer, f, st.z);
  }

  // Piece `i` of our answer to the member's ask `q` was applied: the window moves on.
  onMore(peer, f) {
    const st = this.members.get(peer);
    const o = st?.out;
    if (!o || f.q !== o.q || f.i <= o.acked || f.i >= o.next) return;
    o.acked = f.i;
    this.serve(peer, st);
  }

  // The dots we have seen and don't hold, as ranges.
  dead() {
    const out = [];
    for (const [w, l] of this.ctx.writers) {
      const live = [...(this.byWriter.get(w)?.keys() ?? [])].sort((a, b) => a - b);
      let i = 0;
      for (const [a, b] of l) {
        let s = a;
        for (; i < live.length && live[i] <= b; i++) {
          if (live[i] < s) continue;
          if (live[i] > s) out.push([w, s, live[i] - 1]);
          s = live[i] + 1;
        }
        if (s <= b) out.push([w, s, b]);
      }
    }
    return out;
  }

  // The copy on this device.

  // The doc as this device keeps it: its entries and the dots it knows are dead, as the frames of an answer, and
  // its Lamport clock.
  snapshot() {
    return { v: 1, c: this.clock, f: this.pack(ANSWER, [...this.entries.values()], this.dead(), false) };
  }

  // Joins `copy` (see snapshot) into the doc as an answer from ourselves, checked frame by frame as a member's:
  // false, with nothing joined, if any of it is off. A writer of ours in it is an earlier incarnation of ours (a
  // stable secret keeps our PeerId): if this opening drew one of them and hasn't written on it, it draws again,
  // so the copy's dots stay that writer's and aren't taken for a claim on ours (see put).
  restore(copy) {
    if (copy === null || typeof copy !== 'object' || copy.v !== 1 || !index(copy.c) || !Array.isArray(copy.f) || copy.f.length > STORED_FRAMES) return false;
    const frames = copyFrames(this.name, copy.f);
    if (!frames) return false;
    this.joinCopy(frames, copy.f, copy.c, null);
    return true;
  }

  // Joins `frames`, a copy's checked frames (`f` as they came), and its Lamport clock `c`; `origin` as in join.
  joinCopy(frames, f, c, origin) {
    if (!this.n && f.some((m) => m.ws.includes(this.writer))) this.newIncarnation(new Set(f.flatMap((m) => m.ws)));
    // The frames hold the entries first, then the dead dots, so until those join, each live entry between dead
    // dots takes a range of its own: the context may hold one more for each entry the doc can hold meanwhile,
    // then forgets what is past MAX_RANGES, as ever (a copy of a doc that held at most that many loads as it was).
    this.rangeCap = MAX_RANGES + MAX_ENTRIES;
    for (const x of frames) this.join(x, null, origin);
    this.rangeCap = MAX_RANGES;
    if (this.ctx.size >= MAX_RANGES) this.makeRoom(null);
    if (c > this.clock) this.clock = c;
  }

  // The copy on the relays (relays.js).

  // Joins the pieces of a whole save from the relays ({ c, f } each), as one copy, with the origin 'relays': its
  // deletions say they came from there. False, with nothing joined, if any frame is off.
  joinRelays(pieces) {
    const f = pieces.flatMap((p) => p.f);
    const frames = copyFrames(this.name, f);
    if (!frames) return false;
    this.fromRelays = true;
    try {
      this.joinCopy(frames, f, Math.max(...pieces.map((p) => p.c)), 'relays');
    } finally {
      this.fromRelays = false;
    }
    return true;
  }

  // Whether the doc holds something the whole save of `pieces` lacks: a dot it hasn't seen, or a deletion (a dot it
  // holds that we have seen and don't hold). True if its frames are off.
  lacks(pieces) {
    const frames = copyFrames(this.name, pieces.flatMap((p) => p.f));
    if (!frames) return true;
    const ranges = [];
    const live = [];
    for (const x of frames) {
      for (const e of x.entries) {
        ranges.push([e.w, e.n, e.n]);
        live.push(e);
      }
      ranges.push(...x.ranges);
    }
    const theirs = Context.of(ranges);
    for (const [w, l] of this.ctx.writers) for (const [a, b] of l) if (!theirs.covers(w, a, b)) return true;
    return live.some((e) => !this.entries.has(`${e.w}.${e.n}`) && this.ctx.has(e.w, e.n));
  }

  // The doc for a save on the relays: its clock, entries, dead ranges, and how they go in answer frames.
  relayItems() {
    return { c: this.clock, entries: [...this.entries.values()], ranges: this.dead(), pack: (entries, ranges) => this.pack(ANSWER, entries, ranges, false) };
  }

  // Saves the doc SAVE_MS from now, or once the last save allows (see saved), unless a save is due already
  // (see flush) or with the store: then saved() sees to it.
  dirty() {
    if (!this.store || this.forgotten || this.blocked || this.loading || this.closed || this.saveWait || this.inFlight) return;
    this.saveWait = this.host.after(Math.max(SAVE_MS, this.nextSave - this.host.now()), () => {
      this.saveWait = null;
      this.save();
    });
  }

  // Saves the doc now (it is due, or the page was hidden), or, with a save in flight, as soon as that ends.
  // Resolves once saved.
  save() {
    if (this.inFlight) this.saveSoon = true;
    else this.write();
    return this.saving;
  }

  // Hands the store the doc now if it changed since the last save, unless its copy is still loading, this
  // opening couldn't read it (blocked), or the doc is closed. Whether it did.
  write() {
    if (!this.store || this.forgotten || this.blocked || this.loading || this.closed || this.version === this.savedVersion) return false;
    this.saveWait?.();
    this.saveWait = null;
    const copy = this.snapshot();
    const version = (this.savedVersion = this.version);
    const store = this.store;
    this.inFlight = true;
    this.saving = (async () => store.save(copy))().then((bytes) => this.saved(bytes, true), (e) => {
      if (this.savedVersion === version) this.savedVersion = -1;
      this.storeFailed(e);
      return this.saved(0, false);
    });
    return true;
  }

  // A save of `bytes` ended: the next waits SAVE_MS, or `bytes` at SAVE_RATE if longer, and saves the changes
  // made meanwhile once; one asked at once meanwhile goes now. A save that failed is tried again with the next
  // change, SAVE_MS after it ended, twice that after two failures in a row, and so on up to SAVE_DOUBLINGS
  // doublings; one that succeeds starts that again. A promise of the save it started, if any, so `saving`
  // resolves once both are done.
  saved(bytes, ok) {
    this.inFlight = false;
    const wait = ok ? ((Number(bytes) || 0) * 1000) / SAVE_RATE : SAVE_MS * 2 ** Math.min(this.saveFails, SAVE_DOUBLINGS);
    this.saveFails = ok ? 0 : this.saveFails + 1;
    this.nextSave = this.host.now() + Math.max(SAVE_MS, wait);
    if (this.saveSoon) {
      this.saveSoon = false;
      if (this.write()) return this.saving;
    }
    if (ok && this.version !== this.savedVersion) this.dirty();
  }

  // Storage that fails (private browsing, a full disk) is said once; the doc goes on in memory.
  storeFailed(e) {
    if (this.storeWarned) return;
    this.storeWarned = true;
    console.warn(`@peerwire/js: doc ${JSON.stringify(this.name)}: this device can't keep a copy (${e?.message ?? e}); the doc goes on without one`);
  }
}

const EMPTY = Object.freeze([]);

// The views room.doc() hands out. Writes return false or null when members have filled the doc.

class DocMap {
  constructor(doc, name) {
    this.doc = doc;
    this.name = name;
  }

  set(key, value) {
    return this.doc.writeMap(this.name, key, value);
  }

  get(key) {
    return this.doc.get('m', this.name, key);
  }

  has(key) {
    return this.get(key) !== undefined;
  }

  delete(key) {
    return this.doc.deleteKey(this.name, key);
  }

  entries() {
    return this.doc.list('m', this.name);
  }
}

class DocSet {
  constructor(doc, name) {
    this.doc = doc;
    this.name = name;
  }

  add(value) {
    return this.doc.add('s', this.name, value);
  }

  get(id) {
    return this.doc.get('s', this.name, id);
  }

  has(id) {
    return this.get(id) !== undefined;
  }

  delete(id) {
    return this.doc.deleteItem(this.name, id);
  }

  entries() {
    return this.doc.list('s', this.name);
  }
}

class DocLog {
  constructor(doc, name) {
    this.doc = doc;
    this.name = name;
  }

  append(value) {
    return this.doc.add('l', this.name, value);
  }

  get(id) {
    return this.doc.get('l', this.name, id);
  }

  entries() {
    return this.doc.list('l', this.name);
  }
}
