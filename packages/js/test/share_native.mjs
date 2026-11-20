// NB-159: Peerwire Share between the Share page (demo/share/, in headless Chromium) and a native peer, the Rust
// example examples/rust/share.rs on peerwire::files, both ways, byte for byte. Loopback only.
//
// A browser and a native node can't link yet (joinRoom doesn't dial a native peer, which has no WebRTC), so the page
// gets a stand-in room: its import of src/index.js is answered with a joinRoom whose one peer is this test's bridge,
// over a WebSocket, and the bridge passes each message on over TCP to the example (`share ... tcp://<addr>`) and
// back. Each goes as a room message, room.js's bytes tag (0) and the Share frame, as a link carries it, and the
// stand-in says the route is direct. Everything else runs as it is: share.js and files.js (offers, accept, pieces of
// 512 KiB and their acks, the window, the end and the receiver's verdict, the hash worker, the download through the
// service worker) on one side, peerwire::files and its FileSink on the other.
//
// - native -> browser: the example sends a file; the page accepts it and downloads it; its SHA-256 matches.
// - browser -> native: the page sends a file; the example keeps it; it is the same bytes.
// - names: a file with a bidi override, a path and a reserved name in its name, both ways: each side shows and keeps
//   it under the name files.js's cleanName gives.
// - offers that end before they are answered (NB159-3), with no page: an offer and its cancel in one batch, against
//   `recv --yes`, `recv` (answered after it ended) and `send` (which declines offers). The example carries on.
//
//   node packages/js/test/share_native.mjs                    8 MiB each way, the example built here (cargo, gate
//                                                             profile, target/share-native)
//   node packages/js/test/share_native.mjs --mb=1024 [--bin=path/to/share]   1024 MiB each way, timed

import { serve } from './serve.mjs';
import { createHash, randomBytes } from 'node:crypto';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as F from '../demo/share/files.js';

const { cleanName } = F;

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const arg = (name) => process.argv.find((a) => a.startsWith(`--${name}=`))?.split('=')[1] ?? null;
const MB = Number(arg('mb') ?? 8);
const repo = path.join(path.dirname(fileURLToPath(import.meta.url)), '../../..');
const root = path.join(repo, 'packages/js');
const WAIT = { timeout: 60000 };
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'share-native-'));
const NATIVE = 'bb'.repeat(32); // the bridge's peer id, as the page sees it (the example's Bridge::PEER)
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const errors = [];
async function shaFile(p) {
  const h = createHash('sha256');
  for await (const c of fs.createReadStream(p)) h.update(c);
  return h.digest('hex');
}
// A file of `mb` MiB of random bytes: its path and SHA-256.
function makeFile(name, bytes) {
  const file = path.join(tmp, 'src', name);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const h = createHash('sha256');
  const fd = fs.openSync(file, 'w');
  for (let left = bytes; left > 0;) {
    const chunk = randomBytes(Math.min(left, 16 << 20));
    h.update(chunk);
    fs.writeSync(fd, chunk);
    left -= chunk.length;
  }
  fs.closeSync(fd);
  return { file, sha: h.digest('hex'), size: bytes };
}

// ---------- the example ----------

function binary() {
  if (arg('bin')) return arg('bin');
  const dir = path.join(repo, 'target/share-native');
  // Its own target dir, so it never waits on the gate's Rust lane for cargo's lock.
  execFileSync('cargo', ['build', '--profile', 'gate', '--target-dir', dir, '-p', 'peerwire', '--features', 'files', '--example', 'share'],
    { cwd: repo, stdio: ['ignore', 'ignore', 'inherit'] });
  return path.join(dir, 'gate/examples/share');
}

// Runs the example (its stdin `ignore`, or `pipe` to answer its questions); `exit` resolves to its exit code, `out`
// is what it printed and `peak` its peak resident memory (KB).
function run(bin, args, stdin = 'ignore') {
  const p = spawn(bin, args, { stdio: [stdin, 'pipe', 'pipe'] });
  const r = { proc: p, out: '', peak: 0 };
  p.stdout.setEncoding('utf8').on('data', (d) => (r.out += d));
  p.stderr.setEncoding('utf8').on('data', (d) => (r.out += d));
  const timer = setInterval(() => {
    try {
      const m = fs.readFileSync(`/proc/${p.pid}/status`, 'utf8').match(/VmHWM:\s+(\d+)/);
      if (m) r.peak = Math.max(r.peak, Number(m[1]));
    } catch {}
  }, 500);
  r.exit = new Promise((resolve) => p.on('exit', (code) => (clearInterval(timer), resolve(code))));
  return r;
}

