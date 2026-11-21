// NB-162: a native node and a browser in one room over a real, direct WebRTC link. The Share page (demo/share/, in
// headless Chromium) and the Rust `share` example built with `webrtc` join the same room through a Nostr relay on
// loopback (nostr_relay.mjs), find each other there, and link over a WebRTC DataChannel on loopback: the page as it
// links with any page (room.js, webrtc.js), the native node with str0m on its own socket. Files go both ways, byte
// for byte, and chat and presence go between room_chat (the same, `webrtc`) and a joinRoom page.
//
// - native -> browser: the example sends a file; the page accepts and downloads it; its SHA-256 matches; the route
//   is the page's own DataChannel (directForFiles); the example says goodbye at the end and the page sees it leave.
// - browser -> native: the page sends a file; the example keeps it; it is the same bytes.
// - names: a file with a bidi override, a path and a reserved name in its name, both ways.
// - chat: room_chat and a page: each joins with the other's presence, text goes both ways, and the page leaving is
//   seen at once (its goodbye).
// - the relay carried signalling alone: its events hold far less than the files.
//
// The page's src/index.js is answered with one that joins with the relay on loopback and no STUN server; nothing
// leaves the host.
//
//   node packages/js/test/share_webrtc.mjs                   16 MiB each way, the examples built here (cargo, gate
//                                                            profile, target/share-examples, as share_native.mjs
//                                                            builds them)
//   node packages/js/test/share_webrtc.mjs --mb=1024 [--bin=dir/with/share/and/room_chat]   1024 MiB each way, timed

import { serve } from './serve.mjs';
import { startRelay } from './nostr_relay.mjs';
import { createHash, randomBytes } from 'node:crypto';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as F from '../demo/share/files.js';

const { cleanName } = F;

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const arg = (name) => process.argv.find((a) => a.startsWith(`--${name}=`))?.split('=')[1] ?? null;
const MB = Number(arg('mb') ?? 16);
const repo = path.join(path.dirname(fileURLToPath(import.meta.url)), '../../..');
const root = path.join(repo, 'packages/js');
const WAIT = { timeout: 60000 };
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'share-webrtc-'));
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
const roomId = () => randomBytes(16).toString('base64url');

// ---------- the examples ----------

function binaries() {
  const at = (dir) => ({ share: path.join(dir, 'share'), chat: path.join(dir, 'room_chat') });
  if (arg('bin')) return at(arg('bin'));
  const dir = path.join(repo, 'target/share-examples');
  // Its own target dir, so it never waits on the gate's Rust lane for cargo's lock, and share_native.mjs's build.
  execFileSync('cargo', ['build', '--profile', 'gate', '--target-dir', dir, '-p', 'peerwire', '--features', 'files,nostr,webrtc',
    '--example', 'share', '--example', 'room_chat'], { cwd: repo, stdio: ['ignore', 'ignore', 'inherit'] });
  return at(path.join(dir, 'gate/examples'));
}

