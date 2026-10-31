// Peerwire Meet: a video call of up to 6 browsers, peer to peer, on joinRoom and room.call. The lobby previews the
// camera and asks for it, and the microphone, only when the person clicks; the call is a grid of tiles, one per
// member, with its name, its mute and camera marks (from presence) and the state of its media link. Whatever members
// send is untrusted: names reach the page only as text (textContent), and only after they are cleaned.
//
//   meet.html#<room>                 browsers anywhere, through the Nostr signalling relays
//   meet.html?signal=local#<room>    tabs of this browser (BroadcastChannel)
//   ?max=N                           this tab joins calls of at most N people (2 to 6)

import { joinRoom, randomRoomId } from '../src/index.js';

const APP = 'peerwire-meet';
const MAX_CALL = 6;
const MAX_NAME = 32;
const MAX_ROOM = 256; // bytes of the room id in a link (percent-encoded)
const MAX_REASON = 60; // characters of a failed link's reason on its tile
const NAME_KEY = 'peerwire-meet/name';
const CHAT_NAME_KEY = 'peerwire-chat/name';
const VIDEO = { width: { ideal: 640 }, height: { ideal: 360 }, frameRate: { ideal: 24, max: 30 } };
const SPEAKING = 0.03; // RMS level of the microphone above which a member counts as speaking
const SPEAKING_HOLD_MS = 600; // how long the ring stays after the level drops
const SELF = 'self';
const GUESSABLE = 'Anyone who guesses this name can join. Leave it empty, or press New, for a private room.';
const PRIVATE = 'Leave it empty for a new private room.';
const MEDIA_OFF = 'Nothing is recorded or uploaded. Your browser asks for the camera and microphone only when you turn them on.';
const MEDIA_ON = 'Nothing is recorded or uploaded: your camera and microphone go only to the people in the call.';
const NO_MEDIA = 'This page can’t use a camera or microphone: browsers allow them only on https:// pages and on localhost. '
  + 'You can still join to watch and listen.';
// Why a member's media link failed, in a few words, for the reasons the call gives.
const REASONS = { 'no-path': 'no direct path', lost: 'connection lost', error: 'error' };

const params = new URLSearchParams(location.search);
const signaling = params.get('signal') === 'local' ? 'local' : 'nostr';
const max = Math.min(MAX_CALL, Math.max(2, Number.parseInt(params.get('max'), 10) || MAX_CALL));
const $ = (id) => document.getElementById(id);
const grid = $('grid');

let myName = loadName();
let stream = new MediaStream(); // our camera and microphone tracks, as many as are on; the call sends this
let micOn = true; // whether the microphone track, when there is one, is enabled
let want = { audio: false, video: false }; // what was on when we left, to turn back on for Rejoin
const chosen = { audio: '', video: '' }; // the device ids picked in the lists ('' for the default)
let room = null;
let call = null;
let roomId = null;
let session = 0; // bumped on every join and leave, so a join that finishes after another started is dropped
let sentPresence = '';
let audioCtx = null;
let selfMeter = null;
const tiles = new Map(); // member (or SELF) -> its tile

// An element whose `text` goes in as textContent, so markup in it stays text.
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}
function icon(id) {
  const NS = 'http://www.w3.org/2000/svg';
  const s = document.createElementNS(NS, 'svg');
  s.setAttribute('class', 'i');
  const u = document.createElementNS(NS, 'use');
  u.setAttribute('href', `#${id}`);
  s.append(u);
  return s;
}

// A name from anyone: control and format characters out (bidi overrides, zero-width ones), whitespace collapsed,
// at most 32 characters. '' if nothing is left.
function cleanName(v) {
  if (typeof v !== 'string') return '';
  return [...v.replace(/[\p{Cc}\p{Cf}]/gu, '').replace(/\s+/g, ' ').trim()].slice(0, MAX_NAME).join('').trim();
}
function loadName() {
  try { return cleanName(localStorage.getItem(NAME_KEY) ?? localStorage.getItem(CHAT_NAME_KEY)); } catch { return ''; }
}
const fits = (id) => encodeURIComponent(id).length <= MAX_ROOM;
// A room id that doesn't look random, like 'family', is a public lobby: anyone who guesses it can join.
const guessable = (id) => !/^[A-Za-z0-9_-]{22,}$/.test(id);
const hue = (id) => (id === SELF ? 225 : parseInt(id.slice(0, 6), 16) % 360);
const guest = (id) => `guest ${id.slice(0, 4)}`;
const initial = (name) => [...name][0] ?? '?';