// ---------- the bridge: the page's WebSocket <-> the example's TCP ----------

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';

// One WebSocket client (the page) and one TCP client (the example), each message passed to the other: over TCP as a
// 4-byte big-endian length and the message. What comes before the other side is there waits.
async function bridge() {
  const b = { ws: null, tcp: null, toWs: [], toTcp: [], close() {} };
  const sendWs = (m) => {
    const n = m.length;
    const head = n < 126 ? Buffer.from([0x82, n]) : n < 65536 ? Buffer.from([0x82, 126, n >> 8, n & 255]) : Buffer.alloc(10);
    if (n >= 65536) {
      head[0] = 0x82;
      head[1] = 127;
      head.writeBigUInt64BE(BigInt(n), 2);
    }
    b.ws.write(Buffer.concat([head, m]));
  };
  const sendTcp = (m) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(m.length);
    b.tcp.write(Buffer.concat([len, m]));
  };
  const toWs = (m) => (b.ws ? sendWs(m) : b.toWs.push(m));
  const toTcp = (m) => (b.tcp ? sendTcp(m) : b.toTcp.push(m));

  const tcpServer = net.createServer((s) => {
    if (b.tcp) return void s.destroy();
    s.setNoDelay(true);
    b.tcp = s;
    for (const m of b.toTcp.splice(0)) sendTcp(m);
    let buf = Buffer.alloc(0);
    s.on('data', (d) => {
      buf = buf.length ? Buffer.concat([buf, d]) : d;
      while (buf.length >= 4 && buf.length >= 4 + buf.readUInt32BE(0)) {
        const n = buf.readUInt32BE(0);
        toWs(buf.subarray(4, 4 + n));
        buf = buf.subarray(4 + n);
      }
    });
    s.on('error', () => {});
    s.on('close', () => b.ws?.end());
  });
  const httpServer = http.createServer((req, res) => res.writeHead(426).end());
  httpServer.on('upgrade', (req, sock) => {
    if (b.ws) return void sock.destroy();
    const accept = createHash('sha1').update(req.headers['sec-websocket-key'] + GUID).digest('base64');
    sock.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    sock.setNoDelay(true);
    b.ws = sock;
    for (const m of b.toWs.splice(0)) sendWs(m);
    let buf = Buffer.alloc(0);
    let parts = [];
    sock.on('data', (d) => {
      buf = buf.length ? Buffer.concat([buf, d]) : d;
      for (;;) {
        if (buf.length < 2) return;
        const fin = buf[0] & 0x80;
        const op = buf[0] & 0x0f;
        let len = buf[1] & 0x7f;
        let at = 2;
        if (len === 126) {
          if (buf.length < 4) return;
          len = buf.readUInt16BE(2);
          at = 4;
        } else if (len === 127) {
          if (buf.length < 10) return;
          len = Number(buf.readBigUInt64BE(2));
          at = 10;
        }
        if (buf.length < at + 4 + len) return;
        const mask = buf.subarray(at, at + 4);
        const data = Buffer.from(buf.subarray(at + 4, at + 4 + len));
        for (let i = 0; i < data.length; i++) data[i] ^= mask[i & 3];
        buf = buf.subarray(at + 4 + len);
        if (op === 0x8) return void sock.end();
        if (op === 0x9) {
          sock.write(Buffer.concat([Buffer.from([0x8a, data.length]), data]));
          continue;
        }
        if (op !== 0x2 && op !== 0x0) continue;
        parts.push(data);
        if (fin) {
          toTcp(Buffer.concat(parts));
          parts = [];
        }
      }
    });
    sock.on('error', () => {});
    sock.on('close', () => b.tcp?.end());
  });
  await Promise.all([tcpServer, httpServer].map((s) => new Promise((r) => s.listen(0, '127.0.0.1', r))));
  b.tcpPort = tcpServer.address().port;
  b.wsUrl = `ws://127.0.0.1:${httpServer.address().port}`;
  b.close = () => {
    b.ws?.destroy();
    b.tcp?.destroy();
    tcpServer.close();
    httpServer.close();
  };
  return b;
}

