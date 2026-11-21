// Peerwire Share: a room for sending files of any size peer to peer. Members drop files and offer them to one member
// or to all; the receiver accepts (and picks where it goes) or declines; the file goes in pieces over the room's
// reliable lane, a few in flight at a time, each checked with SHA-256, and is written to disk as it arrives. The piece
// protocol is in files.js; this is the page.
//
//   share/#<room>                   browsers anywhere, through the Nostr signalling relays
//   share/?signal=local#<room>      tabs of this browser (BroadcastChannel)
//   ?save=disk|download|memory      how received files are saved (by default the best this browser has)
//   ?rtc=off                        fail this tab's WebRTC links, as behind a symmetric NAT (files then wait)
//
// Direct only: file data goes only over a peer's own WebRTC DataChannel (files.js directForFiles), never through the
// signalling relays, another member or TURN. While a peer's route is anything else its transfers wait, and the page
// says why. A direct link that closes ends its session in the room (room.js rtcClosed), so the pieces it had in flight
// go nowhere; the peer comes back on a new link (direct or not), and the receiver says how many pieces it has written,
// so the sender goes on from there, once the link is direct. A peer that leaves the room (its page closed or reloaded)
// comes back as a new peer, if at all: its transfers end.
//
// Saving, in this order: the File System Access API (showSaveFilePicker, then a writable stream: Chrome and Edge on a
// computer), else a download streamed through the service worker sw.js (not in WebKit: see pickMode), else in memory
// as a Blob, for files up to BLOB_MAX only. A file goes to disk (or the download completes) only once its whole SHA-256 matches; otherwise it is
// dropped.
//
// Whatever members send is untrusted: names and types are cleaned and shown as text only (textContent), offers and
// pieces are checked (files.js), and what one peer may make this page hold is bounded (MAX_OFFERS, MAX_QUEUED).

import { joinRoom, randomRoomId } from '../../src/index.js';
import { qrCode } from '../qr.js';
import * as F from './files.js';

const APP = 'peerwire-share';
const MAX_ROOM = 256; // bytes of a room id in a link, percent-encoded, so its QR code always fits
const MAX_NICK = 32;
const NAME_KEY = 'peerwire-share/name';
const BLOB_MAX = 256 * 1024 * 1024; // the largest file kept in memory, where nothing streams to disk
const RESUME_MS = 10 * 60 * 1000; // how long a transfer waits for its peer to come back
const MAX_OFFERS = 32; // offers from one peer waiting for an answer; more are declined
const MAX_REJECTS = 8; // pieces refused in one transfer before it fails
const MAX_ROWS = 200; // transfers listed; the oldest finished go first
const RATE_MS = 4000; // speed over the last few seconds
const TICK_MS = 250;

const params = new URLSearchParams(location.search);
const signaling = params.get('signal') === 'local' ? 'local' : 'nostr';
const $ = (id) => document.getElementById(id);

let room = null;
let roomId = null;
let session = 0; // bumped on every join, so a join that finishes after another started is dropped
let myName = loadName();
let rtcOff = params.get('rtc') === 'off';
const names = new Map(); // peer -> nickname
const picked = []; // Files chosen, not offered yet
const outgoing = new Map(); // transfer id -> Out
const incoming = new Map(); // `${peer}/${id}` -> In (ids are the sender's: two senders may pick the same)
const pumping = new Set(); // peers whose send loop runs
const stats = { piecesSent: 0, piecesTaken: 0, rejected: 0, offersRefused: 0 };

// ---------- small helpers ----------

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}
function cleanNick(v) {
  if (typeof v !== 'string') return '';
  return [...v.replace(/[\p{Cc}\p{Cf}]/gu, '').replace(/\s+/g, ' ').trim()].slice(0, MAX_NICK).join('').trim();
}
function loadName() {
  try { return cleanNick(localStorage.getItem(NAME_KEY)); } catch { return ''; }
}
const guest = (id) => `guest ${id.slice(0, 4)}`;
const nameOf = (peer) => names.get(peer) || guest(peer);
const fits = (id) => encodeURIComponent(id).length <= MAX_ROOM;
function bytes(n) {
  if (n < 1024) return `${n} B`;
  const u = ['KB', 'MB', 'GB', 'TB'];
  let i = -1;
  do { n /= 1000; i++; } while (n >= 1000 && i < u.length - 1);
  return `${n.toFixed(n < 10 ? 2 : n < 100 ? 1 : 0)} ${u[i]}`;
}
const rate = (bps) => `${(bps / 1e6).toFixed(bps < 1e7 ? 2 : 1)} MB/s`;
function eta(s) {
  if (!Number.isFinite(s) || s < 0) return '';
  s = Math.round(s);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = String(s % 60).padStart(2, '0');
  return h ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
}

// Bytes moved over the last RATE_MS, for speed and ETA.
class Rate {
  constructor() {
    this.total = 0;
    this.samples = [[performance.now(), 0]];
  }
  add(n) {
    this.total += n;
    const now = performance.now();
    this.samples.push([now, this.total]);
    while (this.samples.length > 2 && now - this.samples[1][0] > RATE_MS) this.samples.shift();
  }
  get bps() {
    const [t0, b0] = this.samples[0];
    const dt = (performance.now() - t0) / 1000;
    return dt > 0.25 ? (this.total - b0) / dt : 0;
  }
}

