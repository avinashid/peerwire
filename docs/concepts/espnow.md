# ESP-NOW

> [!NOTE]
> This is the design of Peerwire over ESP-NOW (NB-135): boards linking with each other over Espressif's ESP-NOW
> frames, with no Wi-Fi network and no Bluetooth. The connection logic is the `peerwire` feature `espnow`
> (`espnow::Engine`, sans-IO, with host and fuzz tests); the ESP32 driver is `peerwire-esp` feature `espnow`, and its
> `espnow_mesh` example links two boards. The protocol below has not had its security review yet, and no board has
> run it: the probe (`espnow_probe`, NB-135) measures the radio first, and its results will set the defaults.

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
- **Scans:** a station that scans or joins hops across channels for a few seconds, and frames sent to it meanwhile
  aren't acknowledged; the radio tries each one 4 times and a connection survives 8 lost in a row, so a short scan
  costs some frames, not the connection.
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
  forged far ahead (or behind) moved the receiver there and the real ones are now far behind (or ahead) it (NB135-1).
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

`espnow::Settings` holds the numbers above (`Settings::DEFAULT`); a board run may change them.

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
most lately (`espnow::PeerTable`) and drops the oldest with no frame waiting for a new one (NB135-I2). ESP-NOW's own encryption (CCMP with a key per
peer, for at most 17 peers) isn't used: the node seals what crosses.

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
one way. At 54 Mbit/s the frames are 20 times shorter, and the waits between them dominate. The probe measures it.

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
| DATA, in the window | a value for the node, which drops it (sealed, or not the room's); the real frame with its number is dropped as a copy, and the value it is part of, and so is the value being joined if the forged frame says it starts one: a value or two lost, which the reliable lane sends again. The real frames behind it are still taken (NB135-6) |
| DATA, further off | dropped; 4 in a row (ahead, or more than 64 behind) move the receiver to them, and the sender's next 4 real frames, now far off, move it back: 4 frames lost, which the reliable lane sends again (NB135-1) |
| DATA cut short, or a part of another value | the value dropped: its parts don't add up to its length |
| Anything from a group address, of another version or malformed | dropped |

For each of these the connection closes, loses frames, or stays as it was, and the node connects again. That is what
the tests show, frame by frame, not a proof that no mix of frames can stall a connection while the radio still has it
up. So the node has its own check too: when a link times out, the radio connections bound to its peer close, with no
hold on the advert, and the next link comes over a new connection with new ids (NB135-1); the other end, if it made
the connection, holds the advert only 1 to 4 s when it hears the close, as it does whenever a bound connection closes
(NB135-I6), so the link is back within seconds whichever end timed out first. Connections, queues and buffers are
capped above, and the parser is fuzzed.

The security review of NB-135 (the [threat model](../project/threat-model.md)) found four problems, fixed since, each
with a test that was its repro:

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

What stays, as on any radio: an attacker in range can jam the channel, cut a connection by jamming the answer to a
PROBE, lose a few frames of a connection per forged frame, and hold a dead connection open with forged KEEPALIVEs
until the node's own rules close it. **The MAC address is a residual, accepted (NB135-I3):** with `random_mac` a board
is a new address on each boot, but one address for the whole boot, so a listener can tie a boot's adverts together,
and a network the board joins sees it too; without it, the chip's own address follows the board for good.

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
- **The rate:** frames go at ESP-NOW's default 1 Mbit/s; a faster one per peer (`esp_now_set_peer_rate_config`) waits
  for the probe's results on range and loss.
- **A board run:** the probe's results will set the frame size, the rate and the timers.
