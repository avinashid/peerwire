// room.doc(name, { persist: true }) in headless Chromium on loopback: the doc kept in IndexedDB. A tab writes a
// doc, leaves (which saves) and reloads: the doc is back from its copy alone, and the record IndexedDB holds is
// sealed (its key and bytes show neither the room, the doc nor what it holds). A write the page makes just
// before it is hidden is saved at once, so a tab that then crashes (no pagehide) still has it when it opens the
// doc again. forget() deletes the record. Once the browser closes the connection (site data cleared), the next
// save opens it again. With IndexedDB refusing (as in some private modes) the doc says 'blocked', warns once and
// still syncs with a second tab.
//
//   node packages/js/test/doc_store.mjs

import { serve } from './serve.mjs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const { port, server } = await serve(root);
const roomId = `doc-store-${process.pid}`;
let browser;
let failed = false;
const fail = (m) => {
  console.error(`FAIL ${m}`);
  failed = true;
};

// In the page: joins the room, opens the doc with `persist` and waits for its copy. What the doc holds goes on
// window.view().
async function open(page, persist = true) {
  return page.evaluate(async ([roomId, persist]) => {
    const { joinRoom } = await import('/src/index.js');
    window.room = await joinRoom({ appId: 'peerwire-test', roomId });
    window.doc = window.room.doc('board', { persist });
    window.view = () => JSON.stringify([window.doc.map('meta').entries().map((e) => [e.key, e.value]), window.doc.set('strokes').entries().map((e) => e.value), window.doc.log('chat', { keep: 5 }).entries().map((e) => e.value)]);
    return window.doc.loaded;
  }, [roomId, persist]);
}

// The records IndexedDB holds: [key, the bytes as latin1 text].
const records = (page) => page.evaluate(() => new Promise((resolve, reject) => {
  const r = indexedDB.open('peerwire');
  r.onerror = () => reject(r.error);
  r.onsuccess = () => {
    const db = r.result;
    if (!db.objectStoreNames.contains('docs')) return resolve([]);
    const all = [];
    const cur = db.transaction('docs').objectStore('docs').openCursor();
    cur.onsuccess = () => {
      const c = cur.result;
      if (!c) return (db.close(), resolve(all));
      all.push([c.key, String.fromCharCode(...c.value), c.value.length]);
      c.continue();
    };
  };
}));

