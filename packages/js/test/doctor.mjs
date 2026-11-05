// NB-106: `npx @peerwire/js doctor` (bin/doctor.js) against local fixtures only: STUN servers on loopback that map as
// a NAT would, an RFC 5780 server on 127.0.0.1 and 127.0.0.2, a minimal ws:// Nostr relay and a refused port; then
// the command itself, as a child process, with --json and --redact.
//
//   node packages/js/test/doctor.mjs
import dgram from 'node:dgram';
import http from 'node:http';
import { createHash, randomBytes } from 'node:crypto';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { doctor, render, verdict, mappedAddress, show } from '../bin/doctor.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
const check = (cond, m) => cond || fail(m);
const MAGIC = [0x21, 0x12, 0xa4, 0x42];
const PUBLIC = '198.51.100.7'; // TEST-NET-2: public as far as the doctor can tell

// A Binding success response telling the requester it was seen at ip:port (IPv4, XOR-MAPPED-ADDRESS).
function bindingResponse(tx, ip, port) {
  const b = Buffer.alloc(32);
  b.writeUInt16BE(0x0101, 0);
  b.writeUInt16BE(12, 2);
  b.set(MAGIC, 4);
  tx.copy(b, 8);
  b.writeUInt16BE(0x0020, 20);
  b.writeUInt16BE(8, 22);
  b[25] = 1;
  b.writeUInt16BE(port ^ 0x2112, 26);
  ip.split('.').forEach((o, i) => (b[28 + i] = Number(o) ^ MAGIC[i]));
  return b;
}
const bindingRequest = (b) => b.length >= 20 && b.readUInt16BE(0) === 0x0001 && b.readUInt32BE(4) === 0x2112a442;

const sockets = [];
function udp(host = '127.0.0.1') {
  return new Promise((resolve, reject) => {
    const s = dgram.createSocket('udp4');
    s.once('error', reject);
    s.bind(0, host, () => (sockets.push(s), resolve(s)));
  });
}

// A STUN server on loopback telling each requester it was seen at map(from), from its own address whatever the
// request asks (as Google's and Cloudflare's do).
async function stunServer(map) {
  const s = await udp();
  s.on('message', (b, from) => {
    if (!bindingRequest(b)) return;
    const [ip, port] = map(from);
    s.send(bindingResponse(b.subarray(8, 20), ip, port), from.port, from.address);
  });
  return `127.0.0.1:${s.address().port}`;
}
const cone = (from) => [PUBLIC, from.port];

// An RFC 5780 server on 127.0.0.1, its other port there and its other IP 127.0.0.2; null where that can't be bound.
// `mode` as in crates/peerwire/tests/doctor.rs: full, same-ip-only, no-changes, late (test II's answers come a second
// late, during test III), ignores, refuses.
async function stun5780(mode) {
  let otherIp;
  try {
    otherIp = await udp('127.0.0.2');
  } catch {
    return null;
  }
  const [main, otherPort] = [await udp(), await udp()];
  main.on('message', (b, from) => {
    if (!bindingRequest(b)) return;
    const change = b.length >= 28 && b.readUInt16BE(20) === 0x0003 ? b[27] & 0x06 : 0;
    const ok = bindingResponse(b.subarray(8, 20), from.address, from.port);
    const error = Buffer.from(ok.subarray(0, 20));
    error.writeUInt32BE(0x01110000, 0);
    let via = null;
    if (!change || mode === 'ignores') via = [main, ok];
    else if (mode === 'refuses') via = [main, error];
    else if (change === 0x06 && mode === 'full') via = [otherIp, ok];
    else if (change === 0x06 && mode === 'late') return void setTimeout(() => otherIp.send(ok, from.port, from.address), 1000);
    else if (change === 0x02 && (mode === 'full' || mode === 'same-ip-only')) via = [otherPort, ok];
    via?.[0].send(via[1], from.port, from.address);
  });
  return `127.0.0.1:${main.address().port}`;
}