// A whole file's SHA-256 as it streams: in a worker (hash-worker.js), or here where there are none. update() takes a
// copy; digest() resolves to the hex hash.
function fileHasher() {
  if (typeof Worker !== 'function') {
    const h = new F.Sha256();
    return { update: (d) => h.update(d), digest: async () => F.toHex(h.digest()), close() {} };
  }
  const w = new Worker(new URL('hash-worker.js', import.meta.url), { type: 'module' });
  let failed = null;
  w.onerror = (e) => (failed = e.message || 'the hash worker failed');
  return {
    update(d) {
      const b = d.slice().buffer;
      w.postMessage({ t: 'update', data: b }, [b]);
    },
    digest: () => new Promise((resolve, reject) => {
      if (failed) return reject(new Error(failed));
      w.onmessage = ({ data: m }) => m?.t === 'digest' && resolve(m.hex);
      w.onerror = (e) => reject(new Error(e.message || 'the hash worker failed'));
      w.postMessage({ t: 'digest' });
    }),
    close: () => w.terminate(),
  };
}

// ---------- saving ----------

// How received files are saved: 'disk' (File System Access), 'download' (the service worker), or 'memory'.
const forced = ['disk', 'download', 'memory'].includes(params.get('save')) ? params.get('save') : null;
let saveMode = 'memory';
const sw = registerWorker();
async function registerWorker() {
  if (!navigator.serviceWorker || !isSecureContext) return null;
  try {
    const scope = new URL('dl/', import.meta.url).pathname;
    const reg = await navigator.serviceWorker.register(new URL('sw.js', import.meta.url), { scope });
    const w = reg.installing ?? reg.waiting ?? reg.active;
    if (w && w.state !== 'activated') {
      await new Promise((resolve) => {
        const on = () => w.state === 'activated' && (w.removeEventListener('statechange', on), resolve());
        w.addEventListener('statechange', on);
        on();
      });
    }
    return reg;
  } catch {
    return null;
  }
}
// WebKit (Safari, and every browser on iOS) doesn't reliably start a download a service worker streams into a frame,
// and a stream nobody reads would stall the transfer: there, files are kept in memory unless ?save=download.
const webkit = /AppleWebKit/.test(navigator.userAgent) && !/Chrome|Chromium|Edg|Android/.test(navigator.userAgent);
async function pickMode() {
  const canDisk = typeof globalThis.showSaveFilePicker === 'function';
  const reg = await sw;
  saveMode = forced ?? (canDisk ? 'disk' : reg?.active && !webkit ? 'download' : 'memory');
  $('save-mode').textContent = {
    disk: 'Files you accept are written straight to disk, where you choose, as they arrive.',
    download: 'Files you accept stream into your downloads as they arrive.',
    memory: `This browser can’t stream to disk, so a file you accept is kept in memory until it is whole: only files up ` +
      `to ${bytes(BLOB_MAX)} can be received here. Use Chrome or Edge on a computer for larger ones.`,
  }[saveMode];
  $('save-mode').classList.toggle('warn', saveMode === 'memory');
  renderAll();
}

// Where an accepted file goes: { write(bytes) in order, close() once it is whole and checked, abort() }. Called
// straight from the Accept click: the save picker needs its user gesture.
function openSink(t) {
  if (saveMode === 'disk') return diskSink(t);
  if (saveMode === 'download') return downloadSink(t);
  return memorySink(t);
}

async function diskSink(t) {
  const handle = await globalThis.showSaveFilePicker({ suggestedName: t.name });
  const w = await handle.createWritable();
  return {
    kind: 'disk',
    write: (d) => w.write(d),
    close: () => w.close(), // the file appears only now (the browser writes a temporary one until then)
    abort: () => w.abort().catch(() => {}),
  };
}

async function downloadSink(t) {
  const reg = await sw;
  const worker = reg?.active;
  if (!worker) throw new Error('the download service worker isn’t running');
  const ch = new MessageChannel();
  const key = F.newId();
  let credit = 0;
  let wake = null;
  let gone = false;
  let ready;
  const isReady = new Promise((r) => (ready = r));
  ch.port1.onmessage = ({ data: m }) => {
    if (m?.t === 'ready') ready();
    else if (m?.t === 'pull') credit++;
    else if (m?.t === 'cancelled') {
      gone = true;
      if (t.state === 'receiving') cancel(t, 'the download was cancelled in the browser');
    }
    wake?.();
  };
  worker.postMessage({ t: 'download', key, disposition: F.disposition(t.name), size: t.size }, [ch.port2]);
  await Promise.race([isReady, new Promise((_, no) => setTimeout(() => no(new Error('the download service worker didn’t answer')), 5000))]);
  const frame = el('iframe');
  frame.hidden = true;
  frame.src = new URL(`dl/${key}/${encodeURIComponent(t.name)}`, import.meta.url).href;
  document.body.append(frame);
  // Messages keep the worker alive while the download runs.
  const ping = setInterval(() => reg.active?.postMessage({ t: 'ping' }), 10000);
  const end = (m) => {
    clearInterval(ping);
    ch.port1.postMessage(m);
    setTimeout(() => frame.remove(), 30000);
  };
  return {
    kind: 'download',
    async write(d) {
      while (!credit && !gone) await new Promise((r) => (wake = r));
      if (gone) throw new Error('the download was cancelled');
      credit--;
      const b = d.byteOffset === 0 && d.byteLength === d.buffer.byteLength ? d.buffer : d.slice().buffer;
      ch.port1.postMessage({ t: 'chunk', data: b }, [b]);
    },
    close: async () => end({ t: 'end' }),
    abort: async () => end({ t: 'abort' }),
  };
}

async function memorySink(t) {
  if (t.size > BLOB_MAX) throw new Error(`this browser can only receive files up to ${bytes(BLOB_MAX)}`);
  let parts = [];
  return {
    kind: 'memory',
    write: async (d) => void parts.push(d),
    async close() {
      t.url = URL.createObjectURL(new Blob(parts, { type: 'application/octet-stream' }));
      parts = [];
      const a = el('a');
      a.href = t.url;
      a.download = t.name;
      a.click();
    },
    abort: async () => void (parts = []),
  };
}

