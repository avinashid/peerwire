# Offline messages

An inbox leaves a message for one peer, by its PeerId, while that peer is offline: it gets the message the next time
it opens its inbox. No room is needed, nor anyone online. The message is sealed to the recipient and left on Nostr
relays (kind 30078), and expires after its `ttl`, 7 days at most. Inboxes are in `@peerwire/js`; the API is in the
[JavaScript reference](../reference/javascript.md#openinbox).

## Keep an identity

An inbox belongs to a PeerId, so the recipient needs the same identity each time. Save the 32-byte secret once, and
pass it to `joinRoom` and `openInbox`:

```js
import { joinRoom, openInbox, toHex, fromHex } from '@peerwire/js';

const saved = localStorage.getItem('peerwire-secret');
const room = await joinRoom({ appId: 'my-app', roomId, secret: saved ? fromHex(saved) : undefined });
const secret = room.endpoint.secret();
localStorage.setItem('peerwire-secret', toHex(secret)); // anyone who reads it can read your messages
```

`room.self` is that PeerId: share it with whoever may write to you, in presence or a link. An inbox opened without
`secret` gets a fresh identity, which can send but never receives, as nobody knows its PeerId.

## Receive and send

```js
const inbox = await openInbox({ appId: 'my-app', secret, persist: true });
inbox.on('message', (data, from, { kind, at, id }) => show(from, data)); // reads now, then every minute
const relays = await inbox.send(friendId, { text: 'call me' });       // the number of relays that took it
```

- The first `'message'` listener starts reading: at once, then every minute. Nothing is read while nobody listens.
  `inbox.check()` reads now.
- `send` resolves once every relay has answered, to how many took the message. Give it `{ ttl }` in whole seconds,
  1 to 604,800 (7 days, the default).
- `inbox.close()` stops reading and closes the relay connections.

## Delivered once

A message is delivered once, oldest first, to every listener, and with `persist` across sessions too: `true` for
IndexedDB, or a store as `room.doc` takes (see
[Keep a copy on the device](shared-documents.md#keep-a-copy-on-the-device)). Without `persist`, a message still on
the relays comes again in each new session. `persist` needs `secret`: what it
stores is sealed under a key from it.

An inbox remembers 4096 messages until they expire. Past that (anyone can send to a PeerId), the one that expires
soonest is forgotten, and comes again if anyone posts it again. A message that comes again has the same `id` (the
listener's third argument), so an app that keeps ids can drop it.

## What a relay sees

Neither recipient nor sender. Each message is sealed to the recipient's PeerId (Noise X: a fresh ephemeral key, the
sender's PeerId inside and authenticated), signed by a fresh Nostr key, tagged only with the app (a hash of the
`appId`) and padded to 256 bytes. Every inbox of the app reads every message of the app and opens only its own, so a
relay can't tell whose a message is, or which go together. It does see the IP of whoever leaves or reads messages,
when, and how many the app holds.

## Trust

Anyone can leave a message for any PeerId. `from` says who sealed it, and nobody else can read it: check `from`, and
every field you render. A message is read only in the app it was left for. A relay may keep a message for less than
its `ttl`, or not at all: an inbox is best effort.

## Bounds

- **Size.** 16 KiB a message (`MAX_MESSAGE`), and a JSON value nested 64 deep at most: `send` throws `RangeError`
  past either, and a deeper one that arrives is dropped.
- **Reading cost.** Reading costs one X25519 for each message of the app. A check opens at most 100 a relay, the
  newest first, and reads on down at the next, so a flood of messages delays yours but doesn't make a check longer.
- **Floods.** A relay can't be asked for the rest of a second: 100 messages or more left in the same second as yours,
  and ordered before it on that relay (by event id, which their sender chooses), hide yours there; its copies on other
  relays are still read. So a flood that follows the app's messages on every relay can keep them from you.
- **Clocks.** A message dated more than 15 minutes ahead of our clock, sent for longer than 7 days, or expired, is
  dropped. One whose sender's clock runs more than 15 minutes behind may be missed by an inbox that was reading when it
  was sent.

`packages/js/test/inbox.mjs` checks each of these on fake relays and a fake clock.

## Not yet

Messages over 16 KiB, and inboxes in Rust and Godot.
