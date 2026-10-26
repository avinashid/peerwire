// Shared documents: state that every member of a room holds and that converges, made with
// room.doc(name). Members that join later, or come back after a partition, catch up.
//
//   const doc = room.doc('board');
//   const meta = doc.map('meta');                    // meta.set('title', 'Plans'); meta.get('title')
//   const strokes = doc.set('strokes');              // const id = strokes.add(stroke); strokes.delete(id)
//   const chat = doc.log('messages', { keep: 500 }); // chat.append({ text })
//   doc.on('change', (ev) => render(ev));
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
// member that joins, is asked with our context, one member at a time, so a joiner gets the doc once rather
// than from every member: the answer holds the entries we lack and the dots the member knows are dead. Every
// frame is a piece of state joined into ours (delta-state CRDTs, Almeida et al. 2018), so frames apply in any
// order and any number of times, and every member that has heard the same frames holds the same doc.
//
// Members are trusted alike to write, as in the room itself: any member can overwrite, delete or fill a doc,
// and a member relaying a catch-up can claim any writer, so `from` is as good as the member that told us.
// What the doc defends is each member's app and resources. Frames are checked field by field and dropped
// whole if anything is off; state lives in Maps; names, keys, values, entries, characters, writers, context
// ranges and the answers each member gets are capped. A Lamport stamp may run at most MAX_CLOCK_STEP ahead of
// ours per frame. A member could claim dots another member hasn't written yet, so that the room ignores its
// next writes: a doc that sees such a claim on its own dots moves to a new incarnation.

export const MAX_NAME_LENGTH = 128; // a doc's or a collection's name, in UTF-16 code units
export const MAX_KEY_LENGTH = 256; // a map key
export const MAX_VALUE_LENGTH = 64 * 1024; // a value as JSON text
export const MAX_VALUE_DEPTH = 64; // how deeply a value's arrays and objects nest
export const MAX_ENTRIES = 10000; // live entries in a doc, all collections
export const MAX_DOC_LENGTH = 4 * 1024 * 1024; // what a doc's values, keys and names take as JSON text
export const MAX_WRITERS = 1024; // writers a doc's context holds
export const MAX_RANGES = 8192; // ranges of dots a doc's context holds
export const DEFAULT_KEEP = 1000; // entries a log keeps unless doc.log() says otherwise
export const MAX_CLOCK_STEP = 2 ** 32; // how far a frame's stamps may run ahead of our clock
export const MESSAGE_BYTES = 512 * 1024; // the most a doc frame takes as UTF-8, half room.send's 1 MiB
// Asks a member gets answered while it stays, asks we send it, and how often a member's new incarnation
// (a doc closed and opened again) clears what we have sent it.
const MAX_ANSWERS = 8;
const MAX_ASKS = 8;
const MAX_RESETS = 2;
// How long an answer may take before the next member is asked.
const ASK_WAIT_MS = 5000;
// Incarnations a doc moves through when members claim its future dots.
const MAX_INCARNATIONS = 16;
// The longest array a frame may hold: entries, dead ranges, writers.
const FRAME_ITEMS = 16384;
const MAX_STAMP = Number.MAX_SAFE_INTEGER;
const DELTA = 1;
const ASK = 2;
const ANSWER = 3;
const TYPES = { m: 'map', s: 'set', l: 'log' };
const WRITER = /^[0-9a-f]{72}$/;
// Estimated UTF-8 bytes in a frame: 3 per character of JSON text, and room for numbers and punctuation (a
// range is at most [1023,9007199254740991,9007199254740991], 41 bytes).
const WRITER_BYTES = 80;
const RANGE_BYTES = 48;
const ENTRY_BYTES = 96;