// ---- Camera and microphone ----

const hasAudio = () => stream.getAudioTracks().length > 0;
const hasVideo = () => stream.getVideoTracks().length > 0;
// What we tell the room: our name, and whether our microphone and camera are on.
const presenceNow = () => ({ name: myName, mic: hasAudio() && micOn, cam: hasVideo() });

function constraints(kind) {
  if (kind === 'audio') return chosen.audio ? { deviceId: { exact: chosen.audio } } : true;
  return chosen.video ? { ...VIDEO, deviceId: { exact: chosen.video } } : VIDEO;
}
const what = (kinds) => (kinds.length === 2 ? 'camera and microphone' : kinds[0] === 'audio' ? 'microphone' : 'camera');
function mediaError(e, kinds) {
  const n = e?.name;
  const thing = what(kinds);
  if (n === 'NotAllowedError' || n === 'SecurityError') {
    const them = kinds.length === 2 ? 'them' : 'it';
    return `The browser blocked the ${thing}. To allow ${them}, open the site settings (the icon at the left of the address bar), `
      + `allow ${them}, then try again. You can still join to watch and listen.`;
  }
  if (n === 'NotFoundError' || n === 'OverconstrainedError') return `No ${thing} found.`;
  if (n === 'NotReadableError' || n === 'AbortError') {
    return `The ${thing} is in use by another app, or the system blocked it. Close the other app and try again.`;
  }
  return `Couldn’t start the ${thing}: ${e?.message || n || 'unknown error'}.`;
}

// Asks for `kinds` ('audio', 'video'), and only ever from a click: browsers prompt on getUserMedia. What it gets goes
// into our stream in place of that kind's old track (so it also switches devices); a kind it can't get is reported
// and the other still comes on. Two kinds at once are one prompt, and asked again one by one if that fails for a
// reason other than a refusal (a laptop with no camera still gets its microphone).
async function acquire(kinds, { keep = false } = {}) {
  if (!navigator.mediaDevices?.getUserMedia) return report(NO_MEDIA, 'bad');
  const busy = [$('allow'), $('pre-mic'), $('pre-cam'), $('mic'), $('cam')];
  for (const b of busy) b.disabled = true;
  const got = [];
  const failed = [];
  try {
    try {
      got.push(...(await navigator.mediaDevices.getUserMedia(Object.fromEntries(kinds.map((k) => [k, constraints(k)])))).getTracks());
    } catch (e) {
      if (kinds.length === 1 || e?.name === 'NotAllowedError' || e?.name === 'SecurityError') failed.push([e, kinds]);
      else {
        for (const k of kinds) {
          try {
            got.push(...(await navigator.mediaDevices.getUserMedia({ [k]: constraints(k) })).getTracks());
          } catch (e2) {
            failed.push([e2, [k]]);
          }
        }
      }
    }
  } finally {
    for (const b of busy) b.disabled = false;
  }
  for (const t of got) {
    for (const old of stream.getTracks().filter((o) => o.kind === t.kind)) {
      old.stop();
      stream.removeTrack(old);
    }
    stream.addTrack(t);
    t.addEventListener('ended', () => lost(t));
    if (t.kind === 'audio' && !keep) micOn = true;
  }
  $('allow').textContent = failed.length && !got.length ? 'Try again' : 'Turn on camera and microphone';
  if (failed.length) report(failed.map(([e, k]) => mediaError(e, k)).join(' '), failed.some(([e]) => e?.name === 'NotAllowedError') ? 'bad' : 'warn');
  else if (got.length) report(null);
  if (got.length) {
    tracksChanged();
    listDevices();
  }
}

// A track the browser ended (a camera unplugged, a permission taken back).
function lost(t) {
  if (!stream.getTracks().includes(t)) return;
  stream.removeTrack(t);
  report(`The ${t.kind === 'audio' ? 'microphone' : 'camera'} stopped.`, 'warn');
  tracksChanged();
}

