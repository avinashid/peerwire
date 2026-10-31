// Calls (NB-010) in headless Chromium on loopback: tabs of one room over local signalling, with Chromium's fake
// camera and microphone (test/call.html).
// 1. 2, 3 and 4 tabs in the call: each gets every other's stream, with video frames decoded and audio packets
//    received (getStats). Sending nothing and then the camera again (setStream) changes no description, and the
//    video goes on. With 2, ICE restarted on both ends at once (glare) settles on new credentials, the video
//    going on. One leaving the call is 'ended' ('left') on the others, one leaving the room 'ended' ('gone').
// 2. Seats: 6 tabs in the call, receiving only. A 7th that knows who is in gets 'full' at once; an 8th that calls
//    as soon as it joins the room gets 'full' once it learns. The 6 keep their 5 peers each and see neither. One
//    of them leaves, and the 7th then gets in.
// 3. privacy 'public-only' with no STUN server: no candidates at all, so the room links over signalling (its
//    pipe), the call is signalled over that, and its media fails ('no-path') CONNECT_MS after the link was made.
// 4. A member that sends nothing, its transceivers receive-only (a modified client), with 2 that send: it decodes
//    their video, and each of them still gets one 'stream' for it, audio and video (silent), once it connects.
//
//   node packages/js/test/call_browser.mjs [--part=1,2,3,4]

import { serve } from './serve.mjs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { CONNECT_MS } from '../src/call.js';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const parts = (process.argv.find((a) => a.startsWith('--part='))?.split('=')[1] ?? '1,2,3,4').split(',').map(Number);
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const { port, server } = await serve(root);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};

// Polls `fn` until it gives a truthy value, for at most `ms`; null if it never does.
async function until(fn, ms) {
  const end = Date.now() + ms;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) return null;
    await sleep(250);
  }
}

async function tab(ctx, name, query = '') {
  const p = await ctx.newPage();
  p.on('pageerror', (e) => console.log(`page error: ${e.message}`));
  await p.goto(`http://127.0.0.1:${port}/test/call.html?room=${name}${query}`);
  await p.waitForFunction(() => window.t, null, { timeout: 30000 });
  return p;
}

async function tabs(ctx, name, n, query) {
  const pages = [];
  for (let i = 0; i < n; i++) pages.push(await tab(ctx, name, query));
  return pages;
}

const linked = (pages, n, ms = 30000) => until(async () => (await Promise.all(pages.map((p) => p.evaluate(() => window.t.peers())))).every((k) => k >= n), ms);
const events = (p, e) => p.evaluate((e) => window.t.events().filter((x) => x.e === e).map((x) => x.a), e);
// Whether each tab has `n` call peers, every one connected, sending video we decode and audio.
const flowing = (pages, n) => until(async () => {
  for (const p of pages) {
    const d = Object.values(await p.evaluate(() => window.t.decoded()));
    const st = Object.values(await p.evaluate(() => window.t.states()));
    if (d.length !== n || !d.every((x) => x.frames > 0 && x.audio > 0) || !st.every((s) => s === 'connected')) return false;
  }
  return true;
}, 30000);
// Video frames `p` has decoded from `peer`.
const frames = async (p, peer) => (await p.evaluate(() => window.t.decoded()))[peer]?.frames ?? 0;

