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
// whole if anything is off; state lives in Maps; names, keys, values, entries, characters, writers and context
// ranges are capped, and so is what a member can have us send it to catch up: answers and asks at a rate
// (CATCHUP_BURST at once, then one per CATCHUP_EVERY_MS), and what answers carry, CATCHUP_CHARS at once, then
// CATCHUP_CHARS_PER_S, as an asker can always claim to lack the whole doc.
//
// Every member takes any stamp up to 2^53 - 1, as refusing one far ahead would split the room: no member is a
// reference for how far ahead is plausible, and a refused entry never comes again. So a member can move the
// stamps to 2^53 - 1 in one frame; from then on, values set at the same time, and set items and log entries,
// are ordered by writer, and a log keeps the highest writers' entries. A later set still wins, as it replaces
// what it has seen. A member could claim dots another member hasn't written yet, so that the room ignores its
// next writes: a doc that sees such a claim on its own dots moves to a new incarnation, at most
// MAX_INCARNATIONS times. Past that it writes after the dots it knows are claimed; a write on a dot claimed
// where it hasn't heard is lost at the members that took the claim, and once every dot is claimed, writes are
// refused with a warning until the doc is opened again.

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
// CATCHUP_CHARS (two full docs) that refills CATCHUP_CHARS_PER_S. An answer or ask past a budget waits for it
// rather than being dropped, so a doc closed and opened again (a component that remounts) catches up every
// time, and an asker can't have us send more, whatever it claims to lack.
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
  if (t === ASK) return { t, ranges };
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
// { self, peers() (joined members), send(peer, message), after(ms, fn) -> cancel, now() (ms), closed(doc),
// incarnation() }.
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
    this.touched = new Map(); // changes during one frame or write: collection -> ref -> { coll, ref, before }
    this.idle = new Set(); // writers the context holds with no live entry, longest idle first
    this.members = new Map(); // peer -> { answers, asks, chars, theirs, answerWait, askWait, stalled, askAgain }
    this.queue = []; // members to ask, in turn
    this.asking = null; // the member asked, until its answer ends or ASK_WAIT_MS passes
    this.cancelWait = null;
    this.closed = false;
    // While a frame is applied: a claim on our own dots, the span of them its entries took, a sender's write
    // we count as seen and don't hold, and the entries a catch-up dropped.
    this.claimed = false;
    this.claim = null; // the lowest and highest of our dots that a frame's entries took
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
    for (const st of this.members.values()) {
      st.answerWait?.();
      st.askWait?.();
    }
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
    const st = this.members.get(peer);
    st?.answerWait?.();
    st?.askWait?.();
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
    // Entries on our own dots, from the lowest to the highest: all above n, as [1, n] is in our context, so the
    // span claims nothing a member couldn't claim with one dead range, and it is sent on as one.
    if (this.claim) {
      const [a, b] = this.claim;
      this.claim = null;
      this.record(this.writer, a, b);
      this.dropped?.push([this.writer, a, b]);
    }
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
    // A member whose ask timed out is asked again once it has answered, if it was due an ask meanwhile.
    const st = this.members.get(peer);
    if (f.t === ANSWER && f.end && st?.stalled) {
      st.stalled = false;
      if (st.askAgain) {
        st.askAgain = false;
        this.startAsk(peer);
      }
    }
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
  // named and the entries held.
  kill(w, l) {
    if (w === this.writer && l[l.length - 1][1] > this.n) this.claimed = true;
    const held = this.byWriter.get(w);
    if (held) {
      let dots = 0;
      for (const [a, b] of l) dots += b - a + 1;
      if (dots < held.size) {
        for (const [a, b] of l) {
          for (let n = a; n <= b; n++) {
            const e = held.get(n);
            if (e && e.t !== 'l') this.drop(e);
          }
        }
      } else {
        for (const [n, e] of [...held]) {
          const i = seek(l, n);
          if (i < l.length && l[i][0] <= n && e.t !== 'l') this.drop(e);
        }
      }
    }
    for (const [a, b] of l) this.record(w, a, b);
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
    if (!this.byWriter.has(w)) this.idle.add(w);
    return true;
  }

  room(w, a, b) {
    const writers = this.ctx.writers.has(w) || this.ctx.writers.size < MAX_WRITERS;
    return writers && (this.ctx.size < MAX_RANGES || this.ctx.touches(w, a, b));
  }

  // A full context forgets what matters least: first the writers with no live entry (longest idle first, kept
  // in `idle` so finding them costs nothing), then the highest ranges of the writers with the most, most
  // first, each down to one range, until FORGET_RANGES are free: so the writers are sorted about once per that
  // many ranges recorded, not scanned for each. A forgotten dead dot can come back from a member that missed
  // its deletion; nothing else is lost.
  makeRoom(keep) {
    for (const w of this.idle) {
      if (this.ctx.writers.size < MAX_WRITERS && this.ctx.size < MAX_RANGES) return;
      if (w === keep || w === this.writer) continue;
      this.ctx.drop(w);
      this.idle.delete(w);
    }
    if (this.ctx.size < MAX_RANGES) return;
    const most = [...this.ctx.writers].filter(([, l]) => l.length > 1).sort((x, y) => y[1].length - x[1].length);
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

  // Emits 'change' for what a frame or write changed, now that all of it is in.
  flush(from, local) {
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

  // A member's catch-up: when each of its budgets (BUDGETS) is whole again, its latest ask (`theirs`) and the
  // wait for its answer, the wait for our ask of it, and whether our last ask timed out (`stalled`) and another
  // is due once it answers.
  member(peer) {
    let st = this.members.get(peer);
    if (!st) this.members.set(peer, (st = { answers: 0, asks: 0, chars: 0, theirs: null, answerWait: null, askWait: null, stalled: false, askAgain: false }));
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

  // Spends `n` units of it. An answer's characters may take it past empty: the next answer waits until it's back.
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

  // Queues an ask of `peer`. One that let an ask of ours time out isn't asked again until it answers, so a member
  // that never does holds the others' turns up once, not each time it prompts us.
  startAsk(peer) {
    const st = this.member(peer);
    if (this.asking === peer || this.queue.includes(peer)) return;
    if (st.stalled) return void (st.askAgain = true);
    const now = this.now();
    const wait = this.early(st, 'asks', now);
    if (wait > 0) return this.later(peer, st, 'askWait', wait, () => this.startAsk(peer));
    this.spend(st, 'asks', 1, now);
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
      this.cancelWait = this.host.after(ASK_WAIT_MS, () => {
        if (this.asking !== p) return;
        this.member(p).stalled = true;
        this.nextAsk(true);
      });
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
    return { d: this.name, t: ASK, ws, r };
  }

  onAsk(peer, f) {
    if (f.ranges.some(([w, , b]) => w === this.writer && b > this.n)) this.newIncarnation();
    const st = this.member(peer);
    // Answered now or, past the member's budgets, once they allow: only its latest ask waits.
    st.theirs = Context.of(f.ranges);
    this.answer(peer, st);
    // It has seen dots we haven't: ask it back.
    if (f.ranges.some(([w, a, b]) => !this.ctx.covers(w, a, b))) this.startAsk(peer);
  }

  // The entries the member's ask lacks, and every dot we know is dead, if its budgets allow. A wait may find
  // its ask answered already, when a later one came as the budget did.
  answer(peer, st) {
    if (!st.theirs) return;
    const now = this.now();
    const wait = Math.max(this.early(st, 'answers', now), this.early(st, 'chars', now));
    if (wait > 0) return this.later(peer, st, 'answerWait', wait, () => this.answer(peer, st));
    st.answerWait?.();
    st.answerWait = null;
    const theirs = st.theirs;
    st.theirs = null;
    const entries = [];
    let chars = 0;
    for (const e of this.entries.values()) {
      if (theirs.has(e.w, e.n)) continue;
      entries.push(e);
      chars += ENTRY_BYTES + e.len;
    }
    const dead = this.dead();
    this.spend(st, 'answers', 1, now);
    this.spend(st, 'chars', chars + RANGE_BYTES * dead.length, now);
    for (const f of this.pack(ANSWER, entries, dead, true)) this.host.send(peer, f);
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