function stopVideo() {
  for (const t of stream.getVideoTracks()) {
    t.stop(); // so the camera's light goes off, not just the picture
    stream.removeTrack(t);
  }
  tracksChanged();
}
function stopMedia() {
  for (const t of stream.getTracks()) t.stop();
  stream = new MediaStream();
  selfMeter?.close();
  selfMeter = null;
}

// Our tracks changed: the call sends the new set, and everything showing them is redrawn.
function tracksChanged() {
  // The call swaps tracks (replaceTrack) and never renegotiates, so a camera turned on mid-call needs nothing else.
  if (call) Promise.resolve().then(() => call?.setStream(stream)).catch((e) => toast(`The call didn’t take the new tracks: ${e?.message ?? e}`));
  selfMeter?.close();
  selfMeter = hasAudio() ? meter(stream.getAudioTracks()[0]) : null;
  // Our own picture, like the preview: no source while the camera is off, set again when it comes back.
  const self = tiles.get(SELF);
  const src = hasVideo() ? stream : null;
  if (self && self.video.srcObject !== src) self.video.srcObject = src;
  if (self) self.hasFrames = hasVideo();
  showMedia();
}

// The microphone's enabled state, the buttons, the preview, our tile and our presence, from our tracks.
function showMedia() {
  for (const t of stream.getAudioTracks()) t.enabled = micOn;
  const mic = hasAudio() && micOn;
  const cam = hasVideo();
  for (const b of [$('pre-mic'), $('mic')]) {
    b.classList.toggle('off', !mic);
    b.firstElementChild.firstElementChild.setAttribute('href', mic ? '#i-mic' : '#i-mic-off');
    b.setAttribute('aria-label', mic ? 'Mute' : hasAudio() ? 'Unmute' : 'Turn microphone on');
  }
  for (const b of [$('pre-cam'), $('cam')]) {
    b.classList.toggle('off', !cam);
    b.firstElementChild.firstElementChild.setAttribute('href', cam ? '#i-cam' : '#i-cam-off');
    b.setAttribute('aria-label', cam ? 'Turn camera off' : 'Turn camera on');
  }
  const preview = $('preview');
  if (cam && preview.srcObject !== stream) preview.srcObject = stream;
  if (!cam) preview.srcObject = null;
  $('preview-off').classList.toggle('hidden', cam);
  $('preview-avatar').textContent = initial(myName || '?');
  $('preview-text').textContent = hasAudio() || cam ? 'Your camera is off' : 'Your camera and microphone are off';
  $('allow').classList.toggle('hidden', hasAudio() || cam);
  $('level').classList.toggle('hidden', !mic);
  renderTile(SELF);
  sharePresence();
}

function sharePresence() {
  if (!room) return;
  const p = presenceNow();
  const s = JSON.stringify(p);
  if (s === sentPresence) return;
  try {
    room.setPresence(p);
    sentPresence = s;
  } catch {}
}

// A level meter on one audio track; null where Web Audio isn't available. The AudioContext is made on a click (the
// join or a toggle), so browsers let it run.
function meter(track) {
  try {
    audioCtx ??= new AudioContext();
    if (audioCtx.state === 'suspended') audioCtx.resume().catch(() => {});
    const src = audioCtx.createMediaStreamSource(new MediaStream([track]));
    const an = audioCtx.createAnalyser();
    an.fftSize = 512;
    src.connect(an);
    const buf = new Float32Array(an.fftSize);
    return {
      level() {
        an.getFloatTimeDomainData(buf);
        let s = 0;
        for (const v of buf) s += v * v;
        return Math.sqrt(s / buf.length);
      },
      close() { src.disconnect(); },
    };
  } catch {
    return null;
  }
}

// The device lists, once the browser shows their names (after a grant).
async function listDevices() {
  let all;
  try { all = await navigator.mediaDevices.enumerateDevices(); } catch { return; }
  fillDevices($('cam-select'), all.filter((d) => d.kind === 'videoinput'), stream.getVideoTracks()[0], 'camera');
  fillDevices($('mic-select'), all.filter((d) => d.kind === 'audioinput'), stream.getAudioTracks()[0], 'microphone');
}
function fillDevices(select, devices, track, thing) {
  const named = devices.filter((d) => d.deviceId && d.label);
  const current = track?.getSettings?.().deviceId || chosen[track?.kind] || '';
  if (!named.length) {
    select.replaceChildren(el('option', '', devices.length ? `Turn the ${thing} on to choose` : `No ${thing} found`));
    select.disabled = true;
    return;
  }
  select.replaceChildren(...named.map((d) => {
    const o = el('option', '', d.label);
    o.value = d.deviceId;
    o.selected = d.deviceId === current;
    return o;
  }));
  select.disabled = false;
}
function pickDevice(kind, id) {
  chosen[kind] = id;
  const on = kind === 'audio' ? hasAudio() : hasVideo();
  if (on) acquire([kind], { keep: true });
}