// Runs an example on loopback, with the relay alone (its stdin `ignore`, or `pipe`); `exit` resolves to its exit
// code, `out` is what it printed, `peak` its peak resident memory (KB).
function run(bin, args, relay, { stdin = 'ignore', name = 'native' } = {}) {
  const env = { ...process.env, PW_RELAYS: relay.url, PW_BIND: '127.0.0.1:0', PW_NAME: name };
  const p = spawn(bin, args, { stdio: [stdin, 'pipe', 'pipe'], env });
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
  // Its id as it prints it, the first 12 hex digits: the page finds it by them (findPeer).
  r.id = async () => {
    for (const end = Date.now() + WAIT.timeout; ;) {
      const m = r.out.match(/^([0-9a-f]{12}) in room/m);
      if (m) return m[1];
      if (p.exitCode !== null || Date.now() > end) throw new Error(`no id from the example:\n${r.out}`);
      await new Promise((ok) => setTimeout(ok, 20));
    }
  };
  return r;
}
async function until(ok, what, ex, ms = WAIT.timeout) {
  for (const end = Date.now() + ms; !(await ok());) {
    if (Date.now() > end) throw new Error(`no ${what}${ex ? `; the example printed:\n${ex.out}` : ''}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

// ---------- the page ----------

// src/index.js as the page gets it: the package's, with joinRoom joining through the relay on loopback and no STUN.
const index = (relayUrl) => `
import * as real from './index.js?real';
export * from './index.js?real';
export const joinRoom = (o = {}) => real.joinRoom({ ...o, relays: ['${relayUrl}'], iceServers: [] });
`;

async function newPage(ctx, name, relay) {
  const p = await ctx.newPage();
  p.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  p.on('console', (m) => m.type() === 'error' && !/favicon/.test(m.text()) && errors.push(`${name}: console ${m.text()}`));
  await p.route((u) => u.pathname.endsWith('/src/index.js') && !u.search, (r) => r.fulfill({ contentType: 'text/javascript', body: index(relay.url) }));
  return p;
}

// The Share page in room `room`, once the example whose id starts `short` is in it, on a route that carries files:
// the page and the example's whole id.
async function openShare(ctx, relay, room, short) {
  const p = await newPage(ctx, 'page', relay);
  await p.goto(`http://127.0.0.1:${port}/demo/share/index.html?save=download#${room}`);
  await p.waitForFunction((s) => window.share?.room?.peers.some((x) => x.startsWith(s)) && window.share.saveMode === 'download', short, WAIT);
  const native = await p.evaluate((s) => window.share.room.peers.find((x) => x.startsWith(s)), short);
  await p.waitForFunction((n) => window.share.room.route(n)?.kind === 'direct' && !window.share.room.route(n).turn, native, WAIT);
  return [p, native];
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
// What the example printed when the page joined: its route and kind, and the page's presence.
const joinedLine = (out) => out.match(/^\+ [0-9a-f]{12} \((.*)\) browser (.*)$/m);

// native -> browser: the example sends `f`; the page accepts and downloads it; the example says goodbye at the end.
async function toBrowser(ctx, bins, relay, f, label) {
  const room = roomId();
  const ex = run(bins.share, ['send', room, f.file], relay);
  try {
    const [p, native] = await openShare(ctx, relay, room, await ex.id());
    const presence = await p.evaluate((n) => window.share.room.getPresence(n), native);
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
    const joined = joinedLine(ex.out);
    check(state === 'done' && code === 0 && got === f.sha && d.suggestedFilename() === shown && joined?.[1].includes('Some(Direct)'),
      `native -> browser, ${label}: ${f.size} B as ${JSON.stringify(shown)} over WebRTC (the example: ${joined?.[1]}), the download's SHA-256 matches (${speed(f.size, ms)} from the accept; the example: ${nativeSpeed(ex.out)}, peak RSS ${(ex.peak / 1024).toFixed(0)} MB)`);
    if (state !== 'done' || code !== 0) console.log(ex.out);
    // The example said goodbye as it ended: the page saw it leave, not time out.
    await until(() => p.evaluate((n) => !window.share.room.peers.includes(n), native), 'leave on the page', ex, 5000).then(
      () => check(presence?.name === 'native', `native -> browser, ${label}: the page listed the example by its presence ${JSON.stringify(presence)}, and saw it leave at its goodbye`),
      (e) => check(false, `native -> browser, ${label}: ${e.message}`));
    await d.delete();
    await p.close();
  } finally {
    ex.proc.kill();
  }
}

// browser -> native: the page sends `f`; the example keeps it in a directory of its own.
async function toNative(ctx, bins, relay, f, label) {
  const room = roomId();
  const dir = fs.mkdtempSync(path.join(tmp, 'recv-'));
  const ex = run(bins.share, ['recv', room, dir, '--yes', '--once'], relay);
  try {
    const [p, native] = await openShare(ctx, relay, room, await ex.id());
    const shown = cleanName(path.basename(f.file));
    const t0 = Date.now();
    await p.setInputFiles('#pick', f.file);
    await p.selectOption('#to', native);
    await p.click('#send');
    const state = await waitState(p, 'out', shown, 'done', Math.max(60000, f.size / 2e3));
    const ms = Date.now() - t0;
    const code = await ex.exit;
    const kept = fs.readdirSync(dir);
    const ok = kept.length === 1 && kept[0] === shown && (await shaFile(path.join(dir, kept[0]))) === f.sha;
    const joined = joinedLine(ex.out);
    check(state === 'done' && code === 0 && ok && joined?.[1].includes('Some(Direct)') && /^\{"name":".*"\}$/.test(joined[2]),
      `browser -> native, ${label}: ${f.size} B kept as ${JSON.stringify(kept[0])} over WebRTC (the example saw the page join ${joined?.[1]} with ${joined?.[2]}), the same bytes (${speed(f.size, ms)} from the send; the example: ${nativeSpeed(ex.out)}, peak RSS ${(ex.peak / 1024).toFixed(0)} MB)`);
    if (state !== 'done' || code !== 0 || !ok) console.log(ex.out, kept);
    for (const k of kept) fs.rmSync(path.join(dir, k));
    await p.close();
  } finally {
    ex.proc.kill();
  }
}

// Chat and presence: room_chat and a joinRoom page in one room.
async function chat(ctx, bins, relay) {
  const room = roomId();
  const ex = run(bins.chat, [room, '127.0.0.1:0'], relay, { stdin: 'pipe', name: 'chat-native' });
  try {
    const native = await ex.id(); // its first 12 hex digits
    const p = await newPage(ctx, 'chat', relay);
    await p.goto(`http://127.0.0.1:${port}/test/serve.mjs`);
    await p.evaluate(async (room) => {
      const { joinRoom } = await import('/src/index.js');
      const r = await joinRoom({ appId: 'peerwire-lan-chat', roomId: room, signaling: 'nostr', presence: { name: 'web' } });
      window.got = { joins: [], messages: [], presence: [] };
      r.on('peer-join', (p, pr) => window.got.joins.push([p, pr, r.route(p)]));
      r.on('presence', (p, pr) => window.got.presence.push([p, pr]));
      r.on('message', (d, p, info) => window.got.messages.push([p, d, info.kind]));
      window.r = r;
    }, room);
    await until(() => p.evaluate(() => window.got.joins.length > 0), 'join on the page', ex);
    const [[peer, presence, route]] = await p.evaluate(() => window.got.joins);
    await until(() => / browser \{"name":"web"\}$/m.test(ex.out), 'the page joining the example', ex);
    check(peer.startsWith(native) && presence?.name === 'chat-native' && route?.kind === 'direct',
      `chat: the page and room_chat joined each other over WebRTC with each other's presence (${JSON.stringify(presence)}, route ${JSON.stringify(route)}; the example: ${joinedLine(ex.out)?.[0]})`);
    await p.evaluate(() => window.r.send('héllo native'));
    await until(() => ex.out.includes(': héllo native'), 'the page\'s text at the example', ex);
    ex.proc.stdin.write('hi browser\n');
    await until(() => p.evaluate(() => window.got.messages.some(([, d]) => d === 'hi browser')), 'the example\'s text on the page', ex);
    const m = await p.evaluate(() => window.got.messages);
    check(m.length === 1 && m[0][0] === peer && m[0][2] === 'text', `chat: text both ways (${JSON.stringify(m)})`);
    await p.evaluate(() => window.r.leave());
    await until(() => /^- [0-9a-f]{12}$/m.test(ex.out), 'the page leaving', ex, 3000).then(
      () => check(true, 'chat: the page left and the example saw it at once (its goodbye)'),
      (e) => check(false, `chat: ${e.message}`));
    await p.close();
  } finally {
    ex.proc.kill();
  }
}

let browser;
let server;
let port;
let relay;
try {
  const bins = binaries();
  relay = await startRelay();
  ({ port, server } = await serve(root));
  const big = makeFile(`big-${MB}MiB.bin`, MB << 20);
  const odd = makeFile('con.‮gpj.exe:x', 700 * 1024 + 3);
  console.log(`     ${big.file}: ${big.size} B, SHA-256 ${big.sha}`);
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns', '--allow-loopback-in-peer-connection'] });
  const ctx = await browser.newContext({ acceptDownloads: true });
  await chat(ctx, bins, relay);
  await toBrowser(ctx, bins, relay, odd, 'an odd name');
  await toNative(ctx, bins, relay, odd, 'an odd name');
  await toBrowser(ctx, bins, relay, big, `${MB} MiB`);
  await toNative(ctx, bins, relay, big, `${MB} MiB`);
  const bytes = relay.events.reduce((n, e) => n + (e.content?.length ?? 0), 0);
  check(bytes < big.size / 8, `the relay carried signalling alone: ${relay.events.length} events, ${bytes} B of content in all`);
} catch (e) {
  check(false, `threw: ${e.stack ?? e}`);
} finally {
  await browser?.close();
  server?.kill();
  relay?.close();
  fs.rmSync(tmp, { recursive: true, force: true });
}
if (errors.length) check(false, `page errors:\n     ${errors.join('\n     ')}`);
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