// The page's src/index.js: joinRoom gives a room of one peer, the bridge, whose route is direct. Room messages as
// room.js frames them: the bytes tag (0), then the bytes. Messages that come before the page listens wait for it.
const standIn = (wsUrl) => `
const NATIVE = '${NATIVE}';
export const randomRoomId = () => 'stand-in';
export async function joinRoom() {
  const ws = new WebSocket('${wsUrl}');
  ws.binaryType = 'arraybuffer';
  await new Promise((ok, no) => { ws.onopen = ok; ws.onerror = () => no(new Error('no bridge')); });
  const on = new Map();
  const emit = (ev, ...a) => { for (const f of on.get(ev) ?? []) f(...a); };
  let early = [];
  const deliver = (b) => b[0] === 0 && emit('message', b.subarray(1), NATIVE, { kind: 'bytes', lane: 'reliable' });
  ws.onmessage = (e) => (early ? early.push(new Uint8Array(e.data)) : deliver(new Uint8Array(e.data)));
  const room = {
    self: 'cc'.repeat(32),
    peers: [],
    on(ev, f) { on.set(ev, [...(on.get(ev) ?? []), f]); },
    send(data, { to } = {}) {
      if (!(data instanceof Uint8Array) || (to && to !== NATIVE) || ws.readyState !== 1) return 0;
      const m = new Uint8Array(1 + data.length);
      m.set(data, 1);
      ws.send(m);
      return 1;
    },
    route: (p) => (room.peers.includes(p) ? { kind: 'direct', rttMs: 1 } : null),
    rtt: () => 1,
    retryDirect: () => 0,
    setPresence() {},
    async leave() { ws.close(); },
  };
  ws.onclose = () => { if (room.peers.length) { room.peers.length = 0; emit('peer-leave', NATIVE, 'left'); } };
  setTimeout(() => {
    room.peers.push(NATIVE);
    emit('peer-join', NATIVE, { name: 'native' });
    for (const b of early) deliver(b);
    early = null;
  }, 50);
  return room;
}
`;

async function open(ctx, name, wsUrl) {
  const p = await ctx.newPage();
  p.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  p.on('console', (m) => m.type() === 'error' && !/favicon/.test(m.text()) && errors.push(`${name}: console ${m.text()}`));
  await p.route('**/src/index.js', (r) => r.fulfill({ contentType: 'text/javascript', body: standIn(wsUrl) }));
  await p.goto(`http://127.0.0.1:${port}/demo/share/index.html?save=download#stand-in`);
  await p.waitForFunction(() => window.share?.room?.peers.length === 1 && window.share.saveMode === 'download', null, WAIT);
  return p;
}
const row = (p, dir, name) => p.locator(`#transfers li[data-dir="${dir}"]`).filter({ has: p.locator('.name', { hasText: name }) }).first();
async function waitState(p, dir, name, want, timeout) {
  const until = Date.now() + timeout;
  for (;;) {
    const s = await row(p, dir, name).getAttribute('data-state').catch(() => null);
    if (s === want || (s && !['offered', 'sending', 'receiving'].includes(s))) return s;
    if (Date.now() > until) throw new Error(`${name} (${dir}) is ${s}, not ${want}, after ${timeout} ms`);
    await p.waitForTimeout(200);
  }
}
const speed = (bytes, ms) => `${(bytes / 1e6 / (ms / 1000)).toFixed(1)} MB/s`;
const nativeSpeed = (out) => {
  const m = out.match(/done .* in ([\d.]+) s, ([\d.]+) MB\/s/);
  return m ? `${m[2]} MB/s in ${m[1]} s` : '?';
};

