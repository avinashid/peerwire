# Quick start

Each section below gets two peers talking on one platform. They're independent: read the one you need.

## JavaScript

A chat in the browser, with [Vite](https://vite.dev) as the dev server. Create a folder with an `index.html`:

```html
<ul id="log"></ul>
<form id="chat"><input id="text" autocomplete="off"><button>Send</button></form>
<script type="module">
  import { joinRoom } from '@peerwire/js';
  const room = await joinRoom({ appId: 'quick-start', roomId: 'lobby' }); // signaling: 'nostr' to reach other machines
  const show = (who, text) => log.append(Object.assign(document.createElement('li'), { textContent: `${who}: ${text}` }));
  room.on('peer-join', (peer) => show(peer.slice(0, 6), 'joined'));
  room.on('message', (text, peer) => show(peer.slice(0, 6), text));
  chat.onsubmit = (e) => {
    e.preventDefault(); room.send(text.value);
    show('me', text.value); chat.reset();
  };
</script>
```

Install and run it:

```sh
npm init -y
npm install @peerwire/js vite
npx vite
```

Open the address Vite prints in two tabs. Each tab shows the other join; type in one and the line appears in the
other. The tabs meet over the browser's `BroadcastChannel` (`signaling: 'local'`, the default), and then talk over a
WebRTC DataChannel.

To chat between two devices, add `signaling: 'nostr'` to `joinRoom`. Peers then find each other through public
Nostr relays, and anyone who knows the room name can join, so use a name nobody can guess:

```js
import { joinRoom, randomRoomId } from '@peerwire/js';

const roomId = location.hash.slice(1) || (location.hash = randomRoomId()); // share the page's link to invite
const room = await joinRoom({ appId: 'quick-start', roomId, signaling: 'nostr' });
```

Next: [rooms and invites](../guides/rooms.md), [sending messages](../guides/messaging.md),
[shared documents](../guides/shared-documents.md), and the [JavaScript API](../reference/javascript.md).

## React

Start from Vite's React template and add the two packages:

```sh
npm create vite@latest my-app -- --template react-ts
cd my-app
npm install @peerwire/react @peerwire/js
```

Replace `src/App.tsx`:

```tsx
import { RoomProvider, usePeers, useSharedState } from '@peerwire/react';

export default function App() {
  return (
    <RoomProvider appId="quick-start" roomId="lobby">
      <Counter />
    </RoomProvider>
  );
}

function Counter() {
  const peers = usePeers();
  const [count, setCount] = useSharedState('count', 0);
  return (
    <button onClick={() => setCount((c) => c + 1)}>
      {count} (with {peers.length} other {peers.length === 1 ? 'member' : 'members'})
    </button>
  );
}
```

Run `npm run dev` and open the page in two tabs: the count is the same in both. As in JavaScript, pass
`signaling="nostr"` to `RoomProvider` to reach other devices. The [React guide](../guides/react.md) covers members,
chat, per-member state and checking what other members send.

## Rust

A chat between terminals. Create a project:

```sh
cargo new chat && cd chat
cargo add peerwire --features nostr
```

In `src/main.rs`:

```rust
use peerwire::{Event, Lane, Node, NostrSignaling};
use std::sync::mpsc;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let room = std::env::args().nth(1).ok_or("usage: chat <room name>")?;
    let mut node = Node::builder().signaling(NostrSignaling::default()).bind("[::]:0")?;
    node.join_room("quick-start", &room);
    println!("I am {}; waiting for others in the room", node.peer_id());

    // Read lines on another thread, so the network loop never waits on the keyboard.
    let (lines, typed) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });

    loop {
        node.poll(); // read the socket, run timers, send what's queued
        while let Some(event) = node.try_recv_event() {
            match event {
                Event::Connected { peer, path, .. } => println!("+ {peer} ({path:?})"),
                Event::Disconnected { peer, .. } => println!("- {peer}"),
                Event::Message { peer, data, .. } => {
                    println!("{peer}: {}", String::from_utf8_lossy(&data).escape_debug())
                }
                _ => {}
            }
        }
        while let Ok(line) = typed.try_recv() {
            let peers: Vec<_> = node.peers().copied().collect();
            for peer in &peers {
                node.send(peer, Lane::Reliable, line.as_bytes())?;
            }
        }
        std::thread::sleep(node.next_timeout().unwrap_or(Duration::MAX).min(Duration::from_millis(5)));
    }
}
```

Run it in two terminals, or on two machines anywhere on the internet, with the same room name:

```sh
cargo run -- 9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4
```

Each side prints `+ <peer>` when the link is up; then type to chat. `path` says whether the link is direct or
relayed through another peer.

- **No internet?** Use `LanSignaling::new()?` instead of `NostrSignaling::default()`, and `"0.0.0.0:0"` as the
  address: peers on the same network find each other over multicast. See [LAN discovery](../guides/lan-discovery.md).
- **Async?** With the `tokio` feature, `AsyncNode` replaces the poll loop with `node.next_event().await`. See
  [sending messages](../guides/messaging.md#the-event-loop).
- **The repository has more:** `cargo run -p peerwire --example echo` connects by invite with no signaling at all, and
  [examples](../../examples/README.md) lists the rest.

## Godot 4

Build the extension and add it to your project as [installation](installation.md#godot-4) describes. Then, in a
script on your main scene:

```gdscript
extends Node

const APP := "quick-start"
const ROOM := "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4"   # share it with the players; it's the room's password

func host() -> void:
	var peer := PeerwireMultiplayerPeer.new()
	peer.host_room(APP, ROOM)
	multiplayer.multiplayer_peer = peer

func join() -> void:
	var peer := PeerwireMultiplayerPeer.new()
	peer.join_room(APP, ROOM)
	multiplayer.multiplayer_peer = peer
	multiplayer.connected_to_server.connect(func(): say.rpc("hello from %d" % multiplayer.get_unique_id()))

@rpc("any_peer", "reliable")
func say(text: String) -> void:
	print(text)
```

Call `host()` in one copy of the game and `join()` in another, on the same machine or anywhere else. From then on,
Godot's multiplayer API works as usual: `@rpc`, `MultiplayerSynchronizer` and `MultiplayerSpawner` all go over
Peerwire. The [Godot guide](../guides/godot.md) covers invites, shared documents, NAT traversal and the demo project.
