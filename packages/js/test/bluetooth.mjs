// NB-110d: BluetoothTransport (bluetooth.js) against a peripheral built from the same code, over a fake Web Bluetooth
// with no radio. The fake device runs the wasm's BleLink as the peripheral, beside an endpoint of its own, as a board
// running ble_led does: it reports a central once it subscribes to `tx`, challenges it at once, checks the page's room
// hello as a node's traversal would (Rust's exact JSON, the proof under the room key, sealed for the room) and answers
// `led on`. Like Chrome, the fake drops a notification that comes before startNotifications() settles.
//
// Checks: the link comes up and carries messages both ways, every value written fits 20 bytes, a dropped challenge is
// got over by connecting again, drop() says goodbye, and each failure says what it is: 'not-available' (no Web
// Bluetooth, or off), 'cancelled', 'not-member' (another room's board), 'refused', 'wrong-peer', 'timeout' (a board
// that never proves the room), and a TypeError for an endpoint of the module without Bluetooth.
//
//   node packages/js/test/bluetooth.mjs [path/to/peerwire_ffi_ble.wasm]
import { loadPeerwire, BleLink } from '../src/peerwire.js';
import { BluetoothTransport, BLUETOOTH_WASM, SERVICE_UUID, RX_UUID, TX_UUID } from '../src/bluetooth.js';
import { RoomProof } from '../src/proof.js';
import { roomSealKey } from '../src/nostr.js';

const wasm = process.argv[2] ?? BLUETOOTH_WASM;
const pw = await loadPeerwire(wasm);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const until = async (what, cond, ms = 10000) => {
  const t = Date.now();
  while (!cond()) {
    if (Date.now() - t > ms) {
      check(false, `timed out waiting for ${what}`);
      return false;
    }
    await new Promise((r) => setTimeout(r, 5));
  }
  return true;
};
const dec = new TextDecoder();
const APP = 'peerwire-test';
const ROOM = `ble-${process.pid}`;
// The shape Rust's traversal.rs `parse_hello` takes, and nothing else.
const RUST_HELLO = /^\{"t":"hello","from":"([0-9a-f]{64})","meta":\{"nb":1,"role":"peer","ts":(\d{1,19}),"p":"[0-9a-f]{64}"\}\}$/;

// A board on a fake radio: a peripheral in `room`, with an endpoint of its own. `drop`: its first challenge goes
// before the central has subscribed, and is lost. `refuse`: it closes each connection before challenging. `silent`:
// it takes no write.
class Board {
  constructor(room, { drop = false, refuse = false, silent = false } = {}) {
    Object.assign(this, { room, drop, refuse, silent });
    this.ep = pw.endpoint();
    this.id = this.ep.id;
    this.proof = RoomProof.create(`${APP}/${room}`);
    this.sealKey = roomSealKey(`${APP}/${room}`);
    Object.assign(this, { connects: 0, hellos: [], got: [], events: [], written: [], link: null, conn: null });
    this.ep.onTransmit = (_addr, bytes) => {
      this.link?.datagram(bytes);
      this.notifyAll();
    };
    this.ep.onEvent = (e) => {
      this.events.push(e);
      if (e.type !== 'message') return;
      const text = dec.decode(e.data);
      this.got.push(text);
      if (text === 'led on') this.ep.send(e.peer, 'led: on');
    };
    this.device = this.makeDevice();
  }

  makeDevice() {
    const board = this;
    const device = new EventTarget();
    const tx = new EventTarget();
    tx.uuid = TX_UUID;
    tx.startNotifications = async () => {
      board.subscribed();
      tx.notifying = true;
      return tx;
    };
    const rx = { uuid: RX_UUID, writeValueWithoutResponse: async (v) => board.write(v) };
    const service = {
      uuid: SERVICE_UUID,
      async getCharacteristic(uuid) {
        if (uuid === RX_UUID) return rx;
        if (uuid === TX_UUID) return tx;
        throw new DOMException('no such characteristic', 'NotFoundError');
      },
    };
    const server = {
      async getPrimaryService(uuid) {
        if (uuid !== SERVICE_UUID) throw new DOMException('no such service', 'NotFoundError');
        return service;
      },
    };
    device.gatt = {
      connected: false,
      async connect() {
        board.connects++;
        tx.notifying = false;
        device.gatt.connected = true;
        board.conn = { tx, device };
        return server;
      },
      // The page closing the connection: Chrome tells the page too.
      disconnect() {
        board.close();
      },
    };
    return device;
  }

