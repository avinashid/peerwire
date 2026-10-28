// room.doc() in real rooms: three members in one process ('local' signalling, datagrams over
// BroadcastChannel, the Peerwire core in wasm) write to one shared document and converge. A fourth joins
// late and catches up; a member that opens the doc after the others wrote catches up when it opens; doc
// frames never show as messages, to 'message' handlers or poll(); a member's writes outlive it; leaving
// closes the room's docs.
//
//   node packages/js/test/room_doc.mjs
import { joinRoom } from '../src/index.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
const until = async (what, cond, ms = 15000) => {
  const t = Date.now();
  while (!cond()) {
    if (Date.now() - t > ms) fail(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 10));
  }
};
const opts = { appId: 'peerwire-test', roomId: `doc-${process.pid}` };
// What a member's doc shows: every collection's entries, in order.
const view = (doc) => JSON.stringify([doc.map('meta').entries(), doc.set('strokes').entries(), doc.log('chat', { keep: 20 }).entries()]);

const rooms = await Promise.all(['ana', 'bo', 'cy'].map((name, i) => joinRoom({ ...opts, presence: { name }, queue: i === 2 })));
const [a, b, c] = rooms;
const messages = [];
for (const r of rooms) r.on('message', (data, from) => messages.push([r.self, from, data]));
await until('a full mesh', () => rooms.every((r) => r.peers.length === 2));

// a and b open the doc and write; c's room hasn't opened it, so their frames are dropped there.
const [da, db] = [a, b].map((r) => r.doc('board'));
if (a.doc('board') !== da) fail('room.doc(name) twice gave two docs');
const changes = [];
db.on('change', (ev) => changes.push(ev));
da.map('meta').set('title', 'Plans');
const s1 = da.set('strokes').add({ points: [0, 0, 10, 10], color: 'red' });
db.set('strokes').add({ points: [5, 5, 6, 6], color: 'blue' });
for (let i = 0; i < 30; i++) da.log('chat', { keep: 20 }).append({ text: `m${i}` });
await until('a and b to converge', () => view(da) === view(db) && db.set('strokes').entries().length === 2 && db.log('chat', { keep: 20 }).entries().length === 20);
// A reply made after seeing the others sorts after them on every member.
db.log('chat', { keep: 20 }).append({ text: 'reply' });
await until('the reply', () => view(da) === view(db) && da.log('chat', { keep: 20 }).entries().at(-1).value.text === 'reply');
const chat = db.log('chat', { keep: 20 }).entries();
if (chat.length !== 20 || chat[0].value.text !== 'm11' || chat.at(-2).value.text !== 'm29') fail(`log kept ${chat[0].value.text}..${chat.at(-1).value.text}`);
const title = changes.find((e) => e.key === 'title');
if (!title || title.from !== a.self || title.local || title.value !== 'Plans' || title.type !== 'map' || title.name !== 'meta') fail(`change event ${JSON.stringify(title)}`);
// A doc asks members in the order of their routes: the room tells it each one's.
if (da.host.route(b.self) !== a.route(b.self)?.kind || da.host.route(b.self) !== 'direct' || da.host.route('ff'.repeat(32)) !== null) fail(`a doc's routes: ${da.host.route(b.self)}`);
console.log('a and b converged: a map key, two set items, a log trimmed to its newest 20');

// c opens the doc after the others wrote: it asks them and catches up.
const dc = c.doc('board');
await until('c to catch up', () => view(dc) === view(da));
dc.set('strokes').delete(s1);
dc.map('meta').set('title', 'Plans, v2');
await until('c\'s writes to reach a and b', () => view(da) === view(dc) && view(db) === view(dc) && !da.set('strokes').has(s1));
console.log('c opened the doc late, caught up, and its writes reached a and b');

// Doc frames never show as messages.
const pulled = c.poll().filter((e) => e.type === 'message');
if (messages.length || pulled.length) fail(`doc frames showed as messages: ${JSON.stringify([...messages, ...pulled]).slice(0, 200)}`);
a.send('hello');
await until('a real message', () => messages.length === 2);
console.log("doc frames never showed as messages, to 'message' handlers or poll()");

