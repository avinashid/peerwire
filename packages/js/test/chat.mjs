// Opens demo/chat.html in tabs of headless Chromium (?signal=local, loopback only) and checks the chat: members
// and their routes, nicknames kept in presence and localStorage, the typing indicator and its expiry, messages
// shown as text only, frames that aren't chat messages (or are too long) dropped, leaving and New room.
//
//   node packages/js/test/chat.mjs [screenshot dir]

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const shots = process.argv[2];
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const port = 8765 + Math.floor(Math.random() * 1000);
const server = spawn('python3', ['-m', 'http.server', String(port), '--bind', '127.0.0.1'], { cwd: root, stdio: 'ignore' });
const url = `http://127.0.0.1:${port}/demo/chat.html?signal=local#chat-test-${port}`;
const WAIT = { timeout: 45000 }; // a loaded host links tabs slowly
let browser;
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const errors = [];
const watch = (p, name) => {
  p.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  p.on('dialog', (d) => (errors.push(`${name}: dialog ${d.message()}`), d.dismiss()));
};
const lastBubble = (p) => p.$$eval('#messages .msg .bubble', (l) => l.at(-1)?.textContent);
const typingText = (p) => p.$eval('#typing', (t) => t.textContent);

try {
  await new Promise((r) => setTimeout(r, 600));
  // Tabs of one browser on one machine link over host candidates; headless Chromium can't resolve mDNS ones.
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const a = await ctx.newPage();
  watch(a, 'A');
  await a.goto(url);
  await a.waitForSelector('#welcome:not(.hidden)');
  if (shots) await a.screenshot({ path: `${shots}/chat-welcome.png` });
  // Markup and a bidi override in a nickname: the override goes, the markup stays text.
  await a.fill('#name', '<b>Ada</b>‮');
  await a.click('#go');
  await a.waitForFunction(() => window.chat?.room);
  check(await a.$eval('#me', (i) => i.value) === '<b>Ada</b>', 'a nickname loses its bidi override and keeps its markup as text');

  const b = await ctx.newPage();
  watch(b, 'B');
  await b.goto(url);
  await b.waitForFunction(() => window.chat?.room);
  check(await b.$eval('#welcome', (w) => w.classList.contains('hidden')), 'a tab with a remembered nickname joins without asking');
  await b.fill('#me', 'Grace');
  await b.press('#me', 'Tab');
  check(await b.evaluate(() => localStorage.getItem('peerwire-chat/name')) === 'Grace', 'the nickname is remembered in localStorage');

  const t0 = Date.now();
  await a.waitForSelector('#members li[data-route="direct"]', { ...WAIT, state: 'attached' });
  await b.waitForSelector('#members li[data-route="direct"]', { ...WAIT, state: 'attached' });
  console.log(`     the tabs linked directly in ${Date.now() - t0} ms`);
  await a.waitForFunction(() => document.querySelector('#members li[data-route="direct"] .name')?.textContent === 'Grace', null, WAIT);
  check(true, 'A lists Grace (her new nickname, from presence) on a direct route');
  check(await b.$eval('#members li[data-route="direct"] .name', (n) => n.textContent) === '<b>Ada</b>', 'B lists Ada\'s nickname as text');
  check(await b.$$eval('#members .name > :not(.dim), .meta .name *', (l) => l.length) === 0, 'no element came from a nickname');
  check(/direct link( · \d+ ms)?/.test(await a.$eval('#members li[data-route="direct"] .route', (r) => r.textContent)), 'the member\'s route reads "direct link"');

  // Typing: one keystroke sends one typing frame, which lasts 3 s.
  await b.type('#text', 'h');
  await a.waitForFunction(() => document.getElementById('typing').textContent === 'Grace is typing…', null, { timeout: 10000 });
  const shown = Date.now();
  await a.waitForFunction(() => document.getElementById('typing').textContent === '', null, { timeout: 10000 });
  const lasted = Date.now() - shown;
  check(lasted > 2000 && lasted < 4500, `the typing indicator went after ${lasted} ms (3 s after the frame)`);

  // A message with markup arrives as text, and ends the sender's typing indicator.
  const html = '<img src=x onerror="window.pwned=1"><b>bold</b> & <script>window.pwned=2</script>';
  await b.fill('#text', '');
  await b.type('#text', 'x');
  await a.waitForFunction(() => document.getElementById('typing').textContent !== '', null, { timeout: 10000 });
  await b.fill('#text', html);
  await b.press('#text', 'Enter');
  await a.waitForFunction(() => document.querySelectorAll('#messages .msg').length === 1, null, WAIT);
  check(await lastBubble(a) === html, 'a message with markup shows as its exact text');
  check(await a.$$eval('#messages img, #messages script, .bubble *', (l) => l.length) === 0, 'no element came from a message');
  await a.waitForTimeout(300);
  check(await a.evaluate(() => window.pwned) === undefined, 'nothing in a message ran');
  check(await typingText(a) === '', 'a message ends its sender\'s typing indicator');
  check(await b.$$eval('#messages .msg', (l) => l.at(-1)?.classList.contains('mine')) && await lastBubble(b) === html, 'the sender sees its own message');
  check(await b.$eval('#text', (t) => t.maxLength) === 2000, 'the composer takes at most 2000 characters');

  // Typing counts on the unreliable lane only. The presence change after it, on the same ordered reliable
  // lane, shows when A has handled it.
  await b.evaluate(() => {
    window.chat.room.send({ t: 'typing' });
    window.chat.room.setPresence({ name: 'Grace H' });
  });
  await a.waitForFunction(() => document.querySelector('#members li[data-route="direct"] .name')?.textContent === 'Grace H', null, WAIT);
  check(await typingText(a) === '', 'a typing frame on the reliable lane is ignored');
  await b.evaluate(() => window.chat.room.setPresence({ name: 'Grace' }));
  await a.waitForFunction(() => document.querySelector('#members li[data-route="direct"] .name')?.textContent === 'Grace', null, WAIT);

  // What a member may send that isn't a chat message: none of it shows.
  await b.evaluate(() => {
    const r = window.chat.room;
    r.send({ t: 'msg', text: 'x'.repeat(2001) });
    r.send({ t: 'msg', text: 42 });
    r.send({ t: 'msg', text: '   ' });
    r.send({ t: 'msg' });
    r.send({ text: 'no type' });
    r.send('a plain string');
    r.send(null);
    r.send([1, 2]);
    r.send(new Uint8Array([1, 2, 3]));
    r.send({ t: 'msg', text: 'y'.repeat(2000), name: 17 });
  });
  await a.waitForFunction(() => document.querySelectorAll('#messages .msg').length >= 2, null, WAIT);
  await a.waitForTimeout(1000);
  check(await a.$$eval('#messages .msg', (l) => l.length) === 2, 'frames that aren\'t chat messages, or are over 2000 characters, are dropped');
  check(await lastBubble(a) === 'y'.repeat(2000), 'a message of exactly 2000 characters shows');

  await a.fill('#text', 'Exactly. And it all goes peer to peer.');
  await a.press('#text', 'Enter');
  await b.waitForFunction(() => document.querySelectorAll('#messages .msg').length === 2, null, WAIT);
  check(await lastBubble(b) === 'Exactly. And it all goes peer to peer.', 'Ada\'s message reaches Grace');
  check(await b.$$eval('#messages .msg .meta .name', (l) => l.at(-1)?.textContent) === '<b>Ada</b>', 'it is signed with her nickname, as text');

  // A reload keeps the nickname, and the others see the tab leave and come back.
  await b.reload();
  await a.waitForFunction(() => [...document.querySelectorAll('#messages .sys')].some((s) => s.textContent === 'Grace left'), null, WAIT);
  check(true, 'A saw Grace leave');
  await b.waitForFunction(() => window.chat?.room, null, WAIT);
  check(await b.$eval('#me', (i) => i.value) === 'Grace' && await b.$eval('#welcome', (w) => w.classList.contains('hidden')), 'after a reload the tab rejoins as Grace');
  await a.waitForFunction(() => document.querySelector('#members li[data-route="direct"] .name')?.textContent === 'Grace', null, WAIT);
  check(true, 'A lists Grace again once she is back');

  if (shots) {
    await b.fill('#text', 'Back. Reloading kept my nickname.');
    await b.press('#text', 'Enter');
    await a.fill('#text', 'Welcome back! Messages from before you joined don’t show yet.');
    await a.press('#text', 'Enter');
    await b.waitForFunction(() => document.querySelectorAll('#messages .msg').length === 2, null, WAIT);
    await a.type('#text', 'One more', { delay: 20 });
    await b.waitForFunction(() => document.getElementById('typing').textContent !== '', null, { timeout: 10000 });
    await a.screenshot({ path: `${shots}/chat-desktop.png` });
    await b.setViewportSize({ width: 390, height: 844 });
    await b.waitForTimeout(300);
    await b.screenshot({ path: `${shots}/chat-mobile.png` });
    await b.click('#people');
    await b.waitForTimeout(400);
    await b.screenshot({ path: `${shots}/chat-mobile-members.png` });
    await b.click('#backdrop', { position: { x: 20, y: 400 } });
    await b.setViewportSize({ width: 1280, height: 800 });
  }

  // New room: a fresh random room, its own invite link, and the old room sees us go.
  const before = await a.evaluate(() => location.hash);
  await a.click('#new');
  await a.waitForFunction((h) => location.hash !== h && window.chat.room && window.chat.roomId === location.hash.slice(1), before, WAIT);
  const id = await a.evaluate(() => window.chat.roomId);
  check(/^[A-Za-z0-9_-]{22}$/.test(id), `New room made a random room id (${id.length} characters)`);
  check(await a.$eval('#link', (l) => l.value) === `http://127.0.0.1:${port}/demo/chat.html?signal=local#${id}`, 'the invite link is for the new room');
  check(await a.$$eval('#messages .msg', (l) => l.length) === 0 && await a.$eval('#empty', (e) => !e.classList.contains('hidden')), 'the new room starts empty, with the invite prompt');
  await b.waitForFunction(() => document.querySelectorAll('#members li').length === 1, null, WAIT);
  check(true, 'Grace sees Ada leave the old room');
  if (shots) await a.screenshot({ path: `${shots}/chat-empty.png` });

  check(errors.length === 0, `no page errors${errors.length ? `: ${errors.join('; ')}` : ''}`);
} catch (e) {
  console.log(`FAIL ${e.message.split('\n')[0]}`);
  failed++;
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `${failed} failed` : 'all passed');
process.exit(failed ? 1 : 0);