function report(text, kind = '') {
  const m = $('media-msg');
  if (!$('call').classList.contains('hidden')) return text && toast(text);
  m.className = `hintbox ${kind}`.trim();
  m.textContent = text ?? (hasAudio() || hasVideo() ? MEDIA_ON : MEDIA_OFF);
}
function toast(text) {
  const t = $('toast');
  t.textContent = text;
  t.classList.remove('hidden');
  clearTimeout(t.timer);
  t.timer = setTimeout(() => t.classList.add('hidden'), Math.min(9000, 2500 + text.length * 40));
}

// ---- The call ----

const inviteLink = (id) => `${location.origin}${location.pathname}${signaling === 'local' ? '?signal=local' : ''}#${encodeURIComponent(id)}`;

async function enter(id) {
  const my = ++session;
  roomId = id;
  history.replaceState(null, '', `${location.pathname}${location.search}#${encodeURIComponent(id)}`);
  showCall(true);
  tileFor(SELF);
  tracksChanged();
  banner('connecting', signaling === 'local' ? 'Joining…' : 'Joining through the Nostr signalling relays…');
  let r;
  try {
    r = await joinRoom({ appId: APP, roomId: id, signaling, presence: presenceNow() });
  } catch (e) {
    if (my === session) banner('bad', `Couldn’t join: ${e.message}`, { label: 'Retry', run: () => join(id) });
    return;
  }
  if (my !== session) return void r.leave();
  room = r;
  sentPresence = JSON.stringify(presenceNow());
  if (typeof r.call !== 'function') return banner('bad', 'This build of @peerwire/js has no calls (room.call).');
  call = r.call(stream, { max });
  wireCall(r, call);
  sharePresence();
  renderAll();
}
// enter(), with anything it throws shown, rather than left as a page that never joins.
function join(id) {
  const done = enter(id);
  const my = session;
  done.catch((e) => my === session && banner('bad', `Something went wrong: ${e.message}`, { label: 'Retry', run: () => join(id) }));
}

// The call's events, onto the tiles. A member's tile comes with its first event and goes with 'ended', which the
// call gives for a member that leaves the call or the room.
function wireCall(r, c) {
  const live = () => r === room && c === call;
  c.on('state', (peer, state, reason) => {
    if (!live()) return;
    const t = tileFor(peer);
    t.state = state === 'connected' || state === 'failed' ? state : 'connecting';
    t.reason = state === 'failed' ? reasonText(reason) : '';
    renderTile(peer);
  });
  c.on('stream', (peer, s) => {
    if (!live() || !(s instanceof MediaStream)) return;
    const t = tileFor(peer);
    if (t.video.srcObject !== s) {
      t.video.srcObject = s;
      t.hasFrames = false;
      s.addEventListener('addtrack', () => listenAudio(t, s));
    }
    listenAudio(t, s);
    play(t.video);
  });
  c.on('ended', (peer) => live() && dropTile(peer));
  c.on('full', (seats) => live() && callFull(seats));
  r.on('presence', (peer) => live() && renderTile(peer));
}
// No seat for us: back to the lobby, camera still on, with the choice to try again or start another room.
function callFull(seats) {
  leave({ keepMedia: true });
  const n = Number.isInteger(seats) && seats > 0 ? seats : max;
  $('full-text').textContent = `Every browser in a call sends its video to every other one, so this call stops at ${n} people. `
    + 'Try again when someone leaves, or start a new room.';
  $('full').classList.remove('hidden');
}
function reasonText(reason) {
  if (typeof reason !== 'string' || !reason) return 'couldn’t connect';
  return REASONS[reason] ?? [...reason.replace(/[\p{Cc}\p{Cf}]/gu, '')].slice(0, MAX_REASON).join('');
}
function listenAudio(t, s) {
  const a = s.getAudioTracks()[0];
  if (!a || t.audioTrack === a) return;
  t.meter?.close();
  t.audioTrack = a;
  t.meter = meter(a);
}
function play(video) {
  video.play().catch((e) => e?.name === 'NotAllowedError' && $('unmute').classList.remove('hidden'));
}