// ---------- the room ----------

// An RTCPeerConnection that gathers and takes no candidates while `rtcOff` (?rtc=off), so ICE fails and the room
// falls back; tests turn rtcOff off to let a retry link directly.
const Pc = globalThis.RTCPeerConnection && class extends RTCPeerConnection {
  constructor(c) {
    super(c);
    this.off = rtcOff;
  }
  get onicecandidate() { return super.onicecandidate; }
  set onicecandidate(fn) { super.onicecandidate = this.off ? null : fn; }
  get localDescription() {
    const d = super.localDescription;
    if (!this.off || !d) return d;
    const sdp = strip(d.sdp);
    return { type: d.type, sdp, toJSON: () => ({ type: d.type, sdp }) };
  }
  setRemoteDescription(d) { return super.setRemoteDescription(this.off ? { type: d.type, sdp: strip(d.sdp) } : d); }
  addIceCandidate(c) { return this.off ? Promise.resolve() : super.addIceCandidate(c); }
};
const strip = (sdp) => sdp.split('\r\n').filter((l) => !l.startsWith('a=candidate:')).join('\r\n');

async function enter(id) {
  const my = ++session;
  if (room) {
    const r = room;
    room = null;
    await r.leave();
    if (my !== session) return;
  }
  roomId = id;
  names.clear();
  for (const t of outgoing.values()) if (live(t)) finish(t, 'cancelled', 'you left the room');
  for (const t of incoming.values()) if (live(t)) abortIn(t, 'cancelled', 'you left the room');
  setStatus('connecting', signaling === 'local' ? 'Joining…' : 'Joining through the Nostr signalling relays…');
  setInvite(id);
  renderAll();
  let r;
  try {
    r = await joinRoom({
      appId: APP, roomId: id, signaling, presence: { name: myName },
      ...(params.get('rtc') === 'off' && Pc && { RTCPeerConnection: Pc }),
    });
  } catch (e) {
    if (my === session) setStatus('error', `Couldn’t join: ${e.message}`);
    return;
  }
  if (my !== session) return void r.leave();
  room = r;
  listen(r);
  setStatus(null);
  renderAll();
}

function listen(r) {
  r.on('peer-join', (peer, presence) => {
    if (r !== room) return;
    names.set(peer, cleanNick(presence?.name) || guest(peer));
    rejoined(peer);
    renderAll();
  });
  r.on('presence', (peer, presence) => {
    if (r !== room) return;
    names.set(peer, cleanNick(presence?.name) || guest(peer));
    renderAll();
  });
  r.on('peer-leave', (peer, reason) => {
    if (r !== room) return;
    left(peer, reason);
    renderAll();
  });
  r.on('message', (data, peer, info) => {
    if (r !== room || info.kind !== 'bytes' || info.lane !== 'reliable') return;
    if (data[0] === F.PIECE_FRAME) return onPiece(peer, data);
    const m = F.decodeControl(data);
    if (m) onControl(peer, m);
  });
}

function send(peer, m) {
  try {
    return room?.send(F.encodeControl(m), { to: peer }) ?? 0;
  } catch {
    return 0;
  }
}
const present = (peer) => !!room && room.peers.includes(peer);
const direct = (peer) => present(peer) && F.directForFiles(room.route(peer));

// ---------- transfers ----------
//
// Out (we send): offered -> sending -> done, or declined, cancelled, failed. `next` is the piece read and sent next,
// `acked` how many the receiver has written, `read` how many the whole-file hash has taken (pieces are hashed once,
// in order, however often a resume sends them again).
// In (we receive): offered -> receiving -> done, or declined, cancelled, failed. `have` is how many pieces are
// written, `next` the index of the piece taken next, and `queue` the pieces taken but not yet checked and written;
// `want` is the whole-file hash the sender gave.
// `gone` is when the peer left (null while it is here). When it is back, the receiver says where to go on from (an
// accept with `have`); the sender asks too (`resume`), which a receiver that has finished meanwhile answers with its
// `done` again, and one that has dropped the transfer with a cancel.

const live = (t) => t.state === 'offered' || t.state === 'sending' || t.state === 'receiving';
const keyOf = (peer, id) => `${peer}/${id}`;

function offerFiles(files, to) {
  if (!room) return;
  const peers = to ? [to].filter(present) : room.peers;
  for (const file of files) {
    for (const peer of peers) {
      const id = F.newId();
      const offer = F.makeOffer({ id, name: file.name, size: file.size, type: file.type });
      const t = {
        dir: 'out', id, peer, file, name: offer.name, size: file.size, type: offer.type, piece: offer.piece,
        count: offer.count, offer, state: 'offered', next: 0, acked: 0, read: 0, sent: 0, resumes: 0, hasher: null,
        sha: null, endSent: false, rate: new Rate(), started: 0, ended: 0, gone: null, note: '', view: null,
      };
      outgoing.set(id, t);
      send(peer, offer);
      render(t);
    }
  }
  prune();
}