const count = (n) => Number.isSafeInteger(n) && n >= 1;
const peerOf = (w) => w.slice(0, 64);
// Whether entry a sorts after entry b: the later stamp, then the higher writer, then the later dot.
const after = (a, b) => a.c > b.c || (a.c === b.c && (a.w > b.w || (a.w === b.w && a.n > b.n)));
const byStamp = (a, b) => (after(a, b) ? 1 : after(b, a) ? -1 : 0);
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
function parseFrame(m, peer) {
  if (m === null || typeof m !== 'object' || Array.isArray(m)) return null;
  const { t, ws } = m;
  if (t !== DELTA && t !== ASK && t !== ANSWER) return null;
  if (!Array.isArray(ws) || ws.length > FRAME_ITEMS || !ws.every((w) => typeof w === 'string' && WRITER.test(w))) return null;
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
  if (t === ASK) {
    if (typeof m.i !== 'string' || !WRITER.test(m.i) || peerOf(m.i) !== peer) return null;
    return { t, ranges, inc: m.i };
  }
  const entries = [];
  if (m.e !== undefined) {
    if (!Array.isArray(m.e) || m.e.length > FRAME_ITEMS) return null;
    for (const x of m.e) {
      if (!Array.isArray(x) || x.length !== 7) return null;
      const [i, n, c, type, coll, k, v] = x;
      const w = writer(i);
      if (!w || !count(n) || !count(c) || !Object.hasOwn(TYPES, type) || !validName(coll)) return null;
      if (type === 'm' ? !validKey(k) : k !== null) return null;
      // A delta carries only its sender's writes.
      if (t === DELTA && peerOf(w) !== peer) return null;
      const text = JSON.stringify(v);
      if (text === undefined || text.length > MAX_VALUE_LENGTH || !freezeJson(v)) return null;
      entries.push({ w, n, c, t: type, coll, k, v, len: text.length + jsonLength(k) + jsonLength(coll) });
    }
  }
  if (m.end !== undefined && m.end !== 1) return null;
  return { t, entries, ranges, end: m.end === 1 };
}

