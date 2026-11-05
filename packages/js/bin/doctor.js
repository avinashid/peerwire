// peerwire doctor for Node (`npx @peerwire/js doctor`): what this network does to peer-to-peer traffic, as a report
// to paste into an issue or a row of the field results (docs/project/field-connectivity.md). The same report as the
// Rust doctor (examples/rust/doctor.rs) as far as Node and this package go:
//
// - NAT mapping (STUN from one socket to several servers) and filtering (RFC 5780, with --stun5780), IPv4 and IPv6;
// - whether each Nostr relay takes connections and forwards a room hello from one to another.
//
// Router port mapping and the LAN carrier are the Rust crate's alone, so they say `not-available`; and Node has no
// WebRTC, so the mapping is what a browser's STUN meets on this network, not a test of ICE itself.
//
// Nothing is uploaded: doctor() returns the report and the command prints it. To make it, it sends STUN Binding
// requests to the probe servers (Google's and Cloudflare's), and publishes one room hello on a random room under a
// throwaway key to each relay, sealed as every hello is (relays forward those ephemeral events and keep none).
import dgram from 'node:dgram';
import { lookup } from 'node:dns/promises';
import { randomBytes } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { loadPeerwire } from '../src/peerwire.js';
import { NostrSignaling, DEFAULT_RELAYS } from '../src/nostr.js';

// As the Rust doctor's: two server IPs and four IP:port pairs, interleaved so consecutive requests go to different
// IPs. Not port 53, which carriers and routers intercept.
export const PROBE_SERVERS = ['stun.l.google.com:19302', 'stun.cloudflare.com:3478', 'stun.l.google.com:3478',
  'stun.cloudflare.com:443'];
// The report's format, the same number as the Rust doctor's: bumped when a field changes meaning.
export const FORMAT = 1;
const LAN_GROUP = '239.255.80.87:20567';
const RUST_DOCTOR = 'the Rust doctor checks it (cargo run -p peerwire --features nostr --example doctor)';
const VERSION = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')).version;
const MAGIC = 0x2112a442;
const MAGIC_BYTES = Buffer.from([0x21, 0x12, 0xa4, 0x42]);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export const defaults = () => ({
  stun: [...PROBE_SERVERS], // the mapping probe's servers
  stun5780: null, // a STUN server with RFC 5780 support, for the filtering test
  stunWaitMs: 1000, // how long to wait for a STUN answer before asking again (three tries)
  relays: [...DEFAULT_RELAYS],
  waitMs: 8000, // how long one relay check may take
  redact: false, // mask public IP addresses
  wasm: new URL('../pkg/peerwire_ffi.wasm', import.meta.url),
});

// Runs every check and returns the report; `progress` hears each step as it starts.
export async function doctor(options = {}, progress = () => {}) {
  const cfg = { ...defaults(), ...options };
  progress('IPv4: asking the STUN servers how this network maps a socket');
  const ipv4 = await probe(cfg, false);
  progress('IPv6: the same');
  const ipv6 = await probe(cfg, true);
  progress('signaling: asking the relays');
  const pw = cfg.relays.length && typeof WebSocket === 'function' ? await loadPeerwire(cfg.wasm) : null;
  const relays = await Promise.all(cfg.relays.map((url) => relay(pw, url, cfg.waitMs)));
  const report = {
    doctor: FORMAT,
    tool: `@peerwire/js ${VERSION} (Node ${process.version})`,
    date: new Date().toISOString().replace(/\.\d+Z$/, 'Z'),
    os: process.platform,
    arch: process.arch,
    redacted: cfg.redact,
    notes: ["Node has no WebRTC: a browser links over WebRTC, whose STUN meets the same NAT as the probe below"],
    ipv4,
    ipv6,
    port_mapping: { kind: 'not-available', detail: `@peerwire/js maps no router ports (browsers can't): ${RUST_DOCTOR}` },
    signaling: [...relays, { network: 'lan', group: LAN_GROUP, kind: 'not-available',
      detail: `@peerwire/js has no LAN carrier (browsers can't send UDP): ${RUST_DOCTOR}` }],
  };
  report.summary = summary(report);
  return report;
}