// A Nostr relay on loopback, plain ws://: forwards each event to the live subscriptions whose #p filter names its
// p tag and answers OK, storing nothing and checking no signatures (as examples/rust/nostr_relay.rs).
function relay() {
  const subs = new Map(); // socket -> Map(subscription id -> tags)
  const send = (socket, msg) => {
    const p = Buffer.from(JSON.stringify(msg));
    const head = p.length < 126 ? Buffer.from([0x81, p.length]) : Buffer.from([0x81, 126, p.length >> 8, p.length & 0xff]);
    socket.write(Buffer.concat([head, p]));
  };
  const take = (socket, text) => {
    let m;
    try {
      m = JSON.parse(text);
    } catch {
      return;
    }
    if (m[0] === 'REQ') subs.get(socket)?.set(m[1], m[2]?.['#p'] ?? []);
    if (m[0] === 'CLOSE') subs.get(socket)?.delete(m[1]);
    if (m[0] !== 'EVENT') return;
    const tag = m[1].tags?.find((t) => t[0] === 'p')?.[1];
    send(socket, ['OK', m[1].id, true, '']);
    for (const [s, list] of subs) for (const [id, tags] of list) if (tags.includes(tag)) send(s, ['EVENT', id, m[1]]);
  };
  const server = http.createServer();
  server.on('upgrade', (req, socket) => {
    const accept = createHash('sha1').update(`${req.headers['sec-websocket-key']}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    subs.set(socket, new Map());
    let buf = Buffer.alloc(0);
    socket.on('data', (d) => {
      buf = Buffer.concat([buf, d]);
      while (buf.length >= 2) {
        let len = buf[1] & 0x7f;
        let at = 2;
        if (len === 126) [len, at] = [buf.length >= 4 ? buf.readUInt16BE(2) : Infinity, 4];
        if (len === 127) [len, at] = [buf.length >= 10 ? Number(buf.readBigUInt64BE(2)) : Infinity, 10];
        const masked = (buf[1] & 0x80) !== 0;
        if (buf.length < at + (masked ? 4 : 0) + len) return;
        const mask = masked ? buf.subarray(at, at + 4) : null;
        at += masked ? 4 : 0;
        const data = Buffer.from(buf.subarray(at, at + len));
        if (mask) for (let i = 0; i < data.length; i++) data[i] ^= mask[i & 3];
        const op = buf[0] & 0x0f;
        buf = buf.subarray(at + len);
        if (op === 8) return void socket.end();
        if (op === 1) take(socket, data.toString());
      }
    });
    socket.on('close', () => subs.delete(socket));
    socket.on('error', () => {});
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => {
    resolve({ url: `ws://127.0.0.1:${server.address().port}`, close: () => server.close() });
  }));
}

