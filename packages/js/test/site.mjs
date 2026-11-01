// The site (NB-016): builds it (scripts/site/build.mjs) into a temporary directory, serves it under /peerwire/ as
// GitHub Pages will (loopback only), and checks in headless Chromium:
//  - every page is reachable from the landing page, and every link resolves: each page's relative href and src is
//    fetched (no script runs, so the demos that default to Nostr dial no public relay), none answers an error, and
//    each #fragment names an id on its page;
//  - the landing page and two docs load with no request to another host, styled light or dark as the system asks;
//  - the code tabs switch with no script, the docs' menu opens on a phone, and no page is wider than a phone;
//  - the first site's URLs land on their new pages;
//  - each demo loads its modules, the wasm and nothing else from assets/<id>/, and reaches ready over local
//    signalling (BroadcastChannel), with no request to another host;
//  - two chat tabs link to each other on a direct route;
//  - no page logs a Content-Security-Policy violation or throws.
//
//   node packages/js/test/site.mjs [--shots DIR]

import { serve } from './serve.mjs';
import { buildSite } from '../../../scripts/site/build.mjs';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../..');
const k = process.argv.indexOf('--shots');
const shots = k > 0 ? path.resolve(process.argv[k + 1]) : null;
if (shots) fs.mkdirSync(shots, { recursive: true });
const WAIT = { timeout: 30000 };

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'peerwire-site-'));
const { id, pages, redirects } = buildSite({ root: ROOT, out: path.join(tmp, 'peerwire') });
const { port, server } = await serve(tmp);
const origin = `http://127.0.0.1:${port}`;
const base = `${origin}/peerwire/`;
const assets = `${base}assets/${id}/`;

let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};

// Everything a page asks for or reports: requests to other hosts, error answers, CSP violations, thrown errors.
function watch(page, name) {
  const seen = { requests: [], foreign: [], errors: [] };
  page.on('request', (r) => {
    const url = r.url();
    if (/^(data|blob):/.test(url)) return;
    seen.requests.push(url);
    if (!url.startsWith(`${origin}/`)) seen.foreign.push(url);
  });
  page.on('response', (r) => {
    if (r.status() >= 400) seen.errors.push(`${r.status()} ${r.url()}`);
  });
  page.on('console', (m) => {
    if (/Content Security Policy/i.test(m.text())) seen.errors.push(`CSP: ${m.text()}`);
  });
  page.on('pageerror', (e) => seen.errors.push(`${name} threw: ${e.message}`));
  return seen;
}

const shot = async (page, name, opts) => {
  if (shots) await page.screenshot({ path: path.join(shots, `${name}.png`), ...opts });
};

