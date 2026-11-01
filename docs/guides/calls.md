# Calls

`room.call(stream)` joins the room's voice and video call. Every member in the call sends its stream to every other
(a mesh), up to 6 by default. The app gets the stream; the library never asks for the camera or microphone. Calls
are in `@peerwire/js`, in the browser; the API is in the [JavaScript reference](../reference/javascript.md#call-1).
`packages/js/demo/meet.html` is a working meeting page built on it.

## Join a call

```html
<button id="join">Join the call</button>
<div id="tiles"></div>
<script type="module">
  import { joinRoom } from '@peerwire/js';

  const room = await joinRoom({ appId: 'my-app', roomId: 'call-demo' }); // two tabs; signaling: 'nostr' for two machines
  const tiles = new Map();
  const tile = (peer) => { // a peer's tile, made on first use
    if (!tiles.has(peer)) {
      const video = Object.assign(document.createElement('video'), { autoplay: true, playsInline: true });
      tiles.set(peer, document.getElementById('tiles').appendChild(video));
    }
    return tiles.get(peer);
  };

  join.onclick = async () => {
    join.disabled = true;                 // one call per room: room.call() throws while ours is open
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true, video: true }); // on a click
    const call = room.call(stream);
    call.on('state', (peer, state, reason) => (tile(peer).title = reason ? `${state}: ${reason}` : state));
    call.on('stream', (peer, s) => (tile(peer).srcObject = s));
    call.on('ended', (peer, reason) => { tile(peer).remove(); tiles.delete(peer); }); // 'left' or 'gone'
    call.on('full', (max) => alert(`The call is full (${max})`));
  };
</script>
```

- **`'state'`** comes first for each peer: `'connecting'` once its link is made, then `'connected'`, or `'failed'`
  with a reason. Draw the peer's tile from its first `'state'`: it gets our media from `'connected'` on.
- **`'stream'`** comes once per peer, whatever it sends: one audio and one video track. A peer that only receives
  still gets a stream (silent and black).
- **`'ended'`** says a peer left the call (`'left'`) or the room (`'gone'`). **`'full'`** says there was no seat for
  us: we are out of the call, and `room.call()` may try again later.
- `room.call(null)` joins to receive only. `call.peers`, `call.state(peer)` and `call.stream(peer)` read the same
  state at any time.

## Mute, turn the camera off, share the screen

```js
await call.setStream(null);                                                      // send nothing
await call.setStream(await navigator.mediaDevices.getDisplayMedia({ video: true })); // a screen share
await call.setStream(stream);                                                    // back to the camera
room.setPresence({ ...room.getPresence(), muted: true, camera: false });         // say so
call.leave();                                                                     // the room stays
```

`setStream` sends the first audio and first video track of its stream, or nothing for a kind it lacks. It swaps
tracks and never renegotiates. A peer's stream keeps its tracks when the peer changes what it sends; they go silent
or black. So put "muted" or "camera off" in presence rather than guessing from black frames. `room.leave()` leaves
the call too.

## How it works

Each member in the call gets an `RTCPeerConnection` for media alone, apart from the room's link to it, and is
signalled over that link, sealed, whatever route it takes. No call frame is a signalling relay event of its own: on a
link over the relays (a pipe) they go inside it, so a relay sees only their sizes and timing. Each connection
carries one audio and one video track each way.

**Seats.** Members in the call tell every room member so, so a member joining when `max` are in gets `'full'` at
once; one that calls before it knows who is in (as soon as it joins the room) gets it when it learns. A newcomer goes
on another member's `'full'` only when its own count has no seat for it either, and a member already linked keeps
its seat whatever a newcomer claims. Members are trusted alike here too: one can refuse to link a newcomer (that pair
fails `'no-path'`) or fill the call with PeerIds of its own, but not take a linked member's seat. Two joining at the
same moment for the last seat may both get `'full'`, or one stay in unlinked from a member that turned it away.

**Paths.** Media takes a direct path only: there is no TURN server unless you put one in `iceServers`. A peer whose
media doesn't connect within 20 s fails (`'no-path'`), shows as failed in `call.peers` until it leaves, and isn't
tried again until it or we join again. One that connected and lost ICE gets 2 restarts in a minute, then fails
(`'lost'`). The room's own link can still reach that peer by its
[fallbacks](../reference/javascript.md#through-any-nat); media can't.

**Privacy.** `joinRoom`'s `privacy` and `iceServers` apply to calls as to the room's links. It matters more here: a
page with microphone or camera permission shows its local addresses to connections made after the grant. With
`privacy: 'public-only'`, the call's candidates and descriptions carry none. See
[Privacy](../reference/javascript.md#privacy).

## What it costs

Each member uploads its stream once for every other: `(N − 1) × bitrate`, with the bitrates in
[Performance §6](../project/performance.md#6-meetings-audio-and-video-theoretical) (theoretical):

| In the call | Uplink per member at 720p (1.5 Mbit/s) | At 360p (0.5 Mbit/s) | Audio alone (32 kbit/s) |
|---|---|---|---|
| 6 (the default `max`) | 7.5 Mbit/s | 2.5 Mbit/s | 160 kbit/s |
| 16 (the largest `max`) | 22.5 Mbit/s | 7.5 Mbit/s | 480 kbit/s |

7.5 Mbit/s is more than many uplinks; the browser's congestion control lowers the video bitrate on a smaller one.
Raise `max` (`room.call(stream, { max: 16 })`) for audio or small video only.

## Bounds

What a member sends for a call is checked and rate-limited (`packages/js/src/call.js`):

| | |
|---|---|
| A call frame | 64 KiB |
| A description | 32 KiB, 4 media sections, 64 candidates |
| A candidate | 512 characters |
| Rate | 6 joins, 8 descriptions and 64 candidates at once, then one join every 5 s, one description every 3 s and four candidates a second |

Past those, frames are dropped.

## Not yet

A member forwarding the others' streams (peer SFU) and broadcast trees for larger calls, simulcast, more than one
video track (camera and screen at once), and calls in Rust and Godot.
