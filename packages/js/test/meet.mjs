// Opens demo/meet.html in tabs of headless Chromium with its fake camera and microphone (?signal=local, loopback
// only) and checks the call: no camera before a click, a refused one explained, each tab's tiles for the others with
// their video playing, mute and camera-off shown on the other tabs, leave removing the tile, names as text only, a
// full call, and the grid on a phone.
//
//   node packages/js/test/meet.mjs [tabs: 2 to 4, default 3] [screenshot dir]

import { serve } from './serve.mjs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const N = Math.min(4, Math.max(2, Number(process.argv[2]) || 3));
const shots = process.argv[3];
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const { port, server } = await serve(root);
const base = `http://127.0.0.1:${port}/demo/meet.html?signal=local`;
const WAIT = { timeout: 60000 }; // a loaded host links tabs, and starts their video, slowly
const NAMES = ['Ada', 'Grace', 'Linus', 'Hedy'];
let browser;
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const errors = [];
const shot = (p, name) => shots && p.screenshot({ path: `${shots}/${name}.png` });

// Counts getUserMedia calls, so the test can tell the page asked for nothing before a click.
const COUNT_GUM = () => {
  const md = navigator.mediaDevices;
  const real = md.getUserMedia.bind(md);
  window.gumCalls = 0;
  md.getUserMedia = (c) => (window.gumCalls++, real(c));
};
const DENY_GUM = () => {
  navigator.mediaDevices.getUserMedia = async () => { throw new DOMException('Permission denied', 'NotAllowedError'); };
};
// Holds every getUserMedia until the test calls releaseGum(), as a prompt nobody has answered yet would.
const HELD_GUM = () => {
  const md = navigator.mediaDevices;
  const ask = md.getUserMedia.bind(md);
  let release;
  const held = new Promise((r) => (release = r));
  window.releaseGum = release;
  window.gumTracks = [];
  md.getUserMedia = async (c) => {
    await held;
    const s = await ask(c);
    window.gumTracks.push(...s.getTracks());
    return s;
  };
};

async function open(ctx, name, room, { query = '', viewport, init } = {}) {
  const p = await ctx.newPage();
  if (viewport) await p.setViewportSize(viewport);
  p.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  p.on('dialog', (d) => (errors.push(`${name}: dialog ${d.message()}`), d.dismiss()));
  await p.addInitScript(COUNT_GUM);
  if (init) await p.addInitScript(init);
  await p.goto(`${base}${query}#${room}`);
  await p.waitForSelector('#lobby:not(.hidden)');
  return p;
}
async function joinCall(p, name, { media = true } = {}) {
  if (media) {
    await p.click('#allow');
    await p.waitForFunction(() => document.getElementById('preview').videoWidth > 0, null, WAIT);
  }
  await p.fill('#name', name);
  await p.click('#join');
  await p.waitForFunction(() => window.meet?.call, null, WAIT);
}
// The other tabs' tiles in `p`, once each is connected and its video has frames: [name, frames played] each.
const remoteTiles = (p) => p.$$eval('.tile:not(.self)', (l) => l.map((t) => [t.querySelector('.name').textContent, t.dataset.state, t.querySelector('video').getVideoPlaybackQuality().totalVideoFrames]));
const tileOf = (p, name) => `.tile:not(.self):has(.name:text-is("${name}"))`;