try {
  browser = await chromium.launch();
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await page.goto(`http://127.0.0.1:${port}/test/`);

  // Written, left, reloaded: back from the copy.
  if ((await open(page)) !== 'empty') fail('a first opening found a copy');
  const before = await page.evaluate(async () => {
    window.doc.map('meta').set('title', 'Kept in IndexedDB');
    for (let i = 0; i < 3; i++) window.doc.set('strokes').add({ points: [i, i, i + 1, i + 1] });
    for (let i = 0; i < 8; i++) window.doc.log('chat', { keep: 5 }).append(`m${i}`);
    const v = window.view();
    await window.room.leave();
    return v;
  });
  const recs = await records(page);
  if (recs.length !== 1 || !/^[0-9a-f]{64}$/.test(recs[0][0]) || recs[0][1].charCodeAt(0) !== 1) fail(`IndexedDB holds ${JSON.stringify(recs.map((r) => [r[0], r[1].charCodeAt(0)]))}`);
  for (const s of ['Kept in IndexedDB', 'points', 'board', 'chat', roomId]) if (recs.some(([k, v]) => k.includes(s) || v.includes(s))) fail(`the record shows ${s}`);
  // Nothing in clear beside it: one database and one store, of constant names.
  const names = await page.evaluate(async () => {
    const dbs = (await indexedDB.databases()).map((d) => d.name);
    const db = await new Promise((r) => (indexedDB.open('peerwire').onsuccess = (e) => r(e.target.result)));
    const stores = [...db.objectStoreNames];
    db.close();
    return { dbs, stores };
  });
  if (JSON.stringify(names) !== '{"dbs":["peerwire"],"stores":["docs"]}') fail(`IndexedDB names ${JSON.stringify(names)}`);
  await page.reload();
  if ((await open(page)) !== 'loaded') fail('the reloaded page found no copy');
  const after = await page.evaluate(() => window.view());
  if (after !== before) fail(`reloaded: ${after}, was ${before}`);
  console.log(`a doc written, left and reloaded came back from IndexedDB (one sealed record of ${recs[0]?.[2]} bytes)`);

  // A write just before the page is hidden is saved then, not SAVE_MS later: a crash after it keeps it.
  await page.evaluate(async () => {
    window.doc.set('strokes').add({ points: [9, 9] });
    Object.defineProperty(document, 'visibilityState', { value: 'hidden', configurable: true });
    document.dispatchEvent(new Event('visibilitychange'));
    await window.doc.saving;
  });
  const cdp = await ctx.newCDPSession(page);
  const crashed = new Promise((r) => page.once('crash', r));
  cdp.send('Page.crash').catch(() => {});
  await crashed;
  const page2 = await ctx.newPage();
  page2.on('pageerror', (e) => errors.push(e.message));
  await page2.goto(`http://127.0.0.1:${port}/test/`);
  if ((await open(page2)) !== 'loaded') fail('no copy after the crash');
  const strokes = await page2.evaluate(() => window.doc.set('strokes').entries().length);
  if (strokes !== 4) fail(`after the page was hidden and crashed: ${strokes} strokes, not 4`);
  console.log('a write before the page was hidden was saved then, and outlived a crash');

  // forget(): the record goes, and the next opening finds nothing.
  await page2.evaluate(async () => {
    await window.doc.forget();
    window.doc.set('strokes').add('after forget');
    await window.room.leave();
  });
  if ((await records(page2)).length) fail('forget() left a record');
  await page2.reload();
  if ((await open(page2)) !== 'empty') fail('a page opened after forget() found a copy');
  console.log('forget() deleted the record');

  // The browser closes the connection under the page (its site data cleared): the next save opens it again.
  const saveWarnings = [];
  page2.on('console', (m) => m.type() === 'warning' && saveWarnings.push(m.text()));
  await page2.evaluate(async () => {
    window.doc.set('strokes').add('before clearing');
    await window.doc.save();
  });
  await (await ctx.newCDPSession(page2)).send('Storage.clearDataForOrigin', { origin: `http://127.0.0.1:${port}`, storageTypes: 'indexeddb' });
  await page2.evaluate(async () => {
    window.doc.set('strokes').add('after clearing');
    await window.doc.save();
    await window.room.leave();
  });
  const cleared = await records(page2);
  if (cleared.length !== 1 || saveWarnings.length) fail(`saved after the site data was cleared: ${cleared.length} records, ${saveWarnings.join(' | ')}`);
  console.log('the connection closed by the browser: the next save opened it again');

  // IndexedDB refusing: 'blocked', one warning, and the doc still syncs with another tab.
  const ctx2 = await browser.newContext();
  const [p, q] = [await ctx2.newPage(), await ctx2.newPage()];
  const warnings = [];
  p.on('console', (m) => m.type() === 'warning' && warnings.push(m.text()));
  await p.addInitScript(() => {
    Object.defineProperty(window, 'indexedDB', { value: { open() { throw new DOMException('refused', 'InvalidStateError'); } } });
  });
  for (const x of [p, q]) await x.goto(`http://127.0.0.1:${port}/test/`);
  const [found] = await Promise.all([open(p), open(q, false)]);
  await p.evaluate(async () => {
    for (let i = 0; i < 3; i++) {
      window.doc.set('strokes').add(i);
      await new Promise((r) => setTimeout(r, 1100));
    }
  });
  await q.waitForFunction(() => window.doc.set('strokes').entries().length === 3, null, { timeout: 15000 }).catch(() => fail('a doc whose IndexedDB refused did not sync'));
  const warned = warnings.filter((w) => w.includes("can't keep a copy")).length;
  if (found !== 'blocked' || warned !== 1) fail(`IndexedDB refusing: ${found}, ${warned} warnings`);
  console.log("IndexedDB refusing: the doc said 'blocked', warned once and synced");
  if (errors.length) fail(`page errors: ${errors.join('; ')}`);
} catch (e) {
  fail(e.stack ?? e);
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? 'doc_store: FAILED' : 'doc_store: ok');
process.exit(failed ? 1 : 0);
