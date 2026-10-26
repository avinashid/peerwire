// Peerwire chat: a room per URL hash, nicknames in presence, a typing indicator on the unreliable lane and messages
// on the reliable lane. Whatever members send is untrusted, so it reaches the page only as text (textContent and
// Text nodes, never innerHTML), and only after it is checked.
//
//   chat.html#<room>                 browsers anywhere, through the Nostr signalling relays
//   chat.html?signal=local#<room>    tabs of this browser (BroadcastChannel)

import { joinRoom, randomRoomId } from '../src/index.js';
import { qrCode } from './qr.js';

const APP = 'peerwire-chat';
const MAX_TEXT = 2000; // characters (UTF-16 units, as the textarea's maxlength counts them)
const MAX_NAME = 32;
const MAX_ROOM = 256; // bytes of the room id in a link (percent-encoded), so the link's QR code always fits
const TYPING_MS = 3000; // how long "is typing" lasts after the last typing frame
const TYPING_EVERY_MS = 1000; // how often a typing frame goes out while typing goes on
const KEEP = 500; // lines on screen
const GROUP_MS = 3 * 60 * 1000; // one sender's messages this close together share a name and avatar
const NAME_KEY = 'peerwire-chat/name';
const KEY_HINT = 'Anyone with this link can join: it is the room’s only key.';
const GUESSABLE = 'This room’s name can be guessed: anyone who guesses it can join and read along. Start a New room for a private one.';

const params = new URLSearchParams(location.search);
const signaling = params.get('signal') === 'local' ? 'local' : 'nostr';
const $ = (id) => document.getElementById(id);
const list = $('messages');
const scroller = $('scroll');
const input = $('text');

let myName = loadName();
let room = null;
let roomId = null;
let chatHistory = null;
let session = 0; // bumped on every join, so a join that finishes after another started is dropped
let joinedAt = 0;
let pending = null; // the room the welcome form joins
const names = new Map(); // member -> its nickname
const typing = new Map(); // member -> when its "is typing" ends
let typingShown = '';
let lastTyping = 0;
let last = null; // { from, at } of the last message on screen, to group a run of them

// An element whose `text` goes in as textContent, so markup in it stays text.
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

// A nickname from anyone: control and format characters out (bidi overrides, zero-width ones), whitespace
// collapsed, at most 32 characters. '' if nothing is left.
function cleanName(v) {
  if (typeof v !== 'string') return '';
  return [...v.replace(/[\p{Cc}\p{Cf}]/gu, '').replace(/\s+/g, ' ').trim()].slice(0, MAX_NAME).join('').trim();
}
function loadName() {
  try { return cleanName(localStorage.getItem(NAME_KEY)); } catch { return ''; }
}
// A room id fits in a link, and its QR code, at 256 bytes once percent-encoded.
const fits = (id) => encodeURIComponent(id).length <= MAX_ROOM;
// A room id that doesn't look random, like 'family', is a public lobby: anyone who guesses it can join. (A floor,
// not a proof: randomRoomId() gives 22 of these characters.)
const guessable = (id) => !/^[A-Za-z0-9_-]{22,}$/.test(id);
const hue = (id) => parseInt(id.slice(0, 6), 16) % 360;
const guest = (id) => `guest ${id.slice(0, 4)}`;
const nameOf = (peer) => (room && peer === room.self ? myName : names.get(peer) || guest(peer));
const clock = (t) => new Date(t).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });

// Messages, behind the one interface the page uses: `post({ name, text })` sends one, `onEntry` gets every
// message from anyone (ours too) as { from, name, text, at }, and `close()` stops. This one sends each message
// over the reliable lane to the members present, so a member who joins later sees what is said from then on.
// A shared log, room.doc('chat').log('messages', { keep: 500 }), keeps the last 500 for late joiners: a history
// over it replaces this function.
function liveHistory(r, onEntry) {
  const off = r.on('message', (data, from, info) => {
    const m = info.lane === 'reliable' && chatMessage(data);
    if (m) onEntry({ from, ...m, at: Date.now() });
  });
  return {
    hint: 'You’ll see messages sent from now on.',
    post({ name, text }) {
      r.send({ t: 'msg', name, text }); // throws, and sends nothing, if the link can't take it now
      onEntry({ from: r.self, name, text, at: Date.now() });
    },
    close: off,
  };
}