  // The central subscribed: as the ESP32's driver does, the node hears of it now, and challenges it.
  subscribed() {
    if (this.refuse) return setTimeout(() => this.close(), 0);
    this.link = new BleLink(this.ep, { appId: APP, roomId: this.room, central: false, mtu: 23, addr: 1 });
    if (this.drop && this.connects === 1) this.notifyAll(); // before startNotifications() settles: lost
    else setTimeout(() => this.notifyAll(), 0);
  }

  // Notifies what the link queued; one before the central subscribed is lost, as in Chrome.
  notifyAll() {
    const c = this.conn;
    if (!this.link || !c) return;
    for (const v of this.link.values()) {
      if (!c.tx.notifying) continue;
      setTimeout(() => {
        if (this.conn !== c) return;
        c.tx.value = new DataView(v.buffer, v.byteOffset, v.byteLength);
        c.tx.dispatchEvent(new Event('characteristicvaluechanged'));
      }, 0);
    }
  }

  async write(v) {
    if (!this.conn) throw new DOMException('GATT Server is disconnected.', 'NetworkError');
    this.written.push(v.length);
    if (this.silent || !this.link) return;
    for (const e of this.link.push(v)) {
      if (e.type === 'peer') this.peer = e.peer;
      if (e.type === 'signal' && e.inner === 1) this.hellos.push(await this.checkHello(e.payload));
      if (e.type === 'closed') {
        this.closedFor = e.reason;
        return setTimeout(() => this.close(), 0);
      }
    }
    this.notifyAll();
  }

  // As a node's traversal takes a room hello: sealed for the room, Rust's exact JSON, a fresh proof under the room key,
  // and from the peer that proved who it is on the connection.
  async checkHello(sealed) {
    try {
      const key = await this.sealKey;
      const plain = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: sealed.subarray(0, 12) }, key, sealed.subarray(12));
      const json = dec.decode(plain);
      const m = RUST_HELLO.exec(json);
      const h = JSON.parse(json);
      const ok = !!m && h.from === this.peer && (await (await this.proof).checkHello(h.from, h.meta));
      return { ok, json };
    } catch (e) {
      return { ok: false, json: String(e) };
    }
  }

  // The connection closes, at either end.
  close() {
    const c = this.conn;
    if (!c) return;
    this.conn = null;
    this.link?.free();
    this.link = null;
    c.device.gatt.connected = false;
    setTimeout(() => c.device.dispatchEvent(new Event('gattserverdisconnected')), 0);
  }
}

// A fake navigator.bluetooth whose picker gives `board`'s device, or rejects with `pick` (a DOMException).
function fakeBluetooth(board, { available = true, pick = null } = {}) {
  return {
    asked: [],
    async getAvailability() {
      return available;
    },
    async requestDevice(options) {
      this.asked.push(options);
      if (pick) throw pick;
      return board.device;
    },
  };
}

// A page: an endpoint of the module with Bluetooth, and a transport on it.
function page(bluetooth, opts = {}) {
  const ep = pw.endpoint();
  const got = [];
  const states = [];
  ep.onEvent = (e) => e.type === 'message' && got.push(dec.decode(e.data));
  const bt = new BluetoothTransport(ep, { appId: APP, roomId: ROOM, bluetooth, onPeer: (p, s) => states.push(s), ...opts });
  return { ep, bt, got, states };
}

const codeOf = (p) => p.then(() => 'linked', (e) => e.code ?? `${e.name}: ${e.message}`);