// A late joiner catches up; a member's writes outlive it.
const d = await joinRoom({ ...opts, presence: { name: 'di' } });
const dd = d.doc('board');
await until('d to join', () => d.peers.length === 3);
await until('d to catch up', () => view(dd) === view(da));
const t0 = performance.now();
await a.leave();
dd.log('chat', { keep: 20 }).append({ text: 'after a left' });
await until('the others to hold a\'s writes and d\'s new one', () => [db, dc].every((x) => view(x) === view(dd)) && dd.map('meta').get('title') === 'Plans, v2');
if (dd.log('chat', { keep: 20 }).entries().at(-1).value.text !== 'after a left') fail('a write after a left');
console.log(`d joined late and caught up; a left and its writes stayed (${(performance.now() - t0).toFixed(0)} ms)`);

// A whiteboard-sized doc (40 strokes of 1000 points) reaches a late joiner deflated, about 2.5 times smaller
// than its JSON, and the joiner says it has caught up; so do live writes, but one that deflates past 16 times
// goes plain, and a plain one waits for the deflated one before it. Members that don't have the doc open answer
// its asks with nothing.
{
  let seed = 9;
  const r = () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const stroke = () => {
    let [x, y, h] = [200 + r() * 1600, 200 + r() * 900, r() * 6.28];
    const p = [];
    for (let i = 0; i < 1000; i++) {
      h += (r() - 0.5) * 0.6;
      const s = 2 + r() * 6;
      x += Math.cos(h) * s;
      y += Math.sin(h) * s;
      p.push(Math.round(x * 10) / 10, Math.round(y * 10) / 10);
    }
    return { k: 'pen', c: '#e5484d', w: 4, p };
  };
  // b's frames to d, by tag, and those d has taken (and, with the doc not open there, dropped).
  let sent = {};
  let frames = 0;
  let taken = 0;
  const write = b.write.bind(b);
  b.write = (peer, f, lane) => {
    if (peer === d.self && f[0] >= 7) {
      sent[f[0]] = (sent[f[0]] ?? 0) + f.length;
      frames++;
    }
    return write(peer, f, lane);
  };
  const docFrame = d.docFrame.bind(d);
  d.docFrame = (peer, text) => (peer === b.self && taken++, docFrame(peer, text));
  // b writes the board while c and d don't have it open: they answer b's ask with nothing, so b has caught up.
  const wb = b.doc('wb');
  for (let i = 0; i < 40; i++) wb.set('strokes').add(stroke());
  await until("b's writes to reach d, which drops them", () => frames && taken === frames && wb.synced);
  sent = {};
  const wd = d.doc('wb');
  await until('d to catch up the whiteboard', () => wd.synced && wd.set('strokes').entries().length === 40);
  const json = wd.set('strokes').entries().reduce((s, e) => s + JSON.stringify(e.value).length, 0);
  if (wd.sync.state !== 'synced' || !(sent[8] < json / 2)) fail(`the whiteboard: ${JSON.stringify(sent)} bytes by tag for ${json} characters of strokes, ${JSON.stringify(wd.sync)}`);
  const zipped = sent[8];
  // A write that deflates about 900 times goes plain; a stroke, deflated.
  sent = {};
  wb.map('meta').set('pad', 'x'.repeat(60000));
  await until('the pad', () => wd.map('meta').get('pad')?.length === 60000);
  const pad = { ...sent };
  // A stroke (deflated, which takes a moment) and then a key (plain) reach d in that order.
  const order = [];
  d.docFrame = (peer, text) => {
    const m = JSON.parse(new TextDecoder().decode(text));
    if (m.d === 'wb' && m.t === 1) order.push(m.e[0][3]);
    return docFrame(peer, text);
  };
  wb.set('strokes').add(stroke());
  wb.map('meta').set('k', 1);
  await until('a stroke and a key more', () => wd.set('strokes').entries().length === 41 && wd.map('meta').get('k') === 1);
  b.write = write;
  d.docFrame = docFrame;
  if (!(pad[7] > 60000) || pad[8] || !(sent[8] > 0) || order.join() !== 's,m') fail(`a pad went as ${JSON.stringify(pad)}, then a stroke as ${JSON.stringify(sent)}; d took ${order}`);
  console.log(`a whiteboard of ${json} characters of strokes reached a late joiner in ${zipped} bytes deflated (${(json / zipped).toFixed(2)}x); a write that deflates 900 times went plain`);
}