let browser;
try {
  browser = await chromium.launch({ args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--disable-features=WebRtcHideLocalIpsWithMdns'] });

  if (parts.includes(1)) {
    for (const n of [2, 3, 4]) {
      const ctx = await browser.newContext();
      const pages = await tabs(ctx, `m${port}-${n}`, n);
      check(await linked(pages, n - 1), `${n} tabs: the room links`);
      const ids = await Promise.all(pages.map((p) => p.evaluate(() => window.t.id)));
      const t0 = Date.now();
      await Promise.all(pages.map((p) => p.evaluate(() => window.t.join())));
      check(await flowing(pages, n - 1), `${n} tabs: each decodes video and gets audio from the other ${n - 1} (${Date.now() - t0} ms)`);
      for (const [i, p] of pages.entries()) {
        const s = await events(p, 'stream');
        const others = ids.filter((_, j) => j !== i);
        check(s.length === n - 1 && others.every((id) => s.some(([peer, tr]) => peer === id && tr === 'tracks:audio,video')), `${n} tabs: tab ${i} had one 'stream' per peer, with an audio and a video track`);
      }
      // setStream(null), then the camera again: replaceTrack, no new description; the video goes on.
      const before = await frames(pages[1], ids[0]);
      check(await pages[0].evaluate(() => window.t.swap()), `${n} tabs: setStream twice changes no description`);
      check(await until(async () => (await frames(pages[1], ids[0])) > before + 15, 10000), `${n} tabs: tab 0's video goes on after setStream`);
      if (n === 2) {
        const old = await pages[0].evaluate(() => window.t.ufrag());
        await Promise.all(pages.map((p) => p.evaluate(() => window.t.restart())));
        const settled = await until(async () => {
          const st = await Promise.all(pages.map((p) => p.evaluate(() => window.t.signaling())));
          const now = await pages[0].evaluate(() => window.t.ufrag());
          return st.every((s) => s.every((x) => x === 'stable')) && now !== old;
        }, 15000);
        check(settled, '2 tabs: ICE restarted on both ends at once settles (stable, new ufrag)');
        const f = await frames(pages[1], ids[0]);
        check(await until(async () => (await frames(pages[1], ids[0])) > f + 15, 10000), '2 tabs: the video goes on after the restart');
      }
      // The last tab leaves the call: 'ended' ('left') on the others, at once.
      const t1 = Date.now();
      await pages[n - 1].evaluate(() => window.t.leave());
      const left = await until(async () => {
        for (const p of pages.slice(0, n - 1)) {
          const e = await events(p, 'ended');
          if (!e.some(([peer, r]) => peer === ids[n - 1] && r === 'left') || (await p.evaluate(() => window.t.callPeers())).includes(ids[n - 1])) return false;
        }
        return true;
      }, 5000);
      check(left, `${n} tabs: leaving the call is 'ended' ('left') on the others (${Date.now() - t1} ms)`);
      if (n >= 3) {
        await pages[n - 2].evaluate(() => window.t.leaveRoom());
        const gone = await until(async () => {
          for (const p of pages.slice(0, n - 2)) if (!(await events(p, 'ended')).some(([peer, r]) => peer === ids[n - 2] && r === 'gone')) return false;
          return true;
        }, 5000);
        check(gone, `${n} tabs: leaving the room is 'ended' ('gone') on the others`);
      }
      await ctx.close();
    }
  }

  if (parts.includes(2)) {
    const ctx = await browser.newContext();
    const name = `f${port}`;
    const pages = await tabs(ctx, name, 7);
    check(await linked(pages, 6), '7 tabs: the room links');
    const ids = await Promise.all(pages.map((p) => p.evaluate(() => window.t.id)));
    for (const p of pages.slice(0, 6)) await p.evaluate(() => window.t.join({ send: false }));
    const six = pages.slice(0, 6);
    const seated = () => until(async () => {
      for (const p of six) {
        const st = Object.values(await p.evaluate(() => window.t.states()));
        if (st.length !== 5 || !st.every((s) => s === 'connected')) return false;
      }
      return true;
    }, 30000);
    check(await seated(), '6 in the call: each connected to the other 5');
    await pages[6].evaluate(() => window.t.join({ send: false }));
    const full7 = await until(async () => (await events(pages[6], 'full')).length && (await pages[6].evaluate(() => window.t.left())), 5000);
    check(full7 && (await pages[6].evaluate(() => window.t.callPeers())).length === 0, "the 7th gets 'full' at once and is out of the call");
    // An 8th calls as soon as it has joined the room, before it knows who is in.
    const p8 = await tab(ctx, name, '&call=1');
    const id8 = await p8.evaluate(() => window.t.id);
    const full8 = await until(async () => (await events(p8, 'full')).length, 30000);
    check(full8 && (await p8.evaluate(() => window.t.left())), "an 8th that called on joining the room gets 'full' once it learns who is in");
    await sleep(1000);
    let kept = true;
    for (const p of six) {
      const peers = await p.evaluate(() => window.t.callPeers());
      const ended = await events(p, 'ended');
      if (peers.length !== 5 || peers.includes(ids[6]) || peers.includes(id8) || ended.length) {
        kept = false;
        console.log(`  tab ${six.indexOf(p)}: ${peers.length} peers, ended ${JSON.stringify(ended)}, full ${JSON.stringify(await events(p, 'full'))} left ${await p.evaluate(() => window.t.left())}, 7th ${peers.includes(ids[6])}, 8th ${peers.includes(id8)} (${id8.slice(0, 8)})`);
      }
    }
    check(kept, 'the 6 keep their 5 peers each, with nothing ended, and never had the 7th or the 8th');
    // A seat frees: the 7th gets in.
    await pages[0].evaluate(() => window.t.leave());
    await until(async () => {
      for (const p of six.slice(1)) if ((await p.evaluate(() => window.t.callPeers())).length !== 4) return false;
      return true;
    }, 5000);
    await pages[6].evaluate(() => window.t.join({ send: false }));
    const in7 = await until(async () => {
      const st = Object.values(await pages[6].evaluate(() => window.t.states()));
      return st.length === 5 && st.every((s) => s === 'connected');
    }, 30000);
    check(in7 && !(await pages[6].evaluate(() => window.t.left())), 'once one leaves, the 7th gets in and connects to the 5');
    await ctx.close();
  }

  if (parts.includes(3)) {
    const ctx = await browser.newContext();
    const pages = await tabs(ctx, `p${port}`, 2, '&privacy=public-only');
    check(await linked(pages, 1, 45000), "privacy 'public-only', no STUN: the room links (over signalling)");
    const ids = await Promise.all(pages.map((p) => p.evaluate(() => window.t.id)));
    check((await pages[0].evaluate((id) => window.t.route(id), ids[1])) === 'signaling', "privacy 'public-only', no STUN: the link's route is 'signaling'");
    const t0 = Date.now();
    await Promise.all(pages.map((p) => p.evaluate(() => window.t.join())));
    const fails = await until(async () => {
      const s = await Promise.all(pages.map((p) => events(p, 'state')));
      return s.every((e) => e.some(([, st, r]) => st === 'failed' && r === 'no-path')) && s;
    }, CONNECT_MS + 15000);
    const ms = Date.now() - t0;
    check(fails && ms >= CONNECT_MS - 1000, `privacy 'public-only', no STUN: the call's media fails 'no-path' on both after CONNECT_MS (${ms} ms)`);
    const st = await Promise.all(pages.map((p) => p.evaluate(() => window.t.states())));
    check(st.every((s, i) => s[ids[1 - i]] === 'failed'), 'a failed peer stays in the call, failed');
    await ctx.close();
  }

  if (parts.includes(4)) {
    const ctx = await browser.newContext();
    const name = `r${port}`;
    const pages = [...(await tabs(ctx, name, 2)), await tab(ctx, name, '&recvonly=1')];
    check(await linked(pages, 2), '3 tabs, one receive-only: the room links');
    const ids = await Promise.all(pages.map((p) => p.evaluate(() => window.t.id)));
    const [deaf, id] = [pages[2], ids[2]];
    await Promise.all(pages.map((p, i) => p.evaluate((send) => window.t.join({ send }), i < 2)));
    const watches = await until(async () => {
      const d = Object.values(await deaf.evaluate(() => window.t.decoded()));
      return d.length === 2 && d.every((x) => x.frames > 0);
    }, 30000);
    const dirs = await deaf.evaluate(() => window.t.directions());
    check(watches && dirs.length === 4 && dirs.every((d) => d === 'recvonly'), `the receive-only member decodes both others' video, its transceivers all recvonly (${dirs})`);
    const order = ids.slice(0, 2).map((x) => (id < x ? 'it offers' : 'it answers')).join(', ');
    for (const [i, p] of pages.slice(0, 2).entries()) {
      const shown = await until(async () => (await p.evaluate((id) => window.t.states()[id], id)) === 'connected' && p.evaluate((id) => window.t.hasStream(id), id), 10000);
      const s = (await events(p, 'stream')).filter(([peer]) => peer === id);
      check(shown && s.length === 1 && s[0][1] === 'tracks:audio,video' && (await frames(p, id)) === 0, `tab ${i}: the member sending nothing is 'connected', with one 'stream' for it (audio and video, no frames) (${order})`);
    }
    await ctx.close();
  }
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `FAIL: ${failed} check(s)` : 'PASS');
process.exit(failed ? 1 : 0);
