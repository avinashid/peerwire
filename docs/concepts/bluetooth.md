# Bluetooth

> [!NOTE]
> This is the design of Peerwire over Bluetooth Low Energy (NB-110), the contract that the Rust host on Linux, the
> ESP32 port and the browser build to, with security's requirements (BLE-R1 to R15 on NB-110b) folded in. The framing
> codec is in `peerwire-core` (`ble.rs`); the node's side is the `peerwire` feature `ble`, and the Linux driver
> `ble-bluez`. Nothing here has run over a real radio yet.

Peers near each other link over Bluetooth LE when they share no network: a laptop and an ESP32 board with the Wi-Fi
down, say. A Bluetooth connection is one more path for a link, beside its UDP paths: the same identities, Noise
handshake, packets and lanes go over it, and the link moves to UDP when that works. Rooms find their members over the
radio too, so no network is needed at all. It is opt-in: a node uses the radio only when the app gives it one.

| Platform | Connects out | Is found and accepted | Notes |
|---|---|---|---|
| Rust on Linux (BlueZ) | yes | yes, one central at a time | feature `ble-bluez` |
| ESP32-C3 and ESP32 (NimBLE) | yes | yes | NB-110c; three connections at most |
| Chrome and Edge, Android Chrome | yes, after the user picks the device | no | NB-110d; central only |
| Firefox, Safari, every iPhone browser | no | no | no Web Bluetooth |