function tileFor(peer) {
  let t = tiles.get(peer);
  if (t) return t;
  const box = el('div', `tile${peer === SELF ? ' self' : ''}`);
  box.dataset.peer = peer;
  box.style.setProperty('--h', hue(peer));
  const video = el('video');
  video.playsInline = true;
  video.autoplay = true;
  if (peer === SELF) video.muted = true; // never play our own microphone back
  const face = el('div', 'face');
  const av = el('span', 'avatar');
  face.append(av);
  const badge = el('span', 'badge');
  const label = el('div', 'label');
  const muted = el('span', 'muted');
  muted.append(icon('i-mic-off'));
  muted.title = 'Muted';
  const name = el('span', 'name');
  label.append(muted, name);
  box.append(video, face, badge, label);
  t = { box, video, av, badge, name, state: peer === SELF ? 'connected' : 'connecting', reason: '', hasFrames: false, meter: null, audioTrack: null, loudUntil: 0 };
  // The picture shows once frames arrive, so a member's tile shows their initial, not a black box, until then.
  video.addEventListener('resize', () => {
    t.hasFrames = video.videoWidth > 0;
    renderTile(peer);
  });
  tiles.set(peer, t);
  if (peer === SELF) grid.prepend(box);
  else grid.append(box);
  layout();
  renderCount();
  return t;
}
function dropTile(peer) {
  const t = tiles.get(peer);
  if (!t) return;
  t.meter?.close();
  t.video.srcObject = null;
  t.box.remove();
  tiles.delete(peer);
  layout();
  renderCount();
}
function nameOf(peer) {
  if (peer === SELF) return myName;
  return cleanName(room?.getPresence(peer)?.name) || guest(peer);
}
function renderTile(peer) {
  const t = tiles.get(peer);
  if (!t) return;
  const p = peer === SELF ? presenceNow() : room?.getPresence(peer);
  const name = nameOf(peer);
  const mic = p?.mic !== false; // a member that doesn't say counts as on
  const cam = p?.cam !== false && t.hasFrames;
  t.name.textContent = peer === SELF ? `${name} (you)` : name;
  t.av.textContent = initial(name);
  t.box.classList.toggle('nomic', !mic);
  t.box.classList.toggle('nocam', !cam);
  t.box.dataset.state = t.state;
  t.badge.replaceChildren(...(t.state === 'connecting' ? [el('span', 'spinner'), 'connecting…']
    : t.state === 'failed' ? [`failed · ${t.reason}`] : ['● connected']));
  t.box.title = t.state === 'failed' ? `${name}: the media link failed (${t.reason}); you are still in the same room.` : '';
}
function renderAll() {
  for (const peer of tiles.keys()) renderTile(peer);
  renderCount();
}
function renderCount() {
  const n = tiles.size;
  const pill = $('count-pill');
  pill.textContent = `${n} of ${max}`;
  pill.className = `pill ${n >= max ? 'warn' : 'info'}`;
  if (!room) return;
  if (n <= 1) banner('info', 'You’re the only one here. Send the invite link to someone: when they open it, they join this call.', { label: 'Copy invite link', run: copyInvite });
  else banner(null);
}

// The tile size that fills the grid best for this many tiles: every column count is tried, each tile kept between
// 4:3 and 16:9 (between 3:4 and 16:9 on a grid taller than wide, a phone held upright) so a camera's picture isn't
// cropped much, and the largest wins: two people stack on a phone and sit side by side on a laptop.
function layout() {
  const n = tiles.size;
  if (!n || $('call').classList.contains('hidden')) return;
  const cs = getComputedStyle(grid);
  const gap = Number.parseFloat(cs.columnGap) || 10;
  const W = grid.clientWidth - Number.parseFloat(cs.paddingLeft) - Number.parseFloat(cs.paddingRight);
  const H = grid.clientHeight - Number.parseFloat(cs.paddingTop) - Number.parseFloat(cs.paddingBottom);
  const narrow = W >= H ? 4 / 3 : 3 / 4;
  let best = null;
  for (let cols = 1; cols <= n; cols++) {
    const rows = Math.ceil(n / cols);
    let w = (W - gap * (cols - 1)) / cols;
    let h = (H - gap * (rows - 1)) / rows;
    if (w <= 0 || h <= 0) continue;
    if (w / h > 16 / 9) w = h * 16 / 9;
    else if (w / h < narrow) h = w / narrow;
    if (!best || w * h > best.w * best.h + 1) best = { w, h };
  }
  if (!best) return;
  grid.style.setProperty('--tw', `${Math.floor(best.w)}px`);
  grid.style.setProperty('--th', `${Math.floor(best.h)}px`);
}

