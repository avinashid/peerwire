// A board's LED (NB-110d, NB-126): switch the LED of an ESP32 board from the page, over Bluetooth when the board is
// near (pick it in the list: ports/esp32's ble_led), or over the internet through the room's Nostr relays, from
// anywhere (a board on Wi-Fi in the room, with the same commands). The page links with the board over Bluetooth alone
// (bluetooth.js) or over the relays alone (relay_link.js), then sends it `led on`, `led off` or `led toggle` as the
// board takes them, raw text on the reliable lane; what the board says shows in the log. Over the internet the page
// dials the first native peer whose hello it hears in the room: the board answers each command, so another page in
// the room would show as a peer that says nothing back.
//
// ?app= and ?room= name the room: the board's app is peerwire-lan-chat, and its room esp32 unless room.sh named
// another. ?via=bluetooth or ?via=internet picks the way (Bluetooth where the browser has it), and ?relays= the relays
// (comma-separated ws:// or wss:// urls). Bluetooth needs Web Bluetooth (Chrome or Edge; on a phone, Android Chrome)
// on an https page; the internet way, any browser.
import { loadPeerwire } from '../src/peerwire.js';
import { BluetoothTransport, BLUETOOTH_WASM } from '../src/bluetooth.js';
import { RelayTransport } from '../src/relay_link.js';
import { DEFAULT_RELAYS } from '../src/nostr.js';

const params = new URLSearchParams(location.search);
const $ = (id) => document.getElementById(id);
const dec = new TextDecoder();
$('app').value = params.get('app') ?? 'peerwire-lan-chat';
$('room').value = params.get('room') ?? 'esp32';
$('via').value = ['bluetooth', 'internet'].includes(params.get('via')) ? params.get('via') : navigator.bluetooth ? 'bluetooth' : 'internet';
const relays = params.get('relays')?.split(',').filter(Boolean) ?? DEFAULT_RELAYS;
// No board heard this long after joining the relays: say so (and go on listening).
const QUIET_MS = 40000;

const log = (s) => {
  const d = $('log');
  d.textContent += `${new Date().toTimeString().slice(0, 8)} ${s}\n`;
  d.scrollTop = d.scrollHeight;
};
const setLinked = (on) => {
  for (const b of document.querySelectorAll('.led')) b.disabled = !on;
  $('state').textContent = on ? `linked with ${peer.slice(0, 12)} over ${$('via').value === 'bluetooth' ? 'Bluetooth' : 'the relays'}` : 'not linked';
};
const showVia = () => {
  const bt = $('via').value === 'bluetooth';
  $('connect').textContent = bt ? 'Connect to the board' : 'Find the board online';
  $('connect').disabled = bt && !navigator.bluetooth;
  if (bt && !navigator.bluetooth) log('This browser has no Web Bluetooth. Use Chrome or Edge (on a phone, Android Chrome) on an https page, or the internet.');
};

let endpoint = null;
let link = null;
let peer = null;
let run = 0; // the newest connect; an older one that ends late does nothing
let quiet = null;
// The wasm of each way, loaded once, as soon as the way is picked: a click then waits for nothing slow before the
// device picker, which needs the click's user gesture. The module with Bluetooth is ~50 KB more. One that failed to
// load is fetched again at the next click.
const modules = new Map();
const module = (via) => {
  let p = modules.get(via);
  if (!p) {
    p = loadPeerwire(via === 'bluetooth' ? BLUETOOTH_WASM : new URL('../pkg/peerwire_ffi.wasm', import.meta.url));
    modules.set(via, p);
    p.catch(() => modules.get(via) === p && modules.delete(via));
  }
  return p;
};
setLinked(false);
showVia();
module($('via').value);
$('via').onchange = () => {
  stop();
  showVia();
  module($('via').value);
};

function stop() {
  run++;
  clearTimeout(quiet);
  link?.close();
  endpoint?.close();
  link = endpoint = peer = null;
  setLinked(false);
}

$('connect').onclick = async () => {
  stop();
  const mine = run;
  const via = $('via').value;
  const room = { appId: $('app').value, roomId: $('room').value };
  let pw;
  try {
    pw = await module(via);
  } catch (e) {
    return void log(`the wasm didn't load: ${e.message}`);
  }
  if (mine !== run) return;
  endpoint = pw.endpoint();
  endpoint.onEvent = (e) => {
    if (e.type === 'message') log(`board: ${dec.decode(e.data)}`);
    else if (e.type === 'disconnected' && e.peer === peer) {
      log(`link closed (${e.reason})`);
      setLinked(false);
    }
  };
  if (via === 'bluetooth') return bluetooth(room, mine);
  online(room, mine);
};

async function bluetooth(room, mine) {
  const onPeer = (p, state) => {
    if (state !== 'closed' || mine !== run) return;
    log('the Bluetooth connection closed');
    setLinked(false);
  };
  link = new BluetoothTransport(endpoint, { ...room, onPeer });
  log('pick the board in the list…');
  try {
    const p = await link.dial();
    if (mine !== run) return;
    peer = p;
    log(`linked with ${peer.slice(0, 12)} over Bluetooth`);
    setLinked(true);
  } catch (e) {
    if (mine === run) log(`not linked: ${e.code ?? 'error'}: ${e.message}`);
  }
}

function online(room, mine) {
  let dialling = false;
  const onPeer = async (p, state) => {
    if (state !== 'seen' || mine !== run || dialling || peer) return;
    dialling = true;
    clearTimeout(quiet);
    log(`heard ${p.slice(0, 12)} in the room: linking over the relays…`);
    try {
      const linked = await link.dial(p);
      if (mine !== run) return;
      peer = linked;
      log(`linked with ${peer.slice(0, 12)} over the relays`);
      setLinked(true);
    } catch (e) {
      if (mine === run) log(`not linked: ${e.code ?? 'error'}: ${e.message}`);
    } finally {
      dialling = false;
    }
  };
  link = new RelayTransport(endpoint, { ...room, relays, onPeer });
  log(`listening for the board on ${relays.length} relay(s)…`);
  quiet = setTimeout(() => mine === run && !peer && log('no board heard yet: is it on Wi-Fi, in this room? (still listening)'), QUIET_MS);
}

for (const b of document.querySelectorAll('.led')) {
  b.onclick = () => {
    const text = `led ${b.dataset.cmd}`;
    const r = endpoint?.send(peer, text);
    log(r === 0 ? `you: ${text}` : `not sent (${r})`);
  };
}
