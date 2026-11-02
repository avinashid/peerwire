// room.doc(name, { persist: true }) in headless Chromium on loopback: the doc kept in IndexedDB. A tab writes a
// doc, leaves (which saves) and reloads: the doc is back from its copy alone, and the record IndexedDB holds is
// sealed (its key and bytes show neither the room, the doc nor what it holds). A write the page makes just
// before it is hidden is saved at once, so a tab that then crashes (no pagehide) still has it when it opens the
// doc again. forget() deletes the record. Once the browser closes the connection (site data cleared), the next
// save opens it again, also when it runs before the close event comes (each order forced). With IndexedDB refusing
// (as in some private modes) the doc says 'blocked', warns once and still syncs with a second tab.
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

  // The browser closes the connection under the page (its site data cleared): the next save opens it again. A save
  // that never ends fails here in 10 s, not at the gate's timeout (whose SIGTERM makes Playwright close the browser,
  // so the waiting evaluate fails with 'Target page, context or browser has been closed').
  const saveWarnings = [];
  page2.on('console', (m) => m.type() === 'warning' && saveWarnings.push(m.text()));
  await page2.evaluate(async () => {
    window.doc.set('strokes').add('before clearing');
    await window.doc.save();
  });
  await (await ctx.newCDPSession(page2)).send('Storage.clearDataForOrigin', { origin: `http://127.0.0.1:${port}`, storageTypes: 'indexeddb' });
  const left = await page2.evaluate(() => {
    window.doc.set('strokes').add('after clearing');
    const saved = (async () => {
      await window.doc.save();
      await window.room.leave();
    })();
    return Promise.race([saved, new Promise((r) => setTimeout(r, 10000, 'hung'))]);
  });
  if (left === 'hung') fail('the save after the site data was cleared hung');
  const cleared = await records(page2);
  if (cleared.length !== 1 || saveWarnings.length) fail(`saved after the site data was cleared: ${cleared.length} records, ${saveWarnings.join(' | ')}`);
  console.log('the connection closed by the browser: the next save opened it again');

  // The same in the orders a loaded page meets it, forced: a save on the connection after the browser closed it,
  // before its close event came. Closed already, transaction() throws InvalidStateError ('closed'); with the site
  // data cleared while the page is busy, a transaction made before it hears of it fails only its request
  // (AbortError) and never ends ('busy before the transaction'), and one whose put succeeded aborts as it commits
  // (UnknownError, 'busy before the commit'). Each saves on a connection opened anew, without a warning, and the save
  // after it keeps that one, even if the old one's close event comes late. A connection still open that fails so
  // ('aborted on an open connection') is closed as it is left. One closed every time is tried twice: the doc warns
  // once and goes on.
  await page2.reload();
  if ((await open(page2)) !== 'loaded') fail('no copy before the forced closes');
  let go = false;
  let polled;
  await page2.route('**/hold', (route) => {
    polled?.();
    route.fulfill({ contentType: 'text/plain', body: go ? 'go' : 'wait' });
  });
  await page2.evaluate(() => {
    window.opens = 0;
    const open = IDBFactory.prototype.open;
    IDBFactory.prototype.open = function (...a) {
      window.opens++;
      return open.apply(this, a);
    };
    // f(connection), in the same task as the next transaction made, just before it (every one, with `always`).
    window.beforeTransaction = (f, always) => {
      const tx = (window.transaction ??= IDBDatabase.prototype.transaction);
      IDBDatabase.prototype.transaction = function (...a) {
        if (!always) IDBDatabase.prototype.transaction = tx;
        f(this);
        return tx.apply(this, a);
      };
    };
    // f(request), as the next put is made.
    window.onPut = (f) => {
      const put = IDBObjectStore.prototype.put;
      IDBObjectStore.prototype.put = function (...a) {
        IDBObjectStore.prototype.put = put;
        const req = put.apply(this, a);
        f(req);
        return req;
      };
    };
    // Keeps the page busy, hearing nothing, until the test says go (a request made in sync, which it answers).
    window.busy = () => {
      for (;;) {
        const x = new XMLHttpRequest();
        x.open('GET', '/hold', false);
        x.send();
        if (x.responseText === 'go') return;
      }
    };
    // Whether promise p isn't settled in 10 s.
    window.hung = (p) => Promise.race([p.then(() => false), new Promise((r) => setTimeout(r, 10000, true))]);
    // Saves a write after `how`, then, once a close event of the connection it failed on came, another: the
    // connections each opened and whether that one is closed, or 'hung' for a save not done in 10 s.
    window.forced = async (how) => {
      let old;
      if (how === 'closed') window.beforeTransaction((d) => (old = d).close());
      if (how === 'aborted on an open connection') window.onPut((req) => ((old = req.transaction.db), req.transaction.abort()));
      if (how === 'busy before the transaction') window.beforeTransaction((d) => ((old = d), window.busy()));
      if (how === 'busy before the commit') window.onPut((req) => ((old = req.transaction.db), req.addEventListener('success', window.busy)));
      const opens = window.opens;
      window.doc.set('strokes').add(how);
      if (await window.hung(window.doc.save())) return 'hung';
      const reopened = window.opens - opens;
      let closed = false;
      try {
        old.transaction('docs').abort();
      } catch (e) {
        closed = e.name === 'InvalidStateError';
      }
      old?.dispatchEvent(new Event('close'));
      window.doc.set('strokes').add(`${how}, again`);
      if (await window.hung(window.doc.save())) return 'hung';
      return [reopened, window.opens - opens - reopened, closed];
    };
  });
  for (const how of ['closed', 'aborted on an open connection', 'busy before the transaction', 'busy before the commit']) {
    go = false;
    const busy = new Promise((r) => (polled = r));
    const opened = page2.evaluate((how) => window.forced(how), how);
    // Cleared once the page is held (unless the save failed before it got there).
    if (how.startsWith('busy') && (await Promise.race([busy.then(() => true), opened.then(() => false)]))) {
      await (await ctx.newCDPSession(page2)).send('Storage.clearDataForOrigin', { origin: `http://127.0.0.1:${port}`, storageTypes: 'indexeddb' });
    }
    go = true;
    const got = await opened;
    const recs = await records(page2);
    if (JSON.stringify(got) !== '[1,0,true]' || recs.length !== 1 || saveWarnings.length) fail(`a save on a connection ${how}: ${JSON.stringify(got)} (opened, then, the old one closed), ${recs.length} records, ${saveWarnings.join(' | ')}`);
  }
  const tries = await page2.evaluate(async () => {
    let n = 0;
    window.beforeTransaction((d) => (n++, d.close()), true);
    window.doc.set('strokes').add('closed every time');
    const hung = await window.hung(window.doc.save());
    IDBDatabase.prototype.transaction = window.transaction;
    return hung || (await window.hung(window.room.leave())) ? 'hung' : n;
  });
  if (tries !== 2 || saveWarnings.length !== 1 || !saveWarnings[0].includes("can't keep a copy")) fail(`a connection closed every time: ${tries} tries, ${saveWarnings.join(' | ')}`);
  console.log('a save on a connection closed before its close event came saved on a new one; one closed every time was tried twice');

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
