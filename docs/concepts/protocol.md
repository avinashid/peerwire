# Protocol

This page describes what goes on the wire between two Peerwire peers: identities, the handshake, packets and frames,
the lanes, and invites. The source of truth is `peerwire-core`: `crypto.rs`, `wire.rs`, `reliable.rs` and
`invite.rs`. Signaling messages (hellos and offers) are in [NAT traversal](nat-traversal.md#messages).

> [!NOTE]
> Builds before 1.0 don't promise to interoperate. A peer that meets a frame type it doesn't know takes it as a
> protocol violation and drops the link.

## Identity and handshake

- **Identity.** An Ed25519 key pair. A `PeerId` is the 32-byte public key. Its X25519 (Montgomery) form is the
  Noise static key, so one key both signs and handshakes.
- **Handshake.** `Noise_IK_25519_ChaChaPoly_BLAKE2s` with the prologue `peerwire-core/1`. The initiator already
  knows the responder's static key, because that's the PeerId it dialled, so the handshake takes **one round trip**
  and nobody can answer in the target's place. The initiator retries every 500 ms, up to 6 times, and gives up
  after 10 s.
- **Simultaneous dials.** When both sides dial at once, as hole punching needs, the dial from the smaller PeerId
  wins, so exactly one session results.
- **Data packets.** ChaCha20-Poly1305 with an explicit 64-bit nonce, and a 2048-packet sliding anti-replay window,
  as in WireGuard.
- **Under load.** A responder with many half-open handshakes answers an HS1 with a 16-byte cookie instead of doing
  the Diffie-Hellman work, and the initiator repeats its HS1 with the cookie (HS1C). That keeps a flood of forged
  HS1s from filling the half-open table.

## Packets

All integers are little-endian, and all parsing is bounds-checked and allocation-free. A datagram is at most
**1,232 bytes**: the IPv6 minimum MTU (1,280) less the IPv6 and UDP headers, so IP never fragments it.

```text
packet = kind(u8: version<<4 | type) ...
  HS1     kind  sender_idx(u32)                      noise_msg1(128)
  HS2     kind  sender_idx(u32)  receiver_idx(u32)   noise_msg2
  DATA    kind  receiver_idx(u32) nonce(u64)         AEAD(frames) + 16-byte tag
  COOKIE  kind  receiver_idx(u32) cookie(16)         a loaded responder's answer to an HS1
  HS1C    kind  sender_idx(u32)  cookie(16)          noise_msg1(128): an HS1 that echoes it
```

A DATA packet holds one or more frames:

```text
  PING                                       keepalive and liveness probe
  ACK        cum(u32) bits(u64)              cumulative ACK plus a 64-bit selective bitmap
  REL        seq(u32) flags(u8) len(u16) data   reliable fragment (≤ 1,100 bytes)
  UREL / UACK                                the same as REL / ACK, for the urgent lane's own sequence
  UNREL      len(u16) data                   unreliable message (≤ 1,100 bytes)
  CLOSE                                      the link is closing
  OBSERVED   alen(u8) addr                   "I see you at addr" (your reflexive address)
  FWD        to(32) len(u16) packet          relay: please forward this packet to peer `to`
  FWD_QUIET  to(32) len(u16) packet          the same, without telling `to` my address (relay-only peers)
  RELAYED    alen(u8) addr len(u16) packet   a relayed packet, and where the relay saw its origin
  PUNCH      token(u64)                      hole-punch probe
  PUNCH_ACK  token(u64)                      its echo
```

Overhead is 13 bytes of header and a 16-byte tag per packet, plus 3 bytes per unreliable message or 8 per reliable
fragment. A message of `s` bytes costs about **`s + 60` bytes** on the wire natively over IPv4, and messages sent in
the same tick share a packet.

A relay forwards only ciphertext: `FWD` and `RELAYED` wrap the end-to-end packet, which the relay can't read or
change. A whole relayed packet still fits in one datagram.

## Lanes and reliability

| Lane | Guarantee | Size |
|---|---|---|
| `Unreliable` | at most once; the oldest is dropped when the queue (256) is full | ≤ 1,100 bytes |
| `Reliable` | ordered, exactly once, fragmented | ≤ 1 MiB (`Config::max_message`) |
| `Urgent` | exactly once, ordered among urgent messages, ahead of queued reliable data; every fragment sent twice, 4 ms apart | ≤ 64 KiB |

The reliable lane is a small transport tuned for real-time traffic:

- **Selective ACKs.** A cumulative ACK plus a 64-bit bitmap. Fragments 3 sequence numbers behind the highest ACK are
  retransmitted at once, without waiting for a timeout, once per loss: only an ACK for a fragment sent after the
  last copy counts, and a quarter of the minimum RTT is allowed for reordering (as in RACK) before resending.
- **Retransmission timeout.** 300 ms at first, kept between 20 ms and 5 s, with back-off capped at 10 s. A 25 ms
  ACK-delay allowance (like QUIC's `max_ack_delay`) covers busy game frames and browser event loops. A fragment is
  declared lost for good after 16 sends.
- **Tail-loss probe.** If nothing is acknowledged for `srtt + 2·rttvar + 4 ms`, the newest fragment is sent again,
  so a lost tail is repaired in about one RTT instead of a full timeout. That matters for sparse traffic, such as a
  lone event. In a steady transfer the ACKs keep coming, and a fragment lost at the end of a burst reaches its
  timeout first: if an ACK acknowledged new data after it went out, that expiry is its probe, as in RACK-TLP
  (RFC 8985), and the loss counts as one found by fast retransmit, not as a timeout.
- **Congestion control.** The window starts at 16 fragments, with a floor of 4. It backs off on loss, and also when
  queueing delay (RTT above the lowest seen) passes **25 ms**, before any loss. That keeps router and DataChannel
  buffers short.
- **Receive window.** 4,096 out-of-order fragments are buffered ahead of the next one expected (64 on the urgent
  stream), and the sender never sends further ahead than that.
- **Send buffer.** 4 MiB per link for reliable and for urgent each (`Config::max_send_buffer`); `send` returns
  `WouldBlock` past it.
- **Liveness.** A keepalive goes out every 10 s, and a link is dropped after 30 s of silence. Links survive address
  changes (roaming), and `PathChanged` reports the move.

## Invites

```text
"pw:" base64url( version(u8)=2  peer(32)  key(32)
                 n(u8)  [alen(u8) addr]*                      addresses to try
                 m(u8)  [relay_peer(32) alen(u8) addr]* )     relay peers to reach it through
```

An invite is a public key, some places to try it, and an **invite key**. Whoever answers must prove in the handshake
that they hold the public key. The invite key is a capability, not a session secret: `Node::connect` also sends the
host a sealed offer proved with it, so the host dials back at the same moment and the two punch through. Someone who
knows only the PeerId gets no answer and no address. Each list holds at most 16 entries. Version 1 (with no key)
still parses; such an invite works only where the host can be reached without a dial-back.

## Signaling

Hellos and offers travel over an untrusted publish/subscribe carrier, on 32-byte topics:

- the room topic: `sha256("peerwire/room/" + app + "/" + room)`;
- a peer's inbox: `sha256("peerwire/peer/" + hex(PeerId))`.

Hellos are sealed and proved under keys derived from the room name; offers are `Noise_X` sealed boxes to one
recipient. Rust and the JavaScript package derive the same topics and keys, so they can share relays and rooms.
[NAT traversal](nat-traversal.md) has the message formats, the rules a node follows, and its budgets against floods.
