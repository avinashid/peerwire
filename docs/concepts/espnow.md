# ESP-NOW

> [!NOTE]
> This is the design of Peerwire over ESP-NOW (NB-135): boards linking with each other over Espressif's ESP-NOW
> frames, with no Wi-Fi network and no Bluetooth. The connection logic is the `peerwire` feature `espnow`
> (`espnow::Engine`, sans-IO, with host and fuzz tests); the ESP32 driver is `peerwire-esp` feature `espnow`, and its
> `espnow_mesh` example links two boards. The protocol below has had its security review, in eight rounds (what it
> found is [below](#forged-frames)). The probe (`espnow_probe`, NB-135) measured the radio on two boards first
> (NB-138, [below](#limits)), and the defaults stand; the same two boards then ran a link for 5 minutes, once, from
> before the review's second round (NB-139).

Boards near each other link over ESP-NOW when they share no network: two ESP32 boards in a field, say, or a board
whose Wi-Fi network is down. ESP-NOW is a connectionless link layer of Espressif's: the chip's Wi-Fi radio sends a
frame of up to 250 bytes (1,470 with ESP-NOW v2 at both ends) to a MAC address, which the receiving radio
acknowledges, or to every radio on the channel, with no access point and no association. Peerwire makes a Bluetooth
radio of it (`ble::Radio`): adverts go out as broadcasts and connections are emulated over unicast frames, so
everything a node does over Bluetooth runs unchanged on top: the room advert and its proof, Noise IK, sealed signals
and datagrams, identity proofs, and the path's rank, below every UDP path. The path is of its own kind,
`PathKind::EspNow` (NB-142), as the radio names it (`ble::Radio::kind`): `Node::path_kind` says so, and a policy must
list it for links over ESP-NOW (`Policy::new([PathKind::EspNow])` for ESP-NOW alone); one that lists `Bluetooth`
alone keeps them off, and the node logs a warning.

| | ESP-NOW | Bluetooth LE (NimBLE on the same boards) |
|---|---|---|
| Who | ESP32 boards only: no PC, phone or browser speaks it | boards, PCs, Chrome |
| Frame | 250 B (v1), 1,470 B (v2 at both ends) | a GATT value of up to 253 B (ATT MTU 256) |
| Rate | 1 Mbit/s by default (802.11b), up to 54 Mbit/s and more set per peer | 1 Mbit/s |
| Connections | 4 by default; ESP-NOW's peer table holds 20 addresses | 3 |
| Radio | Wi-Fi's: the same channel at every end; Wi-Fi on | Bluetooth's: Wi-Fi may be off |

A node has one radio (`Builder::bluetooth`): Bluetooth or ESP-NOW, not both yet. A link over ESP-NOW proves nothing
about distance, nor who is in range, and nothing may treat a peer linked over it as near or trusted.

## Channel

ESP-NOW frames go on the channel the Wi-Fi radio is on, so every board in a room must be on the same one.

- **No network:** the board starts Wi-Fi as a station on no network on a fixed channel
  (`wifi::start_on_channel`). `espnow_mesh` takes the channel it hears wifi.sh's network on, so boards that join that
  network later stay on it, or channel 11 (`PW_ESPNOW_CHANNEL`).
- **On a network:** the radio follows the access point's channel, and ESP-NOW goes with it (peers are added for "the
  channel the station is on"). Boards on different networks reach each other only if the networks share a channel.
- **Scans:** a station that scans or joins hops across channels for a few seconds (joining an access point took 6.7 and
  7.3 s on the NB-138 boards), and frames sent to it meanwhile aren't acknowledged; the radio tries each one 4 times
  and a connection survives 8 lost in a row, so a short scan costs some frames, not the connection.
- **Interference:** ESP-NOW shares the channel with the networks on it; Bluetooth on the same chip time-shares the
  antenna with Wi-Fi (ESP-IDF's coexistence), which the probe's modes `c` and `c2` measure.

## Frames

Every frame starts with `PW`, the version (1) and its kind. Each kind has one exact length (a data frame's payload is
the rest), all numbers are little-endian, and an id is a connection's number at one end, 64 bits drawn at random by
that end from a ChaCha20 generator seeded from the system's random numbers; never 0.

| Kind | Bytes after the 4-byte header | Sent |
|---|---|---|
| 1 ADVERT | the service data (the room advert, 9 B; 1 to 31 B) | broadcast, every 500 ms ± 12.5% while advertising |
| 2 OPEN | opener's id (8), longest frame it takes (2, 250 to 1,470) | by the connecting end, every 500 ms until accepted, for 10 s |
| 3 ACCEPT | opener's id (8), accepter's id (8), longest frame the accepter takes (2) | in answer to an OPEN |
| 4 PROBE | the receiver's id (8) | after a CLOSE, to check it |
| 5 DATA | the receiver's id (8), sequence number (4), flags (1: first, last), the value's length (2, 1 to 512), payload | a value, or part of one |
| 6 KEEPALIVE | the receiver's id (8) | after 1 s with nothing sent on a connection, and in answer to a PROBE |
| 7 CLOSE | the receiver's id (8) | when the node closes a connection, and for an ACCEPT nobody waits for |

A frame that doesn't start with `PW` is another app's and is ignored; one of another version, an unknown kind, the
wrong length, an empty advert or payload, an id of 0, unknown flags, a frame size out of range, or a value's length of
0, over 512 or under the payload's, is dropped (`espnow::frame::parse`, the fuzz target `fuzz_frames`: whatever
parses encodes back to the same bytes). A frame from a group address is dropped too.

**The advert's tag.** The service data is the node's room advert ([Bluetooth](bluetooth.md#advertising)): flags,
`n`, and a tag only members can check. A board keeps its address for the boot, and anyone in range can copy an advert
and send it from another address at any instant, so over ESP-NOW the tag is bound to the address it is sent from:
the first 4 bytes of `HMAC-SHA256(ble key, "pw-ble-adv" ‖ n ‖ address)`, the address as 8 bytes, two zero bytes and
the MAC (`Radio::address`, `Ether::mac`). A node takes an advert of its room only from the address its tag names, so
a copy from any other is another room's, and a connect to a copy goes to the member's own radio (NB135-12).

**Sealed datagrams.** The values carry the node's Bluetooth framing and messages ([Bluetooth](bluetooth.md#messages)),
with one change, as anyone in range can forge a value: a datagram (kind 0) goes sealed under the connection's link key
as a signal is (NB135-9). It is kind 0, `seq` (u64 LE), then AES-256-GCM(link key, nonce, datagram), with the signal's
nonce (4 bytes of direction, 8 of `seq`) and `seq` from the same count as the signals', so no nonce repeats: 24 bytes
more than the datagram, and no new key. The receiver takes one only if it opens and its `seq` is above the last it
took (a signal's or a datagram's), and drops any other before the node sees it, without counting it towards the
connection's 32 drops: so a forged or replayed datagram takes neither the HS1 slot of a connection no peer is bound to
yet nor the radio's budget for handshakes, nor closes the connection. The radio asks for it (`Limits::forgeable`), and
both ends of an ESP-NOW connection seal (`ble::Link::seal_datagrams`); a Bluetooth connection doesn't, and its
datagrams go as they are. A message is then 1,257 B at most (a 1,232 B datagram), still three values of 512 bytes: 8
frames on v1 (7 before), 3 on v2. A datagram of up to 205 B fits one v1 frame (229 B before), and one of up to 486 B
one value (510 B before).

## Connections

The end that connects (the node's central) sends OPEN with its id. The other end, if it advertises, draws its own id,
answers ACCEPT and reports the connection to its node (`Connected` with no device, as a central's); the connecting end
reports it when the ACCEPT comes. At its 4 open connections it takes one more and leaves it to the node, as a
Bluetooth stack does: the node closes a connection that makes way for it, or closes the newcomer (NB135-11). Connects
of the node's own under way don't count there, as the node counts them itself and makes way among its open ones: they
did, and beside one the node took the one over with room to spare, closed none, and the radio stayed silent to every
OPEN after it (NB135-13). Past that one, and when not advertising, it stays silent until it holds 4 open again, which
the node's decision on the one over brings about: the node's advert already says when it is full, and a refusal would
be one more frame to forge. A connect of the node's own is never refused for the limits, as the node counts its own;
twice the limits in all is the backstop. A connection is then known at each end by the pair (the other end's MAC
address, this end's id), and every frame on it carries the receiver's id and must come from that address.

- **Frame size:** each end says the longest frame it takes; a connection uses the shorter (250 B unless both have
  ESP-NOW v2, and `PW_ESPNOW_V1` keeps a board to 250 B).
- **Values:** the node is told an ATT MTU of 515, so it sends values of up to 512 bytes; the radio splits each into
  data frames (three on v1, one on v2), numbered in order, the first and last flagged, each with the value's length.
  The receiver joins them, and drops a value that lost a part or whose parts don't add up to its length; the node's
  framing then drops the message the value was part of, and the reliable lane sends it again.
- **Numbers:** the receiver takes a data frame up to 64 numbers past the highest one it took, and one up to 64
  behind it that it hasn't taken yet, once: it keeps a bit for each of the 64 behind, as DTLS's and IPsec's
  anti-replay windows do (NB135-6). It drops one it took already (a copy: the sender's radio didn't hear the
  acknowledgement and sent it again). One further off, ahead or more than 64 behind, is dropped, unless 4 come in a
  row: then the receiver takes up the sender's numbers anew, whether the sender lost more than 64 frames or frames
  forged far ahead (or behind) moved the receiver there and the real ones are now far behind (or ahead) it (NB135-1),
  with none of the 64 behind its new place taken (NB135-7).
  A value's parts are joined in order, each the number after the last part taken; a value's first part starts a new
  one.
- **Liveness:** a connection with nothing sent on it for 1 s sends a KEEPALIVE; one with nothing heard on it for 5 s
  is gone (`Disconnected`), and so is one whose last 8 frames the radio couldn't deliver. A connect not accepted within
  10 s fails (`ConnectFailed`), as the node's radio contract asks; one whose first OPEN no radio acknowledged fails at
  once, as no radio is at that address to answer it, made up or gone (NB135-12), and so does one an ACCEPT opened
  before that verdict came, as one forged from that address can (`Disconnected`, NB135-16).
- **Closing:** the node closes a connection with a CLOSE, and hears nothing more of it. A CLOSE received isn't taken at
  its word: the receiver sends a PROBE and closes only if nothing comes on the connection within 500 ms. The other end
  answers a PROBE with a KEEPALIVE at once, if it still has the connection.
- **A second ACCEPT:** the opener takes the first ACCEPT. A second one with another id within 3 s of the first (the
  time a connection has to prove the room) means one of the two was forged, and which can't be told, whatever came on
  the connection meanwhile, as that may be forged too: the connection closes (NB135-4). The radio reports it as
  `Dropped`, not `Disconnected`, as it says nothing of the other end, so the node connects again 1 to 4 s later, as
  after a refusal, though the room's challenge has mostly come by then (NB135-5). Later, one is ignored.
- **Connections that haven't proven the room:** anyone in range can make one with an OPEN from a made-up address, and
  its central never proves the room. So the node (`Limits::forgeable`) counts a central's connection that hasn't
  proven the room against none of its own connects, and closes the oldest such one when a connect of its own needs the
  slot; and a new central takes the oldest's place at once, where over Bluetooth one makes way after 1 s (NB135-11).
  The oldest goes, not the newest: none has answered the challenge, which a member does in a round trip and is proven,
  so the oldest has gone longest without, and a member mid-proof outlives the next newcomer; it is closed only if 2
  more come within its round trip. A forger holds 2 such connections at most, the unbound ones, as before. Any of
  them makes way before a connection the node made that hasn't proven the room either (NB135-14): frames forged with
  the numbers of its room proof keep it from proving it, and two forged OPENs then made it make way.
- **Connecting again:** the end that connected holds the advert of a connection that closed before it connects to it
  again, as over Bluetooth, but anyone in range can forge a value on an ESP-NOW connection, and the radio says so
  (`Limits::forgeable`). So it holds it 1 to 4 s, as after a refusal, after any close forged frames can cause: one for
  a fault of the connection's link (a message the room proof doesn't expect, a wrong answer or proof, or 32 messages
  dropped once proven), one the other end made before the room was proven (NB135-8), one that missed the room proof's
  deadline (3 s), one of a proven connection bound to no peer (a standby) that the other end closed while this node's
  link to that peer goes nowhere else (NB135-10), and one the radio dropped (NB135-5); and after the close of a
  connection bound to a peer (NB135-I6). So it does after a connect that failed, as OPENs forged as fast as the radio
  takes them keep its one over taken and a newcomer's go unanswered (NB135-13), and after a close that made way for
  another connection, a new central's or one that proved the room into the last unbound slot (NB135-14). It holds none
  after its link timed out (NB135-1), and a minute otherwise: after 60 s with no room member heard over a standby, and
  after the other end closed a standby beside a link to the same peer elsewhere (over the network, through a peer, or
  over another open connection), so spares don't churn. A link whose connection closed under it moves to the next
  connection proven to reach its peer as soon as there is one, with a new handshake, rather than waiting out its
  session's idle timeout (NB135-I7).

`espnow::Settings` holds the numbers above (`Settings::DEFAULT`); the first board run (NB-138) kept them.

## Sending

ESP-NOW's send callback says whether the receiving radio acknowledged a frame, after the radio's own retries; with
several frames in flight, ESP-IDF says the callbacks may come out of order. So the ESP32 driver puts one frame on the
air at a time, from a thread of its own, and the next once the callback for the last has come (or 200 ms have
passed). A unicast frame not acknowledged goes again, 4 times in all on a connection whose other end has acknowledged
one, and twice to an address that never has (it may be made up: an OPEN to a replayed advert, an ACCEPT to a forged
OPEN), so a frame to an absent address holds the air for two tries, not four; a broadcast and a refusal go once
(NB135-3). The radio hands the driver 4 frames at a time, the next as a verdict comes, and keeps the rest itself, under
its caps, which count the 4 too (NB135-15): it handed the driver all it would take, 32, and the caps saw none of them,
so a PROBE every 100 ms on a forged connection was answered every time, and seconds of frames to made-up addresses
waited ahead of every real frame. Each frame's verdict comes back in order (`espnow::TxQueue`), with the tag the radio
gave the frame, so one that comes late, after its frame was taken as lost (5 s, more than the 4.8 s the 4 can hold the
air: 4 tries each, a try ending at its send callback, or with none 200 to 300 ms on, when the driver's thread, which
looks every 100 ms, expires it), is dropped, not taken for the next frame's. A value's queue slot frees once its
last frame is acknowledged, and a connection whose frames fail 8 times in a row is gone.

ESP-NOW sends a unicast frame only to an address in its peer table, of 20 at most: the driver keeps the 16 it sent to
most lately (`espnow::PeerTable`) and drops the oldest with no frame waiting for a new one (NB135-I2); as it holds 4
frames at most, 4 addresses at most have frames waiting, and frames to made-up ones can't fill the table (NB135-15).
ESP-NOW's own encryption (CCMP with a key per peer, for at most 17 peers) isn't used: the node seals what crosses.

## Limits

| | Default |
|---|---|
| Connections | 4, all of them centrals' if need be; 2 bound to no peer. The radio takes one OPEN over its 4 open connections for the node to decide on (NB135-11), its connects under way not counted (NB135-13), and never more than 8 in all |
| Connects | an OPEN every 500 ms for 10 s; failed at once if no radio acknowledged the first (NB135-12), even if an ACCEPT came before the verdict (NB135-16) |
| Values queued per connection | 8; then the node keeps its own until the radio takes them |
| Distinct HS1s on unbound connections, across the radio | 2, then one per 5 s, as on a board's Bluetooth |
| Frames handed to the driver | 4, counted against the caps below; each one's verdict comes back in order, with its tag (NB135-15). The ESP32 driver's queue holds 32 |
| Frames received waiting for the node (ESP32) | 32 and 16 KB; more are dropped, and counted. A frame that isn't ours by its first bytes (`PW`, version 1) or is over 531 B (the longest the parser takes) is dropped before it is copied (NB135-2) |
| Adverts and values waiting for the node | 16, and 128 values of 16 KB in all; more are dropped, and counted (NB135-2) |
| Frames waiting in the engine for the ether | 256; of them and the 4 handed over, 4 of no connection and one refusal; more are dropped, and counted |
| Refusals (a CLOSE for an ACCEPT nobody waits for) | one per 100 ms, one try |
| Tries of a frame not acknowledged | 4; 2 to an address that never acknowledged one; 1 for a broadcast or a refusal |
| PROBEs answered per connection | one per 100 ms, and none while a KEEPALIVE of the connection waits for the air, in the engine or with the driver (NB135-I5, NB135-15) |

**Throughput (theoretical):** at 1 Mbit/s a 250-byte frame is on the air for 192 µs of preamble and 8 µs per byte of
the frame and its ~43 bytes of 802.11 and ESP-NOW headers: 2.5 ms; with its acknowledgement (0.3 ms) and the gaps
between (~0.4 ms), about 3.2 ms. One at a time, that is ~310 frames a second, ~70 KB/s of payload (231 bytes each)
one way. At 54 Mbit/s the frames are 20 times shorter, and the waits between them dominate.

**Measured (NB-138):** an ESP32-C3 and an ESP32 on channel 11, ESP-NOW v2 at both ends, 1 Mbit/s, Bluetooth off, with
no access point, one board on an access point, or both. No ping was lost in 3,600 and no frame in 9,482 sent 16 at a
time. Round trips took 4 ms at 32 B, 7.5 ms at 250 B and 28 ms at 1,470 B (p50), 84 ms at most at p99, 141 ms at
most. 241 to 305 frames of 250 B went a second (480 to 610 kbit/s), as above, and 57 to 71 of 1,470 B (670 to 840
kbit/s). With Bluetooth LE advertising and scanning on the same chip, the p90 at 250 B and under grew to 24 to 48 ms
and 1,470 B round trips to 66 to 76 ms (p50), 1 frame in 120 was lost at 32 B, and Bluetooth took ~76 KB of the
C3's heap. At 54 Mbit/s (802.11g, per peer) the boards didn't link: the ESP32 heard the C3's first frame, and the C3
never heard an answer in 120 s.

**A link on boards (NB-139):** `espnow_mesh` on the same C3 and ESP32, on an access point's channel, 11, built from
before the review's second round, one run of 5 minutes. The link held throughout, no message was lost once it was up,
and the node's round trips were 35 to 42 ms. A failed try to an absent address (250 B, 5 each) took 85.7 to 88.7 ms on
the C3 and 99.5 to 105.2 ms on the ESP32, so the driver's 200 ms wait for a send's verdict is ~1.9 times the longest.
With the ESP32 not polling for 2 s while the C3 broadcast 128 frames of 531 B, its receive queue was full at 32 frames,
13,250 B in all, 84 frames were dropped, its heap's low point was 145,032 B, and the link stayed up. The first link took
~27 KB of heap: from 182,200 B free once joined to ~155,452 B on the C3, from 185,020 B to ~157,848 B on the ESP32. One
run on two boards: 4 connections at once haven't run on boards.

**Where a round trip goes (NB-142, not run on boards yet):** against ~4 ms for a raw ping of 32 B, the node's round
trips were 35 to 42 ms. What the code says, **theoretical**: the example's loop sleeps up to 10 ms
(`peerwire_esp::idle`) and nothing wakes it for a frame, so each end's half of a round trip waits 0 to 10 ms for its
next pass (10 ms on average for both); the radio hands the driver 4 frames at a time and learns of their verdicts only
when it is polled, so a datagram of more than 4 frames waits a pass too; every fifth message is 2,000 B, five frames of
up to 531 B, ~5 ms each at 1 Mbit/s one at a time (~28 ms), which the smoothed RTT takes in; and a report line at
115200 baud holds the loop for tens of ms. `espnow_mesh` now logs each link's smoothed and lowest RTT
(`ESPNOW_RTT`) and the loop's times (`ESPNOW_LOOP`), and takes turns a minute each with an idle the radio wakes
(`Node::set_waker`), so the next run tells these apart.

## What a listener on the radio sees

- **The address:** every ESP-NOW frame carries its sender's MAC address in the clear. The chip's own (Espressif's
  OUI and a fixed number) is the same on every boot and on every network the board joins, so anyone in range can
  follow the board. `wifi::start_on_channel_as` with `wifi::random_mac` gives it a random, locally administered
  address instead, drawn at boot, as `espnow_mesh` does by default; it lasts the boot, unlike Bluetooth's, which
  changes with every advert, so within a boot the adverts that change every 15 minutes are tied together by it.
- **Adverts:** that a Peerwire peer is near, with `n` and a tag that change every 15 minutes, and whether it is full;
  with the room's name, whether it is a member, as over Bluetooth.
- **Connections:** their ids, sizes and timing, and the sealed signals and Noise packets they carry: not their
  contents. Nothing on ESP-NOW is encrypted below the node.

## Forged frames

ESP-NOW frames aren't authenticated: anyone in range can send any frame from any address, and anyone in range can
overhear a connection's ids. So the ids keep out stale frames and anyone not listening, not an attacker in range,
which could jam the channel anyway. What such an attacker can do, frame by frame:

| Forged | At most |
|---|---|
| ADVERT | a `Seen` for the node, which checks the room's tag (HMAC), 50 at once and then one per 20 ms; 16 wait for it at most. The tag is bound to the address it is sent from, so a copy of a member's advert from another address, made up or a radio of the forger's, is another room's: it drew a connect that held the member's own adverts off for 10 s, and one from a new address as each ran out kept two boards apart (NB135-12). A real advert replayed from its own address, a member's that has gone, draws one OPEN, two tries, and the connect fails at once as no radio acknowledges it (NB135-12), even if an ACCEPT forged from that address comes before the verdict (NB135-16); while the replays go on, again 1 to 4 s later. With a radio of the forger's at that address, which acknowledges the OPENs and answers none, the connect runs 10 s, and a few such adverts in turn keep a pair apart while they go on (NB135-16, accepted: What stays, below) |
| OPEN, from made-up addresses | a connection whose central never proves the room, closed after 3 s; 2 of them at most, the unbound ones. They count against none of the node's own connects, which close the oldest if they need its slot, and a new central takes the oldest's place at once, before any connection of the node's own (NB135-14); the radio takes one OPEN over its 4 open connections, its connects under way not counted (NB135-13), and leaves it to the node, silent past that one until the node has decided on it. So OPENs forged to two boards, one each every 1.2 s or twice a second, no longer keep them apart (linked in tens of ms on `FakeAir`), nor do two forged beside two bound ones keep a newcomer out (NB135-11), with a connect of the node's under way beside them too (linked in 0.48 s, NB135-13). OPENs forged as fast as the radio takes them, each right after the node has decided on the last, keep the one over taken: a newcomer's OPENs go unanswered, its connect fails after 10 s, and it connects again 1 to 4 s later (NB135-13), as jamming would cost. Each ACCEPT to a made-up address holds the air for two tries (NB135-3), and waits in the engine, under its caps (NB135-15) |
| ACCEPT, before the real one | the opener takes the forged id; when the real ACCEPT comes, with another id and within 3 s, the connection closes, even if forged data came meanwhile (NB135-4), and the node connects again 1 to 4 s later (NB135-5). If the attacker also keeps the real one from arriving (jamming), the opener sends to the forged id, the room proof stalls, and the node closes the connection after 3 s and connects again 1 to 4 s later (NB135-10) |
| ACCEPT, to an OPEN no radio acknowledges | the connection it opens closes at the OPEN's verdict, as the connect fails, and the node connects again 1 to 4 s later: it held one of the node's unbound slots until the room proof's deadline, 3 s, with an ACCEPT forged from a gone member's address while the OPEN's two tries were on the air (NB135-16) |
| ACCEPT, after the real one, within 3 s of it | the connection closes, though the room's challenge came on it already, and the node connects again 1 to 4 s later, as the radio reports the close as `Dropped`, which says nothing of the other end (NB135-5): one forged frame for each reconnect, as jamming would cost |
| ACCEPT, more than 3 s after the first | ignored |
| CLOSE | nothing, unless the attacker also keeps the other end's answer to the PROBE from arriving (jamming): then a close and a reconnect |
| PROBE | a KEEPALIVE in answer, one per 100 ms per connection, and none while one waits for the air, in the engine or with the driver: one waiting at most (NB135-I5, NB135-15). On a connection from a made-up address each answer holds the air for two tries, and a PROBE every 100 ms held the driver's 32 frames, seconds ahead of every real frame, until the caps counted the driver's (NB135-15) |
| KEEPALIVE | keeps a connection whose peer left open for as long as it keeps sending: the node closes it once its link times out and no proof of a room member has come over it for 60 s, as over Bluetooth |
| DATA, in the window | a value for the node; the real frame with its number is dropped as a copy, and the value it is part of, and so is the value being joined if the forged frame says it starts one: a value or two lost, which the reliable lane sends again. The real frames behind it are still taken (NB135-6). The value can be a whole message of the node's framing, and the connection's link takes it: before the room is proven, any message the proof doesn't expect closes the connection; once proven, a datagram that doesn't open under the link key is dropped before the node sees it, uncounted, and takes nothing (NB135-9), and any other message that fails its checks (a signal, or of a kind the link drops) is dropped and counted, and 32 close it. Frames forged with the numbers of the other end's room proof, each a part that is no message, make the real proof a copy, so it never comes, and the connection closes at its 3 s deadline, or sooner if it makes way for another connection (NB135-14); 32 forged to the accepter of a proven connection no handshake has bound yet close it there. The node connects again 1 to 4 s later, as after any close forged frames can cause, and a link on the connection moves to the new one at once (NB135-8, NB135-10, NB135-I7): one forged frame, 8, or 32, for each reconnect, as jamming would cost |
| DATA, further off | dropped; 4 in a row (ahead, or more than 64 behind) move the receiver to the last of them, with none of the 64 numbers behind it taken (NB135-7). The sender's real frames that land within those 64 are taken, and the one with its number is dropped; 4 further off move it back. Either way 4 real frames are lost at most, which the reliable lane sends again (NB135-1). A forger that goes on, one frame in the receiver's window after every 3 real ones, keeps that end out of step: its link times out and comes back on a new connection within seconds (NB135-I6) |
| DATA cut short, or a part of another value | the value dropped: its parts don't add up to its length |
| Anything from a group address, of another version or malformed | dropped |

For each of these the connection closes, loses frames, or stays as it was, and the node connects again. That is what
the tests show, frame by frame, not a proof that no mix of frames can stall a connection while the radio still has it
up. So the node has its own check too: when a link times out, the radio connections bound to its peer close, with no
hold on the advert, and the next link comes over a new connection with new ids (NB135-1); the other end, if it made
the connection, holds the advert only 1 to 4 s when it hears the close, as it does whenever a bound connection closes
(NB135-I6), so the link is back within seconds whichever end timed out first; so it does after any close forged
frames can cause (NB135-8, NB135-10), and a link whose connection closed under it moves to the next one at once
(NB135-I7).
Connections, queues and buffers are capped above, and the parser is fuzzed.

The security review of NB-135 (the [threat model](../project/threat-model.md)) found sixteen problems in eight rounds,
fixed since, each with a test that was its repro:

- **NB135-1 (Medium):** four forged DATA frames far ahead, sent each way with ids overheard on the air, moved both
  ends' numbers so that every real frame after was dropped as old, while keepalives kept the connections up: the link
  was gone for good. The resync now works both ways, and a link that times out closes its radio connections.
- **NB135-2 (Low):** a board's receive callback copied any frame heard, up to 1,470 B, before it checked its queue
  had room: about 47 KB of anyone's frames (**theoretical**). It now drops what isn't ours or is over 531 B before any
  copy, and caps the queue at 16 KB.
- **NB135-3 (Low):** frames to addresses that never acknowledge (an ACCEPT to a forged OPEN, an OPEN to a replayed
  advert, refusals) took 4 tries each of the one slot on the air, and the engine's queue had no cap. They now get 2
  tries (a refusal 1), the queue is capped, and one refusal waits at most.
- **NB135-4 (Low):** a forged ACCEPT followed by one forged DATA frame made the real ACCEPT look like the forgery,
  with no jamming. A second ACCEPT within 3 s of the first now closes the connection whatever came.
- **NB135-5 (Low, from NB135-4's fix):** that close came after the room's challenge, which arrives within
  milliseconds, so the node held the advert for a minute: one forged ACCEPT a minute kept two boards apart. The radio
  now reports such a close as `Dropped`, and the node connects again 1 to 4 s later.
- **NB135-6 (Low):** one forged DATA frame 64 ahead made the receiver drop the next 63 real ones as behind it; on an
  idle link those are the session's keepalives, so the link timed out, and came back a minute later if the end it
  timed out at was the accepter. The receiver now keeps a bit for each of the 64 numbers behind the highest it took,
  and takes each once, so a forged frame costs the real frame of its number.
- **NB135-7 (Low):** a resync counted the 64 numbers behind the receiver's new place as taken. Four DATA frames
  forged 65 ahead moved it there, and the sender's next 64 real frames, landing behind it, were dropped as copies,
  which don't count towards the resync back: on an idle link about 64 s of keepalives, so the link timed out
  (**theoretical**). A resync, either way, now counts none of them as taken: the move costs the real frame of the
  forged number, and 4 real frames at most wherever it lands.
- **NB135-8 (Low, round 4):** a DATA frame's value can be a whole message of the node's framing, which anyone who
  overheard the ACCEPT can forge. One that the room proof doesn't expect closes the connection, and so do 32 dropped
  on a proven one; the end that connected then held the advert a minute, as it did when the other end closed before
  the proof. So one forged frame during the room proof kept two boards apart for a minute (60.3 and 60.7 s on
  `FakeAir`, against 55 ms with none), and 32 on a live link did too. The radio now says its values can be forged
  (`Limits::forgeable`), and the node holds the advert 1 to 4 s after such a close; Bluetooth keeps the minute.
- **NB135-9 (Low, round 5):** a datagram carried no MAC, and the first HS1 on a connection no peer was bound to yet
  took its one slot for 10 s, and the radio's budget for handshakes, before the endpoint looked at it. So one 37-byte
  HS1 forged to each end just after the identity proofs kept the real handshake out: linked after 15.5 s on `FakeAir`,
  against 63 ms with none, and with a pair every 10 s, never in 70 s; it held up a moved link's redial too (NB135-I7).
  Datagrams are now sealed under the link key as signals are, and one that doesn't open is dropped before it takes
  anything. It isn't counted towards the connection's 32 drops: counted, 32 forged datagrams would close a proven
  connection.
- **NB135-10 (Low, round 5):** two closes forged frames reach without jamming still held the advert a minute. Eight
  frames forged to the opener with the numbers of the accepter's room proof made the real ones copies, and the
  connection missed the proof's deadline (linked again after 63.2 s); and 32 forged to the accepter of a proven
  connection no handshake had bound yet made it close it (60.9 s). Over ESP-NOW both now hold it 1 to 4 s, but a
  standby closed beside a link to the same peer elsewhere keeps the minute, so spares don't churn.
- **NB135-11 (Low, round 6):** an OPEN forged from a made-up address makes a connection whose central never proves
  the room, and two of them took a board's two unbound slots, which counted against its own connects: it connected to
  no one. One OPEN to each of two boards every 1.2 s kept them apart for good (70 in 40 s on `FakeAir`, against 16 to
  56 ms with none). And a central's connection made way for a newcomer only after 1 s, so OPENs twice a second, each
  within it, kept newcomers out too; and a board holding 2 bound and 2 forged was silent at its radio's 4 before its
  node could make way, so a newcomer's connect failed after 10 s and held the advert a minute. Now such connections
  count against no connect of the node's own, the oldest closes when one needs its slot, a newcomer takes the oldest's
  place at any age, and the radio takes one OPEN over its limits for the node to decide on, and never refuses the
  node's own connect for them. Bluetooth is unchanged.
- **NB135-12 (Low, round 7):** anyone in range could copy a member's advert and send it from a made-up address at any
  instant: a node connected to the copy, which no one answered, and skipped the member's own adverts as one it was
  connecting to already, and a copy from a new address as each connect ran out kept the two apart (not linked in 40 s,
  87 copies, against 0.47 s with none). The advert's tag is now bound to the address it is sent from, so a copy from
  another one isn't taken (linked in 0.51 s with copies from made-up addresses, 0.55 s with copies from radios that
  acknowledge the OPENs), and a connect whose first OPEN no radio acknowledged fails at once (two gone members'
  adverts replayed from their own addresses: linked in 0.60 s, against 10.5 s).
- **NB135-13 (Low, round 7, in NB135-11's fix):** the radio's one OPEN over its limits counted a connect of the node's
  own under way, which the node's choice of a connection to make way doesn't, so beside one the node took the one
  over with room to spare and the radio was silent to every OPEN after it: a newcomer's connect failed after 10 s and
  held the advert a minute (linked after 70.4 s). The radio now counts open connections (linked in 0.48 s), and a
  failed connect holds the advert 1 to 4 s, as OPENs forged as fast as the radio takes them can keep its one over
  taken (the review's run: linked 15.5 s after the newcomer came, with the forger on for its first 15 s).
- **NB135-14 (Low, round 7):** the closes that make way for another connection held the advert a minute: a new
  central's took the oldest connection that hadn't proven the room once it had had 1 s, the node's own among them,
  and so did one that proved the room into the last unbound slot, at any age. NB135-10's 8 frames, then 2 forged
  OPENs 1.2 s on, made the node's own connection make way: linked again after 61.4 s, against 6.3 s with the frames
  alone. A central's connection that may be forged now makes way before one of the node's own, and one of its own that
  makes way holds the advert 1 to 4 s (linked again after 7.0 s). Three boards joining at once with no forger, which
  closed one of their own connections this way, now link within seconds (2.4 to 6.8 s on `FakeAir`, where 4 runs in 6
  took 60 to 63 s).
- **NB135-15 (Low, round 7):** the radio handed the driver all it would take, 32 frames on an ESP32, outside its caps,
  so a PROBE every 100 ms on a forged connection was answered every time: the driver's queue held 32, a value waited
  4.37 s behind frames to made-up addresses, and a verdict that came after its frame was taken as lost (5 s) was taken
  for the next frame's, and every one after it for the frame before its own; and frames waiting for made-up addresses
  grew the driver's peer table past its 16, and past ESP-NOW's 20 a frame to a real peer fails. The radio now hands the
  driver 4 at a time, its caps count them, and each verdict is matched to its frame by a tag: the queue holds 4, a
  value waits 0.1 s, no verdict is taken for another frame's, and the peer table keeps to 16.
- **NB135-16 (Low, round 8, in NB135-12's fix):** an ACCEPT forged from a gone member's address, with the id of the
  node's OPEN to it overheard, came while the OPEN's two tries were on the air (~0.2 s on a board, **theoretical**):
  it opened the connection, and the OPEN's verdict, unacknowledged, then closed nothing. So one overheard OPEN and one
  forged ACCEPT held one of the node's unbound slots until the room proof's deadline, 3 s, with no connect given up
  (still open 3 s on, in the engine's test). The verdict now closes it whatever came first, and a real ACCEPT whose
  OPEN's acknowledgement was lost goes with it: the node connects again 1 to 4 s later.

And four smaller ones: a PROBE was answered every 100 ms even with an answer still waiting for the air (NB135-I5: now
one waits at most); the end that made a connection held the advert a minute when the other end closed it as its link
timed out (NB135-I6: a bound connection that closes now holds the advert 1 to 4 s, so a room member that keeps
closing them makes its peer connect again every 1 to 4 s, which is accepted); a link whose connection closed
under it stayed on it until its session timed out, 30 s, so the next connection came within seconds but the link
didn't (NB135-I7: it is now dialled again over the next connection that proves its peer, at once); and no test closed a
standby beside a link left on a connection that had closed, which should hold the advert 1 to 4 s, not a minute
(NB135-I8: one does now).

The eighth round checked the fixes of NB135-12 to NB135-15 and found NB135-16 in NB135-12's, fixed above. It
accepted two things for v1, both Low and costing a pair only while a forger goes on, as jamming does: OPENs forged as
fast as a board decides, which keep a newcomer out of a full one (NB135-13), and replays of gone members' adverts
with a radio of the forger's at each address (NB135-16). Both are in What stays, below.

What stays, as on any radio: an attacker in range can jam the channel, cut a connection by jamming the answer to a
PROBE, and hold a dead connection open with forged KEEPALIVEs until the node's own rules close it. Forged DATA frames
cost a connection a value or two each (4 frames for the 4 that move its numbers) if the forger then stops; one that
goes on, one frame for every 3 real ones, keeps the connection's numbers out of step for as long as it does, so the
link times out and comes back on a new connection within seconds, as jamming would. One forged DATA frame during the
room proof, 8 that take the numbers of its messages, or 32 messages (not datagrams, which are sealed) once it is
proven, close the connection, and the link is back on a new one within seconds (1.5 to 7 s on `FakeAir`), as after a
cut. OPENs from made-up addresses, 2 or more within a member's round trip of the room proof (milliseconds), can close
the member's connection before it proves the room, and it connects again 1 to 4 s later; sent as fast as a board takes
them, each right after it has decided on the last, they keep the radio's one OPEN over its limits taken, so a newcomer
that connects to it is kept out while they go on and links within seconds of their end, holding the advert 1 to 4 s
after each failed connect (NB135-13): a forger that keeps either up is jamming. **OPENs that keep the one over taken
are accepted for v1 (Low, the review's choice 7):** as jamming does, they cost a newcomer only while a forger sends
one right after each of the node's decisions. A replay of a gone member's advert from that member's own address still
draws a connect, which fails at once when no radio acknowledges its OPEN, even if an ACCEPT forged from that address
comes before the verdict (NB110-11's residual, NB135-12, NB135-16). **A radio at a gone member's address is accepted
for v1 (NB135-16, Low):** it acknowledges the OPENs to it, as any radio does for its own address, and answers none,
so the connect runs its 10 s. A forger with a few gone members' adverts, each `n` above the node's, and a radio at each
of their addresses, replaying one whenever fewer than two of the node's connects to them are under way, keeps the
pair apart while it goes on, and they link as it stops (the review's run: not linked in the 30 s it ran, linked
0.25 s after; the test's, 10 s: 0.12 s after). It needs the forger's radios to take gone members' addresses, and
their adverts, and lasts only while it runs. To bound it later, the node could prefer an advert from a device it
hasn't connected to lately. **The MAC address is a residual, accepted (NB135-I3):** with `random_mac` a board is a new
address on each boot, but one address for the whole boot, so a listener can tie a boot's adverts together, and a network
the board joins sees it too; without it, the chip's own address follows the board for good.

## The driver

`peerwire::espnow::EspNow<E: Ether>` is the radio: the engine (`espnow::Engine`), driven by the time and by an
`Ether`, the platform's ESP-NOW (its address; a frame to an address or to all, handed over a few at a time; each
frame's verdict, in order, with the tag the radio gave it; the frames heard).
`peerwire-esp`'s `espnow::EspNowEther` is the ESP32's: `esp_now_init` on the started station, a broadcast peer,
ESP-NOW's two callbacks (which only queue and wake, on the Wi-Fi task), a thread that puts frames on the air one at a
time, and the peer table. `espnow::FakeAir` (feature `testing`) has ethers in one process, for tests: frames lost, sent
twice, overheard or forged.

```sh
# Two boards, each with its node number and the room in the config sector (as the mesh example's):
cd ports/esp32
CHIP=esp32c3 FEATURES=espnow EXAMPLE=espnow_mesh ./build.sh   # out/espnow_mesh.bin
CHIP=esp32 FEATURES=espnow EXAMPLE=espnow_mesh ./build.sh
```

## Not yet

- **Several radios at once:** a node has one radio, so a board links over Bluetooth or over ESP-NOW. Both need the
  core to take several; the path kinds are apart already (`PathKind::Bluetooth`, `PathKind::EspNow`).
- **Values over 512 bytes:** the node's framing keeps GATT's 512, so a v2 frame carries one value at most.
- **The rate:** frames go at ESP-NOW's default 1 Mbit/s, which carried 670 to 840 kbit/s on the NB-138 boards. At 54
  Mbit/s per peer (`esp_now_set_peer_rate_config`) they didn't link; a faster rate needs a way back to 1 Mbit/s, as
  a rate one end can't hear leaves a connection silent until it times out. The probe now tries 11 Mbit/s (802.11b)
  and 6, 24 and 54 Mbit/s (802.11g) in turn, each given up after 30 s, with its mode change also broadcast at 1
  Mbit/s and counts on both boards of the frames sent, acknowledged and received by rate (`ESPNOW_RATE_COUNTS`), to
  show where one fails (NB-142); it hasn't run on boards yet.
- **More on boards:** the probe kept the frame size, the rate and the timers (NB-138), and one run of `espnow_mesh`
  showed a link's heap, the receive queue full and the time a failed try holds the air (NB-139), from before the
  review's second round; 4 connections at once, and the fixes since, haven't run on boards.