// Deflated frames from a member are taken in order with the plain ones, and only to 16 times their size and the
// lane's 1 MiB: one that inflates further, or isn't raw deflate, or whose member goes while it is inflated, is
// dropped without a throw.
{
  const { deflateRawSync } = await import('node:zlib');
  const db = b.doc('z');
  const got = [];
  const receive = db.receive.bind(db);
  db.receive = (peer, m) => (m.o && got.push(m.o), receive(peer, m)); // not the members' answers to b's ask
  const w = d.self + '0000000a';
  const answer = (o, n, v) => ({ d: 'z', t: 3, ws: [w], e: [[0, n, n, 's', 's', null, v]], o });
  const tagged = (tag, bytes) => {
    const f = new Uint8Array(bytes.length + 1);
    f[0] = tag;
    f.set(bytes, 1);
    return f;
  };
  const z = (m) => tagged(8, deflateRawSync(Buffer.from(JSON.stringify(m))));
  const plain = (m) => tagged(7, Buffer.from(JSON.stringify(m)));
  const noisy = Array.from({ length: 2000 }, (_, i) => (i * 7919) % 1000); // deflates about 3 times
  // Past the lane's 1 MiB inflated, though within 16 times: 20 entries of 54 K characters, deflating 2.4 times.
  let x = 7;
  const next = () => ((x = (x * 1103515245 + 12345) % 2 ** 31) >> 16) % 1000;
  const many = { d: 'z', t: 3, ws: [w], e: Array.from({ length: 20 }, (_, i) => [0, 10 + i, 1, 's', 's', null, Array.from({ length: 14000 }, next)]), o: 6 };
  if (JSON.stringify(many).length < 2 ** 20 || deflateRawSync(Buffer.from(JSON.stringify(many))).length * 16 < 2 ** 20) fail('the frame past 1 MiB is not that frame');
  const frames = [
    z(answer(1, 1, noisy)), plain(answer(2, 2, 'small')), z(answer(3, 3, 'x'.repeat(20000))), // inflates 300 times
    Uint8Array.of(8, 1, 2, 3, 4, 5), z(many), z(answer(4, 4, noisy)), plain(answer(5, 5, 'last')),
  ];
  for (const f of frames) b.onMessage(d.self, f, 0);
  await until('the frames from d', () => got.length === 4);
  if (got.join() !== '1,2,4,5' || db.set('s').entries().length !== 4) fail(`frames taken ${got}, ${db.set('s').entries().length} items`);
  // One still being inflated when its member leaves is dropped.
  b.onMessage(d.self, z(answer(8, 8, noisy)), 0);
  const presence = b.members.get(d.self);
  b.members.delete(d.self);
  await new Promise((r) => setTimeout(r, 100));
  b.members.set(d.self, presence);
  if (got.includes(8)) fail('a frame inflated after its member left was taken');
  console.log('deflated frames: taken in order with plain ones; one inflating past 16 times its size or 1 MiB, or not deflate, or from a member gone meanwhile, dropped');
}

// Leaving closes the room's docs.
let threw = false;
try {
  da.map('meta').set('x', 1);
} catch {
  threw = true;
}
if (!threw || da.map('meta').entries().length) fail('a doc stayed open after its room left');
threw = false;
try {
  a.doc('board');
} catch {
  threw = true;
}
if (!threw) fail('room.doc() after leaving');
// A doc closed and opened again starts afresh and catches up again.
dd.close();
const dd2 = d.doc('board');
if (dd2 === dd) fail('a closed doc came back');
await until('a doc opened again to catch up', () => view(dd2) === view(db));
for (const r of [b, c, d]) await r.leave();
console.log('room_doc: 3 members converge, late joiners catch up, frames stay out of messages, leave closes docs: ok');
process.exit(0);