// A member's chat message as { name, text }, or null if it isn't one.
function chatMessage(d) {
  if (!d || typeof d !== 'object' || d.t !== 'msg' || typeof d.text !== 'string' || d.text.length > MAX_TEXT) return null;
  const text = d.text.trim();
  return text ? { name: cleanName(d.name), text } : null;
}

async function enter(id) {
  const my = ++session;
  closeDrawer();
  setComposer(false);
  if (room) {
    const r = room;
    room = null;
    chatHistory.close();
    chatHistory = null;
    await r.leave();
    if (my !== session) return; // a newer join started meanwhile
  }
  roomId = id;
  names.clear();
  typing.clear();
  last = null;
  list.replaceChildren();
  renderTyping();
  renderMembers();
  setStatus('connecting', signaling === 'local' ? 'Joining…' : 'Joining through the Nostr signalling relays…');
  setInvite(id);
  let r;
  try {
    r = await joinRoom({ appId: APP, roomId: id, signaling, presence: { name: myName } });
  } catch (e) {
    if (my === session) setStatus('error', `Couldn’t join: ${e.message}`, { label: 'Retry', run: () => join(id) });
    return;
  }
  if (my !== session) return void r.leave();
  room = r;
  joinedAt = Date.now();
  listen(r);
  chatHistory = liveHistory(r, (e) => r === room && addMessage(e));
  setStatus(null);
  setComposer(true);
  system(['You joined as ', [myName], `. ${chatHistory.hint}`]);
  if (guessable(id)) system([GUESSABLE], 'warn');
  renderMembers();
}

// enter(), with anything it throws shown, rather than left as a page that never joins.
function join(id) {
  const done = enter(id);
  const my = session;
  done.catch((e) => {
    if (my !== session) return;
    setComposer(false);
    setStatus('error', `Something went wrong: ${e.message}`, { label: 'Retry', run: () => join(id) });
  });
}

function listen(r) {
  r.on('peer-join', (peer, presence) => {
    if (r !== room) return;
    names.set(peer, cleanName(presence?.name) || guest(peer));
    system([[nameOf(peer)], Date.now() - joinedAt < 3000 ? ' is here' : ' joined']);
    renderMembers();
  });
  r.on('presence', (peer, presence) => {
    if (r !== room) return;
    const before = names.get(peer);
    const now = cleanName(presence?.name) || guest(peer);
    names.set(peer, now);
    if (before && before !== now) system([[before], ' is now ', [now]]);
    renderMembers();
  });
  r.on('peer-leave', (peer, reason) => {
    if (r !== room) return;
    typing.delete(peer);
    system([[nameOf(peer)], reason === 'timeout' ? ' lost the connection' : ' left']);
    names.delete(peer);
    renderMembers();
    renderTyping();
  });
  r.on('message', (data, peer, info) => {
    if (r !== room || info.lane !== 'unreliable' || data?.t !== 'typing') return;
    typing.set(peer, Date.now() + TYPING_MS);
    renderTyping();
  });
}

function avatar(id, name) {
  const a = el('span', 'avatar', [...name][0] ?? '?');
  a.style.setProperty('--h', hue(id));
  return a;
}

function addMessage({ from, name, text, at }) {
  const mine = from === room.self;
  const who = mine ? myName : names.get(from) || cleanName(name) || guest(from);
  const cont = last?.from === from && at - last.at < GROUP_MS;
  const li = el('li', `msg${mine ? ' mine' : ''}${cont ? ' cont' : ''}`);
  li.style.setProperty('--h', hue(from));
  li.dataset.from = from;
  const meta = el('div', 'meta');
  meta.append(el('span', 'name', who), el('time', '', clock(at)));
  const bubble = el('div', 'bubble', text);
  bubble.dir = 'auto';
  const body = el('div', 'body');
  body.append(meta, bubble);
  li.append(avatar(from, who), body);
  typing.delete(from);
  renderTyping();
  push(li);
  last = { from, at };
}