try {
  browser = await chromium.launch({
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--disable-features=WebRtcHideLocalIpsWithMdns'],
  });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const room = `meet-test-${port}`;

  // The lobby asks for nothing until a click.
  const tabs = [await open(ctx, 'A', room)];
  const a = tabs[0];
  await a.waitForTimeout(500);
  check(await a.evaluate(() => window.gumCalls) === 0, 'the lobby asks for no camera or microphone before a click');
  check(await a.$eval('#preview-off', (e) => !e.classList.contains('hidden')), 'the preview shows the camera off');
  await shot(a, 'meet-lobby');
  await a.click('#allow');
  await a.waitForFunction(() => document.getElementById('preview').videoWidth > 0, null, WAIT);
  check(await a.evaluate(() => window.gumCalls) === 1, 'one click asks once, for both the camera and the microphone');
  check(await a.evaluate(() => window.meet.stream.getTracks().map((t) => t.kind).sort().join()) === 'audio,video', 'the preview has a camera and a microphone track');
  await a.fill('#name', NAMES[0]);
  await shot(a, 'meet-lobby-preview');
  await a.click('#join');
  await a.waitForFunction(() => window.meet?.call, null, WAIT);
  check(await a.$eval('#banner', (b) => b.textContent.includes('only one here')), 'alone, the call says to send the invite link');

  for (let i = 1; i < N; i++) {
    const p = await open(ctx, String.fromCharCode(65 + i), room);
    tabs.push(p);
    await joinCall(p, NAMES[i]);
  }

  // Every tab: a tile for each other tab, connected, with its video playing (frames going up).
  const t0 = Date.now();
  for (const p of tabs) {
    await p.waitForFunction((n) => {
      const ts = [...document.querySelectorAll('.tile:not(.self)')];
      return ts.length === n && ts.every((t) => t.dataset.state === 'connected' && t.querySelector('video').videoWidth > 0);
    }, N - 1, WAIT).catch(async (e) => {
      for (const [i, q] of tabs.entries()) console.log(`     ${NAMES[i]}: ${JSON.stringify(await q.evaluate(() => [window.meet.room?.peers.length, window.meet.tiles(), [...document.querySelectorAll('.tile video')].map((v) => v.videoWidth)]))}`);
      throw e;
    });
  }
  console.log(`     ${N} tabs connected and showing video in ${Date.now() - t0} ms`);
  for (const [i, p] of tabs.entries()) {
    const before = await remoteTiles(p);
    await p.waitForTimeout(1000);
    const after = await remoteTiles(p);
    const names = after.map(([n]) => n).sort().join();
    check(names === NAMES.slice(0, N).filter((_, j) => j !== i).sort().join(), `tab ${NAMES[i]} shows the others' tiles by name (${names})`);
    check(after.every(([, s, f], j) => s === 'connected' && f > before[j][2]), `tab ${NAMES[i]}: every remote video is playing (frames ${after.map(([, , f], j) => f - before[j][2]).join(', ')} in 1 s)`);
    check(await p.$eval('#unmute', (b) => b.classList.contains('hidden')), `tab ${NAMES[i]} could play the call's sound (no "click to hear")`);
    check(await p.$eval('#count-pill', (e) => e.textContent) === `${N} of 6`, `tab ${NAMES[i]} counts ${N} of 6`);
  }
  check(await a.$eval('#banner', (b) => b.classList.contains('hidden')), 'with others in the call, the invite banner goes');
  await shot(a, `meet-call-${N}`);

  // Mute on B shows on every other tab; camera off on the last tab shows its initial instead of its video.
  const b = tabs[1];
  const last = tabs[N - 1];
  await b.click('#mic');
  check(await b.evaluate(() => window.meet.stream.getAudioTracks()[0].enabled) === false, 'mute disables the microphone track');
  for (const p of tabs.filter((p) => p !== b)) await p.waitForSelector(`${tileOf(p, NAMES[1])}.nomic`, WAIT);
  check(true, `${NAMES[1]}'s mute shows on the other tabs`);
  await last.click('#cam');
  check(await last.evaluate(() => window.meet.stream.getVideoTracks().length) === 0, 'camera off stops the camera track (its light goes off)');
  for (const p of tabs.filter((p) => p !== last)) await p.waitForSelector(`${tileOf(p, NAMES[N - 1])}.nocam`, WAIT);
  check(await a.$eval(`${tileOf(a, NAMES[N - 1])} .face .avatar`, (e) => e.textContent) === NAMES[N - 1][0], `${NAMES[N - 1]}'s camera-off shows the initial on the other tabs`);
  await shot(a, 'meet-muted-camoff');
  await last.click('#cam');
  await a.waitForSelector(`${tileOf(a, NAMES[N - 1])}:not(.nocam)`, WAIT);
  await a.waitForFunction((n) => [...document.querySelectorAll('.tile:not(.self)')].find((t) => t.querySelector('.name').textContent === n)?.querySelector('video').videoWidth > 0, NAMES[N - 1], WAIT);
  check(true, 'camera back on shows the video again');
  await last.waitForFunction(() => document.querySelector('.tile.self video').videoWidth > 0 && !document.querySelector('.tile.self').classList.contains('nocam'), null, WAIT);
  check(true, 'and on the tab\'s own tile');
  await b.click('#mic');
  await a.waitForSelector(`${tileOf(a, NAMES[1])}:not(.nomic)`, WAIT);
  check(true, 'unmute shows on the other tabs');
  // A member whose presence says muted never gets the speaking ring, even while its audio still plays (as in the
  // moment before a muted microphone goes quiet). Here it sends a steady tone: the fake microphone's beeps fade
  // under the browser's noise suppression, the tone doesn't. It is heard (ringed) first, then says it is muted: the
  // ring goes with the mute mark, not up to a speaking() tick later.
  await b.evaluate(async () => {
    const ctx = new AudioContext();
    const tone = ctx.createOscillator();
    const out = ctx.createMediaStreamDestination();
    tone.connect(out);
    tone.start();
    await window.meet.call.setStream(new MediaStream([out.stream.getAudioTracks()[0], ...window.meet.stream.getVideoTracks()]));
  });
  await a.waitForSelector(`${tileOf(a, NAMES[1])}.speaking`, WAIT);
  await a.evaluate((name) => {
    const tile = [...document.querySelectorAll('.tile:not(.self)')].find((t) => t.querySelector('.name').textContent === name);
    window.mutedRinged = 0;
    new MutationObserver(() => tile.classList.contains('nomic') && tile.classList.contains('speaking') && window.mutedRinged++)
      .observe(tile, { attributes: true, attributeFilter: ['class'] });
  }, NAMES[1]);
  await b.evaluate(() => window.meet.room.setPresence({ ...window.meet.room.getPresence(), mic: false }));
  await a.waitForSelector(`${tileOf(a, NAMES[1])}.nomic`, WAIT);
  const rang = await a.evaluate(async (name) => {
    const tile = [...document.querySelectorAll('.tile:not(.self)')].find((t) => t.querySelector('.name').textContent === name);
    let n = 0;
    for (let i = 0; i < 25; i++, await new Promise((r) => setTimeout(r, 100))) if (tile.classList.contains('speaking')) n++;
    return { atMute: window.mutedRinged, samples: n };
  }, NAMES[1]);
  check(rang.atMute === 0 && rang.samples === 0, `a member muted in presence loses its speaking ring with the mute mark, and isn't ringed while its audio plays (ringed and muted together ${rang.atMute} times; ringed in ${rang.samples} of 25 samples after)`);
  await b.evaluate(() => window.meet.call.setStream(window.meet.stream));
  await b.click('#mic');
  await b.click('#mic');
  await a.waitForSelector(`${tileOf(a, NAMES[1])}:not(.nomic)`, WAIT);

  // Leave: the tile goes everywhere else, and the leaver's camera and microphone go off.
  await last.click('#leave');
  check(await last.$eval('#left', (e) => !e.classList.contains('hidden')), 'leaving shows "You left the call"');
  check(await last.evaluate(() => window.meet.stream.getTracks().length) === 0, 'leaving turns the camera and microphone off');
  for (const p of tabs.slice(0, -1)) await p.waitForFunction((n) => document.querySelectorAll('.tile:not(.self)').length === n, N - 2, WAIT);
  check(true, 'the leaver\'s tile goes from every other tab');
  await shot(last, 'meet-left');
  for (const p of tabs) await p.close();

  // Names are text: markup and a bidi override in one.
  const room2 = `meet-names-${port}`;
  const x = await open(ctx, 'X', room2);
  await joinCall(x, '<img src=x onerror=pwned=1>‮', { media: false });
  const y = await open(ctx, 'Y', room2);
  await joinCall(y, 'Yan', { media: false });
  await y.waitForSelector('.tile:not(.self)', WAIT);
  check(await y.$eval('.tile:not(.self) .name', (n) => n.textContent) === '<img src=x onerror=pwned=1>', 'a name with markup shows as its text, without its bidi override');
  check(await y.$$eval('.tile .name *, .tile img', (l) => l.length) === 0 && await y.evaluate(() => window.pwned) === undefined, 'no element came from a name, and nothing in it ran');
  check(await y.$eval('.tile:not(.self)', (t) => t.classList.contains('nocam')), 'a member with no camera shows its initial');
  // A member that leaves the call but stays in the room (another app on the same appId, say): its tile goes too.
  await x.evaluate(() => window.meet.call.leave());
  await y.waitForFunction(() => document.querySelectorAll('.tile:not(.self)').length === 0, null, WAIT);
  check(await y.evaluate(() => window.meet.room.peers.length) === 1, 'a member that leaves only the call loses its tile, and stays in the room');
  for (const p of [x, y]) await p.close();

  // A full call: two tabs in a call of at most 2, then a third.
  const room3 = `meet-full-${port}`;
  const f = [];
  for (const [i, n] of ['Ann', 'Bob'].entries()) {
    f.push(await open(ctx, `F${i}`, room3, { query: '&max=2' }));
    await joinCall(f[i], n, { media: false });
  }
  // Both seated (linked to each other) before the third calls: a member whose links still connect may give way.
  for (const p of f) await p.waitForSelector('.tile:not(.self)[data-state="connected"]', WAIT);
  const third = await open(ctx, 'F2', room3, { query: '&max=2' });
  await joinCall(third, 'Cy', { media: false });
  await third.waitForSelector('#full:not(.hidden)', WAIT);
  check(true, 'a third member of a call of at most 2 is told the call is full');
  check(await third.$eval('#lobby', (l) => !l.classList.contains('hidden')), 'and is back in the lobby');
  await shot(third, 'meet-full');
  await third.waitForTimeout(1500);
  check(await f[0].$$eval('.tile:not(.self)', (l) => l.length) === 1, 'the call keeps its 2 members');
  for (const p of [...f, third]) await p.close();

  // A refused camera is explained, and joining still works.
  const d = await ctx.newPage();
  d.on('pageerror', (e) => errors.push(`D: ${e.message}`));
  await d.addInitScript(DENY_GUM);
  await d.goto(`${base}#meet-denied-${port}`);
  await d.click('#allow');
  await d.waitForSelector('#media-msg.bad');
  check(/blocked the camera and microphone.*site settings/.test(await d.$eval('#media-msg', (m) => m.textContent)), 'a refused camera and microphone get a message saying how to allow them');
  await shot(d, 'meet-denied');
  await d.close();

  // A camera asked for and not yet given at Leave: it goes off as it comes, and Rejoin doesn't send it.
  const h = await open(ctx, 'H', `meet-held-${port}`, { init: HELD_GUM });
  await joinCall(h, 'Hal', { media: false });
  await h.click('#cam');
  await h.click('#leave');
  await h.evaluate(() => window.releaseGum());
  await h.waitForFunction(() => window.gumTracks.length > 0 && !document.getElementById('cam').disabled, null, WAIT);
  const late = await h.evaluate(() => ({
    sheet: !document.getElementById('left').classList.contains('hidden'),
    tracks: window.meet.stream.getTracks().length,
    live: window.gumTracks.filter((t) => t.readyState === 'live').length,
    preview: document.getElementById('preview').srcObject !== null,
  }));
  check(late.sheet && late.tracks === 0 && late.live === 0 && !late.preview, `a camera given after Leave goes straight off, with no preview behind "You left the call" (${JSON.stringify(late)})`);
  await shot(h, 'meet-left-late-camera');
  await h.click('#rejoin');
  await h.waitForFunction(() => window.meet?.call, null, WAIT);
  check(await h.evaluate(() => window.meet.stream.getTracks().length === 0 && window.gumCalls === 1), 'Rejoin sends no camera, and asks for none: it was off at Leave');
  await h.close();

  // On a phone held upright, two people stack, and the controls fit.
  const room4 = `meet-phone-${port}`;
  const ph = await open(ctx, 'P', room4, { viewport: { width: 390, height: 844 } });
  await joinCall(ph, 'Pat');
  const q = await open(ctx, 'Q', room4);
  await joinCall(q, 'Quinn');
  await ph.waitForFunction(() => document.querySelector('.tile:not(.self)')?.querySelector('video').videoWidth > 0, null, WAIT);
  const boxes = await ph.$$eval('.tile', (l) => l.map((t) => t.getBoundingClientRect().toJSON()));
  check(boxes.length === 2 && boxes[1].top >= boxes[0].bottom && boxes.every((r) => r.width > 300), `on a phone two tiles stack, full width (${boxes.map((r) => `${Math.round(r.width)}×${Math.round(r.height)}`).join(', ')})`);
  const dock = await ph.$eval('#dock', (e) => e.getBoundingClientRect().toJSON());
  check(dock.right <= 390 && dock.bottom <= 844, 'the controls fit the phone\'s screen');
  await shot(ph, 'meet-phone');
  for (const p of [ph, q]) await p.close();
} catch (e) {
  console.log(`FAIL ${e.stack ?? e}`);
  failed++;
} finally {
  await browser?.close();
  server.kill();
}
for (const e of errors) console.log(`FAIL page error: ${e}`);
failed += errors.length;
console.log(failed ? `${failed} failed` : 'meet: all ok');
process.exit(failed ? 1 : 0);
