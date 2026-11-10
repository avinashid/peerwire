// Bluetooth LE in the browser (Web Bluetooth: Chrome and Edge, Chrome on Android), as a central only: the user picks a
// device nearby that runs Peerwire (a Linux machine running ble_chat, an ESP32 board), and the page links with the room
// member there over Bluetooth alone, with no network. Two browsers can't link this way: Web Bluetooth has no
// peripheral. Firefox, Safari and every iPhone browser have no Web Bluetooth: dial() says 'not-available'.
//
// The protocol (docs/concepts/bluetooth.md) runs in the wasm, the module with Bluetooth (peerwire_ffi_ble.wasm,
// BLUETOOTH_WASM): the room proof, the identity proofs, sealed signals and the framing (BleLink, peerwire.js). This
// file moves the GATT values, notifications from `tx` and writes without response to `rx`, one write at a time and
// 20 bytes at most each, as Web Bluetooth doesn't say the ATT MTU.
//
// The room is a native one, the Rust node's (ble_led's, ble_chat's), not a joinRoom room: once the connection proves
// the room and the peer proves who it is, the page sends it a room hello over the connection, as a member does (the node
// takes links only with members), and dials it over the connection. The link then carries the endpoint's datagrams;
// messages are the endpoint's, raw bytes, as a node sends them.
//
//   const pw = await loadPeerwire(BLUETOOTH_WASM);
//   const endpoint = pw.endpoint();
//   const bt = new BluetoothTransport(endpoint, { appId: 'peerwire-lan-chat', roomId: 'esp32' });
//   endpoint.onEvent = (e) => e.type === 'message' && show(new TextDecoder().decode(e.data));
//   button.onclick = async () => {
//     const peer = await bt.dial(); // the device picker, from a click; resolves once linked
//     endpoint.send(peer, 'led on');
//   };
//
// Nothing from the picker (the device's name or id) is trusted or sent anywhere: who is at the other end comes from
// its identity proof and the Noise handshake alone.

import { BleLink, isPeerId } from './peerwire.js';
import { RoomProof } from './proof.js';
import { roomSealKey, sealRoomBytes } from './nostr.js';

// The module with Bluetooth, next to this package's own: loadPeerwire(BLUETOOTH_WASM).
export const BLUETOOTH_WASM = new URL('../pkg/peerwire_ffi_ble.wasm', import.meta.url);
export const SERVICE_UUID = '1c3eea41-c2be-493c-ab0b-7707ad2dbed6';
export const RX_UUID = '1c3eea42-c2be-493c-ab0b-7707ad2dbed6';
export const TX_UUID = '1c3eea43-c2be-493c-ab0b-7707ad2dbed6';

// The ATT MTU the link cuts values to: 23, so values of 20 bytes, which fit any link.
const MTU = 23;
const INNER_ROOM = 1;
// The device sends its challenge as soon as we subscribe to `tx`, and Chrome drops a notification that comes before
// startNotifications() settles. A connection with no challenge this long after that is closed and made again.
const CHALLENGE_MS = 2000;
const TRIES = 3;
const RETRY_MS = 300;
// A GATT connect over the air takes hundreds of ms at least (1.8 s on Android). One that resolves faster gave back a
// connection the browser still holds: after Bluetooth was turned off and on, Android Chrome keeps the old one, which
// carries nothing. Such a connection with no challenge is 'stale', and connecting again gives the same dead one back
// (NB-127): only reloading the page clears it.
const FAST_CONNECT_MS = 50;
// A dial not linked by then (the GATT connect, the proofs and the handshake) fails.
const LINK_MS = 15000;
// A write that fails is tried again this often, this long apart, before the connection is given up.
const WRITE_TRIES = 3;
const WRITE_RETRY_MS = 20;
// How long drop() waits for the endpoint's goodbye to be written before it closes the connection.
const DROP_MS = 1000;
// The endpoint handles of this transport's connections ('BLE' and a counter), apart from other transports' small ones.
const HANDLE = 0x424c4500;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function failure(code, message) {
  return Object.assign(new Error(`bluetooth: ${message}`), { code });
}