// One doc, shared by every member that opens the same name. `host` is the room's side:
// { self, peers() (joined members), send(peer, message), after(ms, fn) -> cancel, closed(doc), incarnation() }.
export class Doc {
  constructor(name, host) {
    if (!validName(name)) throw new RangeError(`a doc name is 1 to ${MAX_NAME_LENGTH} characters`);
    this.name = name;
    this.host = host;
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
    this.touched = new Map(); // changes during one frame or write: ref -> { coll, ref, before }
    this.members = new Map(); // peer -> { answers, asks, sent, inc, resets }
    this.queue = []; // members to ask, in turn
    this.asking = null; // the member asked, until its answer ends or ASK_WAIT_MS passes
    this.cancelWait = null;
    this.closed = false;
    // While a frame is applied: a claim on our own dots, a sender's write we count as seen and don't hold, and
    // the entries a catch-up dropped.
    this.claimed = false;
    this.unheard = false;
    this.dropped = null;
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

  on(event, fn) {
    if (event !== 'change') throw new TypeError(`a doc has one event, 'change', not ${String(event)}`);
    this.handlers.add(fn);
    return () => this.off(event, fn);
  }

  off(event, fn) {
    if (event === 'change') this.handlers.delete(fn);
  }

  // Stops syncing and forgets the doc: room.doc(name) later opens it afresh and catches up again.
  close() {
    if (this.closed) return;
    this.closed = true;
    this.cancelWait?.();
    this.queue = [];
    this.asking = null;
    this.handlers.clear();
    this.entries.clear();
    this.byWriter.clear();
    this.colls.clear();
    this.members.clear();
    this.host.closed?.(this);
  }

  // The room's side.

  // Asks every member: the doc was just opened.
  open() {
    for (const p of this.host.peers()) this.startAsk(p);
  }

  joined(peer) {
    if (!this.closed) this.startAsk(peer);
  }

  left(peer) {
    this.members.delete(peer);
    this.queue = this.queue.filter((p) => p !== peer);
    if (this.asking === peer) this.nextAsk(true);
  }

  // A doc frame from member `peer`, already parsed from JSON. One that is malformed is dropped.
  receive(peer, m) {
    if (this.closed) return;
    const f = parseFrame(m, peer);
    if (!f) return;
    if (f.t === ASK) return this.onAsk(peer, f);
    this.claimed = false;
    this.unheard = false;
    this.dropped = f.t === ANSWER ? [] : null;
    this.apply(f, peer);
    if (this.claimed) this.newIncarnation();
    this.flush(peer, false);
    // Entries a catch-up told us are dead go from our other members too, as if we deleted them: they may
    // have missed the deletion as we did (a partition), or never been told (a member's claim sent to some).
    if (this.dropped?.length) this.broadcast([], this.dropped, peer);
    this.dropped = null;
    // A write of the sender's own that we count as seen yet don't hold: it was deleted before it reached us,
    // or a member claimed the sender's dots ahead of it. Our ask shows the sender our context.
    if (this.unheard) this.startAsk(peer);
    if (f.t === ANSWER && f.end && this.asking === peer) this.nextAsk(true);
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
  fits(cost, old) {
    const entries = this.entries.size - old.length + 1;
    const length = this.length - old.reduce((s, o) => s + o.len, 0) + cost;
    if (entries <= MAX_ENTRIES && length <= MAX_DOC_LENGTH) return true;
    console.warn(`@peerwire/js: doc ${JSON.stringify(this.name)} is full (${MAX_ENTRIES} entries or ${MAX_DOC_LENGTH} characters as JSON); the write was dropped`);
    return false;
  }

  // A new entry of ours, held and counted in our context.
  mint(t, name, k, v, len) {
    const n = ++this.n;
    const c = this.clock < MAX_STAMP ? ++this.clock : MAX_STAMP;
    this.ctx.add(this.writer, n, n);
    const e = { id: `${this.writer}.${n}`, w: this.writer, n, c, t, coll: name, k, v, len };
    this.insert(e);
    return e;
  }

  newIncarnation() {
    if (this.incarnations >= MAX_INCARNATIONS) return;
    this.incarnations++;
    const r = this.host.incarnation?.() ?? [...crypto.getRandomValues(new Uint8Array(4))].map((b) => b.toString(16).padStart(2, '0')).join('');
    this.writer = this.host.self + r;
    this.n = 0;
  }

  // State.

  // Joins frame `f` into the doc. Its entries and dead ranges never share a dot, so their order doesn't
  // matter, and 'change' compares each key or item before and after the whole frame.
  apply(f, peer) {
    const limit = this.clock + MAX_CLOCK_STEP;
    for (const x of f.entries) this.put(x, limit, f.t === DELTA && peer);
    for (const [w, a, b] of f.ranges) this.kill(w, a, b);
  }

  put(x, limit, sender) {
    const id = `${x.w}.${x.n}`;
    if (this.entries.has(id)) return;
    if (this.ctx.has(x.w, x.n)) {
      if (sender) this.unheard = true;
      return;
    }
    if (x.c > limit) return;
    if (this.entries.size >= MAX_ENTRIES || this.length + x.len > MAX_DOC_LENGTH) return;
    if (x.w === this.writer && x.n > this.n) this.claimed = true;
    if (!this.record(x.w, x.n, x.n)) return;
    if (x.c > this.clock) this.clock = x.c;
    const e = { id, w: x.w, n: x.n, c: x.c, t: x.t, coll: x.coll, k: x.k, v: x.v, len: x.len };
    this.insert(e);
    if (e.t === 'l') this.trim(this.colls.get('l' + e.coll));
  }

  // Dots a to b of writer w are dead: entries of theirs we hold go, except a log's, which only its own
  // `keep` trims (members may keep different lengths).
  kill(w, a, b) {
    if (w === this.writer && b > this.n) this.claimed = true;
    const held = this.byWriter.get(w);
    if (held) {
      if (b - a < held.size) {
        for (let n = a; n <= b; n++) {
          const e = held.get(n);
          if (e && e.t !== 'l') this.drop(e);
        }
      } else {
        for (const [n, e] of [...held]) if (n >= a && n <= b && e.t !== 'l') this.drop(e);
      }
    }
    this.record(w, a, b);
  }

  drop(e) {
    this.remove(e);
    this.dropped?.push([e.w, e.n, e.n]);
  }

  // Counts dots a to b of w as seen, if the context has room or can make it. False if not.
  record(w, a, b) {
    if (this.ctx.covers(w, a, b)) return true;
    if (!this.room(w, a, b)) this.makeRoom(w);
    if (!this.room(w, a, b)) return false;
    this.ctx.add(w, a, b);
    return true;
  }

  room(w, a, b) {
    const writers = this.ctx.writers.has(w) || this.ctx.writers.size < MAX_WRITERS;
    return writers && (this.ctx.size < MAX_RANGES || this.ctx.touches(w, a, b));
  }

  // A full context forgets what matters least: first the writers with no live entry (oldest first), then the
  // highest range of the writer with the most. A forgotten dead dot can come back from a member that missed
  // its deletion; nothing else is lost.
  makeRoom(keep) {
    for (const w of this.ctx.writers.keys()) {
      if (this.ctx.writers.size < MAX_WRITERS && this.ctx.size < MAX_RANGES) return;
      if (w !== keep && w !== this.writer && !this.byWriter.has(w)) this.ctx.drop(w);
    }
    while (this.ctx.size >= MAX_RANGES) {
      let most = null;
      for (const [w, l] of this.ctx.writers) if (!most || l.length > this.ctx.writers.get(most).length) most = w;
      if (!most || this.ctx.writers.get(most).length < 2) return;
      this.ctx.dropLast(most);
    }
  }

  collection(t, name, create) {
    let coll = this.colls.get(t + name);
    if (!coll && create) {
      coll = { t, name, keys: t === 'm' ? new Map() : null, items: t === 'm' ? null : new Map(), keep: this.keeps.get(name) ?? DEFAULT_KEEP, list: null };
      this.colls.set(t + name, coll);
    }
    return coll;
  }

  insert(e) {
    const coll = this.collection(e.t, e.coll, true);
    this.touch(coll, e.t === 'm' ? e.k : e.id);
    this.entries.set(e.id, e);
    let held = this.byWriter.get(e.w);
    if (!held) this.byWriter.set(e.w, (held = new Map()));
    held.set(e.n, e);
    this.length += e.len;
    if (e.t === 'm') {
      const list = coll.keys.get(e.k);
      if (list) list.push(e);
      else coll.keys.set(e.k, [e]);
    } else {
      coll.items.set(e.id, e);
    }
    coll.list = null;
  }

  remove(e) {
    const coll = this.colls.get(e.t + e.coll);
    this.touch(coll, e.t === 'm' ? e.k : e.id);
    this.entries.delete(e.id);
    const held = this.byWriter.get(e.w);
    held.delete(e.n);
    if (!held.size) this.byWriter.delete(e.w);
    this.length -= e.len;
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

  // Trims a log to its newest `keep` entries.
  trim(coll) {
    while (coll && coll.items.size > coll.keep) {
      let oldest = null;
      for (const e of coll.items.values()) if (!oldest || after(oldest, e)) oldest = e;
      this.remove(oldest);
    }
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

  touch(coll, ref) {
    const key = `${coll.t}${coll.name}\u0000${ref}`;
    if (!this.touched.has(key)) this.touched.set(key, { coll: { t: coll.t, name: coll.name }, ref, before: this.current(coll, ref) });
  }

  // Emits 'change' for what a frame or write changed, now that all of it is in.
  flush(from, local) {
    const touched = [...this.touched.values()];
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
    for (const p of this.host.peers()) if (p !== except) for (const f of frames) this.host.send(p, f);
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

  member(peer) {
    let st = this.members.get(peer);
    if (!st) this.members.set(peer, (st = { answers: 0, asks: 0, sent: new Set(), inc: null, resets: 0 }));
    return st;
  }

  // Catching up.

  startAsk(peer) {
    const st = this.member(peer);
    if (this.asking === peer || this.queue.includes(peer) || st.asks >= MAX_ASKS) return;
    st.asks++;
    this.queue.push(peer);
    if (!this.asking) this.nextAsk(false);
  }

  // Asks the next member in the queue; `done` ends the ask under way.
  nextAsk(done) {
    if (done) {
      this.cancelWait?.();
      this.cancelWait = null;
      this.asking = null;
    }
    if (this.asking || this.closed) return;
    const peers = new Set(this.host.peers());
    while (this.queue.length) {
      const p = this.queue.shift();
      if (!peers.has(p)) continue;
      this.asking = p;
      this.host.send(p, this.askFrame());
      this.cancelWait = this.host.after(ASK_WAIT_MS, () => this.asking === p && this.nextAsk(true));
      return;
    }
  }

  // Our context, so the member answers with what we lack. Under MESSAGE_BYTES at the caps; were it not, a
  // shorter one only asks for more.
  askFrame() {
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
    return { d: this.name, t: ASK, ws, r, i: this.writer };
  }

  onAsk(peer, f) {
    const theirs = new Context();
    let claimed = false;
    for (const [w, a, b] of f.ranges) {
      theirs.add(w, a, b);
      if (w === this.writer && b > this.n) claimed = true;
    }
    if (claimed) this.newIncarnation();
    const st = this.member(peer);
    // A new incarnation of theirs (the doc opened again) holds none of what we sent the old one.
    if (st.inc !== f.inc) {
      if (st.inc !== null && st.resets < MAX_RESETS) {
        st.sent.clear();
        st.resets++;
      }
      st.inc = f.inc;
    }
    if (st.answers < MAX_ANSWERS) {
      st.answers++;
      this.answer(peer, theirs, st);
    }
    // It has seen dots we haven't: ask it back.
    if (f.ranges.some(([w, a, b]) => !this.ctx.covers(w, a, b))) this.startAsk(peer);
  }

  // The entries `theirs` lacks that we haven't sent this member, and every dot we know is dead.
  answer(peer, theirs, st) {
    for (const id of st.sent) if (!this.entries.has(id)) st.sent.delete(id);
    const entries = [];
    for (const e of this.entries.values()) {
      if (theirs.has(e.w, e.n) || st.sent.has(e.id)) continue;
      entries.push(e);
      st.sent.add(e.id);
    }
    for (const f of this.pack(ANSWER, entries, this.dead(), true)) this.host.send(peer, f);
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