// Who is speaking: a green ring on a tile while its microphone is above the level, and a little after.
function speaking() {
  const now = performance.now();
  for (const [peer, t] of tiles) {
    const m = peer === SELF ? (micOn ? selfMeter : null) : t.meter;
    const level = m?.level() ?? 0;
    if (level > SPEAKING) t.loudUntil = now + SPEAKING_HOLD_MS;
    t.box.classList.toggle('speaking', t.loudUntil > now && t.state !== 'failed');
  }
  if (!$('lobby').classList.contains('hidden')) $('level').style.setProperty('--lvl', Math.min(1, (selfMeter?.level() ?? 0) * 8).toFixed(3));
}

function banner(kind, text, action) {
  const b = $('banner');
  if (!kind) return b.classList.add('hidden');
  b.className = `hintbox ${kind === 'connecting' || kind === 'info' ? '' : kind}`.trim();
  const parts = kind === 'connecting' ? [el('span', 'spinner'), el('span', '', text)] : [el('span', '', text)];
  if (action) {
    const btn = el('button', '', action.label);
    btn.type = 'button';
    btn.onclick = action.run;
    parts.push(btn);
  }
  b.replaceChildren(...parts);
}

// Leaves the call and the room. Our camera and microphone go off too, unless `keepMedia` (a full call, where the
// person will try again or start another).
function leave({ keepMedia = false, sheet = false } = {}) {
  session++;
  const c = call;
  const r = room;
  call = null;
  room = null;
  want = { audio: hasAudio(), video: hasVideo() };
  try { c?.leave(); } catch {}
  r?.leave().catch(() => {});
  if (!keepMedia) stopMedia();
  for (const peer of [...tiles.keys()]) dropTile(peer);
  $('unmute').classList.add('hidden');
  $('devpop').classList.add('hidden');
  showCall(false);
  showMedia();
  report(null);
  if (sheet) $('left').classList.remove('hidden');
}

function showCall(on) {
  document.body.classList.toggle('in-call', on);
  $('call').classList.toggle('hidden', !on);
  $('lobby').classList.toggle('hidden', on);
  $('roombar').classList.toggle('hidden', !on);
  $('count-pill').classList.toggle('hidden', !on);
  // One set of device lists: in the lobby's card, or in the call's pop-up.
  if (on) $('devpop').append($('devices'));
  else $('allow').parentElement.after($('devices'));
  $('settings').setAttribute('aria-expanded', 'false');
  if (on) {
    $('room-short').textContent = roomId;
    $('room-short').title = roomId;
    layout();
  } else lobbyRoom(roomId ?? hashRoom() ?? '');
}

async function copyInvite() {
  const link = inviteLink(roomId);
  let ok = false;
  try {
    await navigator.clipboard.writeText(link);
    ok = true;
  } catch {
    // No clipboard API on plain http:// (a LAN address, say): copy from a selected field instead.
    const i = el('input');
    i.value = link;
    document.body.append(i);
    i.select();
    try { ok = document.execCommand('copy'); } catch {}
    i.remove();
  }
  toast(ok ? 'Invite link copied. Anyone with it can join this call.' : `Copy the invite link: ${link}`);
}

// ---- The lobby ----