// Asks each probe server, in turn and from one socket, for its view of our address, and says what the answers make
// of the NAT; then the filtering test.
async function probe(cfg, v6) {
  const family = v6 ? 'IPv6' : 'IPv4';
  const noRoute = (detail) => ({ local: null, mapping: { kind: 'no-route', detail } });
  const localIp = await primaryIp(v6);
  if (!localIp) return noRoute(`no ${family} route from this machine`);
  if (v6 && !isGlobalV6(localIp)) return noRoute(`${localIp} is not a global address: no IPv6 internet here`);
  let sock;
  try {
    sock = await openSocket(v6);
  } catch (e) {
    return noRoute(`can't open a UDP socket: ${e.message}`);
  }
  try {
    const local = { ip: localIp, port: sock.address().port };
    // One request per distinct destination: several names can share an address.
    const dests = [];
    for (const server of cfg.stun) {
      const addr = await resolve(server, v6);
      if (addr && !dests.some((d) => same(d.addr, addr))) dests.push({ server, addr });
    }
    const answers = [];
    const mapped = [];
    for (const { server, addr } of dests) {
      const tx = randomBytes(12);
      const got = await ask(sock, addr, bindingRequest(tx), tx, cfg.stunWaitMs);
      const m = got && mappedAddress(got.b, tx);
      if (m) {
        answers.push({ server, addr: fmt(addr), mapped: show(cfg, m), ms: Math.round(got.ms) });
        mapped.push([addr, m]);
      } else {
        answers.push({ server, addr: fmt(addr), mapped: null });
      }
    }
    const [kind, detail] = dests.length ? verdict(local, mapped) : ['unknown', `none of the STUN servers has an ${family} address`];
    let filtering;
    if (v6) filtering = { kind: 'not-measured', detail: 'the filtering test runs over IPv4' };
    else if (cfg.stun5780) filtering = await filteringTest(cfg.stun5780, cfg.stunWaitMs);
    else filtering = { kind: 'not-measured', detail: "needs a STUN server with RFC 5780 support (--stun5780 host:port): Google's and Cloudflare's can't answer from another address" };
    return { local: show(cfg, local), stun: answers, mapping: { kind, detail }, filtering };
  } finally {
    sock.close();
  }
}

// Classifies the NAT from [destination, mapped address] pairs, in send order: a kind for the JSON and a sentence
// for people. The Rust doctor's verdict(), word for word.
export function verdict(local, mapped) {
  if (!mapped.length) return ['blocked', 'no STUN answers: UDP is blocked here, or the servers are unreachable'];
  if (mapped.length === 1) return ['unknown', "only one answer: can't tell"];
  const first = mapped[0][1];
  if (mapped.every(([, m]) => same(m, first))) {
    if (same(first, local)) return ['no-nat', "the public address is this machine's own (a firewall may still drop unsolicited packets)"];
    const kept = first.port === local.port ? ', port preserved' : '';
    return ['endpoint-independent', `cone: the same public address for every destination${kept}`];
  }
  const distinct = new Set(mapped.map(([, m]) => m.ip)).size;
  if (mapped.every(([, m]) => m.port === first.port)) {
    const kept = first.port === local.port ? ' (preserved)' : '';
    return ['pooled', `port kept for every destination${kept}, but the public IP varies across ${distinct} addresses ` +
      '(an address pool without paired pooling)'];
  }
  const multiIp = distinct > 1 ? '; public IP varies by destination' : '';
  // Same mapping for every port of one server IP means the NAT keys on the address only.
  const sameIp = (d) => mapped.filter(([d2]) => d2.ip === d.ip);
  const perIp = mapped.every(([d, m]) => sameIp(d).every(([, m2]) => same(m2, m)));
  const shared = mapped.some(([d]) => sameIp(d).length > 1);
  const [kind, keyed] = !shared ? ['dependent', '']
    : perIp ? ['address-dependent', ', address-dependent (one port per server IP)']
      : ['address-and-port-dependent', ', address-and-port-dependent (a new port for every destination)'];
  // Deltas between successive new ports: small steps mean a predictable allocator.
  const ports = [...new Set(mapped.map(([, m]) => m.port))];
  const deltas = ports.slice(1).map((p, i) => p - ports[i]);
  const list = `[${deltas.join(', ')}]`;
  let alloc;
  if (!deltas.length) alloc = 'one port';
  else if (deltas.length === 1) alloc = `1 delta ${list}, too few to call the allocator`;
  else if (deltas.every((d) => Math.abs(d) <= 16)) alloc = `sequential ports, deltas ${list}`;
  else alloc = `random ports, deltas ${list}`;
  return [kind, `symmetric${keyed}; ${alloc}${multiIp}`];
}