function onControl(peer, m) {
  if (m.t === 'offer') return onOffer(peer, m);
  if (!F.isId(m.id)) return;
  const out = outgoing.get(m.id);
  const mine = out?.peer === peer ? out : null;
  const t = incoming.get(keyOf(peer, m.id));
  if (m.t === 'accept' && mine) return onAccept(mine, m.have);
  if (m.t === 'decline' && mine?.state === 'offered') return finish(mine, 'declined', 'declined');
  if (m.t === 'ack' && mine) return onAck(mine, m.have);
  if (m.t === 'done' && mine?.state === 'sending') return onDone(mine, m);
  if (m.t === 'end' && t) return onEnd(t, m.sha256);
  if (m.t === 'resume') return onResume(peer, m.id, t);
  if (m.t === 'cancel') {
    const why = `${nameOf(peer)} cancelled`;
    if (mine && live(mine)) finish(mine, 'cancelled', why);
    if (t && live(t)) abortIn(t, 'cancelled', why);
  }
}

// ----- sending -----

function onAccept(t, have) {
  if (t.state === 'offered') {
    if (have !== 0) return;
    t.state = 'sending';
    t.started = performance.now();
    t.hasher = fileHasher();
  } else if (t.state === 'sending') {
    // A resume, on a new link: the receiver says how many pieces it has written.
    const at = F.resumeAt(t, t.read, have);
    if (!at) return finishAndTell(t, 'failed', 'the receiver asked to resume from a piece never sent');
    t.next = at.next;
    t.acked = at.next;
    t.endSent = false; // it may not have arrived
    t.resumes++;
    t.note = `resumed from piece ${at.next + 1} of ${t.count}`;
  } else return;
  t.gone = null;
  render(t);
  pump(t.peer);
}

function onAck(t, have) {
  if (t.state !== 'sending' || F.intIn(have, t.acked, t.next) === null) return;
  const was = t.acked;
  t.acked = have;
  t.rate.add(Math.min(t.size, have * t.piece) - Math.min(t.size, was * t.piece));
  pump(t.peer);
}

// The receiver's verdict: its whole-file SHA-256 is ours (it may come again after a resume, its acks lost), or it
// isn't, once every piece was acked.
function onDone(t, m) {
  if (m.ok === true && t.sha && m.sha256 === t.sha) {
    t.acked = t.count;
    finish(t, 'done', 'delivered: the receiver’s SHA-256 matches');
  } else if (t.acked === t.count) finish(t, 'failed', 'the receiver’s SHA-256 doesn’t match');
}

const sending = (peer) => [...outgoing.values()].filter((t) => t.peer === peer && t.state === 'sending');
// Whether `peer` may have another piece: what is in flight to it over all our transfers to it (sent, not acked) is
// under its window in bytes (files.js windowBytes, from how fast its acks come and the link's round trip), and under
// WINDOW pieces, so the receiver never holds more than MAX_QUEUED.
function windowOpen(peer) {
  let pieces = 0;
  let bytes = 0;
  let bps = 0;
  for (const t of sending(peer)) {
    pieces += t.next - t.acked;
    bytes += Math.min(t.size, t.next * t.piece) - Math.min(t.size, t.acked * t.piece);
    bps += t.rate.bps;
  }
  return pieces < F.WINDOW && bytes < F.windowBytes(bps, room.rtt(peer) ?? 0);
}
// The transfer to `peer` whose pieces go next: the oldest accepted one with pieces (or its end) still to send.
const head = (peer) => sending(peer).find((t) => t.gone === null && !(t.endSent && t.next === t.count));

// Sends what may go to `peer` now, one piece at a time: while the link is direct and the window has room. Acks, a
// resume, a new link and the tick call it again.
async function pump(peer) {
  if (pumping.has(peer)) return;
  pumping.add(peer);
  try {
    for (let t = head(peer); t && direct(peer); t = head(peer)) {
      if (t.next < t.count) {
        if (!windowOpen(peer)) break;
        if (!(await sendPiece(t))) break;
      } else if (!(await sendEnd(t))) break;
    }
  } catch (e) {
    console.error(e);
  } finally {
    pumping.delete(peer);
  }
}

async function sendPiece(t) {
  const i = t.next;
  const [start, end] = F.pieceRange(t, i);
  let data;
  try {
    data = new Uint8Array(await t.file.slice(start, end).arrayBuffer());
  } catch (e) {
    finishAndTell(t, 'failed', `couldn’t read the file (${e.name ?? 'error'}): was it moved or changed?`);
    return false;
  }
  if (data.length !== end - start) {
    finishAndTell(t, 'failed', 'the file changed while it was being sent');
    return false;
  }
  const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', data));
  // Each piece goes into the whole-file hash once, in order, whether or not it is sent now.
  if (i === t.read) {
    t.hasher.update(data);
    t.read++;
  }
  // Cancelled, resumed elsewhere, or the link moved while the piece was read: it goes later, if at all.
  if (t.state !== 'sending' || t.next !== i || !direct(t.peer)) return false;
  room.send(F.encodePiece(t.id, i, hash, data), { to: t.peer });
  t.next++;
  t.sent++;
  stats.piecesSent++;
  return true;
}

async function sendEnd(t) {
  if (!t.sha) {
    try {
      t.sha = await t.hasher.digest();
    } catch (e) {
      finishAndTell(t, 'failed', e.message);
      return false;
    }
  }
  if (t.state !== 'sending' || t.next !== t.count || !direct(t.peer)) return false;
  send(t.peer, { t: 'end', id: t.id, sha256: t.sha });
  t.endSent = true;
  render(t);
  return true;
}

function finish(t, state, note) {
  if (!live(t)) return;
  t.state = state;
  t.note = note;
  t.ended = performance.now();
  t.hasher?.close?.();
  render(t);
}
function finishAndTell(t, state, note) {
  if (!live(t)) return;
  send(t.peer, { t: 'cancel', id: t.id });
  finish(t, state, note);
}

// ----- receiving -----