// 1. The page links with the board and they swap messages.
{
  const board = new Board(ROOM);
  const bluetooth = fakeBluetooth(board);
  const { ep, bt, got, states } = page(bluetooth);
  const t0 = performance.now();
  const peer = await bt.dial();
  console.log(`linked in ${(performance.now() - t0).toFixed(0)} ms`);
  check(peer === board.id, 'dial() resolves to the PeerId the board proved on the connection');
  check(bt.has(peer) && states[0] === 'open', "has(peer), and onPeer said 'open'");
  const asked = bluetooth.asked[0];
  check(asked?.filters?.[0]?.services?.[0] === SERVICE_UUID && !asked.acceptAllDevices, 'the picker is filtered on the service');
  check(board.hellos.length === 1 && board.hellos[0].ok, `the board took the page's room hello: ${board.hellos[0]?.json}`);
  check(ep.send(peer, 'led on') === 0, 'a message to the board');
  await until("the board's answer", () => got.includes('led: on'));
  check(board.got.includes('led on'), 'the board got it');
  check(board.written.length > 10 && Math.max(...board.written) <= 20, `every value written fits 20 bytes (${board.written.length} writes)`);
  check(board.connects === 1, 'one connection');
  bt.drop(peer);
  await until("onPeer 'closed'", () => states.includes('closed'));
  await until("the board's 'disconnected'", () => board.events.some((e) => e.type === 'disconnected' && e.reason === 'closed'));
  check(!bt.has(peer) && board.conn === null, 'drop(): the goodbye went over the connection, which then closed');
  bt.close();
  ep.close();
}

// 2. Chrome drops the board's first challenge: the page connects again.
{
  const board = new Board(ROOM, { drop: true });
  const { ep, bt } = page(fakeBluetooth(board));
  check((await bt.dial()) === board.id && board.connects === 2, 'a lost challenge: linked on the second connection');
  bt.close();
  ep.close();
}

// 3. Failures, each with its code.
{
  const board = new Board(ROOM);
  check((await codeOf(page(null).bt.dial())) === 'not-available', "no Web Bluetooth: 'not-available'");
  check(!(await page(null).bt.available()), 'available() is false without it');
  check((await codeOf(page(undefined).bt.dial())) === 'not-available', "Node's navigator, which has none: 'not-available'");
  const off = page(fakeBluetooth(board, { available: false }));
  check((await codeOf(off.bt.dial())) === 'not-available', "Bluetooth off: 'not-available'");
  const cancelled = page(fakeBluetooth(board, { pick: new DOMException('User cancelled', 'NotFoundError') }));
  check((await codeOf(cancelled.bt.dial())) === 'cancelled', "the picker closed with no device: 'cancelled'");
  const gesture = page(fakeBluetooth(board, { pick: new DOMException('Must be handling a user gesture', 'SecurityError') }));
  check((await codeOf(gesture.bt.dial())) === 'not-allowed', "not from a user gesture: 'not-allowed'");
  const other = new Board(`${ROOM}-other`);
  check((await codeOf(page(fakeBluetooth(other)).bt.dial())) === 'not-member', "another room's board: 'not-member'");
  check(other.closedFor === 'wrong-answer' && other.hellos.length === 0, 'it refused our answer, and heard no hello');
  const full = new Board(ROOM, { refuse: true });
  check((await codeOf(page(fakeBluetooth(full)).bt.dial())) === 'refused', "a board that refuses the central: 'refused'");
  const wrong = page(fakeBluetooth(board));
  const someone = pw.endpoint().id;
  check((await codeOf(wrong.bt.dial(someone))) === 'wrong-peer', "a board that proves it is someone else: 'wrong-peer'");
  const silent = new Board(ROOM, { silent: true });
  const t0 = performance.now();
  const late = await codeOf(page(fakeBluetooth(silent)).bt.dial());
  const ms = performance.now() - t0;
  check(late === 'timeout' && ms > 2900 && ms < 5000, `a board that never proves the room: 'timeout' after ${ms.toFixed(0)} ms`);
  const plain = await loadPeerwire(new URL('../pkg/peerwire_ffi.wasm', import.meta.url));
  let threw = null;
  try {
    new BluetoothTransport(plain.endpoint(), { appId: APP, roomId: ROOM, bluetooth: fakeBluetooth(board) });
  } catch (e) {
    threw = e;
  }
  check(threw instanceof TypeError && /BLUETOOTH_WASM/.test(threw.message), 'an endpoint of the module without Bluetooth: a TypeError');
}

console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
