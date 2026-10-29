// NB-022b (SEC-2, NB22-2): Rust and Godot members seal their room hellos exactly as
// nostr.js seals a browser's, and a browser Room that opens one never dials that peer
// over WebRTC (Rust marks it meta.nb). The vector was made by sealRoom with the nonce
// 0..11 and is asserted again in traversal.rs `hello_seal_matches_nostr_js`: change both
// or neither. No relays: events go straight into onRelay.
import { loadPeerwire } from '../src/peerwire.js';
import { NostrSignaling, unb64 } from '../src/nostr.js';
import { RoomProof } from '../src/proof.js';
import { Room } from '../src/room.js';

const wasm = process.argv[2] ?? new URL('../pkg/peerwire_ffi.wasm', import.meta.url).pathname;
const pw = await loadPeerwire(wasm);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');

const ROOM = 'demo/lobby'; // app 'demo', room 'lobby'
const ENC = '7e6a0b7e0ba0d5c263896aa5345c379c8bd23f796815a02a6282b076002fbb96';
const RUST_ID = '8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c'; // Identity::from_secret_bytes(&[1; 32])
const TS = 1800000000123;
// traversal.rs hello(), role peer: byte for byte what a Rust member seals.
const BODY = `{"t":"hello","from":"${RUST_ID}","meta":{"nb":1,"role":"peer","ts":${TS},`
  + '"p":"e72685be158e95e099ef47af264ce803b7380a7e1204708b9c3e582dd43bb2b7"}}';
const SEALED = 'AAECAwQFBgcICQoLbW0PRSqiDrpAc6clOpp1LqmbpsEwi1V1wdARHJcaKrdJxmyODZ8iKLorORTIX6Vx'
  + '8SqfUhbwUB9d/uZyGAkxUyvvZjSlOtZMEp+2Mb2gIdnJdMv2G1Nt0R3eGuOeeypLbPIz3oSh0hb4d+DQ'
  + 'MCqOucNUEwbFBqFZNWSPsHEajTFV5wIaTVagPhYhSq47vXbL5Yu6HVfna1kQ4yoOgiCbyoqB3bpRUtdn'
  + '5dBQT6nGdTxbCDzj7UYsZGHduNlouDgcziOF/y02ZiP4F4134j4u345c0SOyA1AU23udVOprOg==';
const NONCE = Uint8Array.from({ length: 12 }, (_, i) => i);

// 1. The cross-stack vector: same key, nonce and body, same base64.
const enc = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(`peerwire/room-enc/${ROOM}`)));
check(hex(enc) === ENC, 'the hello key is sha256("peerwire/room-enc/demo/lobby"), as Rust derives it');
check(pw.endpoint({ secret: new Uint8Array(32).fill(1) }).id === RUST_ID, "the vector's sender is the PeerId of secret [1; 32]");
const sig = new NostrSignaling(pw, pw.endpoint(), { room: ROOM, relays: [] });
await sig.ready;
const getRandomValues = crypto.getRandomValues;
crypto.getRandomValues = (a) => (a.length === 12 ? a.set(NONCE) : getRandomValues.call(crypto, a), a);
let sealed;
try {
  sealed = await sig.sealRoom(JSON.parse(BODY));
} finally {
  crypto.getRandomValues = getRandomValues;
}
check(sealed === SEALED, `sealRoom gives Rust's hello the bytes Rust gives it (${sealed.length} base64 characters)`);
const opened = await sig.openRoom(SEALED);
check(JSON.stringify(opened) === BODY, 'openRoom opens the Rust hello to its exact JSON');
check(!new TextDecoder('latin1').decode(unb64(SEALED)).includes(RUST_ID.slice(0, 16)), 'the PeerId is not in the sealed body');
const proof = await RoomProof.create(ROOM);
check(await proof.checkHello(RUST_ID, opened.meta, TS), "its proof checks out under the room key: only the mark tells it apart");

// 2. A Room hears the sealed Rust hello through NostrSignaling and neither dials it,
// lets it use the pipe, nor answers it; with meta.pw instead, the same. The same proof
// without a mark (a browser's hello) is answered and dialled, so the mark is what stops it.
const realNow = Date.now;
Date.now = () => TS + 1000; // the vector's hello is fresh
let ep;
do ep = pw.endpoint(); while (!(ep.id < RUST_ID)); // the smaller id dials
const dialled = [];
const piped = [];
let greeted = 0;
sig.hello = async () => void greeted++;
const room = new Room(ep, sig, () => ({ has: () => false, dial: (p) => dialled.push(p), allowPipe: (p) => piped.push(p), drop() {}, close() {} }), {}, proof);
// Every onHello and the answer it sends (room.hello(), not awaited there) settle before a check.
const pending = [];
const heard = [];
const onHello = sig.onHello;
sig.onHello = (p, m) => (heard.push(m), pending.push(onHello(p, m)));
const answer = room.hello.bind(room);
room.hello = () => pending.at(pending.push(answer()) - 1);
let n = 0;
const feed = async (content) => {
  const ev = { id: (++n).toString(16).padStart(64, '0'), kind: 25059, pubkey: 'x', tags: [['p', sig.roomTag]], content };
  await sig.onRelay('test', JSON.stringify(['EVENT', 's', ev]));
  while (pending.length) await Promise.all(pending.splice(0));
};
const before = greeted;
await feed(SEALED);
check(heard.length === 1 && heard[0].nb === 1, 'the Room hears the sealed Rust hello, marked meta.nb');
check(dialled.length === 0 && piped.length === 0 && greeted === before, 'and does not dial it, open the pipe to it, or answer it');
const { nb, ...meta } = opened.meta;
await feed(await sig.sealRoom({ t: 'hello', from: RUST_ID, meta: { ...meta, pw: 1 } }));
check(heard.length === 2 && dialled.length === 0 && piped.length === 0 && greeted === before, 'meta.pw is taken as the same mark');
await feed(await sig.sealRoom({ t: 'hello', from: RUST_ID, meta }));
check(dialled.length === 1 && dialled[0] === RUST_ID && piped.length === 1 && greeted === before + 1,
  'control: the same hello with no mark is answered and dialled');
Date.now = realNow;
await room.leave();
sig.close();

console.log(failed ? `${failed} FAILED` : 'rust hello: all passed');
process.exit(failed ? 1 : 0);