// A line from the page itself: strings as text, [name] in bold (as text too).
function system(parts, cls = '') {
  const li = el('li', `sys ${cls}`.trim());
  for (const p of parts) li.append(Array.isArray(p) ? el('b', '', p[0]) : p);
  push(li);
  last = null;
}

const nearBottom = () => scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
function scrollDown() {
  scroller.scrollTop = scroller.scrollHeight;
  $('newer').classList.add('hidden');
}
function push(li) {
  const stick = nearBottom();
  list.append(li);
  while (list.children.length > KEEP) list.firstElementChild.remove();
  if (stick || li.classList.contains('mine')) scrollDown();
  else $('newer').classList.remove('hidden');
  renderEmpty();
}
function renderEmpty() {
  $('empty').classList.toggle('hidden', !room || room.peers.length > 0 || list.querySelector('.msg') !== null);
}

function renderTyping() {
  const now = Date.now();
  for (const [peer, until] of typing) if (until <= now || !room?.peers.includes(peer)) typing.delete(peer);
  const who = [...typing.keys()].map(nameOf);
  const text = who.length === 0 ? '' : who.length === 1 ? `${who[0]} is typing…`
    : who.length === 2 ? `${who[0]} and ${who[1]} are typing…` : `${who.length} people are typing…`;
  if (text === typingShown) return; // redrawing would restart the dots
  typingShown = text;
  const dots = el('span', 'dots');
  dots.append(el('i'), el('i'), el('i'));
  $('typing').replaceChildren(...(text ? [dots, text] : []));
}

// How a member is reached: [pill class, pill text, detail].
function routeOf(peer) {
  const r = room.route(peer);
  if (!r) return ['wait', 'linking', 'connecting…'];
  const ms = r.rttMs ?? room.rtt(peer);
  const rtt = ms == null ? '' : ` · ${Math.round(ms)} ms`;
  if (r.kind === 'direct') return ['direct', 'direct', `direct link${rtt}`];
  if (r.kind === 'peer') return ['peer', 'via peer', `relayed by ${nameOf(r.via)}${rtt}`];
  return ['signaling', 'relayed', `over the signalling relays${rtt}`];
}

function renderMembers() {
  const items = [];
  if (room) {
    const me = el('li', 'member');
    me.dataset.peer = room.self;
    const who = el('div', 'who');
    const name = el('div', 'name', myName);
    name.append(el('span', 'dim', ' (you)'));
    who.append(name, el('div', 'route', signaling === 'local' ? 'tabs of this browser' : relayText()));
    me.append(avatar(room.self, myName), who);
    items.push(me);
    for (const peer of room.peers) {
      const [kind, pill, detail] = routeOf(peer);
      const li = el('li', 'member');
      li.dataset.peer = peer;
      li.dataset.route = kind;
      const w = el('div', 'who');
      w.append(el('div', 'name', nameOf(peer)), el('div', 'route', detail));
      li.append(avatar(peer, nameOf(peer)), w, el('span', `pill ${kind}`, pill));
      items.push(li);
    }
  }
  $('members').replaceChildren(...items);
  const n = room ? String(room.peers.length + 1) : '';
  $('count').textContent = n || '1';
  $('member-count').textContent = n;
  renderEmpty();
}
function relayText() {
  const open = room?.signaling.stats?.relaysOpen ?? 0;
  return open ? `Nostr signalling · ${open} relay${open > 1 ? 's' : ''} open` : 'no signalling relay open yet: retrying';
}

function setStatus(kind, text, action) {
  const s = $('status');
  s.className = kind ? (kind === 'error' ? 'error' : '') : 'hidden';
  const parts = kind === 'connecting' ? [el('span', 'spinner')] : [];
  if (kind) parts.push(el('span', '', text));
  if (action) {
    const b = el('button', '', action.label);
    b.type = 'button';
    b.onclick = action.run;
    parts.push(b);
  }
  s.replaceChildren(...parts);
}