So a browser links only with a native peer nearby, never with another browser. Throughput is about 10 to 100 KB/s,
and range 10 to 30 m (**theoretical**, from the radio's specifications): enough for chat, game state and shared
documents, not for calls. A link over Bluetooth proves nothing about distance (a relay can stretch the range), so
nothing may treat a peer linked over it as near or trusted.

## The GATT service

Peers use LE GATT alone, since that is all Web Bluetooth offers. A peer that accepts connections serves one primary
service with two characteristics, and nothing else of its own: no readable characteristic, no device information.

| | UUID | Properties | Carries |
|---|---|---|---|
| Service | `1c3eea41-c2be-493c-ab0b-7707ad2dbed6` | primary | |
| `rx` | `1c3eea42-c2be-493c-ab0b-7707ad2dbed6` | write without response | central to peripheral |
| `tx` | `1c3eea43-c2be-493c-ab0b-7707ad2dbed6` | notify | peripheral to central |

- **No pairing or bonding.** Noise IK authenticates both ends and gives confidentiality, integrity and replay
  protection over any path. No characteristic needs encryption, so no system shows a pairing prompt; a peer never
  asks for pairing and keeps no bonds, and [Platforms](#platforms) says what happens when a device pairs anyway.
  (Just Works, the only pairing a board with no screen can do, wouldn't stop a man in the middle anyway.)
- **Writes.** `rx` takes plain writes without response at offset 0; a write request, a prepared (long) write or a
  read is refused. Notifications on `tx` go to one connection only.
- **MTU.** A central asks for the largest ATT MTU its stack allows (517). Each value carries at most ATT_MTU − 3
  bytes and 512 (GATT's longest value), at least 20. A sender may use smaller values (Web Bluetooth doesn't say the
  MTU), and a receiver takes any size.
- **Names.** The GAP device name is `Peerwire`, never the host's name.

## Advertising

A peer in a room advertises connectable, as `ADV_IND`, from a random non-resolvable private address, with one AD
structure besides the flags: **service data** under the 128-bit service UUID (type `0x21`), 9 bytes:

```text
flags u8      bit 0: full (takes no central now); bits 1-7: 0
n     4 B     random
tag   4 B     the first 4 bytes of HMAC-SHA256(ble key, "pw-ble-adv" || n)
```

That is 30 of the 31 bytes a legacy advert holds. Where the stack allows a scan response, it lists the service UUID
(type `0x07`) for scanners that filter on service UUIDs, such as Web Bluetooth's device picker.

- **The ble key** is `sha256("peerwire/room-ble/" + app + "/" + room)`, a fourth label over the room's name beside
  the topic, the room key and the hello key ([NAT traversal](nat-traversal.md#messages)). It is never derived from
  the topic, which relays and the LAN see, and it reaches no other carrier.
- **Matching.** A scanner keeps the adverts whose tag matches its room for their `n`, and ignores the rest without
  connecting. It scans passively, takes at most 50 adverts a second, and skips one it has seen from the same address
  with the same bytes. A false match is 2^-32 per advert (**theoretical**).
- **Rotation.** The address, `n` and the tag change together, at least every 15 minutes and at each join or leave. A
  peer not in a room doesn't advertise or scan. Nothing in the advert lasts longer: no `PeerId`, name, topic or
  address. A listener with the room's name can tell a member of that room, as it could from a Nostr relay.

**Who connects.** Native peers both advertise and scan. A peer connects, as the central, to an advert that isn't full
when its own `n` is the smaller, or when its own advert is full; with equal values neither does until one changes. A
peer with no advert of its own (a browser) connects to the device the user picks, and a native peer whose advert isn't
on the air (its stack refused it, or its adapter went, and its driver says so) to any advert of the room that isn't
full, whatever its `n`: else two peers could each wait for the other's advert (NB-112). So a native peer accepts a
central that never advertises. A peer doesn't connect to an `n` it is connected or connecting to, nor within 60 s to an
`n` from a device whose connection closed or failed. A connect not up within 12 s counts as failed (a driver fails its
own within 10 s and drops the link), and one that comes up after that is closed: so a device that takes the link and
stalls its setup holds a connect 12 s at most, and then not again for 60 s. The hold is that device's alone, so a copy
of a member's advert, which anyone in range can replay, holds only the copy, not the member's own. But one whose
connection closed before it sent a challenge (it refused the central, or never challenged) is tried again after 1 to
4 s, at random, so a device that connects on a cycle of its own can't keep a newcomer landing in its grace (see the
limits). Nothing a peer says of its own advert is taken, as a member could name another's to keep two members apart: so
two peers may make two connections (a full desktop connects back to the peer that filled it, say), and the [second one
closes](#binding-a-connection-to-its-peer).

## Framing

A message is up to 1,233 bytes (a kind byte and a datagram), a GATT value as little as 20, so each message is split
into chunks, one per value. Each chunk is a one-byte header and up to ATT_MTU − 4 bytes of the message (511 at most):

```text
header u8   bit 7: the message's first chunk   bit 6: its last   bits 0-5: chunk counter, mod 64
```

The counter starts at 0 on each connection, in each direction, and goes up by one for every chunk sent. GATT keeps
the order but may drop a value when a buffer is full, so the receiver checks the counter. It drops the message it was
assembling on a gap or a repeat, on a first chunk while a message is open (its end was lost), when the message would
pass 1,233 bytes, when its last chunk came more than 2 s ago, and on a chunk with no payload. The chunk goes with it
unless it is a first one, which starts the next; a chunk that isn't a first one with no message open is dropped. Each
connection holds at most one partial message per direction, in one buffer. Lost messages are lost as UDP datagrams
are; the lanes resend what needs it.

Only a run of exactly 64 lost chunks (or a multiple) within 2 s between two that arrive goes unseen, and joins two
messages' parts: Peerwire packets and sealed signals are authenticated, so such a message fails its checks.
`peerwire_core::ble` has the codec (`Chunker`, `Reassembler`).

## Messages

A message's first byte is its kind:

| Kind | Rest of the message | When |
|---|---|---|
| `3` challenge | `c_p`, 16 random bytes | the peripheral's first message |
| `4` answer | `c_c`, 16 random bytes; `HMAC-SHA256(ble key, "pw-ble-c" ‖ c_p ‖ c_c)` | the central's reply |
| `5` proof | `HMAC-SHA256(ble key, "pw-ble-p" ‖ c_c ‖ c_p)` | the peripheral's reply to a good answer |
| `1` signal | `seq` u64 LE, then AES-256-GCM(link key, nonce, `inner kind ‖ payload`) | once proven |
| `0` datagram | one Peerwire packet ([Protocol](protocol.md#packets)), 1 to 1,232 B | once proven |

**The room proof.** On a new connection the peripheral sends a challenge, and the central answers it. A wrong answer
closes the connection; a good one is answered with the proof, which the central checks in turn. So each end knows the
other holds the room's ble key, freshly, with no clock, and a peripheral sends nothing a non-member could use. Until
then a connection carries only these three messages; anything else closes it. The link key is
`HMAC-SHA256(ble key, "pw-ble-k" ‖ c_p ‖ c_c)`.

**The identity proof.** Then each end proves who it is, on this connection: its first signal is of inner kind 3, its
`PeerId` (32 bytes) and its Ed25519 signature over `"pw-ble-id" ‖ c_p ‖ c_c` (64 bytes). The receiver checks it, as
the radio's budget allows ([Limits](#limits)), and from then on knows the peer at the other end. A proof that fails
(made for another connection, say), one in the receiver's own name, or none within 3 s of the connection, closes the
connection; a second one is dropped and counted. Only an identity proven this way, or by a session bound to the
connection (below), counts for anything: who a connection is dialled for, the one connection kept per peer, and where a
peer signal goes. A hello or an offer proves who sent it, as on any carrier, never who is at the other end of the
connection it came over: a member could pass on another's. A relay that passes every byte between two connections,
and so gives them the same challenges, still joins them, as it could over any carrier: a link over Bluetooth proves
nothing about distance.

**Signals.** Hellos and offers travel sealed under the link key: the GCM nonce is 4 bytes of direction (0 from the
central, 1 from the peripheral) and the 8 bytes of `seq`, which goes up with each signal sent, and a receiver takes only
a `seq` above the last it took. The inner kind is 1 for a room hello, as on the room topic, or 2 for a message sealed to
the receiver, such as an offer, as on its peer topic; 3 is the identity proof. No topic goes over the air: the receiver
knows its room and its own inbox, so a listener never sees the hash of a `PeerId`, and only a member can tell a hello
from an offer. A room signal goes on every proven connection, a peer signal on the connection proven to reach the peer
it is for if there is one, and otherwise on every proven connection whose peer isn't known yet. A peer never passes a
signal from one connection on to another. When a connection proves the room, a room hello goes over it alone, so the
members it reaches hear of the peer at once, and the hellos on other carriers keep their schedule. Inside, every check
stands as it does on any carrier: the hello key, room proofs, freshness and the hello and offer budgets.

A message of another kind, or one that fails its checks, is dropped and counted against the connection.

## Binding a connection to its peer

A connection starts **unbound**, and once proven it takes only a handshake that opens a session on it (an HS1), the
answer to a dial over it (an HS2 or a cookie), and data for a session whose path is it; anything else is dropped and
counted. It is **bound** to a peer by the first packet on it that authenticates that peer as fresh in a session on
it: an HS2 that finishes a dial over it, or a data packet with a new nonce. An HS1 never binds one, as Noise IK's
first message can be replayed. From then on, until it closes, only that peer's packets are taken from it, as a WebRTC
channel's are: a packet for another peer's link, or a handshake from anyone else, is dropped. A connection binds once;
a new connection, even to the same device, starts unbound. The core does this for every host:
`Endpoint::handle_datagram_unbound` for an unbound connection, `handle_datagram_from` for a bound one.

A peer keeps one connection to each other peer. When one binds to a peer, it is the one, and every other proven to
reach that peer closes. Otherwise it is the first proven to reach it: both ends prove that one before a second is
made, so the second closes, at the end that made it, which doesn't connect to that advert again until it changes;
the other end leaves it to that one, doesn't count it as a central, and closes it for a new central that needs its
place. Two made at once may each be the first at the
end that made it: then, once the other end's has been proven for 3 s, the one whose central has the larger `PeerId`
closes, at the end that made it. A proven connection bound to no one, such as one between peers linked over UDP, stays
open while a signal proving a member comes over it at least every 60 s (hellos come every 30 s at most): it is ready
for the day UDP goes.

## Paths: UDP first

A Bluetooth hop is ranked as a relayed address is (`Address::relayed`): a last resort, which only a handshake on it
puts a link on.

- **Dialling.** A dial to a peer with no link also dials over the connection proven to reach it, beside its UDP
  candidates, and a member proven on a new connection while it has no link is offered to again at once. A
  connection is never offered, sent in an `OBSERVED` or relay frame, or decoded from one: its encoding is unique per
  connection, for the handshake cookie, and decodes to nothing.
- **Up on Bluetooth.** A link on Bluetooth punches to the peer's UDP candidates and moves to the first that answers,
  on an authenticated answer (`PathChanged`). No race, punch answer or late packet moves a link onto Bluetooth, and
  no peer relays across it.
- **UDP gone.** A link on UDP that goes silent closes when it times out (30 s). A member a radio connection reaches is
  then offered to again at once, over it, and the dial goes over Bluetooth. A move costs the link a reconnect.
- **UDP back.** A link on Bluetooth punches to the peer's UDP addresses when it comes up, and again every 15 s, and
  moves to the first that answers.
- **No flapping.** A link moves to Bluetooth only when its UDP link is gone, and off it only on an authenticated
  answer over UDP: at most one reconnect for each time the network goes, and one move back for each time it returns.
  Tested on simulated time: with UDP off 5 s at a time for 2 minutes, nothing moves; off 40 s and on 40 s for 4
  minutes, each side loses its link once and moves back to UDP once each time.
- **Policy.** Bluetooth is `PathKind::Bluetooth`, not in `Policy::default()`: with a policy, the radio carries links
  only if its order lists it, and it ranks last wherever it is listed. With neither `Direct` nor `Birthday` in the
  order, a link on Bluetooth stays there: it punches to nothing and answers none of the peer's punches
  (`Endpoint::set_keep_radio`), so a peer with another policy can't move it to UDP, only to have this node drop it
  there. With Bluetooth alone in the order the node leaves its UDP socket unused, both ways, so a peer's dials to it
  there get no answer, and the peer links over the radio. Under `Privacy::RelayOnly` no link goes over Bluetooth,
  which would announce the device within radio range; `PublicOnly` allows it, as no IP address goes over the radio.
- **What the app sees.** A link on Bluetooth reports `Path::Direct` at an address in `0.0.0.0/8` with port 0, which no
  network uses, one per connection; `Node::path_kind` says `PathKind::Bluetooth`.

## Limits

| | Linux | ESP32 |
|---|---|---|
| Connections | 8, one of them as the peripheral | 3 |
| Connections bound to no peer | 4; proven ones (standbys) leave one free: one that proves the room into the last closes the weakest other (unproven, then with no proven peer, then the oldest), so standbys can't lock newcomers out | 2 |
| Time to prove the room and the peer's identity | 3 s, then the connection closes | same |
| A connect of the peer's own | up within 12 s, or given up as a failed one; the driver fails it within 10 s | same |
| A central's connection whose peer hasn't proven who it is | makes way, after 1 s, for a new central over the limits; a second one to a peer, at once | same |
| A proven connection bound to no peer | open while a signal proves a member at least every 60 s | same |
| Distinct HS1s on an unbound connection | 1 per 10 s (a half-open handshake's life); its resends pass | same |
| Distinct HS1s on unbound connections, across the radio | 4 at once, then one per 250 ms | 2, then one per 5 s (a C3 answers one in 156 ms: about 3% of it) |
| Identity proofs checked, across the radio | 16 at once, then one per 62.5 ms | 8, then one per 1.25 s (four times the HS1s': a check costs about a quarter of what answering an HS1 does, **theoretical**) |
| HS1s on a bound connection | a burst of 4, then one per 5 s | same |
| Signals taken per connection | a burst of 16, then one per 500 ms; the rest dropped before any cryptography | same |
| Messages dropped or refused before the connection closes | 32 | same |
| Partial messages per connection | one per direction, at most 1,233 B | same |
| Values queued to send per connection | 64; more are dropped, never waited for | 16 |

A peer that holds all the connections or all the centrals it can marks its advert full and refuses another central,
counting no central whose peer hasn't proven who it is, nor a second one to a peer: those make way for a new central
(one whose peer hasn't proven who it is, once it has had 1 s), so an advert that says it takes a central refuses one
only within another's first second. A peer's own connects under way count against its own connects alone, never
against a central or a connection that proves the room, as one that never completes can't make way: a device that
stalls them can't keep centrals out. Linux takes one central at a time because BlueZ sends a notification to every
subscribed central (BLE-R8). The addresses naming connections (`Hop::Ble`) aren't
reused within a process before 2^24 (16.7 million) more connections have been made.

## What a listener on the radio sees

- **Adverts:** that a Peerwire peer is near, with an address, `n` and tag that change every 15 minutes, and whether it
  is full. With the room's name it can tell a member of that room.
- **Connections:** the link layer isn't encrypted, as nothing is paired, so a sniffer that follows a connection sees
  the challenge, the answer, the proof, sealed signals and Noise packets: their sizes and timing, not their contents
  or a lasting identifier.
- **What it can do:** jam the radio, cut a connection, or refuse to carry what it should, as an on-path attacker on
  any network can, and push a link onto Bluetooth by stopping UDP, which it could stop anyway.

## The radio driver

The node runs all of the above itself (adverts, who connects, the room proof, framing, binding, limits) over a small
driver for the radio, so each platform writes only the driver. In `peerwire` (feature `ble`, no dependencies):

```text
pub trait Radio: Send {
    /// Advertises with `data` as the service data, replacing the last; `None` stops. Serves the GATT service, and
    /// accepts centrals, while advertising. Each call with data comes from a new random address, and lists the
    /// service UUID (AD 0x07) in the scan response where the stack can set one. An advert that isn't on the air (the
    /// stack refused it, or the adapter went) is reported (`Advertising(false)`) and tried again until it is
    /// (`Advertising(true)`).
    fn advertise(&mut self, data: Option<&[u8]>);
    /// Starts or stops scanning, passively, for the service's adverts.
    fn scan(&mut self, on: bool);
    /// Connects to `device` (from `Seen`) as a central, subscribes to `tx`, and asks for the largest ATT MTU. A
    /// connect not set up within 10 s fails (`ConnectFailed`) and leaves no link up; the node gives one up after
    /// 12 s, and closes a connection that comes after that.
    fn connect(&mut self, device: DeviceId);
    /// Sends one value of at most ATT_MTU - 3 bytes on `conn`: a write without response to `rx` as the central, a
    /// notification on `tx` to that one central as the peripheral. `false` if it can't take it now (a full buffer):
    /// the node keeps it and tries again later.
    fn send(&mut self, conn: ConnId, value: &[u8]) -> bool;
    /// Closes `conn`.
    fn disconnect(&mut self, conn: ConnId);
    /// The next event, without blocking.
    fn poll(&mut self) -> Option<RadioEvent>;
    /// What it holds at once (connections, unbound ones, centrals, values queued): `Limits::DESKTOP` unless the
    /// driver says otherwise; an ESP32's says `Limits::SMALL`.
    fn limits(&self) -> Limits { Limits::DESKTOP }
    /// A waker to wake each time it queues an event, so a node asleep until its next timer polls at once; a driver
    /// that never wakes it still works, as a node with a radio polls at least every 50 ms. Default: ignored.
    fn set_waker(&mut self, waker: Waker) {}
}

pub enum RadioEvent {
    Seen { device: DeviceId, data: Vec<u8> },                       // an advert of the service, its service data
    Connected { conn: ConnId, device: Option<DeviceId>, mtu: u16 }, // ours to `device`, or a central's (`None`)
    ConnectFailed { device: DeviceId },
    Mtu { conn: ConnId, mtu: u16 },                                  // the ATT MTU changed
    Value { conn: ConnId, value: Vec<u8> },                          // a write to `rx`, or a notification on `tx`
    Disconnected { conn: ConnId },                                   // as soon as its link goes down
    Advertising(bool),                                               // whether the advert asked for is on the air
}
```

`DeviceId(u64)` and `ConnId(u32)` are the driver's own numbers. A driver reports `Disconnected` as soon as a link
goes down, unless the node closed it: till then the node takes it to be up, and doesn't connect to that advert again.
A driver that never sends `Advertising` is taken to advertise as asked. `Builder::bluetooth(radio)` gives a node its
radio; `Node::poll` drains its events, and `Node::set_waker` (which `AsyncNode` calls itself) hands it a waker.
Feature `ble-bluez` adds the Linux driver, `BlueZ`; the ESP32's NimBLE driver lives in `peerwire-esp` (NB-110c), and
tests use an in-process pair of fake radios.

## Platforms

- **Linux:** BlueZ over D-Bus (`bluer`, with `libdbus` built from source), on one adapter, on a thread of its own.
  The driver can't choose BlueZ's addresses, its GAP name or how it scans:
  - with `Privacy=device` in `/etc/bluetooth/main.conf` BlueZ advertises from a resolvable private address that
    changes every 15 minutes, and without it from the adapter's own, which tracks the machine (the driver can't
    tell which);
  - the GAP name is the adapter's alias, the host name unless set (`bluetoothctl system-alias Peerwire`):
    `BlueZ::alias` reports it, and `BlueZ::names_this_host` says when it is the host name, for the app to tell its
    user;
  - its discovery scans actively, so the radio sends scan requests from its address;
  - the driver scans for every LE advert, as a discovery filter on the service UUID crashes bluetoothd 5.87 (NB-112),
    and follows at most 256 devices at once: past that, only one advertising the service, in place of one that never
    has. Any device can put the service in its adverts, so one that has is never dropped for another, and one left
    out is picked up once there is room: the driver lists BlueZ's devices again, at most every 30 s. While a burst of
    such adverts holds all 256, a member that comes is reported once, then not followed until it ends;
  - it can't add a scan response, so Web Bluetooth's picker must filter on the service data (to be checked on a
    phone);
  - pairing is the system's: BlueZ answers a central's pairing request through the desktop's agent, so a device
    nearby may raise a pairing prompt. Nothing in Peerwire pairs or needs it: refuse such a prompt.
  - on BlueZ 5.87 without upstream commit `82af2beaf` (in no release yet), `bluetoothd` crashes when a scan with a UUID
    discovery filter meets an advert or scan response that lists a service UUID, and the host has no Bluetooth until it
    restarts. Before NB-112 this driver's scan set such a filter, for the service UUID, so a peer nearby whose advert
    or scan response listed that UUID set it off; any other program can set one too, so use a BlueZ with that commit (a
    later release, or a 5.87 package that carries it). 5.86 and earlier are unaffected.

  As the peripheral the driver serves the central whose subscription to `tx` BlueZ hands over, as a socket with its
  address (`AcquireNotify`), and takes writes from that device alone; another that writes meanwhile is disconnected.
  As the central it gives a connect 10 s to come up (the link, the service found, both sockets), and reports one that
  fails or doesn't as failed, then disconnects it, which also stops a connect BlueZ still has under way. BlueZ answers
  a disconnect only once the link is down, 2 s or more later, so the driver never waits on one: each runs on its own.
  A link that drops is reported closed at once: as the central, once BlueZ says the device isn't connected, or shuts
  the `tx` socket down, which it does as a link drops (a read then gives nothing, not an error).
  bluer's sockets take values of up to 5 bytes under the ATT MTU, so a connection at an ATT MTU of 23 or 24 (a phone
  browser that doesn't raise it, say; to be checked with NB-110d) can't carry the codec's 20-byte values and isn't
  used.

  The driver keeps the radio up (NB-112). A part that fails (finding the adapter, the GATT service, the advert, the
  scan) is logged through the `log` crate and tried again 1, 2, 4, 8, 16, then every 30 s, until it works, which is
  logged once; an advert that isn't on the air is reported (`Advertising`). When the adapter in use goes (a USB reset
  brings it back as `hci1`, say), stops answering (it is asked every 10 s, as a bluetoothd that crashed says nothing)
  or comes again (bluetoothd restarted, and forgot what was set up on it), or another becomes the default, its links
  are reported closed, connects under way fail, and the driver sets up on the default adapter: serving, advertising
  and scanning. One powered off has its links reported closed too, and is set up again once powered on. The driver
  powers an adapter on only at start (`BlueZ::new`), never one its user turned off; BlueZ's `AutoEnable`, on by
  default, powers on one it finds, as after a USB reset.

  The log names a device by a hash under a key drawn for each run, never by its address. At `info` the driver logs a
  connect that fails, with the step and BlueZ's error, and a link that goes down, with why. At `debug` the node adds
  each connection's life (made, refused, proven, closed and why, and how long its advert is held) and, once each 10 s
  for an advert, why it doesn't connect to one of its room: it has the larger `n`, the advert is full, a connection
  to it is open or under way, it is held, or the node has no room.

  Ordinary users need access to `bluetoothd` over D-Bus, which most desktop distributions give the logged-in user.
- **ESP32 (NB-110c):** NimBLE, three connections, an ATT MTU of 256 and a random non-resolvable address.
  - The security manager is compiled in, as esp32-nimble 0.13 calls `ble_sm_inject_io`, so a peer can still pair:
    a central that asks, or a peripheral the board connects to that sends a Security Request. The driver turns
    bonding off and asks for neither MITM protection nor Secure Connections, so a pairing runs as legacy Just Works.
    It hands over no keys either way (no IRK, no identity address), and the driver closes the connection once it
    completes. No bonds are stored. On the original ESP32 the driver turns NimBLE's host privacy off, so the address
    of each advert is the board's only one.
  - The prepared-write pool is empty (`BT_NIMBLE_ATT_MAX_PREP_ENTRIES=0`), so NimBLE itself refuses a long write
    before the driver sees it: on an ESP32-C3, a 300 B write fails with `rc=6`.
  - As the central it gives a connect 10 s in all (the link, the MTU, the service, subscribing to `tx`). A peer that
    asks for the MTU first, as BlueZ does as soon as the link is up, has done the exchange: NimBLE refuses a second.
  - An advert NimBLE won't set up or start is reported (`Advertising(false)`) and tried again 1, 2, 4, 8, 16, then
    every 30 s; one that falls off the air while there is room for a connection is put back within a second.
  - With Wi-Fi off, every random draw comes after the Bluetooth controller starts (`nimble_port_init`), as NB-063 has
    it for Wi-Fi.
  - A board with no Wi-Fi has no SNTP, and room hellos must be within a minute of the receiver's clock. So a board
    joins a room over Bluetooth only with a clock set since power-on (its RTC keeps it across restarts, not power
    cuts; NB-093).
- **Browsers (NB-110d):** a central only. Each connection starts from a user gesture and the device picker, filtered
  on the service UUID; never `acceptAllDevices`. The permission belongs to the page's origin. Nothing from the picker
  (`device.name`, `device.id`) goes into signaling or is trusted: identity comes from Noise alone. The page runs the
  room proof, the codec (from the wasm) and the binding rules as above.

## Try it

`ble_chat` (`examples/rust/ble_chat.rs`) joins a room over the radio and the LAN on Linux, and prints the path each
link takes (`Direct` over UDP, `Bluetooth` over the radio), again each time it moves. Run it on two machines nearby,
or beside an ESP32 in the same room (`room_chat`'s rooms, app `peerwire-lan-chat`):

```sh
cargo run -p peerwire --features ble-bluez --example ble_chat -- kitchen            # UDP first, the radio as well
cargo run -p peerwire --features ble-bluez --example ble_chat -- kitchen --no-udp   # Bluetooth only
RUST_LOG=debug cargo run -p peerwire --features ble-bluez --example ble_chat -- kitchen --no-udp   # and each connection
```

`--no-udp` gives the node no LAN carrier and the policy `[PathKind::Bluetooth]`, so its links stay on the radio even
where UDP works: the node leaves its UDP socket unused, and answers none of the peer's dials or punches there.
At start it prints the adapter's alias, which centrals read as its name, and warns when that is the host name.
Building it needs a C compiler for `libdbus` (built from source); running it, `bluetoothd` and a powered adapter.

## Not yet

- L2CAP connection-oriented channels, which are faster but missing from Web Bluetooth.
- Moving a link from UDP to Bluetooth without a reconnect, through a spare connection kept for it.
- Bluetooth on macOS and Windows, and invites that name a Bluetooth peer.
