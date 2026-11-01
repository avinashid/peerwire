# Sending messages

Every link between two peers carries three lanes. Pick the lane by what a late or lost message costs you.

## Lanes

| Lane | Delivery | Largest message | Use for |
|---|---|---|---|
| **Reliable** (default) | exactly once, in order | 1 MiB (`Config::max_message`) | chat, document edits, game events |
| **Unreliable** | at most once, maybe out of order; when the queue is full the oldest is dropped | 1,100 bytes: one datagram | positions, cursors, voice activity: anything where only the latest value matters |
| **Urgent** | exactly once, in order among urgent messages; goes out ahead of queued reliable data, and every fragment is sent twice, 4 ms apart | 64 KiB | inputs and hits: small events where p99 latency matters more than bytes |

Urgent messages arrive marked as reliable: the lane is about how they're sent, not how they're read. A lost urgent
fragment costs nothing when its copy gets through, instead of a retransmission round trip.

Messages sent in the same tick share packets, so many small messages don't cost a packet each. On a native link, a
message of `s` bytes costs about `s + 60` bytes on the wire over IPv4. [Performance](../project/performance.md) has
the numbers for games, whiteboards, chat and calls.

## Sending

In JavaScript, `room.send` goes to every member unless you name one or several:

```js
room.send('hi');                                    // reliable, to every member
room.send({ x: 1, y: 2 }, { lane: 'unreliable' });  // latest-wins state
room.send(new Uint8Array([1, 2, 3]), { to: peer }); // one member (or an array of them)
room.send({ hit: target }, { lane: 'urgent' });
```

Strings arrive as strings, `Uint8Array` and `ArrayBuffer` as `Uint8Array`, and anything else as JSON. `send` throws a
`RangeError` for a message over its lane's limit. Reliable sends past the send window queue up rather than fail.

In Rust, `send` goes to one peer, as bytes:

```rust
use peerwire::{Lane, Node, PeerId, SendError};

fn broadcast(node: &mut Node, data: &[u8]) {
    let peers: Vec<PeerId> = node.peers().copied().collect();
    for peer in &peers {
        match node.send(peer, Lane::Reliable, data) {
            Ok(()) => {}
            Err(SendError::WouldBlock) => { /* 4 MiB already queued for this peer: try again later */ }
            Err(e) => eprintln!("{peer}: {e:?}"),
        }
    }
}
```

`send` fails with `NotConnected` when there's no link to that peer, `TooLarge` over the lane's limit, and
`WouldBlock` when the peer already has 4 MiB queued or unacknowledged on that lane (`Config::max_send_buffer`).
Reliable and urgent each have their own buffer, so a full reliable lane never refuses an urgent message.

## Receiving

In JavaScript, listen for events on the room:

```js
room.on('peer-join', (peer, presence) => {});
room.on('peer-leave', (peer, reason) => {});        // 'left', 'closed', 'failed', 'timeout', 'replaced', 'violation'
room.on('message', (data, peer, { lane }) => {});
```

A member counts as joined once its presence has arrived, so `peer-join` always carries it. To hand a newcomer the
room's state, send it from `peer-join` with `{ to: peer }` on the reliable lane: it arrives after your own presence,
so the newcomer already lists you.

In Rust, a node produces events:

| Event | When |
|---|---|
| `Connected { peer, path, initiator }` | a link is up. `path` is `Direct(addr)` or `Via(relay)`; `initiator` says whether we dialled |
| `Message { peer, lane, data }` | a message arrived |
| `Disconnected { peer, reason }` | a link closed: `Timeout` (30 s of silence), `ClosedByPeer`, `Replaced` (the peer re-dialled; a new `Connected` follows) or `ProtocolViolation` |
| `ConnectFailed { peer }` | a dial gave up. `node.failure(&peer)` says why, for example which side's NAT is symmetric |
| `PathChanged { peer, path }` | a link moved: relayed to direct after a punch, direct to relayed, or to a new address (roaming) |
| `AddrObserved { by, addr }` | a peer told us the address it sees us at |

`Replaced` can show up right after two room members dial each other at the same moment: one of the two handshakes
wins, and the link reports `Disconnected { Replaced }` then `Connected` again within about 100 ms.

## The event loop

`peerwire-core` keeps no thread or timer of its own, so something has to drive it. Each platform does that for you
in its own style.

**Rust, blocking.** Call `poll()`, drain the events, and sleep until the next timer:

```rust
use peerwire::{Event, Node};
use std::time::Duration;

fn run(node: &mut Node) {
    loop {
        node.poll(); // read the socket, fire timers, flush sends
        while let Some(event) = node.try_recv_event() {
            if let Event::Message { peer, data, .. } = event {
                println!("{peer}: {} bytes", data.len());
            }
        }
        std::thread::sleep(node.next_timeout().unwrap_or(Duration::MAX).min(Duration::from_millis(5)));
    }
}
```

A game can call `poll()` once a frame instead, and skip the sleep.

**Rust, async.** With the `tokio` feature, `AsyncNode` waits on the socket and the timers itself:

```rust
use peerwire::{AsyncNode, Event};

async fn run(mut node: AsyncNode) {
    loop {
        if let Event::Message { peer, data, .. } = node.next_event().await {
            println!("{peer}: {} bytes", data.len());
        }
    }
}
```

`AsyncNode` dereferences to `Node`, so `join_room`, `invite`, `peers` and the rest work on it as they do on `Node`.

**JavaScript, callbacks or polling.** Rooms drive themselves and call your handlers. Engines that handle callbacks
poorly, such as Godot's web export, can poll once a frame instead:

```js
const room = await joinRoom({ appId: 'my-game', roomId: 'match-1', queue: true });
// every frame:
for (const e of room.poll()) {
  // { type: 'peer-join' | 'presence', peer, presence }
  // { type: 'peer-leave', peer, reason }
  // { type: 'message', peer, data, kind: 'text' | 'bytes' | 'json', lane }
}
room.sendTo('', 'x,y', 'unreliable'); // '' or null: everyone; else a peer id
```

**Godot.** The binding drives the node from `_poll()`, which Godot's `SceneMultiplayer` calls every frame. Use `@rpc`
with `"reliable"`, `"unreliable"` or `"unreliable_ordered"` as usual: see the [Godot guide](godot.md).

## Link quality

| | JavaScript | Rust |
|---|---|---|
| Round-trip time | `room.rtt(peer)` | `node.peer_stats(&peer)` |
| Route | `room.route(peer)`: `direct`, `peer` (through another member) or `signaling` | `node.path(&peer)`: `Direct(addr)` or `Via(relay)` |
| Node-wide counters | `room.endpoint` | `node.stats()` |

When a link goes through a relay, it keeps trying to go direct and moves as soon as a punch is confirmed
(`PathChanged` in Rust). [NAT traversal](../concepts/nat-traversal.md) explains when that happens.

## How reliability works

The reliable lane is a small transport tuned for real time: a cumulative ACK with a 64-bit selective bitmap, fast
retransmits, tail-loss probes, and congestion control that backs off when queueing delay rises past 25 ms, before
any loss, so router and DataChannel buffers stay short. [Protocol](../concepts/protocol.md#lanes-and-reliability)
has the details.