function setName(v) {
  const n = cleanName(v);
  if (!n) return false;
  $('name').value = n;
  if (n !== myName) {
    myName = n;
    try { localStorage.setItem(NAME_KEY, n); } catch {}
  }
  showMedia();
  return true;
}
function hashRoom() {
  let h;
  try { h = decodeURIComponent(location.hash.slice(1)).trim(); } catch { return null; }
  return h && fits(h) ? h : null;
}
// An invite link or a bare room name, as typed; '' for a new private room, null if it isn't valid.
function roomFrom(v) {
  v = v.trim();
  if (!v) return '';
  try {
    const u = new URL(v);
    v = decodeURIComponent(u.hash.slice(1)).trim();
  } catch {}
  return v && fits(v) ? v : null;
}
function lobbyRoom(id) {
  $('room-input').value = id;
  $('join-title').textContent = id ? 'Join the call' : 'Start a video call';
  roomHint();
}
function roomHint() {
  const id = roomFrom($('room-input').value);
  const h = $('room-hint');
  h.textContent = id === null ? 'That isn’t a valid room name or invite link (at most 256 bytes).' : id && guessable(id) ? GUESSABLE : PRIVATE;
  h.className = `hint${id === null || (id && guessable(id)) ? ' warn' : ''}`;
}

$('allow').addEventListener('click', () => acquire(['audio', 'video']));
for (const b of [$('pre-mic'), $('mic')]) {
  b.addEventListener('click', () => {
    if (!hasAudio()) return acquire(['audio']);
    micOn = !micOn;
    showMedia();
  });
}
for (const b of [$('pre-cam'), $('cam')]) b.addEventListener('click', () => (hasVideo() ? stopVideo() : acquire(['video'])));
$('cam-select').addEventListener('change', (e) => pickDevice('video', e.target.value));
$('mic-select').addEventListener('change', (e) => pickDevice('audio', e.target.value));
navigator.mediaDevices?.addEventListener?.('devicechange', listDevices);
$('random').addEventListener('click', () => {
  $('room-input').value = randomRoomId();
  roomHint();
});
$('room-input').addEventListener('input', roomHint);
$('name').addEventListener('input', () => ($('preview-avatar').textContent = initial(cleanName($('name').value) || '?')));
$('join-form').addEventListener('submit', (e) => {
  e.preventDefault();
  if (!setName($('name').value)) return $('join-msg').textContent = 'Type a name first: the others see it on your tile.', $('name').focus();
  const id = roomFrom($('room-input').value);
  if (id === null) return $('room-input').focus();
  $('join-msg').textContent = '';
  join(id || randomRoomId());
});
$('settings').addEventListener('click', () => {
  const open = $('devpop').classList.toggle('hidden') === false;
  $('settings').setAttribute('aria-expanded', String(open));
  if (open) listDevices();
});
$('invite').addEventListener('click', copyInvite);
$('copy').addEventListener('click', copyInvite);
$('leave').addEventListener('click', () => leave({ sheet: true }));
$('rejoin').addEventListener('click', async () => {
  $('left').classList.add('hidden');
  const kinds = ['audio', 'video'].filter((k) => want[k]);
  if (kinds.length) await acquire(kinds);
  join(roomId);
});
$('home').addEventListener('click', () => $('left').classList.add('hidden'));
$('full-retry').addEventListener('click', () => {
  $('full').classList.add('hidden');
  join(roomId);
});
$('full-new').addEventListener('click', () => {
  $('full').classList.add('hidden');
  join(randomRoomId());
});
$('unmute').addEventListener('click', () => {
  $('unmute').classList.add('hidden');
  audioCtx?.resume().catch(() => {});
  for (const t of tiles.values()) if (t.video.srcObject) play(t.video);
});
addEventListener('keydown', (e) => e.key === 'Escape' && !$('devpop').classList.contains('hidden') && $('settings').click());
addEventListener('hashchange', () => {
  const id = hashRoom();
  if (room && id && id !== roomId) leave({ keepMedia: true });
  if (!room) lobbyRoom(id ?? '');
});
new ResizeObserver(layout).observe(grid);
setInterval(speaking, 100);

$('sig-pill').textContent = signaling === 'local' ? 'tabs of this browser' : 'Nostr signalling';
$('name').value = myName;
lobbyRoom(hashRoom() ?? '');
showMedia();
report(null);
// A keyboard is there to type the name, but not on a phone, where focus would open it over the preview.
if (matchMedia('(pointer: fine)').matches) (myName ? $('join') : $('name')).focus({ preventScroll: true });

// For tests.
window.meet = {
  get room() { return room; },
  get call() { return call; },
  get roomId() { return roomId; },
  get stream() { return stream; },
  tiles: () => [...tiles].map(([peer, t]) => ({ peer, state: t.state, reason: t.reason, box: t.box.className })),
};