// The verdicts, as the Rust doctor's tests have them.
const a = (s) => {
  const i = s.lastIndexOf(':');
  return { ip: s.slice(0, i).replace(/^\[|\]$/g, ''), port: Number(s.slice(i + 1)) };
};
const [G1, C1, G2, C2] = ['74.125.250.129:19302', '162.159.207.0:3478', '74.125.250.129:3478', '162.159.207.0:53'].map(a);
const LOCAL = a('192.168.1.5:40000');
const kind = (local, pairs) => verdict(local, pairs.map(([d, m]) => [d, a(m)]));
check(kind(LOCAL, [[G1, '203.145.57.120:1369'], [C1, '203.145.57.120:19000']])[0] === 'dependent', 'two IPs, two ports');
let v = kind(LOCAL, [[G1, '203.0.113.9:1369'], [C1, '203.0.113.9:19000'], [G2, '203.0.113.9:40211'], [C2, '203.0.113.9:7777']]);
check(v[0] === 'address-and-port-dependent' && v[1].includes('random ports, deltas [17631, 21211, -32434]'), v);
v = kind(LOCAL, [[G1, '203.0.113.9:5000'], [C1, '203.0.113.9:5001'], [G2, '203.0.113.9:5000'], [C2, '203.0.113.9:5001']]);
check(v[0] === 'address-dependent' && v[1].includes('1 delta [1], too few'), v);
v = kind(a('10.129.132.88:60241'), [[G1, '152.58.117.228:60241'], [C1, '152.58.117.228:60241'], [C2, '152.58.117.166:60241']]);
check(v[0] === 'pooled' && v[1].startsWith('port kept for every destination (preserved)') && v[1].includes('2 addresses'), v);
v = kind(LOCAL, [[G1, '203.0.113.9:40000'], [C1, '203.0.113.9:40000']]);
check(v[0] === 'endpoint-independent' && v[1].endsWith('port preserved'), v);
check(kind(a('[2001:4860::5]:40000'), [[G1, '[2001:4860::5]:40000'], [C1, '[2001:4860::5]:40000']])[0] === 'no-nat', 'no NAT');
check(kind(LOCAL, [])[0] === 'blocked' && kind(LOCAL, [[G1, '203.0.113.9:1']])[0] === 'unknown', 'blocked, unknown');

// XOR-MAPPED-ADDRESS over IPv6, and MAPPED-ADDRESS when there is no XOR one.
const tx = randomBytes(12);
const key = Buffer.concat([Buffer.from(MAGIC), tx]);
const v6 = Buffer.alloc(44);
v6.writeUInt16BE(0x0101, 0);
v6.writeUInt16BE(24, 2);
key.copy(v6, 4, 0, 4);
tx.copy(v6, 8);
v6.writeUInt16BE(0x0020, 20);
v6.writeUInt16BE(20, 22);
v6[25] = 2;
v6.writeUInt16BE(5000 ^ 0x2112, 26);
Buffer.from([0x24, 0x09, 0x40, 0x60, ...Array(11).fill(0), 1]).forEach((x, i) => (v6[28 + i] = x ^ key[i]));
let m = mappedAddress(v6, tx);
check(m?.ip === '2409:4060::1' && m.port === 5000, `IPv6 XOR-MAPPED-ADDRESS: ${JSON.stringify(m)}`);
check(mappedAddress(v6, randomBytes(12)) === null, 'another transaction');
const plain = Buffer.from(bindingResponse(tx, '0.0.0.0', 0));
plain.writeUInt16BE(0x0001, 20);
plain.set([0x12, 0x34, 203, 0, 113, 9], 26);
m = mappedAddress(plain, tx);
check(m?.ip === '203.0.113.9' && m.port === 0x1234, `MAPPED-ADDRESS: ${JSON.stringify(m)}`);

// --redact masks public addresses only.
const masked = (s) => show({ redact: true }, a(s));
check(masked('203.0.113.9:40000') === '203.0.x.x:40000' && masked('[2409:4060::1]:5') === '[2409:4060:x:x:x:x:x:x]:5', 'masks');
for (const s of ['192.168.1.5:1', '10.0.0.1:2', '100.72.0.1:3', '172.20.0.1:4', '127.0.0.1:5', '[fd00::1]:6', '[fe80::1]:7']) {
  check(masked(s) === s, `${s} is not public`);
}
check(show({ redact: false }, a('203.0.113.9:1')) === '203.0.113.9:1', 'not redacted');

// A whole report from local fixtures.
const r1 = await relay();
const base = { stun: [], relays: [], stunWaitMs: 300, waitMs: 3000 };
const steps = [];
const full = await stun5780('full');
const report = await doctor({ ...base, stun: [await stunServer(cone), await stunServer(cone)], stun5780: full,
  relays: [r1.url, 'ws://127.0.0.1:1'] }, (s) => steps.push(s));