function onOffer(peer, m) {
  const o = F.parseOffer(m);
  if (!o) return void stats.offersRefused++;
  const k = keyOf(peer, o.id);
  if (incoming.has(k)) return; // sent again on a new link
  const waiting = [...incoming.values()].filter((t) => t.peer === peer && t.state === 'offered').length;
  if (waiting >= MAX_OFFERS) {
    stats.offersRefused++;
    return void send(peer, { t: 'decline', id: o.id });
  }
  const t = {
    dir: 'in', ...o, peer, state: 'offered', have: 0, next: 0, queue: [], chain: Promise.resolve(), sink: null, hasher: null,
    want: null, sha: null, rate: new Rate(), started: 0, ended: 0, gone: null, rejected: 0, note: '', url: null, view: null,
    accepting: false,
  };
  incoming.set(k, t);
  render(t);
  prune();
}

async function accept(t) {
  if (t.state !== 'offered' || t.accepting || !present(t.peer)) return;
  t.accepting = true;
  let sink;
  try {
    sink = await openSink(t); // first: the save picker needs this click
  } catch (e) {
    t.accepting = false;
    if (e?.name !== 'AbortError') t.note = `couldn’t save: ${e.message}`;
    return render(t);
  }
  t.accepting = false;
  if (t.state !== 'offered') return void sink.abort();
  t.sink = sink;
  t.hasher = fileHasher();
  t.state = 'receiving';
  t.started = performance.now();
  t.note = '';
  send(t.peer, { t: 'accept', id: t.id, have: 0 });
  // An empty file has no pieces: its end may have come already.
  if (t.count === 0 && t.want) t.chain = t.chain.then(() => complete(t));
  render(t);
}

function decline(t) {
  if (t.state !== 'offered') return;
  send(t.peer, { t: 'decline', id: t.id });
  t.state = 'declined';
  t.note = 'declined';
  render(t);
}

function onPiece(peer, f) {
  const p = F.decodePiece(f);
  if (!p) return;
  const t = incoming.get(keyOf(peer, p.id));
  // Nothing is kept for a transfer we haven't accepted.
  if (!t || t.state !== 'receiving') return void stats.rejected++;
  const why = F.checkPiece(t, t.next, p);
  if (why !== 'ok') {
    stats.rejected++;
    if (++t.rejected > MAX_REJECTS) cancel(t, `the sender sent pieces it shouldn’t (${why})`);
    return;
  }
  const queued = [...incoming.values()].reduce((n, x) => n + (x.peer === peer ? x.queue.length : 0), 0);
  if (queued >= F.MAX_QUEUED) return cancel(t, 'the sender ignored flow control');
  // The room's buffer is its own once this returns: copy, and start hashing at once.
  const data = p.data.slice();
  const hash = p.hash.slice();
  const digest = crypto.subtle.digest('SHA-256', data);
  t.next++;
  t.queue.push(p.index);
  stats.piecesTaken++;
  t.chain = t.chain.then(() => writePiece(t, p.index, data, hash, digest)).catch((e) => cancel(t, `couldn’t write: ${e.message}`));
}

// Checks and writes one piece, then acks it. The piece stays in the queue until it is written, so the queue bounds
// what this tab holds.
async function writePiece(t, index, data, hash, digest) {
  try {
    const got = await digest;
    if (t.state !== 'receiving') return;
    if (!F.sameHash(got, hash)) return cancel(t, `piece ${index + 1} failed its SHA-256 check`, 'failed');
    const n = data.length; // a download sink takes the buffer
    t.hasher.update(data);
    await t.sink.write(data);
    if (t.state !== 'receiving') return;
    t.have++;
    t.rate.add(n);
    send(t.peer, { t: 'ack', id: t.id, have: t.have });
    if (t.have === t.count && t.want) await complete(t);
  } finally {
    t.queue.shift();
  }
}

// The sender, back on a new link, asks where a transfer is: one we are receiving gets our accept anyway (rejoined);
// one we finished gets our verdict again; one we no longer have, a cancel.
function onResume(peer, id, t) {
  if (t?.state === 'done') return void send(peer, { t: 'done', id, ok: true, sha256: t.sha });
  if (!t || !live(t)) send(peer, { t: 'cancel', id });
}

function onEnd(t, sha) {
  if (t.state !== 'receiving' && t.state !== 'offered') return;
  if (!F.isSha(sha)) return cancel(t, 'the sender’s end had no valid SHA-256');
  // After the pieces before it, which may still be queued.
  t.chain = t.chain.then(() => {
    t.want = sha;
    if (t.state === 'receiving' && t.have === t.count) return complete(t);
  });
}

async function complete(t) {
  if (t.state !== 'receiving') return;
  t.note = 'checking the whole file…';
  render(t);
  let sha;
  try {
    sha = await t.hasher.digest();
  } catch (e) {
    return cancel(t, e.message, 'failed');
  }
  if (t.state !== 'receiving') return;
  t.sha = sha;
  if (sha !== t.want) {
    send(t.peer, { t: 'done', id: t.id, ok: false, sha256: sha });
    return abortIn(t, 'failed', 'the whole file’s SHA-256 doesn’t match: not saved');
  }
  try {
    await t.sink.close();
  } catch (e) {
    send(t.peer, { t: 'done', id: t.id, ok: false, sha256: sha });
    return abortIn(t, 'failed', `couldn’t save: ${e.message}`);
  }
  send(t.peer, { t: 'done', id: t.id, ok: true, sha256: sha });
  t.state = 'done';
  t.ended = performance.now();
  t.note = { disk: 'saved and verified', download: 'downloaded and verified', memory: 'verified: save it below' }[t.sink.kind];
  t.hasher.close();
  render(t);
}