// native -> browser: the example sends `f`; the page accepts and downloads it.
async function toBrowser(ctx, bin, f, label) {
  const b = await bridge();
  const ex = run(bin, ['send', `tcp://127.0.0.1:${b.tcpPort}`, f.file]);
  try {
    const p = await open(ctx, 'page', b.wsUrl);
    const shown = cleanName(path.basename(f.file));
    await row(p, 'in', shown).waitFor(WAIT);
    const dl = p.waitForEvent('download', WAIT);
    const t0 = Date.now();
    await row(p, 'in', shown).locator('[data-act="accept"]').click();
    const d = await dl;
    const state = await waitState(p, 'in', shown, 'done', Math.max(60000, f.size / 2e3));
    const ms = Date.now() - t0;
    const code = await ex.exit;
    const got = await shaFile(await d.path());
    check(state === 'done' && code === 0 && got === f.sha && d.suggestedFilename() === shown,
      `native -> browser, ${label}: ${f.size} B as ${JSON.stringify(shown)}, the download's SHA-256 matches (${speed(f.size, ms)} from the accept; the example: ${nativeSpeed(ex.out)}, peak RSS ${(ex.peak / 1024).toFixed(0)} MB)`);
    if (state !== 'done' || code !== 0) console.log(ex.out);
    await d.delete();
    await p.close();
  } finally {
    ex.proc.kill();
    b.close();
  }
}

// browser -> native: the page sends `f`; the example keeps it in a directory of its own.
async function toNative(ctx, bin, f, label) {
  const b = await bridge();
  const dir = fs.mkdtempSync(path.join(tmp, 'recv-'));
  const ex = run(bin, ['recv', `tcp://127.0.0.1:${b.tcpPort}`, dir, '--yes', '--once']);
  try {
    const p = await open(ctx, 'page', b.wsUrl);
    const shown = cleanName(path.basename(f.file));
    const t0 = Date.now();
    await p.setInputFiles('#pick', f.file);
    await p.selectOption('#to', NATIVE);
    await p.click('#send');
    const state = await waitState(p, 'out', shown, 'done', Math.max(60000, f.size / 2e3));
    const ms = Date.now() - t0;
    const code = await ex.exit;
    const kept = fs.readdirSync(dir);
    const ok = kept.length === 1 && kept[0] === shown && (await shaFile(path.join(dir, kept[0]))) === f.sha;
    check(state === 'done' && code === 0 && ok,
      `browser -> native, ${label}: ${f.size} B kept as ${JSON.stringify(kept[0])}, the same bytes (${speed(f.size, ms)} from the send; the example: ${nativeSpeed(ex.out)}, peak RSS ${(ex.peak / 1024).toFixed(0)} MB)`);
    if (state !== 'done' || code !== 0 || !ok) console.log(ex.out, kept);
    await p.close();
  } finally {
    ex.proc.kill();
    b.close();
  }
}

// ---------- offers that end before they are answered (NB159-3) ----------

