# Bluetooth

> [!NOTE]
> This is the design of Peerwire over Bluetooth Low Energy (NB-110), the contract that the Rust host on Linux, the
> ESP32 port and the browser build to. The framing codec is in `peerwire-core` (`ble.rs`); the Linux host is the
> `peerwire` feature `ble`. Nothing here has run over a real radio yet.

Peers near each other link over Bluetooth LE when they share no network: a laptop and an ESP32 board with the Wi-Fi
down, say. A Bluetooth connection is one more path for a link, beside its UDP paths: the same identities, Noise
handshake, packets and lanes go over it, and the link moves to UDP when that works. Rooms find their members over the
radio too, so no network is needed at all.

| Platform | Connects out | Is found and accepted | Notes |
|---|---|---|---|
| Rust on Linux (BlueZ) | yes | yes | feature `ble` |
| ESP32-C3 and ESP32 (NimBLE) | yes | yes | NB-110c; three connections at most |
| Chrome and Edge, Android Chrome | yes, after the user picks the device | no | NB-110d; central only |
| Firefox, Safari, every iPhone browser | no | no | no Web Bluetooth |

So a browser links only with a native peer nearby, never with another browser. Throughput is about 10 to 100 KB/s,
and range 10 to 30 m (**theoretical**, from the radio's specifications): enough for chat, game state and shared
documents, not for calls.

## The GATT service

Peers use LE GATT alone, since that is all Web Bluetooth offers. A peer that accepts connections serves one primary
service with two characteristics:

| | UUID | Properties | Carries |
|---|---|---|---|
| Service | `1c3eea41-c2be-493c-ab0b-7707ad2dbed6` | primary | |
| `rx` | `1c3eea42-c2be-493c-ab0b-7707ad2dbed6` | write without response | central to peripheral |
| `tx` | `1c3eea43-c2be-493c-ab0b-7707ad2dbed6` | notify | peripheral to central |

Neither needs pairing or bonding, and neither may ask for it: Noise IK already authenticates both ends, and pairing
would add operating-system prompts and a lasting bond that identifies the device. The central subscribes to `tx`,
then each side writes [chunks](#framing) to its own characteristic. A central asks for the largest ATT MTU its stack
allows (517); each value carries at most ATT_MTU − 3 bytes, at least 20.

## Advertising

A peer in a room advertises connectable, as `ADV_IND`, with one AD structure besides the flags: **service data**
under the 128-bit service UUID (type `0x21`), 9 bytes:

```text
ver   u8      1
token 4 B     random, new every 15 minutes and on each join or leave
tag   4 B     the first 4 bytes of HMAC-SHA256(ble key, "pw-ble-tag" || token)
```

That is 30 of the 31 bytes a legacy advert holds. The ble key is `sha256("peerwire/room-ble/" + app + "/" + room)`,
a fourth label over the room's name beside the topic, the room key and the hello key
([NAT traversal](nat-traversal.md#messages)). A scanner keeps the adverts whose tag matches its own room for their
token, and ignores the rest without connecting. The advert carries no `PeerId`, name or other lasting value: tokens
that change together with the stack's random address can't be linked by a listener without the room's name. A peer
not in a room doesn't advertise or scan.

**Who connects.** Native peers both advertise and scan. Of two peers that see each other's adverts, the one whose
token is the smaller connects, as the central; with equal tokens neither does until one changes. A peer with no
advert of its own (a browser) connects to any advert it picks. So a native peer accepts a central that never
advertises. A peer doesn't connect twice to one token, nor again within 60 s to a token whose connection closed.

## Framing

A datagram is up to 1,232 bytes, a GATT value as little as 20, so each message is split into chunks, one per GATT
value. Each chunk is a one-byte header and up to ATT_MTU − 4 bytes of the message:

```text
header u8   bit 7: the message's first chunk   bit 6: its last   bits 0-5: chunk counter, mod 64
```

The counter starts at 0 on each connection, in each direction, and goes up by one for every chunk sent. GATT keeps
the order but may drop a value when a buffer is full, so the receiver checks the counter: a gap, a chunk that isn't a
first one with no message open, a first one while a message is open, or a message longer than 1,233 bytes drops the
message it was assembling (and the chunk, unless it is a first one, which starts the next). Lost messages are lost
as UDP datagrams are; the lanes resend what needs it. Each connection holds at most one partial message per
direction. A chunk with no payload is invalid and dropped.

## Messages

A message's first byte says what it is:

| Kind | Rest of the message | Size |
|---|---|---|
| `0` datagram | one Peerwire packet ([Protocol](protocol.md#packets)) | 1 to 1,232 B |
| `1` room signal | a sealed hello for the room the receiver is in, as on the room topic | |
| `2` peer signal | a message sealed to the receiver, such as an offer, as on its peer topic | |

A message of another kind is dropped. Signals carry no topic: the receiver knows its room and its own inbox, so a
listener on the radio never sees the hash of a `PeerId`. A room signal goes on every connection. A peer signal goes
on the connection bound to the peer it is for, if one is, and otherwise on every connection not bound to another
peer. A receiver opens a room signal only in a room, and checks both kinds exactly as it would from any carrier.

## Binding a connection to its peer

A connection starts **unbound**. It is **bound** to a peer by the first datagram on it that authenticates that peer
as fresh and leaves the peer's link on this connection: an HS2 that finishes a dial over it, or a data packet with a
new nonce. A replayed HS1 can't bind one, as Noise IK's first message isn't fresh. From then on, until it closes,
only that peer's packets are taken from it, as a WebRTC channel's are (`Endpoint::handle_datagram_from`): a packet
for another peer's link, or a handshake from anyone else, is dropped. So a device nearby can't move one peer's link
onto its own connection. A peer has at most one bound connection: when a second is bound to it, the older closes.

## Paths: Bluetooth after UDP

A link's Bluetooth connection is its **slow address** (`Endpoint::set_slow`), which ranks below every other path.

- **Dialling.** A dial to a peer whose hello or offer came over a connection also dials over it, beside its UDP
  candidates. A connection is never offered or told to a peer as an address.
- **Up on Bluetooth.** A link that comes up on Bluetooth punches to the peer's UDP candidates, as a relayed link
  does, and moves to the first that answers (`PathChanged`). It is never raced: a faster Bluetooth round trip never
  takes a link off a UDP path.
- **Falling back.** A link off Bluetooth keeps the connection as its slow address. When its UDP path carries nothing
  for `direct_timeout` (15 s) and no relay is linked, it moves back to Bluetooth, and punches again.
- **No flapping.** A link moves to Bluetooth only after 15 s of silence on UDP, and off it only on an authenticated
  answer over UDP: one round at most every 15 s, however the Wi-Fi comes and goes.
- **Policy.** With a [`Policy`](../reference/rust.md), `PathKind::Bluetooth` in its order allows the radio, which
  always ranks last wherever it is listed. Under `Privacy::RelayOnly` no link goes over Bluetooth. `PublicOnly`
  allows it: no IP address goes over the radio.
- **What the app sees.** A link on Bluetooth reports `Path::Direct` at an address in `0.0.0.0/8` with port 0, which no
  network uses, one per connection; `Node::path_kind` says `PathKind::Bluetooth`.

A link that came up on UDP and never used Bluetooth has no slow address. If its UDP path dies, it closes when the
link times out, and the next hello over the radio dials the peer again over Bluetooth.

## Limits

| | Linux | ESP32 |
|---|---|---|
| Connections | 8 | 3 |
| Unbound connections | 4; more are refused at once | 2 |
| Signals taken per connection | a burst of 16, then one per 500 ms; the rest dropped before any cryptography | same |
| Partial messages per connection | one per direction, at most 1,233 B | same |

An unbound connection closes after 10 s without a signal that opens and checks (a hello proving the room, or an offer
to this peer), and after 60 s without one since.

## What a listener on the radio sees

- **Adverts:** that a Peerwire peer is near, with a token and tag that change every 15 minutes. With the room's name it
  can tell a member of that room. The device's Bluetooth address can link adverts across tokens unless the stack
  changes it too: BlueZ does with `Privacy=device` in `main.conf`, NimBLE with a resolvable private address, and
  phones always do.
- **Connections:** the link layer isn't encrypted, as nothing is paired, so a sniffer that follows a connection sees
  sealed hellos, sealed offers and Noise packets: their sizes and timing, not their contents or a lasting identifier.
- **What it can do:** jam the radio, or refuse to carry what it should, as an on-path attacker on any network can.

## Platforms

- **Linux:** BlueZ over D-Bus (`bluer`, with `libdbus` built from source). It serves the GATT service, advertises,
  scans and connects as a central, on one adapter. Ordinary users need access to the `bluetoothd` D-Bus API, which most
  desktop distributions grant to the logged-in user.
- **ESP32 (NB-110c):** NimBLE, three connections, an ATT MTU of 256. A board without Wi-Fi has no SNTP: it keeps the
  time it last set in its RTC across restarts but not power cuts, and a board whose clock was never set can't join a
  room, as room hellos must be within a minute of the receiver's clock (NB-093).
- **Browsers (NB-110d):** a central only. Each connection starts from a user gesture and a device picker, filtered on
  the service data's UUID, and the page's origin gets the permission. The page then signals and sends datagrams over
  the connection as above.

## Not yet

- L2CAP connection-oriented channels, which are faster but missing from Web Bluetooth.
- A spare connection between peers already linked over UDP, so that their first move to Bluetooth needs no new dial.
- Bluetooth on macOS and Windows, and invites that name a Bluetooth peer.
