# Privacy

A direct link is fast because packets go straight from one peer to the other. That also means each side sees the
other's IP address: it's on every packet. Peerwire lets each peer choose which of its addresses it gives out, and
whether to link directly at all.

What signaling relays learn is a separate question, covered in [security model](../concepts/security-model.md): in
short, they see hashed topics, sealed bytes and the IP address you connect to them from, never your candidate
addresses.

## Privacy modes

| Mode | Invites and offers carry | Links | Cost |
|---|---|---|---|
| **Direct** (default) | every candidate: public, LAN, loopback | direct wherever possible | none |
| **Public only** | public addresses only: no LAN, loopback, CGNAT or link-local | direct wherever possible | two peers on one LAN go through their router (if it supports hairpinning) or a relay |
| **Relay only** | nothing, except to the relay | every link except the relay's goes through the relay and never moves to direct | each message takes `rtt(A, relay) + rtt(relay, B)`, and the relay uploads every byte again |

A peer you link with directly still sees the address your packets come from, in every mode but relay-only.

### Rust

```rust
use peerwire::{Node, Privacy};

fn main() -> std::io::Result<()> {
    let mut node = Node::builder().privacy(Privacy::PublicOnly).bind("[::]:0")?;
    // later, at runtime: the next invites and offers change; links already up stay
    node.set_privacy(Privacy::Direct);
    Ok(())
}
```

`Privacy::RelayOnly { via }` names the relay, or with `via: None` takes the room's host: the pinned one, otherwise the
first member that proves the host role and is dialled. With no relay linked, `connect` fails with
`ConnectError::NoRelay` rather than going direct.

### Godot

`peer.set_privacy_mode(PRIVACY_PUBLIC_ONLY)`, or `PRIVACY_DIRECT`, `PRIVACY_RELAY_ONLY`. In Godot every player links
only with the host anyway, so players never see each other's addresses in any mode.

### JavaScript

```js
const room = await joinRoom({ appId: 'my-app', roomId, signaling: 'nostr', privacy: 'public-only' });
```

`'public-only'` leaves out `host` ICE candidates (LAN, loopback and mDNS `.local` names), in trickled ICE and in the
SDP, and clears any local address from a candidate's related address and the SDP's `c=` line. Only what STUN reports
goes out, so two tabs on one machine with no STUN server can't link this way. The room's calls use the same filter.

Rooms don't offer relay-only through a peer. To keep your addresses from every member, give the room your own TURN
server and the policy `order: ['turn']`: WebRTC then offers relay candidates alone (see
[choosing paths](connection-paths.md)). The lower layer has relay-only through a peer: `new WebRtcTransport(endpoint,
signaling, { privacy: 'relay-only', relay })` opens WebRTC to the relay only, and a peer that isn't linked to the
relay can't be reached. See the [JavaScript API](../reference/javascript.md).

#### Microphone and camera permission

Until a page has microphone or camera permission, browsers hide its local addresses behind mDNS names. Once an origin
has that permission, connections made afterwards list every network interface. The grant is stored per origin, so it
also applies on later visits and on pages of that origin that never ask.

- With `'direct'`, members then get every interface's real address, including LAN, hotspot and IPv6 ones. That's
  also what lets a phone and a laptop on its hotspot link directly: in one field test (October 2026, Android Chrome
  on mobile data and a laptop on its hotspot), the pair linked directly in 1.2 s at about 10 ms with permission on
  both pages, and over the signaling relays in 5–6 s at about 600 ms without it.
- With `'public-only'`, none of those go out, and neither does that direct path. Each interface's public address,
  as STUN sees it, still does. On IPv6 there's no NAT, so that public address is the device's own: a phone on
  mobile data can show peers its IPv6 address in either mode (Android Chrome lists it even without the permission).

## Who learns what

| Party | Direct | Public only | Relay only |
|---|---|---|---|
| A room member, or someone with your invite | all your candidate addresses | your public addresses | none, unless it's your relay |
| A peer linked to you directly | also the address your packets come from | the same | (only your relay is) |
| The relay of a relayed link | that the two ends talk, both their IP addresses, packet sizes and timing; never the content | the same | the same, for every link but its own |

Someone with your invite whom your node didn't dial back gets only your public addresses in offers, and none of the
PeerIds you're linked to, even after its dial failed.

Under relay-only, the relay is asked not to tell the far end where it sees you. A relay-only node's invite names its
relay with the relay's address, so whoever holds the invite learns the relay's IP. In a room without a pinned host,
anyone who knows the room name can claim the host role and become the relay, and learn your IP: **pin the host when
you know its key** (`join_room_with(.., Some(host))`, Godot `join_room_host`).

In the simulator, a room with a public host and 37 pairs of peers links 25 pairs directly and relays 12 in Direct
mode, 24 and 13 in public-only (the LAN pair goes through the host), and 0 and 37 in relay-only. None fail.

[NAT traversal](../concepts/nat-traversal.md#privacy-modes) has the rules each mode follows, and the
[threat model](../project/threat-model.md) the review findings behind them.
