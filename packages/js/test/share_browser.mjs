// Opens Peerwire Share (demo/share/) in tabs of headless Chromium (?signal=local, loopback only) and sends files
// between them, checking each one saved against node:crypto's SHA-256:
//
// - A and B save through the service worker (?save=download), C to disk (?save=disk; showSaveFilePicker is stubbed
//   with a file in the origin's private file system, which has the same writable stream), D in memory (?save=memory)
//   and with WebRTC failing (?rtc=off), so its links go over the fallback.
// - Several small files to everyone, one of no bytes, and one declined; drag and drop.
// - Direct only: D accepts a file, and no piece goes while its link is on the fallback; once WebRTC works again and a
//   retry links it directly, the file goes. In memory, a file over 256 MB can't be accepted.
// - Resume: A's DataChannel to B closes mid-file; the two link again and the file goes on from the last piece B
//   wrote, not from the start (the sessions are ended at once, as the core's 30 s idle timeout would; --natural-drop
//   waits for it instead). And one that closes once A has sent the whole file, while B still writes it: B finishes
//   alone, and A learns it was delivered when they link again.
// - Cancel by the receiver (nothing saved on disk) and by the sender (the download fails).
// - A peer that leaves the room: what it was offered ends at once.
// - A hostile peer: offers with a path in the name, a bad size, a flood of offers, an oversized control message,
//   pieces for a transfer not accepted, out of order, and with a bad hash.
//
//   node packages/js/test/share_browser.mjs [--natural-drop]
//   node packages/js/test/share_browser.mjs --big=1024 [--file=path]   one file of 1024 MB (made in a temp dir, or
//                                      `path`), A to B: time, speed, peak memory of each tab; the files are deleted

import { serve } from './serve.mjs';
import { createHash, randomBytes } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const arg = (name) => process.argv.find((a) => a.startsWith(`--${name}`))?.split('=')[1] ?? (process.argv.includes(`--${name}`) ? '' : null);
const BIG = arg('big');
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const { port, server } = await serve(root);
const page = (q) => `http://127.0.0.1:${port}/demo/share/index.html?signal=local&${q}#share-test-${port}`;
const WAIT = { timeout: 45000 }; // a loaded host links tabs slowly
const MARK = `--peerwire-share-test=${process.pid}-${Date.now()}`; // finds this test's browser among the host's
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'share-test-'));
let browser;
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const errors = [];
const sha = (b) => createHash('sha256').update(b).digest('hex');
async function shaFile(p) {
  const h = createHash('sha256');
  for await (const c of fs.createReadStream(p)) h.update(c);
  return h.digest('hex');
}
const MiB = 1024 * 1024;

// A tab, its errors watched.
async function open(ctx, name, q, init) {
  const p = await ctx.newPage();
  p.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  p.on('console', (m) => m.type() === 'error' && !/favicon/.test(m.text()) && errors.push(`${name}: console ${m.text()}`));
  if (init) await p.addInitScript(init);
  await p.goto(page(q));
  await p.waitForFunction(() => window.share?.room, null, WAIT);
  return p;
}
const self = (p) => p.evaluate(() => window.share.room.self);
// The newest row of a file, by direction ('in' or 'out') and name.
const row = (p, dir, name) => p.locator(`#transfers li[data-dir="${dir}"]`).filter({ has: p.locator('.name', { hasText: name }) }).first();
const stateOf = (p, dir, name) => row(p, dir, name).getAttribute('data-state');
async function waitState(p, dir, name, states, timeout = WAIT.timeout) {
  const want = [states].flat();
  const until = Date.now() + timeout;
  for (;;) {
    const s = await stateOf(p, dir, name).catch(() => null);
    if (want.includes(s)) return s;
    if (Date.now() > until) throw new Error(`${name} (${dir}) is ${s}, not ${want.join(' or ')}, after ${timeout} ms`);
    await p.waitForTimeout(100);
  }
}
// Offers files from tab `p` to `to` (a peer id, or '' for everyone).
async function offer(p, files, to = '') {
  await p.setInputFiles('#pick', files);
  await p.selectOption('#to', to);
  await p.click('#send');
}
// A transfer's fields, from the tab's own maps.
const xfer = (p, dir, name) => p.evaluate(([dir, name]) => {
  const t = [...(dir === 'out' ? window.share.outgoing : window.share.incoming).values()].filter((x) => x.name === name).at(-1);
  return t && { state: t.state, next: t.next, acked: t.acked, have: t.have, count: t.count, sent: t.sent, resumes: t.resumes, sha: t.sha, note: t.note, endSent: t.endSent };
}, [dir, name]);
const statsOf = (p) => p.evaluate(() => ({ ...window.share.stats }));

