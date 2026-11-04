// A room's connection policy (joinRoom's `policy`, room.setPolicy()): the paths a link may take, in the
// order to try them, and the servers they use.
//
// - 'direct': WebRTC over host and server-reflexive (STUN) candidates, with port prediction and
//   birthday punches;
// - 'turn': WebRTC through the TURN servers in iceServers;
// - 'peers': relayed by another member (the core relays for linked peers);
// - 'signaling': the pipe, datagrams over the signalling relays (webrtc.js).
//
// A path missing from `order` is off. The side that dials tries the first path first and later ones when
// the earlier ones fail or time out, and a link moves only to a path earlier than its own. WebRTC paths
// next to each other in the order are one WebRTC attempt (ICE picks between them); 'peers' and 'signaling'
// next to each other are one fallback stage, the first of them dialled PEER_FIRST_MS before the other
// (room.js). So TURN after a fallback is a WebRTC attempt of its own, made once the fallback has failed.

export const PATHS = Object.freeze(['direct', 'turn', 'peers', 'signaling']);
// joinRoom's `fallback`, as an order.
export const FALLBACK_ORDERS = Object.freeze({
  auto: PATHS,
  peers: Object.freeze(['direct', 'turn', 'peers']),
  off: Object.freeze(['direct', 'turn']),
});
const RTC = new Set(['direct', 'turn']);
const KEYS = new Set(['order', 'iceServers', 'relays', 'fallbackMs', 'sameNatFallbackMs', 'birthday', 'privacy']);
const ICE_URL = /^(stuns?|turns?):/i;
const TURN_URL = /^turns?:/i;

// `order` checked: a frozen copy. TypeError for anything but a non-empty list of distinct path names.
export function checkOrder(order) {
  if (!Array.isArray(order) || !order.length) throw new TypeError("policy order must be a non-empty array of 'direct', 'turn', 'peers' and 'signaling'");
  for (const p of order) {
    if (!PATHS.includes(p)) throw new TypeError(`unknown path ${typeof p === 'string' ? `'${p}'` : typeof p} in policy order: use 'direct', 'turn', 'peers' or 'signaling'`);
  }
  if (new Set(order).size !== order.length) throw new TypeError('a path appears twice in policy order');
  return Object.freeze([...order]);
}

// The order's stages: { rtc: { direct, turn } } for WebRTC paths next to each other, { fallback: [paths] } for
// fallback paths next to each other, in order.
export function stagesOf(order) {
  const out = [];
  for (const p of order) {
    const rtc = RTC.has(p);
    const last = out[out.length - 1];
    if (last && !!last.rtc === rtc) {
      if (rtc) last.rtc[p] = true;
      else last.fallback.push(p);
    } else out.push(rtc ? { rtc: { direct: p === 'direct', turn: p === 'turn' } } : { fallback: [p] });
  }
  return out;
}

// An entry room.policy gives (urls only) -> the entry it stands for, credentials and all, so the one given back
// to setPolicy keeps them.
const shown = new WeakMap();

// iceServers as room.policy gives them: each entry's urls, never its username or credential.
export function showServers(list) {
  return list.map((s) => {
    const out = Object.freeze({ urls: Array.isArray(s.urls) ? Object.freeze([...s.urls]) : s.urls });
    shown.set(out, s);
    return out;
  });
}

// `list` checked: a copy, with entries room.policy gave swapped back for the entries they stand for. TypeError
// unless each entry has urls of stun:, stuns:, turn: or turns:; the message names the entry by its index only,
// never its urls or credentials.
export function checkServers(list) {
  if (!Array.isArray(list)) throw new TypeError('iceServers must be an array of RTCIceServer');
  return list.map((x, i) => {
    const s = shown.get(x) ?? x;
    const urls = typeof s?.urls === 'string' ? [s.urls] : s?.urls;
    if (!Array.isArray(urls) || !urls.length || !urls.every((u) => typeof u === 'string' && ICE_URL.test(u))) {
      throw new TypeError(`iceServers[${i}] needs urls of stun:, stuns:, turn: or turns:`);
    }
    return s;
  });
}

// The servers a WebRTC attempt over `use` ({ direct, turn }) gets: STUN with 'direct', TURN with 'turn'. An
// entry that keeps all its urls is the same object.
export function serversFor(list, { direct, turn }) {
  const out = [];
  for (const s of list) {
    const all = [s.urls].flat();
    const urls = all.filter((u) => (TURN_URL.test(u) ? turn : direct));
    if (urls.length === all.length) out.push(s);
    else if (urls.length) out.push({ ...s, urls });
  }
  return out;
}

// Whether `list` has a TURN server.
export const hasTurn = (list) => list.some((s) => [s.urls].flat().some((u) => TURN_URL.test(u)));

// `list` checked: the relay URLs, each once. TypeError unless each is one the WebSocket constructor takes: it
// parses, ws: or wss:, with no fragment. The message names one by its index only (a URL may carry a token).
export function checkRelays(list) {
  if (!Array.isArray(list) || !list.length) throw new TypeError('relays must be a non-empty array of ws:// or wss:// URLs');
  list.forEach((u, i) => {
    if (!(typeof u === 'string' && wsUrl(u))) throw new TypeError(`relays[${i}] must be a ws:// or wss:// URL with no fragment`);
  });
  return [...new Set(list)];
}

const wsUrl = (u) => {
  try {
    const url = new URL(u);
    return (url.protocol === 'ws:' || url.protocol === 'wss:') && !url.href.includes('#');
  } catch {
    return false;
  }
};

const ms = (k, v) => {
  if (!Number.isFinite(v) || v < 0) throw new TypeError(`${k} must be a number of ms, 0 or more`);
  return v;
};

// The policy `next` (any of KEYS) gives over `base` (a whole one), each key checked: a new whole policy, with its
// stages. TypeError for an unknown key or a value not allowed.
export function makePolicy(next, base) {
  if (!next || typeof next !== 'object' || Array.isArray(next)) throw new TypeError('a policy is an object');
  for (const k of Object.keys(next)) if (!KEYS.has(k)) throw new TypeError(`unknown policy key ${k}`);
  const p = { ...base };
  if (next.order !== undefined) p.order = checkOrder(next.order);
  if (next.iceServers !== undefined) p.iceServers = checkServers(next.iceServers);
  if (next.relays !== undefined) p.relays = checkRelays(next.relays);
  if (next.fallbackMs !== undefined) p.fallbackMs = ms('fallbackMs', next.fallbackMs);
  if (next.sameNatFallbackMs !== undefined) p.sameNatFallbackMs = ms('sameNatFallbackMs', next.sameNatFallbackMs);
  if (next.birthday !== undefined) {
    if (typeof next.birthday !== 'boolean') throw new TypeError('birthday is true or false');
    p.birthday = next.birthday;
  }
  if (next.privacy !== undefined) {
    // A room dials every member, so relay-only (one WebRTC link, to the relay) can't apply.
    if (next.privacy !== 'direct' && next.privacy !== 'public-only') throw new TypeError(`privacy must be 'direct' or 'public-only', not ${next.privacy}`);
    p.privacy = next.privacy;
  }
  p.stages = stagesOf(p.order);
  p.rank = new Map(p.order.map((x, i) => [x, i]));
  return p;
}

// Where `path` comes in `policy`'s order: Infinity for one that is off.
export const rankOf = (policy, path) => policy.rank.get(path) ?? Infinity;