// The example's one peer over TCP, with no page: it writes Share control messages, those of one say() in one write
// (one read for the example), and keeps those the example sends.
async function fakePeer() {
  const p = { got: [], sock: null };
  const server = net.createServer((s) => {
    if (p.sock) return void s.destroy();
    p.sock = s;
    let buf = Buffer.alloc(0);
    s.on('data', (d) => {
      buf = Buffer.concat([buf, d]);
      while (buf.length >= 4 && buf.length >= 4 + buf.readUInt32BE(0)) {
        const m = buf.subarray(4, 4 + buf.readUInt32BE(0));
        buf = buf.subarray(4 + m.length);
        const c = m[0] === 0 ? F.decodeControl(m.subarray(1)) : null;
        if (c) p.got.push(c);
      }
    });
    s.on('error', () => {});
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  p.addr = `tcp://127.0.0.1:${server.address().port}`;
  // Each as a room message: its length (4 bytes, big-endian), the bytes tag (0), the control frame.
  p.say = (...ms) => p.sock.write(Buffer.concat(ms.flatMap((m) => {
    const f = F.encodeControl(m);
    const head = Buffer.alloc(5);
    head.writeUInt32BE(1 + f.length);
    return [head, f];
  })));
  p.close = () => {
    p.sock?.destroy();
    server.close();
  };
  return p;
}
// Waits for `ok()` to hold while the example runs: throws with what it printed if it exits or the wait times out.
async function until(ex, ok, what) {
  for (const end = Date.now() + WAIT.timeout; !ok();) {
    if (ex.proc.exitCode !== null || Date.now() > end) {
      throw new Error(`no ${what}: the example ${ex.proc.exitCode === null ? 'runs' : `exited with ${ex.proc.exitCode}`}:\n${ex.out}`);
    }
    await new Promise((r) => setTimeout(r, 20));
  }
}
const EMPTY_SHA = createHash('sha256').digest('hex');
const offerOf = (name) => F.makeOffer({ id: F.newId(), name, size: 0, type: '' });

// An offer and its cancel in one batch, so the offer has ended before the example answers it: `recv --yes` takes it,
// `recv` asks (and is answered "y" after it ended), `send` declines it. Each carries on: the next offer is kept, or
// its own file delivered.
async function endedOffers(bin) {
  for (const mode of ['recv --yes', 'recv', 'send']) {
    const p = await fakePeer();
    const dir = fs.mkdtempSync(path.join(tmp, 'ended-'));
    const sent = mode === 'send' ? makeFile('sent.bin', 1000) : null;
    const args = mode === 'send' ? ['send', p.addr, sent.file] : ['recv', p.addr, dir, ...(mode === 'recv' ? [] : ['--yes'])];
    const ex = run(bin, args, mode === 'recv' ? 'pipe' : 'ignore');
    try {
      await until(ex, () => p.sock, 'connection');
      const gone = offerOf('gone.txt');
      p.say(gone, { t: 'cancel', id: gone.id });
      await until(ex, () => ex.out.includes('"gone.txt" ended: the peer cancelled'), 'end of the cancelled offer');
      let ok;
      if (mode === 'send') {
        await until(ex, () => p.got.some((m) => m.t === 'offer'), 'offer');
        const mine = p.got.find((m) => m.t === 'offer');
        p.say({ t: 'accept', id: mine.id, have: 0 });
        await until(ex, () => p.got.some((m) => m.t === 'end' && m.id === mine.id), 'end of its file');
        const end = p.got.find((m) => m.t === 'end' && m.id === mine.id);
        p.say({ t: 'done', id: mine.id, ok: true, sha256: end.sha256 });
        const code = await ex.exit;
        ok = code === 0 && end.sha256 === sent.sha;
      } else {
        if (mode === 'recv') {
          ex.proc.stdin.write('y\n'); // too late for gone.txt: it answers nothing
          await new Promise((r) => setTimeout(r, 300));
        }
        const kept = offerOf('kept.txt');
        p.say(kept);
        if (mode === 'recv') {
          await until(ex, () => ex.out.includes('accept "kept.txt"? [y/N]'), 'question');
          ex.proc.stdin.write('y\n');
        }
        await until(ex, () => p.got.some((m) => m.t === 'accept' && m.id === kept.id), 'accept');
        p.say({ t: 'end', id: kept.id, sha256: EMPTY_SHA });
        await until(ex, () => p.got.some((m) => m.t === 'done' && m.id === kept.id), 'done');
        const done = p.got.find((m) => m.t === 'done' && m.id === kept.id);
        ok = done.ok === true && ex.proc.exitCode === null && fs.readdirSync(dir).join() === 'kept.txt';
      }
      check(ok, `share ${mode}: an offer cancelled before it was answered, then ${mode === 'send' ? 'its own file delivered (exit 0)' : 'the next offer kept'}`);
      if (!ok) console.log(ex.out, p.got);
    } catch (e) {
      check(false, `share ${mode}: an offer cancelled before it was answered: ${e.message}`);
    } finally {
      ex.proc.kill();
      p.close();
    }
  }
}

let browser;
let server;
let port;
try {
  const bin = binary();
  await endedOffers(bin);
  ({ port, server } = await serve(root));
  const big = makeFile(`big-${MB}MiB.bin`, MB << 20);
  // A name with a bidi override, a path and a reserved device name: cleaned the same on both sides.
  const odd = makeFile('con.‮gpj.exe:x', 700 * 1024 + 3);
  console.log(`     ${big.file}: ${big.size} B, SHA-256 ${big.sha}`);
  browser = await chromium.launch();
  const ctx = await browser.newContext({ acceptDownloads: true });
  await toBrowser(ctx, bin, odd, 'an odd name');
  await toNative(ctx, bin, odd, 'an odd name');
  await toBrowser(ctx, bin, big, `${MB} MiB`);
  await toNative(ctx, bin, big, `${MB} MiB`);
} catch (e) {
  check(false, `threw: ${e.stack ?? e}`);
} finally {
  await browser?.close();
  server?.kill();
  fs.rmSync(tmp, { recursive: true, force: true });
}
if (errors.length) check(false, `page errors:\n     ${errors.join('\n     ')}`);
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