// Resident memory (KB) of each process of this test's browser, by pid.
function browserRss() {
  const rows = execFileSync('ps', ['-eo', 'pid=,ppid=,rss=,args='], { encoding: 'utf8' }).trim().split('\n').map((l) => {
    const m = l.trim().match(/^(\d+)\s+(\d+)\s+(\d+)\s+(.*)$/);
    return m && { pid: +m[1], ppid: +m[2], rss: +m[3], args: m[4] };
  }).filter(Boolean);
  const top = rows.find((r) => r.args.includes(MARK) && !r.args.includes('--type='));
  if (!top) return new Map();
  const tree = new Set([top.pid]);
  for (let grew = true; grew;) {
    grew = false;
    for (const r of rows) if (!tree.has(r.pid) && tree.has(r.ppid)) (tree.add(r.pid), (grew = true));
  }
  return new Map(rows.filter((r) => tree.has(r.pid)).map((r) => [r.pid, { rss: r.rss, type: r.args.match(/--type=([\w-]+)/)?.[1] ?? 'browser' }]));
}

try {
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns', MARK] });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 860 }, acceptDownloads: true });
  if (BIG !== null) await big(ctx);
  else await checks(ctx);
} catch (e) {
  check(false, `threw: ${e.stack ?? e}`);
  for (const p of browser?.contexts()[0]?.pages() ?? []) {
    console.log(await p.evaluate(() => JSON.stringify({
      self: window.share.room?.self.slice(0, 8), stats: window.share.stats,
      out: [...window.share.outgoing.values()].map((t) => [t.name, t.peer.slice(0, 8), t.state, t.next, t.acked, t.read, t.sent, t.endSent, t.gone, t.note]),
      in: [...window.share.incoming.values()].map((t) => [t.name, t.peer.slice(0, 8), t.state, t.have, t.queue.length, t.want?.slice(0, 8), t.gone, t.note]),
    })).catch((x) => String(x)));
  }
} finally {
  await browser?.close();
  server.kill();
  fs.rmSync(tmp, { recursive: true, force: true });
}
if (errors.length) check(false, `page errors:\n     ${errors.join('\n     ')}`);
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);