function setComposer(on) {
  input.disabled = !on;
  input.placeholder = on ? 'Message' : 'Joining…';
  $('send').disabled = !on || !input.value.trim();
  if (on && matchMedia('(pointer: fine)').matches) input.focus();
}

// The link carries the room and ?signal=local, nothing else from this page's query.
const inviteLink = (id) => `${location.origin}${location.pathname}${signaling === 'local' ? '?signal=local' : ''}#${encodeURIComponent(id)}`;
function setInvite(id) {
  const link = inviteLink(id);
  const open = guessable(id);
  $('link').value = link;
  $('where').textContent = `${signaling === 'local' ? 'tabs of this browser' : 'peer to peer, end-to-end encrypted'} · room ${id.slice(0, 6)}…${open ? ' · its name can be guessed' : ''}`;
  $('where').classList.toggle('warn', open);
  $('key-hint').textContent = open ? GUESSABLE : KEY_HINT;
  $('key-hint').classList.toggle('warn', open);
  try {
    const { size, modules } = qrCode(link, { ecl: 'M' });
    const q = 4; // the quiet zone around it, in modules
    let d = '';
    for (let y = 0; y < size; y++) for (let x = 0; x < size; x++) if (modules[y][x]) d += `M${x + q} ${y + q}h1v1h-1z`;
    $('qr').setAttribute('viewBox', `0 0 ${size + 2 * q} ${size + 2 * q}`);
    $('qr').setAttribute('shape-rendering', 'crispEdges');
    $('qr-path').setAttribute('d', d);
  } catch {
    // Too long for a QR code: none is shown (the CSS hides #qr without a viewBox); the link and Copy still work.
    $('qr').removeAttribute('viewBox');
    $('qr-path').removeAttribute('d');
  }
}

async function copyLink(btn) {
  btn.dataset.label ??= btn.textContent;
  let ok = false;
  try {
    await navigator.clipboard.writeText($('link').value);
    ok = true;
  } catch {
    // No clipboard API on plain http:// (a LAN address, say): copy from the selected field instead.
    $('link').select();
    try { ok = document.execCommand('copy'); } catch {}
  }
  btn.textContent = ok ? 'Copied ✓' : 'Select and copy';
  clearTimeout(btn.timer);
  btn.timer = setTimeout(() => (btn.textContent = btn.dataset.label), 1600);
}

// Our nickname, cleaned, remembered and shared. False if nothing is left of it.
function setName(v) {
  const n = cleanName(v);
  if (!n) return false;
  $('me').value = n;
  if (n === myName) return true;
  myName = n;
  try { localStorage.setItem(NAME_KEY, n); } catch {}
  if (room) {
    room.setPresence({ name: n });
    system(['You are now ', [n]]);
    renderMembers();
  }
  return true;
}

function hashRoom() {
  let h;
  try { h = decodeURIComponent(location.hash.slice(1)).trim(); } catch { return null; }
  return h && fits(h) ? h : null;
}
// An invite link or a bare room name, as typed into the welcome form.
function roomFrom(v) {
  v = v.trim();
  try {
    const u = new URL(v);
    if (u.origin === location.origin && u.pathname === location.pathname) return { href: u.href };
    v = decodeURIComponent(u.hash.slice(1)).trim();
  } catch {}
  return v && fits(v) ? { id: v } : null;
}