export class BluetoothTransport {
  // `endpoint`: one of the module with Bluetooth (loadPeerwire(BLUETOOTH_WASM)). `bluetooth`: Web Bluetooth,
  // navigator.bluetooth by default. `onPeer(peer, state)`: 'open' once a link is up, 'closed' once its connection went.
  constructor(endpoint, { appId, roomId, bluetooth = globalThis.navigator?.bluetooth, onPeer = null } = {}) {
    if (typeof endpoint?.pw?.x?.pw_ble_new !== 'function') {
      throw new TypeError('BluetoothTransport needs an endpoint of the module with Bluetooth: loadPeerwire(BLUETOOTH_WASM)');
    }
    if (typeof appId !== 'string' || typeof roomId !== 'string' || !appId || !roomId) {
      throw new TypeError('BluetoothTransport needs an appId and a roomId');
    }
    this.ep = endpoint;
    this.appId = appId;
    this.roomId = roomId;
    this.bt = bluetooth ?? null;
    this.onPeer = onPeer;
    this.conns = new Map(); // endpoint handle -> Conn
    this.byPeer = new Map(); // peer -> its Conn, once linked or dialled
    this.next = HANDLE;
    this.closed = false;
    const name = `${appId}/${roomId}`;
    this.proof = RoomProof.create(name);
    this.sealKey = roomSealKey(name);
    // The endpoint's datagrams to our handles go over their connections; others to the transport that was there.
    const other = endpoint.onTransmit;
    endpoint.onTransmit = (addr, bytes) => {
      const c = this.conns.get(addr);
      if (c) c.datagram(bytes);
      else other?.(addr, bytes);
    };
  }

  // Whether this browser has Web Bluetooth and a radio it can use now: false on Firefox, Safari and every iPhone
  // browser, or with Bluetooth off or missing.
  async available() {
    if (!this.bt || typeof this.bt.requestDevice !== 'function') return false;
    try {
      return (await this.bt.getAvailability?.()) ?? true;
    } catch {
      return false;
    }
  }

  // Asks the user to pick a device (call it from a click or another user gesture), connects to it and links with the
  // room member there: resolves to its PeerId once the link is up. With `peer`, only if the device proves it is that
  // peer. Rejects with an Error whose `code` says why:
  // - 'not-available': no Web Bluetooth, or Bluetooth is off;
  // - 'cancelled': no device was picked;
  // - 'not-allowed': not from a user gesture, on a page that isn't secure (https), or blocked by policy;
  // - 'not-member': the device closed the connection once we answered its room challenge (another room's name), or
  //   its proof was wrong;
  // - 'refused': the device closed the connection before it challenged us (full, say);
  // - 'stale': the browser gave back a dead connection to the device, as Android Chrome does after Bluetooth was turned
  //   off and on: reload the page, then dial again;
  // - 'wrong-peer': it proved it is another peer than `peer`, or one we are linked with over another connection;
  // - 'violation': it broke the protocol; 'timeout': not linked within 15 s; 'failed': Web Bluetooth failed;
  //   'closed': the transport was closed.
  async dial(peer = null) {
    if (peer !== null && !isPeerId(peer)) throw new TypeError('dial: not a peer id (64 lowercase hex characters)');
    if (this.closed) throw failure('closed', 'the transport is closed');
    if (!(await this.available())) {
      throw failure('not-available', this.bt ? 'Bluetooth is off or missing' : 'this browser has no Web Bluetooth');
    }
    let device;
    try {
      // Filtered on the service, never acceptAllDevices: the picker lists Peerwire devices alone.
      device = await this.bt.requestDevice({ filters: [{ services: [SERVICE_UUID] }] });
    } catch (e) {
      if (e?.name === 'NotFoundError') throw failure('cancelled', 'no device was picked');
      if (e?.name === 'SecurityError' || e?.name === 'NotAllowedError') throw failure('not-allowed', e.message);
      throw failure('failed', e?.message ?? String(e));
    }
    for (let i = 1; ; i++) {
      try {
        return await new Promise((resolve, reject) => new Conn(this, device, peer, resolve, reject).start());
      } catch (e) {
        if (e.code !== 'no-challenge') throw e;
        if (i >= TRIES || this.closed) throw failure('timeout', 'the device sent no room challenge');
        await sleep(RETRY_MS);
      }
    }
  }