// Ends a transfer we receive, and tells the sender.
function cancel(t, note, state = 'cancelled') {
  if (!live(t)) return;
  send(t.peer, { t: 'cancel', id: t.id });
  abortIn(t, state, note);
}
function abortIn(t, state, note) {
  if (!live(t)) return;
  t.state = state;
  t.note = note;
  t.ended = performance.now();
  t.sink?.abort();
  t.hasher?.close?.();
  t.queue.length = 0;
  render(t);
}

// ----- peers coming and going -----

// A peer whose link dropped may come back with the same id; one that left the room (closed or reloaded its page)
// comes back, if at all, as a new peer, so its transfers end now.
function left(peer, reason) {
  const now = Date.now();
  for (const t of [...outgoing.values(), ...incoming.values()]) {
    if (t.peer !== peer || !live(t)) continue;
    if (reason !== 'left') t.gone ??= now;
    else if (t.dir === 'out') finish(t, 'cancelled', `${nameOf(peer)} left the room`);
    else abortIn(t, 'cancelled', `${nameOf(peer)} left the room`);
  }
}

function rejoined(peer) {
  for (const t of outgoing.values()) {
    if (t.peer !== peer || !live(t)) continue;
    // An offer not answered yet goes again (the receiver keeps one per id); a transfer under way waits for the
    // receiver's accept, which says where to go on from.
    if (t.state === 'offered') {
      t.gone = null;
      send(peer, t.offer);
    } else send(peer, { t: 'resume', id: t.id });
  }
  for (const t of incoming.values()) {
    if (t.peer !== peer || !live(t)) continue;
    t.gone = null;
    if (t.state === 'receiving') {
      // Once the pieces still queued are written: the sender goes on from the first one we don't have.
      t.chain = t.chain.then(() => {
        if (t.state !== 'receiving' || t.gone !== null) return;
        t.next = t.have;
        send(peer, { t: 'accept', id: t.id, have: t.have });
      });
      t.note = `${nameOf(peer)} is back: resuming from piece ${t.have + 1}`;
    }
  }
}

function tick() {
  const now = Date.now();
  for (const t of [...outgoing.values(), ...incoming.values()]) {
    if (!live(t) || t.gone === null || now - t.gone < RESUME_MS) continue;
    if (t.dir === 'out') finish(t, 'failed', `${nameOf(t.peer)} didn’t come back`);
    else abortIn(t, 'failed', `${nameOf(t.peer)} didn’t come back`);
  }
  // A link that turned direct, or a window that the acks opened while a loop wasn't running.
  for (const peer of new Set([...outgoing.values()].filter((t) => t.state === 'sending').map((t) => t.peer))) pump(peer);
  for (const t of [...outgoing.values(), ...incoming.values()]) if (live(t)) render(t);
  renderPeers();
}

// ---------- rendering ----------

function stateOf(t) {
  const who = nameOf(t.peer);
  if (t.state === 'done') return ['ok', t.note];
  if (t.state === 'declined') return ['bad', t.dir === 'out' ? `${who} declined` : 'declined'];
  if (t.state === 'cancelled' || t.state === 'failed') return ['bad', t.note];
  if (t.gone !== null && present(t.peer)) return ['', `${who} is back: resuming…`];
  if (t.gone !== null) return ['warn', `${who} left: waiting for them to come back (${eta((RESUME_MS - Date.now() + t.gone) / 1000)} left)`];
  if (t.state === 'offered') {
    if (t.dir === 'out') return ['', `offered to ${who}: waiting for an answer`];
    if (t.note) return ['warn', t.note];
    if (saveMode === 'memory' && t.size > BLOB_MAX) return ['warn', `too large to receive in this browser (over ${bytes(BLOB_MAX)})`];
    return ['', `${who} offers this file`];
  }
  if (!direct(t.peer)) return ['warn', 'waiting for a direct link (files never go through the relays)'];
  if (t.dir === 'out') {
    if (head(t.peer) !== t && t.next === 0) return ['', `queued for ${who}`];
    if (t.acked === t.count) return ['', 'waiting for the receiver’s check…'];
    return ['', `sending to ${who}${t.note ? ` · ${t.note}` : ''}`];
  }
  return ['', t.note || `receiving from ${who}`];
}

