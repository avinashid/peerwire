// A board's LED over Bluetooth (NB-110d): pick the ESP32 running ports/esp32's ble_led, and switch its LED from the
// page. The page links with the board over Bluetooth alone (bluetooth.js) and sends it `led on`, `led off` or `led
// toggle` as ble_led takes them, raw text on the reliable lane; what the board says shows in the log. ?app= and
// ?room= name the room: ble_led's app is peerwire-lan-chat, and its room esp32 unless room.sh named another. Needs
// Web Bluetooth (Chrome or Edge; on a phone, Android Chrome) on an https page.
import { loadPeerwire } from '../src/peerwire.js';
import { BluetoothTransport, BLUETOOTH_WASM } from '../src/bluetooth.js';

const params = new URLSearchParams(location.search);
const $ = (id) => document.getElementById(id);
const dec = new TextDecoder();
$('app').value = params.get('app') ?? 'peerwire-lan-chat';
$('room').value = params.get('room') ?? 'esp32';

const log = (s) => {
  const d = $('log');
  d.textContent += `${new Date().toTimeString().slice(0, 8)} ${s}\n`;
  d.scrollTop = d.scrollHeight;
};
const setLinked = (on) => {
  for (const b of document.querySelectorAll('.led')) b.disabled = !on;
  $('state').textContent = on ? `linked with ${peer.slice(0, 12)}` : 'not linked';
};

let endpoint = null;
let bt = null;
let peer = null;
setLinked(false);
const pw = await loadPeerwire(BLUETOOTH_WASM);
if (!navigator.bluetooth) {
  $('connect').disabled = true;
  log('This browser has no Web Bluetooth. Use Chrome or Edge (on a phone, Android Chrome), on an https page.');
}

$('connect').onclick = async () => {
  bt?.close();
  endpoint?.close();
  setLinked(false);
  endpoint = pw.endpoint();
  endpoint.onEvent = (e) => {
    if (e.type === 'message') log(`board: ${dec.decode(e.data)}`);
    else if (e.type === 'disconnected' && e.peer === peer) {
      log(`link closed (${e.reason})`);
      setLinked(false);
    }
  };
  const onPeer = (p, state) => {
    if (state !== 'closed') return;
    log('the Bluetooth connection closed');
    setLinked(false);
  };
  bt = new BluetoothTransport(endpoint, { appId: $('app').value, roomId: $('room').value, onPeer });
  log('pick the board in the list…');
  try {
    peer = await bt.dial();
    log(`linked with ${peer.slice(0, 12)} over Bluetooth`);
    setLinked(true);
  } catch (e) {
    log(`not linked: ${e.code ?? 'error'}: ${e.message}`);
  }
};

for (const b of document.querySelectorAll('.led')) {
  b.onclick = () => {
    const text = `led ${b.dataset.cmd}`;
    const r = endpoint?.send(peer, text);
    log(r === 0 ? `you: ${text}` : `not sent (${r})`);
  };
}