  // Whether a link to `peer` is up over one of our connections.
  has(peer) {
    return this.byPeer.get(peer)?.up === true;
  }

  // Ends the link to `peer`: the endpoint's goodbye goes over its connection, which then closes.
  drop(peer) {
    const c = this.byPeer.get(peer);
    if (!c) return;
    this.ep.disconnect(peer);
    c.closeAfterWrites();
  }

  // Closes every connection; a dial under way rejects with 'closed'.
  close() {
    this.closed = true;
    for (const c of [...this.conns.values()]) c.end('closed', 'the transport is closed');
  }
}

// One GATT connection: from connect to close.
class Conn {
  constructor(t, device, expect, resolve, reject) {
    this.t = t;
    this.device = device;
    this.expect = expect;
    this.resolve = resolve;
    this.reject = reject;
    this.handle = t.next++ >>> 0;
    this.link = null;
    this.peer = null;
    this.helloSent = false;
    this.dialled = false;
    this.up = false;
    this.over = false;
    this.writing = false;
    this.held = false; // whether connect() gave back a connection the browser held (FAST_CONNECT_MS)
    this.timers = new Set();
  }

  later(ms, fn) {
    const id = setTimeout(() => {
      this.timers.delete(id);
      if (!this.over) fn();
    }, ms);
    this.timers.add(id);
    return id;
  }

  async start() {
    const t = this.t;
    t.conns.set(this.handle, this);
    this.later(LINK_MS, () => this.end('timeout', 'not linked in time'));
    this.onGone = () => this.gone();
    this.device.addEventListener('gattserverdisconnected', this.onGone);
    try {
      const t0 = performance.now();
      const server = await this.device.gatt.connect();
      this.held = performance.now() - t0 < FAST_CONNECT_MS;
      const service = await server.getPrimaryService(SERVICE_UUID);
      this.rx = await service.getCharacteristic(RX_UUID);
      this.tx = await service.getCharacteristic(TX_UUID);
      if (this.over) return;
      // Sealed now, sent once the room is proven: well within the minute a hello stays fresh.
      this.hello = this.makeHello();
      this.hello.catch(() => {});
      this.link = new BleLink(t.ep, { appId: t.appId, roomId: t.roomId, central: true, mtu: MTU, addr: this.handle });
      this.onValue = (e) => this.value(e.target.value);
      this.tx.addEventListener('characteristicvaluechanged', this.onValue);
      await this.tx.startNotifications();
      if (this.over) return;
      this.later(CHALLENGE_MS, () => {
        if (this.link.state() !== 'proving') return;
        if (this.held) this.end('stale', 'the browser holds a dead connection (Bluetooth went off and on?): reload the page');
        else this.end('no-challenge', 'no room challenge came');
      });
      this.tick();
    } catch (e) {
      this.end('failed', e?.message ?? String(e));
    }
  }

  // A room hello as a member of a native room sends one (Rust's traversal.rs `hello`): our id, role and time, proved
  // under the room key (proof.js) and sealed under the room's hello key. `nb` marks a peer no WebRTC dials.
  async makeHello() {
    const { ep, proof, sealKey } = this.t;
    const meta = await (await proof).hello(ep.id, 'peer');
    return sealRoomBytes(await sealKey, { t: 'hello', from: ep.id, meta: { nb: 1, ...meta } });
  }

  value(view) {
    if (this.over || !this.link) return;
    const v = new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
    this.onEvents(this.link.push(v));
    this.check();
    this.flush();
  }