// A transfer's row, made once and updated in place (the tick redraws live ones): its buttons are made again only
// when they change, so a click never lands on one that was just replaced.
function render(t) {
  let v = t.view;
  if (!v) {
    const row = el('li', `xfer ${t.dir}`);
    row.dataset.id = t.id;
    row.dataset.dir = t.dir;
    const top = el('div', 'top');
    top.append(el('span', 'dir', t.dir === 'out' ? '↑' : '↓'), el('span', 'name', t.name), el('span', 'size', bytes(t.size)));
    v = t.view = { row, state: el('div', 'state'), fill: el('i'), info: el('span', 'info'), actions: el('span', 'actions'), acts: null };
    v.hash = el('div', 'hash');
    const bar = el('div', 'bar');
    bar.append(v.fill);
    const foot = el('div', 'foot');
    foot.append(v.info, v.actions);
    row.append(top, v.state, bar, foot, v.hash);
    $('transfers').prepend(row);
    $('no-transfers').classList.add('hidden');
  }
  const done = t.dir === 'out' ? t.acked : t.have;
  const bytesDone = t.state === 'done' ? t.size : Math.min(t.size, done * t.piece);
  const pct = t.size ? (100 * bytesDone) / t.size : t.state === 'done' ? 100 : 0;
  const [cls, text] = stateOf(t);
  v.row.dataset.state = t.state;
  v.state.className = `state ${cls}`.trim();
  v.state.textContent = text;
  v.fill.style.width = `${pct.toFixed(1)}%`;
  if (t.state === 'sending' || t.state === 'receiving') {
    const bps = t.rate.bps;
    v.info.textContent = `${pct.toFixed(1)}% · ${rate(bps)}${bps > 0 ? ` · ETA ${eta((t.size - bytesDone) / bps)}` : ''}`;
  } else if (t.state === 'done' && t.ended > t.started) {
    v.info.textContent = `${rate(t.size / ((t.ended - t.started) / 1000))} on average · ${eta((t.ended - t.started) / 1000)}`;
  } else v.info.textContent = t.type;
  v.hash.textContent = t.sha ? `SHA-256 ${t.sha}` : '';
  v.hash.hidden = !t.sha;
  const offer = t.dir === 'in' && t.state === 'offered';
  const canAccept = offer && !t.accepting && present(t.peer) && !(saveMode === 'memory' && t.size > BLOB_MAX);
  const acts = `${offer}|${canAccept}|${live(t)}|${t.url}`;
  if (acts === v.acts) return;
  v.acts = acts;
  const buttons = [];
  if (offer) {
    const a = el('button', 'primary', 'Accept');
    a.dataset.act = 'accept';
    a.disabled = !canAccept;
    a.onclick = () => accept(t);
    const d = el('button', '', 'Decline');
    d.dataset.act = 'decline';
    d.onclick = () => decline(t);
    buttons.push(a, d);
  } else if (live(t)) {
    const c = el('button', 'danger', 'Cancel');
    c.dataset.act = 'cancel';
    c.onclick = () => (t.dir === 'out' ? finishAndTell(t, 'cancelled', 'you cancelled') : cancel(t, 'you cancelled'));
    buttons.push(c);
  }
  if (t.url) {
    const a = el('a', '', 'Save file');
    a.href = t.url;
    a.download = t.name;
    buttons.push(a);
  }
  v.actions.replaceChildren(...buttons);
}

// Forgets the oldest finished transfers past MAX_ROWS.
function prune() {
  const all = [...outgoing.values(), ...incoming.values()].filter((t) => !live(t)).sort((a, b) => a.ended - b.ended);
  for (let n = outgoing.size + incoming.size; n > MAX_ROWS && all.length; n--) {
    const t = all.shift();
    t.view?.row.remove();
    if (t.url) URL.revokeObjectURL(t.url);
    if (t.dir === 'out') outgoing.delete(t.id);
    else incoming.delete(keyOf(t.peer, t.id));
  }
}

// How a peer is reached: [pill class, pill text, detail].
function routeOf(peer) {
  const r = room.route(peer);
  if (!r) return ['relay', 'linking', 'connecting…'];
  const ms = r.rttMs ?? room.rtt(peer);
  const rtt = ms == null ? '' : ` · ${Math.round(ms)} ms`;
  if (F.directForFiles(r)) return ['direct', 'DIRECT', `direct link${rtt}`];
  if (r.kind === 'direct' && r.adopted) return ['punched', 'punched', `a punched channel${rtt}: files wait for the browsers’ own link`];
  if (r.kind === 'direct') return ['relay', 'TURN', `through a TURN server${rtt}: files wait for a direct link`];
  if (r.kind === 'peer') return ['relay', 'via peer', `relayed by ${nameOf(r.via)}${rtt}: files wait for a direct link`];
  return ['relay', 'via relay', `over the signalling relays${rtt}: files wait for a direct link`];
}

function renderPeers() {
  const peers = room ? room.peers : [];
  const items = peers.map((peer) => {
    const [kind, pill, detail] = routeOf(peer);
    const li = el('li', 'peer');
    li.dataset.peer = peer;
    li.dataset.route = kind;
    const who = el('div');
    who.append(el('div', 'name', nameOf(peer)), el('div', 'route', detail));
    li.append(who, el('span', `pill ${kind}`, pill));
    return li;
  });
  $('peers').replaceChildren(...items);
  $('peer-count').textContent = peers.length ? String(peers.length) : '';
  $('alone').classList.toggle('hidden', peers.length > 0 || !room);
  $('direct-hint').classList.toggle('hidden', !peers.some((p) => !direct(p)));
  // The target list keeps its choice while that peer stays.
  const sel = $('to');
  const was = sel.value;
  const opts = [['', `Everyone (${peers.length})`], ...peers.map((p) => [p, nameOf(p)])];
  if (sel.options.length !== opts.length || opts.some(([v, l], i) => sel.options[i].value !== v || sel.options[i].text !== l)) {
    sel.replaceChildren(...opts.map(([v, l]) => Object.assign(el('option', '', l), { value: v })));
    sel.value = opts.some(([v]) => v === was) ? was : '';
  }
  $('send').disabled = !picked.length || !peers.length;
  const n = outgoing.size + incoming.size;
  $('xfer-count').textContent = n ? String(n) : '';
  $('no-transfers').classList.toggle('hidden', n > 0);
}

function renderPicked() {
  $('picked').replaceChildren(...picked.map((f, i) => {
    const li = el('li');
    const x = el('button', '', '×');
    x.title = 'Remove';
    x.onclick = () => {
      picked.splice(i, 1);
      renderPicked();
    };
    li.append(el('span', 'name', F.cleanName(f.name)), el('span', 'dim', bytes(f.size)), x);
    return li;
  }));
  $('send-row').classList.toggle('hidden', !picked.length);
  renderPeers();
}

function renderAll() {
  renderPeers();
  for (const t of [...outgoing.values(), ...incoming.values()]) render(t);
}

