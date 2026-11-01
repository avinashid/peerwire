# @peerwire/js

Peer-to-peer rooms for the browser and Node, with no server of your own. Peers that share a room name find each other
through public Nostr relays (or a BroadcastChannel, for tabs of one browser), link over WebRTC DataChannels, and talk
over an end-to-end encrypted session. Underneath is the Peerwire core in Rust, compiled to a WebAssembly module of
355,588 bytes with no imports (as `packages/js/build.sh` builds it on main), and no wasm-bindgen.

```sh
npm install @peerwire/js
```

## A chat in ten lines

<!-- readme-chat: test/readme.mjs runs this block in two browser tabs -->
```html
<ul id="log"></ul>
<form id="chat"><input id="text" autocomplete="off"><button>Send</button></form>
<script type="module">
  import { joinRoom } from '@peerwire/js';
  const room = await joinRoom({ appId: 'readme-chat', roomId: 'lobby' }); // signaling: 'nostr' to reach other machines
  const show = (who, text) => log.append(Object.assign(document.createElement('li'), { textContent: `${who}: ${text}` }));
  room.on('peer-join', (peer) => show(peer.slice(0, 6), 'joined'));
  room.on('message', (text, peer) => show(peer.slice(0, 6), text));
  chat.onsubmit = (e) => {
    e.preventDefault(); room.send(text.value);
    show('me', text.value); chat.reset();
  };
</script>
```

Open it in two tabs. With a bundler (Vite, webpack, esbuild) `@peerwire/js` resolves
from npm and the wasm is found next to the package. Without one, map it in the
page: `<script type="importmap">{"imports":{"@peerwire/js":"/path/to/packages/js/src/index.js"}}</script>`.

## Highlights

- **No server to run.** Rooms meet over public Nostr relays and public STUN. A room's id is its secret: relays see
  only a hash of it, and peers prove they know it before they see anything (`randomRoomId()` for a closed room).
- **Encrypted end to end.** Every link is a Noise IK session between two PeerIds, with reliable, unreliable and urgent
  lanes, whatever carries it.
- **Through any NAT.** A peer WebRTC can't reach is reached through another member, or over the signalling relays,
  with no TURN server and no setup; the link moves back to a direct path when one opens.
- **Shared documents.** `room.doc()`: maps, sets and logs every member holds, with catch-up for late joiners and
  sealed copies on the device or the relays.
- **Calls and offline messages.** `room.call()` for voice and video between members (6 by default, a mesh), and
  `openInbox()` for sealed messages left on the relays for a peer who is offline.

## Docs

- [Quick start](https://avinashid.github.io/peerwire/docs/getting-started/quick-start.html) and
  [installation](https://avinashid.github.io/peerwire/docs/getting-started/installation.html)
- [JavaScript API reference](https://avinashid.github.io/peerwire/docs/reference/javascript.html): every option,
  event and limit
- Guides: [rooms](https://avinashid.github.io/peerwire/docs/guides/rooms.html),
  [messages](https://avinashid.github.io/peerwire/docs/guides/messaging.html),
  [shared documents](https://avinashid.github.io/peerwire/docs/guides/shared-documents.html),
  [calls](https://avinashid.github.io/peerwire/docs/guides/calls.html),
  [offline messages](https://avinashid.github.io/peerwire/docs/guides/offline-messages.html),
  [privacy](https://avinashid.github.io/peerwire/docs/guides/privacy.html)
- React: `@peerwire/react`, its [guide](https://avinashid.github.io/peerwire/docs/guides/react.html) and
  [reference](https://avinashid.github.io/peerwire/docs/reference/react.html)
- [All the docs](https://avinashid.github.io/peerwire/docs/README.html) and the
  [source](https://github.com/avinashid/peerwire)

## From source

```sh
./packages/js/build.sh                         # -> packages/js/pkg/peerwire_ffi.wasm (355,588 bytes on main)
python3 -m http.server -d packages/js 8000     # any static server; the demos are in demo/
```

## License

MIT OR Apache-2.0, at your option.