  // The link's events.
  onEvents(events) {
    for (const e of events) {
      if (this.over) return;
      if (e.type === 'proven') {
        this.hello.then((h) => {
          if (this.over) return;
          this.link.signal(INNER_ROOM, h);
          this.helloSent = true;
          this.flush();
          this.dial();
        }, (err) => this.end('failed', `the room hello: ${err?.message ?? err}`));
      } else if (e.type === 'peer') {
        const other = this.t.byPeer.get(e.peer);
        if (this.expect && e.peer !== this.expect) return this.end('wrong-peer', 'the device proved it is another peer');
        if (other && other !== this) return this.end('wrong-peer', 'linked with that peer over another connection');
        this.peer = e.peer;
        this.t.byPeer.set(e.peer, this);
        this.dial();
      } else if (e.type === 'closed') {
        const code = { 'wrong-proof': 'not-member', late: 'timeout' }[e.reason] ?? 'violation';
        this.end(code, `the link closed: ${e.reason}`);
      }
      // Signals: the member's room hello and offers. The page needs neither: the peer proved who it is.
    }
  }

  // Dials the peer over the connection, once our hello has gone and the peer proved who it is.
  dial() {
    if (this.dialled || !this.helloSent || !this.peer) return;
    this.dialled = true;
    this.t.ep.connect(this.handle, this.peer);
    this.check();
  }

  // Whether the link is up yet: the endpoint's handshake completed over this connection.
  check() {
    if (this.up || !this.dialled || this.over || !this.t.ep.isConnected(this.peer)) return;
    this.up = true;
    for (const id of this.timers) clearTimeout(id);
    this.timers.clear();
    this.resolve(this.peer);
    this.t.onPeer?.(this.peer, 'open');
  }

  // Runs the link's timer: a connection that hasn't proven the room and the peer's identity in 3 s closes.
  tick() {
    if (this.over) return;
    const ms = this.link.timeout();
    this.onEvents(this.link.events());
    if (ms !== null && !this.over) this.later(ms, () => this.tick());
  }

  datagram(bytes) {
    if (this.over || !this.link) return;
    this.link.datagram(bytes);
    this.flush();
  }

  // Writes what the link queued, one write at a time, without response.
  async flush() {
    if (this.writing || this.over || !this.link) return;
    this.writing = true;
    try {
      for (let vs = this.link.values(); vs.length && !this.over; vs = this.link.values()) {
        for (const v of vs) {
          if (this.over) return;
          await this.write(v);
        }
      }
    } catch (e) {
      this.end('failed', `a write: ${e?.message ?? e}`);
    } finally {
      this.writing = false;
      this.drained?.();
    }
  }

  async write(v) {
    for (let i = 1; ; i++) {
      try {
        if (this.rx.writeValueWithoutResponse) await this.rx.writeValueWithoutResponse(v);
        else await this.rx.writeValue(v);
        return;
      } catch (e) {
        if (i >= WRITE_TRIES || this.over) throw e;
        await sleep(WRITE_RETRY_MS);
      }
    }
  }

  // Closes the connection once what is queued has been written (the endpoint's goodbye), or after DROP_MS.
  closeAfterWrites() {
    if (this.over) return;
    this.drained = () => this.end('closed', 'dropped');
    this.later(DROP_MS, this.drained);
    if (!this.writing) this.flush();
  }

  // The device went: before the link was up, say why as well as the link can tell.
  gone() {
    if (this.over) return;
    const state = this.link?.state() ?? 'proving';
    if (state === 'answered') this.end('not-member', 'the device closed the connection on our answer: another room?');
    else if (state === 'proving') this.end('refused', 'the device closed the connection before it challenged us');
    else this.end('closed', 'the device closed the connection');
  }

  // Ends the connection: closes it, frees the link and settles the dial, or tells onPeer the link's connection went.
  end(code, message) {
    if (this.over) return;
    this.over = true;
    for (const id of this.timers) clearTimeout(id);
    this.timers.clear();
    const t = this.t;
    t.conns.delete(this.handle);
    if (this.peer && t.byPeer.get(this.peer) === this) t.byPeer.delete(this.peer);
    this.device.removeEventListener('gattserverdisconnected', this.onGone);
    this.tx?.removeEventListener('characteristicvaluechanged', this.onValue);
    this.link?.free();
    try {
      if (this.device.gatt?.connected) this.device.gatt.disconnect();
    } catch {
      // Already gone.
    }
    if (this.up) t.onPeer?.(this.peer, 'closed');
    else this.reject(failure(code, message));
  }
}
