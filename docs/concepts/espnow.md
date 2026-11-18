# ESP-NOW

> [!NOTE]
> This is the design of Peerwire over ESP-NOW (NB-135): boards linking with each other over Espressif's ESP-NOW
> frames, with no Wi-Fi network and no Bluetooth. The connection logic is the `peerwire` feature `espnow`
> (`espnow::Engine`, sans-IO, with host and fuzz tests); the ESP32 driver is `peerwire-esp` feature `espnow`, and its
> `espnow_mesh` example links two boards. The protocol below has had its security review, in four rounds (what it
> found is [below](#forged-frames)). The probe (`espnow_probe`, NB-135) measured the radio on two boards first
> (NB-138, [below](#limits)), and the defaults stand; the same two boards then ran a link for 5 minutes, once, from
> before the review's second round (NB-139).

Boards near each other link over ESP-NOW when they share no network: two ESP32 boards in a field, say, or a board
whose Wi-Fi network is down. ESP-NOW is a connectionless link layer of Espressif's: the chip's Wi-Fi radio sends a
frame of up to 250 bytes (1,470 with ESP-NOW v2 at both ends) to a MAC address, which the receiving radio
acknowledges, or to every radio on the channel, with no access point and no association. Peerwire makes a Bluetooth
radio of it (`ble::Radio`): adverts go out as broadcasts and connections are emulated over unicast frames, so
everything a node does over Bluetooth runs unchanged on top: the room advert and its proof, Noise IK, sealed signals
and datagrams, identity proofs, and the path's rank, below every UDP path (`PathKind::Bluetooth`).

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
- **Scans:** a station that scans or joins hops across channels for a few seconds (joining the hotspot took 6.7 and
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

## Connections

The end that connects (the node's central) sends OPEN with its id. The other end, if it advertises and has room,
draws its own id, answers ACCEPT and reports the connection to its node (`Connected` with no device, as a central's);
the connecting end reports it when the ACCEPT comes. If not, it stays silent: the node's advert already says when it
is full, and a refusal would be one more frame to forge. A connection is then known at each end by the pair (the other
end's MAC address, this end's id), and every frame on it carries the receiver's id and must come from that address.

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
  10 s fails (`ConnectFailed`), as the node's radio contract asks.
- **Closing:** the node closes a connection with a CLOSE, and hears nothing more of it. A CLOSE received isn't taken at
  its word: the receiver sends a PROBE and closes only if nothing comes on the connection within 500 ms. The other end
  answers a PROBE with a KEEPALIVE at once, if it still has the connection.
- **A second ACCEPT:** the opener takes the first ACCEPT. A second one with another id within 3 s of the first (the
  time a connection has to prove the room) means one of the two was forged, and which can't be told, whatever came on
  the connection meanwhile, as that may be forged too: the connection closes (NB135-4). The radio reports it as
  `Dropped`, not `Disconnected`, as it says nothing of the other end, so the node connects again 1 to 4 s later, as
  after a refusal, though the room's challenge has mostly come by then (NB135-5). Later, one is ignored.
- **Connecting again:** the end that connected holds the advert of a connection that closed before it connects to it
  again, as over Bluetooth, but anyone in range can forge a value on an ESP-NOW connection, and the radio says so
  (`Limits::forgeable`). So it holds it 1 to 4 s, as after a refusal, after any close one forged frame can cause: one
  for a fault of the connection's link (a message the room proof doesn't expect, a wrong answer or proof, or 32 messages
  dropped once proven), one the other end made before the room was proven (NB135-8), and one the radio dropped
  (NB135-5); and after the close of a connection bound to a peer (NB135-I6). It holds none after its link timed out
  (NB135-1), and a minute otherwise: after a connect that failed, say, or a room proof that stalled for 3 s once the
  challenge came. A link whose connection closed under it moves to the next connection proven to reach its peer as soon
  as there is one, with a new handshake, rather than waiting out its session's idle timeout (NB135-I7).

`espnow::Settings` holds the numbers above (`Settings::DEFAULT`); the first board run (NB-138) kept them.

## Sending

ESP-NOW's send callback says whether the receiving radio acknowledged a frame, after the radio's own retries; with
several frames in flight, ESP-IDF says the callbacks may come out of order. So the ESP32 driver puts one frame on the
air at a time, from a thread of its own, and the next once the callback for the last has come (or 200 ms have
passed). A unicast frame not acknowledged goes again, 4 times in all on a connection whose other end has acknowledged
one, and twice to an address that never has (it may be made up: an OPEN to a replayed advert, an ACCEPT to a forged
OPEN), so a frame to an absent address holds the air for two tries, not four; a broadcast and a refusal go once
(NB135-3). The engine hears each frame's verdict in order (`espnow::TxQueue`): a value's queue slot frees once its
last frame is acknowledged, and a connection whose frames fail 8 times in a row is gone.

ESP-NOW sends a unicast frame only to an address in its peer table, of 20 at most: the driver keeps the 16 it sent to
most lately (`espnow::PeerTable`) and drops the oldest with no frame waiting for a new one (NB135-I2). ESP-NOW's own
encryption (CCMP with a key per peer, for at most 17 peers) isn't used: the node seals what crosses.

## Limits

| | Default |
|---|---|
| Connections | 4, all of them centrals' if need be; 2 bound to no peer |
| Values queued per connection | 8; then the node keeps its own until the radio takes them |
| Distinct HS1s on unbound connections, across the radio | 2, then one per 5 s, as on a board's Bluetooth |
| Frames waiting for the air (ESP32) | 32; each one's verdict comes back in order |
| Frames received waiting for the node (ESP32) | 32 and 16 KB; more are dropped, and counted. A frame that isn't ours by its first bytes (`PW`, version 1) or is over 531 B (the longest the parser takes) is dropped before it is copied (NB135-2) |
| Adverts and values waiting for the node | 16, and 128 values of 16 KB in all; more are dropped, and counted (NB135-2) |
| Frames waiting in the engine for the ether | 256; of them, 4 of no connection and one refusal; more are dropped, and counted |
| Refusals (a CLOSE for an ACCEPT nobody waits for) | one per 100 ms, one try |
| Tries of a frame not acknowledged | 4; 2 to an address that never acknowledged one; 1 for a broadcast or a refusal |
| PROBEs answered per connection | one per 100 ms, and none while a KEEPALIVE of the connection waits for the air (NB135-I5) |

**Throughput (theoretical):** at 1 Mbit/s a 250-byte frame is on the air for 192 µs of preamble and 8 µs per byte of
the frame and its ~43 bytes of 802.11 and ESP-NOW headers: 2.5 ms; with its acknowledgement (0.3 ms) and the gaps
between (~0.4 ms), about 3.2 ms. One at a time, that is ~310 frames a second, ~70 KB/s of payload (231 bytes each)
one way. At 54 Mbit/s the frames are 20 times shorter, and the waits between them dominate.

**Measured (NB-138):** an ESP32-C3 and an ESP32 on channel 11, ESP-NOW v2 at both ends, 1 Mbit/s, Bluetooth off, with
no access point, one board on the hotspot, or both. No ping was lost in 3,600 and no frame in 9,482 sent 16 at a
time. Round trips took 4 ms at 32 B, 7.5 ms at 250 B and 28 ms at 1,470 B (p50), 84 ms at most at p99, 141 ms at
most. 241 to 305 frames of 250 B went a second (480 to 610 kbit/s), as above, and 57 to 71 of 1,470 B (670 to 840
kbit/s). With Bluetooth LE advertising and scanning on the same chip, the p90 at 250 B and under grew to 24 to 48 ms
and 1,470 B round trips to 66 to 76 ms (p50), 1 frame in 120 was lost at 32 B, and Bluetooth took ~76 KB of the
C3's heap. At 54 Mbit/s (802.11g, per peer) the boards didn't link: the ESP32 heard the C3's first frame, and the C3
never heard an answer in 120 s.

**A link on boards (NB-139):** `espnow_mesh` on the same C3 and ESP32, on the hotspot's channel 11, built from before
the review's second round, one run of 5 minutes. The link held throughout, no message was lost once it was up, and the
node's round trips were 35 to 42 ms. A failed try to an absent address (250 B, 5 each) took 85.7 to 88.7 ms on the C3
and 99.5 to 105.2 ms on the ESP32, so the driver's 200 ms wait for a send's verdict is ~1.9 times the longest. With the
ESP32 not polling for 2 s while the C3 broadcast 128 frames of 531 B, its receive queue was full at 32 frames,
13,250 B in all, 84 frames were dropped, its heap's low point was 145,032 B, and the link stayed up. The first link took
~27 KB of heap: from 182,200 B free once joined to ~155,452 B on the C3, from 185,020 B to ~157,848 B on the ESP32.
One run on two boards: 4 connections at once haven't run on boards.

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
| ADVERT | a `Seen` for the node, which checks the room's tag (HMAC), 50 at once and then one per 20 ms; 16 wait for it at most. A real advert replayed from an absent address draws OPENs for 10 s, two tries each |
| OPEN, from made-up addresses | fills the 4 connections, and the radio stays silent to the rest; the node closes one that hasn't proven the room within 3 s, and a central's makes way for a new one after 1 s. Each ACCEPT to a made-up address holds the air for two tries (NB135-3) |
| ACCEPT, before the real one | the opener takes the forged id; when the real ACCEPT comes, with another id and within 3 s, the connection closes, even if forged data came meanwhile (NB135-4), and the node connects again 1 to 4 s later (NB135-5). If the attacker also keeps the real one from arriving (jamming), the opener sends to the forged id, the room proof stalls, and the node closes the connection after 3 s and holds the advert for 60 s |
| ACCEPT, after the real one, within 3 s of it | the connection closes, though the room's challenge came on it already, and the node connects again 1 to 4 s later, as the radio reports the close as `Dropped`, which says nothing of the other end (NB135-5): one forged frame for each reconnect, as jamming would cost |
| ACCEPT, more than 3 s after the first | ignored |
| CLOSE | nothing, unless the attacker also keeps the other end's answer to the PROBE from arriving (jamming): then a close and a reconnect |
| PROBE | a KEEPALIVE in answer, one per 100 ms per connection, and none while one waits for the air: one waiting at most (NB135-I5) |
| KEEPALIVE | keeps a connection whose peer left open for as long as it keeps sending: the node closes it once its link times out and no proof of a room member has come over it for 60 s, as over Bluetooth |
| DATA, in the window | a value for the node; the real frame with its number is dropped as a copy, and the value it is part of, and so is the value being joined if the forged frame says it starts one: a value or two lost, which the reliable lane sends again. The real frames behind it are still taken (NB135-6). The value can be a whole message of the node's framing, and the connection's link takes it: before the room is proven, any message the proof doesn't expect closes the connection; once proven, one that fails its checks (sealed, or of a kind the link drops) is dropped and counted, and 32 close it. The node connects again 1 to 4 s later, as after any close one forged frame can cause, and a link on the connection moves to the new one at once (NB135-8, NB135-I7): one forged frame, or 32, for each reconnect, as jamming would cost |
| DATA, further off | dropped; 4 in a row (ahead, or more than 64 behind) move the receiver to the last of them, with none of the 64 numbers behind it taken (NB135-7). The sender's real frames that land within those 64 are taken, and the one with its number is dropped; 4 further off move it back. Either way 4 real frames are lost at most, which the reliable lane sends again (NB135-1). A forger that goes on, one frame in the receiver's window after every 3 real ones, keeps that end out of step: its link times out and comes back on a new connection within seconds (NB135-I6) |
| DATA cut short, or a part of another value | the value dropped: its parts don't add up to its length |
| Anything from a group address, of another version or malformed | dropped |

For each of these the connection closes, loses frames, or stays as it was, and the node connects again. That is what
the tests show, frame by frame, not a proof that no mix of frames can stall a connection while the radio still has it
up. So the node has its own check too: when a link times out, the radio connections bound to its peer close, with no
hold on the advert, and the next link comes over a new connection with new ids (NB135-1); the other end, if it made
the connection, holds the advert only 1 to 4 s when it hears the close, as it does whenever a bound connection closes
(NB135-I6), so the link is back within seconds whichever end timed out first; so it does after any close one forged
frame can cause (NB135-8), and a link whose connection closed under it moves to the next one at once (NB135-I7).
Connections, queues and buffers are capped above, and the parser is fuzzed.

The security review of NB-135 (the [threat model](../project/threat-model.md)) found eight problems in four rounds,
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

And three smaller ones: a PROBE was answered every 100 ms even with an answer still waiting for the air (NB135-I5: now
one waits at most); the end that made a connection held the advert a minute when the other end closed it as its link
timed out (NB135-I6: a bound connection that closes now holds the advert 1 to 4 s, so a room member that keeps
closing them makes its peer connect again every 1 to 4 s, which is accepted); and a link whose connection closed
under it stayed on it until its session timed out, 30 s, so the next connection came within seconds but the link
didn't (NB135-I7: it is now dialled again over the next connection that proves its peer, at once).

What stays, as on any radio: an attacker in range can jam the channel, cut a connection by jamming the answer to a
PROBE, and hold a dead connection open with forged KEEPALIVEs until the node's own rules close it. Forged DATA frames
cost a connection a value or two each (4 frames for the 4 that move its numbers) if the forger then stops; one that
goes on, one frame for every 3 real ones, keeps the connection's numbers out of step for as long as it does, so the
link times out and comes back on a new connection within seconds, as jamming would. One forged DATA frame during the
room proof, or 32 once it is proven, close the connection, and the link is back on a new one within seconds (1.5 to
6 s on `FakeAir`), as after a cut. **The MAC address is a
residual, accepted (NB135-I3):** with `random_mac` a board is a new address on each boot, but one address for the
whole boot, so a listener can tie a boot's adverts together, and a network the board joins sees it too; without it,
the chip's own address follows the board for good.

## The driver

`peerwire::espnow::EspNow<E: Ether>` is the radio: the engine (`espnow::Engine`), driven by the time and by an
`Ether`, the platform's ESP-NOW (send a frame to an address or to all, the verdicts in order, the frames heard).
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

- **Several radios at once:** a node has one radio, so a board links over Bluetooth or over ESP-NOW. Both, and a path
  kind of ESP-NOW's own (`PathKind::EspNow`), need the core to take several.
- **Values over 512 bytes:** the node's framing keeps GATT's 512, so a v2 frame carries one value at most.
- **The rate:** frames go at ESP-NOW's default 1 Mbit/s, which carried 670 to 840 kbit/s on the NB-138 boards. At 54
  Mbit/s per peer (`esp_now_set_peer_rate_config`) they didn't link; a faster rate needs a way back to 1 Mbit/s, as
  a rate one end can't hear leaves a connection silent until it times out.
- **More on boards:** the probe kept the frame size, the rate and the timers (NB-138), and one run of `espnow_mesh`
  showed a link's heap, the receive queue full and the time a failed try holds the air (NB-139), from before the
  review's second round; 4 connections at once, and the fixes since, haven't run on boards.