// The RFC 5780 filtering test (section 4.4) from a fresh socket: a Binding request to `server` (test I), then the
// same asking for the answer from its other IP and port (II), then from its other port only (III). What comes
// through says what this NAT lets in from addresses it hasn't sent to. A server that answers a change from its own
// address, or refuses the attribute, has no RFC 5780 support: that says nothing about the NAT.
async function filteringTest(server, waitMs) {
  const result = (kind, detail) => ({ kind, detail, server });
  const dest = await resolve(server, false);
  if (!dest) return result('not-measured', `${server} has no IPv4 address`);
  let sock;
  try {
    sock = await openSocket(false);
  } catch {
    return result('not-measured', "can't open a UDP socket");
  }
  try {
    // Where an answer came from, and whether it was an error response.
    const test = async (change) => {
      const tx = randomBytes(12);
      const got = await ask(sock, dest, bindingRequest(tx, change), tx, waitMs);
      return got && { from: got.from, error: got.b.readUInt16BE(0) === 0x0111 };
    };
    const first = await test(0);
    if (!first || first.error || !same(first.from, dest)) return result('not-measured', `no answer from ${server}`);
    const unsupported = ({ from, error }) => result('unsupported', `${server} ${error ? 'refused CHANGE-REQUEST'
      : 'answered from its own address'} (from ${fmt(from)}): it has no RFC 5780 support`);
    const other = await test(0x06);
    if (other && !other.error && other.from.ip !== dest.ip) {
      return result('endpoint-independent', 'an answer from another IP and port came in: any host can reach a mapping');
    }
    if (other) return unsupported(other);
    const port = await test(0x02);
    if (port && !port.error && port.from.ip === dest.ip && port.from.port !== dest.port) {
      return result('address-dependent', 'only an answer from the same IP came in: a host this machine sent to can reach it from any port');
    }
    if (port) return unsupported(port);
    return result('address-and-port-dependent', 'no answer from another IP or port came in: only the address this machine sent to gets through');
  } finally {
    sock.close();
  }
}

