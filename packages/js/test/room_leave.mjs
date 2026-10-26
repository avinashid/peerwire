// A leaving room must reach its peers at once even when its reliable send window is
// full, and must do it all before leave() returns: on pagehide the page may be gone
// before any timer or await. Three rooms in this process (BroadcastChannel and
// LocalTransport): a sends 6 MiB, then leaves at once. Then, with the Bye lost, the
// core's sealed close alone must still tell the other side. Then the pagehide handler
// and its grace. Last, a pair WebRTC can't link, on the signalling pipe: over
// BroadcastChannel, idle and backed up, and over Nostr (an in-process relay). test/leave.mjs
// does the same with real tab closes in Chromium.
//
//   node packages/js/test/room_leave.mjs
import { joinRoom, Lane, Room } from '../src/index.js';
import { PIPE_BATCH } from '../src/webrtc.js';

let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (cond, ms) => {
  for (const t = Date.now(); !cond() && Date.now() - t < ms;) await sleep(5);
  return cond();
};
const FAST_MS = 2000; // far below the 30 s idle_timeout

async function rooms(n, name, extra = {}) {
  const opts = { appId: 'peerwire-test', roomId: `leave-${name}-${process.pid}`, ...extra };
  const rs = await Promise.all(Array.from({ length: n }, () => joinRoom(opts)));
  check(await until(() => rs.every((r) => r.peers.length === n - 1), 10000), `${n} rooms joined`);
  return rs;
}
const leaves = (r) => {
  const seen = [];
  r.on('peer-leave', (peer, reason) => seen.push({ peer, reason, at: performance.now() }));
  return seen;
};

// 1. A full send window: the Bye must not wait behind it.
{
  const [a, b, c] = await rooms(3, 'full');
  const seen = [b, c].map(leaves);
  for (let i = 0; i < 48; i++) a.send(new Uint8Array(128 << 10)); // 6 MiB to each, over the 4 MiB window
  const waiting = [...a.outbox.values()].reduce((n, q) => n + q.length, 0);
  check(waiting > 0, `the send window is full (${waiting} frames wait in the outbox)`);
  const t0 = performance.now();
  const p = a.leave();
  check(a.endpoint.closed && a.left, 'leave() said goodbye and closed before returning (no timer, no await)');
  check(await until(() => seen.every((s) => s.length), FAST_MS), `both peers saw the leave within ${FAST_MS} ms`);
  for (const [i, s] of seen.entries()) {
    check(s.length === 1 && s[0].peer === a.self && s[0].reason === 'left',
      `peer ${i + 1}: peer-leave ${s[0]?.reason} after ${s.length ? (s[0].at - t0).toFixed(0) : '-'} ms`);
  }
  await p;
  await Promise.all([b.leave(), c.leave()]);
}

// 2. The Bye is lost: the core's close (Endpoint.disconnect) still tells the peer.
{
  const [a, b] = await rooms(2, 'nobye');
  const seen = leaves(b);
  const send = a.endpoint.send.bind(a.endpoint);
  let dropped = 0;
  a.endpoint.send = (peer, data, lane) => (data.length === 1 && lane === Lane.Unreliable ? void dropped++ : send(peer, data, lane));
  const t0 = performance.now();
  a.leave();
  check(dropped === 1, 'the Bye was dropped');
  check(await until(() => seen.length, FAST_MS) && seen[0].peer === a.self && seen[0].reason === 'closed',
    `with no Bye the core's close reached the peer: peer-leave ${seen[0]?.reason} after ${seen.length ? (seen[0].at - t0).toFixed(0) : '-'} ms`);
  await b.leave();
}

// 3. pagehide: the handler says goodbye, then holds the page ~20 ms (the browser may end
// the renderer once it returns, before the Bye is on the wire), but only if it had peers.
{
  const [a, b] = await rooms(2, 'pagehide');
  const seen = leaves(b);
  let t0 = performance.now();
  a.onPageHide();
  const held = performance.now() - t0;
  check(a.endpoint.closed && held >= 20, `the pagehide handler left and held the page ${held.toFixed(1)} ms`);
  check(await until(() => seen.length, FAST_MS) && seen[0].reason === 'left', `the peer saw peer-leave ${seen[0]?.reason}`);
  t0 = performance.now();
  a.onPageHide();
  const again = performance.now() - t0;
  check(again < 5, `a second pagehide, with no peers left, returns at once (${again.toFixed(1)} ms)`);
  await b.leave();
}