const v4 = report.ipv4;
check(steps.length === 3 && report.doctor === 1 && report.redacted === false, `steps ${steps}`);
check(v4.mapping.kind === 'endpoint-independent' && v4.mapping.detail.endsWith('port preserved'), JSON.stringify(v4));
check(v4.stun.length === 2 && v4.stun.every((s) => s.mapped.startsWith(`${PUBLIC}:`) && Number.isInteger(s.ms)), JSON.stringify(v4));
check(!full || v4.filtering.kind === 'endpoint-independent', JSON.stringify(v4.filtering));
check(['unknown', 'no-route'].includes(report.ipv6.mapping.kind), JSON.stringify(report.ipv6));
check(report.port_mapping.kind === 'not-available', 'no port mapping in JS');
const [up, down, lan] = report.signaling;
check(up.relay === r1.url && up.kind === 'reachable' && Number.isInteger(up.open_ms) && Number.isInteger(up.round_trip_ms), JSON.stringify(up));
check(down.kind === 'unreachable' && lan.network === 'lan' && lan.kind === 'not-available', JSON.stringify([down, lan]));
check(report.summary.join('\n').includes('Nostr: 1 of 2 relays work'), report.summary);
const text = render(report);
for (const want of ['Peerwire doctor: @peerwire/js ', 'Nothing was uploaded. This report shows', 'Note: Node has no WebRTC',
  '\nIPv4\n  local ', '\nIPv6\n', '\nRouter port mapping\n  result       not-available: ', `nostr ${r1.url}`,
  '\nIn short\n  - IPv4: an easy NAT']) {
  check(text.includes(want), `${JSON.stringify(want)} in\n${text}`);
}
console.log(`a whole report from local fixtures: relay open in ${up.open_ms} ms, round trip ${up.round_trip_ms} ms`);

// Filtering, and servers without RFC 5780.
for (const [mode, want] of [['full', 'endpoint-independent'], ['same-ip-only', 'address-dependent'],
  ['no-changes', 'address-and-port-dependent'], ['late', 'address-and-port-dependent'], ['ignores', 'unsupported'],
  ['refuses', 'unsupported']]) {
  const server = await stun5780(mode);
  if (!server) {
    console.log('127.0.0.2 can\'t be bound here: no filtering test');
    break;
  }
  const r = await doctor({ ...base, stun5780: server });
  check(r.ipv4.filtering.kind === want, `${mode}: ${JSON.stringify(r.ipv4.filtering)}`);
}
check((await doctor({ ...base, stun5780: '127.0.0.1:9' })).ipv4.filtering.kind === 'not-measured', 'no answer, not measured');

// The command: a symmetric NAT, --json and --redact, nothing but the fixtures.
const far = await stunServer((f) => [PUBLIC, (f.port + 1000) % 65536]);
const farther = await stunServer((f) => [PUBLIC, (f.port + 2000) % 65536]);
const bin = new URL('../bin/peerwire.js', import.meta.url).pathname;
const run = promisify(execFile);
const { stdout, stderr } = await run(process.execPath, [bin, 'doctor', '--json', '--redact', '--stun', far, '--stun', farther,
  '--relay', r1.url]);
const out = JSON.parse(stdout);
check(out.ipv4.mapping.kind === 'address-and-port-dependent' && out.redacted === true, stdout);
check(!stdout.includes(PUBLIC) && stdout.includes('198.51.x.x:'), `masked: ${stdout}`);
check(out.signaling[0].kind === 'reachable' && out.summary[0].startsWith('IPv4: a hard NAT'), stdout);
check(stderr.includes('doctor: IPv4'), `progress on stderr: ${stderr}`);
for (const args of [[], ['doctor', '--bogus'], ['doctor', '--stun']]) {
  const code = await run(process.execPath, [bin, ...args]).then(() => 0, (e) => e.code);
  check(code === 1, `${args.join(' ') || '(no command)'} exits 1, not ${code}`);
}

r1.close();
for (const s of sockets) s.close();
console.log('doctor: OK');
process.exit(0);