// Whether the relay at `url` takes connections, and forwards a room hello from one to another in a random room.
async function relay(pw, url, waitMs) {
  const base = { network: 'nostr', relay: url };
  if (typeof WebSocket !== 'function') {
    return { ...base, kind: 'not-available', detail: 'this Node has no WebSocket: Node 22 or later has one' };
  }
  const room = `peerwire-doctor-${randomBytes(8).toString('hex')}`;
  const eps = [pw.endpoint(), pw.endpoint()];
  const [rx, tx] = eps.map((ep) => new NostrSignaling(pw, ep, { room, relays: [url] }));
  const start = performance.now();
  try {
    let back = null;
    rx.onHello = (_, meta) => {
      if (Number.isInteger(meta?.doctor)) back ??= { n: meta.doctor, at: performance.now() };
    };
    const open = () => rx.stats.relaysOpen > 0 && tx.stats.relaysOpen > 0;
    while (!open() && performance.now() - start < waitMs) await sleep(5);
    if (!open()) return { ...base, kind: 'unreachable', detail: `no connection in ${waitMs / 1000} s` };
    const openMs = Math.round(performance.now() - start);
    // Again each second: the first can reach the relay before the subscription does.
    const sent = [];
    while (performance.now() - start < waitMs) {
      if (back && sent[back.n] !== undefined) {
        const ms = Math.round(back.at - sent[back.n]);
        return { ...base, kind: 'reachable', open_ms: openMs, round_trip_ms: ms, detail: `open in ${openMs} ms, an event came back in ${ms} ms` };
      }
      if (!sent.length || performance.now() - sent.at(-1) >= 1000) {
        sent.push(performance.now());
        tx.hello({ doctor: sent.length - 1 });
      }
      await sleep(5);
    }
    const refused = tx.stats.rejected > 0 ? ' (the relay refused it)' : '';
    return { ...base, kind: 'no-echo', open_ms: openMs, detail: `open in ${openMs} ms, but our event didn't come back${refused}` };
  } finally {
    rx.close();
    tx.close();
    for (const ep of eps) ep.close();
  }
}

// A few lines on what the report means for links from here: the Rust doctor's summary().
function summary(r) {
  const out = [];
  switch (r.ipv4.mapping.kind) {
    case 'no-nat': out.push('IPv4: a public address with no NAT: peers can dial in unless a firewall drops them'); break;
    case 'endpoint-independent': out.push('IPv4: an easy NAT (endpoint-independent mapping): hole punching works'); break;
    case 'address-dependent': case 'address-and-port-dependent': case 'dependent': case 'pooled':
      out.push('IPv4: a hard NAT: a direct link needs an easy NAT on the other side, a birthday punch or port mapping; ' +
        'otherwise a relay peer carries it');
      break;
    case 'blocked': out.push('IPv4: no STUN answer: UDP looks blocked, so no direct or peer-relayed links over IPv4'); break;
    case 'no-route': out.push('IPv4: no route from this machine'); break;
    default: out.push('IPv4: too few STUN answers to tell');
  }
  const v6 = ['no-nat', 'endpoint-independent'].includes(r.ipv6.mapping.kind);
  out.push(v6 ? 'IPv6: works: peers with IPv6 too can link directly over it' : 'IPv6: none working here: links use IPv4');
  if (r.port_mapping.kind === 'mapped') out.push('Port mapping works: peers can reach the mapped port, whatever the NAT does');
  if (r.port_mapping.kind === 'not-public') out.push("Port mapping: the router is behind another NAT, so a mapping can't help");
  const relays = r.signaling.filter((s) => s.network === 'nostr');
  const up = relays.filter((s) => s.kind === 'reachable').length;
  if (relays.length && !up) out.push("Nostr: no relay works from here, so rooms over Nostr won't meet: use invites");
  else if (relays.length) out.push(`Nostr: ${up} of ${relays.length} relays work, so rooms can meet`);
  return out;
}