const BAD_ROOM = 'That room name isn’t valid: it is malformed or longer than 256 bytes. Start a new room, or paste another link.';
function welcomeWarn(text) {
  $('welcome-warn').textContent = text;
  $('welcome-warn').classList.toggle('hidden', !text);
}
// The welcome form: for the room `id` if there is one, else to start one. `bad` if the link's room isn't valid.
function showWelcome(id, bad = false) {
  pending = id;
  $('welcome').classList.remove('hidden');
  $('name').value = myName;
  $('welcome-title').textContent = id ? 'You’re invited to a chat' : 'Peerwire chat';
  $('go').textContent = id ? 'Join the room' : 'Start a new room';
  $('join-other').classList.toggle('hidden', Boolean(id));
  welcomeWarn(bad ? BAD_ROOM : id && guessable(id) ? GUESSABLE : '');
  (myName ? $('go') : $('name')).focus();
}
function route() {
  const id = hashRoom();
  if (id && myName) {
    $('welcome').classList.add('hidden');
    if (id !== roomId || !room) join(id);
  } else showWelcome(id, !id && location.hash.length > 1);
}

const openDrawer = () => { $('side').classList.add('open'); $('backdrop').classList.add('open'); };
const closeDrawer = () => { $('side').classList.remove('open'); $('backdrop').classList.remove('open'); };

$('welcome-form').addEventListener('submit', (e) => {
  e.preventDefault();
  if (!setName($('name').value)) return $('name').focus();
  if (pending) route();
  else location.hash = randomRoomId();
});
$('join-go').addEventListener('click', () => {
  const to = roomFrom($('join-link').value);
  if (!to) return welcomeWarn($('join-link').value.trim() ? BAD_ROOM : ''), $('join-link').focus();
  if (!setName($('name').value)) return $('name').focus();
  if (to.href) location.href = to.href;
  else location.hash = encodeURIComponent(to.id);
});
$('join-link').addEventListener('keydown', (e) => e.key === 'Enter' && (e.preventDefault(), $('join-go').click()));
for (const b of [$('new'), $('side-new')]) b.addEventListener('click', () => (location.hash = randomRoomId()));
$('copy').addEventListener('click', (e) => copyLink(e.currentTarget));
$('empty-copy').addEventListener('click', (e) => copyLink(e.currentTarget));
if (navigator.share) {
  $('share').classList.remove('hidden');
  $('share').addEventListener('click', () => navigator.share({ title: 'Peerwire chat', url: $('link').value }).catch(() => {}));
}
$('me').addEventListener('change', () => setName($('me').value) || ($('me').value = myName));
$('people').addEventListener('click', () => ($('side').classList.contains('open') ? closeDrawer() : openDrawer()));
$('backdrop').addEventListener('click', closeDrawer);
addEventListener('keydown', (e) => e.key === 'Escape' && closeDrawer());
$('newer').addEventListener('click', scrollDown);
scroller.addEventListener('scroll', () => nearBottom() && $('newer').classList.add('hidden'));

function send() {
  const text = input.value.trim();
  if (!room || !chatHistory || !text || text.length > MAX_TEXT) return;
  try {
    chatHistory.post({ name: myName, text });
  } catch (e) {
    return system([`Not sent: ${e.message}`], 'warn');
  }
  input.value = '';
  lastTyping = 0;
  edited();
}
function edited() {
  input.style.height = 'auto';
  input.style.height = `${Math.min(input.scrollHeight, 150)}px`;
  const n = input.value.length;
  $('left').textContent = n > MAX_TEXT - 200 ? `${n} / ${MAX_TEXT}` : '';
  $('left').classList.toggle('over', n >= MAX_TEXT);
  $('send').disabled = !room || !input.value.trim();
}
input.addEventListener('input', () => {
  edited();
  const now = Date.now();
  if (room?.peers.length && input.value.trim() && now - lastTyping >= TYPING_EVERY_MS) {
    lastTyping = now;
    try { room.send({ t: 'typing' }, { lane: 'unreliable' }); } catch {}
  }
});
input.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    send();
  }
});
$('composer').addEventListener('submit', (e) => {
  e.preventDefault();
  send();
});

addEventListener('hashchange', route);
setInterval(renderTyping, 250);
setInterval(() => room && renderMembers(), 1000);
$('me').value = myName;
route();

// For tests.
window.chat = {
  get room() { return room; },
  get roomId() { return roomId; },
  get name() { return myName; },
};