async function checks(ctx) {
  // C saves to "disk": a file in the origin's private file system, through the same writable stream.
  const picker = () => {
    window.showSaveFilePicker = async ({ suggestedName }) => {
      window.pickerCalls = (window.pickerCalls ?? 0) + 1;
      const dir = await navigator.storage.getDirectory();
      return dir.getFileHandle(`recv-${suggestedName}`, { create: true });
    };
  };
  const a = await open(ctx, 'A', 'save=download');
  const b = await open(ctx, 'B', 'save=download');
  const c = await open(ctx, 'C', 'save=disk', picker);
  const [A, B, C] = await Promise.all([a, b, c].map(self));
  const t0 = Date.now();
  for (const [p, n] of [[a, 3], [b, 3], [c, 3]]) await p.waitForFunction((n) => document.querySelectorAll('#peers li').length >= n - 1, n, WAIT);
  for (const p of [a, b, c]) await p.waitForFunction(() => [...document.querySelectorAll('#peers li')].every((l) => l.dataset.route === 'direct'), null, WAIT);
  console.log(`     A, B and C linked directly in ${Date.now() - t0} ms`);
  check(await a.$eval('#save-mode', (e) => e.textContent).then((t) => /stream into your downloads/.test(t)) &&
    await c.$eval('#save-mode', (e) => e.textContent).then((t) => /straight to disk/.test(t)), 'each tab says how it saves');
  check(await a.$eval('#peers li .pill', (e) => e.textContent) === 'DIRECT' && await a.$eval('#direct-hint', (e) => e.classList.contains('hidden')),
    'peers show DIRECT, and no hint about direct links');
  check(/#share-test-\d+$/.test(await a.$eval('#link', (e) => e.value)) && await a.$eval('#qr', (e) => e.hasAttribute('viewBox')), 'the invite link and its QR code');

  // 1. Small files to everyone: B downloads, C writes to disk.
  const files = [
    { name: 'hello.txt', mimeType: 'text/plain', buffer: Buffer.from('hello, peer\n') },
    { name: 'empty.bin', mimeType: 'application/octet-stream', buffer: Buffer.alloc(0) },
    { name: 'two-pieces.bin', mimeType: 'application/octet-stream', buffer: randomBytes(512 * 1024 + 1000) },
  ];
  await offer(a, files);
  for (const p of [b, c]) await p.waitForFunction(() => document.querySelectorAll('#transfers li[data-dir="in"][data-state="offered"]').length === 3, null, WAIT);
  check(true, 'B and C are each offered the 3 files');
  check(await statsOf(b).then((s) => s.piecesTaken === 0), 'nothing is taken before an accept');
  for (const f of files) {
    const dl = b.waitForEvent('download', WAIT);
    await row(b, 'in', f.name).locator('[data-act="accept"]').click();
    const d = await dl;
    await waitState(b, 'in', f.name, 'done');
    check(d.suggestedFilename() === f.name && await shaFile(await d.path()) === sha(f.buffer), `B downloaded ${f.name} (${f.buffer.length} B), its SHA-256 matching`);
    await row(c, 'in', f.name).locator('[data-act="accept"]').click();
    await waitState(c, 'in', f.name, 'done');
    const got = await c.evaluate(async (n) => {
      const file = await (await (await navigator.storage.getDirectory()).getFileHandle(`recv-${n}`)).getFile();
      return [...new Uint8Array(await crypto.subtle.digest('SHA-256', await file.arrayBuffer()))].map((x) => x.toString(16).padStart(2, '0')).join('');
    }, f.name);
    check(got === sha(f.buffer), `C wrote ${f.name} to disk, its SHA-256 matching`);
  }
  for (const f of files) await waitState(a, 'out', f.name, 'done');
  const shown = await row(b, 'in', 'two-pieces.bin').locator('.hash').textContent();
  check(shown === `SHA-256 ${sha(files[2].buffer)}` && /matches/.test(await row(a, 'out', 'two-pieces.bin').locator('.state').textContent()),
    'both sides show the whole file\'s SHA-256 as verified');
  check(await c.evaluate(() => window.pickerCalls) === 3, 'C chose where each file goes (the save picker, once per file)');

  // 2. Declined: nothing is sent.
  await offer(a, [{ name: 'no-thanks.bin', mimeType: 'application/octet-stream', buffer: randomBytes(700 * 1024) }], B);
  await row(b, 'in', 'no-thanks.bin').locator('[data-act="decline"]').click();
  await waitState(a, 'out', 'no-thanks.bin', 'declined');
  check((await xfer(a, 'out', 'no-thanks.bin')).sent === 0 && await c.$$eval('#transfers li', (l) => l.length) === 3, 'a declined file sends no piece, and went to B alone');

  // 3. Drag and drop picks files.
  await a.evaluate(() => {
    const dt = new DataTransfer();
    dt.items.add(new File(['dropped'], 'dropped.txt', { type: 'text/plain' }));
    window.dispatchEvent(new DragEvent('drop', { dataTransfer: dt, bubbles: true, cancelable: true }));
  });
  check(await a.$eval('#picked', (e) => e.textContent).then((t) => t.includes('dropped.txt')) && await a.isVisible('#send'), 'a dropped file is picked');
  await a.click('#picked button');
  check(await a.$$eval('#picked li', (l) => l.length) === 0, 'and can be removed before sending');

  // 4. Resume after the link drops.
  const resumeBuf = randomBytes(24 * MiB + 333);
  const resumeDl = b.waitForEvent('download', { timeout: 120000 });
  await offer(a, [{ name: 'resume.bin', mimeType: 'application/octet-stream', buffer: resumeBuf }], B);
  await row(b, 'in', 'resume.bin').locator('[data-act="accept"]').click();
  await b.waitForFunction(() => [...window.share.incoming.values()].find((t) => t.name === 'resume.bin')?.have >= 8, null, WAIT);
  const natural = arg('natural-drop') !== null;
  await a.evaluate(([B, natural]) => {
    const r = window.share.room;
    r.transport.links.get(r.transport.byPeer.get(B)).dc.close();
    if (!natural) r.endpoint.disconnect(B);
  }, [B, natural]);
  if (!natural) await b.evaluate((A) => window.share.room.endpoint.disconnect(A), A);
  await b.waitForFunction(() => /left: waiting/.test(document.querySelector('#transfers li[data-dir="in"][data-state="receiving"] .state')?.textContent), null, WAIT);
  const before = await xfer(b, 'in', 'resume.bin');
  check(/left: waiting for them to come back/.test(await row(b, 'in', 'resume.bin').locator('.state').textContent()), `the link dropped at piece ${before.have} of ${before.count}: B waits for A`);
  if (!natural) for (const p of [a, b]) await p.evaluate(() => window.share.room.hello());
  const d4 = await resumeDl;
  await waitState(b, 'in', 'resume.bin', 'done', natural ? 120000 : WAIT.timeout);
  const out4 = await xfer(a, 'out', 'resume.bin');
  check(out4.resumes === 1 && out4.sent >= out4.count && out4.sent <= out4.count + 8,
    `A resumed once and sent ${out4.sent} pieces for ${out4.count} (at most a window of 8 again, not the file again)`);
  check(await shaFile(await d4.path()) === sha(resumeBuf) && (await xfer(b, 'in', 'resume.bin')).sha === sha(resumeBuf), 'the resumed download\'s SHA-256 matches');
  await waitState(a, 'out', 'resume.bin', 'done');

  // 4b. The link drops once A has sent everything, B still writing (slowly here): B's verdict reaches A on the new link.
  const lateBuf = randomBytes(4 * 512 * 1024 - 5);
  const lateDl = b.waitForEvent('download', WAIT);
  await offer(a, [{ name: 'late-done.bin', mimeType: 'application/octet-stream', buffer: lateBuf }], B);
  await row(b, 'in', 'late-done.bin').locator('[data-act="accept"]').click();
  await b.waitForFunction(() => {
    const t = [...window.share.incoming.values()].find((x) => x.name === 'late-done.bin');
    if (!t?.sink || t.slowed) return !!t?.slowed;
    const write = t.sink.write;
    t.sink.write = async (d) => (await new Promise((r) => setTimeout(r, 500)), write(d));
    return (t.slowed = true);
  }, null, WAIT);
  await a.waitForFunction(() => [...window.share.outgoing.values()].find((x) => x.name === 'late-done.bin')?.endSent, null, WAIT);
  // Every piece has reached B (the end follows them on the same ordered lane), and B is still writing them.
  await b.waitForFunction(() => [...window.share.incoming.values()].find((x) => x.name === 'late-done.bin')?.next === 4, null, WAIT);
  await b.waitForTimeout(200);
  check((await xfer(b, 'in', 'late-done.bin')).have < 4, 'A sent all 4 pieces and the end, and B took them, still writing');
  await a.evaluate((B) => {
    const r = window.share.room;
    r.transport.links.get(r.transport.byPeer.get(B)).dc.close();
    r.endpoint.disconnect(B);
  }, B);
  await b.evaluate((A) => window.share.room.endpoint.disconnect(A), A);
  await waitState(b, 'in', 'late-done.bin', 'done');
  const unknown = await stateOf(a, 'out', 'late-done.bin') === 'sending';
  check(await shaFile(await (await lateDl).path()) === sha(lateBuf), 'B finished and verified it after the link dropped');
  for (const p of [a, b]) await p.evaluate(() => window.share.room.hello());
  await waitState(a, 'out', 'late-done.bin', 'done');
  const late = await xfer(a, 'out', 'late-done.bin');
  check(late.sent === 4 && late.resumes === 0, `A learned it was delivered ${unknown ? 'once they linked again (it asked, and B answered with its verdict)' : 'from B on a link that came back before B finished'}, with no piece sent again`);

  // 5. Cancel: by the receiver (C: nothing on disk), and by the sender (B's download fails).
  await offer(a, [{ name: 'cancel-c.bin', mimeType: 'application/octet-stream', buffer: randomBytes(24 * MiB) }], C);
  await row(c, 'in', 'cancel-c.bin').locator('[data-act="accept"]').click();
  await c.waitForFunction(() => [...window.share.incoming.values()].find((t) => t.name === 'cancel-c.bin')?.have >= 2, null, WAIT);
  await row(c, 'in', 'cancel-c.bin').locator('[data-act="cancel"]').click();
  await waitState(a, 'out', 'cancel-c.bin', 'cancelled');
  const onDisk = await c.evaluate(async () => (await (await (await navigator.storage.getDirectory()).getFileHandle('recv-cancel-c.bin')).getFile()).size);
  check(await stateOf(c, 'in', 'cancel-c.bin') === 'cancelled' && onDisk === 0 && /cancelled/.test(await row(a, 'out', 'cancel-c.bin').locator('.state').textContent()),
    `a receiver's cancel stops the sender, and leaves nothing on disk (${onDisk} B)`);
  const cancelDl = b.waitForEvent('download', WAIT);
  await offer(a, [{ name: 'cancel-b.bin', mimeType: 'application/octet-stream', buffer: randomBytes(24 * MiB) }], B);
  await row(b, 'in', 'cancel-b.bin').locator('[data-act="accept"]').click();
  const d5 = await cancelDl;
  await b.waitForFunction(() => [...window.share.incoming.values()].find((t) => t.name === 'cancel-b.bin')?.have >= 2, null, WAIT);
  await row(a, 'out', 'cancel-b.bin').locator('[data-act="cancel"]').click();
  await waitState(b, 'in', 'cancel-b.bin', 'cancelled');
  check(await d5.failure() !== null, `a sender's cancel ends the receiver's download as failed (${await d5.failure()})`);

  // 6. A hostile peer (A, through its room): names, sizes, floods, and pieces it shouldn't send.
  const taken = (await statsOf(b)).piecesTaken;
  const hostile = await a.evaluate(async ([B]) => {
    const F = await import('./files.js');
    const r = window.share.room;
    const ctl = (m) => r.send(F.encodeControl(m), { to: B });
    const offer = (o) => ({ t: 'offer', id: F.newId(), name: 'x', size: 10, type: '', piece: F.PIECE, ...o });
    const evil = offer({ name: '../../x‮evil.exe', type: 'text/html;"><script>' });
    ctl(evil);
    ctl(offer({ size: -5 }));
    ctl(offer({ piece: 1 }));
    ctl(offer({ name: 'n'.repeat(2000) }));
    const big = r.send(Uint8Array.of(F.CONTROL_FRAME, ...new TextEncoder().encode(JSON.stringify(offer({ name: 'big', pad: 'p'.repeat(5000) })))), { to: B });
    const flood = [];
    for (let i = 0; i < 40; i++) {
      const o = offer({ name: `flood-${i}.bin` });
      flood.push(o.id);
      ctl(o);
    }
    // A piece for a transfer B never accepted.
    const data = new Uint8Array(10);
    r.send(F.encodePiece(evil.id, 0, new Uint8Array(await crypto.subtle.digest('SHA-256', data)), data), { to: B });
    return { evil: evil.id, big, flood };
  }, [B]);
  await b.waitForFunction(() => [...window.share.incoming.values()].filter((t) => /^flood-/.test(t.name)).length > 0, null, WAIT);
  await b.waitForTimeout(500);
  const evilName = await b.evaluate((id) => [...window.share.incoming.values()].find((t) => t.id === id)?.name, hostile.evil);
  check(evilName === '_.._xevil.exe' && await row(b, 'in', '_.._xevil.exe').count() === 1 && await b.$$eval('#transfers .name *', (l) => l.length) === 0,
    `an offer named "../../x\\u202Eevil.exe" shows as "${evilName}", as text`);
  check(await row(b, 'in', '_.._xevil.exe').locator('.info').textContent() === '', 'its odd MIME type shows as none');
  const sB = await statsOf(b);
  check(sB.offersRefused >= 3 + 8 && sB.rejected >= 1 && sB.piecesTaken === taken, `bad offers (size, piece, name length) and the flood past 32 are refused (${sB.offersRefused}); a piece for an offer not accepted is dropped (${sB.rejected})`);
  const floodShown = await b.evaluate(() => [...window.share.incoming.values()].filter((t) => /^flood-/.test(t.name)).length);
  const pending = await b.evaluate((A) => [...window.share.incoming.values()].filter((t) => t.peer === A && t.state === 'offered').length, A);
  check(pending === 32 && floodShown === 31 && !(await b.evaluate(() => [...window.share.incoming.values()].some((t) => t.name === 'big'))),
    `at most 32 offers from one peer wait (${pending}: the evil one and ${floodShown} of 40 floods); a control message over 4 KB isn't read`);
  // B accepts the evil offer; A then sends a piece out of order, and one with a bad hash.
  const evilDl = b.waitForEvent('download', WAIT).catch(() => null);
  await row(b, 'in', '_.._xevil.exe').locator('[data-act="accept"]').click();
  await waitState(b, 'in', '_.._xevil.exe', 'receiving');
  await a.evaluate(async ([B, id]) => {
    const F = await import('./files.js');
    const r = window.share.room;
    const data = new Uint8Array(10).fill(7);
    const h = new Uint8Array(await crypto.subtle.digest('SHA-256', data));
    r.send(F.encodePiece(id, 1, h, data), { to: B }); // out of range (10 bytes is one piece)
    r.send(F.encodePiece(id, 0, h, data.subarray(0, 9)), { to: B }); // the wrong length
    h[0] ^= 1;
    r.send(F.encodePiece(id, 0, h, data), { to: B }); // a bad hash
  }, [B, hostile.evil]);
  await waitState(b, 'in', '_.._xevil.exe', 'failed');
  const ev = await xfer(b, 'in', '_.._xevil.exe');
  check(ev.have === 0 && /piece 1 failed its SHA-256 check/.test(ev.note), `pieces out of range or of the wrong length are refused, and one with a bad hash fails the file: "${ev.note}"`);
  const evd = await evilDl;
  check(evd === null || await evd.failure() !== null, 'and nothing of it is saved (its download failed)');

  // 7. Direct only: D's WebRTC fails, so its links go over the fallback.
  const d = await open(ctx, 'D', 'save=memory&rtc=off');
  const D = await self(d);
  await a.waitForFunction((D) => document.querySelector(`#peers li[data-peer="${D}"]`)?.dataset.route === 'relay' &&
    window.share.room.route(D) !== null, D, { timeout: 60000 });
  await d.waitForFunction((A) => window.share.room.route(A) !== null, A, { timeout: 60000 });
  const routeD = await a.evaluate((D) => window.share.room.route(D).kind, D);
  check(['signaling', 'peer'].includes(routeD) && /via (relay|peer)/.test(await a.$eval(`#peers li[data-peer="${D}"] .pill`, (e) => e.textContent)),
    `D is linked over the fallback (${routeD}) and shown so`);
  check(await d.isVisible('#direct-hint') && /microphone/.test(await d.$eval('#direct-hint', (e) => e.textContent)), 'D shows the hint about direct links and the microphone');
  check(/kept in memory/.test(await d.$eval('#save-mode', (e) => e.textContent)), 'D says it keeps files in memory, up to a size');
  const directBuf = randomBytes(3 * MiB + 17);
  await offer(a, [{ name: 'direct-only.bin', mimeType: 'application/octet-stream', buffer: directBuf }], D);
  await row(d, 'in', 'direct-only.bin').locator('[data-act="accept"]').click();
  await waitState(d, 'in', 'direct-only.bin', 'receiving');
  await a.waitForTimeout(3000);
  const held = await xfer(a, 'out', 'direct-only.bin');
  check(held.sent === 0 && (await xfer(d, 'in', 'direct-only.bin')).have === 0 &&
    /waiting for a direct link/.test(await row(a, 'out', 'direct-only.bin').locator('.state').textContent()) &&
    /waiting for a direct link/.test(await row(d, 'in', 'direct-only.bin').locator('.state').textContent()),
    'accepted over the fallback, no piece goes: both sides wait for a direct link');
  // Too large for memory: a 300 MB offer can't be accepted on D.
  await a.evaluate(async ([D]) => {
    const F = await import('./files.js');
    window.share.room.send(F.encodeControl(F.makeOffer({ id: F.newId(), name: 'too-big.bin', size: 300e6 })), { to: D });
  }, [D]);
  await d.waitForFunction(() => [...window.share.incoming.values()].some((t) => t.name === 'too-big.bin'), null, WAIT);
  check(await row(d, 'in', 'too-big.bin').locator('[data-act="accept"]').isDisabled() &&
    /too large to receive in this browser/.test(await row(d, 'in', 'too-big.bin').locator('.state').textContent()), 'in memory, a 300 MB file can\'t be accepted, and says why');
  // WebRTC works again: a retry links A and D directly, and the file goes.
  const memDl = d.waitForEvent('download', { timeout: 90000 });
  await d.evaluate(() => (window.share.rtcOff = false));
  for (const [p, peer] of [[d, A], [a, D]]) await p.evaluate((peer) => window.share.room.retryDirect(peer), peer);
  await a.waitForFunction((D) => document.querySelector(`#peers li[data-peer="${D}"]`)?.dataset.route === 'direct', D, { timeout: 90000 });
  await waitState(d, 'in', 'direct-only.bin', 'done', 60000);
  const md = await memDl;
  check(await shaFile(await md.path()) === sha(directBuf), 'once the retry linked them directly, the file went, its SHA-256 matching (kept in memory, then saved)');
  check(await d.evaluate(() => window.share.stats.piecesTaken) === Math.ceil(directBuf.length / (512 * 1024)), 'D took exactly the file\'s pieces, all over the direct link');

  // A peer that leaves the room (its page closed) won't come back as itself: what it was offered ends at once.
  await offer(a, [{ name: 'gone.bin', mimeType: 'application/octet-stream', buffer: randomBytes(1000) }], D);
  await d.waitForFunction(() => [...window.share.incoming.values()].some((t) => t.name === 'gone.bin'), null, WAIT);
  await d.evaluate(() => window.share.room.leave());
  await waitState(a, 'out', 'gone.bin', 'cancelled');
  check(/left the room/.test(await row(a, 'out', 'gone.bin').locator('.state').textContent()), 'an offer to a peer that left the room ends at once, and says so');

  if (process.argv[2] && !process.argv[2].startsWith('--')) await a.screenshot({ path: path.join(process.argv[2], 'share.png') });
}

// One big file, A to B, timed, with each tab's peak memory.
async function big(ctx) {
  const mb = Number(BIG) || 1024;
  let file = arg('file');
  let want;
  if (!file) {
    file = path.join(tmp, `big-${mb}MB.bin`);
    const h = createHash('sha256');
    const fd = fs.openSync(file, 'w');
    for (let left = mb * 1e6; left > 0;) {
      const chunk = randomBytes(Math.min(left, 16 * MiB));
      h.update(chunk);
      fs.writeSync(fd, chunk);
      left -= chunk.length;
    }
    fs.closeSync(fd);
    want = h.digest('hex');
  } else want = await shaFile(file);
  const size = fs.statSync(file).size;
  console.log(`     ${file}: ${size} B, SHA-256 ${want}`);
  const a = await open(ctx, 'A', 'save=download');
  const b = await open(ctx, 'B', 'save=download');
  await a.waitForSelector('#peers li[data-route="direct"]', WAIT);
  await b.waitForSelector('#peers li[data-route="direct"]', WAIT);
  const cdp = await Promise.all([a, b].map(async (p) => {
    const s = await ctx.newCDPSession(p);
    await s.send('Performance.enable');
    return s;
  }));
  const peak = { heapA: 0, heapB: 0, rss: new Map(), sumRss: 0 };
  const sample = async () => {
    const [ma, mb2] = await Promise.all(cdp.map((s) => s.send('Performance.getMetrics')));
    const heap = (m) => m.metrics.find((x) => x.name === 'JSHeapUsedSize').value;
    peak.heapA = Math.max(peak.heapA, heap(ma));
    peak.heapB = Math.max(peak.heapB, heap(mb2));
    const rss = browserRss();
    let sum = 0;
    for (const [pid, r] of rss) {
      sum += r.rss;
      const was = peak.rss.get(pid);
      if (!was || r.rss > was.rss) peak.rss.set(pid, r);
    }
    peak.sumRss = Math.max(peak.sumRss, sum);
  };
  await sample();
  const startRss = new Map([...peak.rss].map(([pid, r]) => [pid, r.rss]));
  const name = path.basename(file);
  await offer(a, file);
  const dl = b.waitForEvent('download', WAIT);
  const t0 = Date.now();
  await row(b, 'in', name).locator('[data-act="accept"]').click();
  const d = await dl;
  let lastLog = 0;
  for (;;) {
    await sample();
    const s = await stateOf(b, 'in', name);
    if (s !== 'receiving') break;
    if (Date.now() - lastLog > 15000) {
      lastLog = Date.now();
      console.log(`     ${((Date.now() - t0) / 1000).toFixed(0)} s: B ${await row(b, 'in', name).locator('.info').textContent()}`);
    }
    await b.waitForTimeout(1000);
  }
  const secs = (Date.now() - t0) / 1000;
  await waitState(a, 'out', name, 'done');
  const got = await shaFile(await d.path());
  const inB = await xfer(b, 'in', name);
  check(await stateOf(b, 'in', name) === 'done' && inB.sha === want && got === want, `${size} B arrived; B's SHA-256 and the download's match the file's (${got})`);
  console.log(`     ${(size / 1e6).toFixed(0)} MB in ${secs.toFixed(1)} s from the accept: ${(size / 1e6 / secs).toFixed(2)} MB/s`);
  console.log(`     B's row: ${await row(b, 'in', name).locator('.info').textContent()}`);
  console.log(`     peak JS heap: A ${(peak.heapA / 1e6).toFixed(1)} MB, B ${(peak.heapB / 1e6).toFixed(1)} MB`);
  for (const [pid, r] of [...peak.rss].sort((x, y) => y[1].rss - x[1].rss)) {
    console.log(`     peak RSS ${r.type} ${pid}: ${(r.rss / 1024).toFixed(0)} MB (at the start ${((startRss.get(pid) ?? 0) / 1024).toFixed(0)} MB)`);
  }
  console.log(`     peak RSS of the whole browser: ${(peak.sumRss / 1024).toFixed(0)} MB`);
  check(peak.heapA < 200e6 && peak.heapB < 200e6, 'neither tab\'s JS heap held the file (under 200 MB each)');
  await d.delete();
}
