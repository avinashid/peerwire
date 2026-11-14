# Rooms and invites

Peers meet in one of two ways:

- **A room**: everyone who joins the same room name connects to everyone else (or to a host). Nobody hands
  anything around except the name.
- **An invite**: one peer prints a `pw:` string (a link, a QR code, a chat message) and another dials it. It
  connects exactly those two peers, and needs no signaling at all when the host can be reached.

## Joining a room

A room is an app id plus a room name. Rooms of different apps never meet, even with the same name.

```js
import { joinRoom, randomRoomId } from '@peerwire/js';

const room = await joinRoom({ appId: 'my-app', roomId: randomRoomId(), signaling: 'nostr' });
```

```rust
use peerwire::{Node, NostrSignaling};

fn main() -> std::io::Result<()> {
    let mut node = Node::builder().signaling(NostrSignaling::default()).bind("[::]:0")?;
    node.join_room("my-app", "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4");
    Ok(())
}
```

```gdscript
peer.host_room("my-game", "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4")   # the host
peer.join_room("my-game", "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4")   # each player
```

Members find each other by announcing a hello on the room's topic over a signaling carrier, then swap sealed offers
and dial each other. A Rust peer and a browser in the same room and on the same relays can link: both derive the
same topics and keys.

### The room name is the password

> [!IMPORTANT]
> Anyone who knows a room's name can join it. Relays see only a hash of the name, but it's an unsalted hash, so
> anyone can test guesses against it offline. **For a private room, use at least 128 random bits**: 32 hex digits,
> or `randomRoomId()` in JavaScript (22 base64url characters). A name someone could guess, like `lobby`, makes a
> public lobby.

The name protects the room in three places:

- **Hellos** carry a proof (an HMAC) under a key derived from the name, and are sealed with another key derived from
  it. A peer that doesn't know the name can't read who's in the room, and its hellos are dropped.
- **Offers** carry the same proof, so a peer answers only members, and gives its addresses only to them.
- **Links** that don't prove membership are closed: in Rust while a node is in a room, and in JavaScript within
  10 s, before any presence or message.

Every member is trusted alike. There's no way to let someone in once and lock them out later, other than moving the
room to a new name. [Security model](../concepts/security-model.md) has the full picture.

### Mesh or star

By default every member links with every other one: a full mesh. That suits up to a dozen or so members; each one
sends its own copy of everything to each of the others.

In Rust and Godot, a room can also be a star: one host, many clients that link only with the host.

| Role | Links with | Use |
|---|---|---|
| `Role::Peer` (`join_room`) | every peer and host | chat, whiteboards, small games |
| `Role::Host` (`join_room_as`, Godot `host_room`) | every client and peer | a game host |
| `Role::Client` (`join_room_as`, Godot `join_room`) | hosts only | a game client |

A role is proved the same way as membership, so only someone who knows the room name can act as its host. If clients
already know the host's key, pin it with `join_room_with(app, room, Role::Client, Some(host))` (Godot
`join_room_host`): then no other member can pose as the host.

JavaScript rooms are full meshes. For larger games, see [scaling](../concepts/scaling.md).

### Leaving

`room.leave()` in JavaScript tells the others at once (they see `peer-leave`). In Rust, `node.leave_room()` stops
announcing and dialling; links already up stay until you `disconnect` them or the node is dropped.

## Signaling carriers

The carrier only has to deliver small published messages to subscribers of a topic. It never sees addresses or
messages, and it doesn't have to be trusted.

| Carrier | Where | Reaches |
|---|---|---|
| Nostr relays | JS `signaling: 'nostr'`; Rust `NostrSignaling` (feature `nostr`); Godot by default | anyone on the internet. Defaults: `wss://relay.primal.net`, `wss://relay.nostr.net`, `wss://nostr.oxtr.dev` |
| BroadcastChannel | JS `signaling: 'local'` (the default) | tabs of one browser, or one Node process and its workers |
| LAN multicast | Rust `LanSignaling` | the local network, with no internet: see [LAN discovery](lan-discovery.md) |
| In-process | Rust `MemoryHub` | nodes in one process, for tests |
| Your own | JS: a factory function; Rust: implement the `Signaling` trait | whatever you have: a message bus, a game platform's lobby |

To use your own relays, pass them: `relays: ['wss://…']` in JavaScript, `NostrSignaling::new(["wss://…"])` in Rust,
`set_signaling_relays([...])` in Godot. The repository has a small relay for tests and LANs:
`cargo run -p peerwire --features nostr --example nostr_relay -- 127.0.0.1:7447`.

## Invites

An invite connects two peers directly, with no room. In Rust:

```rust
use peerwire::Node;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = Node::bind("[::]:0")?;
    let invite = host.invite(); // "pw:…": share it as a link, a QR code or a message

    let mut guest = Node::bind("[::]:0")?;
    let host_id = guest.connect(&invite)?; // dials the addresses in the invite
    println!("dialling {host_id}");
    Ok(())
}
```

In Godot, `peer.host()` returns the invite and `peer.join(invite)` dials it.

An invite holds the host's public key, the addresses to try, and an **invite key**. The key is what makes the host
dial back: `connect` also sends the host a sealed offer, proved with the invite key, over the signaling carrier if
there is one, so both sides dial at the same moment and punch through their NATs. Someone who knows only the host's
PeerId gets no answer and no address. Without a carrier, an invite works where the host can be reached directly: on a
LAN, or with a public address or a forwarded port.

Whoever answers the dial must prove they hold the key in the invite, so an invite can't be hijacked by anyone who
copies its addresses. [Protocol](../concepts/protocol.md#invites) has the format.

## Which one to use

| You want | Use |
|---|---|
| A lobby or a channel anyone in on it can join | a room with a guessable name |
| A private session for people you send a link to | a room with a random name (`randomRoomId()`) |
| A game with a host | a room with roles (Rust, Godot), or an invite to the host |
| To connect two specific devices you control | an invite |
| No internet at all | a room over `LanSignaling`, or an invite on the LAN |
