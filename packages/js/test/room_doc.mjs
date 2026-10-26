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