function setStatus(kind, text) {
  const s = $('status');
  s.className = kind ? (kind === 'error' ? 'error' : '') : 'hidden';
  s.replaceChildren(...(kind === 'connecting' ? [el('span', 'spinner')] : []), ...(kind ? [el('span', '', text)] : []));
}

const inviteLink = (id) => `${location.origin}${location.pathname}${signaling === 'local' ? '?signal=local' : ''}#${encodeURIComponent(id)}`;
function setInvite(id) {
  const link = inviteLink(id);
  $('link').value = link;
  $('code').textContent = id;
  const how = signaling === 'local' ? 'tabs of this browser' : 'peer to peer, end-to-end encrypted';
  $('where').textContent = `${how} · room ${id.slice(0, 6)}… · direct links only`;
  try {
    const { size, modules } = qrCode(link, { ecl: 'M' });
    const q = 4;
    let d = '';
    for (let y = 0; y < size; y++) for (let x = 0; x < size; x++) if (modules[y][x]) d += `M${x + q} ${y + q}h1v1h-1z`;
    $('qr').setAttribute('viewBox', `0 0 ${size + 2 * q} ${size + 2 * q}`);
    $('qr').setAttribute('shape-rendering', 'crispEdges');
    $('qr-path').setAttribute('d', d);
  } catch {
    $('qr').removeAttribute('viewBox');
    $('qr-path').removeAttribute('d');
  }
}

// ---------- page wiring ----------

function addFiles(list) {
  for (const f of list) if (f instanceof File) picked.push(f);
  renderPicked();
}
const drop = $('drop');
for (const ev of ['dragenter', 'dragover']) {
  addEventListener(ev, (e) => {
    if (![...(e.dataTransfer?.types ?? [])].includes('Files')) return;
    e.preventDefault();
    drop.classList.add('over');
  });
}
addEventListener('dragleave', (e) => e.relatedTarget === null && drop.classList.remove('over'));
addEventListener('drop', (e) => {
  if (!e.dataTransfer?.files?.length) return;
  e.preventDefault();
  drop.classList.remove('over');
  addFiles(e.dataTransfer.files);
});
$('pick').addEventListener('change', (e) => {
  addFiles(e.target.files);
  e.target.value = '';
});
$('send').addEventListener('click', () => {
  if (!room || !picked.length) return;
  offerFiles(picked.splice(0), $('to').value || null);
  renderPicked();
});

function hashRoom() {
  let h;
  try { h = decodeURIComponent(location.hash.slice(1)).trim(); } catch { return null; }
  return h && fits(h) ? h : null;
}
// A room code or an invite link, as typed.
function roomFrom(v) {
  v = v.trim();
  try {
    const u = new URL(v);
    v = decodeURIComponent(u.hash.slice(1)).trim();
  } catch {}
  return v && fits(v) ? v : null;
}
function route() {
  const id = hashRoom();
  $('welcome').classList.toggle('hidden', Boolean(id));
  if (id && (id !== roomId || !room)) enter(id).catch((e) => setStatus('error', `Something went wrong: ${e.message}`));
}
$('welcome-form').addEventListener('submit', (e) => {
  e.preventDefault();
  location.hash = randomRoomId();
});
$('join-go').addEventListener('click', () => {
  const id = roomFrom($('join-code').value);
  $('welcome-warn').textContent = id ? '' : 'That isn’t a room code or an invite link.';
  $('welcome-warn').classList.toggle('hidden', Boolean(id));
  if (id) location.hash = encodeURIComponent(id);
});
$('join-code').addEventListener('keydown', (e) => e.key === 'Enter' && (e.preventDefault(), $('join-go').click()));
$('new').addEventListener('click', () => (location.hash = randomRoomId()));
$('copy').addEventListener('click', async (e) => {
  const b = e.currentTarget;
  let ok = false;
  try {
    await navigator.clipboard.writeText($('link').value);
    ok = true;
  } catch {
    $('link').select();
    try { ok = document.execCommand('copy'); } catch {}
  }
  b.textContent = ok ? 'Copied ✓' : 'Select and copy';
  setTimeout(() => (b.textContent = 'Copy'), 1600);
});
if (navigator.share) {
  $('share-row').classList.remove('hidden');
  $('share').addEventListener('click', () => navigator.share({ title: 'Peerwire Share', url: $('link').value }).catch(() => {}));
}
$('me').value = myName;
$('me').addEventListener('change', () => {
  const n = cleanNick($('me').value);
  $('me').value = n;
  myName = n;
  try { localStorage.setItem(NAME_KEY, n); } catch {}
  room?.setPresence({ name: n });
});
// The microphone: browsers offer this device's local addresses (a phone's hotspot one, say) only to connections
// made after a microphone or camera grant. Only the permission is needed: the microphone closes at once, and the room
// tries WebRTC again with every peer it reaches by fallback.
if (typeof navigator.mediaDevices?.getUserMedia !== 'function') $('mic').disabled = true;
$('mic').addEventListener('click', async () => {
  $('mic').disabled = true;
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    for (const t of stream.getTracks()) t.stop();
    $('mic-state').textContent = `allowed: retrying ${room?.retryDirect() ?? 0} link(s)`;
  } catch (e) {
    $('mic-state').textContent = `not allowed (${e?.name ?? 'error'})`;
    $('mic').disabled = false;
  }
});

addEventListener('hashchange', route);
setInterval(tick, TICK_MS);
pickMode();
route();

// For tests.
window.share = {
  get room() { return room; },
  get roomId() { return roomId; },
  get saveMode() { return saveMode; },
  set rtcOff(v) { rtcOff = Boolean(v); },
  outgoing,
  incoming,
  stats,
};