// 4. The signalling pipe, for a pair WebRTC can't link (symmetric NATs, no TURN): its
// datagrams wait PIPE_FLUSH_MS to be batched, so the Bye and the sealed close are still
// queued when leave() closes the transport, which must send them. They arrive in one batch,
// and the peer must still see 'left'. Backed up past a batch, the last one carries the newest.
class DeadPc { // its DataChannel never opens
  connectionState = 'new';
  iceGatheringState = 'complete';
  localDescription = null;
  createDataChannel() {
    return { readyState: 'connecting', bufferedAmount: 0, send() {}, close() {} };
  }
  async createOffer() {
    return { type: 'offer', sdp: 'v=0' };
  }
  async createAnswer() {
    return { type: 'answer', sdp: 'v=0' };
  }
  async setLocalDescription(d) {
    this.localDescription = { ...d, toJSON: () => ({ type: d.type, sdp: d.sdp }) };
  }
  async setRemoteDescription() {}
  async addIceCandidate() {}
  addEventListener() {}
  close() {}
}
Room.peerFirstMs = 100; // no member to relay: signalling joins the dial sooner
const PIPED = { RTCPeerConnection: DeadPc, fallbackMs: 200 };
async function overPipe(name, extra = {}) {
  const rs = await rooms(2, name, { ...PIPED, ...extra });
  check(rs.every((r) => r.route(r.peers[0])?.kind === 'signaling'), `${name}: both reach the other over signalling`);
  return rs;
}
const leftFast = async (seen, a, t0, what) => {
  const ok = await until(() => seen.length, FAST_MS) && seen.length === 1 && seen[0].peer === a.self && seen[0].reason === 'left';
  check(ok, `${what}: peer-leave ${seen[0]?.reason} after ${seen.length ? (seen[0].at - t0).toFixed(0) : '-'} ms`);
};
{
  const [a, b] = await overPipe('pipe');
  const seen = leaves(b);
  const t0 = performance.now();
  a.leave();
  await leftFast(seen, a, t0, 'over the pipe');
  await b.leave();
}
{
  const [a, b] = await overPipe('pipe-full');
  const seen = leaves(b);
  const pipe = [...a.transport.pipes.values()][0];
  a.send(new Uint8Array(100 << 10));
  check(await until(() => pipe.bytes > PIPE_BATCH, 5000),
    `the pipe is backed up past a batch (${pipe.q.length} datagrams, ${pipe.bytes} bytes queued)`);
  const t0 = performance.now();
  a.leave();
  await leftFast(seen, a, t0, 'over a backed-up pipe');
  await b.leave();
}

// 5. The same over Nostr, which hashes and signs a message after send() returns: close()
// must let the last batch out, then close the sockets.
{
  class RelaySocket { // an in-process relay: an EVENT goes to every other socket that asked for its tag
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    static open = new Set();
    constructor() {
      this.readyState = RelaySocket.CONNECTING;
      setTimeout(() => {
        this.readyState = RelaySocket.OPEN;
        RelaySocket.open.add(this);
        this.onopen?.();
      });
    }
    send(raw) {
      if (this.readyState !== RelaySocket.OPEN) return;
      const [type, x, y] = JSON.parse(raw);
      if (type === 'REQ') this.req = { sub: x, tags: y['#p'] };
      if (type !== 'EVENT') return;
      for (const ws of RelaySocket.open) {
        if (ws === this || !ws.req?.tags.includes(x.tags[0][1])) continue;
        const data = JSON.stringify(['EVENT', ws.req.sub, x]);
        setTimeout(() => ws.readyState === RelaySocket.OPEN && ws.onmessage?.({ data }));
      }
    }
    close() {
      this.readyState = RelaySocket.CLOSED;
      RelaySocket.open.delete(this);
      setTimeout(() => this.onclose?.());
    }
  }
  const Real = globalThis.WebSocket;
  globalThis.WebSocket = RelaySocket;
  const [a, b] = await overPipe('nostr', { signaling: 'nostr', relays: ['wss://relay.test'] });
  const seen = leaves(b);
  const t0 = performance.now();
  a.leave();
  await leftFast(seen, a, t0, 'over Nostr');
  const shut = await until(() => a.signaling.sockets.every((ws) => ws.readyState === RelaySocket.CLOSED), FAST_MS);
  check(shut, "then the leaving room's relay socket closed");
  await b.leave();
  globalThis.WebSocket = Real;
}

console.log(failed ? `${failed} FAILED` : 'room leave: all passed');
process.exit(failed ? 1 : 0);
