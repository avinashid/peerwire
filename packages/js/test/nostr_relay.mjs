// A Nostr relay on loopback, for tests (NB-126): NIP-01 over a WebSocket server of its own (node:http and the frames
// of RFC 6455, as Node has no WebSocket server), forwarding each event to the live subscriptions whose filter takes it
// (kinds, #p) and answering it OK. It keeps nothing: Peerwire's signalling events are ephemeral. Signatures aren't
// checked. `events` holds every event published, `drop(ev)` (settable) loses an event when it says so.
//
//   import { startRelay } from './nostr_relay.mjs';
//   const relay = await startRelay(); // relay.url: ws://127.0.0.1:<port>
//   ...
//   relay.close();
//
// On its own, for a run against a Rust node (examples/rust/nostr_relay.rs is the same in Rust):
//
//   node packages/js/test/nostr_relay.mjs [port]
import http from 'node:http';
import crypto from 'node:crypto';
import { pathToFileURL } from 'node:url';

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';
const MAX_FRAME = 1024 * 1024;

export function startRelay({ port = 0, host = '127.0.0.1' } = {}) {
  const server = http.createServer((req, res) => {
    res.writeHead(426, { 'content-type': 'text/plain' });
    res.end('a Nostr relay: connect with a WebSocket\n');
  });
  const relay = { url: null, events: [], clients: new Set(), drop: () => false, close: () => close(relay, server) };
  server.on('upgrade', (req, sock) => {
    const key = req.headers['sec-websocket-key'];
    if (typeof key !== 'string' || req.headers.upgrade?.toLowerCase() !== 'websocket') return void sock.destroy();
    const accept = crypto.createHash('sha1').update(key + GUID).digest('base64');
    sock.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n' +
      `Sec-WebSocket-Accept: ${accept}\r\n\r\n`);
    sock.setNoDelay(true);
    const c = { sock, subs: new Map(), buf: Buffer.alloc(0), parts: [] };
    relay.clients.add(c);
    sock.on('data', (d) => {
      c.buf = Buffer.concat([c.buf, d]);
      frames(relay, c);
    });
    sock.on('close', () => relay.clients.delete(c));
    sock.on('error', () => relay.clients.delete(c));
  });
  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, host, () => {
      relay.url = `ws://${host}:${server.address().port}`;
      resolve(relay);
    });
  });
}

function close(relay, server) {
  for (const c of relay.clients) c.sock.destroy();
  relay.clients.clear();
  server.close();
}

// Reads the whole frames in `c.buf`: a text message once its last part is in, a ping answered, a close returned.
function frames(relay, c) {
  for (;;) {
    const b = c.buf;
    if (b.length < 2) return;
    const fin = (b[0] & 0x80) !== 0;
    const op = b[0] & 0x0f;
    const masked = (b[1] & 0x80) !== 0;
    let len = b[1] & 0x7f;
    let i = 2;
    if (len === 126) {
      if (b.length < 4) return;
      len = b.readUInt16BE(2);
      i = 4;
    } else if (len === 127) {
      if (b.length < 10) return;
      len = Number(b.readBigUInt64BE(2));
      i = 10;
    }
    if (len > MAX_FRAME) return void c.sock.destroy();
    const mask = masked ? b.subarray(i, i + 4) : null;
    if (masked) i += 4;
    if (b.length < i + len) return;
    const data = Buffer.from(b.subarray(i, i + len));
    if (mask) for (let k = 0; k < data.length; k++) data[k] ^= mask[k & 3];
    c.buf = b.subarray(i + len);
    if (op === 0x8) {
      write(c, data.subarray(0, 2), 0x8);
      return void c.sock.end();
    }
    if (op === 0x9) write(c, data, 0xa);
    if (op > 0x2) continue;
    c.parts.push(data);
    if (!fin) continue;
    const text = Buffer.concat(c.parts).toString('utf8');
    c.parts = [];
    message(relay, c, text);
  }
}

function write(c, data, op = 0x1) {
  if (c.sock.destroyed) return;
  const n = data.length;
  let head;
  if (n < 126) head = Buffer.from([0x80 | op, n]);
  else if (n < 65536) head = Buffer.from([0x80 | op, 126, n >> 8, n & 0xff]);
  else {
    head = Buffer.alloc(10);
    head[0] = 0x80 | op;
    head[1] = 127;
    head.writeBigUInt64BE(BigInt(n), 2);
  }
  c.sock.write(Buffer.concat([head, data]));
}

const send = (c, msg) => write(c, Buffer.from(JSON.stringify(msg)));

function message(relay, c, text) {
  let m;
  try {
    m = JSON.parse(text);
  } catch {
    return;
  }
  if (!Array.isArray(m)) return;
  if (m[0] === 'REQ' && typeof m[1] === 'string') {
    c.subs.set(m[1], m.slice(2).filter((f) => f && typeof f === 'object'));
    send(c, ['EOSE', m[1]]);
  } else if (m[0] === 'CLOSE') {
    c.subs.delete(m[1]);
  } else if (m[0] === 'EVENT' && m[1] && typeof m[1].id === 'string') {
    const ev = m[1];
    relay.events.push(ev);
    send(c, ['OK', ev.id, true, '']);
    if (relay.drop(ev)) return;
    const tags = Array.isArray(ev.tags) ? ev.tags.filter((t) => Array.isArray(t) && t[0] === 'p').map((t) => t[1]) : [];
    const takes = (f) => (!f.kinds || f.kinds.includes(ev.kind)) && (!f['#p'] || f['#p'].some((p) => tags.includes(p)));
    for (const o of relay.clients) {
      for (const [sub, filters] of o.subs) if (filters.some(takes)) send(o, ['EVENT', sub, ev]);
    }
  }
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  const relay = await startRelay({ port: Number(process.argv[2] ?? 0) });
  console.log(`relay on ${relay.url}`);
}
