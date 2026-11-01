# LAN discovery

Peers on the same local network can find each other with no internet at all: no relays, no STUN. `LanSignaling` is a
signaling carrier over UDP multicast. Give it to a node, join a room, and every node on the network in that room
links with you directly. It works the same on a PC, in a container and on an ESP32.

```rust
use peerwire::{LanSignaling, Node};

fn main() -> std::io::Result<()> {
    let mut node = Node::builder().signaling(LanSignaling::new()?).bind("0.0.0.0:0")?;
    node.join_room("my-app", "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4");
    // poll the node as usual: see the quick start
    Ok(())
}
```

Try it with the bundled chat, on two or more machines on one Wi-Fi (or in two terminals on one machine):

```sh
cargo run -p peerwire --example lan_chat -- 9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4
```

Each copy prints `+ <peer>` when it links with another; then type a line to send it to everyone in the room.

## How it works

- Each signaling message goes out as **one datagram** to the multicast group `239.255.80.87:20567`, with a TTL of 1,
  so it never leaves the local network. Every carrier on the network hears it, including other processes on the
  same machine, and keeps only the topics it subscribed to.
- From there, rooms work exactly as they do over Nostr: sealed hellos, sealed offers, simultaneous dials. Peers link
  over their LAN addresses, so there's nothing to punch.
- A message is at most 1,356 bytes (`LanSignaling::MAX_PAYLOAD`), so nothing is fragmented: ESP-IDF's network stack
  doesn't reassemble IP fragments.

To use another group or a particular network interface, use `LanSignaling::on(group, interface)`, for example
`LanSignaling::on(LanSignaling::GROUP, "192.168.1.20".parse()?)`.

## Requirements

- **The firewall must let in UDP port 20567** (or your group's port). Without that, a node hears nobody.
- **The network must pass multicast between clients.** Home networks do. Some guest and public Wi-Fi networks drop
  multicast or keep clients apart; there a node finds nobody, and you need invites or Nostr instead.
- **Browsers can't send UDP multicast.** A browser on the same network finds peers through invites, a
  BroadcastChannel (tabs of one browser) or Nostr. A Rust node and a browser can still share a room if both are on
  the same Nostr relays.

## Security

The carrier is as untrusted as a public relay. Hellos and offers are sealed and proved under keys derived from the
room name, so a neighbour on the network can't read them or forge them. What a neighbour **does** learn:

- that a machine runs Peerwire at all, from its multicast group membership and its traffic on port 20567;
- the room's topic (a hash of the app id and room name) and when members announce themselves; a guessable room name
  can be tested against that hash and then joined, so **use a room name nobody can guess**;
- the topic of each peer that's offered a link, which is a stable hash of its PeerId: a neighbour could recognise the
  same device on another day;
- source IP and MAC addresses, message sizes and timing.

A replayed hello only makes members offer again, within their budgets. The socket is bound to the multicast group,
so unicast datagrams sent straight to the port from off the network aren't taken (except on Windows, which can't bind
a multicast address). Junk sent to a member costs it a bounded amount of work: opening incoming offers has its own
budget, spent before the costly decryption.

One open issue: a neighbour who floods two members' inbox topics (where their offers arrive) with junk can keep them
from linking for as long as it keeps going (NB-065, low severity). It needs both members' PeerIds in advance. The
[threat model](../project/threat-model.md) lists the findings for LAN discovery and their status.

## On ESP32

The [ESP32 prototype](esp32.md) uses `LanSignaling` with no hard-coded peers: each chip binds its own address, joins
a room and links with whoever answers. In Espressif's emulator, 8 chips linked with each other: 28 of 28 links, 26 of
them within 3 s of joining and the last at about 6 s, on a later hello.