// The report as text that pastes well: one section per check, as the Rust doctor prints it.
export function render(r) {
  const row = (label, text) => `  ${label.padEnd(12)} ${text}\n`;
  let out = `Peerwire doctor: ${r.tool}, ${r.os} ${r.arch}, ${r.date}\n`;
  out += r.redacted ? 'Nothing was uploaded. Public IP addresses are masked.\n'
    : 'Nothing was uploaded. This report shows your public IP addresses: --redact masks them.\n';
  for (const note of r.notes ?? []) out += `Note: ${note}.\n`;
  for (const [name, f] of [['IPv4', r.ipv4], ['IPv6', r.ipv6]]) {
    out += `\n${name}\n${row('local', f.local ?? 'none')}`;
    (f.stun ?? []).forEach((a, i) => {
      out += row(i ? '' : 'STUN', `${a.server} (${a.addr}) -> ${a.mapped ? `${a.mapped} in ${a.ms} ms` : 'no answer'}`);
    });
    out += row('mapping', `${f.mapping.kind}: ${f.mapping.detail}`);
    if (f.filtering) out += row('filtering', `${f.filtering.kind}: ${f.filtering.detail}`);
  }
  out += `\nRouter port mapping\n${row('result', `${r.port_mapping.kind}: ${r.port_mapping.detail}`)}`;
  out += '\nSignaling\n';
  for (const s of r.signaling) out += `  ${`${s.network} ${s.network === 'lan' ? s.group : s.relay}`.padEnd(32)} ${s.kind}: ${s.detail}\n`;
  out += '\nIn short\n';
  for (const line of r.summary) out += `  - ${line}\n`;
  return out;
}

// STUN (RFC 8489): a Binding request, with a CHANGE-REQUEST attribute (RFC 5780) when `change` has flags.
function bindingRequest(tx, change = 0) {
  const b = Buffer.alloc(change ? 28 : 20);
  b.writeUInt16BE(0x0001, 0);
  b.writeUInt16BE(change ? 8 : 0, 2);
  b.writeUInt32BE(MAGIC, 4);
  tx.copy(b, 8);
  if (change) {
    b.writeUInt16BE(0x0003, 20);
    b.writeUInt16BE(4, 22);
    b.writeUInt32BE(change, 24);
  }
  return b;
}

// The reflexive address in a Binding success response to transaction `tx`: XOR-MAPPED-ADDRESS, else MAPPED-ADDRESS.
export function mappedAddress(b, tx) {
  if (b.length < 20 || b.readUInt16BE(0) !== 0x0101 || b.readUInt32BE(4) !== MAGIC || !b.subarray(8, 20).equals(tx)) return null;
  const end = Math.min(b.length, 20 + b.readUInt16BE(2));
  let plain = null;
  for (let i = 20; i + 4 <= end; i += 4 + ((b.readUInt16BE(i + 2) + 3) & ~3)) {
    const type = b.readUInt16BE(i);
    const v = b.subarray(i + 4, Math.min(end, i + 4 + b.readUInt16BE(i + 2)));
    if (type === 0x0020) return decodeAddr(v, tx, true);
    if (type === 0x0001) plain ??= decodeAddr(v, tx, false);
  }
  return plain;
}

function decodeAddr(v, tx, xor) {
  const n = v[1] === 1 ? 4 : v[1] === 2 ? 16 : 0;
  if (!n || v.length < 4 + n) return null;
  const key = Buffer.concat([MAGIC_BYTES, tx]);
  const b = Buffer.from(v.subarray(4, 4 + n).map((x, i) => (xor ? x ^ key[i] : x)));
  const ip = n === 4 ? [...b].join('.') : canon(Array.from({ length: 8 }, (_, i) => b.readUInt16BE(2 * i).toString(16)).join(':'));
  return { ip, port: v.readUInt16BE(2) ^ (xor ? MAGIC >>> 16 : 0) };
}

// Sends `req` to `dest` up to three times, waiting `waitMs` after each, for a STUN message of transaction `tx`:
// resolves to where it came from, its bytes and the time since the last send, or null.
function ask(sock, dest, req, tx, waitMs) {
  return new Promise((resolve) => {
    let tries = 0;
    let sent = 0;
    let timer;
    const done = (v) => {
      clearTimeout(timer);
      sock.off('message', onMessage);
      resolve(v);
    };
    const onMessage = (b, from) => {
      if (b.length >= 20 && b.readUInt32BE(4) === MAGIC && b.subarray(8, 20).equals(tx)) {
        done({ from: { ip: canon(from.address), port: from.port }, b, ms: performance.now() - sent });
      }
    };
    const send = () => {
      if (tries++ === 3) return done(null);
      sent = performance.now();
      sock.send(req, dest.port, dest.ip, (e) => e && done(null));
      timer = setTimeout(send, waitMs);
    };
    sock.on('message', onMessage);
    send();
  });
}