let browser;
try {
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 860 } });

  // 1. The crawl: from the landing page, every relative link of every page, fetched without running scripts.
  const ids = new Map();
  const body = new Map();
  const queue = [base];
  const reached = new Set();
  const bad = [];
  const fragments = [];
  while (queue.length) {
    const url = queue.shift();
    if (reached.has(url)) continue;
    reached.add(url);
    const res = await ctx.request.get(url);
    if (!res.ok()) {
      bad.push(`${res.status()} ${url}`);
      continue;
    }
    if (!/text\/html/.test(res.headers()['content-type'] ?? '')) continue;
    const html = await res.text();
    body.set(url, html);
    ids.set(url, new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((m) => m[1])));
    for (const [, raw] of html.matchAll(/\s(?:href|src)="([^"]*)"/g)) {
      const link = raw.replace(/&amp;/g, '&');
      if (/^(https?:|mailto:)/.test(link)) continue;
      const to = new URL(link, url);
      if (!to.href.startsWith(base)) {
        bad.push(`${url}: ${link} leaves /peerwire/`);
        continue;
      }
      if (to.hash.length > 1) fragments.push([url, link, to.href.replace(/[?#].*$/, ''), decodeURIComponent(to.hash.slice(1))]);
      to.hash = '';
      to.search = '';
      queue.push(to.href);
    }
  }
  for (const [from, link, page, frag] of fragments) {
    const target = page === base ? `${base}` : page;
    if (!ids.get(target)?.has(frag)) bad.push(`${from}: ${link}: no id "${frag}"`);
  }
  const reachedPages = new Set([...body.keys()].map((u) => (u === base ? 'index.html' : u.slice(base.length))));
  const listed = pages.filter((p) => !redirects.includes(p));
  check(listed.every((p) => reachedPages.has(p)), `all ${listed.length} pages but the redirects are reachable from the landing page` +
    `${listed.some((p) => !reachedPages.has(p)) ? ` (not: ${listed.filter((p) => !reachedPages.has(p)).join(', ')})` : ''}`);
  check(bad.length === 0, `${reached.size} URLs fetched, ${fragments.length} #fragments checked: every link resolves` +
    `${bad.length ? `\n     ${bad.join('\n     ')}` : ''}`);

  // 2. The landing page and two docs, rendered light and dark: nothing from another host, no CSP violation.
  const dark = await browser.newContext({ viewport: { width: 1280, height: 860 }, colorScheme: 'dark' });
  const light = await browser.newContext({ viewport: { width: 1280, height: 860 }, colorScheme: 'light' });
  for (const [scheme, c, bg] of [['light', light, 'rgb(255, 255, 255)'], ['dark', dark, 'rgb(11, 13, 19)']]) {
    for (const [name, url, full] of [['landing', base, true], ['overview', `${base}docs/index.html`],
      ['threat-model', `${base}docs/project/threat-model.html#threat-model`]]) {
      const page = await c.newPage();
      const seen = watch(page, name);
      await page.goto(url);
      await page.waitForLoadState('load');
      const styled = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
      const icon = await page.evaluate(() => document.querySelector('.brand img').naturalWidth);
      check(seen.foreign.length === 0 && seen.errors.length === 0 && styled === bg && icon > 0,
        `${name} (${scheme}): loads styled from assets/${id}/site.css (${styled}) with its logo, nothing from another host, no error` +
        `${[...seen.foreign, ...seen.errors].map((e) => `\n     ${e}`).join('')}`);
      await shot(page, `site-${name}-${scheme}`, { fullPage: !!full });
      await page.close();
    }
  }
  // The code tabs: a label shows its code and hides the rest, with no script.
  const tabs = await light.newPage();
  await tabs.goto(base);
  const shown = () => tabs.evaluate(() => [...document.querySelectorAll('.tabs .tab')]
    .filter((t) => getComputedStyle(t).display !== 'none').map((t) => t.className).join());
  const before = await shown();
  await tabs.click('label[for="tab-rust"]');
  const after = await shown();
  await tabs.click('label[for="tab-godot"]');
  check(before === 'tab tab-js' && after === 'tab tab-rust' && await shown() === 'tab tab-godot',
    `the code tabs switch with CSS alone (${before} -> ${after} -> ${await shown()})`);
  await tabs.close();
  // The first site's URLs land on their new pages.
  for (const [from, to] of [['README.html', 'index.html'], ['docs/THREAT-MODEL.html', 'docs/project/threat-model.html'],
    ['packages/js/README.html', 'docs/reference/javascript.html']]) {
    const page = await light.newPage();
    await page.goto(base + from);
    const ok = await page.waitForURL(base + to, { timeout: 10000 }).then(() => true, () => false);
    check(ok, `${from} moves to /peerwire/${to} (${page.url()})`);
    await page.close();
  }
  await light.close();
  await dark.close();
  // On a phone: the sidebar gives way to a menu that opens, and nothing is wider than the screen.
  const mobile = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  for (const [name, url] of [['landing', base], ['doc', `${base}docs/guides/godot.html`], ['overview', `${base}docs/index.html`]]) {
    const m = await mobile.newPage();
    await m.goto(url);
    await m.waitForLoadState('load');
    const wide = await m.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    let menu = true;
    if (name !== 'landing') {
      const hidden = await m.evaluate(() => getComputedStyle(document.querySelector('.side')).display === 'none');
      await m.click('.mnav summary');
      menu = hidden && await m.isVisible('.mnav nav a[aria-current="page"]');
    }
    check(wide <= 0 && menu, `${name} on a phone: no wider than the screen (${wide} px over)` +
      `${name === 'landing' ? '' : ', the sidebar is a menu that opens'}`);
    await shot(m, `site-${name}-mobile`, { fullPage: name === 'landing' });
    await m.close();
  }
  await mobile.close();

  // 3. Each demo reaches ready, with its modules and wasm from assets/<id>/ only.
  const room = `site${port}`;
  const demos = [
    ['index', `demo/index.html?room=${room}`, () => window.demo?.id],
    ['poll', `demo/poll.html?room=${room}`, () => window.demo?.id],
    ['arena', `demo/arena.html?room=${room}&role=super`, () => window.arena?.id],
    ['netlab', `demo/netlab.html?signal=local&name=s0#key=${room}abcdefgh`, () => window.netlab?.key && window.netlab.self],
  ];
  for (const [name, url, ready] of demos) {
    const page = await ctx.newPage();
    const seen = watch(page, name);
    await page.goto(base + url);
    let ok = true;
    await page.waitForFunction(ready, null, WAIT).catch((e) => {
      ok = false;
      seen.errors.push(`not ready: ${e.message.split('\n')[0]}`);
    });
    const loaded = seen.requests.filter((u) => !u.startsWith(`${base}demo/${name}.html`));
    const outside = loaded.filter((u) => !u.startsWith(assets));
    const wasm = loaded.filter((u) => u === `${assets}pkg/peerwire_ffi.wasm`).length;
    check(ok && wasm === 1 && outside.length === 0 && seen.errors.length === 0 && seen.foreign.length === 0,
      `${name}: ready, its ${loaded.length} requests (modules, the wasm) all to assets/${id}/` +
      `${[...outside.map((u) => `outside assets: ${u}`), ...seen.errors, ...seen.foreign].map((e) => `\n     ${e}`).join('')}`);
    await shot(page, `demo-${name}`);
    await page.close();
  }

  // 4. Two chat tabs over ?signal=local: a nickname, then both list the other on a direct route.
  const chatUrl = `${base}demo/chat.html?signal=local#chat-${room}`;
  const a = await ctx.newPage();
  const seenA = watch(a, 'chat A');
  await a.goto(chatUrl);
  await a.waitForSelector('#welcome:not(.hidden)', WAIT);
  await a.fill('#name', 'Ada');
  await a.click('#go');
  await a.waitForFunction(() => window.chat?.room, null, WAIT);
  const b = await ctx.newPage();
  const seenB = watch(b, 'chat B');
  await b.goto(chatUrl);
  await b.waitForFunction(() => window.chat?.room, null, WAIT);
  let linked = true;
  await Promise.all([a, b].map((p) => p.waitForSelector('#members li[data-route="direct"]', { ...WAIT, state: 'attached' })))
    .catch(() => { linked = false; });
  await b.fill('#me', 'Grace');
  await b.press('#me', 'Tab');
  const invite = await a.$eval('#link', (i) => i.value);
  check(invite.startsWith(`${base}demo/chat.html?signal=local#`), `chat: the invite link keeps the subpath (${invite})`);
  const chatErrors = [...seenA.errors, ...seenB.errors, ...seenA.foreign, ...seenB.foreign];
  const chatOutside = [...seenA.requests, ...seenB.requests].filter((u) => !u.startsWith(assets) && !u.startsWith(`${base}demo/chat.html`));
  check(linked && chatErrors.length === 0 && chatOutside.length === 0,
    'chat: two tabs link on a direct route over local signalling, everything from assets/' +
    `${[...chatErrors, ...chatOutside].map((e) => `\n     ${e}`).join('')}`);
  await b.fill('#text', 'hello from the site').catch(() => {});
  await b.press('#text', 'Enter').catch(() => {});
  await a.waitForFunction(() => document.querySelector('#messages')?.textContent.includes('hello from the site'), null, { timeout: 5000 }).catch(() => {});
  await shot(a, 'demo-chat-a');
  await shot(b, 'demo-chat-b');
} catch (e) {
  check(false, `site test threw: ${e.stack ?? e}`);
} finally {
  await browser?.close();
  server.kill();
  fs.rmSync(tmp, { recursive: true, force: true });
}
console.log(failed ? `${failed} check(s) failed` : 'all passed');
process.exit(failed ? 1 : 0);
