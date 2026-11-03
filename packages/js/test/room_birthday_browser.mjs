// NB-084: the room's own birthday punch (src/punch.js) with real WebRTC, in headless
// Chromium on loopback: two rooms in one page, each with an RTCPeerConnection class of its
// own (so a punch slot of its own, as two pages have) that plays its NAT:
// - the room's own links get none of the other's candidates, so WebRTC can't link them and
//   the room falls back over signalling (a pipe on the BroadcastChannel);
// - their descriptions show public server-reflexive candidates: the cone side one port at
//   203.0.113.7, the symmetric side two ports far apart at 198.51.100.9 (no prediction);
//   the cone side's punch connection names 203.0.113.7 at its real socket's port;
// - the fleet's checks to the cone side's address are dropped (its NAT lets in only what
//   it sent to), and of the cone side's guesses at 198.51.100.9, two land on real fleet
//   sockets on 127.0.0.1 (as a guess that hits a NAT mapping does), the rest nowhere.
// So the channel forms only as across real NATs: through a sprayed guess, at the IP the
// failed link named. Checks: the room links fall back; the cone side sprays, the
// symmetric side opens its fleet aimed at the cone side's own address; both rooms adopt
// the channel (route direct, adopted), messages go over it and not over signalling, and of
// the fleet only that connection stays open. Then with no guess landing, the symmetric
// side leaves mid-punch: every connection it made is closed, and the cone side ends its
// punch and closes its own.
//
//   node packages/js/test/room_birthday_browser.mjs [fleet=16]
import { serve } from './serve.mjs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const PW = process.env.PLAYWRIGHT ?? '/home/ubuntu/.local/share/playwright-tools/node_modules/playwright/index.mjs';
const { chromium } = await import(PW);
const fleet = Number(process.argv[2] ?? 16); // fewer than 256 for a fast loopback run; the mechanism is the same
const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const { port, server } = await serve(root);
let browser;
let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};
try {
  browser = await chromium.launch({ args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] });
  const page = await browser.newPage();
  page.on('pageerror', (e) => ok(false, `page error: ${e.message}`));
  await page.goto(`http://127.0.0.1:${port}/`);
  const out = await page.evaluate(async ({ fleet, run }) => {
    const { joinRoom } = await import('/src/index.js');
    const { Punches } = await import('/src/punch.js');
    Object.assign(Punches, { afterMs: 200, punchMs: 20000, gapMs: 0, fleetSize: fleet });
    const res = [];
    const ok = (cond, what) => res.push([!!cond, what]);
    const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
    const until = async (cond, ms = 20000) => {
      const t = performance.now();
      while (!cond() && performance.now() - t < ms) await sleep(50);
      return cond();
    };
    const IP = { cone: '203.0.113.7', symmetric: '198.51.100.9' };
    const realLocal = Object.getOwnPropertyDescriptor(RTCPeerConnection.prototype, 'localDescription').get;
    const hostPort = (pc) => /^a=candidate:\S+ \d+ udp \d+ \d+\.\d+\.\d+\.\d+ (\d+) typ host/m.exec(realLocal.call(pc)?.sdp ?? '')?.[1] ?? null;
    let hits = [20, 40]; // which of the cone side's guesses land on a fleet socket

    // An RTCPeerConnection class for one page behind `nat`.
    const natPc = (nat) => {
      const seen = { made: [], aimedAt: [], sprayedAt: new Set() };
      class NatPc extends RTCPeerConnection {
        constructor(config = {}) {
          super(config);
          this.inFleet = !!config.certificates;
          seen.made.push(this);
        }
        // Our description, with the public address(es) STUN would have seen.
        get localDescription() {
          const d = realLocal.call(this);
          if (!d) return d;
          const at = (p, f) => `a=candidate:${f} 1 udp 1686052607 ${IP[nat]} ${p} typ srflx raddr 0.0.0.0 rport 0\r\n`;
          const sdp = d.sdp + (nat === 'cone' ? at(hostPort(this) ?? 40000, 9) : at(40001, 9) + at(52345, 8));
          return { type: d.type, sdp, toJSON: () => ({ type: d.type, sdp }) };
        }
        setRemoteDescription(d) {
          return super.setRemoteDescription(d && { type: d.type, sdp: d.sdp.replace(/^a=candidate:.*\r?\n/gm, '') });
        }
        async addIceCandidate(c) {
          const s = c?.candidate ?? '';
          const m = /^candidate:pw(b|e)(\d+) .* (\S+) (\d+) typ (?:host|srflx)/.exec(s);
          if (!m) return; // a room link's candidate: none, so WebRTC can't link the room
          if (m[1] === 'e') return void seen.aimedAt.push(`${m[3]} ${m[4]}`); // the cone side's NAT drops what the fleet sends
          seen.sprayedAt.add(m[3]);
          if (m[3] !== IP.symmetric || !hits.includes(Number(m[2]))) return; // a guess that lands nowhere
          const sockets = () => peer.made.filter((p) => p.inFleet && p.signalingState !== 'closed').map(hostPort).filter(Boolean);
          await until(() => sockets().length, 3000);
          const to = sockets()[Number(m[2]) % sockets().length];
          return super.addIceCandidate({ ...c, candidate: s.replace(` ${m[3]} ${m[4]} `, ` 127.0.0.1 ${to} `) });
        }
      }
      let peer = null;
      return { Pc: NatPc, seen, peerWith: (other) => (peer = other) };
    };

    const pair = async (n) => {
      const cone = natPc('cone');
      const sym = natPc('symmetric');
      cone.peerWith(sym.seen);
      sym.peerWith(cone.seen);
      const steps = { cone: [], symmetric: [] };
      const join = (side, nat) => joinRoom({ appId: 'peerwire-test', roomId: `bday-browser-${run}-${n}`, fallbackMs: 2000, RTCPeerConnection: side.Pc })
        .then((r) => ((r.transport.trace = (p, what, d) => what === 'birthday' && steps[nat].push(d.step + (d.why ? `(${d.why})` : ''))), r));
      const [a, b] = await Promise.all([join(cone, 'cone'), join(sym, 'symmetric')]);
      return { a, b, cone: cone.seen, sym: sym.seen, steps };
    };

    // 1. The punch links the pair.
    {
      const { a, b, cone, sym, steps } = await pair(1);
      const fellBack = await until(() => a.route(b.self)?.kind === 'signaling' && b.route(a.self)?.kind === 'signaling' && a.peers.length === 1 && b.peers.length === 1);
      ok(fellBack, 'WebRTC could not link the rooms: they fell back over signalling');
      const t0 = performance.now();
      const adopted = await until(() => a.route(b.self)?.adopted && b.route(a.self)?.adopted);
      ok(adopted && a.route(b.self).kind === 'direct', `both rooms adopted the punched channel ${Math.round(performance.now() - t0)} ms after the fallback (cone: ${steps.cone.join(' ')}; symmetric: ${steps.symmetric.join(' ')})`);
      const coneSpray = a.transport.carrier(b.self)?.pc;
      ok(cone.made.includes(coneSpray) && !coneSpray.inFleet && ![...a.transport.links.values()].some((l) => l.pc === coneSpray), "the cone side's channel is its punch connection");
      ok(sym.made.filter((p) => p.inFleet).length === fleet && sym.made.filter((p) => p.inFleet && p.signalingState !== 'closed').length === 1, `the symmetric side opened a fleet of ${fleet}; only the one carrying the link is open`);
      const own = `${IP.cone} ${hostPort(coneSpray)}`;
      ok(sym.aimedAt.length === fleet && sym.aimedAt.every((x) => x === own), `each fleet connection was aimed at the cone side's own address (${own}) and nothing else`);
      ok(cone.sprayedAt.size === 1 && cone.sprayedAt.has(IP.symmetric), `the cone side sprayed only ${IP.symmetric}, the IP the failed link named`);
      const got = [];
      b.on('message', (m) => got.push(m));
      const piped = a.transport.piped.sent;
      for (let i = 0; i < 20; i++) a.send(`m${i}`);
      ok(await until(() => got.length === 20, 5000), '20 messages arrived over the channel');
      ok(a.transport.piped.sent === piped, 'none went over signalling');
      await Promise.all([a.leave(), b.leave()]);
      ok([...cone.made, ...sym.made].every((p) => p.signalingState === 'closed'), 'after leaving, every connection either made is closed');
    }

    // 2. No guess lands; the symmetric side leaves mid-punch.
    {
      hits = [];
      const { a, b, cone, sym, steps } = await pair(2);
      const going = await until(() => steps.symmetric.includes('fleet') && steps.cone.includes('spray'));
      ok(going, 'a punch with no landing guess under way');
      await b.leave();
      ok(sym.made.every((p) => p.signalingState === 'closed'), `the symmetric side left mid-punch: all ${sym.made.length} of its connections closed`);
      ok(await until(() => steps.cone.some((s) => /^end/.test(s)), 10000), `the cone side ended its punch (${steps.cone.join(' ')})`);
      ok(cone.made.filter((p) => !p.inFleet).every((p) => p.signalingState === 'closed' || [...a.transport.links.values()].some((l) => l.pc === p)), "and closed its punch connection (only its room's own links are left)");
      await a.leave();
    }
    return res;
  }, { fleet, run: port });
  for (const [pass, what] of out) {
    console.log(`${pass ? 'ok  ' : 'FAIL'} ${what}`);
    if (!pass) failed++;
  }
} catch (e) {
  ok(false, e.stack ?? String(e));
} finally {
  await browser?.close();
  server.kill();
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