function openSocket(v6) {
  return new Promise((resolve, reject) => {
    const s = dgram.createSocket({ type: v6 ? 'udp6' : 'udp4', ipv6Only: v6 });
    s.once('error', reject);
    s.bind(0, () => {
      s.off('error', reject);
      s.on('error', () => {}); // a send's error goes to its callback
      resolve(s);
    });
  });
}

// The address the default route leaves by (connect on UDP sends nothing).
function primaryIp(v6) {
  return new Promise((resolve) => {
    const s = dgram.createSocket(v6 ? 'udp6' : 'udp4');
    let closed = false;
    const done = (ip) => {
      if (closed) return;
      closed = true;
      s.close();
      resolve(ip);
    };
    s.on('error', () => done(null));
    s.connect(9, v6 ? '2001:db8::1' : '192.0.2.1', (e) => {
      if (e) return done(null);
      const ip = canon(s.address().address);
      done(['0.0.0.0', '::', '::1'].includes(ip) || ip.startsWith('127.') ? null : ip);
    });
  });
}

// 'host:port' or '[v6]:port' -> the first address of the family, or null.
async function resolve(server, v6) {
  const m = /^\[([^\]]+)\]:(\d+)$/.exec(server) ?? /^([^:]+):(\d+)$/.exec(server);
  if (!m) return null;
  try {
    const all = await lookup(m[1], { all: true, family: v6 ? 6 : 4 });
    return all.length ? { ip: canon(all[0].address), port: Number(m[2]) } : null;
  } catch {
    return null;
  }
}

// One spelling per IPv6 address (RFC 5952, as the URL parser writes it), so addresses compare as strings.
const canon = (ip) => (ip.includes(':') ? new URL(`http://[${ip.split('%')[0]}]/`).hostname.slice(1, -1) : ip);
const same = (a, b) => a.ip === b.ip && a.port === b.port;
const fmt = (a) => (a.ip.includes(':') ? `[${a.ip}]:${a.port}` : `${a.ip}:${a.port}`);

function groups(ip) {
  const [head, tail] = canon(ip).split('::');
  const h = head ? head.split(':') : [];
  const t = tail ? tail.split(':') : [];
  return [...h, ...Array(tail === undefined ? 0 : 8 - h.length - t.length).fill('0'), ...t].map((g) => parseInt(g, 16));
}

// 2000::/3, the global unicast range (not link-local, ULA or a documentation prefix).
function isGlobalV6(ip) {
  const [a, b] = groups(ip);
  return (a & 0xe000) === 0x2000 && !(a === 0x2001 && b === 0x0db8);
}

// An address on the internet, which --redact masks: not private, carrier-grade NAT (100.64/10), loopback,
// link-local or ULA.
function isPublic(ip) {
  if (ip.includes(':')) return isGlobalV6(ip);
  const [a, b] = ip.split('.').map(Number);
  return !(a === 10 || a === 127 || a === 0 || (a === 172 && (b & 0xf0) === 16) || (a === 192 && b === 168) ||
    (a === 169 && b === 254) || (a === 100 && (b & 0xc0) === 64));
}

// `a` as the report shows it: a public IP masked under --redact.
export function show(cfg, a) {
  if (!cfg.redact || !isPublic(a.ip)) return fmt(a);
  if (!a.ip.includes(':')) return `${a.ip.split('.').slice(0, 2).join('.')}.x.x:${a.port}`;
  const [g0, g1] = groups(a.ip);
  return `[${g0.toString(16)}:${g1.toString(16)}:x:x:x:x:x:x]:${a.port}`;
}
