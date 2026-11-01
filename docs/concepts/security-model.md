# Security model

> [!WARNING]
> Peerwire is pre-1.0 and hasn't had an external audit. Every change goes through an internal security review, and
> the findings are public in the [threat model](../project/threat-model.md), but don't rely on it yet to protect
> high-value data.

## What Peerwire guarantees

- **You talk to who you dialled.** A `PeerId` is an Ed25519 public key, and every link is a Noise IK handshake
  against the X25519 form of that key. A completed handshake proves the other side holds the secret for the PeerId
  you dialled. No signaling relay, peer relay or NAT in the path can pose as a peer or sit in the middle of a link.
- **Nobody in the path reads or changes your traffic.** After the handshake, every packet is sealed with
  ChaCha20-Poly1305 under the session keys. A peer that relays your link sees only ciphertext.
- **Replayed packets are dropped.** Each link has a 2,048-packet sliding nonce window, as in WireGuard.
- **Signaling can't read or forge what matters.** Offers, which carry IP addresses, are sealed to their recipient's
  key and authenticate the sender. Room hellos are sealed under a key derived from the room name. A tampered or
  forged message fails to open.
- **Only members get in.** Hellos, offers and links in a room carry proofs under a key derived from the room name.
  Peers that can't prove it get no answer, none of your addresses, and no link.

## Who can see what

| Party | Can see | Can't |
|---|---|---|
| A signaling relay, or anyone watching it | room and inbox topics (hashes), sealed hellos and offers (their length and timing), and the IP address each client connects to it from | read hellos, offers or addresses; join a room or pose as its host without the name; forge a link |
| Someone who knows a PeerId, but not its invite | the PeerId | make the node dial back, or learn its addresses |
| A peer relaying a link | that A and B talk, both their IP addresses, packet sizes and timing | read, change or inject the link's traffic |
| An on-path attacker | ciphertext, sizes and timing | impersonate a PeerId; replay data packets |
| A room member, or someone holding an invite | candidate addresses, as the [privacy mode](../guides/privacy.md) allows, and the source address of a direct link | pose as another PeerId |
| A neighbour on the LAN (with [LAN discovery](../guides/lan-discovery.md)) | that a host runs Peerwire, room and inbox topics, source IP and MAC addresses, sizes and timing | read or forge hellos and offers |

## Trust boundaries

| Boundary | Trusted? | |
|---|---|---|
| Signaling relays (Nostr, BroadcastChannel, LAN multicast) | No | May read, drop, reorder, replay or inject any signaling message, but can't forge a sealed one or pose as a PeerId. |
| Peer relays | No | See only ciphertext; relaying is rate-limited and goes only to peers already linked. |
| NATs and the network | No | Can drop or delay packets; can't get into a link. |
| Other peers | Partly | Authenticated, but may be malicious: send malformed frames, flood or probe. Bounds-checked parsers, budgets and caps are the defence. |
| Your app, or the C ABI's caller | Yes | The C ABI trusts its caller to pass valid pointers and lengths. |

Out of scope: a compromised operating system or random number generator, side channels in the crypto crates,
traffic analysis by a global passive adversary, and malicious code in the app itself.

## What it doesn't protect yet

- **Metadata from people who know the room name.** Anyone who knows a room's name can open its hellos and see its
  members' PeerIds. Anyone who knows a PeerId can watch its inbox topic for offers. Treat room names as secrets, and
  don't publish PeerIds you want kept private.
- **Recognising a device.** A PeerId that stays the same (which apps usually want) lets relays and LAN neighbours
  recognise the same device on another day or network, by its inbox topic.
- **Connection-setup floods from many real addresses.** Handshake cookies stop floods from spoofed addresses, and each
  IPv4 address or IPv6 /64 can hold at most 4 half-open handshakes, but an attacker with 256 such addresses can still
  fill the part of the half-open table above the cookie mark.
- **Key rotation within a session.** Session keys are only replaced by a new handshake, on reconnect.

## Using it safely

- **Use a random room name for anything private**: 128 bits or more, such as `randomRoomId()`. A guessable name is a
  public lobby, and an unsalted hash of it is visible to relays.
- **Pin the host** when clients know its key (`join_room_with(.., Some(host))`, Godot `join_room_host`), so no other
  member can claim the host role.
- **Pick a privacy mode** if your users shouldn't see each other's IP addresses: see [privacy](../guides/privacy.md).
- **Treat what members send as untrusted input.** They're authenticated, not trusted: check the shape of JSON before
  you render it, and escape text before you print it.
- **Keep identities secret.** A persisted identity (`room.endpoint.secret()`, `Identity`) is the device's key: store
  it as you'd store a password.

## Reporting a vulnerability

Please report it privately, as [SECURITY.md](../../SECURITY.md) describes, rather than in a public issue. The
[threat model](../project/threat-model.md) lists every finding so far, with its severity, fix and status.
