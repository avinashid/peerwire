# Threat model

Scope of this document: `peerwire-core` (handshake, wire format, lanes, relay, NAT
traversal, STUN, invites, sealed messages), the C ABI (`peerwire-ffi`), shared docs in
Rust (`peerwire-sync`), the JavaScript glue and transport (`packages/js`), the React
hooks (`packages/react`), the Nostr, BroadcastChannel and LAN multicast (`LanSignaling`)
signaling adapters, the Bluetooth LE transport (`peerwire` features `ble` and
`ble-bluez`), the ESP32 prototype (`ports/esp32`), and the docs and demos site built for
GitHub Pages. It records what Peerwire defends against, what it does not, and the
findings of the first security review (M5, NB-013) and of every review since.

Status: pre-1.0, no external audit. See [SECURITY.md](../../SECURITY.md).

## What Peerwire protects

1. **Peer authentication.** A `PeerId` is an Ed25519 public key. A link is
   `Noise_IK_25519_ChaChaPoly_BLAKE2s`, whose static key is the X25519 form of
   that identity, and the initiator sends its Ed25519 PeerId inside HS1 bound to
   the handshake transcript. A completed handshake proves the remote holds the
   secret for the PeerId you dialled. No signaling relay, peer relay or NAT in
   the path can impersonate a peer or man-in-the-middle a link.
2. **Link confidentiality and integrity.** After the handshake, every data
   packet is AEAD-sealed (ChaCha20-Poly1305) under the session keys. A relay
   carrying a `Via` link sees only ciphertext.
3. **Replay of data packets.** Each link has a 2048-wide sliding nonce window
   (WireGuard-style); replayed or too-old data packets are dropped before
   decryption is trusted.
4. **Signaling payload secrecy.** Offers (which carry IP candidates) are sent as
   sealed messages (`Noise_X`): encrypted to the recipient PeerId and
   authenticating the sender's PeerId. A tampered or forged offer fails to open.

## Trust boundaries

| Boundary | Trusted? | Notes |
|---|---|---|
| Signaling relay (Nostr, BroadcastChannel) | No | May read, drop, reorder, replay or inject any signaling message. Cannot forge a sealed payload or impersonate a PeerId. |
| The LAN, for `LanSignaling` | No | Every device on the link, and every program on the machine, hears each frame and can send its own, with no relay to pace it. The same limits as a relay: cannot open or forge a sealed payload. See [LAN signalling](#lan-signalling-lansignaling-nb-064--nb64-1nb64-6). |
| Devices in Bluetooth range, for a node with a radio (`ble`, NB-110) | No | Hear every advert and can follow any connection, as nothing is paired; may connect, replay adverts, relay between two connections, jam or cut a link. Cannot prove the room without its name, open a sealed signal or forge a Noise packet. See [Bluetooth LE](#bluetooth-le-nb-110-nb-112--nb110-1nb110-12-nb112-1). |
| Peer relay (`FWD`/`RELAYED`) | No | Sees only ciphertext; rate-limited; forwards only to already-connected peers. |
| NAT / on-path network | No | Noise authenticates the endpoints; cannot MITM. Can drop/delay. |
| A TURN server the app lists (`iceServers`; in Rust `turn_servers`, NB-099) | Partially | The app's choice, not ours. Sees our address, the peers' addresses, sizes and timing, never a payload; can drop or delay; picks the relayed address we offer. Under `Privacy::RelayOnly` with `Turn` in the policy (NB-101) the peers see its relayed address instead of ours. See [TURN client in Rust](#turn-client-in-rust-nb-099--nb99-1-nb99-2) and [TURN under relay-only in Rust](#turn-under-relay-only-in-rust-nb-101--nb101-1). |
| Remote peer | Partially | Authenticated, but may be malicious: send malformed frames, flood, probe. Parsers and resource caps are the defence. |
| Host application / C caller | Yes | The C ABI trusts its caller to pass valid pointers and lengths; since NB-085 it refuses null ones (see SEC-8). |

## Attacker capabilities considered

- Off-path attacker who can spoof source addresses but not observe replies.
- On-path attacker who observes and modifies datagrams.
- A malicious or curious signaling relay, or any party who learns a room name or
  a target PeerId.
- A device on the same LAN segment, or a program on the same machine, when the node
  signals over `LanSignaling`.
- A device within Bluetooth range, when the node has a radio (`ble`, NB-110).
- A connected, authenticated peer behaving maliciously.

Out of scope: compromise of the host OS or its CSPRNG; side channels in the
underlying crypto crates (`snow`, `ed25519-dalek`, `k256`); traffic analysis by a
global passive adversary; malicious application code above the API.

---

## Findings

Severity uses likelihood × impact for this software's threat model. Each finding
that needs a change is tracked as an NB card.

**Status as of main `0af38e9` (2026-11-03): NB-085 (`ac842cc`) merged in `697dcae`,
NB-086 (`1f37042`) in `b2ae061`, NB-083 (`56309f9`) in `9665afb` and NB-084
(`fd2f246`) in `9c7789b`; the site was first deployed from `be989d4`.**
The JS and wasm fixes on this main are in `@peerwire/js` 0.2.2 (2026-11-03), and
those on main `806a048` (NB-097, NB-098) in `@peerwire/js` 0.3.0 (2026-11-04);
`@peerwire/react` is still 0.1.1. Fixes merged after that reach apps with the next
release of each.

| Finding | Status |
|---|---|
| SEC-1 (High) | **Fixed** in NB-021 (`ccc3b79`), with its review findings N21-1 and N21-2 |
| SEC-2 (Medium) | **Fixed** in JS by NB-022 (hello body encrypted to the room), and in Rust/Godot by NB-022b (below) |
| NB22-1 (Medium), NB22-2 (Low) | **Fixed** by `4e3c146`, merged `e205792`, before any native or Godot release (Rust/Godot hellos sealed as `nostr.js` seals them; `room.js` skips a hello marked `meta.nb`), reviewed 2026-10-29; residual NB22-I1 (Info, accepted; docs `494a400`, merged `9159b08`), [SEC-2](#sec-2--room-hello-discloses-the-long-term-peerid-in-cleartext-medium--nb-022) |
| SEC-3 (Medium) | Fixed for rooms (NB-031 Rust, NB-037 JS); open for a bare `WebRtcTransport` |
| SEC-4 (Medium) | **Fixed** by NB-024 (`85b8460`), merged `208659b` (under load a handshake from a UDP address takes no state until it echoes a cookie, and each source holds at most 4 half-open handshakes), reviewed 2026-10-29; residuals R1–R5, [SEC-4](#sec-4--half-open-handshake-table-exhaustion-medium--nb-024) |
| NB24-1 (Medium) | **Fixed** by `3e1cab3` (its type named in `ae34744`) before the merge `208659b`, so never on main (a handshake a UDP address took below the mark, which may be spoofed, isn't counted against that address), reviewed 2026-10-29; Info NB24-I1…I6 (accepted or optional; I3 fixed by NB-050), same section |
| SEC-5…SEC-7 | Open (NB-025…NB-027) |
| SEC-8 (Low) | **Fixed** by `aad222d` (NB-085), merged `697dcae` (every `pw_*` function refuses a null handle, a null output, or a null input with a length over 0, before it reads, writes or takes anything), reviewed 2026-11-03, [SEC-8](#sec-8--c-abi-trusts-caller-pointers-and-lengths-low-by-contract--nb-028) |
| SEC-9…SEC-11 | Met by NB-020/NB-030 and `docs/guides/godot.md` (Security); NB-029 audit and N29-1 fix |
| SEC-12 (Info) | Accepted, no card |
| SEC-13 (Low) | **Fixed** by NB-050 (`2f0c823`), merged `fe6854d` (an HS1's Noise part must be exactly 128 bytes, so a short copy of a half-open handshake's HS1 no longer draws its 57-byte HS2), reviewed 2026-10-30; residual NB50-I1 (Info, accepted), [SEC-13](#sec-13--a-short-copy-of-a-half-open-handshakes-hs1-reflects-its-hs2-low) |
| N38-1…N38-3 (port mapping) | **Fixed** in NB-038 (`71d83b2`) and NB-041 (`0dfdbc9`) |
| NPM-1 (Low) | **Fixed** by `4cffbf9` (merged `14f463a`): no port mapping under `RelayOnly` |
| NPM-2 (Low) | **Fixed** in `docs/guides/godot.md` (`9be92d5`); the flood risk itself is SEC-4 |
| NPM-3 (Low) | **Fixed** by `6f947f3`: a map request in flight at stop gets one delete |
| NB44-1 (Medium) | **Fixed** by `40e42db` (per-IP punch cap), re-checked 2026-10-23 ([below](#hard-nat-punching-nb-044-nb-045-nb-046-nb-048--nb44-1nb48-2)) |
| NB48-1 (Medium) | **Fixed** by `d1b2d25` (fleet checks one public address; Net Lab consent and cooldown), re-checked 2026-10-23 |
| NB48-2 (Low) | **Fixed** by `d1b2d25` (public spray IP) and `695b38b` (NB48b-3); the IP isn't matched to the peer's signalling (accepted) |
| NB44-2, NB44-3 (Low) | **Fixed** by `a16072e` (NB-044b: authenticated routes, fd reserve), re-checked 2026-10-23; residuals NB44c-1, NB44c-2 |
| NB48b-2 (Low) | **Fixed** by `de9efea` (the busy slot is held per peer), re-checked 2026-10-24 |
| NB48b-1, NB48b-3 (Low) | **Fixed** by `de9efea` (the answer is stripped) and `695b38b` (Chrome's line model, checked after apply), re-checked 2026-10-24 |
| NB48b-4, NB48b-7 (Low) | **Fixed** by `4b6c0f7`, `46a5da3` and `4c37bb7` (relay-only dials through a relay stay relayed, also when switched on mid-dial), re-checked 2026-10-24 |
| NB44c-1, NB44c-2 (Low) | **Fixed** by `8095325` and `7742801` (NB-085), merged `697dcae` (an extra birthday socket takes a route only from a packet of its own punch's peer that passed crypto here, and holds one route per peer), reviewed 2026-11-03, same section |
| NB85-1 (Low), NB85-I1 (Info) | **Fixed** by `df232ba` before NB-085 merged, so never on main (a copy of the peer's HS1, which Noise IK can't tell from a new one, routes only its answer until the socket's `until`; a relay's tag is a candidate only for a packet that authenticated), reviewed 2026-11-03; NB85-I2, NB85-I3 (Info) open, same section |
| NB85-2 (Low) | Open (on main, found in the NB-085 review): a copy of an HS1 a peer sent on an earlier link cancels our dial to it when our PeerId is the larger, same section |
| NB46-1 (Low) | **Fixed** by `851a0e2`, merged `bae16c7` (a peer's `restart` ask heeded only while our ICE is down too; at most 3 restarts per link a minute; offers, answers and their copies to one peer under a budget), reviewed 2026-10-26, same section |
| NB46-1a (Low), NB46-1b, NB46-1c, NB46-T1 (Info) | **Fixed** by `88bbf37`, merged `ee4cbe6` (an answer the budget refused holds its candidates; a copy of a new offer never gets the last answer; a refused restart is made when the window frees), reviewed 2026-10-26, same section |
| NB45-I1, NB44-I2, NB44b-I1 (Info) | Optional hardening or accepted, same section; NB44-I2 is still open after NB-085 |
| NB46-I1 (Info) | **Fixed** for port guesses by `0a8a796` (NB-084), merged `9c7789b` (a peer's prediction counts only at a public IPv4 its own server-reflexive candidates name, and a link adds at most 128 predicted candidates), reviewed 2026-11-03; residual: other remote candidates per link have no total cap, same section |
| NB48b-5, NB48b-6 (Info) | **Fixed** by `08425b8` (a stale link's close keeps the peer) and `2a00ecb` (CARRY replies bounded), re-checked 2026-10-24 |
| NB83-1 (Low) | **Fixed** by `188987b` before NB-083 merged, so never on main (merged `9665afb`): a node behind a NAT that maps per IP and port only sprays towards a `SymmetricIp` peer, at most 1024 probes with no sockets, so a try costs its NAT at most 1,026 ports, not 2,304; reviewed 2026-11-03; NB83-I1, NB83-I2 (Info) accepted, [Two-sided birthday punch](#two-sided-birthday-punch-nb-083--nb83-1) |
| NB84-1 (Medium) | **Fixed** by `fd2f246` before NB-084 merged, so never on main (merged `9c7789b`): a page's automatic birthday punches aim at most 2 fleets or sprays at any one IP per 5 min, whichever rooms and peers ask, and Net Lab's open global room runs none; reviewed 2026-11-03; residuals accepted; NB84-I1, NB84-I2 (Info) accepted, NB84-I3 fixed by `0af38e9`, [Automatic birthday punch](#automatic-birthday-punch-in-rooms-nb-084--nb84-1) |
| MIC-1, MIC-3 (Low) | **Fixed** by `d78d046`, `877d02c` and `6303bd8` (Net Lab's global room and punch offers; `public-only` in `@peerwire/js` 0.1.1 and earlier), reviewed 2026-10-24; residual MIC-4 ([below](#bounds-and-residuals-from-reviews-nb-014nb-043)) |
| MIC-2 (Info) | **Fixed** by `9282d36` (a punch that ended closes what it opened) and `691dec0` (one offer and one answer per punch, which also closes a Low NB48-1 bypass), reviewed 2026-10-24 |
| MIC-4 (Low) | **Fixed** by `134fed3` (the button rejoins before it re-tests the network), reviewed 2026-10-24 |
| MIC-5 (Info) | **Fixed** by `1385715`, merged `82be3bd` (an SDP with bare `\n` line ends is stripped too), reviewed 2026-10-26, [Addresses we give out](#bounds-and-residuals-from-reviews-nb-014nb-043) |
| FL1-1 (Info), FL1-2 (Low) | **Fixed** by `4b131ee` (at most one new connection per peer per 2 s from retry offers; only the dialling side acts on a `retry` ask), reviewed 2026-10-24, [WebRTC retries](#webrtc-retries-fast-lane-1--fl1-1-fl1-2) |
| FL2-1 (Low), FL2-2 (Low) | **Fixed** by `63e5b72` and `ddae245`, merged `8314aa8` (a member relay that stalls data hands the link back to the pipe and is held off; a datagram names the peer whose channel it came on), reviewed 2026-10-24; residuals R1, R2, [Member relays for pipe links](#member-relays-for-pipe-links-fast-lane-2--fl2-1-fl2-2) |
| FL2F-1 (Info) | **Fixed** by `0dcdff0`, merged `12fda2d` (a link roams from one member relay to another only through a member linked directly), reviewed 2026-10-26, same section |
| FL3-1 (Info), FL3-2, FL3-3 (Low), FL3-N1, FL3-D1 (Info) | **Fixed** by `475c442`, `8958a23`, `af58aee`, `e995440` and `f4929d9`, merged `3ce181e` (a packet sealed before a punch roams nothing; a slow address is never raced; a relay that stalls data on a link with no slow address is held off and the others raced; race scores saturate; relay docs), reviewed 2026-10-26; NB43-I1 narrowed on native UDP, [Latency races](#latency-races-fast-lane-3--fl3-1fl3-3) |
| RH-2 (Medium), RH-1, RH-3, RH-4 (Low) | **Fixed** by `78c4ebe` and `8126119`, merged `dec1554`, before `@peerwire/react` 0.1.0 was published (deep `accept` checks required, per-key clocks bounded by wall time, per-member quotas, catch-up by digest), reviewed 2026-10-24, [`@peerwire/react` hooks](#peerwirereact-hooks-nb-049--rh-1rh-6-nb53-1) |
| RH-5 (Low), catch-up message size (Low), RH-6 (Info), N1 (nit) | **Fixed** by `12974c4`, merged `41cc24a`, in `@peerwire/react` 0.1.1 (a write members' data leaves no room for is dropped with a warning; catch-up messages capped at 512 KiB; versions kept instead of entries; a newer version an asker holds isn't sent), reviewed 2026-10-26, same section |
| NB53-1 (Low), NB54-T1, NB54-I2 (Info) | **Fixed** by `0af9af3` and `5edfff0` (NB-054), merged `fd9d5fd` (a member's value nested past 64 is dropped before anything stringifies it, and ours throws; a catch-up entry that throws costs none of the others), reviewed 2026-10-30; residuals: apps on `@peerwire/react` 0.1.1 or earlier until they upgrade, NB53-I2 (Info, documented), same section |
| NB54-I1 (Info) | **Fixed** by `d6e66f3` (NB-056), merged `e43391c`, before either walk was published (the depth checks of the store and of `room.doc()` walk arrays by index and objects by their own keys, as `JSON.stringify` does: a 1 MiB array of numbers costs a tenth or less, and keys a page put on a prototype no longer count), reviewed 2026-10-30, same section |
| OUT-1, OUT-1a, OUT-1b (Low) | **Fixed** by `324cb8f` and `131f9f5`, merged `8271f1f` (a message over its lane's limit throws whatever the outbox holds; lanes are 0, 1 or 2 only; a presence is checked before it is stored), reviewed 2026-10-26, [JS endpoint and rooms](#bounds-and-residuals-from-reviews-nb-014nb-043) |
| OUT-1c (Low), OUT-1d (Info) | **Fixed** by `f5c1f45`, merged `f28f027` (a presence JSON can't encode throws `TypeError`), and `d29d200`, merged `7bf37e4` (`usePeerwireRoom` fails the join and leaves when that presence comes mid-join), reviewed 2026-10-26, same section |
| PBF-1, PBF-2 (Info) | **Fixed** by `344e90e`, merged `fc94833` (at most 16 last pipe batches at close; a test that a closed Nostr transport reads nothing), reviewed 2026-10-26, [Fallback pipes](#bounds-and-residuals-from-reviews-nb-014nb-043) |
| CHAT-1, CHAT-2 (Low), CHAT-3 (Info) | **Fixed** by `dfa8ea0`, merged `92dfa42` with the chat demo (a guessable room name is flagged; an oversized name or QR code fails visibly; the wording says messages may pass through a member or the relays), reviewed 2026-10-27, [Demos](#bounds-and-residuals-from-reviews-nb-014nb-043) |
| RX-1 (Low), RX-2 (Info) | **Fixed** by `b0d354b` (the README's React example keeps string messages only, and only the last 100), reviewed 2026-10-24, [Demos](#bounds-and-residuals-from-reviews-nb-014nb-043) |
| DOC-1 (Medium), DOC-2, DOC-3 (Low) | **Fixed** by `979cdae`, `eaa2a22` and `a4fddfd`, with DOC-2a and DOC-3a by `d6b148c`, merged `c30ab0f`, before `room.doc()` was published (stamps taken however far ahead; no entry on our own writer that we didn't write; a frame costs about one pass over what it names), reviewed 2026-10-27, [Shared docs](#shared-docs-roomdoc-nb-006--doc-1doc-7) |
| DOC-2b (Low) | **Fixed** by `8b7946c`, merged `45c7cfd` (a catch-up's deletions passed on one range per range of it, charged to the sender's budget), reviewed 2026-10-27; residuals DOC-2c and I6, same section |
| DOC-4, DOC-5 (Low) | **Fixed** by `f40cffa` (the merge of `ff51ce6`): a member over signalling is paused for a better routed one once per answer; a room with no doc open drops deflated doc frames unread. Reviewed 2026-10-28; residual I8, same section |
| DOC-6 (Medium) | **Fixed** by `0374cb8` (NB-051), merged `b968f8c` (a doc entry whose type isn't a string drops its frame, so a member can no longer put an entry in a named map or log that makes the app's own calls on it throw), reviewed 2026-10-30; residual R1 (accepted), I9, I10 (Info), same section |
| DOC-7 (Info) | **Fixed** by `0358c1d` (NB-053), merged `907b771`, before `room.doc()` was published (a frame value's depth is checked before `JSON.stringify`, which recurses on Node 22, so a value 100000 deep is dropped, not thrown, also in a copy on this device or the relays), reviewed 2026-10-30; NB53-I1 (Info, the test's FAIL line) fixed by `8be53e8`, merged `6e998db`, same section |
| NB6F-1, NB6F-2, NB6F-3 (Low) | **Fixed** by `2e71dc5`, merged `8140998`, before `persist` was published (an opening that couldn't read the copy never writes it; one save at a time, paced at 256 KiB/s; a 5 s load deadline). The seal is security's call, in `52bb7e3`. Reviewed 2026-10-28; residuals R1–R4, [Docs kept on this device](#docs-kept-on-this-device-persist-nb-006f--nb6f-1nb6f-3) |
| NB6F-2a (Info) | **Fixed** by `b45e15c`, merged `76bdd64` (each retry of a store that keeps failing waits twice as long, 1 s up to 64 s, until a save succeeds), reviewed 2026-10-29, same section |
| I3a (Info), I4 (accepted) | **Fixed** by `1b0fc35` (the `VERSION` comment covers a copy read only in part; `close()` and `leave()` say they wait for the store), same section |
| I2, fixed further, NB77-I1 (Info) | **Fixed** by `a29ce7c` (NB-077), merged `45a793b` (a save on a connection the browser closed before its close event came is tried once more on a new one, 250 ms later; a request that fails no longer leaves the save hanging), and NB77-I1 by `1fc707a` (NB-078), merged `9f165e6` (an open the browser aborts, as one made at that close event, is tried once more, 250 ms later), reviewed 2026-11-03, same section |
| NB6H-1 (Low), NB6H-2, I1, I2 (Info) | **Fixed** by `c1b3878`, merged `71f5c6e`, with NB6H-2's first half by `dc156a3` (relay pacing on a monotonic clock, with clamps; the pipe asks `wait()` again each second; a held relay is sent nothing at close; `wait?()` typed). Reviewed 2026-10-29, [Nostr relay pacing](#nostr-relay-pacing-and-pipe-striping-nb-006h-nb-006i--nb6h-1-nb6h-2-nb6i-1) |
| NB6I-1 (Low) | **Fixed** by `dc156a3`, merged `6a60ef0` (a relay slowed behind its queue gives up its turn), beside the lead's close fix `cf4a0a9`. Reviewed 2026-10-29; a relay that answers OK and delivers nothing gets 1/n of the pipe (accepted), same section |
| Half-open relay (Low) | **Fixed** by `659eb28`, merged `5ae3c4e` (NB-006j: a relay that has answered nothing for 5 s since it was sent an event gets no pipe turns while one that answers is left; its next OK gives them back), reviewed 2026-10-29; residuals NB6J-I1, I3, I4 (Info, accepted), same section |
| NB6J-I2 (Info) | **Fixed** by `0d40053` and `f4a89a4` (NB-006k), merged `b26e869` (a socket that has heard nothing from its relay for 10 s after a non-lossy event is redialled, then after 20 s and 30 s, and no more until it answers in time; the new socket takes no pipe batch until its relay answers), reviewed 2026-10-31; NB6K-I1…I3 (Info) fixed by `d76ed0b` (NB-058), merged `2f68aa8`, [Nostr relay pacing](#nostr-relay-pacing-and-pipe-striping-nb-006h-nb-006i--nb6h-1-nb6h-2-nb6i-1) |
| Reconnect flap (Low) | **Fixed** by `df01209` (NB-058), merged `2f68aa8` (a relay socket that closes soon after it opens no longer gives its relay's reconnect wait back: the wait starts over only after a socket stayed open 30 s, so a relay that drops each connection is dialled, and the room says hello, at most twice a minute), reviewed 2026-10-31; residuals accepted, same section |
| NB58-1 (Low), NB58-I1 (Info) | **Fixed** by `4609955` before NB-058 merged, so never on main (a `navigator.connection` 'change' with the same connection type no longer redials the relays; the docs say "dialled 30 s ago" where that is what counts), reviewed 2026-10-31, same section |
| NB6L-1 (Low), NB6L-2…NB6L-6 (Info) | **Fixed** by `564e68a` and `8d6d24a`, merged `55b47c5` with the relay copy (NB-006l), before `relays: true` was published (a save whose read no relay completed stays due; a `t: 5` naming a save already made, or from a member now ignored, holds nothing back; a relay can't date `sync.at` ahead; a piece holds at most 16 frames), reviewed 2026-10-30; residuals accepted, [Docs kept on the relays](#docs-kept-on-the-signalling-relays-relays-nb-006l--nb6l-1nb6l-6) |
| NB6B-1 (Low), NB6B-3 (Info) | **Fixed** by `834ef90` and `4d6f9a2`, merged `6c5af8f` with `peerwire-sync` (NB-006b), before any native or Godot release (a Godot host charges what it passes on to the joiner writing, and cuts that joiner, not its readers; the host's relay is fuzzed), reviewed 2026-10-30; NB6B-2 (Info) accepted, [Shared docs in Rust](#shared-docs-in-rust-peerwire-sync-nb-006b-nb-052--nb6b-1nb6b-4-nb52-1-nb52-2) |
| NB6B-4 (Low) | **Fixed** by NB-052 (`1113cac`, before any native or Godot release; merged in `143923f`): a joiner's queue past 16 MiB is dropped and the joiner caught up afresh, not cut, so a flood no longer cuts readers. Reviewed 2026-10-31; residuals (1), (2), (3), (5) accepted, same section |
| NB52-1, NB52-2 (Low), NB52-I1, NB52-I2 (Info) | **Fixed** by `3749a67`, `ba693d0`, `1113cac` and `da4fe8b` before NB-052 merged, so never on main (a catch-up afresh keeps the joiner's budgets; our replies to its asks for docs not open go again; one that alone queues over 8 MiB cuts the joiner), reviewed 2026-10-31, same section |
| NB19-1, NB19-3, NB19-4, NB19-5 (Low), NB19-I1, NB19-I2 (Info) | **Fixed** by `5a3fb65` and `c173890` before NB-019 merged, so never on main, before `openInbox` was published (a read opens each content once, whichever relay states its id; JSON nested past 64 is dropped; the envelope names the app; every read asks `until`; listeners get the message's id; `since` is rounded down to 10 min), reviewed 2026-10-31, [Offline messages](#offline-messages-to-a-peerid-openinbox-nb-019--nb19-1nb19-5) |
| NB19-2 (Low) | Accepted (the lead's call, documented in the README and `index.d.ts`): 100 drops or more in a message's second, ordered before it, hide it on that relay, same section |
| NB10-1, NB10-2 (Low), NB10-I1, NB10-I2 (Info) | **Fixed** by `c578263` and `41f4021` before NB-010 merged, so never on main (merged `869e4cf`), before `room.call()` was published (a member's `full` turns a joiner away only when the joiner's own roster has no seat for it either; 'stream' comes for every peer once its description applies, whatever it sends; a description with more than 64 candidates is dropped; the docs say call frames on a pipe go inside it), reviewed 2026-10-31; NB10-I1a, NB10-I3 (Info) and the residuals accepted, [Calls](#calls-roomcall-nb-010-nb-010a--nb10-1-nb10-2-nb10a-1) |
| NB10a-1 (Low) | **Fixed** by `f979384` before NB-010a merged, so never on main (merged `4f681b4`): in the meet page, a camera or microphone request still pending at Leave is turned off when it lands, reviewed 2026-10-31, same section |
| NB16-1 (Low) | **Fixed** by `6107674`, merged `888e0d6`, before the site was ever deployed (the Pages build job runs no third-party action), reviewed 2026-10-31; NB16-I1, I2 (Info) open, NB16-I3 done when Pages was enabled, [The docs and demos site](#the-docs-and-demos-site-nb-016-nb-067--nb16-1-nb16-2-nb67-1) |
| The shared origin (Low) | Accepted by the repo owner on 2026-11-01: the site is served from `avinashid.github.io` with no origin of its own. The meet page stays off the site, so NB10a-I1 doesn't arise; it joins only with the owner's approval and a frame check, same section |
| NB16-2 (Low) | Accepted by the repo owner on 2026-11-01 (NB-071): Net Lab keeps its microphone button on the site, and on a `*.github.io` host its note says, before and after the grant, that the permission covers the whole origin, so the account's other Pages sites could use the microphone, and see the device's local addresses, without asking (`1d434cb`, and `8f4a635` for the addresses, NB71-I1), reviewed 2026-11-01, same section |
| NB67-1 (Low), NB67-I3, NB67-I4 (Info) | **Fixed** by `41c91ef`, `a7da8c2` and `4386eec` before NB-066 merged, so never on main (an SVG under `docs/assets/` may hold only 20 drawing elements, SVG's and xlink's namespaces and printable ASCII, with no prefix, `&#`, processing instruction, backslash, or `//` but in the `xmlns`), reviewed 2026-11-01; NB67-I1, I2 (Info) fixed by NB-073, next row, same section |
| NB67-I1, NB67-I2 (Info) | **Fixed** by `1f2a492` and `9d3396d` (NB-073): the edit link takes the doc's path through `encodeURI`, then escaped, and every link to a page is escaped; a symlink under `docs/assets/`, or `docs/assets` one or under one, fails the build. Reviewed 2026-11-01; NB73-I1, NB73-I2 (Info) fixed by NB-082, next row, same section |
| NB73-I1, NB73-I2 (Info) | **Fixed** by `b3a569c` and `06623cd` (NB-082): a symlink among the files the build reads or publishes (`docs/README.md` and each page it lists, all of `docs/`, the demo pages and their modules, the repo's wasm, `site.css`), or a directory on their way from the repo's root that is one, fails the build, and a doc's path must be `[\w-]` names and slashes, with `.md`. The site built from main is unchanged. Reviewed 2026-11-03 |
| NB64-1, NB64-2 (Low), NB64-3, NB64-4, NB64-5 (Info) | **Fixed** by `a0fe2b1` and `4c86af7` before NB-064 merged, so never on main (merged `34a35a8`): `LanSignaling` takes only datagrams sent to its group, except on Windows; an inbox message spends a token before it is opened; `lan_chat` escapes member text; the docs say the ESP32 seed fixes the identity only, and what a replayed hello does. Reviewed 2026-11-01; residuals accepted, [LAN signalling](#lan-signalling-lansignaling-nb-064--nb64-1nb64-6) |
| NB64-6 (Low) | Open (NB-065): a neighbour who knows two members' PeerIds can keep them from linking by flooding their inbox topics, same section |
| NB92-1, NB92-2 (Low), NB92-I1 (Info) | **Fixed** by `5ef2482` before NB-092 merged, so never on main (`broadcast()` refuses the device's own addresses, and the ESP32 firmware adds no leg on a /31 or /32; `new()` sends only to the broadcast address of the interface the group goes through, if private; the docs say a frame published twice within a second is heard once), reviewed 2026-11-03; NB92-I2 (Info) accepted, [The broadcast leg](#nb-092--the-broadcast-leg--nb92-1nb92-3) |
| NB92-3 (Low) | **Fixed** by `63f639d` before NB-092 merged, so never on main (on Linux and Android each leg's socket is bound to the interface whose subnet holds its address, so the device's other networks can't send frames to the LAN's broadcast address), reviewed 2026-11-03; residuals accepted, same section |
| NB62-1 (Info) | **Fixed** by `4bbde33`, merged `34a35a8` (the ESP-IDF comment says a socket bound to any interface offers loopback only, and the port binds the chip's own address), [The ESP32 prototype](#the-esp32-prototype-nb-062-nb-064--nb62-1-nb62-2) |
| NB62-2 (Info in the emulator, High on a board) | **Fixed** on a board by `c5e13b7` (NB-091), merged `4697a77`: the firmware starts Wi-Fi, with modem sleep off, before any random draw, reviewed 2026-11-03; in QEMU the draws still repeat on every chip (accepted), and the order is the firmware's, not the library's; NB-095's relay carrier draws after Wi-Fi too, reviewed 2026-11-04, same section |
| NB95-1 (Medium on a board), NB95-2, NB95-3, NB95-4 (Low), NB95-5, NB95-I1 (Info) | **Fixed** by `0b909e3` and `d5c2cfb` before NB-095 merged: on ESP-IDF a relay message is at most 4 KiB and the inbox 16 KiB, and on every target a message of more than 64 `[`, `{` and `,` is dropped unparsed; relay writes time out after 5 s; the relay thread runs at the main task's priority; the firmware goes on the relay only in a room its flash names, random by default; the log escapes members' bytes; NB95-I1 is documented (its fix is SEC-5's), reviewed 2026-11-04, [Several carriers](#several-carriers-and-the-esp32-on-nostr-nb-095--nb95-1nb95-6) |
| NB95-1a, NB95-6 (Low) | **Fixed** by `51d3ba9` and `a777178` before NB-095 merged: a relay message with more than 2 `{` or 8 `[` is dropped unparsed too; the firmware forgets the address of a peer that left, reviewed 2026-11-04, same section |
| NB95-I2 (Info) | **Fixed** by `4c71c67` (NB-103, merged `9b3959b`): the firmware never starts lwIP's SNTP; its own ask, at boot and every 6 hours, takes only an answer from the address and port asked that echoes its 8 random bytes, refuses leap indicator 3 and stratum 0 or 16 and up, and reads era 1, reviewed 2026-11-05; one answer still sets the clock, so the server asked or anyone on the path can, same section |
| NB103-I1, NB103-I2 (Info) | **Fixed** by `c0e0ffc` and `e663830` (peerwire-esp 0.1.1): `ntp::time` refuses an answer whose seconds fall in 1968–1969 with a checked subtraction; a board whose clock no server set before its Node was bound restarts once a later ask sets it, so the Node binds again with the time, reviewed 2026-11-05; with ESP-IDF's default time keeping the time survives the restart, so a board restarts for its clock at most once per power-on, same section |
| NB98-1, NB98-2 (Low) | **Fixed** by `3e055f1` before NB-098 merged, so never on main (a call's connections take a policy set live, restart ICE on our side when it turns `'direct'` or `'turn'` off or brings new servers, and fail `'no-path'` if still on a path now off 20 s later; relay urls are parsed with `new URL()`, and every check in `setPolicy` comes before the policy is set), reviewed 2026-11-04; residuals accepted, [Connection policy](#connection-policy-nb-098--nb98-1-nb98-2) |
| NB98-I1, NB98-I2, NB98-I3, NB98-I4 (Info) | NB98-I1, NB98-I3 **fixed** by `3e055f1` (a link dropped as it came up leaves no proof deadline; without `'direct'` links and calls send relay candidates alone, with no related address and no port prediction, whatever WebRTC gathers); NB98-I4 fixed by it on the dialling side, while the answering side's kept link keeps its old session until the dialler restarts; NB98-I2 documented (an order binds the dialling side only), same section |
| NB97-1 (Low) | **Fixed** by `a00fff5` before NB-097 merged, so never on main (the node drops what came in with a link it refuses, and holds a new link's `AddrObserved`, and the `Replaced` of the link it replaces, until its `Connected` says whether it is admitted), reviewed 2026-11-04, [Connection policy in Rust](#connection-policy-in-rust-nb-097--nb97-1) |
| NB97-I1, NB97-I2, NB97-I3, NB97-I4 (Info) | NB97-I1, NB97-I2 **fixed** by `a00fff5` (a refused replacement ends the old link with `PathOff`; a link that moves onto a kept relay stops punching, and the answer to an earlier punch moves it nowhere); NB97-I3 accepted (a Rust policy hides no address, the guide says so); NB97-I4 **fixed** by `9131188` (a refused link is closed at the end of the pass, so an admitted link of the same peer that replaced it in the same read is kept), reviewed 2026-11-04, same section |
| NB79-I1 (Info) | Open, a design call: on IPv6, `'public-only'` still gives members the device's own address (the docs say so since `9ea2200`), [Addresses we give out](#bounds-and-residuals-from-reviews-nb-014nb-043) |
| NB86-I1 (Info) | Accepted: since NB-086 (`b2ae061`), a peer can make each link hold about 5.7 MB of received data, not 2.3 MB, on every node; 256 links of `peerwire-peer` can pin about 1.6 GB (the always-on peer guide says so), [Core sessions](#bounds-and-residuals-from-reviews-nb-014nb-043) |
| NB99-1 (Medium), NB99-2 (Low), NB99-I1 (Info) | **Fixed** by `b2b2919` and `41471eb` before NB-099 merged, so never on main (a relayed address an offer names relabels only the packets that name its claimant's link, and the policy takes it as TURN's for that peer alone; full TURN tables make room for a later peer, a peer holds one offer's IPs, channel numbers are bound again once quiet), reviewed 2026-11-05; residuals accepted, [TURN client in Rust](#turn-client-in-rust-nb-099--nb99-1-nb99-2) |
| NB99-I2 (Info) | **Fixed** by `1551da7` (NB-100, merged with NB-101), reviewed 2026-11-05: every claimant of a relayed address is kept, and a claim relabels its claimant's packets alone; the 256-claim cap stays, [TURN client in Rust](#turn-client-in-rust-nb-099--nb99-1-nb99-2) |
| NB101-1 (Low), NB101-I1 (Info) | **Fixed** by `ee3ef67` and `016c77f` before NB-101 merged, so never on main (under relay-only with TURN, a relay peer's invite is dialled through the allocation; a link that shows the node's address goes onto an allocation at any of the peer's addresses, or closes without a Close and is dialled afresh through the relay), reviewed 2026-11-05; residuals accepted, [TURN under relay-only in Rust](#turn-under-relay-only-in-rust-nb-101--nb101-1) |
| NB101-I2 (Info) | **Fixed** by `807d66a` (NB-108), reviewed 2026-11-05: `hide()` retags a link a policy change is moving onto an allocation, or closes it, before its `moving` guard, then leaves it to that move, which closes it if it fails or is late, so nothing more of the link leaves the socket, same section |
| NB110-1, NB110-2, NB110-3 (Low) | **Fixed** by `7ae7ac8`, `4cb542f`, `fee4639`, `ebf3530` and `33b1f4f` before NB-110 merged (`74c7452`), so never on main, before `ble` was published (a connection no peer is bound to takes only an HS1, the answer to a dial over it and data of a session on it; a newcomer closes the weakest unbound connection, and standbys leave one slot free; new handshakes on unbound connections are budgeted per connection and across the radio), reviewed 2026-11-05; residual: Bluetooth can hold 4 of an ESP32-C3's 8 half-open handshakes, [Bluetooth LE](#bluetooth-le-nb-110-nb-112--nb110-1nb110-12-nb112-1) |
| NB110-4, NB110-5 (Low) | **Fixed** on the node's side by `ec79f1a`, `fee4639` and `4cb542f` before NB-110 merged (the address, `n` and tag change together; the GAP name is `Peerwire`, and `BlueZ::names_this_host` says when Linux's is the host name), reviewed 2026-11-05; on Linux BlueZ picks the address and the name: accepted as documented, same section |
| NB110-6…NB110-9 (Low) | **Fixed** by `33b1f4f`, `287ebd8` and `d202fd3` before NB-110 merged (only a `PeerId` proven on the connection, or a session bound to it, counts for anything; the BlueZ driver serves the subscriber BlueZ hands over and takes its writes alone; a refused central tries again in 1–4 s; a central the advert doesn't count makes way), reviewed 2026-11-05; NB110-8 has one BlueZ behaviour to check on a radio, same section |
| NB110-10 (Low) | **Fixed** by `b95141b`, merged `6efda8b`, before `ble` was published (a central holds an advert by the device it came from as well as its `n`, so a replayed copy holds itself alone), reviewed 2026-11-05, same section |
| NB110-11 (Low) | **Fixed** by `708038f`, merged `51b4e8f`, before `ble` was published (a connect not up within 12 s is given up as a failed one and holds that device's advert; connects under way count against the node's own connects alone; the BlueZ driver gives its setup 10 s and disconnects on a failure), reviewed 2026-11-05; residual: a replayed advert heard first costs each race it wins 10 s, the driver's own deadline, since NB-112 (`86fee9d`), same section |
| NB110-12 (Low), NB110-T1 (Info) | **Fixed** by `86fee9d` (NB-112), before `ble-bluez` was published, reviewed 2026-11-05: the BlueZ driver's command loop awaits no disconnect (each runs in a task of its own, the served central's task drops its link, a failed connect is reported first), and a test pins a connect under way against the connection limit, same section |
| NB112-1 (Low), NB112-I1, NB112-I2 (Info) | **Fixed** by `680e273` before NB-112 merged, so never on main (the BlueZ scan never drops a device that has advertised the service for another, and lists BlueZ's devices again, at most every 30 s, for those it left out; connects the driver gives up when it leaves an adapter are stopped at BlueZ; the driver powers an adapter on only at start), reviewed 2026-11-05; residual: while a burst of service adverts holds all 256 slots, a member that arrives is reported once, then not followed, same section |
| NB110-I1 (Info) | Open (on main, over UDP): on the ESP32 port one source that knows the node's PeerId fills all 8 half-open handshake slots, 10 s at a time, same section |
| NB110-I2…NB110-I10 (Info) | NB110-I3 **fixed** by `33b1f4f`, and NB110-I10's first edge by `708038f`; I2, I4 documented; I5, I6, I8 accepted; I7, I9 and I10's second edge open, same section |
| BlueZ 5.87 (NB-113) | Not Peerwire's: `bluetoothd` 5.87 crashes when a UUID discovery filter meets an advert that lists a UUID; fixed upstream by `82af2beaf`, in no BlueZ release yet. The Linux driver's scan set such a filter until NB-112 (`bb4857c`) dropped it, same section |
| Earlier reviews' bounds and residuals | Recorded in [Bounds from reviews](#bounds-and-residuals-from-reviews-nb-014nb-043) |

### SEC-1 — Unvalidated peer-id length corrupts the wasm heap (High) — NB-021

**Status: fixed** in NB-021 (merged `ccc3b79`). Every fixed-size copy into wasm
memory checks its length first (`peerBytes`, `exact`), every variable-size one
takes bytes only (`bytesOf`), and the signaling adapters (`nostr.js`, `webrtc.js`,
`local.js`, `room.js`) drop a `from` that is not 64 lowercase hex characters. The
reproduction below now throws a `TypeError` and the module stays healthy
(`packages/js/test/peer_id.mjs`, in the gate). The C ABI peer-length argument is
a separate defence-in-depth card.

`packages/js/peerwire.js` `Endpoint.setPeer(id)` copies `id` into the fixed 32-byte
`peerBuf` with `mem().set(..., this.peerBuf)` and performs no length check;
`seal()` and `open()`/`put()` have the same shape. An over-long id writes past the
buffer into adjacent wasm linear memory, corrupting other endpoint state and the
allocator; the module then traps (`unreachable`) or behaves unpredictably.

This is reachable from untrusted input: the demos and `webrtc.js` pass a
signaling peer identifier (`from` of a Nostr/BroadcastChannel `hello`, or a `to`
for `seal`) straight into `dial()` → `seal()` → `setPeer()`. A hostile relay or
peer that publishes a `hello` with an over-long `from` can drive the overflow in
any client that auto-dials newcomers (as `demo/demo.js` and `demo/arena.js` do).

**Reproduction.** Load the wasm, call `ep.seal('ab'.repeat(4096), ...)`: the copy
runs past `peerBuf` and a subsequent `nb.endpoint()` allocation traps the module.

**Fix.** Validate length at the boundary before every wasm copy:
- `setPeer`/`seal`/`open`: require a 32-byte id (or a 64-hex-char string) and
  reject otherwise;
- in `nostr.js`/`webrtc.js`, validate that a signaling `from`/`to` is 64 lowercase
  hex chars before using it as a PeerId.
Defence in depth: the C ABI could take an explicit peer-length argument, but the
JS-side check is the necessary fix.

The review of the fix found two more unbounded inputs, both fixed in the same
merge:

- **N21-1 (Medium) — fixed.** `webrtc.js` passed DataChannel messages to
  `new Uint8Array(e.data)`. `binaryType = 'arraybuffer'` covers binary messages
  only, so a peer could send a *string* such as `"2147483648"`, which became a
  length: one 10-byte message made the tab allocate 2 GB and aborted the module
  in `pw_alloc` (`"100000000"` grew wasm memory to ~101 MB, never returned). It
  needed only an open DataChannel, before the Noise handshake. Now `onmessage`
  accepts only an `ArrayBuffer` of at most `MAX_DATAGRAM` (1232) bytes, and
  `Endpoint.receive` drops anything larger before allocating, which also covers
  `local.js` and custom transports.
- **N21-2 (Low) — fixed.** `nostr.js` decoded and opened event content of any
  size, so a hostile relay could push multi-MB events, each costing about twice
  its size in wasm memory. Content that is not a string or is over 64 KiB is now
  skipped before decoding.

### SEC-2 — Room `hello` discloses the long-term PeerId in cleartext (Medium) — NB-022

`nostr.js` `hello()` publishes `{ t:'hello', from:<full PeerId>, meta }` as plain
base64 under a tag derived only from the room name. Anyone who knows the room name
(the room tag is `sha256("peerwire/room/<room>")`, computable by anyone with the
name) can read every member's long-term Ed25519 PeerId, correlate it with the
ephemeral Nostr pubkey, the `created_at` timestamp and the relay set, and track
that identity across rooms and time. This defeats the "fresh key per session"
unlinkability the adapter otherwise aims for.

**Fix.** Don't broadcast the raw PeerId in a room-wide, unencrypted hello. Options,
in order of preference: (a) gossip presence only to peers already known
out-of-band and seal it; (b) include only an ephemeral, per-session handle in the
hello and exchange the real PeerId inside the sealed channel; (c) at minimum
document that room membership reveals every member's identity to anyone who knows
the room name, so room names must be treated as secrets.

**Status: fixed, in JS (NB-022) and in Rust/Godot (NB-022b).** Two earlier steps
narrowed it: NB-031 (Rust) and NB-037 (JS) derive a room key, separate from the
topic, from the name, and a hello is acted on only with its HMAC proof, so someone
without the name can no longer get dialled. NB-022 then encrypts the JS hello body
(AES-GCM, key `sha256("peerwire/room-enc/<room>")`, random 12-byte nonce), so a
relay or kind-25059 subscriber without the room name can't read the PeerId. It
still sees that a hello was posted (room tag, a body whose length shows its kind,
NB22-I1), the ephemeral pubkey and `created_at`, and via the inbox tag (SEC-5) a
stable per-PeerId handle. Freshness comes from the NB-037 proof (`meta.ts`, 60 s): a
replayed hello is refused once it is older than that. The residual is inherent: a
peer that holds the room name (a legitimate member, or anyone who learns it) can read
and forge hellos and learns the PeerIds of everyone in the room, the same bar as
joining. Option (c) is also done: the docs say private room names need at least 128
random bits (`randomRoomId()`), and a guessable name is a public lobby.

**NB22-1 — Rust/Godot hellos were plaintext (Medium).** Godot `join_room` uses the
Rust `NostrSignaling` with the default public relays, so every native or Godot room
member published its PeerId in the clear to any kind-25059 subscriber. **Fixed** by
`4e3c146` (NB-022b), merged `e205792`, before any native or Godot release:
- `traversal.rs` seals a hello byte for byte as `nostr.js` does: a 12-byte nonce,
  then AES-256-GCM (no AAD) under `sha256("peerwire/room-enc/<app>/<room>")`.
- A hello that doesn't open is dropped before it is parsed: plaintext, another
  room's, altered or cut. One longer than the longest sealed hello is refused before
  decrypting.
- A cross-stack vector made by `sealRoom` is pinned in both stacks
  (`hello_seal_matches_nostr_js`, `packages/js/test/rust_hello.mjs`).
- `aes-gcm` is a dependency of the `peerwire` crate only; the wasm is unchanged.

Native nodes from before `4e3c146` and after it don't find each other through room
hellos. No native release has shipped, so this goes in the first one's notes.

**NB22-2 — The native-hello marker (Low).** JS and Rust dropped each other's hellos
only because JS couldn't open a plaintext one: Rust marks its hello `meta.nb` while
`room.js` checked `meta.pw`, so once Rust sealed its hellos, a browser would have
dialled native members over WebRTC. **Fixed** by `4e3c146`: `room.js` skips a hello
marked `meta.nb` or `meta.pw`, and a test feeds it Rust's sealed hello.

**NB22-I1 — A sealed hello's length shows what kind of member sent it (Info,
accepted).** A native hello is 316 base64 characters (320 for a `client`), a
browser's 304 (308). So a relay can tell a native member from a browser and a client
from a host or peer, but not who it is. The fix, if it is ever wanted, is to pad both
stacks' hellos to one length. Documented in NAT-TRAVERSAL and the README by
`494a400`, merged `9159b08`.

### SEC-3 — Sealed-offer probe reveals IP candidates to anyone who knows your PeerId (Medium) — NB-023

Because a valid offer only requires the target's PeerId (a public value), any
party who learns a PeerId can seal an offer to it over a public relay and receive
a WebRTC *answer* — which contains the target's ICE candidates (local and
server-reflexive IP addresses). The Noise handshake still protects against
impersonation, but the metadata (presence + IP) leaks before any mutual
authentication of intent. Combined with SEC-2, a passive observer of a room can
deanonymize every member's IP.

**Fix.** Treat an inbound offer as a request, not a command: rate-limit answers
per source, and gate candidate disclosure on some proof the peer belongs in the
room (a room-scoped shared secret, or an allow-list the app supplies). Document
the residual "known PeerId ⇒ discoverable IP" exposure for apps that publish
PeerIds widely.

**Status: fixed for rooms, open elsewhere (NB-023).** In Rust (NB-031), room
offers are acted on only with a verified room-key MAC, and a plain offer only
from a peer we invited. In JS (NB-037), `joinRoom` gives `WebRtcTransport` an
`offerProof`, and an offer without a valid room proof gets no answer, so no
candidates. A `WebRtcTransport` built directly without `offerProof` still answers
any sealed offer, so the finding stays open for that use.

### SEC-4 — Half-open handshake table exhaustion (Medium) — NB-024

`Config::max_pending_handshakes` (default 4096) caps half-open responder links,
and the cap is checked before the expensive Noise build, so CPU per flood packet
is bounded once the table is full and memory is bounded (~a few MB, entries expire
after `handshake_timeout`, 10 s). However, there is no address-validation cookie:
an attacker sending ~4096 HS1s per 10 s with spoofed or varied `(sender_idx,
ephemeral)` keeps the table saturated, so legitimate inbound handshakes are
dropped for the duration — a connection-establishment denial of service against a
public/always-on peer. HS2 is roughly HS1-sized, so reflection amplification is
~1×.

**Fix (hardening).** Add a stateless retry/cookie step (QUIC Retry / WireGuard
cookie style): on the first HS1 from an unvalidated address, reply with a keyed
cookie and allocate no state until the address echoes it. This moves the cost to
address-spoofers and keeps the table for validated peers. Until then, operators of
always-on peers should size `max_pending_handshakes` and `handshake_timeout` for
their exposure.

**Status: fixed** by NB-024 (`85b8460`, with NB24-1's fix `3e1cab3`), merged
`208659b`, reviewed 2026-10-29. Below three quarters of `max_pending_handshakes`
(the mark: 3072 of the default 4096) a handshake is an HS1 and an HS2, byte for byte
as before. From the mark the endpoint is under load:
- An HS1 from a UDP address (`handle_datagram`) takes no state and costs no DH. It
  is answered with a 21-byte `COOKIE`: HMAC-BLAKE2s over its index, its ephemeral
  key and the address, cut to 16 bytes. Only an `HS1C` that echoes it from that
  address goes on to the DH. The secret is new every 120 s, and a cookie holds for
  the rest of its epoch and the next.
- Each source holds at most 4 half-open handshakes. A source is an IPv4 address,
  an IPv6 /64, a relay, or any other address (a channel). A handshake on a peer's
  own channel (`handle_datagram_from`) or through a relay needs no cookie, since
  neither can be spoofed, but it counts against its source.
- So a flood from spoofed addresses fills the table to the mark at most, and a real
  dial takes one round trip more. With 5000 valid HS1s a second from spoofed
  addresses for 25 s, a dial from a real address links within 20 ms; before NB-024
  it failed after 23.5 s. Test:
  `a_dial_links_through_a_flood_of_handshakes_from_spoofed_addresses`.
- A dialler takes a cookie only for its own index, from an address it dials, on a
  direct path, and sends its HS1 there again with it. A cookie is unauthenticated,
  so `handle_datagram_direct` says false for one.

**NB24-1 — Handshakes spoofed below the mark used up a source's quota (Medium).**
Found in the review of `354ed36`, before the merge. Every half-open handshake was
counted against its source, also one that a UDP address took below the mark with no
cookie, whose address may be spoofed. So a flood could send 4 of its HS1s from a
victim's IPv4 address (any port) or /64 while it brought the endpoint to the mark.
The victim's own dial was then refused at the per-source cap, cookie and all, for
as long as the flood kept the endpoint loaded. **Fixed** by `3e1cab3` (the entry's
type named in `ae34744`): a handshake counts against its source only if the source
is known, that is, if it was taken under load (with a cookie), on a peer's channel
or through a relay. Tests: `handshakes_spoofed_below_the_mark_dont_fill_a_source`
(the reproduction) and `a_channel_or_a_relay_counts_what_it_took_below_the_mark`.

Residuals (accepted):
- **R1, many real addresses.** 256 IPv4 addresses or IPv6 /64s, at 4 each, fill
  the 1024 slots above the mark (default sizes). A dial then waits for entries to
  expire. SECURITY.md says so; see NB24-I1 for what 256 /64s cost.
- **R2, old diallers.** A dialler from before NB-024 ignores a `COOKIE`, so it
  can't reach a responder at or above the mark. Below the mark nothing changed. It
  affects native UDP only: JS transports pass the peer (`handle_datagram_from`), and
  a channel needs no cookie.
- **R3, forged cookies.** An on-path attacker, or one who guesses a dial's 32-bit
  index and spoofs the responder's address, can hand the dialler a false cookie. A
  responder with NB-024 answers the bad `HS1C` with a new cookie, so it costs a
  round trip. One without it drops `HS1C`, so that dial fails, which an on-path
  attacker could do anyway by dropping packets.
- **R4, below the mark.** A spoofed HS1 still costs the responder its DH and a
  half-open entry, as before NB-024, up to the mark.
- **R5, fingerprinting.** A loaded endpoint answers an HS1-shaped packet with a
  `COOKIE` before the DH. That shows a Peerwire endpoint listens on the port, but
  not its PeerId. Since NB-050 (`2f0c823`, merged `fe6854d`) the probe must be as
  long as a whole HS1, 133 bytes (149 as an `HS1C`); any other length is dropped as
  malformed, where before any HS1 of 37 bytes or more drew a `COOKIE`. Loading it
  already takes 3072 valid HS1s to that PeerId within `handshake_timeout`.

Info from the review; none was needed for the merge:
- **NB24-I1 — On IPv6, R1 is one /56 (Info, optional).** 256 /64s are a single
  /56, and many ISPs and hosts give one customer a /56 or a /48. Optional fix: a
  second cap per /56 or /48, as nested rate limiters have.
- **NB24-I2 — Shared sources (Info, accepted).** Under load, everyone behind one
  source shares its 4 slots. A CGNAT address is one source, as in WireGuard. So is
  a relay: a relayed packet arrives as `Path::Via(relay)` and carries no origin.
  While the endpoint is loaded, a peer linked to relay `r` can take `r`'s 4 slots
  and keep out others who reach it through `r`.
- **NB24-I3 — A cookie reflects at most 0.16× (Info, fixed by NB-050).** Before
  NB-050 the smallest HS1 that got a cookie was 37 bytes (a 5-byte header and 32
  bytes of noise), for a 21-byte `COOKIE` (0.57×), and SEC-13 sent a half-open
  handshake's HS2 again for 37 bytes (1.54×). Since NB-050 (`2f0c823`, merged
  `fe6854d`) only an HS1 of exactly IK msg1's length is answered: 133 bytes, or 149
  as an `HS1C`. So a `COOKIE` reflects 0.16× at most, and an HS2, new or sent again,
  0.43× (NB50-I1). (An endpoint that is itself dialling the sender answers that
  peer's valid HS1 with its own: 133 bytes, or 149 as an `HS1C` to an address it
  dials and holds a cookie for.) This also narrows R5.
- **NB24-I4 — Hosts with `u32` handles (Info, accepted).** A host that calls
  `pw_handle_datagram`, or `Peerwire.receive` without a peer, gets one source per
  handle, since a handle is no IP address. A UDP host that made one handle per IP
  address and port would give each port its own quota. No host in the repo does:
  every JS transport passes the peer.
- **NB24-I5 — Guessed and sprayed ports (Info, accepted).** A dialler takes a
  cookie only from an address it dials. So a loaded responder reached only at a
  guessed or sprayed port can't be linked until it drops below the mark. Taking
  cookies from anywhere would widen what an unauthenticated cookie can do.
- **NB24-I6 — Tiny caps (Info, optional).** With `max_pending_handshakes` below 4,
  the mark is the cap. At 0, every HS1 gets a cookie that can never be used, where
  before NB-024 it got no answer. Optional fix: send no cookie at the cap.

### SEC-5 — Deterministic, unsalted Nostr peer tag enables presence correlation (Low–Medium) — NB-025

The per-peer tag is `sha256("peerwire/peer/<PeerId>")`, a fixed function of the
public PeerId. Anyone who knows a target's PeerId can compute the tag and
subscribe to it on the public relays to learn whenever that identity is addressed,
and to correlate activity across sessions and rooms. (SEC-2 hands out the PeerIds
to room-name holders; even without it, PeerIds are shared in invites.) Rust's inbox
topic (`Topic::peer`) is the same hash, and over `LanSignaling` it is in clear in every
offer's frame, so every device on the LAN can do this, not only the relays
([LAN signalling](#lan-signalling-lansignaling-nb-064--nb64-1nb64-6)).

**Fix.** Derive the routing tag from a rotating, room- or epoch-scoped secret
rather than from the bare PeerId, so the tag is unlinkable to anyone who only
knows the public PeerId. Accept that two peers must share that secret (they share
a room already).

### SEC-6 — Sealed signaling messages have no replay protection (Low) — NB-026

`sealed::open` intentionally provides no anti-replay (documented in `sealed.rs`:
"callers that care put a timestamp or nonce inside"). `nostr.js` adds none beyond
a per-session `seen` set of event ids, which does not survive a reconnect and does
not stop a relay from re-delivering an old sealed offer with a fresh event id. An
observer can replay a captured sealed offer later; the effect is limited (it
triggers a redundant answer / ICE churn, feeding SEC-3) but non-zero. A re-sent
retry offer (`retry: true`, Fast lane 1) also closes the recipient's pending connection
to that peer, one that never opened, and so cancels an upgrade; the fallback link
carries on. In Rust a room hello is taken again within 60 s of its time, which on
`LanSignaling` any device on the LAN can replay; offers carry a nonce and a time
([LAN signalling](#lan-signalling-lansignaling-nb-064--nb64-1nb64-6), Replays).

**Fix.** Put a `created_at`/nonce inside the sealed body and reject stale or
duplicate (sender, nonce) at the adapter. The Nostr `created_at` is unauthenticated
(it is outside the sealed blob) and must not be trusted for this.

### SEC-7 — No in-session rekeying / forward secrecy refresh (Low) — NB-027

A link keeps one set of transport keys for its whole lifetime (nonce is a u64
counter; exhaustion is not a practical concern). There is no periodic rekey, so
compromise of a live session's keys exposes all traffic on that session, past
(if recorded) and future, until it is torn down. A reconnect performs a fresh
handshake with new ephemeral keys, which bounds exposure in practice.

**Fix (future).** Offer an optional rekey (new ephemeral DH) on a byte/time
threshold for long-lived sessions. Document the current property meanwhile.

### SEC-8 — C ABI trusts caller pointers and lengths (Low, by contract) — NB-028

Several `pw_*` functions dereference the endpoint handle and in/out buffers
without null or length checks (e.g. `pw_secret`, `pw_send`, `pw_stats`), and
`bytes(ptr, len)` with a null `ptr` and non-zero `len` is undefined behaviour.
This is the documented C-ABI contract and the JS glue always upholds it, but a
native (Dart/Swift/C) caller that violates it gets memory unsafety rather than an
error code.

**Fix.** Document the contract explicitly in the header/readme, and add null
checks on the handle and on buffers where `len > 0` as cheap defence in depth.

**Status: fixed** by `aad222d` (NB-085), merged `697dcae`, reviewed 2026-11-03. Every
`pw_*` function in `lib.rs` and `sync.rs` checks its handle, each output (`writable`)
and each input (`bytes` returns an `Option`) before it reads, writes or takes
anything. A null handle, a null output, or a null input with a length over 0 gets
`PW_ERR_INVALID`; functions that return nothing ignore the call, as they already did
a null peer id; `pw_is_connected` returns 0 and the `i64` getters −1. `pw_secret` and
`pw_stats` now return `int32_t` (an ABI change for native callers; JS ignores it),
and `pw_stats` and `pw_poll_transmit`'s `out_addr` write at any alignment. `pw_seal`
checks `MAX_SEALED` before it adds the overhead, so a huge `len` can't overflow. The
contract is in `docs/reference/c-abi.md`. Test: `null_pointers_are_refused`
(`lib.rs`, `sync.rs`). Residual (by contract): a pointer that isn't null must still
point at `len` valid bytes, which no check can tell.

### SEC-12 — An invite holder can confirm a guessed link by naming it as a relay (Info) — no card

Since NB-033 an invite holder the node never dialled learns none of its linked
PeerIds. But relay fallback dials a waiting sender `Via` every relay it listed
that the node is linked to, when the offer arrives (`on_offer`) or when that link
comes up later (`handle_event`, `Connected`). So a holder can list a guessed
PeerId X as a relay: if a relayed handshake from the node arrives through X, the
node is linked to X.

This predates NB-033 (it only extends the check to links that come up later) and
is inherent to relay fallback. It is bounded: at most 8 guesses (`MAX_RELAYS`) per
offer acted on, under the offer rate limits; each guess answers only "linked to X
or not" and enumerates nothing; and X must actually relay to the holder, so X has
to be linked to it. No fix planned; a stronger design (naming only links that
agreed to relay) would be a separate card.

### SEC-13 — A short copy of a half-open handshake's HS1 reflects its HS2 (Low)

Found on 2026-10-30 by fuzzing a loaded endpoint, in the pass after NB-024; it
predates NB-024 (it was there before the rename, `27a69c9`). `on_hs1` first looks
an HS1 up by its index and ephemeral key, before any crypto, and if a half-open
handshake has them it sends that handshake's HS2 again to wherever the packet came
from. The lookup needs only those 36 bytes, and the length check before it only 32
bytes of noise. So a 37-byte packet (the kind byte, the index and the ephemeral key)
draws the 57-byte HS2, every time, from any source address: 1.54× in bytes, one
packet for one. It runs before the cookie and the cap, so it works at any load.

- Reproduction: an attacker sends the endpoint one valid HS1 of its own (it needs
  only the endpoint's PeerId; under load, also a cookie at a real address of its
  own). For the next `handshake_timeout` (10 s), each 37-byte packet with that HS1's
  index and ephemeral key, from a victim's spoofed address, sends the victim the
  57-byte HS2. Another valid HS1 renews it.
- Impact: a small reflector for anyone who knows a PeerId whose node is reachable
  over UDP (native, Godot, `peerwire-peer`). A peer's own channel
  (`handle_datagram_from`) answers only that peer. It is the same early return as
  NB44c-1, whose fix (report true only after crypto) doesn't change this.
- Fix: count an HS1 whose Noise part isn't exactly 128 bytes as malformed. IK
  message 1 with the 32-byte PeerId payload is always 32 + 48 + 48 bytes, and a
  payload of any other length is refused already. The smallest packet answered is
  then 133 bytes: 0.43× for an HS2, 0.16× for a `COOKIE`. That is NB24-I3's
  optional fix too, and fewer probes could fingerprint a loaded endpoint (SEC-4
  R5).

**Status: fixed** by NB-050 (`2f0c823`), merged `fe6854d`, reviewed 2026-10-30.
`on_hs1` counts an HS1 or `HS1C` whose Noise part isn't exactly `wire::HS1_NOISE`
(128) bytes as malformed, before the lookup, on every path (UDP, a peer's channel,
a relay). Every HS1 we send carries a 32-byte PeerId, so it is always 128 bytes of
noise: no correct peer is refused, and the handshake bytes are unchanged. Tests:
`only_a_whole_hs1_draws_its_hs2_again` (a half-open handshake's HS1 cut to 37
bytes, a byte short or a byte long, as an HS1 or an `HS1C`, draws nothing from
another address and counts as malformed; the whole HS1 still draws its HS2 again),
and `fuzz_handshakes_under_load`, which found this, now checks that no answer is
larger than its datagram, with no exception.

Residual (accepted):
- **NB50-I1 — A whole-length copy still draws the HS2 again (Info).** The lookup
  still runs before any crypto. A 133-byte HS1 (or a 149-byte `HS1C`) with a
  half-open handshake's index and ephemeral key, whatever its other 96 bytes, draws
  that handshake's 57-byte HS2 from any address while it is half-open (up to
  `handshake_timeout`): 0.43× (0.38× as an `HS1C`), one packet for one, so it
  amplifies nothing. Matching the rest against the stored HS1 wouldn't help: the
  attacker who sent that HS1 holds all of it.

### Router port mapping (NB-038, Godot switch) — N38-1…N38-3, NPM-1…NPM-3

`Builder::port_mapping(true)` (Godot: `set_port_mapping(true)`), off by default,
asks the LAN gateway (PCP, then NAT-PMP, then UPnP-IGD) to forward the node's UDP
port. The mapped address is offered like a public one. This changes the attack
surface in three ways:

- **Unsolicited reachability.** While mapped, anyone on the internet can send to
  the port, with no NAT filtering in front. It reaches only the Noise endpoint.
  Under a handshake flood that endpoint takes no state for spoofed handshakes
  (SEC-4, fixed by NB-024), but an attacker with many real addresses can still
  keep others from connecting (SEC-4 R1). It also shows up in port scans.
  `docs/guides/godot.md` says this and advises asking the player first (NPM-2).
- **The gateway and the LAN are trusted for the address only.** Any device on the
  LAN can answer the requests. A rogue responder can report a false address,
  which peers would then dial. It can't get into a session: no one finishes a
  handshake without the peer's key. Accepted residual.
- **Leftover mappings.** After a crash the router keeps the mapping until its
  lease ends (2 h), or until someone deletes it if the router only allows
  permanent UPnP leases. Documented.

Findings:

- **N38-1 (fixed, NB-038 `5b52eb5`).** A dripping or stalled UPnP gateway could
  hold the mapping thread and the delete on drop. Now each HTTP exchange has one
  3 s deadline, and drop waits at most 7 s.
- **N38-2 (fixed, NB-038 `5b52eb5`).** A PCP or NAT-PMP answer with lifetime 0
  was taken as a permanent mapping; it is now a refusal.
- **N38-3 (fixed, NB-041 `bf769e9`).** A UPnP chain stopped half-way kept
  sending; the worker now checks stop before each HTTP exchange.
- **NPM-1 (Low, fixed by `4cffbf9`, merged `14f463a`).** Under `Privacy::RelayOnly` the mapped
  address is never given out, yet the port was still opened. Now a relay-only
  node asks the router nothing (state `RelayOnly`). Switching to relay-only
  deletes the mapping without blocking, and switching out maps again after the
  old delete has gone out. A Godot relay-only host still maps, because it gives
  out its public addresses as under `PublicOnly`.
- **NPM-2 (Low, fixed in `docs/guides/godot.md`, `9be92d5`).** The doc said the port
  "reaches only the Noise endpoint" without the SEC-4 flood caveat.
- **NPM-3 (Low, fixed by `6f947f3`).** `Mapper::stop()` deleted only a lease it
  held. A PCP or NAT-PMP map request still unanswered when the worker stopped
  (drop right after bind, or `set_privacy(RelayOnly)` right after a map request)
  got no delete, so the router's late grant stayed open for its lease (2 h). Now
  a stop with no lease and a map request out sends one lifetime-0 delete: PCP
  with the same nonce and no suggested address (RFC 6887 §15), NAT-PMP with
  external port 0 (RFC 6886 §3.4). With a lease held, only `release()` deletes.

### Hard-NAT punching (NB-044, NB-045, NB-046, NB-048) — NB44-1…NB48-2

Reviewed on 2026-10-23: native `backend/NB-045-lockstep-prediction` (`3a6c8b8`,
NB-044 birthday punch and NB-045 lockstep) and browser
`ui/NB-048-browser-birthday` (`5ee351e`, NB-046 ICE restart and port prediction,
NB-047 Net Lab rooms, NB-048 browser birthday punch). Mechanisms and costs are in
`docs/concepts/nat-traversal.md` (Birthday punching, Lockstep prediction, Reflection).

These features make a node send many packets to an address a remote party names.
The security question is who can name that address and how much traffic follows.

| Trigger | Who can trigger it | Traffic per try | Limits |
|---|---|---|---|
| Native spray (an offer saying `Symmetric` with no prediction, to a cone node) | A room member, an invite holder, or a peer the node dialled | Up to 2048 HS1s of 133 B to ports ≥ 1024 of the offer's first address (public IPv4): 272,916 B over 3.8 s | 5 min per peer; per node, a burst of 4, then 1 per 30 s |
| Native open (an offer saying `Cone`, to a random-port symmetric node) | Same | 256 sockets for 10 s, each sending 1 HS1 to the offer's address (about 34 KB) | Same budget; at most 512 extra sockets open at once |
| Native lockstep sweep (both sides predict) | Same | Up to 96 HS1s (about 13 KB) to predicted ports of the offer's IP, and up to 96 of the node's own NAT ports | Same budget |
| Browser port guesses (NB-046) | A linked or dialling WebRTC peer | 32 extra remote candidates per new `{ ip, next, step }` | Chrome's ICE pacing (about 16 checks/s per connection) |
| Browser fleet and spray (NB-048, Net Lab; in rooms with no click since NB-084, [below](#automatic-birthday-punch-in-rooms-nb-084--nb84-1)) | Any member of the Net Lab room, including the open global room | Fleet: 256 connections checking every candidate in the peer's offer. Spray: 1024 checks to the IP the peer names | 70 s per run, no cooldown |

The core stayed sans-IO and deterministic: `Endpoint::spray`, `sweep` and `opener`
use the endpoint's seeded RNG and the `now` the host passes in. Sockets, the 5 ms
poll and routing live in `peerwire::Node`. `fuzz.rs` checks that a spray visits each
port at most once and respects `MAX_SPRAY`.

**NB44-1 — The birthday spray is a reflection vector with about 270× byte
amplification (Medium).** A sealed offer costs the sender well under 1 KB. When the
node acts on it, the node sends 272,916 B to ports of an IP the sender chose. The IP
only has to be public IPv4 and dialable, and it isn't checked against anything the
peer proved. The 5-minute cooldown is per PeerId, and a room-key holder can make up
any number of PeerIds, so only the node budget limits it: 4 sprays, all at one IP if
the sender wishes.

- Repro (unit test on the branch, `punch_case`): 4 offers from 4 PeerIds naming
  `198.51.100.9` make one cone node send 8,208 datagrams (1,091,664 B) to that IP
  within 6 s; 8 PeerIds send no more (budget).
- Sustained, the limit is about 9 KB/s per node, towards any victims. Every cone
  member of a public room can be recruited, one offer each.
- Fix: a cooldown per destination IP across all PeerIds (for example, one planned
  punch per IP at a time and at most 2 per 5 min), so all four sprays can't land on
  one victim. Correct the "no amplification beyond the count" line in
  NAT-TRAVERSAL.md. Test: 4 PeerIds offering one IP start 1 spray.
- **Status: fixed** by `40e42db` (NB-044b). `start_plans` allows one punch per
  destination IP at a time (20 s) and at most 2 per 5 min across all PeerIds. The
  check runs before the node token, and covers spray, sweep and open. Re-check probe:
  4 or 30 PeerIds offering one IP every 1 s or every 20 s for 15 min. Per 5-min
  window, that IP got at most 4,096 spray probes (2 sprays, 545,832 B) from a cone
  node, or 2 fleets (512 HS1s) from a symmetric one. Before the fix it was about 14
  sprays (3.8 MB) per 5 min.
- Residual: the cap is per node. The first spray is still about 270× the offer, and
  N room members recruited with one offer each still reflect N times the
  per-node bound at one victim. Recruiting needs the room key or an invite.

**NB44b-I1 — The per-IP cap can be spent by a room member (Info, accepted).** A
member can name a real peer's public IP in offers from minted PeerIds, which uses
up that IP's 2 slots. Birthday punches towards it then stop for 5 min and fall back
to the relay. Two genuine peers behind one carrier-NAT IP get at most 2 punches per
5 min between them. A plan the cap blocks is dropped, not deferred, until that
peer's next offer. These are availability costs only, and the cap is worth them.

**NB44-2 — Extra sockets take any sender as a route, before authentication
(Low).** `Node::read_extra` makes the first extra socket that hears from an address
that address's route, and refreshes the socket's `last_recv` on every datagram. It
does this before `Endpoint::handle_datagram` has authenticated anything.

- Repro (node test on the branch): junk from about 57,000 loopback source ports to
  one extra socket left 24,496 entries in `routes`. The socket stayed open 1.2 s past
  `until`, and stays open for as long as junk keeps arriving.
- Who can reach an extra socket:
  - the offer's address, through a port-restricted NAT;
  - any host on the LAN;
  - the internet, if the node is bound dual-stack on a host with global IPv6 and
    no inbound firewall (the ports can be found by a UDP scan).
- A sender that can spoof the source address of a peer the node already talks to
  can move the node's egress to that peer onto the extra socket, which breaks the
  path through the peer's NAT.
- The punch target alone gets an HS1 from every socket's NAT mapping. One datagram
  per socket per minute (under `ROUTE_IDLE`) then keeps all of them open
  indefinitely: up to 512 fds (NB44-3) and the 5 ms poll (NB44-I2) for as long as it
  likes. This is why NB44-2 should land before native builds ship.
- Fix: set a route only for an address the endpoint has accepted, that is a session
  path or dial target equal to `from` after `handle_datagram`. Refresh `last_recv`
  only from that address, and cap `routes` at `MAX_EXTRA_SOCKETS`. Test: junk from N
  ports leaves `routes` empty, and the sockets close at `until`.
- **Status: fixed** by `a16072e` (NB-044b). `read_extra` adopts a route, and refreshes
  `last_recv`, only when `Endpoint::handle_datagram_direct` reports that the datagram
  reached a session whose direct path is `from`; `routes` is capped at `MAX_ROUTES`
  (512). Re-check: junk, a bad-tag Data packet, or anything from another address
  makes no route (test `birthday_sockets_ignore_unauthenticated_floods`), and a
  punched link still adopts its route. Residuals: NB44c-1 and NB44c-2 below.

**NB44c-1 — Two unauthenticated packets still count as "authenticated on this
path" (Low).** `on_packet` returns the session index from two early returns that
run no crypto: an HS1 matching `half_open` by (sender, ephemeral), and an HS2 for a
link that is already live. So `handle_datagram_direct` reports true for either one
when it comes from the peer's current direct address.

- Repro (two in-memory endpoints, linked): an HS2 of `0x12`, any sender, our receiver
  index and 48 junk bytes, from the peer's address, gives true, and `rx_auth_failed`
  does not move. Replaying the peer's HS1 from its address while the link is
  half-open also gives true. The wrong index or another address gives false.
- It needs the peer's exact address and either our receiver index (in clear in every
  Data packet to us) or a captured HS1: an on-path observer, or a spoofer who has
  seen one packet. If such a packet reaches an extra socket, the peer's address
  becomes a route there (our egress to it moves) and the socket is held another
  `ROUTE_IDLE` each time.
- Fix: report true only when this datagram passed crypto (set a flag after
  `read_message` in `on_hs1`, `on_hs2` and `on_data`; the duplicate returns leave it
  unset). Test: both packets above give false.
- **Status: fixed** by `8095325` (NB-085), merged `697dcae`, reviewed 2026-11-03.
  `on_packet` says how a packet authenticated, and the two early returns say it
  didn't, so `handle_datagram_direct` names a peer only for a packet whose crypto ran
  here (since `df232ba` it returns `Option<(PeerId, bool)>`, the bool meaning fresh:
  NB85-1). Test: `direct_datagrams_name_a_peer_only_once_authenticated` (both packets
  above give `None`).

**NB44c-2 — Any linked identity can hold extra sockets open (Low).** `open_extra`
does not record `Birthday.peer`, so a valid handshake from any identity that knows
our PeerId and can reach an extra socket makes a route, and the socket stays open
while that session sends once a minute. `on_data` also moves a session's path on
every authenticated packet from a new source port, so one linked peer cycling
through N source ports holds N sockets (up to 512) and the 5 ms poll (NB44-I2).

- Repro (loopback node test): node A opens 4 extra sockets for a punch to T, and an
  unrelated node C links to one. One route, and the socket is still open after
  `until`.
- The NB44-3 reserve keeps the process 64 fds clear, so the cost is held fds and
  wake-ups, not `EMFILE`.
- Fix: keep the punch's peer in `Extra`, have `handle_datagram_direct` return the
  authenticated session's `PeerId`, adopt a route only for that peer, and keep one
  route per peer. Fold NB44-I2 in: drop the 5 ms poll once every open extra socket
  is a route. Card NB-044c.
- **Status: fixed** by `8095325` and `7742801` (NB-085), merged `697dcae`, reviewed
  2026-11-03. `Extra` keeps the punch's peer (`Birthday::peer`), and `read_extra`
  adopts or refreshes a route only for an authenticated packet of that peer. A new
  route for the peer at another address ends its old one, whose socket then closes at
  its `until`, so a peer cycling source ports holds one socket. Tests:
  `birthday_sockets_take_only_their_peer_authenticated` (a junk HS2 from the peer's
  address and a stranger's handshake make no route, and the sockets close at `until`)
  and `birthday_sockets_keep_one_route_per_peer` (another peer's route is kept). The
  5 ms poll wasn't folded in: NB44-I2 stays open.

**NB85-1 — A copy of the punched peer's HS1 still counted as authenticated (Low).**
Found in the NB-085 review (`7b0f776`), with the lead. Noise IK's first message has
no freshness check. Once the link is up, `announce` forgets its half-open entry, so a
copy of the peer's HS1 runs the DH again, passes, and makes an unannounced responder
session whose path is the copy's sender. With `8095325`, that made a route and held a
punch socket past `until` (60 s a copy, and a copy works every 10 s); with
`7742801`, it also ended the peer's real route.
- Repro (loopback node test, scratch, on `7b0f776`): a is linked with b and opens 4
  sockets for it, and b's link moves through `socks[0]`. Host m sends a copy of b's
  HS1 to `socks[1]`: a's route for b moves to m, m gets the 57-byte HS2 from
  `socks[1]`, and after `until`, `socks[0]` is closed and `socks[1]` open. Behind a
  NAT, a's packets to b then leave by a socket b's NAT drops.
- It needs a copy of an HS1 b sent to a (on the path, or a member that relayed b's
  dial) and a way into an extra socket, as NB44-2 does.
- Fix: tell an HS1, which a copy can repeat, from an HS2 that finishes our dial or a
  Data packet, which a copy can't; let an HS1 route only its answer.
- **Status: fixed** by `df232ba` before NB-085 merged, so never on main, reviewed
  2026-11-03. The core marks an HS1 on either path `Copyable`, only an HS2 that
  finished our dial or Data after the replay check and the AEAD open `Fresh`, and the
  two early returns `No`. `read_extra` lets a packet that isn't fresh only route its
  answer, through its address's open route if it has one: it sets no `last_recv` and
  ends no other route, so its socket closes at `until`. Tests:
  `birthday_sockets_route_a_copied_hs1_only_until_until` and
  `birthday_sockets_keep_the_route_of_a_copied_hs1_from_the_peer` (the probe, from m
  and spoofed from b's address); the core test above checks the fresh flag, the copy
  after the link is up, a simultaneous open and Data sent again.
- Residual: a copy's route holds no socket, and each distinct copy works once per
  10 s, as its own half-open entry matches the next. See NB85-I3.

**NB85-I1 — A relayed packet's tag was a candidate before it authenticated (Info).**
`on_data`'s relay loop added the origin tag of an inner packet as a candidate whether
or not that packet authenticated, so a junk HS2 relayed with a tag could make us
punch at the tag's address; nobody there can answer without the link key. **Fixed**
by `df232ba`: the tag counts only for `Copyable` or `Fresh`, so a relayed dial's HS1
still adds its tag (test `a_relayed_packet_adds_its_tag_only_once_authenticated`).
The relay chooses the tag, as before.

**NB85-I2 — The simulator still uses the route rule from before NB44-2 (Info, test
gap).** `Sim::open_ports` makes an extra port a route on any datagram, so the
birthday simulator tests don't measure the node's rule; only loopback node tests do,
and loopback has no NAT filtering. Fix: adopt a route in the simulator only where
`handle_datagram_direct` names the punch's peer, split as `read_extra` splits it, and
rerun the birthday tests against the numbers in `docs/concepts/nat-traversal.md`.

**NB85-I3 — A copied HS1 sent from another peer's address moves that peer's egress
(Info).** The route added for a packet that isn't fresh steers everything a sends to
`from` through the punch socket, not only the HS2. If `from` is the direct path of
another peer c, linked on a's main socket, a's packets to c leave b's punch socket
until its `until` (10 s); behind a port-restricted NAT, c's NAT drops them, then the
link recovers.
- Repro (loopback node test, scratch, on `ac842cc`): c's socket sends b's HS1 to
  `socks[1]`. c gets the HS2 and a's next Data from `socks[1]`; after `until`, Data
  leaves the main socket again.
- It needs a copy of b's HS1, an open punch socket reachable through a's NAT, and
  c's exact address spoofed, inside the 10 s. Before NB-085 the same input held the
  socket 60 s and refreshed it.
- Fix: skip the route when `from` is already a live link's direct path (node only).

**NB85-2 — A copy of a peer's old HS1 cancels our dial to it (Low).** Found in the
NB-085 review; on main before it. In a simultaneous open, the side with the larger
PeerId drops its own dial for an HS1 that passes crypto (`on_hs1`). Noise IK's first
message has no freshness, so a copy of an HS1 the peer sent on an earlier link passes
too, once its half-open entry is gone.
- Repro (core test, scratch, on `7b0f776`): b, the smaller PeerId, dials a; the link
  comes up and both disconnect. At 20 s a dials b, and a copy of b's old HS1 reaches
  a first, from another address. a answers there and drops its dial, b's answer to
  that dial meets an unknown session, and the two don't link within 30 s.
- It needs a copy of an HS1 the peer sent us: an on-path host, or a member that
  relayed the peer's dial, which sees the inner HS1 and, linked to us, can send the
  copy itself. A copy works once per 10 s and never expires, as it rests on static
  keys. Impact: availability only; a dial from the larger PeerId fails until the node
  dials again.
- Fix: in a simultaneous open, keep our dial until the session the peer's HS1 made is
  confirmed by its first Data, then drop it (no wire change); or put a timestamp in
  HS1 and refuse one no newer than the last (a wire change). Test: the repro links
  within a round trip of the copy.

**NB44-3 — Local resources a remote peer can spend (Low).**
- **File descriptors.** An offer saying `Cone` makes a random-port symmetric node
  open 256 sockets for 10 s, up to 512 at once. The default soft `RLIMIT_NOFILE` is
  256 for macOS apps (the launchd default) and 1024 on Linux desktops.
  `open_extra` stops at the first failed bind, so the process can sit at its limit
  for 10 s: other opens in the host
  fail with `EMFILE` (a Godot resource load, the Nostr websocket reconnect), and the
  punch silently opens fewer sockets.
- **Ports.** Each try takes 256 ports on a carrier NAT, and many carrier NATs give a
  subscriber only 512 to 2048. The NAT keeps them for its UDP timeout after the
  sockets close.
- **Connection tracking.** A spray adds up to 2048 entries to the cone side's
  router (already in NAT-TRAVERSAL.md).
- Fix:
  - On the first failed bind, close a reserve (for example 64 of the sockets just
    opened).
  - Run one open at a time (256).
  - Give open plans their own budget of 1 per 2 min, longer than common UDP mapping
    timeouts.
  - The NB44-1 per-IP cooldown covers repeats.
  - Document in GODOT.md that macOS hosts (and mobile ones, check the limit) should
    raise `RLIMIT_NOFILE`.
- Found by reading the code; no probe was run.
- **Status: fixed** (file descriptors) by `a16072e` (NB-044b). A failed bind closes up
  to `FD_RESERVE` (64) of the sockets that punch opened, and GODOT.md has the
  `RLIMIT_NOFILE` note. Re-check under `ulimit -n 256`: a punch opened 188 sockets
  and left 64 fds free, a second punch at the same time still left 64, the main
  socket kept working, and every extra socket closed at `until` with nothing leaked
  (the same at 128: 60 opened, 64 free). At 256 a punch gets fewer sockets, so it
  may not link. Ports and connection tracking stay bounded by `MAX_EXTRA_SOCKETS`,
  the NB44-1 per-IP cap and the node's punch bucket (accepted).

**NB45-I1 — Sweep ports can be below 1024 (Info).** The sender's prediction (`next`,
`step`) chooses the ports of a lockstep sweep and of port guesses. Unlike the spray,
they aren't kept at 1024 and up, so 96 HS1s can reach well-known ports (53, 123,
161) of the offered IP. An HS1 is not a valid request for those protocols, so
nothing is amplified. Fix (optional): filter ports below 1024 as the spray does.

**NB44-I2 — The 5 ms wake-up lasts as long as a punched link (Info, not
security).** `Node::next_timeout` is capped at 5 ms while any extra socket is open,
and a route socket stays open while its link lives, so a link made by a birthday
punch keeps the host waking 200 times a second. This costs battery on phones.
Suggested: wait on the extra sockets' readiness, or poll less often once only routes
are left.

**NB48-1 — Net Lab: any room member can make a visitor's browser run a 256-connection
fleet against addresses of the member's choosing, with no click, repeatedly
(Medium).** The trigger is two messages: `{t:'bday', step:'start',
yourRole:'hard'}`, then `{t:'bday', step:'offer', sdp}`. With them, `onBirthday`
generates a certificate and calls `openFleet` with `FLEET_SIZE` (256), applying the
attacker's offer, candidates included, to every connection. Every connection then
runs ICE checks to every candidate in that offer.

- Repro (`packages/js/test/sec_fleet_probe.mjs`, Chromium, loopback; scratch only):
  an offer with 20 injected candidates drew 16 STUN checks/s from 1 connection,
  515/s from 32 and 2,095/s from 128. That scales linearly, so about 4,000/s from
  256, for the 70 s timeout. The candidates can name any IP, including the
  visitor's LAN.
- There is no cooldown: `step:'abort'` closes the fleet, and a new `start` opens
  another at once. Each round also costs the visitor 256 connections and a key
  generation, which is heavy on a phone.
- The global room is open to anyone who loads Net Lab.
- Fix:
  - Library: `openFleet` strips the offer's candidates and takes one validated
    public IPv4 address for the easy side, added to each connection with
    `addIceCandidate`, so the fleet checks one address, as the native open does.
    Cap `size` at `FLEET_SIZE`.
  - Net Lab: a remote `start` runs only after a local click. One fleet at a time,
    one per peer per 5 min, and `abort` doesn't reset the wait.
  - Test: an offer with N candidates leaves at most one remote candidate on each
    fleet connection.
- **Status: fixed** by `d1b2d25` (NB-048b).
  - `openFleet` strips the offer's candidates, caps `size` at 256, and adds the
    easy side's `addr` only if it is public IPv4.
  - A remote `start` only sets `invited`; `tryBirthday` runs on a local click only.
  - The cooldown is one punch per peer per 5 min, kept in merged state so a remote
    `start` can't reset it.
- Re-check probe (Chromium, loopback, scratch):

  | Hostile offer | Result |
  |---|---|
  | 20 injected candidates plus `c=`/`m=` at a listener | 0 remote candidates and 0 checks per connection |
  | A last candidate line without its CRLF | Chrome rejects the SDP |
  | `a=CANDIDATE:` in upper case | Not parsed as a candidate |

- Residual (accepted): once the visitor accepts, a peer still chooses the one public
  `ip:port` the fleet checks. Measured with one candidate per connection: 474
  checks/s from 32 connections, 1,912/s from 128 and 2,705/s from 256; 39,288
  checks in 30 s from 256. That is one click per peer per 5 min, public IPv4 only.
  Rooms run the same fleet with no click since NB-084, under NB84-1's per-IP cap.

**NB48b-1 — The easy side still checks the candidates in the peer's answer (Low).**
`openSpray().applyAnswer` passes the answer SDP to `setRemoteDescription` without
`stripCandidates`, so a peer playing the hard side can list any addresses, LAN and
loopback included.
- Repro (re-check probe): an answer with 20 loopback candidates gave the easy
  connection 20 remote candidates. It sent 127 checks in 8 s and hit all 20 ports.
- This needs the visitor's click to accept, and it is one connection.
- Fix: `setRemoteDescription({ type: 'answer', sdp: stripCandidates(answerSdp) })`.
  Our own fleet's answer carries no candidates, so nothing else changes.
- Test: an answer with N candidates leaves only the spray candidates.
- **Status: fix applied** by `de9efea`, re-checked 2026-10-24. Answers with CRLF,
  LF-only, upper-case or tab-separated candidates drew 0 remote candidates and 0
  checks in Chromium. The strip itself could be bypassed, though: NB48b-3, fixed by
  `695b38b`.

**NB48b-2 — "One fleet at a time" is cleared by its own refusal (Low).**
- The bug: `closeBday` always sets `bdayBusy = false`. Both the busy refusal and the
  cooldown refusal in `tryBirthday` go through `bdayFail` → `closeBday`, so refusing
  a second punch frees the slot, and the next click starts it.
- Repro (Net Lab, three tabs, scratch): `punch(0)`, then `punch(1)` twice, left two
  punches running at once in one tab. Every two clicks on another peer add one more.
- Fix: track the busy peer (`bdayPeer`) and clear it only in that peer's
  `closeBday`. Or reset it only when a `ctx` was closed.
- Test: the third click leaves one punch running.
- **Status: fixed** by `de9efea`, re-checked 2026-10-24. `bdayBusy` holds the peer
  whose punch has the slot, and only that peer's `closeBday` frees it. Every way a
  punch ends reaches that call: the timer, `abort`, an answer with no public IP, an
  error, a win. The 70 s timer is armed with the slot, so the slot can't stick.

**NB48b-3 — `stripCandidates` misses some candidate lines Chrome parses (Low).**
`stripCandidates` finds candidate lines with a regex whose idea of a line isn't
Chrome's: Chrome's SDP parser splits only on `\n`. Some line-ending variants
survive the strip and are still parsed as candidates.
- Repro (re-check probe, loopback listeners): a hostile answer with 5 loopback
  candidates written that way, through `applyAnswer`, gave the easy connection 5
  remote candidates and 129 checks in 8 s.
- Reach:
  - the answer path, which leaves NB48b-1 open;
  - by code reading, the offer path into `openFleet`, which brings NB48-1's LAN
    reach back across the fleet.
  - Both still need the visitor's click, at most once per peer per 5 min, so the
    no-click NB48-1 stays fixed.
- Fix: strip on Chrome's line model,
  `sdp.split('\n').filter((l) => !/^\s*a=candidate:/i.test(l)).join('\n')`.
  After `setRemoteDescription`, fail if `pc.remoteDescription.sdp` (Chrome's own
  re-serialisation) still lists a candidate.
- Test: stripping SDPs with every line ending, and a stray `\r` or U+2028 inside a
  candidate line, leaves no `\n`-delimited candidate line. In Chromium, the fleet
  has 0 remote candidates and the easy side only the sprayed ones.
- **Status: fixed** by `695b38b`, re-checked 2026-10-24.
  - `stripCandidates` splits on `\n` only, as Chrome does, and drops every line that
    starts (after any whitespace) with `a=candidate:`, in any case.
  - `openFleet` and `applyAnswer` then check Chrome's own remote description. If a
    candidate is still there, they close every connection they opened and fail.
  - Re-check probe (Chromium, loopback listeners counting STUN checks): 29
    line-ending, prefix and case variants, each with one candidate aimed at a
    listener. Unstripped, Chrome checked 4 of them (41 checks each in 2.5 s) and
    rejected or ignored the rest. Through the fixed `openFleet` and `applyAnswer`:
    0 checks for all 29, and the easy side had only its sprayed candidates. The
    variants the strip leaves alone make Chrome reject the whole description, and
    both calls then fail and close.
  - Residual (Info, accepted): the after-apply check is a backstop for a future
    parser change. On the fleet it runs before the answer, so ICE never starts (0
    checks). On the easy side, which offers, ICE starts as soon as the answer is
    applied: with candidates forced past the strip, exactly one check left before the
    close, for 1, 5 or 20 candidates.

**A punched channel carries the room link (`fd660de`, `cf49f72`; reviewed
2026-10-24).** A birthday punch that works hands its DataChannel to the room
(`room.adoptChannel`). A link over signalling then sends its pipe datagrams over the
channel, and the core sees no new address. A link relayed by a member gets a new
handle, and `pw_add_candidate` lets the core punch to it and move there. The review
found nothing that weakens the session:
- The channel's far end is bound by DTLS to the SDP that came over the authenticated
  room link. Everything it sends goes to `ep.receive` and must authenticate. Probe:
  101 junk datagrams over a carried channel were all rejected (99 malformed, 2 unknown
  session), and the link kept delivering.
- The one-byte CARRY/CARRIED agreement settles only on a binary message of at most
  `MAX_DATAGRAM` bytes. A string, an oversized or an empty buffer doesn't settle it,
  and a one-sided adopt is refused after 5 s. A CARRY gets one reply on the same
  channel, so there is no amplification (but see NB48b-6).
- There is one carrier per peer, and a newer adopt ends the old one. It ends with the
  peer (`lost` → `drop`) and with the room (`close`), which closes its connection.
- No address is shared beyond what the punch already exchanged, and both visitors
  clicked to punch.

**NB48b-4 — Relay-only: a pending relayed dial sends its handshake direct (Low).**
With relay-only on and a dial through a relay still pending, `pw_add_candidate` (or
`pw_connect`) for that peer sends the handshake straight to the new address. That
reveals our address to whoever is there, which relay-only promises not to do.
- Cause: `connect_with` keeps only Via targets when the *current call* names one.
  A Direct-only call made while dialling is added to the existing relayed dial.
- Repro (FFI, scratch): a and b link to r, `pw_set_relay_only(a, 1)`, then
  `pw_connect_via(a, r, b)`, then `pw_add_candidate(a, 11, b)` before routing. a sent
  3 HS1 to handle 11. With the link live first, it sent 0.
- Reach: the host only. Native traversal under `RelayOnly` dials others only through
  the relay, and JS rooms refuse relay-only.
- Fix: while dialling, skip Direct targets when relay-only and the dial has a relay.
- Test: the repro sends 0 datagrams to the candidate in both states.
- **Status: fixed** by `4b6c0f7` and `46a5da3`, re-checked 2026-10-24.
  - `4b6c0f7`: a dial through a relay takes no Direct path added while relay-only
    (`add_candidates`, `connect`, `connect_with`). Tests in the core and the C ABI.
  - `46a5da3`: turning relay-only on while such a dial is pending drops its Direct
    targets, port guesses and spray, and the dial ignores a handshake answer that
    arrives at our own address (NB48b-7). It completes through the relay only.
  - Probes: candidates, guesses and sweeps on a relay-only relayed dial send nothing
    direct, and a simultaneous direct handshake from the peer is dropped.
  - Residual (Info, accepted): a *direct* dial made while relay-only that later also
    names a relay keeps its direct target, so it can still go live direct. As for an
    incoming HS1, the peer's answer is then taken at our address whatever address it
    comes from (`4c37bb7`). Dialling a peer directly gives up relay-only for that
    peer: the host chose to send its handshake there.

**NB48b-7 — Relay-only: a relayed dial takes the peer's answer at our own address
(Low).** Found in the NB48b-4 re-check. The handshake answer (HS2) set the link's path
to wherever it arrived from, with no relay-only check, unlike an incoming HS1. So the
peer being dialled, if it already knew or guessed our address and our NAT let it in,
could answer there: the link went live direct and our traffic confirmed the address.
- Fix: a relay-only dial through a relay drops an HS2 that arrives direct, before
  reading it, and waits for the relayed answer.
- **Status: fixed** by `46a5da3`, with a core test (an answer to an HS1 sent direct
  before the switch is dropped; the link comes up through the relay). `4c37bb7` keys
  the check on the dial instead of its relay, the same rule as an incoming HS1: under
  relay-only, an HS2 at our own address is taken only by a dial with a direct target.
  Re-checked 2026-10-24: a relayed dial, also one switched to relay-only with the
  peer's direct answer in flight, still drops it and completes through the relay.

**NB48b-5 — A route reads `direct` while a relayed link waits to fall back (Info).**
After a carried channel that replaced a relayed link closes, the core keeps the dead
handle until its 15 s direct timeout, and `room.route` reports plain `direct` (probe:
from 50 ms to 15 035 ms). In that window, an old WebRTC link's `closed` event drops the
peer, and the peer counts as a relay for others. Only the peer itself or the network
can cause this.
- Fix: `rtcClosed` drops the peer only if the closed link's handle is its current
  path, or ex-carrier handles stay marked until the path changes.
- **Status: fixed** by `08425b8` (the first fix), re-checked 2026-10-24. The transport
  passes the closed link's handle, and a close that isn't the peer's path now only
  forgets the WebRTC link. The transport keeps one WebRTC link per peer and detaches
  `onclose` from links it drops, so this never closes a live link. Probe: the peer's
  current link closing still drops it.
  - Residual (Info, accepted): in the same window `rtcFailed` can still pick the
    peer as a relay for another member's fallback. That dial fails through it; the
    other relays and the pipe still run.

**NB48b-6 — CARRY replies are unbounded (Info).** Every CARRY is answered with a
CARRIED, sent without the `MAX_BUFFERED` check. A peer can fill its own channel's send
queue with one-byte replies (probe: 500 in, 500 out).
- Fix: answer at most once per `CARRY_EVERY_MS`, through the congestion check.
- **Status: fixed** by `2a00ecb`, re-checked 2026-10-24. Each adopted channel answers
  at most one CARRY per `CARRY_EVERY_MS`, and every marker goes through `dcSend`.
  Probes: 500 CARRY at once got 1 reply (none with the buffer full); 193 spread over
  2 s got 10.

**NB48-2 — The easy side sprays whatever IPv4 address the peer names (Low).** In Net
Lab's `answer` step, `data.ip` goes to `applyAnswer` after `sprayCandidates` checks
only its format. That sends 1024 ICE checks, with retransmits, at about 16/s to
ports 1024 and up of any IPv4 address, including the visitor's own router or LAN
hosts, or loopback. Fix: accept only a public IPv4 address (not RFC 1918, loopback,
100.64/10, link-local or multicast) that matches a server-reflexive address in the
peer's own signalling.
- **Status: fixed** by `d1b2d25` and `695b38b`.
  - Fixed: `isPublicV4` refuses non-public spray IPs, and the spray needs the
    visitor's click.
  - Left as is: the IP isn't matched to anything the peer proved, since the peer
    writes all of that signalling anyway.
  - The LAN reach that remained through the answer's own candidates (NB48b-1, then
    NB48b-3) is closed by `695b38b`.

**NB46-1 — A peer can keep the dialling side publishing offers (Low).** A `restart`
signal makes the dialling side create and publish an ICE-restart offer, resent up to
twice. `MAX_RESTARTS` (3) starts over every time ICE reconnects. So a peer that
answers each restart and lets ICE reconnect can make us publish about 3 offers
(1–2 KB of SDP each) per round, without end.

Independently, and already true on main, every distinct offer from a linked peer is
answered, and every copy of the last offer gets the answer resent, with no per-peer
limit. The publishes land on our Nostr relays, which rate-limit and ban busy IPs:
relay.damus.io banned a shared hotspot IP in testing (see `86119f4`), and `2d40bc6`
dropped it from the defaults for that. Losing the relays cuts signalling for the
whole room.

Fix: honour `restart` only while our own ICE is not `connected` or `completed`, and
allow at most 3 restarts per minute per link whether or not ICE reconnects. Put the
answers and resends we publish to one peer under a budget (for example a burst of 6,
then 1 per 5 s). Found by reading the code; no probe was run.
**Fixed** by `851a0e2` (merged `bae16c7`), with the review's findings below fixed by
`88bbf37` (merged `ee4cbe6`); both reviewed 2026-10-26.
- A `restart` ask is heeded only while our own ICE isn't `connected` or `completed`
  either. A link makes at most `MAX_RESTARTS` (3) restarts in `RESTART_WINDOW_MS`
  (60 s), and ICE reconnecting no longer starts the count over.
- Offers, answers, copies of either and `restart` asks to one peer go through a budget
  per peer (`WebRtcTransport.publishBurst` 6, then 1 per `publishEveryMs`, 5 s); what
  it refuses is counted in `throttled`. So one peer can make us publish 6 of these,
  then 12 a minute, and NB-006h's pacing per relay bounds the total. ICE candidates,
  pipe batches and the `retry` ask don't count; retry offers have their own floor
  (FL1-2). An answer the budget refused waits for the dialler's next copy of the offer.
- **NB46-1a (Low):** a refused answer's candidates still went out, trickled as `ice` or
  late as `ice-batch`, so the budget didn't bound what a peer made us publish. Repro
  (scratch, a fake RTCPeerConnection that finds candidates after each answer): 51
  offers gave 6 answers but 153 `ice` messages, or 51 `ice-batch` in a room. **Fixed**:
  a refused answer holds its candidates, and the copy that releases it rebuilds the
  answer from the local description, candidates included. The same 51 offers give 6
  answers and 18 `ice`, or 6 `ice-batch`.
- **NB46-1b (Info, older than the fix):** a copy of a new offer got the last offer's
  answer. **Fixed**: taking a new offer clears the last answer, and only an answer
  already made is released, since the local description is still the last offer's
  while the next one is made.
- **NB46-1c (Info):** a restart refused for a full window was never made. **Fixed**: it
  is made once the oldest restart leaves the window, if ICE is still down. Accepted: a
  link whose ICE stays down while the session runs on a pipe or a member relay keeps
  restarting 3 times a minute until it is dropped, within the budget.
- **NB46-T1 (Info, test gap):** a mutant whose window never slid passed every check.
  **Fixed**: `restart_budget.mjs` moves the clock past the window.

**NB46-I1 — Port guesses aim at any IPv4 address (Info).** `checkPrediction` checks
only the format of `nat.ip`. It doesn't have to be the peer's server-reflexive IP, or
public. Each new `{ ip, next, step }` adds 32 candidates, and every `ice-batch` can
carry a new one. Remote candidates per link have no total cap, only 32 per message
(this predates NB-046). The bound is Chrome's ICE pacing, which is WebRTC's own
security model for remote candidates (RFC 8826), but the cost is cheap to tighten.
Fix: require `nat.ip` to match one of the peer's server-reflexive addresses, take one
prediction per description, and cap remote candidates per link (for example at 128).
**Status: fixed for port guesses** by `0a8a796` (NB-084), merged `9c7789b`, reviewed
2026-11-03. A prediction counts only if `nat.ip` is a public IPv4 that the peer's own
server-reflexive candidates on that link name, and a link adds at most `MAX_GUESSES`
(128) predicted candidates, however many predictions come. Tests: `nat_predict.mjs`
(private IPs, an IP the peer never named, the cap) and `ice_restart.mjs` (an offer
whose server-reflexive candidates don't name the IP). Residual (Info): other remote
candidates per link still have no total cap, only 32 per message, and Chrome's ICE
pacing bounds them.

### Two-sided birthday punch (NB-083) — NB83-1

Reviewed on 2026-11-03: `backend/NB-083-sym-ip` at `d7e3604`, then its fold-in
`56309f9`, merged `9665afb`. STUN now asks Google's server on a second port, which
tells a symmetric NAT that maps one port per destination IP (`NatType::SymmetricIp`).
Against another symmetric NAT, unless both predict their ports (then the lockstep
sweep, as before), the `SymmetricIp` side opens the 256 sockets and the other sprays
them, at most 1024 ports from a NAT that maps per IP and port (NB83-1); two
`SymmetricIp` sides each do both. The mechanism and costs are in
`docs/concepts/nat-traversal.md` (STUN, Two-sided birthday punch, Messages, What
others learn or can do).

What changes for an attacker:

| Trigger | Who can trigger it | Traffic per try | Limits |
|---|---|---|---|
| An offer saying `SymmetricIp`, to a symmetric node, or `Symmetric` with the mapping entry, to a `SymmetricIp` node, unless both the offer and the node have a prediction | A room member, an invite holder, or a peer the node dialled | A `SymmetricIp` node facing `Symmetric`: the native open above, about 34 KB from 256 sockets. A `Symmetric` node: the native spray, at most 1024 probes to ports ≥ 1024 of the offer's first address. Two `SymmetricIp` nodes: both, in one punch, the spray up to 272,916 B | As the spray: 5 min per peer; the node budget; the NB44-1 per-IP cap, one entry for both halves. A `Symmetric` node's spray: one per 5 min per node (`PER_PORT_PUNCH_GAP`) |

- **Reflection.** What lands on one address is the spray's, under the same per-IP cap,
  so NB44-1's bound holds, but for the opener's 256 HS1s when two `SymmetricIp` nodes
  each do both (NB83-I1, +12.5%). Before NB-083 only a cone node sprayed. Now a
  symmetric node does too, so more nodes can be recruited, each within its own cap
  (NB44-1's per-node residual).
- **Ports on the node's own NAT (NB44-3).** A node behind a NAT that maps per IP and
  port pays one port per probe, held for the NAT's UDP timeout, so it opens no sockets
  for this and sprays at most 1024 probes (NB83-1): up to 1026 ports a try with its
  dial's. Before NB-083 it paid 256 a try (an offer saying `Cone`). Under the node
  budget alone, a member minting PeerIds could start 4 tries at once and 1 per 30 s
  after, about 4,100 ports, so such a node sprays at most once per 5 minutes, whichever
  peers ask: about 1,030 ports at a time from this, beside the 256 per try an offer
  saying `Cone` already allowed (4 tries at once, then 1 per 30 s, under the node
  budget). That is still much of a carrier NAT's per-subscriber block of 512 to 2,048
  ports. A `SymmetricIp` node's spray all goes to one IP, so it costs its NAT one
  port. Test: `sym_ip_nodes_open_and_symmetric_peers_spray` (a
  `Symmetric` node's spray is 1024 probes and no sockets; peers at three IPs start one
  spray on a `Symmetric` node and three opens on a `SymmetricIp` one).
- **Lying about the NAT.** A sender picks its own NAT type, as before. Claiming
  `SymmetricIp`, or the mapping entry, only picks which of the existing punches the
  node runs. The offer is sealed to the recipient and proved, so no relay can add or
  strip the entry.
- **The wire.** The entry is one more tagged item in the address list: `[3][fe 02][0|1]`.
  It counts only with `nat` 3 and exactly that form, the first if there are several,
  and takes one of the 8 slots. Decoders from before NB-083 skip it as an address they
  can't read, so the offer's length bounds and strictness are unchanged. Tests: main's
  codec, copied into `traversal.rs`, reads new offers, and every offer without the
  entry is byte-identical. `fuzz_hello_and_offer_parsers` and
  `fuzz_authenticated_signals` include `SymmetricIp` offers.
- **STUN.** One more server address is asked, at Google's IP. A forged answer still
  needs the transaction id and the server's source address. Forged answers from the
  two Google ports could make a node `SymmetricIp`, or not. That only changes which
  punch it plans, under the limits above.

Checked and holding (read, no probe): both ends plan the same punch;
`PER_PORT_PUNCH_GAP` is checked before NB44-1's per-IP cap and the node token; the
spray half of `Both` is skipped once the link is direct or relay-only; `classify`
handles empty answers; each STUN name is looked up once; the fuzzers draw
`SymmetricIp` offers.

**NB83-1 — One two-sided try could use up a per-port carrier NAT's port block
(Low).** At `d7e3604`, `PER_PORT_PUNCH_GAP` limited how often a node behind a NAT
that maps per IP and port started a two-sided punch (once per 5 min), but not what a
try cost: 256 sockets and 2,048 probes, each to a new destination, so 2,304 NAT
ports, held for the NAT's UDP timeout. A carrier NAT may give a subscriber 512 to
2,048 (NB44-3), so one try could fail the user's other new connections meanwhile.
Any room member, invite holder or dialled peer could start one per 5 min with an
offer saying `SymmetricIp`, and a sequential per-port node, which before could only
be made to sweep (96 ports), ran the full try.
- Repro: the unit test of the time gave a `Symmetric` node one 256-socket `Birthday`
  and 2,048 probes against a `SymmetricIp` offer; in the simulator, a `sym-ip-apdf`
  × `sym-random` try cost 258 + 2,306 mappings.
- Fix: direct the punch (the `SymmetricIp` side opens, the per-port side only sprays)
  and cap that spray at about 1024 probes. The hit chance against 256 sockets over
  64,512 ports, **theoretical** 1 − (1 − 256/64512)^k, is 98.3% at k = 1024 against
  99.97% at 2048.
- **Status: fixed** by `188987b` (docs `56309f9`) before NB-083 merged, so never on
  main, reviewed 2026-11-03: the plans and the Ports bullet above. In the simulator,
  `sym-ip-apdf` against the per-port kinds costs 258 + 1,026 mappings a try (was
  2,306), and the 12-kind matrix still links 116 of 144 pairs directly.

**NB83-I1 — Two `SymmetricIp` nodes add the opener's HS1s to what one address gets
(Info, accepted).** A `Both` punch counts once against the per-IP cap, and its 256
opener HS1s go to the address it then sprays. Per node per IP per 5 min that is
613,928 B, not 545,832 B (+12.5%, **theoretical**: 2 × (272,916 + 256 × 133) B).
Since `188987b` only two `SymmetricIp` nodes do both, and it stays within NB44-1's
accepted per-node residual.

**NB83-I2 — A member can spend a per-port node's one spray (Info, accepted).**
`PER_PORT_PUNCH_GAP` is node-wide, so a member minting PeerIds can use up a
`Symmetric` node's one spray per 5 min, and a genuine `SymmetricIp` peer then stays
on the relay until it passes, as with NB44b-I1. An availability cost only.

### Automatic birthday punch in rooms (NB-084) — NB84-1

Reviewed on 2026-11-03: `ui/NB-084-auto-birthday` (`0a8a796`, `82c0117`, then the
fix `fd2f246`), merged `9c7789b`, with a docs fix in `0af38e9`. `joinRoom` now runs
the browser birthday punch of NB-048 by itself, with no click, for a member it
reaches only by fallback, when the WebRTC link that failed showed one side behind a
cone NAT and the other behind a symmetric one with no prediction. The two sides agree
over the room's encrypted link (`Punch` frames, tag 10), and a channel that opens
takes the link over (`room.adoptChannel`). `joinRoom({ birthday: false })` turns it
off. The mechanism is in `docs/concepts/nat-traversal.md` (In browsers).

| Trigger | Who can trigger it | Traffic per try | Limits |
|---|---|---|---|
| A member on a fallback link, with us as the symmetric side | Any member of a room the page joined | A fleet: 256 connections, each checking one public `ip:port` (39,339 checks in 30 s measured, below) | One punch at a time per page, and 30 s after one that ran; per destination IP, 2 per 5 min (NB84-1); once per peer per room; 60 s at most |
| The same, with us as the cone side | Same | One connection checking 1024 ports of one public IPv4, at ICE's pace (about 20 a second) | Same |

- **Addresses.** Neither side checks a candidate from a description: both are
  stripped (NB48b-1, NB48b-3). The one address each side aims at is public IPv4 and
  comes from the peer's own candidates on the failed link, never from a `Punch`
  frame. The peer writes those candidates, so it can name a third party's (NB84-1).
- **Who.** Any member, so anyone in a room whose name can be guessed (CHAT-1), with
  no click on either side. Relay-only runs no punch, and `Punch` frames go only to
  proven members.
- **Lifetime.** Everything a punch opened closes when it ends (60 s, `punchMs`), when
  the peer goes and when we leave.
- **Port guesses.** `0a8a796` also fixes NB46-I1 for port guesses, in the section
  [above](#hard-nat-punching-nb-044-nb-045-nb-046-nb-048--nb44-1nb48-2).

**NB84-1 — A member can aim a visitor's fleet at a third party, with no click, again
and again (Medium).** At `82c0117`, a member's `Punch` offer made a symmetric-NAT
member open its 256-connection fleet at the offer's `addr`. The only checks were that
`addr` is public IPv4 and that the peer's candidates on the failed link named its IP,
and the peer writes those (as in NB48-2). The cone side was the same: it sprays 1024
ports of the IP the peer's candidates named. Only the page's one punch slot and its
30 s gap limited it, so new PeerIds, or other rooms, could aim fleet after fleet at
one victim. Net Lab's global room, open to anyone, passed no `birthday` option, so it
ran these punches too, without NB48-1's click.
- Repro (scratch loopback probe in headless Chromium, run once): one fleet aimed at a
  listener drew 39,339 checks (3,933,900 B of payload) in about 30 s, then nothing
  while the fleet stayed open.
- Fix: a per-destination-IP cap in the page's punch slot (one at a time, 2 per 5 min,
  a bounded map, as NB44-1 does natively); `birthday: false` for Net Lab's global
  room; docs; a test with 3 PeerIds at one IP.
- **Status: fixed** by `fd2f246` before NB-084 merged, so never on main, reviewed
  2026-11-03.
  - The slot (`punchSlot(...).take(who, ip)`) allows at most `ipMax` (2) punches per
    destination IP per `ipWindowMs` (5 min) that opened a fleet or sprayed,
    whichever rooms and peers ask. The symmetric side counts the IP its fleet aims
    at, the cone side the IP it sprays. A fleet counts once opened, a spray once
    answered, so a refused or unanswered offer costs nothing.
  - It holds at most 64 IPs (`MAX_IPS`) and refuses a new one past that rather than
    forget one; with the 30 s gap a window holds about 10. An address is read by a
    strict IPv4 pattern with no leading zeros, so an IP has one spelling and no alias
    gets a second count.
  - Net Lab's global room joins with `birthday: false`. Its button still punches, on
    a click only: it waits for the same slot but adds no gap or per-IP count
    (NB84-I3).
  - Tests: `room_birthday.mjs` (three peers at one IP get 2 punches on either side
    while another IP still gets one) and `netlab_rooms.mjs` (the global room runs
    none).
- Residuals (accepted):
  - The cap is per page, as NB44-1's is per node: N symmetric-NAT members, each aimed
    by a member, still send one IP 2N fleets per 5 min, each about 39,000 checks.
  - One page whose targets rotate runs about one fleet a minute (**theoretical**: 30 s
    of checks, then the peer's `stop`, then the 30 s gap), at most 2 per IP per 5 min.
  - In a room whose name can be guessed, anyone can start these punches; such an app
    can pass `birthday: false`.

**NB84-I1 — The punch slot is per `RTCPeerConnection` class (Info, documented).** A
page's slot and its per-IP counts are kept per class, so rooms given different
classes get a slot each. `docs/reference/javascript.md` says to give every room in a
page the same class (`fd2f246`).

**NB84-I2 — Each repeated offer gets a `no` (Info, accepted).** A member sending
offer after offer gets one `no` frame for each, over the room link: one reply a
frame, no amplification. Kept, as a prompt `no` frees the slot of a peer that
rejoined.

**NB84-I3 — The docs said Net Lab's button shares the punch limits (Info).** It
shares only the slot. **Fixed** by `0af38e9`: `docs/reference/javascript.md` says the
button waits for the same slot and adds no gap or per-IP count.

### WebRTC retries (Fast lane 1) — FL1-1, FL1-2

While a link runs on a fallback (a member relay or the signalling pipe), the room tries
WebRTC again with a new RTCPeerConnection (`9acde3d`…`0e984ea`, `retry.js`): on a
backoff (20 s doubling to 5 min), after a network change, after a microphone or camera
grant (privacy `'direct'`, `retryOnGrant`) and on `room.retryDirect()`. Only the
dialling side (the smaller id) offers; the other asks it with a `retry` signal. Reviewed
2026-10-24 by reading the code, with two scratch repros:
- A retry offer (`retry: true`) must prove the room key even on an existing link. It
  replaces only an answering link that never opened, and closing that link clears its
  `onclose` first. The fallback carrying the session (pipe, member relay or adopted
  channel) and the session are untouched, and a link whose channel opened is never
  replaced. Over Nostr `from` is bound by the seal, so a member can't retry in another's
  name. A relay re-sending an old retry offer (SEC-6) can close a pending connection, as
  dropping signals could.
- The room acts on a `retry` ask only for a linked peer on a fallback, once per 20 s per
  peer. Its own retries go one at a time, at least 2 s apart, and a failing one holds the
  slot for about 10 s (gathering plus `fallbackMs`). So asks raise one room to about 6
  retries a minute, each an offer, an answer and late candidates.
- Retries describe the connection with the same `description()`, host filter and
  `outgoing()` as a first dial, so `public-only` gives out no local address (MIC-1,
  MIC-3). MIC-5 is unchanged.
- The new `Nudge` room frame (tag 6, empty) goes only to proven peers. Rooms on main and
  in 0.1.1 drop unknown tags after the proof.
- A pipe's queued datagrams go over the channel that now carries the link (the live
  adopted one, else the peer's own open one), in the same order as `onTransmit`. The core
  seals them either way.

**FL1-2 — Retry offers are answered without limit, each with a new connection (Low).**
This is NB46-1's per-peer gap for retries. While our link to a peer hasn't opened, each
proven retry offer from that peer with a new description closes the link, builds a new
RTCPeerConnection (gathering from the STUN servers) and publishes an answer. The room's
budget limits only the retries we begin. Repro (scratch, `room_retry.mjs` harness): a
member sent 20 proven retry offers 100 ms apart to a pair on the pipe. The other built
20 connections and published 20 answers, and the link stayed on the pipe. Fix: take at
most one new retry offer per peer per ~2 s, checked after the proof (a copy still gets
its answer resent), plus NB46-1's answer budget. A prototype of the floor made 1
connection and 1 answer.
**Fixed** by `4b131ee` (merged `a3f40d4`). After the proof, and after the check that
ignores an offer for a link we dial or one that opened, a retry offer that would make a
new connection (it replaces our link, or there is none) is taken at most once per peer
per `WebRtcTransport.retryAnswerMs` (2 s). The rest are counted in `badSignals` and leave
the current link alone. A copy of the current offer gets its answer resent, with no new
connection. Every other way a peer gets a new connection on the answering side is
already bounded, so a peer can keep at most about one connection gathering, made at most
once per 2 s, and each new one closes the last. A legitimate retry the floor refuses is
taken on its resend 3 s later, while the dialler's link waits 8 s (`fallbackMs`).
`room_retry.mjs` (test 10): 20 offers made 1 connection and 1 answer, and 19 were
refused. NB46-1's answer budget, for restart offers on an open link, came with
`851a0e2` (merged `bae16c7`). Still open: a relay replaying a proven retry offer
(SEC-6), which can take the 2 s slot. Test gap
FL-T1 (Info): test 10 starts while the answering side still holds its failed link, so
it didn't reach the no-link case. Closed by `e451fc4` (merged `147537d`): in test 13,
2 proven retry offers within the floor to a side with no link made 1 connection and 1
answer, and 1 was refused.

**FL1-1 — The answering side acts on a `retry` ask (Info).** The larger id queues the
asker and asks it back, though only the dialling side should act on an ask. A
legitimate dialler never asks, and the ask goes back only to its sender, once per 20 s,
so nothing is amplified. Repro: one ask to the larger id on a pipe pair made it ask back
once, and the dialler retried once. Fix: act on `retry` only when `self < peer`; with
it, neither happened. **Fixed** by `4b131ee`: the room acts on a `retry` ask only when
`self < peer` (`room_retry.mjs` test 11: no ask back, no retry).

### Member relays for pipe links (Fast lane 2) — FL2-1, FL2-2

A link on the signalling pipe moves to a member that later links directly to both ends
(`03d3616`, `1d6a3dd`). The room marks the pipe as the link's slow address
(`Endpoint::set_slow`) and offers every proven direct member (`add_relay`). The core
probes with a sealed PUNCH through the member and moves on the PUNCH_ACK that comes back
through it. The far end answers only if it would use that member too. The link goes back
to the pipe when the member's link goes, when the peer's newer traffic comes over the
pipe, or after 15 s of relay silence. Reviewed 2026-10-24 by reading the code, with two
scratch core repros; the follow-up fixes (`63e5b72`, `eb20605`, `ddae245`, merged
`8314aa8`) were re-checked the same day with one more:
- Only the two ends can move a link. Every move or return needs an inner packet sealed
  under their session and passing its replay window; a relay can only forward, withhold
  or delay it. A move needs the prober's random token on a relay it offered. A return to
  the pipe needs a non-probe packet over the pipe with a nonce above everything that came
  through the relay, so withheld or replayed pipe packets move nothing. Signalling relays
  can't inject on the pipe (`from` is sealed).
- A Bye now also ends the core session, so links the leaver relayed go back at once. A
  Bye is acted on only as a message on the leaver's own session, so nobody else can send
  one, and a replay fails the nonce window.
- Probes are 1:1 to the peer, through at most 8 members, every 500 ms for 5 s.
  Relay-only moves nothing, answers no probe and never falls back to a pipe. No new
  frames: a 0.1.x peer ignores a relayed PUNCH, and the link stays on the pipe.
- A relay no longer forwards to a peer it reaches only over its own pipe. This closes an
  older drain, where one member made another spend the 3 messages/s budget all its pipes
  share; the asker has its own pipe. Relays on 0.1.x still forward.
- After a move, the member stays the link's fallback relay. If the pipe is silent for
  15 s, the link goes back to that member without a probe; with a 10 s keepalive, that
  means the pipe is down. Since `63e5b72` the fallback also needs that member linked
  directly, not over its own pipe, and not held off (FL2-1). Before, it needed only a
  link of any kind (review question Q4).

**FL2-1 — A member that drops only data holds a moved link until it fails (Low).** On a
member relay, `last_path` is kept fresh by any packet, and the peer sends through the
member too. A member that passes probes, acks and pings but drops larger packets
triggers neither rule, so reliable data stalls until the link fails as a whole. Before
Fast lane 2 a member couldn't take a pipe link. Repro (scratch core test): the member
dropped what it relayed to b above 200 bytes. a's 400-byte message never arrived, the
link stayed on the member, and a dropped it as timed out at 70 s. Fix: on a relay with a
slow address, go back to the pipe once reliable data has waited about 5 s unacked, and
hold that member off for the link for a while: no offer, no answer to its probes, and no
fallback to it.
**Fixed** by `63e5b72` (tests in `eb20605`, merged `8314aa8`). On a member relay, for a
link with a slow address to go back to, data unacknowledged for `RELAY_STALL` (5 s,
timed from the move at the earliest) sends the link back to the slow address and resends
what is in flight there at once. That member is then held off for the link for
`RELAY_HOLD` (60 s), doubled for each repeat up to 16 times. A link holds at most
`MAX_RELAYS` (8) members off, and a new hold replaces the one that ends first. A held
member isn't offered, its probes aren't answered, and neither the relay-to-relay roam nor
the fallback moves the link to it. A link that moves to a member pings through it at
once, so both ends have a non-probe packet through it and follow each other back. The
core test `a_relay_that_stalls_the_data_hands_the_link_back_and_is_held_off` is the
repro's case: the link goes back to the pipe `RELAY_STALL` after the data went out, the
data arrives there, and the member isn't taken again until its hold runs out. Native UDP
links have no slow address, so the rule didn't apply to them; since FL3-3 a link with no
slow address holds the relay off and races the others. Residuals:
- **R1 (Info, accepted):** the rule watches reliable and urgent data in flight. A member
  that drops only large unreliable datagrams, or delays everything by just under 5 s,
  keeps the link. The unreliable lane promises no delivery, so the ends can't tell those
  drops from loss.
- **R2 (Info, accepted):** holds are per link and at most 8. More than 8 members acting
  together can stall the link in turn, each new hold pushing out an older one and its
  doubling. The link can then keep cycling between the pipe and a stalling member, each
  round delaying its reliable data by up to `RELAY_STALL`. The data still arrives over
  the pipe.

**FL2-2 — A member offered as a relay can roam a pipe link onto its own channel (Low).**
The peer's sealed probes, acks and data for a pipe link pass through every member
offered for it. A member can keep one and send it raw on its own channel. The core takes
an authenticated packet from a new direct address as a roam (NB43-I1), and a relayed
PUNCH_ACK shares the direct punch token. The link then goes to the member, the room
reads `'direct'` and drops the slow mark, and nothing brings the link back. So NB43-I1
now reaches any pipe link whose two ends share a direct member. Repro (scratch core
test): one probe taken from the member's relay queue and sent from the member's address
moved the link from the pipe to the member. Fix: in JS every handle belongs to one peer
(a WebRTC link, a pipe or an adopted channel). Pass that peer to the core with each
datagram, and drop a data packet for another peer's session before any roam, which also
closes NB43-I1 in JS. Give relay probes their own token. Rust UDP sets no slow address
and is unchanged.
**Fixed** by `63e5b72` (core) and `ddae245` (FFI and JS), merged `8314aa8`.
`Endpoint::handle_datagram_from` takes the peer whose channel a datagram came on, and
drops a data packet, HS1 or HS2 meant for another peer before decryption and before any
roam (`Stats::rx_wrong_peer`). `pw_handle_datagram_from` exposes it, and
`Endpoint.receive(addr, bytes, peer)` uses it. Every JS channel passes its peer: WebRTC
data channels, pipes (the authenticated sender of the sealed signal), adopted channels
and `LocalTransport`. Relay probes carry their own token (the direct one inverted), so a
relayed PUNCH_ACK never counts as a direct punch answer. This also closes NB43-I1 in JS.
A native UDP socket serves every peer, so it still uses `handle_datagram`, and NB43-I1
stays there, narrowed by Fast lane 3 (below).

**FL2F-1 — The relay-to-relay roam doesn't need a direct link to the new member
(Info).** Found in the follow-up re-check. The other moves onto a member relay (a probe's
answer, the fallback) need that member linked directly. The roam from one member relay
to another (`endpoint.rs:1518`) also takes a newer packet through a member reached only
over our pipe to it, such as a 0.1.x member, which still forwards over pipes. The link
then runs over that pipe's shared budget until a newer packet through the other member
moves it back. A held member is still refused. Repro (scratch core test): with a's link
to c on its slow address, a packet relayed by c moved a's link from d to c, and later
back to d. Fix: take that roam only when the packet came over a direct link
(`if fast_from`), as the probe answer does; with it the core library tests pass.
**Fixed** by `0dcdff0` (merged `12fda2d`), reviewed 2026-10-26: the roam from one member
relay to another needs `fast_from` and a member not held off, as offers, moves and the
fallback do. In the test `a_relay_linked_on_its_slow_address_is_not_roamed_to` (the
repro's case), a packet of b's that c relays no longer moves a's link.

### Latency races (Fast lane 3) — FL3-1…FL3-3

A link now races the other paths of its kind and takes one clearly faster (`0a84b7d`,
merged `4e42d88`; `f956535`, `475c442`, `75b5122` and `706a4f4`, with the fixes below,
merged `3ce181e`). Reviewed 2026-10-26 by reading the code, with three scratch core
repros; the fixes were re-checked the same day, and the repros pass unchanged on them.
- **Roaming groundwork.** A packet holding only probes (PUNCH, PUNCH_ACK) moves no link.
  A roam needs a packet newer than the newest non-probe one over the link's path, so a
  late or kept packet doesn't take the link back. Of two ends on relays, the one with
  the larger PeerId follows, and the smaller doesn't leave a relay it heard the peer on,
  so they never chase each other. This narrows NB43-I1 on native UDP (FL3-1, below).
- **What races.** A direct link races the peer's other direct addresses: the candidates
  it punches anyway, at most `max_candidates` (8). A relayed link races the relays
  offered with `add_relay` or seen carrying the peer's probes, at most `MAX_RELAYS` (8).
  A round sends one sealed PUNCH down each and down the link's own path, with a new
  token each round, at most every `PATH_GAP` (2 s). A race runs `PATH_ROUNDS` (5) rounds
  when a path turns up or the link changes kind, and once more `PATH_DWELL` (20 s) after
  a switch. An answer counts only for its round's token and for the path it came back
  over. No races while relay-only.
- **Who moves.** A path faster by 15 ms, or by a fifth if that is more, in 3 rounds is
  taken by the end with the smaller PeerId, at most once per 20 s, with a ping so the
  other follows. A peer answers a probe through a relay only if it would use that relay
  too, and a held relay isn't raced. A path that has carried nothing for the keepalive
  plus 2 s is raced again, and one that answers in a round where the link's own doesn't
  wins that round (`75b5122`). Before, a native link on a relay that went silent could
  only time out.
- **No amplification.** Probes go only down paths the link already has: a candidate the
  peer gave, or a relay linked to both ends, which forwards under its relay rate. A
  PUNCH is answered with one PUNCH_ACK of the same size, and only the peer can seal one.
  So a race sends at most 9 small probes a round, 45 a race.
- **What a relay can do.** Probes are sealed under the link's session, so a relay can
  only forward, drop or delay them. A relay that drops probes loses the race.
- **What members learn.** A member offered as a relay already carries the link's
  packets. The race adds probe timing, so it learns the pair's round trip through it.
  `room.route(peer).rttMs`, `pw_path_rtt_us` and Godot's `get_peer_path_rtt_ms` give
  the app the round trip it could measure with its own messages.

**FL3-1 — A packet held back before a punch could roam the link (Info).** After a punch
moved a link from a relay to a direct address, its newest-packet mark started empty. On
native UDP, a relay that had held back one of the peer's relayed packets could send it
from its own address, and the link roamed there until the peer's next non-probe packet
over the direct path (a keepalive, within 10 s). This is NB43-I1, narrowed to that window by the
roaming groundwork. Repro (scratch core test): one held-back packet moved the link to
the relay's address. **Fixed** by `475c442`: after a punch, the mark is the answer's, so
nothing the peer sealed before it roams the link. The test
`a_packet_kept_from_before_an_upgrade_does_not_roam_the_link` is the repro's case, and
a NAT rebinding still roams.

**FL3-2 — A slow address could be raced and taken (Low, native).** Marking an address
slow with `set_slow` left it in a running race, and marking the link's own path slow
didn't end the race. So the slow address was probed and could be taken for being
faster. Repro (scratch core test): 18 datagrams went to the slow address, and both ends
ended on it. JS rooms race no direct addresses (theirs are local handles), so only
native hosts that set a slow address were affected. **Fixed** by `8958a23`: marking an
address slow takes it out of the race, and marking the link's own path slow ends the
race (`an_address_marked_slow_during_a_race_is_raced_no_more`: no datagram to it).

**FL3-3 — A relay that drops only data could hold a link with no slow address (Low).**
`RELAY_STALL` (FL2-1) applied only to links with a slow address to go back to. Native
UDP links have none, and neither do JS links on a member relay reached without a pipe
(a fallback dial through a member). A member that forwards probes, pings and acks but
drops data wins the race and keeps the link. Repro (scratch core test): 0 of 3000
messages arrived, and the link was dropped as timed out at 129.5 s. **Fixed** by
`af58aee`: on a relayed link with no slow address, data unacknowledged there for
`RELAY_STALL` (5 s) holds that relay off (`RELAY_HOLD`, 60 s, doubling with each repeat
up to 16 minutes) and races the others at once. Either end takes a winner off a held
relay, past the dwell, and resends its data on the new path. In
`a_relay_that_drops_data_is_left_for_one_that_carries_it` (the repro's case), either
end leaves the relay in 11.1 s, all 3000 messages arrive, and the link stays up.
`7e1160d` races a path taken for being faster once more after the dwell, so a move won
on lost probes goes back.

**FL3-N1 (Info):** a race's score and the round-trip smoothing were plain integers, so
the huge round trip a host clock jump gives could wrap them (release) or panic (debug,
an abort across the C ABI). **Fixed** by `e995440`: both saturate
(`huge_rtts_saturate_in_a_race_and_in_smoothing`).

**FL3-D1 (Info, docs):** `add_relay`, `pw_add_relay` and the FFI README still described
a relay used only from the slow address. **Fixed** by `f4929d9`.

Residuals:
- **FL3-I1 (Info, accepted):** the race measures probes, which are small and look alike.
  A relay that forwards them at once but delays data by just under `RELAY_STALL` wins
  the race and keeps the link, as R1 for links with a slow address. Each message waits
  at most about 5 s; any longer and the relay is held off.
- **FL3-I2 (Info, accepted):** holds are per link and at most 8, as R2.

### Shared docs, `room.doc()` (NB-006) — DOC-1…DOC-7

`room.doc(name)` (`packages/js/src/doc.js`; room tag 7, and 8 when deflated) is a shared
document of maps, sets and logs that every member holds. It is a delta-state CRDT: each
write goes to every member as a delta, and a member that joins, or a doc opened again,
asks one member at a time with its context and is answered with what it lacks. Members
are trusted alike to write, as in the room: any member can overwrite, delete or fill a
doc. What the doc defends is each member's app and resources:
- Every frame is checked field by field and dropped whole if anything is off.
- State lives in Maps.
- Names, keys, values, entries, characters, writers and context ranges are capped
  (`MAX_*`), and frames stay under 512 KiB.
- What a member can have us send is budgeted per member:
  - answers and asks: 8 at once, then one per 15 s;
  - the characters our answers carry, and the deletions its answers have us pass on to
    the others: 8 Mi at once, then 64 Ki a second.

Reviewed by reading the code, with Node probes on fake networks of docs (no wasm):
- NB-006 at `8ebea00`, then its fold-ins `d798c79` and `d6b148c` (merged `c30ab0f`,
  2026-10-27);
- NB-006c at `8b7946c` (merged `45c7cfd`, 2026-10-27);
- NB-006e at `558f53c` and its fixes `ff51ce6` (merged `f40cffa`, 2026-10-28), by reading
  the code and one mutant run of `doc_crdt.mjs`;
- NB-051's fix `0374cb8` (merged `b968f8c`, 2026-10-30), with Node probes on `dbf0f7c`
  with and without it;
- NB-053's fix `0358c1d` (merged `907b771`, 2026-10-30), with `doc_crdt.mjs` run on
  Node 22 and 26 with and without it;
- NB-056's `d6e66f3` (merged `e43391c`, 2026-10-30), with a Node probe of `freezeJson`
  as before and after it.

**DOC-1 — A Lamport stamp far ahead was dropped for good (Medium).** NB-006 refused an
entry whose stamp was more than 2^32 past our clock. Deltas aren't repeated, and an
answer didn't resend it, so the entry never came again:
- one such delta made a joiner miss later writes;
- a few cut it off from everything newer;
- two sent to one member alone muted that member at everyone else.

**Fixed** by `979cdae`: every member takes any stamp up to 2^53 − 1. Residual: a member
can move the stamps to 2^53 − 1 in one frame. From then on, values set at the same time,
set items and log entries are ordered by writer (README, "Member data").

**DOC-2 — An entry forged on our own next write corrupted the doc (Low).** Past
`MAX_INCARNATIONS` (16) moves to a new writer, a member's answer could hold an entry on
the dot our next write would take. The write then landed on it, which left the doc's
index and length wrong, and the app's `delete()` threw a TypeError. **Fixed** by
`eaa2a22`:
- an entry on our own writer that we didn't write is never held;
- writes skip dots our context has;
- once every dot is claimed, writes return false or null with a warning until the doc
  is opened again.

**DOC-2a (Low)** was found in the re-check of `eaa2a22`: each forged dot was passed on to
every other member as its own range, 3.7× what the forger sent with 6 other members and
7.5× with 12. **Fixed** by `d6b148c`: one span per frame, from the lowest forged dot to
the highest (0.01×).

**DOC-2b — A catch-up's deletions were passed on one range per entry (Low).** An answer
that names a writer's dots dead in one range had us send each other member one range
for each entry it deleted here:
- a 620-byte answer sent about 110 KB to each member (1000×);
- a member's own items, deleted in cycles, sent 3.5× (6 others) to 7.0× (12).

**Fixed** by `8b7946c` (merged `45c7cfd`):
- each range of the answer that deleted entries goes on as one range, from the lowest
  dot it deleted to the highest;
- what a member's answers have us pass on is charged to its characters budget, once for
  each member it goes to;
- claims on our own writer's future dots are held as one span (I5), so our writer takes
  at most two ranges of the context; the span is reset with each frame (I4).

**DOC-3 — Frames that cost far more CPU than their size (Low).** On a loaded host:
- `kill()` walked each copy of a range: a 246 KB frame took 3.0 s;
- a full context churned in `makeRoom()`: a 1 MiB frame took 0.49 s;
- trimming a log scanned per entry: a 490 KB frame took 0.27 s.

**Fixed** by `a4fddfd`: dead ranges are merged per writer before the walk, idle writers
are kept in insertion order, and logs keep a heap, so a frame costs about one pass over
what it names (17–82 ms for the same cases).

**DOC-3a (Low)** was found in the re-check: with 1024 live writers and a full context,
`makeRoom()` scanned every writer once per range recorded (a 295 KB frame of separate
ranges, about 0.4 s). **Fixed** by `d6b148c`, which evicts in batches (0.3–2.1× an empty
context).

**NB6-L1 — A doc opened again didn't catch up (lead's review).** In NB-006 as first
written, catching up with a member was a budget per membership. So a doc closed and
opened again, such as a component that remounts, wasn't caught up.

The first fix, `26b7540`, made catching up a rate per member, and cleared what we had
sent a member for each new incarnation of its doc. The asker picks its incarnation, so
security's note showed that any member could then be sent the whole doc in every
answer: theoretical, about 96 MB at once and 0.8 MB/s from each member.

**Fixed** by `3564acf`: what answers carry has a budget of its own per member, the
characters budget above. A doc that remounts catches up every time: a small doc 8 times
in a row at once, then each up to 15 s late.

**DOC-4 — A better routed member could pause a slower one's answer on every turn
(Low).** NB-006e (`558f53c`) asks members by route: linked directly, then relayed, then
over signalling. An answer coming over signalling is paused when a better routed member
is queued, and its member is asked again after. Nothing limited how often:
- a member linked directly that was queued again during each of the slower member's
  turns paused that answer every cycle;
- its asks budget refills at one per 15 s, faster than a 60 s turn passes;
- so the slower member delivered at most 4 pieces a cycle, about 128 Ki characters a
  minute, instead of its answer.

**Fixed** by `ff51ce6` (merged `f40cffa`): a member is paused this way once until an
answer of its ends. After that, the better routed member waits for the turn to pass (5 s
without a piece, or 60 s with another waiting). The doc_crdt test: on `558f53c`, the
member over signalling was asked 8 times over 7 turns, and 67 entries were sent for 44;
on the fix, it was asked twice, and 47 were sent for 44.

Test gap (Info), closed: the test pinned "once", not "again after the answer ends", so a
mutant that never clears the flag passed `doc_crdt.mjs` (its cost would be time only: a
route that becomes direct later waits for the turn to pass). A second doc_crdt test, "paused
again once its answer ended", has the member's next answer give the turn up again: asked
`a,c,a,a,c,a` on the fix, and `a,c,a,a,c` (a failure) with that mutant.

**DOC-5 — Rooms with no doc open inflated deflated doc frames (Low).** NB-006e's `onDoc`
stopped returning early with no doc open. So a room that never opens a doc (a game, a
chat) inflated every deflated doc frame (tag 8) to as much as 16× its size, and parsed
it. **Fixed** by `ff51ce6` (merged `f40cffa`): with no doc open, a deflated frame is
dropped unread. A member deflates only for a doc whose ask said it reads deflated
frames, so such a frame is for a doc we closed. Plain frames are still read at 1×,
because an ask for a doc we don't have open gets an empty answer, to its sender only.
The room_doc test reads 1 frame on the fix and 2 on `558f53c`.

**NB-006e, checked with nothing found:**
- **Answers in pieces.** An answer goes in pieces (32 Ki characters over signalling or
  through a member, 128 Ki linked directly), at most 4 unacknowledged.
  - A MORE moves only the window of the answer it names, and never past what was sent.
  - Each piece is charged its characters before it goes, and an answer's first piece
    takes one answer unit, so the bound is the one above.
  - A piece is never empty unless it ends the answer.
- **The turn.** A member that trickles pieces holds our turn at most 60 s per ask while
  another member waits. One that stalls gives the turn up 5 s after its last piece, and
  keeps it only while no other member can be asked.
- **Inflating.** Inflate stops at min(1 MiB, 16× the compressed size), fed in 1 KiB
  slices, so at most one slice's output is held past the cap. A truncated or junk stream
  is dropped. Frames stay in order per member both ways, and one inflated after its
  member left is dropped.
- **The pipe.** NB-006e left its budget unchanged: 3 batches a second shared, burst 6
  (6 and 12 since NB-006i, [Nostr relay pacing](#nostr-relay-pacing-and-pipe-striping-nb-006h-nb-006i--nb6h-1-nb6h-2-nb6i-1)).
  - A full batch goes at once when a token is free.
  - A pipe timer that fires 250 ms or more late (a background tab) moves flushes to the
    end of the task, until one fires on time.
  - Out of tokens, the pipe waits on a timer.
- **Sync status.** `doc.sync` is as good as the members that told us (README).

**Info:**
- **I1–I3** (NB-006), addressed by `11cf2e6`:
  - change notes were keyed so that map `a` key `b\0c` and map `a\0b` key `c` gave one
    event;
  - the README now says that versions without `room.doc()` (0.1.0 to 0.2.0) drop doc
    frames;
  - a member that let our ask time out is asked again only once it answers, so one that
    never answers holds the others' turns up once.
- **I4, I5** (NB-006c), fixed by `8b7946c`, under DOC-2b.
- **I7** (NB-006e), fixed by `ff51ce6`: an answer under way held the entries the asker
  lacked. A member that never acknowledged kept the doc as it was at its ask, deleted
  values included, one copy per member. The answer now holds ids, and each is looked up
  as its piece is cut.

**Residuals (accepted; the README's "Member data" and "Logs" say them):**
- **DOC-2c (Info):** a member that keeps a log longer than another can miss older
  entries the other trimmed, once it catches up from that member, directly or through a
  member that did (README `72548e2`). Give `keep` the same value on every member.
- **I6 (Info):** the budgets are per member while it stays. A member that leaves and
  joins again, or joins under a new id, starts with whole ones, so N ids get N budgets
  (README `72548e2`).
- **I8 (Info):** a member can make us inflate and parse up to 16× what it sends in
  deflated doc frames (`DEFLATE_RATIO`). That is capped at 1 MiB a frame, plus at most
  one 1 KiB slice's output past the cap, and holds only in rooms with a doc open
  (DOC-5). A frame that would deflate more goes plain, by design.
- **Member trust.** Any member can:
  - claim another member's next writes, so the room ignores them; the claimed member
    moves to a new writer, 16 times;
  - move the stamps to 2^53 − 1;
  - name any writer when it relays a catch-up (`from` is as good as the member that
    told us);
  - make `doc.sync` show `'synced'` early, or `total` too high.

**DOC-6 — An entry type that wasn't a string broke a named map or log (Medium).** Found
by backend while porting the doc to Rust (NB-006b). `parseFrame` checked an entry's
type with `Object.hasOwn(TYPES, type)`, which turns its key into a string, so an array
`["m"]` (or `["l"]`, `[["m"]]`) passed. `collection()` keys by type and name, so `["m"]`
on `c` took the place of map `c`. One delta of about 150 B from any member did this at
every member that took it in:
- sent before map `c` existed, the app's own `map('c').get`, `.entries()`, `.set()` and
  `.delete()` threw `TypeError`, as did members' writes to `c`, and no call could remove
  the entry;
- `["l"]` entries made `log('c').append` throw; on a log with `keep: 2`, three of them
  pushed out the app's own entry, and every later append threw;
- the rest of its frame was lost: a later entry's dot went unseen, and an answer piece
  was never acknowledged;
- answers and `snapshot()` carried it on, so it reached joiners and outlived its sender;
- a device copy holding map `c` and an `["m"]` entry on `c` made `restore()` throw: the
  opening was `'blocked'`, so that device never saved the doc again until `forget()`.

Medium, the class of DOC-1: it lasts, can't be removed, and makes the app's own calls
throw. Not High: it needs a member, which can already overwrite, delete or fill a doc;
the room and the other collections survive, and nothing leaks.

**Status: fixed** by `0374cb8` (NB-051), merged `b968f8c`, reviewed 2026-10-30. An entry
type must be a string before it is looked up, or the frame is dropped whole. Deltas,
answers and `restore()` all go through `parseFrame`. The doc_crdt test sends `["m"]`,
`["s"]`, `["l"]` and `[["m"]]`, on a new collection and on an existing map, and fails on
`dbf0f7c`. No other check in `parseFrame` or `unopened` looks up a table with member
data. `peerwire-sync` (NB-006b) takes only the JSON strings `"m"`, `"s"` and `"l"`, and
its frame corpus holds `["m"]`.

Residual (accepted):
- **R1.** A room poisoned before the fix. A fixed member drops whole any frame holding
  such an entry, so an older member's answer piece holding one is never acknowledged,
  and the entries packed with it never come from that member; a device copy holding one
  is dropped whole like any copy that is off, and the next save replaces it. It clears
  as members update, or with `forget()` or a new doc name. Found by reading the code,
  not probed.

Info (optional, not filed):
- **I9.** If `restore()` ever threw, `rangeCap` would stay raised; a `try`/`finally`
  would reset it. Nothing reaches that path with the fix in.
- **I10.** The test sends deltas only: a `restore()` of a copy with a bad entry isn't
  pinned. It is the same line, so the delta test catches its mutant.

**DOC-7 — A value nested deeply was stringified before its depth was checked (Info).**
Found when CI, which runs Node 22, failed `rust_doc.mjs` on `6c5af8f`. `parseFrame`
measured each entry's value with `JSON.stringify`, and only then checked that it nests
at most `MAX_VALUE_DEPTH` (64) deep. `JSON.stringify` recurses in some engines: Node
22.23 throws `RangeError` on a value 3000 to 5000 deep, where Node 26.7 and Chromium 153
don't at 100000 (measured). `JSON.parse` is iterative in all three, so a member can send
a value far deeper than that: theoretical, 1 MiB / 2 bytes a level (`[` and `]`) ≈
524000 levels. On Node 22, by reading the code:
- a delta or an answer holding one threw out of `parseFrame`. `room.js` drops a doc
  frame that throws, so it cost what any malformed frame costs;
- a whole save on the relays holding one, which anyone with the room key can make, threw
  out of `joinRelays`. That ended the opening's read of the relays: the saves after it
  weren't joined, and no save was made to the relays whose copy lacked what the doc
  holds or expired within a week, until the doc next changed;
- a copy on this device holding one made `restore()` throw, and the opening
  `'blocked'`. No engine saves such a value, since it was dropped on the way in, so only
  a tampered store gets there.

Info: of the engines measured, only Node 22 throws; it needs a member or a key holder,
who can already overwrite, delete or fill a doc; nothing outlasted the opening; and
`room.doc()` hasn't been published. `peerwire-sync` parses JSON with a loop of its own
and checks a value's depth before anything else (NB-006b), so it never had this.

**Status: fixed** by `0358c1d` (NB-053), merged `907b771`, reviewed 2026-10-30.
`parseFrame` checks the depth first, with `freezeJson` (a loop, so no value can overflow
the stack), and stringifies only a value that passed. Deltas, answers, `restore()`,
`joinRelays()` and `lacks()` all go through it, so a copy holding such a value is
refused, not thrown. The doc_crdt test sends a value 100000 deep in a member's frame and
in a copy; with the fix reverted it fails on Node 22, not on Node 26. Since NB-055
(`6e998db`) the strict gate also runs CI's Node tests on Node 22, `rust_doc.mjs` among
them, so a regression like this one fails before a push, not only in CI.
`freezeJson` walked with `for…in` until NB-056: see NB54-I1, under
[`@peerwire/react` hooks](#peerwirereact-hooks-nb-049--rh-1rh-6-nb53-1).

Info (fixed):
- **NB53-I1.** The test's FAIL line stringified the frame, so on Node 22 a regression
  printed a bare stack trace instead. **Fixed** by `8be53e8` (NB-055, merged `6e998db`):
  FAIL lines print frames through `show()`, which catches the throw.

### Docs kept on this device, `persist` (NB-006f) — NB6F-1…NB6F-3

`room.doc(name, { persist })` (`packages/js/src/store.js`, `doc.js`) keeps a sealed copy
of the doc on this device: in IndexedDB with `true`, or in any store of bytes
`{ get, set, delete }`. Opening loads the copy before the first ask, so members send
only what it lacks. The doc saves while it changes, when the page is hidden, and on
`close()` and `leave()`. Nothing holds a room's docs while nobody is in it: a member who
was never there gets the doc from one that kept it, once that one is online.

Reviewed 2026-10-28, by reading the code:
- the design, before it was built;
- the code at `a4c228c` (`035e4d6`, and `52bb7e3` for the design call), with one timing
  script;
- its fixes, `2e71dc5`, at `255ec0e`, with backend's 78 mutants of `doc_crdt.mjs`,
  `room_doc.mjs` and `doc_store.mjs`.

**The seal (security's design call, `52bb7e3`):**
- **Keys.** The room key is `proof.js`'s HMAC key. Each doc's key is
  `HMAC-SHA256(room key, 'pw-store-key' ‖ name)`, and its copy is kept under the hex of
  `HMAC-SHA256(room key, 'pw-store-id' ‖ name)`.
  - Both labels differ from `pw-hello`, `pw-offer`, `pw-rtc-offer` and `pw-link` at
    byte 3, and from each other at byte 9. So the id kept in clear is never a proof, and
    no proof seen on the wire is a key or an id.
  - Labels aren't length-prefixed, so none may be a prefix of another (`proof.js` says
    so).
- **The record** is `0x01 ‖ iv ‖ AES-256-GCM(copy)`, with additional data `0x01 ‖ id`, a
  128-bit tag, and a new 12-byte IV from `getRandomValues` for each save. The key is
  imported non-extractable, for encrypt and decrypt only.
- **Nothing in clear** beside the bytes: database `peerwire`, store `docs`, the id as the
  key and the record as the value.
- **Another version.** A record whose first byte isn't 1 is neither decrypted nor written
  over. The doc says `'blocked'` and keeps no copy, so an older release never destroys a
  newer one's.
- **Names.** With `persist`, a doc name with a lone surrogate throws RangeError: UTF-8
  turns each into U+FFFD, so two names would share a key and a record.
- **Loading.** A record over `MAX_STORED` (64 × 512 KiB + 1 KiB) isn't decrypted. A copy
  that opens is joined as an answer from ourselves: each of at most 64 frames goes
  through `parseFrame` with the doc's caps, and it forwards nothing and asks nobody. A
  copy that is off anywhere is dropped whole, with a warning that says why, and the next
  save replaces it.
- **The writer.** An opening always draws a fresh incarnation with `n` 0, never one from
  the copy, and draws again if the copy names the one it drew. So a copy put back after
  newer writes makes no dot hold two values: the doc_crdt rollback test.

**NB6F-1 — A copy the opening couldn't read was written over (Low).** When the store
failed to load the copy, the doc said `'blocked'`, but its next save replaced the copy
with what the doc held. That save came with the next change, or on a close during the
load. So one stroke drawn after a storage error replaced the whole board. If no member
held the board, the case persistence is for, it was gone. The README's file store
answered "no copy" to every read error and wrote the record in place, so a crash
mid-save left a truncated record, which the next opening replaced.

**Fixed** by `2e71dc5`:
- An opening is `blocked` when the store fails to load the copy, takes `LOAD_MS`, or
  holds another version's. A `blocked` opening never writes the copy, with a change or
  on close; `forget()` still deletes it.
- In the README's file store, only ENOENT means "no copy", and a save writes a temp file,
  then renames it.
- Tests: `doc_crdt` (no write on a change, after the store is back, on close, or on a
  close during a load that fails) and `room_doc` (a directory where the record was).

**NB6F-2 — Every save wrote the whole doc, once a second (Low).** Any change started a
save of the whole doc within `SAVE_MS` (1 s). A full doc is a 12.1 MiB record, and one
save took 55 ms of main thread on a loaded host (`docCopy(memoryStore())`). So one small
write a second from any member kept every member with `persist` writing about 12 MiB a
second, some 43 GiB an hour. Saves also queued up behind a store slower than that.

**Fixed** by `2e71dc5`:
- One save at a time. The next waits `max(SAVE_MS, bytes / SAVE_RATE)` after the last
  ends, with `SAVE_RATE` 256 KiB/s: 2 s for a 510 KB whiteboard, 48 s for a full doc. The
  changes made during a save are saved once, after it.
- Hiding the page, `close()` and `leave()` still save at once, behind any save in
  flight.
- The doc_crdt test: a full doc changed every 100 ms for 150 s saved at 1.0, 49.4, 97.8
  and 146.2 s, then at once on close. With the first save held, 10 s of writes called
  the store no more; once it ended, they were saved once, 1 s later.

**NB6F-3 — A store that never answered held catch-up for good (Low).** The doc's asks
and answers wait for the copy, and only the store's promise ended the load. With a store
that never answered (a stuck network drive, an IndexedDB whose callbacks never fire), the
doc never asked or answered anyone: it never reached `'synced'`, and members asking it
got nothing.

**Fixed** by `2e71dc5`: `LOAD_MS` (5 s) after opening, the doc goes on without the copy.
It is `'blocked'`, so it never writes the copy (NB6F-1). It asks and answers, and drops a
copy that comes later. A doc closed meanwhile closes then, where before its `close()`
and `leave()` never resolved.

**Info:**
- **I1**, fixed by `2e71dc5`: operations are ordered per store object, and the README
  made its file store inline, so a remount wasn't ordered after the closing save. The
  README now makes the store once, and `DocOptions` says to pass the same object.
- **I2**, fixed by `2e71dc5`: an IndexedDB connection the browser closed (site data
  cleared) was never opened again. `onclose` now drops it.
  - Fixed further by `a29ce7c` (NB-077), merged `45a793b`, reviewed 2026-11-03. The
    close event comes some time after the close, and a save made between them used the
    closed connection, so the doc warned once that it couldn't keep a copy. Worse, in
    Chromium only the request could fail while its transaction never ended: that doc
    never saved again, with no warning.
    - A request that fails now fails the save. A save that fails as the connection
      closes (`InvalidStateError`, `UnknownError` or `AbortError`) is tried once more,
      on a new connection, `REOPEN_MS` (250 ms) later.
    - The retry puts the same sealed bytes, so no AES-GCM nonce seals a second
      plaintext. It runs in the doc's turn (`queued`), so no later save or `forget()`
      overtakes it, and a try that failed never applied.
    - A full disk (`QuotaExceededError`), a refused open (private mode) and a retry
      that fails still fail at once. A late close event drops only its own connection.
  - **NB77-I1**, fixed by `1fc707a` (NB-078), merged `9f165e6`, reviewed 2026-11-03. A
    save started at the old connection's close event opened a new one at once, while the
    browser was still deleting the site's data. That open was aborted, and wasn't
    retried, so the doc could still warn once: 5 of 5 runs on loopback, with the page
    busy across the clear. The next save kept the copy.
    - An open that fails with `AbortError` is now tried once more, `REOPEN_MS` later. A
      refused open (`InvalidStateError`, as in some private modes) still fails at once,
      and a store call opens at most twice.
    - The doc_store test forces this order. It also fakes opens aborted every time (2
      tries), refused (1 try) and aborted for 100 ms (the second opens).
    - Noted: an open whose upgrade transaction aborts (a first open, as a full disk may
      make it) fails with `AbortError` too, by the spec, so it is also tried once more,
      250 ms later. Bounded as above.
- **I3**, fixed by `2e71dc5`: a comment by `VERSION` says to bump it whenever an older
  release's `restore()` would refuse what `snapshot()` now writes.
  - **I3a**, fixed by `1b0fc35`: `parseFrame` and `restore()` ignore fields they don't
    know, so an older release would save such a copy back without them. The comment now
    also says "or read only in part".
- **NB6F-2a**, fixed by `b45e15c` (NB-006f2), merged `76bdd64`: a store that kept
  failing (a full disk) was retried every `SAVE_MS` while the doc changed, and each
  retry built and sealed the whole doc before the store refused it: CPU only, about
  60 ms a second for a full doc.
  - After a failed save, the next now waits `SAVE_MS` doubled for each failure in a row
    before it, 6 times at most: 1, 2, 4 … 64 s, then every 64 s. A save that succeeds
    resets the count.
  - Hiding the page, `close()` and `leave()` still save at once.
  - A member can't reset the count for us: that takes a save that succeeds, paced by
    the doc's size, and refilling the doc costs the member the same bytes.
  - The `doc_crdt` test: a full doc changed every 100 ms with saves failing is tried at
    1, 2, 4, 8, 16, 32, 64, 128, 192 and 256 s in 300 s.
- **I4**, accepted, documented in `index.d.ts` by `1b0fc35`: `close()` and `leave()`
  resolve only once a save in flight settles, so a store whose save never settles holds
  them. `leave()` has closed the network by then, and `@peerwire/react` doesn't await it.

**Residuals (accepted; R1–R4 are in the README's "What a copy keeps secret, and from
whom"):**
- **R1.** A copy is exactly as secret as the roomId. An app that keeps the roomId in the
  URL (history, bookmarks) or in `localStorage` gives it, and with it the copy, to anyone
  who can read the device's browser profile. The seal protects a store taken on its own:
  a copied IndexedDB, a file from a file store, a backup.
- **R2.** A copy is authentic only against someone without the roomId. Any member can
  make a copy that opens, as it can send an answer, so a copy is checked as member data.
- **R3.** A copy's id is the same on every device for one app, room and doc: two stores
  show they hold the same doc, but not which one.
- **R4.** A copy's size and the time of its last save show. Someone who can write the
  storage can put an older copy back; the doc then catches up from the members and
  reuses no dot. `forget()` deletes the record, but storage can keep old bytes on disk or
  in backups.
- A store that works but takes over 5 s to load makes that opening `'blocked'`: its copy
  isn't updated in that session, and isn't lost.
- Two tabs of one browser in one room keep one copy of each doc, whichever saved last.
- A page killed while visible loses its last few seconds of writes on this device: up to
  48 s for a full doc. The members that heard them still have them.

### Nostr relay pacing and pipe striping (NB-006h, NB-006i) — NB6H-1, NB6H-2, NB6I-1

`NostrSignaling` (`packages/js/src/nostr.js`) paces each relay on its own, and since
NB-006i sends each pipe batch through one relay, the relays taking turns. Before, one
`rate-limited:` refusal paused a relay for 30 s, doubling to 10 min, and every event
went through every relay. So a join's burst against relays allowing 2–3 events a second
paused all three, and the joiner never linked.

Reviewed 2026-10-29 by reading the code, with fake-clock probes of our own code on fake
sockets (no relay):
- NB-006h: ui's `9f26bd1` and the lead's fold-in `c1b3878`, merged `71f5c6e`;
- NB-006i: ui's `956f390` (striping) and `4d8f313` (the pipe's rate), the lead's
  `cf4a0a9` (close) and `dc156a3` (NB6I-1, with one guard of the lead's), merged
  `6a60ef0`.

**Pacing:**
- **Per relay.** Each socket publishes from its own queue, through a token bucket of
  `RELAY_RATE` 4 a second with a burst of 8. A queue holds at most 64 events
  (`RELAY_QUEUE`), dropping the oldest, and each waits at most 10 s. At most 64 sent
  events wait for their OK.
- **A rate refusal** (`rate-limited:`) halves that relay's rate, to no less than 0.5 a
  second, and the rate grows back by one a second every 5 s. It holds that relay for
  1 s, doubling to 30 s only while it refuses again before it takes one. The event is
  sent again, up to `RELAY_TRIES` (3) times in all.
- **A ban** (`blocked:`, `banned`) stops that relay for 30 s, doubling to 10 min,
  starting over after 10 quiet minutes, and drops its queue.
- **Never all held.** After each hold, ban and lost socket, `release()` cuts short the
  hold that ends first if no open, unbanned relay is free. So a join or a pipe always has
  a relay to go through, at that relay's rate.
- **The clock.** All pacing runs on `performance.now()`. Wall time is used only for an
  event's `created_at` and the REQ's `since` (NB6H-1).

**Striping (NB-006i).** A lossy event goes through one relay. A lossy event is a pipe
batch, whose datagrams the core resends if they are lost. Everything else goes through
each relay: hellos, offers, answers, candidates, the Bye and the `close()` batches.
- **The choice.** `next()` takes, from `turn` on, the first open relay that has neither
  banned nor held us, isn't silent (NB-006j, below) and can take the event within
  `HOLD_MS` (1 s). If there's none, the open, unbanned relay that answers and is free
  soonest takes it; with only silent relays left, the same rules pick among them. Once
  closed, a relay that answers and isn't slowed comes first, then a slowed one, then a
  silent one; one holding us comes last, since it is sent nothing.
- **`lossy` is a hint** (`opts?.lossy === true`). BroadcastSignaling, and any adapter
  that ignores it, sends every way, as before.
- **A refused lossy event** is one object with one count of tries, sent through the next
  relay each time: at most 3 sends in all. Before NB-006i it was up to 3 per relay, 9 in
  all. It keeps its id, so if a relay refused a copy yet delivered it, `seen` drops it.
- **Lost events.** A lossy event queued on a relay that closes or bans us is lost, and
  the core resends what it carried.

**The pipe** (`webrtc.js`).
- All pipes together send 6 batches a second, a burst of 12 (`PIPE_RATE`,
  `PIPE_BURST`). Before `4d8f313` it was 3 and 6.
- A batch holds at most 16 KiB or 64 datagrams, and waits on `wait()` first, at most a
  second at a time.
- Each pipe queues at most 128 KiB (`PIPE_QUEUE`; 64 KiB before), dropping the oldest.
- With three relays, each carries about 2 pipe batches a second. When every batch went
  through every relay, each carried 3, 9 in all.

**Checked, with nothing found:**
- **Relay-controlled input.** Only a relay's OK messages drive the pacing
  (`answered()`), and holds, bans and buckets are keyed by our own socket's URL. A relay
  can hold, slow or ban only itself. An OK for an id we don't know starts a hold only on
  that relay, and only if none is running. So a relay can push its share of batches onto
  the others, by refusing, banning us or closing, but can't pull more.
- **Reconnects.** A redialled socket goes to the end of the list, which moves the turns
  by one. A relay gets no turn while its socket is closed (at least `RECONNECT_MS`, 1 s),
  so cycling its connection gains it nothing.
- **Metadata.** Sizes, the `p` tag and timing are unchanged. Each relay sees about a
  third of a pipe's batches instead of all of them (NB43-I4).
- **Load.** The per-relay buckets bound every relay, whatever the pipe's rate. With one
  relay open, `wait()` holds the pipe to that relay's 4 a second.
- **`close()`.** What waits goes at once, past the buckets, but not to a relay that has
  banned us or holds us.
  - Lossy events waiting on a held relay are moved to a free one first (`cf4a0a9`).
    They are moved, not copied, so none goes twice.
  - With every relay held, a moved event lands on a held relay and stays unsent, as
    before.

**NB6H-1 — A wall clock set back stalled every relay (Low).** `9f26bd1` paced on
`Date.now()`, and filled the buckets with `now − at` unclamped. A wall clock stepped
back (NTP, the phone's network time, a restored VM) put every bucket in debt for as long
as the step. Past a point it also made a relay's rate negative, and its pump timer then
fired every millisecond and sent nothing. Repro (a scratch probe: a fake socket, a fake
clock, no network): with the clock set 60 s back, 1 event was sent and the timer fired
over 1200 times in 1.5 s, against 6 sent and 10 calls without the step. **Fixed** by
`c1b3878`: the pacing runs on `performance.now()` (the pipe's tokens too), and `rate()`
clamps elapsed time at 0 and the rate at 0.5 a second. The probe then gives 6 sent and
11 calls, and `nostr_bounds` 5b steps the clock back.

**NB6H-2 — The pipe asked `wait()` once per timer (Info).** Two effects:
- (a) `wait()` charged the relay's whole queue, including events about to expire;
- (b) while every open relay had banned us, the pipe waited out the ban (up to 10 min),
  even after another relay opened.

**Fixed:** (b) by `c1b3878`, since the pipe waits at most 1 s and then asks again; (a)
went into NB6I-1.

**Info, fixed by `c1b3878`:**
- **I1:** `close()` sent what waited to a relay that held us for its rate, past its hold.
  Once closed, a held relay is now sent nothing and arms no timer.
- **I2:** `SignalingAdapter` in `index.d.ts` now has `wait?()`.

**A pipe batch dropped at close (found in the lead's check, before merge).** With
striping, a batch waiting on a relay held for its rate is the only copy. At `close()` a
held relay is sent nothing (I1), so a leaving peer silently dropped that batch.
**Fixed** by `cf4a0a9`: `close()` first moves such a batch with `stripe()` to a free
relay.

**NB6I-1 — The relay in turn held the pipe for its whole queue (Low).** On `956f390`,
`wait()` was the time until the relay whose turn it was could take one more event: its
queue at its own rate.
- That queue holds a copy of every signalling event, and its rate is what its refusals
  left it, down to 0.5 a second.
- So one relay that refused an event now and then, strict or hostile, kept the pipe idle
  behind it while the others were free, up to `RELAY_QUEUE_MS` (10 s) a turn.
- A hostile relay could also black-hole what it did take.

Repro: a scratch probe measuring our own code's cost.
- Setup: a fake clock and timers, and three fake sockets that answer OK.
- Load: a saturated pipe at 6 a second that asks `wait()` at least every second, and 8
  signalling events every 5 s.
- Relay c refuses every k-th event; 30 s simulated.

| relay c refuses | `956f390` | with the fix |
|---|---|---|
| never | 6.37 batches/s, longest gap 0.41 s | 6.37/s, 0.41 s |
| 1 in 6 | 2.80/s, 4.04 s | 5.83/s, 2.14 s |
| 1 in 8 | 2.50/s, 4.81 s | 5.90/s, 2.21 s |
| 1 in 12 | 4.17/s, 5.49 s | 5.90/s, 2.16 s |

**Fixed** by `dc156a3`:
- **Passing over.** `next()` passes over an unheld relay that can't take the event
  within `HOLD_MS`, as it passes over a held one. With none left, the one free soonest
  takes it.
- **Push, not pull, still holds.** A relay is passed over only for its own hold, its own
  lowered rate, or its own queue at that rate.
- **At close** (the lead's guard), any relay not holding us takes the batch, however
  long its queue, because everything goes at once and a held relay is sent nothing.
  Without the guard, the batch could land on a relay held for less than `HOLD_MS` and be
  dropped, as before `cf4a0a9`. NB-006j replaced the guard with its ranking, which
  still puts a held relay last at close.
- **Tests,** `nostr_bounds` 5c:
  - with a relay at 0.5 a second and 8 queued, `wait()` is 250 ms, against 18 s without
    the fix;
  - at close, with that relay free and the other two held, the batch goes through it.

**Residuals:**
- **A relay that answers OK and delivers nothing** (accepted, by design) gets 1/n of the
  pipe's batches, n being the number of open relays.
  - It gets more only while the others are held, banned, closed or slowed, each by its
    own doing. Nothing it does pulls more.
  - The core resends what is lost. `doc_catchup` (h), with the joiner deaf to one of
    three relays, takes 17.1 s in the gate on `6a60ef0` (8.3–19.1 s in earlier runs);
    the bar is 60 s.
  - Before NB-006i the other relays carried a copy of each batch, so such a relay cost
    nothing.
- **A half-open relay (Low)**, fixed by NB-006j, below.
- **Relays of different rates** (accepted). While each can take the next batch within
  `HOLD_MS`, they still take equal turns, so the pipe moves at about the slowest relay's
  rate times n.
- **Adapters without `wait()`.** One that ignores `lossy` and sends to several relays
  puts the pipes' whole 6 a second, burst 12, on each (`index.d.ts` says so).
- **Memory** (accepted). 256 pipes (`MAX_PIPE_PEERS`) of 128 KiB is 32 MiB raw, about
  43 MiB as base64. Only our own core's paced sends fill it.
- **The close burst.** At close, a relay gets its queue (at most 64) and the pipe
  batches moved to it. The pipe's budget holds those to about 72: a burst of 12, plus
  10 s at 6 a second. Before NB-006h, everything went out at once anyway.
- **Every relay banned.** `wait()` lasts until the first ban ends, up to 10 min. Nothing
  could be carried meanwhile.

**A half-open relay (Low).** With striping, a socket still `OPEN` whose relay was gone
(a half-open connection, a stalled relay) took its turns until the browser closed it,
and what it took was lost. Before NB-006i the other relays carried a copy of each batch.

**Fixed** by `659eb28` (NB-006j), merged `5ae3c4e`:
- **Silent.** A relay is silent once an event sent to it `SILENT_MS` (5 s) ago has had
  no OK since, taken or refused. The mark (`ws.asked`) is the time of the first EVENT
  sent since its last OK, on `performance.now()`. Any OK clears it, the REQ doesn't
  set it, and a new socket starts without one, except one that replaces a deaf socket
  (NB-006k, below): it starts silent, so it takes no pipe batch until its relay answers.
- **Turns.** While a relay that answers is left, `next()` and `wait()` pass over a
  silent one. Its next OK gives its turns back. With only silent relays left (a frozen
  tab waking with its OKs unread), they take turns as before, so nothing stalls.
- **Nothing new is sent.** A silent relay still gets every non-lossy event, which is
  also how it gets the chance to answer. There's no new timer, and the state is one
  number per socket.
- **A relay can only make itself silent.** It can't mark another relay silent or pull
  more than its 1/n. An on-path attacker who drops honest relays' OKs could already
  drop those relays.
- **NIP-01 relays answer OK within a round trip,** so this needs no wire change. The
  three default relays answered a hello in 0.2–1.2 s.
- **Tests.**
  - `nostr_bounds` 5d: a socket that never answers loses its turns 5 s after its first
    event and gets them back on an OK; one answering at 4 s keeps them; the order at
    close is pinned.
  - `doc_catchup` (i): relay-3 goes half-open as the joiner links. No pipe batch lands
    on it more than `SILENT_MS` after its first unanswered event, and the whole board
    arrives in 8.5–15.1 s (bar 30 s), against 32 s, 39 s or a stall past 60 s on
    `ddf8321`.
  - 29 of 29 mutants caught.

**Info, accepted:**
- **NB6J-I1.** `release()` counts a silent relay as free. With every relay that answers
  held and one silent, no hold is cut short: the pipe waits on the first hold to end
  instead of writing into the silent relay. A relay holds us only by its own refusals:
  1 s (`HOLD_MS`) after an OK that took an event, and up to 30 s (`MAX_HOLD_MS`) only
  after refusals in a row. The core resends what is late.
- **NB6J-I2, the receive side (fixed by NB-006k, below).** If our own socket is
  half-open, its subscription delivers nothing, and senders can't tell. Their batches
  through that relay were lost to us (1/n, as in `doc_catchup` (h)) until the socket
  closed. Our socket did show as silent.
- **NB6J-I3.** A relay that never sends OK loses its pipe turns for good, though it
  keeps every other event. One that dropped a single OK gets them back with the OK for
  our next hello (`HELLO_EVERY_MS`, 15 s).
- **NB6J-I4.** Any OK clears the mark, even one for an id we don't know. Taking only
  OKs for ids still in flight would add nothing against a hostile relay, which knows
  the ids it took: it is the accepted "answers OK, delivers nothing" relay above.

**Deaf sockets, NB6J-I2.** **Status: fixed** by `0d40053` and `f4a89a4` (NB-006k),
merged `b26e869`, reviewed 2026-10-31 by reading the diff, with a probe on fake sockets
and a fake clock (loopback).
- **Deaf.** A socket is deaf once it has heard nothing from its relay, no OK and no
  event, for 10 s after a non-lossy event we sent through it (`ws.unheard`). Lossy pipe
  batches and stored events (a doc's relay copy, the inbox below) don't arm it, and any
  frame clears it.
- **Redial.** A deaf socket is closed and its relay dialled again. The new socket starts
  silent, so it takes no pipe batch until its relay answers.
- **Never in a loop.** The same relay's next redials wait 20 s, then 30 s. After 3 there
  are none until it answers an event within 5 s (`stats.redials`). Real relays echo our
  hello, which our REQ asks for, so an answering relay clears the mark without an OK.
- **Tests.** `nostr_bounds` 5g, with fake sockets and a fake clock running the timers;
  `doc_catchup` (i) counts the socket that went half-open.

**Info, fixed by NB-058** (`d76ed0b`, merged `2f68aa8`, below):
- **NB6K-I1.** A relay that answers the first event on each new socket in time, then
  nothing, gets its redial budget back each time: 20 redials in 10 min, with no end
  (measured on `ce411db` with the probe; before NB-006k there were none). Fixed: a
  timely OK restores the budget only on a socket dialled `MAX_RECONNECT_MS` (30 s) or
  more ago. In the probe, 3 redials in 10 min.
- **NB6K-I2.** A stray "so" in the README; `index.d.ts` says `reconnects` counts closes,
  but a deaf redial counts too.
- **NB6K-I3.** `nostr_bounds` 5g's fake relays don't echo our hello, as real ones do.

**The reconnect flap (Low).** Found in the NB-006k review; on main before it too.
`dial()`'s `onopen` deleted the relay's back-off, so a socket that opened and closed
was dialled again after `RECONNECT_MS` (1 s) every time: the doubling to
`MAX_RECONNECT_MS` covered only dials that failed before opening. Each reopen fires
`onReconnect`, which `room.js` answers with a hello on every open relay.
- **Repro** (fake sockets and clock, Node 22, measured again on `ce411db`): a relay that
  accepts each socket and closes it 10 ms later is dialled 56 times in 60 s, every
  1.06 s, with no end, and 56 hellos go out on a healthy relay, against about 4 a minute
  from the 15 s hello timer.
- **Impact.** About 56 connections a minute from one client for as long as it lasts.
  Relays ban an IP for too many connections; on a shared IP (a hotspot, carrier NAT)
  that hits everyone behind it. Each other relay gets an extra hello a second, a quarter
  of `RELAY_RATE`. Only the relay or the path to it can cause it, not a peer.
- **Fix:** `onopen` keeps the back-off, and `lostSocket()` starts over at 1 s only
  after a socket stayed open ≥ `MAX_RECONNECT_MS`. Reopen hellos then follow the
  back-off: at most 2 a minute at the cap. In the probe, 5 dials in the first minute
  instead of 56.

**NB58-1 — A connection 'change' gave the flapping relay its 1 s back (Low).** Found in
the NB-058 review, so never on main. The page took every `navigator.connection`
'change' as a new network and redialled each relay not open, its wait deleted. Chrome
fires 'change' when its speed estimate moves too, the type the same, so the wait the
flap fix keeps was thrown away each time. With a 'change' every 60, 30, 10 or 5 s, the
probe's flapping relay was dialled, and the room said hello, 6, 10, 24 or 36 times a
minute (57 to 60 before NB-058). Fix: only going online, a new connection type (the
rule `room.js` uses) or the page showing again redials; a 'change' with the same type
does nothing. Then it is 2 a minute at every rate of 'change', and a new type still
redials at once.

**NB58-I1 (Info).** "Open 30 s" was measured from the dial in `answered()` but from the
open in `lostSocket()`. The docs now say "dialled 30 s ago" where that is what counts.

**Status: fixed** by `d76ed0b` (NB6K-I1…I3), `df01209` (the flap) and `4609955`
(NB58-1, NB58-I1), merged in `2f68aa8` (`ui/NB-058-reconnect` at `4609955`), reviewed
2026-10-31. The tests are `nostr_bounds` 5g and 6, with fake sockets and a fake clock;
10 of 10 mutants, the author's and two of ours, are caught on `4609955`.

**Residuals (accepted):**
- **Going online, or the page showing again,** redials at once and starts the wait
  over, so a relay that drops each connection is dialled once more at each such event.
- **A deaf socket's redial** leaves the relay's reconnect wait as it was.
- **A relay that holds each socket 30 s or more** and then closes it starts the wait
  over each time, so it is dialled about every 31 s, with a hello each time: 2 a
  minute, as at the cap.
- **Desktop Chrome has no connection type,** so there a 'change' never redials. A real
  network change is still caught by 'online', the socket closing, or the deaf check.
- **Each real change of type** (Wi-Fi to cellular and back) redials at once and starts
  the wait over at 1 s.

### Docs kept on the signalling relays, `relays` (NB-006l) — NB6L-1…NB6L-6

`room.doc(name, { relays: true })` (`packages/js/src/relays.js`; `nostr.js` `query()`
and `store()`) keeps a sealed copy of the doc on the room's Nostr signalling relays, so
a member who opens it while no other member is online still gets it. The design is
route 1 of [STORE-DESIGN.md](offline-store-design.md), with security's design review (NB-006g-1
to -10) written into it. Its "Threats" are the copy's threat model, and the README and
`index.d.ts` state them for apps.

Reviewed 2026-10-30, by reading the code, with Node probes on the test's own fake relays
(loopback, no public relay):
- the design at `d31fffa`, then its revision `a6e1897`;
- the code at `87697c0`, then its fixes `564e68a` and `8d6d24a` (merged `55b47c5`).

**Checked and holding:**
- **Keys and seal.** The keys follow STORE-DESIGN's byte layout, with prefix-free
  labels, and the signer's fallback is deterministic. A piece is sealed with additional
  data `VERSION ‖ d` and its slot, and the VERSION byte is checked before decrypting.
- **Reading is bounded whatever a relay sends.** Kind, pubkey and slot are filtered, and
  the newest event per slot per relay is kept. Content is at most 60 KiB, and inflating
  stops at the lesser of 1 MiB a piece and what is left of the read's `MAX_STORED`; a
  read cut there isn't complete. Identical content is opened once. A read ends at EOSE,
  CLOSED, its 10 s timeout, socket loss, a ban or close.
- **Joining.** Only whole saves are joined, each frame through `parseFrame` (DOC-6's
  guard included), all or nothing. Deletions joined from the relays aren't passed on.
- **Writing, per relay.** The bank, the `created_at` floor (which skips events more than
  15 minutes ahead) and a VERSION hand-off are decided per relay from its own events.
  Empty pieces go only into occupied slots.
- **`store()`** writes behind signalling, at most one event a second per relay, and each
  write settles exactly once: OK, refusal, rate retry, ban, timeouts, eviction, socket
  loss or close.
- **`forget()`** stops saving, waits for a save under way, then wipes the slots and
  sends a NIP-09 deletion.
- **`room.js`** takes `relays` only with `signaling: 'nostr'`, deflate support and a
  roomId shaped like `randomRoomId()`'s, and refuses a doc name with a lone surrogate.

**NB6L-1 — A save whose read no relay completed was dropped (Low).** `cycle()` cleared
`pending` before the read. When no relay's read completed (offline, the sockets
reconnecting after a network change, or no EOSE within 10 s), it found nothing to write
and returned with nothing due. Nothing saved again until the doc changed, and the
page-hide save skipped it too. In the probe, an author alone drew 5 strokes, and every
relay withheld EOSE at the save timer, then recovered: no event an hour later, nor
after a page hide. Low: it costs the copy's durability, not its secrecy, and a roomId
holder can't cause it; but a phone switching networks at the timer is common.

**Status: fixed** by `564e68a`, merged `55b47c5`, reviewed 2026-10-30. With no relay
complete, the save stays due, with the announcements it was checking, and goes at the
next 5-minute slot or on a page hide. Tests: every relay withholds EOSE at the save,
and the next slot saves; every relay is down, and a page hide saves.

The lead's review found a twin in the same spot (Info), fixed with it: a `t: 5` during
our read, from a member the read then found had never saved, still stopped that save.
In the probe the save came 620 s after the member's first `t: 5`, and 305 s with the
fix. Members now ignored are dropped from the announcements, and the deferral ends
when none is left.

**Info, fixed by `564e68a` and `8d6d24a`:**
- **NB6L-2.** The check at a deferral's end asked only whether the announced save `s`
  was whole on a relay, not whether it was new. A member could read the current save's
  `s` and announce it; each time its `t: 5` landed during our read, we wrote nothing,
  and it was never ignored (no event in an hour, in the probe). Now a `t: 5` naming a
  save our latest read found whole, or our own, has its member ignored at once. A fresh
  `s` is 64 random bits, so an honest saver's is never caught.
- **NB6L-3.** The reader doesn't check signatures, so a relay could serve an honest
  piece with `created_at` a year ahead, and every joiner's `sync.at` stayed there.
  `sync.at` now leaves out events more than 15 minutes ahead (it is the read's time when
  every piece is). Each relay's bank ranking is unchanged.
- **NB6L-4.** The set of ignored members lost a member only when it joined again, so one
  that left for good stayed in it for the doc's life. It is pruned on leave.
- **NB6L-5.** The writer didn't hold a piece to the 16 frames readers take (packing
  makes about 9 today). `pieces()` now splits past it, so a change to packing can't
  write saves no reader takes.
- **NB6L-6**, found in the re-check of `564e68a` and fixed by `8d6d24a`. A read no relay
  completed emptied the list of saves already made, our own included, so one replay of
  our last save's `t: 5` deferred our next save by up to 5 minutes. Such a read now adds
  to the list instead of replacing it.

**Residuals (accepted):**
- **Availability** (STORE-DESIGN "Threats", README): anyone with the roomId can stop the
  relay copy for good, by putting another VERSION's pieces in both banks on every relay,
  or by overwriting it over and over. Device copies (`persist`) stay the main store.
- **The design's other threats** stand as STORE-DESIGN states them:
  - the roomId is a permanent, offline read key to every save a relay or an archive
    keeps, and can't be revoked;
  - anyone with it can write or delete the copy;
  - a relay can serve an old save or none;
  - relays see one stable key per doc, piece sizes, save times, and the IP of each
    member that saves or reads.
- A page-hide save on a closing tab rarely finishes (it reads first), so the relays can
  miss the last writes made alone. `leave()` and `close()` don't save to the relays.
- Each opening reads every relay once, so a component that remounts `room.doc()` reads
  again each time.
- Entries joined from the relays aren't pushed to the members online: they reach a
  member through its own read, or when it asks us.
- A member that never answers keeps the doc `'loading'`, and no relay save happens until
  that ends.
- Two members announcing within the same second can both save. On each relay the other
  bank keeps its complete save, and the next round repairs it.
- An honest member's `t: 5` that arrives after our read already found its save whole
  (its lane backed up) has us ignore that member until it rejoins. We may then save
  beside it; nothing is lost.
- A member that announced, then left mid-save, is checked at the next read, so it is
  added to the ignored set after it left: one stale entry per such member until it
  rejoins.

### Shared docs in Rust, `peerwire-sync` (NB-006b, NB-052) — NB6B-1…NB6B-4, NB52-1, NB52-2

`crates/peerwire-sync` is `room.doc()` in Rust, for Godot (`open_doc`) and native apps
(the C ABI's `pw_doc_*` and `pw_docs_*`, behind the `sync` feature, off by default). It
speaks `doc.js`'s frames (room tag 7, JSON) with the same caps and budgets, so the
threats and residuals of [Shared docs](#shared-docs-roomdoc-nb-006--doc-1doc-7) apply as
written there. What differs:
- **The star.** In Godot, joiners link to the host alone, so the host (the hub) passes
  on what each joiner's frame added, once to each other member, never back to its
  sender.
- **No deflate and no store.** Tag 8 is dropped unread, and `t: 5` drops its frame
  (NB-006l's relay copy is JS only). The snapshot is plain JSON, and the docs say to
  keep it private.
- **A queue per member.** Frames wait in `Docs` until the host sends them. Since NB-052,
  a member whose queue passes `OUTBOX_BYTES` (16 MiB) is caught up afresh (NB6B-4,
  below). One whose link takes nothing across two of those, and for 30 s, is forgotten
  and reported as `Overflow`, which the Godot binding answers with a disconnect.

Reviewed 2026-10-30, by reading the code, with in-process release probes on archives of
the branch: `eb85263..24a01a7` and `6be6ad6`, then the fixes `14e67e8..4d6f9a2` (merged
`6c5af8f`). NB-052 was reviewed 2026-10-31 the same way: `73b3de7..96102e3`, then the
fixes `96102e3..1113cac`.

**Checked and holding:**
- **Parsing.** The JSON reader parses, writes and drops with loops over a flat node
  list, so no input can overflow the stack. It slices only at character starts and
  reads bytes as `TextDecoder` does; a 1 MiB frame takes 26–76 ms. An entry type is
  only the JSON string `"m"`, `"s"` or `"l"` (DOC-6), a `t` outside 1–4 drops the frame,
  and a delta's writers must be its sender's.
- **Caps and budgets** match `doc.js`: asks and answers, 8, then one per 15 s;
  characters, 8 Mi, then 64 Ki a second; 4 pieces unacknowledged. Making room holds the
  range and writer caps, `kill()` walks the fewer of the dots and the entries, and
  timers re-arm at least 1 ms ahead.
- **The hub** takes doc frames from joined members only. After `Overflow`, a member's
  frames are dropped until it joins again.
- **The C ABI** checks strings as UTF-8 and returns `PW_ERR_INVALID` for a closed or
  unknown doc. An id is at most 89 bytes, into a buffer of at least 96. A needed size
  comes back as −16 or lower, so it never reads as a `PW_ERR_*` code, and a buffer too
  short keeps the message or event for the next call.
- **Godot** takes tag 7 on the reliable lane only, and only from members (the host and
  its joiners); it can't collide with Godot's own tags 0 and 1.

**NB6B-1 — One joiner could get every other joiner disconnected (Low).** The hub passed
on what a member's frame added with no budget. A member receiving the relay was cut once
16 MiB waited for it, but the writer never was: nothing goes back to it. Values go on in
canonical form, so the relay can outgrow the frame: `1e20,` (5 bytes) goes on as 22.
- In the probe, a hub with 7 members whose links took nothing, and one writer
  alternating a kill frame with about 1 MB of new set entries, cut all 7 after 5.0 MB
  in (4.2× queued per member per byte), and after 17.6 MB with values `0`.
- A link that drains is cut too, once the relay outruns the host's uplink for long
  enough. Theoretical: with 20 Mbit/s (2.5 MB/s) up and 7 joiners, one joiner writing
  200 KB/s asks for 200 KB/s × 4.2 × 6 ≈ 5.0 MB/s, so each of the other 6 queues about
  0.42 MB/s and passes 16 MiB in about 40 s. The writer stays in.

**Status: fixed** by `834ef90`, merged `6c5af8f`, reviewed 2026-10-30. The hub charges
what it queues for a member's frame, once per recipient and as written, to that member's
relay budget (`RELAY_BYTES`, 8 MiB at once, then `RELAY_BYTES_PER_S`, 64 KiB/s, kept in
integer µs), before queuing it. Once the budget is overdrawn, the writer is forgotten
and reported as `Overflow`: the host cuts the writer, and its readers stay.
- Asks and the host's own writes aren't charged. A budget is kept across a rejoin and
  pruned once whole. `OUTBOX_BYTES` stays for a link that stalls.
- The sim test: a writer at 2 MB/s is cut alone, a reader taking 500 KB/s keeps at most
  `RELAY_BYTES` queued, and all converge.
- GODOT.md says when the host cuts a joiner, including one that brings a large copy the
  host lacks.

**NB6B-2 (Info, accepted).** A map key's values are a `Vec` that a removal scans, so
removals are O(n²): 10,000 values on one key (one 249 KB delta), then one kill range,
cost 93 ms, where parsing any 1 MiB frame costs 26–76 ms. Left as is on purpose:
`doc.js` holds them the same way, and their order goes into deltas byte for byte as
`doc.js` writes them. An ordered set that keeps it would cost about 13 MB more at 10,000
keys. NB-052 left it as is: a fix that keeps the order isn't cheap.

**NB6B-3 (Info, test gaps), fixed by `4d6f9a2`:** the frame fuzzer now reaches the hub's
doc, so the relay is fuzzed (655 relays in 20,000 cases, with rejoins after a cut);
frames with `t` 5, 0 and 6 are in the vectors, dropped by both sides; and the debug fuzz
run, where integer overflow panics, builds `peerwire-ffi --features sync`, in the gate
and in CI.

**NB6B-4 — A joiner that rejoins, or brings fresh keys, still gets stalled readers cut
(Low).** Found in the re-check of `834ef90`, a gap in the fix security asked for: the
frame that overdraws a budget is applied and relayed before it is charged, and budgets
are per key. So each join lands up to a doc's worth (`MAX_DOC_LENGTH`, about 4 MiB) on
every reader. With 7 readers whose links take nothing:
- one key coming back at once, each time sending set adds and a kill of its last items
  in one frame, cut all 7 after 7 rejoins and 7.52 MB in;
- fresh keys, each sending about 0.94 MB of `1e20`s and killing the last key's items (a
  kill may name any writer), cut all 7 after 5 keys and 4.70 MB in;
- one key with no rejoin holds: 3.94 MiB reaches each of 7 readers, and 11.82 MiB a lone
  one, both under 16 MiB.

Nothing paces rejoins (`PER_SOURCE` caps only half-open handshakes, and only under
load), and a host keeps up to 32 joiners by default, so holding the invite is enough.

**Status: fixed** by NB-052 (`ops/NB-052-hub-resync` at `1113cac`, before any native or
Godot release; merged in `143923f`), reviewed 2026-10-31. A reader's link is no
longer cut over the relay:
- **Catch-up afresh (resync).** Past `OUTBOX_BYTES`, a member's queue is dropped, and
  each doc catches it up as if it had left and joined again (`Doc::resync`). It sends
  an empty end for the member's latest ask, or the member would wait on that ask and
  never ask again; the dots it knows are dead, since its ask can't show the member a
  dropped delete of an entry it holds; then its ask, so the member asks back and
  catches up on the paced path. Each is a frame `doc.js` already sends, so readers are
  unchanged.
- **The cut** (`Overflow`) stays for a link that took nothing across 2 resyncs and for
  30 s. `poll_transmit` isn't told the time, so a take is timed by the next call that
  is: a clock that stood still can only make the 30 s longer.
- **The flooder** is still charged before anything is queued, and cut on every join.
- **Test.** `tests/flood.rs` ports the probe: 7 real readers at 2 MB/s and at 0 B/s, 8
  fresh keys, and one key back 19×. Every reader stays linked and holds the hub's
  ~4 MB doc within a minute; with the code from before NB-052, all are cut.

**NB52-1 — A member could resync itself and refill its catch-up budgets (Low).** Found
in the re-check of `96102e3`, fixed by `3749a67` before the merge, so never on main.
`Doc::resync` ran `left`, which drops the member's `answers`, `asks` and `chars`
budgets. A member that asked, then acknowledged every piece (`MORE` needs a piece
queued, not received) while its link took nothing, overflowed its own queue, got fresh
budgets, and asked again, with no reconnect.
- In the probe (release, the clock frozen so nothing refills), 87.7 KB sent had the hub
  build 319.5 MiB of answers, with 20 resyncs, in 79 ms, against a budget of 8 Mi
  characters (at most 24 MiB as UTF-8).
- Fix: a resync keeps `want`, `answers`, `asks` and `chars`. The same probe stops at the
  burst, 23.9 MiB, with 1 resync. `a_member_resynced_keeps_its_budgets` and 5 mutants
  hold it.

**NB52-2 — A dropped reply to an ask for a doc not open left that ask open for good
(Low).** Fixed by `ba693d0` before the merge. Our empty end for an ask about a doc we
hadn't open went straight into the queue, and no doc recorded it. Once a resync dropped
it, a reader asking us alone marked us stalled; when we later opened the doc and asked
it, it never asked back, so it never got what the doc held, and stayed `Loading`.
- Fix: each member's queue keeps our latest such reply per doc name, for the last 16
  names (a name is at most 128 UTF-16 units), and a resync sends them first. A stale
  one is ignored, since a reader takes an end only for the ask it names.
- Test: `a_reply_for_a_doc_not_open_goes_again_when_dropped`, the probe's case.

**Info, fixed before the merge:**
- **NB52-I1** (`1113cac`). What a resync queues isn't capped, so it can't recurse: up to
  about 1.5 MB for each doc open at its caps (*theoretical*: (`MAX_RANGES` +
  `MAX_ENTRIES`) dead ranges at 48 bytes, `MAX_WRITERS` at 80, and an ask of
  `MESSAGE_BYTES`). With a dozen such docs a resync alone passed `OUTBOX_BYTES`, and
  every frame after it resynced again. Now a member whose resync alone queues more than
  half of `OUTBOX_BYTES` is forgotten (`Overflow`).
- **NB52-I2** (`da4fe8b`). `queued()` says a resync can queue an ask per doc for the
  other members too.

**Info, with NB-052** (done: `a4fdadc` corrected both texts, and `ee85cb0` makes
`gate.sh`'s `rust_build` fail if any build failed):
- An honest joiner that answers a host lacking a big copy pays for that relay: with 7
  joiners it is cut after about 1.3 MiB, and each rejoin while in debt delivers one more
  piece (GODOT.md says so). The recommended fix helps here too.
- `lib.rs`'s "one member alone never fills a queue" holds per join (8 MiB plus one frame
  of up to about 4 MiB, under 16 MiB), not across rejoins, and GODOT.md's "about 1.3 MiB
  at once" leaves out the frame that goes past it.

**Residuals (accepted):**
- Parsing a 1 MiB frame briefly holds 16–50× its size in memory.
- A host can hold 16 MiB of doc queue for each joiner before it catches it up afresh,
  plus up to 8 MiB that the catch-up queues (NB52-I1).
- The snapshot is plain JSON, as designed: whoever can read where an app keeps it reads
  the doc.
- **(1) A slow link can still be cut.** One that takes less than one message (at most
  512 KiB) per 30 s, about 17 KB/s, is cut by a flood that lasts 30 s and 2 resyncs
  (about 32 MiB to it).
- **(2) A flood restarts a slow reader's catch-up** at each resync. It converges once
  the flood stops.
- **(3) `synced` early.** A reader whose ask was open shows `synced` after the empty end
  (`doc.js`'s "every member answered") while its data still catches up.
- **(5) Many docs bring the cut back.** A resync's frames grow with doc state any member
  can write: one frame can name the dead dots of 1000 made-up writers, 709 KB a doc in
  the guard's test. On a hub with about half a dozen docs open at their caps
  (*theoretical*: 8 MiB / ~1.5 MB a doc), a member that fills them and then floods gets
  each reader cut at its first resync, NB6B-4's outcome, for such hubs only. GODOT.md
  says to keep a host's docs few. Before NB-052 any such flood cut readers.
- Residual (4), a resync resetting the budgets, was withdrawn: it was NB52-1.

### Offline messages to a PeerId, `openInbox` (NB-019) — NB19-1…NB19-5

`openInbox()` in `@peerwire/js` (`src/inbox.js`) leaves a message for one PeerId on the
Nostr relays while it is offline, as Protocol Zero's dead drop did. Rust and Godot
parity is NB-019b. The design:
- **No recipient tag.** A tag the recipient could ask the relays for is one anyone who
  knows its PeerId could compute and watch (SEC-5). So a drop carries only its app's
  namespace, `d` = hex(SHA-256(`peerwire/drop/` ‖ appId)), and a NIP-40 `expiration`.
  An inbox reads all of its app's drops and trial-opens each: one X25519 a drop.
- **Sealed and padded.** The envelope (version, kind, the app's namespace, a random
  16-byte id, when it was sent and expires, its length, the body) is sealed to the
  recipient with Noise X (`ep.seal`, as signalling seals), which carries the sender's
  PeerId encrypted and authenticated, and padded to 256 bytes. Each drop is kind 30078,
  signed by a fresh Nostr key.
- **Reading.** Each relay is asked for the app's drops from `since` (the start of the
  last complete read, less 15 min, rounded down to 10 min) to `until` (15 min ahead, or
  the cursor reading down), at most `MAX_READ` (100) a check, newest first. A full page
  goes on down at the next check.
- **Once.** SHA-256(from ‖ id) is kept until the message expires (4096 at most, the
  soonest to expire forgotten first). With `persist` it is saved sealed, as `store.js`
  seals a doc's copy, before listeners hear the message. Its first 16 bytes in hex are
  the message's `id` for the app.
- **Caps.** 16 KiB a message. Content over 22,360 characters isn't decoded. A message
  dated more than 15 min ahead, sent for over 7 days, expired, or JSON nested past 64 is
  dropped.

Reviewed 2026-10-31 by reading `b26e869..797afbc`, then the fixes `797afbc..fa25c82`,
with probes on `inbox.mjs`'s own fake relays (loopback, Node 22).

**Checked and holding:**
- Noise X authenticates the sender, and nobody else can open a drop. A recipient whose
  key leaks can be impersonated to itself (key-compromise impersonation, X's known
  limit).
- The envelope parser holds its bounds, and `MAX_CONTENT` is checked before base64 is
  decoded.
- The time checks agree with the dedupe: a message is dropped at `exp ≤ now`, when its
  kept entry goes, so a replay can't slip in between.
- Signalling and the inbox reject each other's sealed bodies by their first byte.
- The saved state is AES-GCM under keys from HMAC(secret, label ‖ appId), so it can't be
  read or mixed up across apps.
- The inbox's signalling sends no `#p` REQ (`listen: false`), so no peer tag reaches a
  relay. Nothing is in clear on a drop or in a read.

**NB19-1 — One relay could hide every drop the others deliver (Low).** `read()` skipped
an event whose `id` was already opened from an earlier relay. That id is the relay's
word (only its shape is checked), so a relay first in socket order that served each real
id with junk content had the honest copies skipped; once the marks moved past the drop,
nobody read it again. In the probe, with that relay first of three, nothing was
delivered in 40 checks. Fix: a read opens each content once, whichever relay serves it.

**NB19-2 — A flood in a message's second hides it (Low, accepted).** NIP-01 has no
cursor within a second: a relay sends a second's events lowest id first, and a full page
inside one second goes on below it. So 100 drops or more left in a message's second
with lower ids (their sender chooses them) hide it on that relay for good.
- In the probe, 99 junk drops a relay: delivered at once; 100: not in 60 checks.
- Doing it on every relay costs 100 × relays events a message (*theoretical*), so a
  flood that follows an app's messages live can keep them from their recipients.
- Paging the stuck second would make that about 10× dearer and cost an honest reader of
  a flooded relay up to about 22 MB a check (*theoretical*: 1000 drops × 22,360
  characters). The lead accepted it instead, and the README and `index.d.ts` say so.

**NB19-3 — A stranger's JSON nested deep enough to throw in the app (Low).** A JSON
message was delivered at any depth. A 16 KiB body nests 8150 deep, and on Node 22
`JSON.stringify` and `structuredClone` of it throw `RangeError`, so an app that logs,
stores or renders a message threw in its listener (an uncaught exception in Node).
Anyone can send to any PeerId. Same class as DOC-7. Fix: a value nested past 64 is
dropped, by `doc.js`'s iterative walk, now shared as `withinDepth`; `send()` throws.

**NB19-4 — A drop copied into another app's namespace was read there (Low).** The
envelope didn't name the app, and the README tells apps to reuse a room's identity. So
anyone could copy a drop's content from app A's namespace into app B's, and B delivered
A's message, authenticated, to code not written for it. Fix, while `VERSION` 1 is
unreleased: the envelope carries the app's 32-byte namespace inside the seal (its header
grows from 30 to 62 bytes, still one 256-byte block), and a mismatch is dropped.

**NB19-5 — Junk dated far ahead topped every read (Low).** The first page had no
`until`. strfry refuses events more than 15 min ahead, but a relay that takes them
would serve them first forever: in the probe, 500 such drops made each message wait 5
or 6 checks. Fix: every read asks `until` at most 15 min ahead, past which nothing is
read anyway. Then each message took 1 check.

**Status: fixed** by `5a3fb65` (NB19-1, 3, 4, 5 and NB19-2's wording) and `c173890`
(I1, I2), before NB-019 merged, so never on main (`backend/NB-019-offline-msg` at
`fa25c82`, merged in `e6c9684`). Reviewed 2026-10-31. Each fix has a test in
`inbox.mjs` and a mutant. The probes for NB19-1, 4, 5 and 3 pass on `fa25c82`, and each
fails with its fix undone.

**Info, folded in:**
- **NB19-I1.** Past 4096 kept, a delivered message comes again if anyone posts it
  again, and anyone can force that: 4096 throwaway messages, then the old content
  posted again. Listeners now get the message's `id`, the same each time, so an app
  can drop it.
- **NB19-I2.** The `since` sent was exact to the second of the reader's last read, so a
  relay could link a reader's sessions across IPs by it. It is now rounded down to
  10 min.

**Residuals (accepted):**
- **The flood (NB19-2),** above. Otherwise a flood delays delivery but can't lengthen a
  check: at most 100 drops opened a relay.
- **Coming again (NB19-I1),** which the app can drop by `id`. Without `persist`, a
  message still on the relays comes again in each new session.
- **Clock skew.** A sender whose clock runs more than 15 min behind may be missed by an
  inbox that had already read past that time.
- **What a relay sees:** the IP and rhythm of senders and readers, and the app's volume.
  Rounding `since` narrows linking by timing; it doesn't remove it. Outer event
  signatures and ids aren't checked, and nothing trusts them: Noise X authenticates.

### Calls, `room.call()` (NB-010, NB-010a) — NB10-1, NB10-2, NB10a-1

`room.call(stream, { max })` in `@peerwire/js` (`src/call.js`) is a voice and video
call among a room's members, each linked to every other (a mesh): `max` is 6 by default
and 16 at most. The meet demo (`demo/meet.html`, NB-010a) is built on it. The design:
- **Signalled over the room's link.** Each peer in the call gets an `RTCPeerConnection`
  for media alone. Its descriptions and candidates are room frames of tag 9, JSON on
  the reliable lane, sealed and authenticated like any room message. They act only from
  a proven member, and `sdp` and `ice` only from one we linked. No call frame is a
  relay event of its own: on a pipe they go inside it, sealed. Old JS peers drop tag 9.
- **Media bound to the PeerId.** Each DTLS fingerprint travels in a description sealed
  by that peer's session, so neither a signalling relay nor another member can sit in
  the middle of a call.
- **Fixed transceivers.** The smaller PeerId offers one audio and one video transceiver,
  and descriptions follow perfect negotiation. Extra m-sections in a remote description
  are set inactive before we answer, and only the first audio and first video
  transceiver reach the app. A stream set later replaces tracks and never renegotiates.
- **Caps** (`parseCall`). A frame of 64 KiB; a description of 32 KiB with at most 4
  m-sections and 64 `a=candidate:` lines; a candidate of 512 characters starting
  `candidate:`; a mid of 32 characters; an index from 0 to 3. Only known fields are
  copied. A malformed frame is dropped and counted, and spends no budget.
- **Budgets per peer** (`BUDGETS`, on `performance.now()`). `join` 6, then 1 per 5 s
  (each makes a connection, whose ICE asks STUN); `sdp` 8, then 1 per 3 s; `ice` 64,
  then 4 a second. Past them a frame is dropped and counted.
- **Privacy.** `joinRoom` refuses `'relay-only'`, so a call runs under `'direct'` or
  `'public-only'`. Under `'public-only'`, descriptions go through
  `withoutHostCandidates` and trickled candidates through `candidateFor`, the data
  links' MIC-1 and MIC-5 filters.
- **No retry loop.** A peer whose media hasn't connected 20 s (`CONNECT_MS`) after its
  link was made fails (`'no-path'`) and stays failed until one side joins again. One
  that lost ICE gets 2 restarts in 60 s, then fails.
- **Seats.** A member in the call tells every room member (`join`, with how long it has
  been in, `age`), so each knows who is in (the roster). Joining with `max` in already
  is `'full'` at once. A member is seated once a link has connected and none to a
  member that was in before it is still connecting. One not seated gives way at
  capacity to a member that joined before it; a seated one never does.

Reviewed 2026-10-31 by reading `888e0d6..78a464b`, then the fixes `78a464b..41f4021`,
and the meet page `c578263..97bdf7d`, then its fix `97bdf7d..f979384`. Probes ran in
headless Chromium with fake devices, on loopback with local signalling (Node 22).

**Checked and holding:**
- `parseCall` parses JSON in a `try`, checks every cap before use and copies only known
  fields, so no prototype key reaches a connection; `age` must be a safe integer. Its
  m-line count is at least the number of sections Chromium splits.
- Budgets go when a peer leaves the room, a link's timers clear when it closes, and
  nothing is sent after we leave the call.
- The meet page writes names and reasons as text. It asks for media only from a click;
  mute disables a track, and turning the camera off stops it. A member's presence
  (`{name, mic, cam}`) speaks only for its own media, and tiles are bounded by the
  call's seats.

**NB10-1 — One room member could keep every newcomer out of the call (Low).** A joiner
not yet seated left on a `full` from any roster member claiming an earlier start, and it
is never seated when that `full` lands: `full` answers its `join` in one hop, while a
seat needs an answer, ICE and DTLS (up to 20 s against a member that never answers). So
any member, which is anyone with the invite, could answer each joiner `full` and lock
the call to whoever was in it. The joiner saw `'full'` and couldn't tell who sent it.
In the probe, a joiner got `'full'` 19 ms after calling, both links still connecting.
Fix: a `full` counts only when the joiner's own roster has no seat for it either
(`roster.size >= max`, the test `start()` makes). The joiner then stays and connects (in
45 ms in the probe), and a claimed `age` matters only at capacity.

**NB10-2 — A member that sent nothing got our media unseen (Low).** `'stream'` came
only from `ontrack`, which a browser fires only for m-sections the remote sends on. A
modified client with recvonly transceivers got every member's audio and video, yet no
member got `'stream'` for it, and `call.stream(peer)` stayed null: an app drawing tiles
on `'stream'`, as the README did, showed nobody there. In the probe it decoded 88 or 89
of a member's video frames and about 300 audio packets in 6 s, unseen. Fix: `show()`
builds the stream from the first audio and first video transceiver's `receiver.track`
once the peer's description applies, whatever it sends, and the README draws a tile on
the first `'state'`.

**NB10a-1 — A camera asked for before Leave came on after it (Low).** The meet page's
`acquire()` awaited `getUserMedia`, then added what it got to whatever stream was
current. Leave wasn't disabled meanwhile, and `stopMedia()` put in a new stream, so a
grant that landed after Leave turned the camera on behind "Your camera and microphone
are off", and Rejoin sent it to the call. In the probe (`getUserMedia` held until after
Leave), the camera was live 2.5 s after Leave and went out on Rejoin. Fix: `stopMedia()`
bumps `mediaEpoch`, and `acquire()` stops what it got if the epoch moved.

**Status: fixed** by `c578263` (NB10-1, NB10-2, NB10-I2) and `41f4021` (NB10-I1) before
NB-010 merged, so never on main (`backend/NB-010-calls` at `41f4021`, merged in
`869e4cf`), and by `f979384` (NB10a-1) before NB-010a merged (`ui/NB-010a-meet` at
`0ac7547`, merged in `4f681b4`). Reviewed 2026-10-31. Each fix has a test: `call.mjs`,
`call_browser.mjs` part 4, and `meet.mjs` with `getUserMedia` held past Leave. None of
the three probes reproduces on the fixed heads.

**Info:**
- **NB10-I1 (fixed).** The `ice` budget didn't count candidates inside a description,
  and 32 KiB holds about 760 minimal `a=candidate` lines (*theoretical*: 32,768 / 43
  bytes). A description with more than 64 is now dropped and counted. 64 leaves room
  for one made again after a rollback on a host with many interfaces.
- **NB10-I1a (accepted).** With that cap, a member can add remote candidates to our
  connection at most 64 (the `ice` burst) + 8 × 64 (the `sdp` burst) = 576 at once,
  then 4/s + 64 / 3 s ≈ 25 a second (*theoretical*, from `BUDGETS` and the cap). Before
  the cap it was about 8 × 760 at once. The browser paces its connectivity checks, so
  the STUN it sends doesn't grow with that count: no amplification.
- **NB10-I2 (fixed).** The docs said nothing went through the signalling relays. That
  holds on a direct or member-relayed link, not on a pipe, where call frames go inside
  it, sealed, and a relay sees their sizes and timing (a burst of descriptions when
  someone joins). The header and README now say so.
- **NB10-I3 (accepted).** A member with several PeerIds in the room (the room key is
  all it takes) can send `join` from each and fill the roster, so every newcomer gets
  `'full'` at once. Members are trusted alike under the room key.

**Residuals (accepted):**
- **Trust between members.** Any member can refuse to link a newcomer (that pair fails
  `'no-path'`) or fill the call with its own PeerIds (NB10-I3), but can't take a linked
  member's seat. A claimed `age` counts only at capacity and between members not
  seated.
- **The last seat.** Two joining for the last seat at the same moment may both go, or
  one stay unlinked from a member that turned it away (that pair fails `'no-path'`).
- **Calls that meet.** After a room splits and joins again, its two calls stay partial
  meshes, each member linking at most `max − 1`.
- **No TURN.** A pair with no direct path fails `'no-path'` after 20 s and isn't retried
  until one of them joins again. An app can pass a TURN server in `iceServers`. Since
  NB-098 a call's connections follow the room's policy, set at join or live (NB98-1,
  fixed): a live change gives them its servers and candidate types, restarts their ICE
  when it turns a WebRTC path off or brings new servers, and fails one still on a path
  now off 20 s later (`'no-path'`).
- **Addresses.** Under `'direct'` with a microphone or camera grant, call members get
  every local address, as data links do. Under `'public-only'`, a member on the same
  network can still see a local address as a peer-reflexive candidate from our ICE
  checks, as for data links ("Not covered" in
  [Addresses we give out](#bounds-and-residuals-from-reviews-nb-014nb-043)).
- **What the room learns.** Every room member, in the call or not, learns who is in it.
  A relay carrying a pipe sees the sizes and timing of the call frames inside it.
- **The meet page on the public site.** It stays off it: the owner accepted the shared
  origin on 2026-11-01 with the meet page kept out. Before it ever goes, see the frame
  check and NB10a-I1 under the
  [site's section](#the-docs-and-demos-site-nb-016-nb-067--nb16-1-nb16-2-nb67-1).

### The docs and demos site (NB-016, NB-067) — NB16-1, NB16-2, NB67-1

`scripts/site.sh` builds a static site into `target/site/`: a landing page; the docs
listed in `docs/README.md`, which is their index and sidebar, and `CHANGELOG.md`,
`CONTRIBUTING.md` and `SECURITY.md` from the repo's root, rendered to HTML by our own
renderer (`scripts/site/markdown.mjs`); the demos that need no server of ours (index,
chat, poll, arena, Net Lab); and 12 pages at the first site's URLs (`README.html`,
`docs/THREAT-MODEL.html`, …) that refresh to where each page is now. The README and the
package READMEs are no longer rendered. `.github/workflows/pages.yml` deploys it to
GitHub Pages at `avinashid.github.io/peerwire/`, run by hand only. It was first deployed
on 2026-11-01, from `be989d4`, without the meet page; the docs layout (NB-067: sidebar,
"On this page", prev/next, build-time highlighting, the logo) came with NB-066. The
design:
- **No third-party code at runtime.** No CDN, font, analytics or remote image; a remote
  image in a doc (a badge) becomes its alt text.
- **The policy is a `<meta>` CSP.** Pages sends no headers we choose, so each page
  carries its own: the demos get `docker/examples-nginx.conf`'s, less
  `frame-ancestors` (a `<meta>` policy can't set it), and the other pages allow no
  script: `default-src 'none'`, with styles from the site and images from it or
  `data:`, so no script, inline style or handler. Their menus are `<details>`, and the
  landing page's code tabs are CSS radio buttons. No page sends a referrer.
- **Versioned assets.** Every module, the wasm, the stylesheet and the images live under
  `assets/<id>/`, `<id>` being 12 hex digits of a SHA-256 of their contents. A demo
  page's `<script src>` is the only rewrite.
- **Images.** A doc may show only a file under `docs/assets/`, named `[\w-]` with an
  image extension. The build refuses a symlink there, to a file or a directory, and
  `docs/assets` itself as one or under one (NB67-I2), so it copies only files the repo
  holds. An SVG there is served from the site's origin with no policy of ours, so the
  build (`checkSvg`; NB67-1) refuses one that holds:
  - an element outside a list of 20 shapes, groups and paint servers (`svg`, `g`,
    `path`, `rect`, `use`, `linearGradient`, `stop`, `mask`, …), so no `script`,
    `foreignObject`, `iframe`, `a`, `image`, `style` or animation;
  - a namespace but SVG's (and xlink's, as `xmlns:xlink`), or an element with a
    prefix;
  - an `on*` handler, a DTD, a character reference (`&#`), a processing instruction
    other than the XML declaration, a backslash, or a character outside printable
    ASCII, tab, LF and CR;
  - a reference to anything outside it: `href` and `url()` name only `#ids`, and no
    URL scheme, `javascript:`, `data:` or `//` appears but in the `xmlns`.
- **Only files the repo holds.** Since NB-082 the same goes for every file the build
  reads or publishes: `docs/README.md` and each page it lists, all of `docs/`, the
  demo pages and every module they load (each checked before it's read), the repo's
  wasm and `site.css`. A symlink there, or a directory on the way from the repo's root
  that is one, fails the build (`inRepo`; NB73-I1). A wasm named by `--wasm` is taken
  as given; Pages (`scripts/site.sh`) builds without it. A doc's path must be `[\w-]`
  names and slashes, with `.md` (NB73-I2), so its links need no URL-encoding.
- **Highlighting.** Code is highlighted at build time: `highlight.mjs` wraps escaped
  text in `<span class>` from a fixed set, and the test checks that the output's text
  is the code, for every fence in the docs and for 400 random inputs × 10 languages.
- **Redirects.** Each of the first site's URLs is a page with a
  `<meta http-equiv="refresh">` to a page of the build, a link to it, and `noindex`.
  The targets are a fixed list, not taken from the URL, and the link check covers
  refresh URLs.
- **Links.** The build fails on any link that isn't relative, http(s), mailto, or a `#`
  anchor that exists. A link to a repo file that isn't rendered goes to GitHub.
- **Workflow.** `workflow_dispatch` only, with no inputs. `permissions: {}`, then
  `contents: read` for the build job (with `persist-credentials: false`) and only
  `pages: write` and `id-token: write` for the deploy job, in the `github-pages`
  environment.

Reviewed 2026-10-31 by reading `2f68aa8..47ce300`, then the fix `6107674`, with one
probe of the renderer (Node 22). The docs layout (NB-067, `6ba7cd9..a2d4029`, merged in
`2735268`) was reviewed on 2026-11-01 by reading the diff, with a loopback probe of
`checkSvg` in headless Chromium for NB67-1, and its fixes were re-read the same day,
with the probes re-run on each. NB-073's fixes for NB67-I1 and NB67-I2
(`cecbb72..9d3396d`) were reviewed the same day by reading the diff, with a probe of
`buildSite` on a copy of the tree (Node 22), on this machine only. NB-082's fixes for
NB73-I1 and NB73-I2 (`5e6dba4..06623cd`) were reviewed on 2026-11-03 the same way, the
probe building `git archive` trees of main and of the branch. Nothing was sent to
Pages or GitHub.

**Checked and holding:**
- The renderer escapes text, code, fence languages, heading ids, table cells, alerts and
  link text (`& < > "`), and attributes always use `"`. Its slots are whole elements,
  replaced in one pass, so nothing reaches an attribute. `safeUrl` runs after
  backslash-unescaping, and entities pass only as text, so `javascript&#58;…` becomes a
  relative path. A `<br` followed by anything but `\s*/?>` stays text.
- `link()` turns every link that isn't http(s), mailto or `#` into a repo file, or
  throws, so `//host`, and `\\host` (which browsers read as `//host`), can't leave the
  site.
- The demo policy sits right after `<meta charset>`, before every `<style>` and the one
  module script, and the build throws on any other script.
- Invite secrets ride in the fragment (`#key=`, `#room=`, chat's `#id`), so neither
  Pages nor its CDN sees them in a request.
- `site.sh --serve` binds 127.0.0.1.
- The layouts escape every string they put in a page: titles, groups, headings and
  their ids, descriptions, a redirect's target, and since NB-073 every link to a page
  (sidebar, prev/next, the overview's cards) and the edit link, whose path goes through
  `encodeURI` first. `docs/README.md` may list only `.md` files inside the repo, and a
  doc missing from it or listed twice fails the build. Since NB-082 a doc's path must
  be `[\w-]` names and slashes, so `%`, `#`, `?`, quotes and spaces fail it by rule;
  before, `#` or `?` failed it at the link check, which finds no file at the sidebar's
  link to such a doc, so its edit link can't name another file on GitHub.
- `logo.svg` and `favicon.svg` hold paths, shapes and a gradient, and refer to nothing
  but their own `#pw-g`.

**NB16-1 — Third-party actions built what Pages would serve (Low).** The build job ran
`dtolnay/rust-toolchain@stable` (a branch) and `Swatinem/rust-cache@v2` (a movable
tag), and its output was deployed as is. Whoever moved either ref could choose the JS
and wasm served to every visitor, with no commit in this repo, as happened to
`tj-actions/changed-files`' tags in March 2025. rust-cache also restored compiled
dependencies that other runs wrote. Fix: the toolchain comes from the runner image's
rustup (`rustup toolchain install stable --profile minimal --target
wasm32-unknown-unknown`), with no build cache, so only GitHub's own `actions/*` are
left. `site_build.mjs` fails on any other `uses:`, or one not on a major version or a
full commit.

**Status: fixed** by `6107674`, merged `888e0d6`, before the site was ever deployed.
Reviewed 2026-10-31 by reading the workflow. The guard's 7 mutants are all caught.

**NB67-1 — `checkSvg` misses a script with a namespace prefix (Low).** An SVG opened
on its own is an XML document, where an element's namespace, not its spelling, makes it
a script: `<s:script>`, with `xmlns:s` bound to the SVG namespace, runs, and so does
`<h:script>` (XHTML) inside `<s:foreignObject>`. `checkSvg` looks for the text
`<script` and `<foreignObject`, so both pass it. The build copies an SVG under
`docs/assets/` to `assets/<id>/img/`, served from `avinashid.github.io` with no policy,
so a link straight to the file would run its script on the shared origin, with the
demos' storage (above). It takes such a file merged into `docs/assets/`, which is the
case the check is there for. Repro, on loopback: both forms pass `checkSvg` (at
`46b8ad0`), and their script runs when headless Chromium opens the file from a
127.0.0.1 server as `image/svg+xml`; the plain `<script>` control is refused. An
embedded XSLT stylesheet (`<?xml-stylesheet href="#x"?>`) didn't run in that Chromium,
and is refused already. Fix: refuse any element name with a prefix
(`/<\/?[A-Za-z_][\w.-]*:/`), which the logo and favicon pass; refusing character
references (`&#`) and processing instructions other than the XML declaration also
closes the spellings a regex can't see through. Found on 2026-11-01 in the review of
NB-066 before its push.

The re-reads found two more forms, reproduced the same way. At `41c91ef`, which refused
the prefix, `&#` and processing instructions: an XHTML element needs no prefix in the
default namespace, so `<g xmlns="http://www.w3.org/1999/xhtml">` around
`<iframe srcdoc="&lt;script&gt;…">` passed, since nothing in it spells `<script`, `&#`
or a URL, and a srcdoc frame runs with its parent's origin; so did the same under an
`<html>` root. At `a7da8c2`, which allowed only listed elements and SVG's namespace:
U+1680 and U+FEFF are XML name characters that JS's `\s` matches, so a prefix of `g`
and U+1680 read to every rule as an allowed `<g`, and hid a `<g…:script>` in the SVG
namespace. The fix is all three steps: no prefix, `&#` or processing instruction but
the XML declaration (`41c91ef`); only 20 drawing elements, and no namespace but SVG's
and xlink's (`a7da8c2`); and only printable ASCII, tab, LF and CR, so that `\s` and
`\w` agree with XML's names (`4386eec`). With no DTD, `&#`, processing instruction or
other character, every element of the parsed file is a literal `<name` in the source,
where the list sees it. `site_build.mjs` holds each form, and taking out the
allow-list, the namespace rule or any of `4386eec`'s three rules makes it fail.

**Status: fixed** by `41c91ef`, `a7da8c2` and `4386eec` before NB-066 merged, so never
on main or on the site. Reviewed 2026-11-01 by reading the diffs and re-running the
loopback probes on each: every form above is refused, and the logo and favicon pass.
Residual: the check reads the source with regexes, not an XML parser; it holds because
what it allows has one reading, as above.

**The shared origin (Low): accepted by the repo owner on 2026-11-01.**
`avinashid.github.io` is one origin for every Pages site of the account. `github.io` is
on the Public Suffix List, so other accounts' sites are kept apart, but any script on
any of this account's sites can:
- read the demos' storage: Net Lab's last 6 named rooms (a room's name is its secret)
  and the nickname;
- `window.open` a demo, which is same-origin, and script it: read the key from
  `location.hash`, or speak in the room as the visitor;
- if the account ever has a user site (a repo named `avinashid.github.io`; there is
  none today), register a service worker at scope `/` that serves its own JS for every
  `/peerwire/` URL and outlives any fix.

The owner chose to keep the site on `avinashid.github.io`, with no custom domain for
now (NB-061), so the account's other Pages sites share this origin, and the points above
are accepted while it stays there. The meet page stays off the site: it joins only with
the owner's approval and the frame check below.

**NB10a-I1 (Info), the camera.** Browsers keep a camera or microphone grant per origin.
Once the meet page is on the site, a visitor's "allow" there is a grant to
`avinashid.github.io`, so every Pages site of the account could then use that visitor's
camera or microphone without asking again, for as long as the browser keeps the grant.
With the meet page off the site this doesn't arise for the camera; NB16-2 is the same
case for the microphone, which a page on the site already asks for (accepted, below).

**NB16-2 — Net Lab's microphone grant goes to the shared origin (Low).** Net Lab is on
the site, and its "Use microphone" button calls `getUserMedia({ audio: true })` and
closes the microphone at once: Chrome offers a page's local addresses only while the
microphone is allowed (MIC-1). The page says the grant is the site's, for later visits
too. On the shared origin that site is `avinashid.github.io`, so once a visitor allows
it and the browser keeps the grant:
- any of the account's other Pages sites can open that visitor's microphone with no
  prompt (the browser still shows that it is recording);
- every WebRTC connection those sites make carries the device's local addresses.

Keeping the meet page off the site doesn't cover this. Repro, by reading:
`packages/js/demo/netlab.js` `useMic()`, and `scripts/site/build.mjs` `DEMOS`, which
lists `netlab.html`. Nothing was run against the live site. Fix, while the site stays
on the shared origin: the site build leaves Net Lab's microphone button out and the
page skips what needs it (Net Lab works without the grant), with a `site_build.mjs`
check that no page on the site calls `getUserMedia`; the Docker and local copies keep
the button. An origin of the site's own fixes it with the rest. Found on 2026-11-01
while recording the owner's decision; the site review on 2026-10-31 named the camera
only.

**Status: accepted** by the repo owner on 2026-11-01 (NB-071), who kept the button on
the site, so neither fix was taken. The mitigation is the button's note (`1d434cb`): on
a `*.github.io` host it adds, before the grant and after it, that the permission is for
all of that host (`avinashid.github.io` here), so the other GitHub Pages sites on it
could use the microphone, and see this device's local addresses, without asking. On
other hosts (local, Docker) the note is unchanged. Reviewed 2026-11-01 by reading the
diff. NB71-I1 (Info), that the note named the microphone only, though those sites'
WebRTC connections would carry the local addresses too with no indicator, is fixed by
the words on the addresses (`8f4a635`); `test/netlab_mic.mjs` checks the note on a
faked `avinashid.github.io` origin (NB71-I2). Residual: like the shared origin, it
holds only while none of the account's other Pages sites runs third-party scripts or
user content.

**Fix not taken for now: an origin of the site's own.** A custom subdomain on a
domain the owner holds is the one choice no other site shares: an organisation's
`<org>.github.io` is still shared by that organisation's other Pages sites. It keeps the
demos' storage, scripting, service workers and media grants apart from everything else
the account publishes, and the site needs no change, since its URLs are all relative.
With a custom domain, verify it in the account's Pages settings and enforce HTTPS, and
remove its DNS record if the site is ever taken down, so no other Pages site can claim
it. Accepting the shared origin, as the owner has, holds only while nothing else on the
account runs third-party scripts or user content, and it doesn't cover the camera
(NB10a-I1). The owner accepted Net Lab's microphone grant on its own, with a note on the
button (NB16-2).

**Framing (Info; a condition for the meet page).** A `<meta>` policy can't set
`frame-ancestors`, so any site can frame the demos. For today's pages that is accepted:
- browsers partition a framed page's storage by the top site, so a framer sees none of
  the visitor's nickname or rooms;
- a microphone prompt in a frame needs the framer's `allow=`, and the grant goes to the
  framer;
- Net Lab's one-click actions reveal nothing a page couldn't learn by running the same
  open code.

A camera page is different: a framer that delegates `allow="camera; microphone"` can
show it under a decoy and have the visitor's own grant put them in a call they can't
see. **Before the meet page joins the site's demos,** it must check
`window.top !== window` and, framed, show a message, ask for no media and join no room.

**Accepted:**
- **Old pages after a deploy.** A page cached from before a deploy (Pages caches for
  about 10 min) asks for its old `assets/<id>/`, which the deploy removed. It fails
  closed until reloaded, and never runs a mix of old and new JS and wasm.
- **Public Nostr and STUN by default.** Chat and Net Lab use them, as designed, and
  their cards on the landing page say so. Net Lab takes `?relays=` from its link, so a
  link to our site can send a visitor to a relay of the link-maker's choosing, which
  learns their IP and room tag. Anyone in the room learns the IP over ICE anyway.

**Info:**
- **NB16-I1.** `buildSite` clears a non-empty `--out` that holds an `assets` entry, and
  many web project directories have one. No repo directory does, and `site.sh` always
  passes `target/site`. A stamp file the build writes would be tighter (written, not
  merged).
- **NB16-I2.** U+E000 and U+E001 in a doc's prose are read as the renderer's slot
  markers, so they repeat an earlier code span or link, or print `undefined`. Not
  exploitable: slots are whole, escaped elements in text positions (fix written, not
  merged).
- **NB16-I3.** For the first deploy, the `github-pages` environment should take `main`
  only, with a required reviewer, so a dispatch from another branch can't deploy. Done
  in the repository's settings when Pages was enabled on 2026-11-01: `main` only, the
  owner as required reviewer.
- **NB67-I1.** The "Edit this page on GitHub" link puts the doc's path in its `href`
  as it is, neither escaped nor URL-encoded. A path holding `"` could leave the
  attribute, which `PAGE_CSP` keeps inert, and only a file the repo holds can be a doc.
  `encodeURI` would do. Fixed by `1f2a492` (NB-073), reviewed 2026-11-01: the path goes
  through `encodeURI`, then is escaped, and the sidebar, prev/next and the overview's
  cards escape their links to a page as well; `site_build.mjs` builds a doc named
  `q" &amp;.md`. `encodeURI` leaves `#` and `?`, but a doc named with either fails the
  build (above).
- **NB67-I2.** The build follows symlinks under `docs/assets/`, so a committed symlink
  named like an image would publish whatever it points at on the build machine. A
  reviewer sees it in the diff as a one-line file. Refusing symlinks there (`lstat`)
  would be tighter. Fixed by `9d3396d` (NB-073), reviewed 2026-11-01: `walk` reads
  `docs/assets/` with `lstat` and throws on a symlink, to a file or a directory, and
  the build throws unless `docs/assets` resolves to itself under the repo, so neither
  it nor `docs` can be one. A probe of a link inside a real subdirectory, and of `docs`
  as a link, was refused. Git stores no hard links, devices or FIFOs, so a commit can
  put nothing else there that names a file outside the repo. Residual: NB73-I1, fixed
  by NB-082, whose `inRepo` replaced both checks.
- **NB67-I3.** `<a><set attributeName="href" to="java…script:…"/>`, with a tab inside
  `javascript:`, passed the check until the allow-list: the URL parser drops the tab,
  but the regex saw no `javascript:`. It didn't run on a click in headless Chromium.
  The allow-list refuses `a` and `set` (`a7da8c2`), and `site_build.mjs` holds the
  case.
- **NB67-I4.** Until `4386eec`, an SVG opened on its own could still fetch from another
  host, with no script: CSS takes a string URL in `image-set('//host/…' 1x)`, with no
  `url(` or scheme, and a CSS escape spells `url(` as `u\72l(` and `//` as `\2f \2f`,
  in a `style` or a `fill`. Each fetched from a second loopback port in headless
  Chromium; that host learns the visitor's IP address. Fixed by `4386eec`: no
  backslash, and no `//` but in the `xmlns`.
- **NB73-I1.** Outside `docs/assets/` the build still follows symlinks: to each `.md`
  that `docs/README.md` lists (anywhere in the repo), to the demos' pages and every
  module they load, and to `site.css`. A commit that adds such a link, with a sidebar
  line or a demo `<script>`, would publish what it names on the build machine. Repro,
  on a copy of `9d3396d`: a listed `docs/leak.md -> /etc/hostname` put the host's name
  on `docs/leak.html`, one to `/proc/self/environ` put the build's environment there,
  and `packages/js/demo/leak.js -> /etc/hostname`, loaded by a demo, was copied to
  `assets/<id>/demo/`. Info, as NB67-I2: it takes a merged commit, where the link shows
  as a one-line file; the Pages build job holds only `contents: read`, no secret and no
  persisted credentials, and the site goes live after that job, and its tokens, have
  ended. The repo tracks no symlinks, so refusing them changes nothing it builds: the
  same `realpath` check as `docs/assets` for every file the build reads and publishes.
  Fixed by `b3a569c` (NB-082), reviewed 2026-11-03: `inRepo` `lstat`s each part of the
  path from the repo's root and throws on a symlink, to a file or on the way to one,
  for `docs/README.md` and each page it lists (in `docs/` or out), all of `docs/`, the
  demo pages, each module before it's read (so its imports aren't followed), the
  repo's wasm and `site.css`, now read from the repo's `scripts/site/`. A probe on
  `git archive` trees put a canary file from outside the tree on the site built from
  main `5e6dba4` through a link at each of a listed doc, `CHANGELOG.md`, `docs/guides`,
  a demo module, `packages/js/src` and `site.css`; `06623cd` refused all six.
  `site_build.mjs` holds these cases and more (a dangling link, `docs`, `packages`).
  A wasm named by `--wasm` is taken as given, as whoever builds chooses it; Pages
  (`scripts/site.sh`) builds without it. The site built from main is byte-identical.
- **NB73-I2.** A page's links are escaped but not URL-encoded, so a doc named
  `a%41.md` builds with links to `a%41.html`, which a browser reads as `aA.html`; the
  link check reads the raw path and passes it. Only the site's own links break.
  Accepting only `[\w-]` doc names, as for images, would settle it and `#` and `?` by
  rule rather than by the link check. Fixed by `06623cd` (NB-082), reviewed
  2026-11-03: `readIndex` fails the build unless a listed doc's path, decoded and from
  the repo's root, is `[\w-]` names and slashes ending in `.md`; `site_build.mjs`
  refuses `a%2541.md` (which decodes to `a%41.md`), `#`, `?`, a quote, a space, a
  non-ASCII letter and a dot, in `docs/` or out. The probe's doc listed as `a%2541.md`
  built on main with links to `a%41.html` and failed the build on `06623cd`. Links to
  repo files that go to GitHub already pass through `encodeURI`.

### LAN signalling, `LanSignaling` (NB-064) — NB64-1…NB64-6

`LanSignaling` (`crates/peerwire/src/lan.rs`) is a `Signaling` carrier over UDP
multicast, and since NB-092 broadcast
([below](#nb-092--the-broadcast-leg--nb92-1nb92-3)), so rooms work on one LAN with no
internet. Each message is one datagram to
the group 239.255.80.87:20567 (organisation-local, TTL 1, multicast loop on,
`SO_REUSEADDR`): `PWL1`, an 8-byte sender tag, the 32-byte topic and the payload, at
most 1400 bytes, never fragmented. `poll` reads at most 64 datagrams a call, keeps only
the topics subscribed to and drops its own frames by their tag. Rooms run over it
unchanged: hellos sealed under the room key and proved by HMAC, offers sealed to the
recipient (`Noise_X`) and proved under the room or invite key, and the room's budgets.
The ESP32 port uses it too
([below](#the-esp32-prototype-nb-062-nb-064--nb62-1-nb62-2)).

Reviewed 2026-11-01 by reading `4bbde33`, `b80cac1` and `d5978f9`, then the fold-in
`a0fe2b1` and `4c86af7`. Measured with loopback probes in release builds on a 4-core
aarch64 host, and with two emulated ESP32 chips on a hub on 127.0.0.1; the probes
aren't in the tree.

**The LAN is as untrusted as a relay, with two differences.** Every device on the link,
and every program on the machine (`SO_REUSEADDR` lets it bind the same port), gets a
copy of each frame and can send its own. That is inherent: anyone on the link can join
the group. It can't open or forge hellos or offers, and the room's budgets hold. But no
relay paces what it sends (NB64-2, NB64-6), and as first written the socket also took
unicast from off the link (NB64-1).

**NB64-1 — The carrier took unicast from any host that reached its port (Low).** The
socket was bound to `0.0.0.0:20567`, and `poll` never asks where a datagram was sent.
TTL 1 limits what we send, not what we take, so any host that reached UDP 20567 by
unicast (a public address with no firewall, a VPN or overlay peer, a container network)
could inject frames from off the link: forged room or inbox traffic, and NB64-2's
flood. Repro: a plain UDP socket sent a frame to `127.0.0.1:<port>`, and the carrier's
`poll` returned it. Fix: bind the group address, so the socket takes only datagrams
sent to the group; lwIP matches a socket bound to the group the same way (from reading
its `udp.c`). Test: `lan::tests::unicast_to_the_port_is_not_delivered`.

**Status: fixed** by `a0fe2b1` before NB-064 merged (`34a35a8`), so never on main,
reviewed 2026-11-01. Measured: a unicast frame to the port is no longer delivered, and
multicast still is, both ways.

Residual (accepted): **Windows** can't bind a multicast address, so there the carrier
binds `0.0.0.0` and still takes unicast to the port; NB64-2's budget bounds what that
costs. A Windows firewall rule that lets UDP 20567 in should be scoped to the local
subnet. Not measured: there is no Windows host here.

**NB64-2 — Each forged inbox message cost a `Noise_X` open before any budget (Low).**
Offers go to the recipient's inbox topic (`Topic::peer`), which is in clear on the
wire, so a neighbour that hears one offer learns it. `handle_signal` ran `sealed::open`
(210 µs here) on every payload on that topic, and only the proof after it spent a
budget; `Node::poll` passes up to 256 messages a call. Measured, one `Node::poll` after
256 junk frames: 46 ms on the inbox topic, against 7.8 µs idle, 83 µs on another topic
and 0.72 ms on the room topic. Theoretical: CPU share = R × t_open for R junk frames a
second, a whole core here at R ≈ 4,800 (about 0.7 MB/s). Nostr has the same path, but
the relays pace what they forward. Fix: a node-wide bucket, a burst of `INBOX_BURST`
(64) and then one per `INBOX_REFILL` (50 ms), taken before `sealed::open`; a
compile-time check keeps that burst above the offer budget's (30). Test:
`traversal::tests::junk_on_the_inbox_spends_the_open_budget`.

**Status: fixed** by `a0fe2b1` before NB-064 merged, so never on main, reviewed
2026-11-01. The bucket is node-wide, so it covers Nostr too. Measured, one `Node::poll`
after junk on the inbox topic: 0.39 ms for 256 frames (median), 0.44 ms for 1,000. The
worst poll is the burst's, 14 ms for 64 opens. Theoretical: at most 20 opens a second
after the burst, 20 × 210 µs = 4.2 ms of CPU a second here. On an ESP32-C3 an open costs
far more (not measured). The costliest junk is now on the room topic, one AES-GCM check
a frame: 0.76 ms per 256 (measured).

**NB64-3, NB64-4, NB64-5 (Info).** Fixed with the fold-in:
- NB64-3: `lan_chat` printed a member's bytes raw, so anyone who guessed the room could
  fake another member's line or send terminal escapes. It now prints `escape_debug()`
  text and says to pick a room name others can't guess (`4c86af7`).
- NB64-4: the ESP32 README offered the host seed as the answer to QEMU's RNG. It fixes
  the identity only (NB62-2), as the README and `main.rs` now say (`4c86af7`).
- NB64-5: the module docs said a neighbour can't replay hellos. They now say what a
  replay does (below) and how the socket is bound (`a0fe2b1`).

**NB64-6 — A targeted inbox flood can keep two members from linking (Low).** NB64-2's
bucket hands out its tokens first come, first served, so junk above 20 frames a second
spends them, and a real offer is opened only when a token is left as it arrives.
Theoretical: P ≈ 20 / R that a given offer is opened, for R junk frames a second,
R well above 20. Starving one member only delays the link: the other member opens its
offer and dials, and the dial is let in, because the hello already made the dialler a
member. Starving both blocks it. Measured, two nodes joining one room on loopback:

| Junk on… | Linked after |
|---|---|
| nothing | 11 ms |
| one member's inbox topic, ~1,800 frames/s | 2.0 s |
| both members' inbox topics, ~1,800 frames/s each | not within 45 s |

The attacker must be on the link (or reach the port on Windows), and must know both
PeerIds beforehand: stable identities give them away (SEC-5, and the privacy list
below). It is still a better default than a core burnt at 4,800 frames a second, or an
ESP32 at far fewer. Fix: an outer 16-byte tag, an HMAC over the topic and the sealed
bytes under a key derived from the room or invite key, checked before the open and
under a budget of its own. Junk from anyone without the key then costs one HMAC and no
token; a member, or anyone who has the room name, can still spend the budget. It
changes the wire format, so its release notes need a compatibility line. Partial fixes:
a cached keypair in `sealed::open` (one scalar multiplication of three, from reading
`snow`, not measured), and per-source fairness in `LanSignaling` (a host on the link can
spoof its source). **Status: open** (NB-065).

**What a LAN neighbour learns.** Without the room key it can't read hellos or offers or
forge them. It learns:
- **That the host runs Peerwire at all:** its IGMP join for 239.255.80.87 and its
  traffic on UDP 20567, before it joins any room.
- **Room topics, and when each member says hello.** A topic is an unsalted SHA-256 of
  the app and room names, as on Nostr, so a guessable name can be tested, then joined:
  `lan_chat`'s default `lobby` and the docs' `kitchen` are such names. Treat room names
  as secrets, as CHAT-1 says for the chat demo.
- **Each member's inbox topic**, the hash of its PeerId that SEC-5 describes. Timing
  ties it to the member's IP (the offers that answer a hello go there), so a neighbour
  can recognise a device on another day or another network, and one that knows a PeerId
  can tell which device holds it. On Nostr that takes the relays; here every device on
  the link sees it.
- **The sender's IP and MAC addresses, frame sizes, and the 8-byte sender tag**, which
  links one carrier's frames across rooms and address changes for as long as it lives.

**Replays.** A hello replayed within 60 s (`FRESH_MS`) is taken again: each member not
yet linked to its sender offers to it again, at most once per 2 s per peer and within
the hello budget (30, then one per 2 s). A replayed offer is dropped by its nonce, or as
stale after 60 s. This is the LAN form of SEC-6.

#### NB-092 — the broadcast leg — NB92-1…NB92-3

NB-091 found a home router that never delivered multicast to an ESP32-C3 on Wi-Fi
(lwIP joins with IGMPv2) but passed broadcast both ways, so the board and a PC never
linked. Since NB-092 each frame also goes to a subnet-directed broadcast address: for
`new()`, that of the interface the system sends the group through; for `on()`, the
named interface's; either only if private (RFC 1918 or link-local). A caller may add
others with `broadcast()` (the ESP32 firmware names its subnet's), at most 4, but not
one of the device's own addresses. The frames and their sealing are unchanged. What
changes:

- **NB64-1 still holds** for unicast. Each broadcast leg has a socket bound to its
  broadcast address, which takes only datagrams sent to that address, not unicast to the
  port (NB92-1 was where it didn't). On lwIP by reading `udp_input_local_match`: a
  broadcast matches a socket bound to an address in the destination's subnet, and
  unicast needs an exact match. With `SO_BROADCAST` set, lwIP also gives the group's
  socket limited broadcasts (255.255.255.255) to the port, which stay on the link and
  are kept once. Windows binds neither: its one socket on `0.0.0.0` already heard
  broadcasts to the port. A broadcast address is not tied to its interface, though:
  Linux takes one that comes in on any interface, so there each leg's socket is also
  bound to the interface whose subnet holds it (NB92-3).
- **TTL 1 for both legs.** Frames go out from the group's socket, with `IP_TTL` 1 as
  well as `IP_MULTICAST_TTL` 1, so a router that forwards directed broadcasts (off by
  default, RFC 2644) doesn't pass ours on. That limits what we send, not what we take
  (NB92-I2).
- **Every device on the subnet receives each frame**, not only those that joined the
  group. What a neighbour learns is unchanged (above), but it no longer needs the
  IGMP join to see the frames.
- **A frame identical to one heard in the last second is dropped**, so the copy that
  arrives on the other leg is kept once: 16 hashes are kept, under a per-carrier
  SipHash key. A neighbour can't use this to suppress a member's frame: it would need
  the frame's exact bytes before the member sent them, and rooms seal each frame with
  a fresh nonce. Junk that pushes hashes out of the ring only lets a second copy
  through, which the room's replay handling already takes.
- **Reading stays bounded.** One `poll` reads at most 64 datagrams over all sockets,
  one from each in turn, so a flood on one leg doesn't starve the other.

Tests: `lan::tests::the_broadcast_leg_works_alone_and_copies_are_kept_once`,
`a_frame_heard_again_within_a_second_is_dropped`,
`broadcast_addresses_are_checked_and_capped`, `own_addresses_are_not_broadcast_legs`,
`any_interface_means_the_group_routes_interface_only`,
`a_broadcast_leg_hears_only_its_own_interface`, `unicast_to_the_port_is_not_delivered`
(now with a broadcast leg too) and `tests/lan.rs`'s `a_room_links_by_broadcast_alone`,
on loopback's 127.255.255.255 (Linux). In QEMU, two chips linked with `NO_MULTICAST=1`
(the hub dropped every multicast frame).

Reviewed 2026-11-03 by reading `3d78c44`, the firmware's use of it in `c6b12ad`, the
fixes `5ef2482` and `63f639d`, and lwIP as ESP-IDF v5.5.5 ships it. NB92-1 and NB92-2
were measured with loopback probes, NB92-3 with two containers on a Docker bridge,
before and after its fix; the probes aren't in the tree.

**NB92-1 — `broadcast()` took one of the device's own addresses, which undid NB64-1
(Low).** `add_broadcast` refused only multicast addresses and `0.0.0.0`. Named with a
unicast address of the device, the leg's socket bound that address and port 20567, and
took unicast from any host that reached the port, as before NB64-1. The ESP32 firmware
did that by itself: its `ip | u32::MAX.checked_shr(prefix).unwrap_or(0)` gives the
chip's own address on a /32 lease, or on a /31 with an odd address. Repro:
`LanSignaling::on(group, 127.0.0.1)?.broadcast(127.0.0.1)` was accepted, and a plain
socket's unicast frame to `127.0.0.1:<port>` came out of `poll`. Fix: refuse the
carrier's interface and, where interfaces can be listed, any of the device's addresses;
the firmware adds no leg on a /31 or /32. Test, the repro:
`lan::tests::own_addresses_are_not_broadcast_legs`.

**Status: fixed** by `5ef2482` before NB-092 merged, so never on main, reviewed
2026-11-03. Residual (accepted): ESP-IDF can't list interfaces, so there only the
carrier's own address is refused; the firmware names no other.

**NB92-2 — `new()` opened legs on container bridges and VPNs (Low).** With `0.0.0.0`,
every private interface with a broadcast address got a leg: Docker and VM bridges, and
L2 VPNs such as ZeroTier or OpenVPN's tap, whose members can be anywhere. They heard
each frame (topics, inbox hashes, addresses) and could send frames the node took, NB64-2
and NB64-6 junk included, where the group is joined on one interface only. Measured on a
host with three Docker bridges: they and the LAN filled the cap of 4, so a LAN listed
after four such networks would have had no leg. Fix: with `0.0.0.0`, the one leg is the
broadcast address of the interface the system sends the group through (a UDP socket
connected to the group reports it and sends nothing), and only if private. Test:
`lan::tests::any_interface_means_the_group_routes_interface_only`.

**Status: fixed** by `5ef2482` before NB-092 merged, so never on main, reviewed
2026-11-03, for what the node sends: its frames no longer go to other networks. What
those networks send to the LAN's broadcast address was NB92-3, fixed by `63f639d`.
Residual (accepted): a VPN that carries the default route is the interface the group
goes through, and the group was joined there before NB-092 too.

**NB92-3 — On Linux, a broadcast leg takes frames that come in on any of the device's
interfaces (Low).** Linux delivers a datagram sent to one of its broadcast addresses
whichever interface it comes in on, and its reverse-path filter checks only the source.
So a host on another network of the device (a container or VM on one of its bridges, a
VPN server or peer, a neighbour on a second link) can route a frame to the LAN's
broadcast address through the device, and the leg's socket takes it: NB92-2's injection,
through another address. Multicast doesn't let this in: the group is joined on one
interface, and group traffic that comes in on another is dropped. Repro, with two
containers on a Docker bridge: in the first, a veth at 10.99.0.1/24 stood in for the
LAN, and `5ef2482`'s `LanSignaling::on(group, 10.99.0.1)` ran with `rp_filter` 1
(strict). The second routed 10.99.0.0/24 through the first's bridge address and sent
three frames: unicast to that address and multicast to the group were dropped, and the
one to 10.99.0.255 came out of `poll`. lwIP does the same with more than one interface
(from reading `ip4_input`); the ESP32 firmware has one. BSDs and macOS take a broadcast
only on the interface whose broadcast address it is (from reading their `ip_input`, not
tested), and Windows binds no leg. Fix: on Linux and Android, each leg's socket is bound
to the interface whose subnet holds its address, the narrowest if several do, with
`SO_BINDTODEVICE` (socket2's `bind_device`, unprivileged since Linux 5.7), and an
address in none of the device's subnets is refused. Test:
`lan::tests::a_broadcast_leg_hears_only_its_own_interface` (a leg on loopback reads back
`lo`, the group's socket no device); it fails with the bind undone. The repro, re-run
against `63f639d` with `5ef2482` beside it: the second container's frame to 10.99.0.255
was dropped, and the device's own broadcast was still heard.

**Status: fixed** by `63f639d` before NB-092 merged, so never on main, reviewed
2026-11-03. Residuals (accepted): where the bind fails there is no leg, only the
multicast one, so nothing more is let in: on Linux before 5.7 without `CAP_NET_RAW`, and
for a name the kernel doesn't know as a device, such as an alias label like `eth0:1`
(from reading `getifaddrs`, which lists an address under its label). If two interfaces
hold one subnet with the same mask (overlapping networks), the leg hears only the first
one listed. lwIP with more than one interface would need the same; the ESP32 firmware
has one.

**NB92-I1 — A frame published twice within a second is heard once (Info).** The dedupe
can't tell a caller's own repeat from the other leg's copy. Rooms never repeat a frame:
each is sealed with a fresh nonce or seed. Fix: say so. **Status: fixed** by `5ef2482`
(`LanSignaling`'s docs and the Rust reference).

**NB92-I2 — TTL 1 limits only what we send (Info, accepted).** As first written, this
section said TTL 1 stops a router that forwards directed broadcasts; that holds for our
frames only. From beyond the device's own networks, a leg is reachable only through a
router that forwards directed broadcasts into the LAN, which is off by default (RFC
2644); from the device's other networks, see NB92-3. The TTL bullet above now says so.

### The ESP32 prototype (NB-062, NB-064) — NB62-1, NB62-2

`ports/esp32` runs the unchanged `peerwire::Node` on an ESP32-C3 under ESP-IDF, tested
in Espressif's QEMU and, since NB-091, on one ESP32-C3 board on a home router's Wi-Fi.
It is a feasibility prototype, not a supported target: it has its own workspace and
toolchain, and the gate, CI and the site never build it. The emulated chips share one
virtual LAN through `l2hub.mjs` on 127.0.0.1, so nothing leaves the host. NB-062 ran
with fixed test identities and invites built from them. Since NB-064 each chip binds its
own address, joins a room over `LanSignaling` and links with whoever answers, with an
identity from a 32-byte seed that `run.sh` draws from the host's `/dev/urandom` for each
node and each run. That seed is one of two QEMU-only settings; the other puts the
emulated Ethernet MAC in promiscuous mode, because it drops multicast otherwise (a real
C3 uses Wi-Fi). Since NB-095 a board on Wi-Fi whose flash names a room joins it over a
Nostr relay ([below](#several-carriers-and-the-esp32-on-nostr-nb-095--nb95-1nb95-6)).

Reviewed 2026-11-01 by reading `a5b4d50` and `e6162f8` (NB-062), then `d5978f9` and
`4c86af7` (NB-064), against ESP-IDF v5.5.5's lwIP and random-number sources. Measured
with two chips of the NB-064 image on a hub of our own on 127.0.0.1.

On lwIP, NB64-1's fix holds by reading: a socket bound to the group takes only
datagrams sent to the group and sends from the interface's address. Eight chips of the
rebuilt image linked 28 of 28 in QEMU before the merge. NB64-2's open costs a C3 far
more than 210 µs (not measured), so its budget takes a larger share of a chip than of a
PC.

**NB62-1 — A node bound to any interface offers loopback only (Info).** `a5b4d50` said
such a socket on ESP-IDF offers its default route's address and loopback. It offers
`127.0.0.1` alone: ESP-IDF has no `getifaddrs`, and lwIP's UDP `connect` picks no source
address, so `primary_ip` finds none. A board bound to `0.0.0.0` would give its peers no
LAN address to dial. Fix: the comment says so, and the port binds the chip's own
address, as it has since NB-062.

**Status: fixed** by `4bbde33`, merged `34a35a8`, reviewed 2026-11-01.

**NB62-2 — ESP-IDF's random numbers are only pseudo-random before Wi-Fi is up (Info in
the emulator, High on a board).** `OsRng` reaches `esp_fill_random` through `getrandom`,
which gives true random numbers only while the radio (Wi-Fi or Bluetooth) is on or
`bootloader_random_enable()` is in effect. `Node::bind` draws from it up to four times,
before an app has any reason to start Wi-Fi, and never reseeds: the identity (when the
app passes none), the endpoint seed (Noise ephemerals, cookie secrets), the traversal
seed (the invite key, and the nonces of room hellos and offers) and the port-mapping
seed.

Measured in QEMU, whose random numbers are the same sequence on every chip:
- With identities generated on the chip, four chips came out with one PeerId.
- With the host seed, which fixes the identity only, two chips sealed all four of their
  room hellos with the same AES-GCM nonces under the room key. Two such hellos XOR to
  the XOR of their plaintexts, so one chip's PeerId gives the other's (checked on the
  12 hex digits each chip logs): between such chips the hello seal (SEC-2) hides
  nothing.

By reading, their Noise ephemerals, cookie secrets and invite keys repeat too. A link
stays confidential while the identity keys are secret, because IK mixes in the
static-static DH, but repeated ephemerals give it no forward secrecy; and one chip's
invite key is every chip's, so its invite proves offers to any of them.

Fix (NB-063, before any board): every draw in `bind` from real entropy, with Wi-Fi up
first or `bootloader_random_enable()` around the draws, and the identity kept in
encrypted NVS. Its test compares two chips' first hello nonces. The emulator shows
discovery and memory, not privacy, as the README has said since `4c86af7` (NB64-4).

**Status: fixed** on a board by `c5e13b7` (NB-091), merged `4697a77`, reviewed
2026-11-03 by reading the firmware at `4697a77` against ESP-IDF v5.5.5's random-number
docs, which promise true random numbers while the radio is on and awake. With a Wi-Fi
record in flash, `main` starts Wi-Fi, turns modem sleep off, joins, waits for its DHCP
address and sets the clock by SNTP, and only then builds the Node; a join that fails is
retried and never reaches `bind`. Every draw from the chip's random numbers comes after
that, in `LanSignaling::on` and `bind`:
- the carrier's sender tag;
- the main task's hash keys, which std draws for each thread when it makes its first
  hash map;
- the identity;
- the endpoint seed;
- the traversal seed, which gives the invite key and the nonces of room hellos and
  offers.

The port-mapping seed is never drawn, since the firmware leaves port mapping off.
Nothing draws before `main`: no crate in the firmware runs code before it, std draws
only for hash keys, and the ESP-IDF crates draw nothing. On the board, seven boots gave
seven different PeerIds.

Since NB-095 a board on Wi-Fi whose flash names a room also builds a `NostrSignaling`
carrier, after SNTP and before `bind` (reviewed 2026-11-04 by reading `6737208`, and
`d5c2cfb`, which builds it only for a named room). Its draws come after Wi-Fi is up too:
- the carrier's signing key and subscription id, and each signature's auxiliary random
  bytes, from `OsRng`;
- on the relay thread: its hash keys; TLS, through ring, which draws from
  `esp_fill_random` only with its `less-safe-getrandom-espidf` feature (without it ring
  has no random source on ESP-IDF); and the websocket's key and masks, from rand's
  thread generator, seeded from `OsRng`.

None of the crates NB-095 adds runs code before `main`, and the QEMU path builds no
relay carrier, so ring never draws there.

Residuals:
- In QEMU every draw but the host-seeded identity still repeats on every chip (Info,
  accepted): the emulator is for discovery and memory.
- The order is the firmware's, not `peerwire`'s. An ESP-IDF app of its own that builds a
  Node before it starts Wi-Fi, with modem sleep on, or on Ethernet draws pseudo-random
  numbers again; the ESP32 guide says so. On Ethernet an app needs
  `bootloader_random_enable()` around `bind`.
- A board makes a new identity each boot: keeping one needs encrypted NVS, its own card
  when a board needs a stable identity.
- The two-chip nonce test needs a second board. By reading, the nonces come from the
  traversal seed, drawn after Wi-Fi.
- The relay's TLS checks certificates against the clock, which SNTP sets without
  authentication: anyone on the path at an ask, at boot or at a re-ask every 6 hours,
  can set the board's clock (NB95-I2; since `4c71c67` a device on the LAN off the path
  can't). TLS then fails closed or, with the clock set back, accepts a certificate
  expired since (Info, accepted for the prototype).

### Several carriers, and the ESP32 on Nostr (NB-095) — NB95-1…NB95-6

`MultiSignaling` (`crates/peerwire/src/signaling.rs`) is one carrier over several: it
subscribes and publishes on each and polls them in turn, so a node on `LanSignaling` and
`NostrSignaling` has one room with the peers of both. With it the ESP32 firmware on
Wi-Fi joins the room its flash names over the LAN and the relay `wss://nostr.mom`, with
STUN on (the default servers, Google's and Cloudflare's); with no room named it stays on
the LAN, in room `esp32`, with STUN off, as before NB-095 (`d5c2cfb`).
`examples/rust/room_chat.rs` does the same on a PC. `NostrSignaling` now builds for
ESP-IDF. There its relay stream owns its one socket, as ESP-IDF can't duplicate one; it
takes relay messages of up to 4 KiB and keeps 32 received events or 16 KiB of them, 128
seen ids and 8 queued (`Limits::SMALL`, `0b909e3`); and its relay thread asks for a
32 KB stack.

Reviewed 2026-11-04 by reading `8b089c2`, `027e4cf`, `fa71e65`, `04c20cb` and `6737208`
against ESP-IDF v5.5.5's pthread and FreeRTOS sources, ring 0.17.14 and tungstenite
0.24. Measured with a relay of our own on 127.0.0.1 and a counting allocator, in a
release build on a 4-core aarch64 host; the probe isn't in the tree. Nothing was run on
a board.

The fixes, `0b909e3` and `51d3ba9` (the library) and `d5c2cfb` and `a777178` (the
firmware), were re-reviewed 2026-11-04 by reading, and measured on the same host with
the relay of our own and the counting allocator of
`crates/peerwire/tests/nostr_heap.rs`, and a probe that paints a thread's stack, which
isn't in the tree.

**What holds.**
- No message crosses from one carrier to another: `MultiSignaling` publishes only what
  its node publishes. A message heard on two carriers is acted on once. A hello's second
  copy finds an offer to its sender made under 2 s before (`OFFER_GAP`), and an offer's
  second copy has a nonce already seen. The budgets are the node's, so two carriers
  don't double them, and one `Node::poll` takes at most 256 signals, the carriers in
  turn.
- On ESP-IDF the relay's stream owns its socket and the read timeout is set through it
  (`027e4cf`); a connection that fails drops its one socket. The 32-bit counters, where
  there are no 64-bit atomics, are statistics only.
- The 4 KB stack that the TLS handshake overflowed (`fa71e65`) hit the C3's stack guard
  and rebooted the board, rather than writing past it.
- TLS checks the relay's certificate against the clock: a board whose SNTP got no answer
  can't reach the relay, and no check is skipped. A boot that fails restarts after 5 s
  and draws anew, after Wi-Fi (NB62-2).

**NB95-1 — One relay message of a few KB can take more heap than an ESP32 has (Medium on
a board, Low elsewhere).** `on_relay_message` parses each message from a relay whole
into a `serde_json::Value`, then builds the event's id from a copy of its `tags`, and
`MAX_MESSAGE` is still 128 KiB on ESP-IDF. A `Value` costs several times the JSON it
came from, most of all for many small values. Measured, the carrier's peak heap for one
message (on a 64-bit host; a 32-bit chip's `Value` is smaller, its byte buffers the
same):

| Message | Size | Peak heap |
|---|---|---|
| an event the size of the largest signal a LAN frame holds | 2.2 KB | +13 KB |
| an event with 16 KB of content | 16.8 KB | +111 KB |
| an event of many empty tags | 4.5 KB | +192 KB |
| the same, near `MAX_MESSAGE` | 123 KB | +5.9 MB |

A board with two links had 81–83 KB free, 63 KB at the lowest, and on ESP-IDF an
allocation that fails aborts and reboots. Anyone who can publish to the relay on a topic
the board subscribes to can send such a message, if the relay forwards it: the room
topic, which the firmware's public room name gives away (NB95-4), or the board's inbox
topic, from its PeerId. The board reboots, rejoins, and the next message does it again.
Theoretical on a board: not run on one.

Fix: on ESP-IDF a `MAX_MESSAGE` of 4 KiB (a signal event is at most about 2.3 KB:
1,356 B, the largest `LanSignaling` payload, is 1,808 B of base64, theoretical). Before
parsing, on every target, drop a message with more than 64 `[`, `{` or `,` (a Peerwire
event has 13), or parse into a type that takes one `p` tag only. On ESP-IDF bound the
inbox in bytes: 32 events under a 4 KiB cap could hold about 120 KB (32 × 3.8 KB,
theoretical). A connection that a frame over the cap closes should keep its back-off, so
a sender can't make the board redo the TLS handshake every second. Measured with the cap
and the pre-parse limit: the tag message is dropped (+6 KB), a 3.9 KB event takes +17 KB
and a 16 KB frame is refused. Test: a carrier test against a relay on loopback that
sends each shape, with the heap counted.

**Status: fixed** by `0b909e3`, reviewed 2026-11-04. On ESP-IDF a relay message is at
most 4 KiB and the inbox holds 32 events or 16 KiB, counted by the relay threads as they
queue and released as the carrier polls. On every target a message with more than 64
`[`, `{` and `,` is dropped before it is parsed, and a connection that drops within 30 s
keeps its back-off. Measured with `tests/nostr_heap.rs`: the tag message is dropped
(+8.5 KB) and a signal-size event takes +13.2 KB; with ESP-IDF's limits a 3.9 KB event
is delivered and a 16 KiB frame refused, with 2 or 3 handshakes in 3.5 s. Residuals: a
message under the cap can still nest deep (NB95-1a); a frame over the cap closes the
connection, so anyone who publishes on the board's topics can keep it off the relay (the
residuals below).

**NB95-1a — A relay message under the cap can still nest deep (Low).** The cap counts
`[`, `{` and `,`, not how deep they nest or how many are objects. A `serde_json` object
is a B-tree whose node takes about 630 B on a 64-bit host even for one key, and the id
check copies `tags`; parsing, copying, writing out and dropping a value each recurse
once per level. Measured, one message at the cap (64 `[`, `{` and `,`) with a valid id,
delivered, with the stack measured by painting a thread's (a plain event took 2.8 KB):

| Message | Size | Peak heap | Stack |
|---|---|---|---|
| 50 nested arrays in `tags` | 538 B | +9.8 KB | 16.8 KB |
| 25 objects side by side in `tags` | 638 B | +35.7 KB | not measured |
| 50 nested objects in `tags` | 738 B | +65.6 KB | 33.0 KB |

On ESP-IDF the relay thread has 32 KB of stack in all. A 32-bit chip's pointers and
`String`s are half this host's and its values smaller, so theoretical on a C3 the 50
nested objects take half to three quarters of 65.6 KB, 33–49 KB, against the 63–83 KB a
board had free; how deep its stack goes isn't known without a board, and an overflow
reboots it. A relay can send any message. Anyone else needs a relay that forwards an
event with objects in its tags or with fields NIP-01 doesn't define; NIP-01 makes tags
arrays of strings, and which relays enforce that wasn't checked.

Fix: also drop a message with more than 2 `{` or 8 `[` (a relay message has at most 1
and 3). Measured with it: the shapes above are dropped; the worst still taken, one
object of 48 keys or arrays 5 deep, peaks at +13.5 KB and +5.9 KB of heap with 3.6 KB
and 4.0 KB of stack, about what a signal-size event takes; the `nostr` tests pass, and
the unit test's case of 20 extra tags needs 5 or fewer. Test: the shapes above in
`tests/nostr_heap.rs`, dropped.

**Status: fixed** by `51d3ba9`, reviewed 2026-11-04. Before parsing, `on_relay_message`
counts `[`, `{` and `,` apart and, on every target, drops a message with more than 64 in
all, more than 2 `{` (`MAX_OBJECTS`) or more than 8 `[` (`MAX_ARRAYS`).
`tests/nostr_heap.rs` now sends the three shapes above; re-run on `a777178`, each was
dropped, at +452 B, +248 B and +52 B, and a signal-size event was delivered at +13.2 KB.
The unit test passes an event with 5 extra tags and drops one with 6. Residual: the
worst message still parsed is the one measured above, an object of 48 keys or arrays 5
deep, about what a signal-size event takes.

**NB95-2 — A relay that stops reading holds every later publish (Low).** The relay
socket has a read timeout but no write timeout, and while a connection is up nothing
bounds the queue from the carrier to its relay thread. Once a relay stops reading (a TCP
window of zero), the thread blocks in its write for good, the relay still counts as
open, and every publish after that stays in memory. Measured: 20,000 publishes of a
1,356 B payload to a relay that stopped reading after the subscription held 41.8 MB for
12 s, with the relay still open; one that read held 10 KB. Theoretical on a board: free
heap / (publishes a second × event size), about 16 min for 81 KB at the board run's 12
publishes in 140 s of about 1 KB each. The relay can do it, or anyone who can hold the
connection's window shut. Older than NB-095 and on every target, it matters most on a
board. Fix: a write timeout on the socket in `connect` (`CONNECT_TIMEOUT`), which now
covers TLS too, as the stream owns the socket; and `MAX_QUEUED` while a connection is
up, as in the back-off. Measured with the write timeout: the connection drops after 5 s
and the heap falls to +142 KB, the 64 events kept (8 on ESP-IDF).

**Status: fixed** by `0b909e3`, reviewed 2026-11-04: `connect` sets a write timeout of
`CONNECT_TIMEOUT`, and the queue is capped while a connection is up as in the back-off
(`enqueue`). Measured with `tests/nostr_heap.rs`: after 5,000 publishes to a relay that
stopped reading, the connection dropped in about 9 s (a write that takes a few bytes
waits again) and +136 KB stayed, the 64 events kept. Residual: commands still reach the
relay thread over an unbounded channel, so what the node publishes during one 5 s write
waits there; the node's own rate bounds it.

**NB95-3 — The relay thread outranks the Node on a C3 (Low).** An ESP-IDF pthread runs
at priority 5 (`CONFIG_PTHREAD_TASK_PRIO_DEFAULT`) unless its thread config says
otherwise, and the firmware's `ThreadSpawnConfiguration` only names the relay thread;
`main` runs in the main task at priority 1 (`ESP_TASK_MAIN_PRIO`). The C3 has one core,
and FreeRTOS runs the highest-priority task that is ready. While a relay streams events
at the board, the thread decrypts, parses and hashes them in software, and the Node's
loop waits until it blocks: no keepalives, retransmits or polls, so links time out and
the inbox fills before the Node drains it. Not measured: it needs a board and a relay of
our own on the LAN. Fix: the firmware's `ThreadSpawnConfiguration` gives the relay
thread priority 1, which ESP-IDF takes from the config even where std sets the stack
(`pthread.c`), and the ESP32 guide tells apps to do the same.

**Status: fixed** by `d5c2cfb`, reviewed 2026-11-04 by reading: the firmware's
`ThreadSpawnConfiguration` gives the relay thread priority 1 and is reset after
`NostrSignaling::new`; the report prints the thread's priority, for the board run to
confirm, and the ESP32 guide tells apps to do the same. At equal priority FreeRTOS gives
the two tasks turns, so a relay that streams events delays the Node's loop by a time
slice at a time rather than holding it off.

**NB95-4 — The Wi-Fi firmware joins a public room on a public relay (Low).** The room
(`lan_chat`'s app, room `esp32`, named in the README) and the relay are fixed, so every
board flashed with the firmware shares one room with everyone who joins it, from
anywhere; before NB-095 that was the LAN only. Each such peer links directly
(`Privacy::Direct`, STUN on), so it learns the board's home public address and its LAN
address from the offers, takes one of its 8 links, and can send it messages (NB95-5).
Boards of different owners link with each other too, and NB95-1 needs only the room
topic. The README and guide don't say so. Fix: the room name from the flash record,
written by `wifi.sh` (a random one by default, printed for `room_chat`), and the relay
only when the record names a room; the docs say that anyone with the name links directly
and sees the home's public address.

**Status: fixed** by `d5c2cfb`, reviewed 2026-11-04. The room comes from its own record
in the config sector, `b"PWR"`, a length of 1 to 64 and the name at +0xC0, read within
its 80 B and clear of the Wi-Fi record, which ends by +0xA5. `wifi.sh` writes a random
name by default, `esp32-` and 16 hex digits from `/dev/urandom` (64 bits), and prints
it; `PW_ROOM` names one, and empty names none. `room.sh` rewrites it in an image or in
the sector read back from a board. With no room the board stays on the LAN, in room
`esp32`, with STUN off and no relay, as before NB-095. The README and guide say that
anyone with the name can join from anywhere, link directly and see the home's public and
LAN addresses. Residuals: a name the owner picks is as guessable as they make it; the
sector read back holds the Wi-Fi password in the clear until deleted, as the README
says; NB95-6.

**NB95-5 — The firmware logs a member's bytes raw (Info).** It logs the first 24 bytes
of the first 8 messages, and of every 50th, through `from_utf8_lossy`, which keeps
control bytes, so a member can send terminal escapes to whoever watches the serial log.
This is NB64-3, fixed in `lan_chat`, in the firmware since NB-062; NB95-4 opens it to
anyone on the internet. Fix: `escape_debug()`, as `room_chat` prints.

**Status: fixed** by `d5c2cfb`, reviewed 2026-11-04: `escape_debug()` on the logged
text. The firmware's other log lines name peers by the address they linked from, not by
what they send.

**NB95-I1 — A node on several carriers offers on all of them (Info).** An offer to a
member found on the LAN goes to the relays as well, under that member's inbox topic
(SEC-5), from the board's public address, though the member may never use Nostr. One to
a member found through the relays goes onto the LAN. So anyone who knows the LAN
member's PeerId can watch for it on the relays, and a LAN neighbour, who sees room and
inbox topics in clear
([LAN signalling](#lan-signalling-lansignaling-nb-064--nb64-1nb64-6)), can watch the
room's internet side from anywhere. A hello replayed on the LAN within 60 s makes the
node publish an offer to the relays too, within the hello budget. Fix: SEC-5's. Short of
it, the `MultiSignaling` docs should say so; later, an offer could go only to the
carrier its hello came on, if `MultiSignaling` said which carrier delivered a message.

**Status: documented** by `0b909e3`, reviewed 2026-11-04: the `MultiSignaling` docs and
`docs/reference/rust.md` say so. The fix is still SEC-5's.

**NB95-6 — The firmware remembers every peer that ever linked (Low).** For its log, the
firmware keeps `names`, a map from each directly linked peer's PeerId to the address it
linked from. It adds one on every `Connected` and never removes any. The endpoint
answers a handshake from anyone who knows the board's PeerId, up to `max_links`, so a
member who links, leaves and links again under a new key adds an entry each time, at
handshake speed; a room in use adds them slowly too, as each run of a node makes a new
identity. Theoretical on a C3: about 80 B an entry (a 32 B PeerId, a 12 B `String` and a
control byte in a table at most 7/8 full, and about 24 B of address text), and past 448
entries the table moves into one block of about 46 KB (1,024 × 45 B) while the old one
is still held, more than a board with 63–83 KB free, in pieces, is likely to have; an
allocation that fails reboots the board. In the firmware since `d5978f9` (NB-064) on the
LAN; NB-095 opens it to anyone with the room's name. Fix: remove the entry on
`Disconnected`, after logging it, and keep only `node.peers()` at each report.

**Status: fixed** by `a777178`, reviewed 2026-11-04 by reading. On `Disconnected` the
firmware logs the peer, then removes its entry unless `node.peers()` still has it, as
when a new link replaced the old one (`Replaced`); at each report, every 10 s, it keeps
only the entries of linked peers. So after each report the map holds at most the board's
8 links. In the author's QEMU run a name outlived a `Replaced` re-dial.

**NB95-I2 — lwIP's SNTP sets the clock from any answer (Info).** Reviewed 2026-11-04 by
reading `a777178`, `a97e0f5` and ESP-IDF v5.5.5's lwIP. Since `a777178` the firmware
sets the clock at boot with an ask of its own (`ask_ntp`): one request to each of
`0.pool.ntp.org` to `3.pool.ntp.org` in turn, 2 s each, for up to 60 s. It takes an
answer only from the address and port it asked, in mode 4 (a server's), of stratum 1 or
more (not a kiss-o'-death), that echoes the request's 8 random bytes, drawn after Wi-Fi
is up (NB62-2), and gives a time from 2023-11-14 on. So at boot only the server, or a
sender that sees the request, can set the clock. The firmware then starts lwIP's SNTP to
keep the clock in step, and ESP-IDF builds it with `SNTP_CHECK_RESPONSE` 0: it takes any
48 B datagram in mode 4 of stratum 1 or more, from any address, at any time, and sets
the clock to its time, whatever that is. A device on the LAN that finds its UDP port,
one of lwIP's 16,384, can set the board's clock, and set it again after each real
answer, which comes hourly.

The Node reads the clock once, at `bind`, and counts on from it by the monotonic clock
(`Traversal::wall_ms`), so this doesn't move the replay guard. The relay carrier reads
the clock for each event's `created_at` and, on each new connection, for its
subscription's `since` and TLS's certificate check. With the clock set forward, the
relay sends the board nothing and may refuse its events; set outside a certificate's
dates, TLS fails; set back to when an expired certificate of the relay's was valid, TLS
accepts it (the NB62-2 residual). So a device on the LAN can cut the board's internet
side, as it mostly can anyway by spoofing the router's address; the LAN and links
already made carry on.

The boot ask itself:
- One answer sets the clock, so a pool server, or anyone on the path, can set any time
  from 2023-11-14 on, and the Node keeps it until the board reboots. More than 60 s off,
  the board's hellos and offers are dropped (`FRESH_MS`), and peers' hellos and offers
  from that time are fresh to it.
- An unsynchronised server's answer, leap indicator 3 or stratum 16 or more, is taken.
- From 2036-02-07 the server's seconds wrap (NTP era 1), so `checked_sub` fails and no
  answer is taken.
- The Node's clock drifts with the chip's crystal, and lwIP's SNTP doesn't reach it:
  theoretical for a crystal within 10 ppm, 60 s / 10 ppm = 6,000,000 s, about 69 days up
  before a peer with a right clock drops the board's hellos.

Fix, none urgent: drop lwIP's SNTP, as the Node never reads the clock again and the
relay carrier's uses bear a drift of about a second a day, or re-ask with `ask_ntp`
every few hours; skip leap indicator 3 and stratum 16 or more; read a seconds field
under `1 << 31` as era 1 (RFC 4330 §3); and, for the Node, take the time only when two
servers agree within a few seconds.

**Status: fixed** by `4c71c67` (NB-103, merged `9b3959b`), reviewed 2026-11-05 by
reading `272c7b0` and ESP-IDF v5.5.5, with the host tests of `src/ntp.rs` run. The
firmware is now the `peerwire-esp` crate (`ports/esp32`), and it never starts lwIP's
SNTP: nothing in the crate, its `mesh` example or `templates/esp32` calls
`esp_sntp_init`, `esp_netif_sntp_init` or `EspSntp`, ESP-IDF starts lwIP's client only
on such a call, and `CONFIG_LWIP_SNTP_MAX_SERVERS` left the sdkconfig. `clock::sync`
asks at boot, as `ask_ntp` did, and `clock::keep_in_step` asks again every 6 hours, or
every minute while the clock is before 2023-11-14, on a thread at priority 1. Each ask
has its own socket and 8 new random bytes, drawn after Wi-Fi is up, and takes an answer
only from the address and port asked, in mode 4, that echoes them. It now also refuses
leap indicator 3, stratum 0 or 16 and up, and a zero transmit time, and reads seconds
under `1 << 31` as era 1, so it takes a time from 2023-11-14 to 2104-02-26. A device on
the LAN can no longer set the clock by sending to the board: it must see an ask to
answer it, so it must be on the path. Nor can it forge the pool's address blind, as
lwIP's DNS keeps a random query id and source port. Of the fix above, all is done but
the two servers that must agree.

NB95-I2's residuals (accepted):
- One answer sets the clock. The pool server asked, or anyone on the path, can set any
  time from 2023-11-14 to 2104-02-26, at boot as before and now at each re-ask. A device
  on the LAN can still put itself on the path, by spoofing the router's address or
  answering DHCP first, and on WPA2-Personal one that has the network's password and
  heard the board join is on it. The Node keeps the time it was bound with until the
  board reboots, with the effects above: the boot answer's or, since NB103-I2's restart,
  that of the first answer to set a clock no server set at boot. Two servers that must
  agree within a few seconds would stop one pool server, not the path.
- A re-ask steps the clock by any amount, with no limit once the clock is set. It
  reaches the relay carrier (each event's `created_at`, a new connection's `since` and
  TLS's certificate check, as above), not a running Node; the next re-ask a real server
  answers, 6 hours on, sets it back.
- The Node's drift is as above: the re-asks don't reach a bound Node; only the first to
  set a clock unset at boot does, by restarting the board (NB103-I2).
- Firmware that starts lwIP's SNTP itself (`EspSntp`, `esp_sntp_init`) opens the LAN
  path again.

**NB103-I1 — `ntp::time` wraps on a time before 1970 (Info).** `ntp::time`
(`ports/esp32/src/ntp.rs`) subtracts the seconds from 1900 to 1970 from the answer's
seconds unchecked. Seconds from `1 << 31` to 2,208,988,799 (era 0, 1968-01-20 to
1969-12-31) wrap. In a debug build that panics ("attempt to subtract with overflow"),
which on the chip aborts and reboots it. In a release build the 2023-11-14 floor passes
and the clock is set to 18,446,744,073,648,046,464 s, which ESP-IDF's unsigned
microseconds turn into about the year 586,500. Only the server asked, or the path, can
send such an answer, as it must echo the ask's 8 bytes, and either can already set any
time to 2104, so the gain is a debug build's reboot. The host tests miss it: their time
before 2023-11-14 is in 2020. Reproduced 2026-11-05 with a host test,
`answer(1 << 31, 0)`, in both builds. Fix:
`ntp.checked_sub(NTP_TO_UNIX).filter(|&unix| unix >= SET_AFTER)`, refusing the answer
otherwise, and that test. Tried in a copy: the 4 tests and the new one pass in both
builds.

**Status: fixed** by `c0e0ffc` (peerwire-esp 0.1.1), reviewed 2026-11-05: `ntp::time`
takes `ntp.checked_sub(NTP_TO_UNIX)` and refuses the answer when that fails or gives a
time before 2023-11-14, and the host tests send both ends, `1 << 31` and 2,208,988,799.
At `e663830` the 4 host tests pass in debug and release builds; with the old line put
back, the new case fails in both: a debug build panics, and a release one passes the
floor.

**NB103-I2 — The clock's re-asks don't reach a running Node (Info).** `Board::connect`
waits up to 60 s for the clock and then goes on with a warning; the Node, bound next,
reads the clock once. When no server answers in those 60 s, the Node keeps the clock it
had, 1970 after a power-on, until the board reboots: the re-asks every minute set the
chip's clock, not the Node's, so peers with a set clock drop the board's hellos for as
long as it runs, where `Board::connect`'s docs say they do so only until the clock is
set. `RESYNC`'s docs give the replay guard as its reason, which it doesn't reach, and
`ports/esp32/README.md` still says lwIP's SNTP keeps the clock in step. That the Node's
clock stays put is also why a re-ask can't move the replay guard. Fix: say so in the
docs, and to recover, rebuild the Node once the clock is first set, or let the Node move
its clock once from before 2023-11-14 to after it, never again.

**Status: fixed** by `e663830` (peerwire-esp 0.1.1), reviewed 2026-11-05 by reading it
and ESP-IDF v5.5.5. `clock::keep_in_step` notes whether the clock is set as it starts;
if not, the first ask that sets it restarts the chip 5 s later (`esp_restart`), and the
next boot asks before it binds the Node. `peerwire` is unchanged. The docs
(`Board::connect`'s, the module's, `RESYNC`'s, the port's and the template's READMEs and
the ESP32 guide) now say a bound Node reads the clock once, the re-asks keep the relay
carrier's, and the board restarts. The restart can't be driven into a loop. ESP-IDF's
default time keeping (`CONFIG_LIBC_TIME_SYSCALL_USE_RTC_HRT`, which neither sdkconfig
changes) keeps the time set in RTC registers and RTC memory, which `esp_restart` doesn't
reset, so the next boot finds the clock set and doesn't restart again; and no answer can
unset the clock, as `ntp::time` refuses a time before 2023-11-14. So a board restarts
for its clock at most once per power-on, and only when the 60 s at boot got no answer
and a later ask got one: the server asked or the path can cause that, by dropping the
boot's answers, and could already leave the Node at 1970 by dropping them all.

NB103-I2's residuals (accepted):
- Firmware that keeps the time only in the high-resolution timer
  (`CONFIG_LIBC_TIME_SYSCALL_USE_HRT`) loses it at the restart. Then, whenever the 60 s
  at boot get no answer and a later ask gets one, it restarts again: the path, or a pool
  that answers only late, can restart it about every 2 minutes plus the boot,
  **theoretical**: 60 s at boot + 60 s to the first re-ask + 5 s. Its Node is at 1970 in
  between, as when the path drops every answer.
- The restart drops the board's links and its firmware's state, once; `Board::connect`'s
  docs say the board restarts.

Residuals (accepted):
- With one relay, the relay, or anyone who publishes on the board's topics, can deny the
  board's internet side: a flood its inbox can't hold, or one event over 4 KiB, which
  closes the connection (the back-off then allows a handshake every 30 s at most). The
  room topic is in every hello the board publishes, so this needs no room name. The LAN
  and links already made carry on. Skipping a frame over the cap without closing would
  need a websocket reader that discards it unbuffered.
- The Xtensa build (`CHIP=esp32`) compiles ring but has run no TLS: its QEMU pair uses
  the LAN only.

### Connection policy in Rust (NB-097) — NB97-1

`Builder::policy` and `Node::set_policy` (`crates/peerwire/src/traversal.rs`, `node.rs`)
order the paths a node's links may take: `PathKind::Direct` (the peer's addresses and
the ports around its prediction), `Birthday` (the planned punches), `Peers` (through a
member linked to both) and `RelayPeers` (through the node's own relay peers). A kind
left out is off; no policy is every path, as before. A dial takes the kinds a step at a
time, `fallback` apart, and a link moves only to a kind earlier in the order: a relayed
link whose relay's kind ranks before both direct kinds is kept on it
(`Endpoint::set_keep_relayed`). A link on a path a new policy turns off moves (a punch
to a direct path, or a dial through an allowed relay that replaces it,
`Endpoint::redial`) or is closed within 6 s with `DisconnectReason::PathOff`, and the
node refuses a new link on a path that is off. `set_stun_servers` and `set_signaling`
swap servers live and keep the links.

Reviewed 2026-11-04 by reading `d6b42fd` (on `90de9f2`) against the core's path rules
(`endpoint.rs`), with probes on loopback sockets in a copy of the tree. The fixes,
`a00fff5`, were re-checked the same day by reading and with the same probes and one
more, the unfixed code run beside them; that one found NB97-I4, whose fix, `9131188`,
was re-checked the same way. The last probe is in the tree
(`refused_link_replaced_by_an_admitted_one_in_one_batch` in `tests/policy.rs`), and the
first two have tests like them there.

**What holds.**
- A kept link takes no direct path in the core: it starts no punch (`start_punch`,
  `opener`), takes no guesses or spray, and answers a PUNCH with no PUNCH_ACK at our
  address, so a peer's punches don't move it. The core's other ways to a direct path
  don't reach it in a node: `back_to_slow` needs a slow address, which only the C ABI
  sets (`pw_set_slow`), and a latency race on a relayed link races other relays only. A
  relay can't move one: it can't seal a PUNCH_ACK, a probe answered through a relay
  carries the relay token, and a sealed packet it sends on from its own address only
  adds a candidate, which a kept link doesn't punch. The node checks the path of every
  `Connected` and `PathChanged` too.
- `redial` and the replacement it ends in are today's restart. Only the peer's static
  key completes the IK handshake, and a responder's link is announced, and replaces the
  old one, only at its first authenticated data packet, which a copy of an HS1 can't
  produce. A failed redial leaves the old link, and its `ConnectFailed` stays the
  node's. A kept link sends a peer and a relay fewer packets than before, and nothing
  new.
- The swaps: the old carrier is dropped (a `NostrSignaling`'s relay threads exit as
  their command channel closes), and on the new one the node subscribes its inbox and
  its room's topic and says hello at once. A STUN answer is taken only from a server
  asked and waiting, with the transaction id it was sent, so an old server's late answer
  is dropped. A `Policy` holds no url or secret, and nothing new is logged.
- `DisconnectReason::PathOff` is reason 4 in the C ABI. Only the `Node` raises it, never
  the endpoint, so the C ABI and the wasm build never give it.

**NB97-1 — A refused link's messages reach the app (Low).** The node refuses a link on a
path its policy has off by closing it at its `Connected` event (`Node::poll_drained`,
`admits`), but passes the events about that peer that follow: `admits` checks the path
of a `Connected` only, so a room member's `Message`, `AddrObserved` and `PathChanged` go
through. The core queues a link's `Connected` with the frames of the packet that
announced it, and the node reads up to `MAX_READS_PER_POLL` datagrams before it handles
events, so what the peer sent with its first packets reaches the app, from a peer whose
status is `Disconnected` and that never raised `Connected`. Under a policy nothing in
the core declines the handshake (`answers_directly` declines only for a relay-only
node), so the node's check is the only one. Measured on loopback: a node with
`[RelayPeers]` whose peer dialled its invite directly and sent one message as it linked
got `AddrObserved` and that `Message`; a node with `[Direct]`, linked directly to a peer
that restarted and dialled it through a third node linked to both, got
`Disconnected { reason: Replaced }`, then the `Message`, and no `Connected`. The
messages come from the peer itself, which alone can complete the handshake, but over a
path the app turned off and to an app that never saw it link.

Fix: in `poll_drained`, remember the peers whose `Connected` was refused in that pass
and drop their later events until a `Connected` that is admitted. Measured: with it,
neither probe delivers the message, and `tests/policy.rs` passes. Test: in
`direct_off_closes_direct_links_and_refuses_new_ones` the peer sends as it links, and
nothing reaches the node that refuses it.

**Status: fixed** by `a00fff5`, reviewed 2026-11-04. `poll_drained` keeps the peers
whose new link it refused in that pass and drops their later events until a link to the
peer is admitted. The core raises a new link's `AddrObserved`, and the `Replaced` of the
link it replaces, before its `Connected`, all from the packet that announces it, so the
node holds those two until the next event: they go out before an admitted `Connected`,
in the core's order, and are dropped with a refused one. By reading, no other event
comes between a held one and its link's `Connected`, and the check the node makes again
as it hands an event on (`take_event`) agrees with the first, since `Disconnected` and
`AddrObserved` change neither the policy's paths nor room membership. Measured with both
probes, the unfixed code beside them: the refusing node's app got nothing about the peer
from the direct dial, and exactly `Disconnected { reason: PathOff }` from the
replacement (NB97-I1). With no policy the events come in the same order as before
(`observed_addresses_reach_the_app_as_before`). Residual: NB97-I4, since fixed.

**NB97-I1 — A refused replacement ends a link with `Replaced` (Info).** In the second
probe the link the node had, on a path it allows, ended with
`Disconnected { reason: Replaced }`, and no `Connected` followed, though
`docs/reference/rust.md` says one does. Only the peer can do this, and it could close
the link anyway. Fix: the core raises `Replaced` right before the new link's `Connected`
(`announce`), so the node can hold it until then and report `PathOff` when it refuses
the link; or the docs say so.

**Status: fixed** by `a00fff5`, reviewed 2026-11-04: the `Replaced` held for a link the
node refuses becomes `Disconnected { reason: PathOff }`, which goes through the
traversal and the node's status as before; `docs/reference/rust.md` says so. Measured
with the second probe above.

**NB97-I2 — A punch under way follows a link onto a kept relay (Info).** The endpoint
clears the punches of kept links when the policy or the relay peers change, and checks
for a kept link when a punch would start, but not when a punching link moves onto a kept
relay. A link on a relay that isn't kept (a member's, under
`[RelayPeers, Direct, Peers]`) that follows its peer, or wins a race, onto a relay peer
punches on for up to `punch_duration` (5 s), and a PUNCH_ACK then moves it to a direct
path the order ranks after its relay. Read, not measured. Every such order has a direct
kind on, so no path that is off is taken. Fix: skip kept links in the punch timer and in
the PUNCH_ACK that upgrades, as the PUNCH answer does.

**Status: fixed** by `a00fff5`, reviewed 2026-11-04 by reading. The timer stops the
punch, guesses and spray of a link that is now kept before it sends or races anything
(`Session::stop_punching`, shared with `set_keep_relayed`), and the PUNCH_ACK that
upgrades skips a kept link, judged from the link's path after its packet has roamed and
before its frames are read; a packet that comes direct can't move a link onto a relay.
The author's core test has a link follow its peer onto a kept relay mid-punch and then
get the answer to a punch sent before the move: the link stays and punches no more.

**NB97-I3 — A Rust policy hides no address (Info, accepted).** Under an order without
`Direct` and `Birthday` the node still gives out its addresses: in offers and invites,
in the origin tag a relay adds to the packets it relays for the node (`relay_out` asks
for one unless the node is relay-only), and in the HS2 the core sends to a direct HS1
before the node refuses the link. `Privacy::RelayOnly` keeps them back, and the
"Choosing paths" guide says a policy doesn't. Leaving them out of offers alone would
keep back none of the rest, so it isn't done. In JavaScript, `['turn']` without
`'direct'` sends members no address of ours
([Connection policy](#connection-policy-nb-098--nb98-1-nb98-2)).

**NB97-I4 — A refused link can close the admitted one that replaced it (Info).** The
node closes a refused link with `Endpoint::disconnect(peer)`, which closes the peer's
link as the core has it when the node handles the event, not the one the event
announced. Two links of one peer can both be announced in one read, the second replacing
the first: two dials with the same key, from two instances or from a restart while an
old dial is under way. When the first is on a path that is off and the second isn't, the
node closes the second. The peer gets `ClosedByPeer`, while the node's app gets
`Replaced` for a link it never saw, then `Connected` and the second link's messages, and
then holds the peer `Connected` with no link (`Node::path` gives none, `send` gives
`NotConnected`) and no `Disconnected` to follow, until the peer links again. Measured on
loopback: a node with `[Direct]` was linked to a third node; a peer dialled it through
that node and, with the same key from a second socket, directly, and both first data
packets reached it in one poll. `d6b42fd` does the same, so it doesn't come from the
fixes. Only the holder of the peer's key can do it, and it ends only its own link.
Main's refusal of a relay-only node's direct link has the same shape, but there the core
answers a direct HS1 only from a peer it dials or links directly; under a policy it
answers any.

Fix: close refused links after the pass, only for the peers still refused at its end (a
later admitted link replaced the refused one otherwise), and drop a held `Replaced`
whose link was refused. Tried: the probe then got `Connected` and the message with the
link up at both ends, in 3 runs of 3, and the node's tests pass. The probe also tests
what the fixes leave untested, that an admitted link takes its peer out of the refused
list: its check that the admitted link's `Connected` and message reach the app fails
with that removed, with or without the fix.

**Status: fixed** by `9131188`, reviewed 2026-11-04. The fix above, as written: the
refusal closes nothing at once; after the pass, and after the held events have gone out,
the node closes the link of each peer still refused, and an admitted `Connected` for a
refused peer first drops the held `Replaced` of the refused link. `Endpoint::disconnect`
raises no event for the peer (only the links it relayed go back to their slow addresses,
and a node sets none), so the later close tells the app nothing. Measured on loopback
with the unfixed code beside it: `tests/policy.rs` passes, 10 of 10 in 3 runs, and with
main's `node.rs` the probe fails at its `send` to the admitted peer.
`a_replaced_link_reaches_the_app_as_before` pins that a restarted peer's `Replaced`
still reaches the app.

Residual, read: a refused link now stays in the core until the end of the pass, not
until its `Connected` is handled, so an offer the traversal sends a room member on
another peer's admitted `Connected` in the same read can name the refused peer among our
links, and a dial through it fails once it is closed. Main already kept a refused link
in the core from the read to its `Connected`, and offers went out in between
(`handle_signal`, `handle_timeout`).

### Connection policy (NB-098) — NB98-1, NB98-2

`joinRoom({ policy })` and `room.setPolicy()` (`packages/js/src/policy.js`, `room.js`,
`webrtc.js`) say which paths a room's links may take, in order: `'direct'` (WebRTC over
host and server-reflexive candidates), `'turn'` (WebRTC through a TURN server in
`iceServers`), `'peers'` (relayed by a member) and `'signaling'` (the pipe over the
signalling relays). A path left out is off; no policy is every path, as before. The
dialling side runs the order's stages, and a link moves only to a path earlier than its
own. An offer without TURN says `turn: false`. `setPolicy` moves or drops links on a
path it turns off, gives new `iceServers` to the room's connections, and moves
`NostrSignaling` to new relays (`setRelays`).

Reviewed 2026-11-04 by reading `7821d87` against the core's path rules (`endpoint.rs`).
Measured in headless Chromium 153 on loopback with a TURN server of our own
(allocations, permissions, channels) whose mapped address is a TEST-NET-3 one,
`203.0.113.7`, so a server-reflexive address anywhere is plain to see; and with the
fakes of `packages/js/test/room_policy.mjs`. The fixes, `3e055f1`, were re-checked the
same day by reading and with the same probes, the unfixed code run beside them. The
probes aren't in the tree.

**What holds.**
- With `'turn'` and no `'direct'` in the policy the room is joined with, every
  connection it makes (a link's, at any stage or retry, and a call's) has
  `iceTransportPolicy: 'relay'` and the TURN urls alone. Birthday punches are off and
  answer a peer's with `'no'`; retries, a channel that opens late and `adoptChannel`
  need a WebRTC path before the link's. Measured: two tabs linked and in a call; each
  connection gathered one relay candidate, `raddr 0.0.0.0 rport 0`, no signal named a
  host, server-reflexive or peer-reflexive address, and `route()` said `turn: true` at
  both ends. Chromium clears a relay candidate's `raddr` only under `'relay'`: under
  `'all'` it is the server-reflexive address.
- An offer's port prediction (`nat`) reads server-reflexive lines only, so without
  `'direct'` it is empty.
- `turn: false` only narrows: the answerer takes TURN only if its own policy and the
  offer both allow it, and `'direct'` from its own policy alone; an answerer with no
  WebRTC path answers nothing. The field comes in an offer sealed by a member that
  proved the room key.
- A link a peer dials over a fallback our policy has off moves to one it allows or drops
  (`checkPath`), and with pipes off a pipe datagram is dropped before the core sees it.
  The core moves a link on a direct address to a member only when that address is the
  slow one, which the room marks only with `'peers'` before `'signaling'`, so a peer's
  relayed packets can't pull our sends onto a member. What a peer sends through a member
  still reaches us, as the docs say.
- `room.policy` gives each ICE server as `{ urls }`; one it gave, passed back, maps to
  the entry it stands for through a `WeakMap`, and errors name an entry by its index.
  Chromium's `icecandidateerror` gives the server's url without its username or
  credential, and masks the local address (`0.0.0.x`), so the `candidate-error` trace
  names no secret.
- A relay `setRelays` drops is closed for good: its handlers are cleared first, its
  redial cancelled, and what waited on it fails. A doc's copy reads and stores through
  the relays listed only. One added subscribes with the room tag and says hello there.

**NB98-1 — A call doesn't follow a live policy (Low, privacy).** `setPolicy` gives the
room's links their new configuration (`configure()`) but not a call's connections, which
take theirs when made (`rtcConfig()`). So a call connection made under `'direct'` keeps
STUN and `'all'` after `setPolicy({ order: ['turn'] })` whenever its room link stays
(one whose end is on our TURN), and its next ICE restart (`call.js` restarts a
connection that fails, as after a network change) gathers host and server-reflexive
candidates and sends them to the call member. Measured: two tabs with every path, linked
and in a call; the tab whose end was on TURN set `['turn']`. Its link stayed, now under
`'relay'`; its call connection kept `'all'` and the STUN url, and a restart gathered a
server-reflexive candidate (`203.0.113.7`), an mDNS host and a relay one with
`raddr 203.0.113.7`. In the same way a call keeps its servers when `'turn'` is turned
off or new `iceServers` come. The docs say new `iceServers` go to every connection, and
that without `'direct'` none of our addresses reach the peer.

Fix: `setPolicy` gives each call connection `rtcConfig()` (`setConfiguration`) and, when
it turns `'direct'` or `'turn'` off or brings new servers, the impolite side restarts
ICE, as it does when a connection fails. Measured in Chromium: after `setConfiguration`
to `'relay'` a restart gathers one relay candidate, `raddr 0.0.0.0`. The docs should
also say that a policy changed live can't take back the addresses connections sent
before it. Test: after `setPolicy`, a call connection has the room's configuration and
its restart offers relay candidates only.

**Status: fixed** by `3e055f1`, reviewed 2026-11-04. `setPolicy` calls
`Calls.configure()`: each call connection takes `rtcConfig()` (`setConfiguration`), and
when the change turns `'direct'` or `'turn'` off or brings new servers, ICE restarts on
our side, polite or not, since only our side knows of the change (perfect negotiation
settles two restarts that cross). With relay candidates alone and no TURN server, each
call link fails `'no-path'` at once. A call's candidate filter and description read the
policy as it is now, so without `'direct'` only relay candidates go out (NB98-I3).
Measured with the probe above, the unfixed code beside it: the tab on TURN set
`['turn']`; its call connection took `'relay'` and the TURN url alone and restarted by
itself; nothing it gathered or sent from then on, before or after a forced ICE restart,
named an address of ours; and 25 s later it was connected on a relay pair. Unfixed, the
forced restart sent an mDNS host, the server-reflexive `203.0.113.7` and a relay
candidate with `raddr 203.0.113.7`, and the call moved to a pair whose end of ours
wasn't a relay one. The docs say an address sent before a change can't be taken back.

Residuals: WebRTC keeps a call's old pair until the new session connects, so a call on a
path now off runs on it for up to 20 s (`CONNECT_MS`) before `Call.onPath` fails it
`'no-path'` (measured by the author in Chromium 153); it sends no new address meanwhile.
That check reads the kind of the path, not the server: by reading, a call or room link
kept on a TURN server dropped from `iceServers` stays on it while the new servers don't
connect.

**NB98-2 — A relay url the WebSocket constructor refuses (Low).** `checkRelays` checks
the `ws://` or `wss://` prefix only. Chromium's `WebSocket` constructor throws a
`SyntaxError` that names the url, as for `wss://relay.test:99999/?token=…`, and
`setRelays()` calls it after `setPolicy` has set the new policy. Measured with the
test's fake socket throwing as Chromium's does: `setPolicy({ order: ['turn'], relays })`
threw that `SyntaxError`, not a `TypeError`, with the token in its message.
`room.policy` then said `['turn']` and the new relays, while the transport still
answered with `'direct'` and kept pipes on; the relay dropped was closed and the one
after the bad url never dialled. At join the same url fails `joinRoom` with the same
message (before NB-098 nothing checked relays at all).

Fix: check each url with the URL parser (`ws:` or `wss:`, no fragment), the message
naming none; in `setPolicy`, do what can throw before the new policy is set; and have
`dial()` take a constructor that throws as a socket that failed, with its back-off.
Test: the validation section with `wss://r.test:99999/?token=SECRET` and a fake socket
that throws.

**Status: fixed** by `3e055f1`, reviewed 2026-11-04. `checkRelays` parses each url with
`new URL()` and takes `ws:` or `wss:` with no fragment, the `TypeError` naming the entry
by its index (`relays[1]`); every check in `setPolicy` comes before the policy is set. A
url that parses but that the constructor still refuses (a port the browser blocks) is a
socket that failed at once: it is dialled again after the doubling wait, and the error
goes nowhere. Measured with the same fake socket, the unfixed code beside it:
`setPolicy({ order: ['turn'], relays })` threw a `TypeError` naming `relays[1]`, and the
policy, the transport's paths, its pipes and the sockets were as before. With a fake
socket that refuses port 6000, `setPolicy` took that url, the link stayed on signalling,
and the url was tried 1 ms, 1.0 s, 3.0 s and 7.0 s later, never after `leave()`, with
nothing printing it.

**NB98-I1 — A proof deadline outlives a link dropped as it came up (Info).** When
`checkPath` drops a link inside its `'connected'` event (a peer dialled a path ours has
off and there is nowhere to move it), the handler goes on to arm the proof deadline.
`Room.proofMs` (10 s) later `lost(peer, 'unproven')` runs `transport.drop()`, which ends
a WebRTC dial with that peer under way then, and forgets its greeting, retries and
punch. Measured with the test's fakes: armed, and `lost` ran 2,993 ms later
(`Room.proofMs` 3000); with the handler stopping once the link is gone, neither. Fix:
return from `'connected'` when `reroute()` has dropped the peer.

**Status: fixed** by `3e055f1`, reviewed 2026-11-04: `'connected'` returns once
`reroute()` has dropped the peer. Measured with the same fakes: no deadline armed and no
`lost` after it; unfixed, `lost(peer, 'unproven')` ran 3,005 ms later.

**NB98-I2 — An order binds the dialling side only (Info, privacy and cost).** The
answering side answers an offer over every WebRTC path its policy allows, whatever their
place in its order, and a channel that opens for a link on a fallback takes it over
(`dc.onopen`). So `['signaling', 'direct']` or `['peers', 'direct']` still sends our
host and server-reflexive addresses to a member whose policy dials or retries WebRTC
first, and the link moves there; and `['direct', 'peers', 'signaling', 'turn']` gathers
on our TURN server for a member's attempt that includes TURN (an older peer's, or one
under the default order). No path the policy has off is used. Fix: the docs say that
only leaving `'direct'` out keeps our addresses from members; or the answering side
leaves out the WebRTC paths its order puts after the link's own.

**Status: documented** by `3e055f1`, reviewed 2026-11-04: the JavaScript reference and
`index.d.ts` say the order binds the side that dials, with these two examples, and that
only leaving a path out keeps it unused.

**NB98-I3 — Relay-only rests on the WebRTC runtime (Info).** Without `'direct'` the room
relies on `iceTransportPolicy: 'relay'` to gather relay candidates only and to clear
their `raddr`. Chromium 153 does both (above); Firefox, Safari and node-datachannel
weren't checked. Fix (hardening): for a connection made without `'direct'`, send only
`typ relay` candidates, with `raddr` and `c=` cleared as `'public-only'` does.

**Status: fixed** by `3e055f1`, reviewed 2026-11-04. Without `'direct'`, at join or
turned off since (`relayOnly()`), links and calls send relay candidates alone, with
`raddr`, `c=`, `a=rtcp` and the `m=` port cleared (`onlyRelayCandidates`,
`candidateFor`), and no port prediction, whatever the runtime gathers; the author's
fakes gather host and server-reflexive candidates under `'relay'` to test it. Residual:
our ICE checks still go out from what the runtime gathers, so one that ignored `'relay'`
would still show a peer our address as a peer-reflexive one. Firefox, Safari and
node-datachannel remain unchecked.

**NB98-I4 — A link kept on TURN keeps its old ICE session (Info).** A link made with
both WebRTC paths whose end is on TURN stays when `'direct'` is turned off. It gets the
relay-only configuration for its next restart, but until then its session keeps its host
and server-reflexive pairs, and ICE may switch to one of them, a path now off, with
nothing to check it again. Fix: when `'direct'` is turned off, the dialling side
restarts ICE on such a link, as it does for new servers.

**Status: fixed** on the dialling side by `3e055f1`, reviewed 2026-11-04: with
`'direct'` turned off, `recheck` restarts ICE on a link kept on our TURN, under the
relay-only configuration, as for new servers. Residual: the answering side's restart ask
is heeded only while ICE is down (NB46-1), so its kept link keeps the old session until
the dialling side restarts (a policy change of its own, new servers, a failure); and the
room checks a WebRTC link's path only when a policy is set, so a later move onto one of
that session's host pairs goes unchecked. A call restarts on whichever side changed
(NB98-1).

### TURN client in Rust (NB-099) — NB99-1, NB99-2

Feature `turn` (`crates/peerwire/src/turn.rs`, on by default) gives a Rust node TURN
servers of its own (`Builder::turn_servers`, `Node::set_turn_servers`; RFC 8656 over
UDP), with a username and password, or credentials minted for each allocation from a
REST secret (`TurnServer::rest`: the username `expiry:user`, the password
base64(HMAC-SHA1(secret, username))). TURN runs only with `PathKind::Turn` in the
policy, and until NB-101 not under `Privacy::RelayOnly` (see
[TURN under relay-only in Rust](#turn-under-relay-only-in-rust-nb-101--nb101-1)). The
node keeps an allocation on each server, offers its relayed address in a tagged `fe 03`
entry, and a dial's `Turn` step goes through the allocation (a channel for each peer
address, Send indications until it is bound) to the peer's addresses, or from the node's
socket to the peer's relayed address. A link at a relayed address counts as `Turn`, for
the policy and in `path_kind`, and leaves it as a link leaves a relay: by a punch to a
direct address, answered there. The codec is `peerwire-core/src/stun/turn.rs`.

Reviewed 2026-11-05 by reading `4c61f49` (on `9131188`) against the core's path rules
(`endpoint.rs`) and RFC 8656, with probes on loopback sockets, and a loopback TURN
server (the simulator's, `peerwire_sim::turn`), in a copy of the tree. The fixes,
`c3585f9` to `6b1b3a4`, were re-checked the same day by reading and with the same probes
and one more, the unfixed code run beside them, and two mutants of the NB99-I1 fix. The
probes aren't in the tree; the author's tests like them are.

**What the TURN server sees.** The server is the app's (coturn, Cloudflare TURN, …),
trusted with metadata only. It sees our address as it reaches it, the IPs we let in
(each peer's last offer's addresses and relayed ones) and the addresses we send to, when
and how much goes each way, how long the allocation lives, and the username: with REST
credentials, `expiry:user`, so the app's name for the user and when they were minted.
What it carries are Noise packets, sealed end to end: it can drop, delay, reorder or
replay them (the link's replay window drops a replay), not read or forge one. It also
picks the relayed address we offer, so a server that lies points the peers' TURN dials
at an address of its choosing, which gets their handshakes, as an offer of any address
can; since NB99-1's fix, naming an address as relayed changes nothing about other peers'
packets from it.

**What holds.**
- The codec reads every attribute within bounds. MESSAGE-INTEGRITY is checked with
  `verify_slice`, which takes only the full 20-byte tag and compares in constant time;
  FINGERPRINT must come last, and a message has at most 24 attributes. `fuzz_turn` runs
  on it.
- Transaction ids are 96 bits from ChaCha20. A success response counts only from the
  server's address and under our key; a request takes at most 2 challenges, and a 401 to
  one already sent under our key (wrong credentials) ends it. A failed allocation is
  tried again after 5 s, doubling to 300 s. ChannelData is taken only on a bound channel
  of a live allocation, a Data indication only from a permitted IP. No credential, key
  or MESSAGE-INTEGRITY is in a `Debug` output or an error.
- From a server's address (`c3585f9`, for one server as both STUN and TURN),
  ChannelData, Data indications and the answers to the TURN client's requests go to it,
  other STUN to the STUN client, which takes a Binding answer only for its pending
  transaction, and anything else is dropped. Nothing from a server's address reaches the
  endpoint as from a network address, so no link is taken to be at the server.
- OBSERVED is neither sent nor taken on a relayed path, and our relayed address is never
  taken as our NAT's mapping.
- Decoders from before NB-099 skip the `fe 03` entry: `SocketAddr::decode` needs a first
  byte of 4 or 6, and the u32 and u64 fields need their exact length.
- Under `Privacy::RelayOnly` the node never dials a peer's relayed address from its
  socket, and allocates nothing unless the policy lists `Turn` (NB-101).

**NB99-1 — An offer's relayed address relabels everyone's packets from it (Medium).** At
`4c61f49`, `on_offer` put each address in a peer's `fe 03` entry in `relayed_of`, and
`Traversal::hop`, which every datagram on the node's socket goes through, took anything
from such an address as from a relayed one, whoever sent it. So a room member M that
names another member V's address as its relayed one makes our node take V's packets as
relayed: V's link is a TURN link, with its observed addresses ignored, which the policy
may have off, and it can move through M. This needs no TURN server, and works with
feature `turn` off (`turn_off.rs` kept `relayed_of` and `hop`). Reproduction: on
loopback, a room of A, M and V on `MemoryHub`, with M changed only to add V's socket
address to its offers' relayed entry, and the same run without the claim beside it. With
no policy, A's link to V was `Turn`; under `Policy::default()` it went through M; under
`[Direct]` there was none. Without the claim, each was direct to V's socket. Fix:
relabel a datagram only if it names the claimant's link, by its receiver index; a
handshake's first message names none, so its link is retagged when it comes up.

**Status: fixed** by `b2b2919`, reviewed 2026-11-05. `Endpoint::peer_of` reads the
receiver index of an HS2, Data or Cookie packet from its header, and `hop` relabels a
datagram only when that names the address's claimant; the retags, on `Connected` and in
`on_offer`, are the claimant's only. Measured with the same probe, the unfixed code
beside it: with the claim, A's link to V was direct to V's socket under all three
policies, as without it; unfixed, `Turn`, through M, and none. The index isn't
authenticated, so it is a hint: a forged one fails that link's keys, and a claimant that
sends from the address it claimed moves only its own link. Residual: NB99-I2 (fixed).

**NB99-2 — Full TURN tables lock out later peers (Low).** At `4c61f49`,
`TurnClient::permit` refused a new IP once 64 were held (`MAX_PERMISSIONS`), `send`
bound no channel past 64 (`MAX_CHANNELS`), and channel numbers were never used again;
`on_offer` let in the IPs of every offer it took, up to 10 each, and an entry stays
while wanted (permitted or sent to in the last 10 minutes). So the members' offers fill
the tables, and a peer met later gets nothing through our allocation, either way.
Reproduction, a unit test against the simulator's server: with 64 IPs permitted and sent
to, a later peer got 0 datagrams out and 0 back; with 63, or none, 3 out and 1 back.
Fix: a quota for each peer; when full, the entry wanted longest ago gives way; channel
numbers used again.

**Status: fixed** by `41471eb`, reviewed 2026-11-05. With 64 held, a new permission or
channel takes the place of the one wanted longest ago. A channel number let go (evicted,
no longer wanted, or unanswered) is bound again only 15 minutes later, the server's 10
and the 5 that RFC 8656 §12 asks, and a new allocation starts afresh. A peer's IPs are
its last offer's (`TurnSet::permit`): those no other peer holds are let go, and peers
the traversal forgets hold none. Measured with the same probe, the unfixed code beside
it: with 64 held, the later peer got 3 out and 1 back; unfixed, 0 and 0. Residual: the
tables are still bounded, and a member with many identities (anyone with the room key
can make them) can push entries out. A peer still sent to keeps its permission at the
server, as each ChannelBind renews it; one not sent to gets in again with its next
offer; while all 4,096 channel numbers are bound or quiet, new peers get Send
indications.

**NB99-I1 — The policy read a link's path before its retag (Info).** A link that a
peer's dial through its allocation makes comes up at its relayed address as from a
network address, and is retagged on `Connected`, or by the offer. At `4c61f49`,
`handle_event` retagged, then gave `follow_policy` the event's path, direct to a network
address, and the node's admission check used the same; `on_offer`'s retag checked
nothing. So a TURN link was judged as a direct one: kept under `[Direct]`, refused under
`[Turn]`. Fix: read the path after the retag.

**Status: fixed** by `b2b2919`, another way, reviewed 2026-11-05. Reading
`ep.path(peer)` again would read the peer's current link, which can be a later one than
the event's (NB97-I4). Instead `allows` takes an address a peer offered as relayed as
TURN's for that peer only, so the node and the traversal agree whichever came first, the
link or the offer, and `on_offer` runs `follow_policy` after its retag. Read against
both orders. The author's test
(`a_link_at_a_relayed_address_follows_the_policy_in_either_order`) moves the link off
under `[Direct]` in each, and fails with either half of the fix taken out: tried, both
mutants caught.

**NB99-I2 — A later claim of a relayed address takes it from the first (Info).**
`on_offer` keeps one claimant for each relayed address (`relayed_of.insert`), so a
member that offers another peer's relayed address as its own takes it over, until that
peer's next offer. NB99-1's fix relabels only the claimant's packets, so the first
peer's packets from its own relayed address then come in as from a network address.
Measured on loopback: T linked to B through T's allocation, both under `[Turn]`, and M,
a member under the default policy, offering T's relayed address. M held the claim at B
in 3,682 of 3,683 polls over 8 s, and the link stayed on TURN at both ends, as packets
from a network address move nothing on a link at a relayed one. After B closed the link,
T's offer came before its dial and took the claim back, and the link came up on TURN
again. By reading, a handshake from that address that lands while M holds the claim
comes up as from a network address and isn't retagged, so the policy reads it as direct
(refused under `[Turn]`, kept under `[Direct]`) until T's next offer retags it. Fix:
keep every claimant of an address (key `relayed_of` by address and peer), so a claim
relabels only its claimant's packets and takes nothing from another's.

**Status: fixed** by `1551da7` (NB-100, merged with NB-101), reviewed 2026-11-05.
`relayed_of` keeps each address's claimants, and `hop`, `retag` and `allows` take an
address as relayed only for a peer that claims it; a peer's offer replaces its own
earlier claims and no one else's. Read against every use of the map; the author's test
(`a_later_claim_leaves_the_first_claimants_address`) keeps the first claimant's packets
relayed after a second claim. Residual: the map holds at most 256 claims, 2 for each
peer, so a member with 128 identities fills it, and a later peer's claim is refused
until those are forgotten (10 minutes after the node last heard of them, unless linked);
that peer's packets from its relayed address then come in as from a network address, as
before the fix.

**Residuals, accepted.**
- UDP to the server only: no TURN over TCP or TLS, so a network that blocks UDP gets no
  TURN path.
- A link on TURN moves to a direct path ranked earlier once a punch answers, but not to
  a relay peer, even one ranked earlier (the guide says so).
- In Rust, `[Turn]` hides no address: offers still carry the node's addresses, a peer
  can name its own address as relayed, and a link to a peer's relayed address goes from
  our socket, so the peer's TURN server sees our address. `Privacy::RelayOnly` hides
  them; since NB-101, with `Turn` in the policy, TURN runs under it through the node's
  own allocations alone, so peers see the TURN server's relayed address. The TURN server
  still sees ours, and the relayed address hides it only if the server isn't on the
  node's own host or behind its NAT
  ([TURN under relay-only in Rust](#turn-under-relay-only-in-rust-nb-101--nb101-1)).
- Off path, a sender that spoofs the server's address injects only what a direct sender
  could: ChannelData on a bound channel (numbered from 0x4000, so easy to guess) or a
  Data indication from a permitted IP becomes a datagram from that peer's TURN hop,
  which must pass that link's keys.
- Error responses aren't checked for integrity, so on path an attacker can fail an
  allocation or a refresh, as it could drop them; the next try backs off, to 300 s.
- The password, or the REST secret, stays in the node's memory for its life, to make new
  allocations. The secret mints credentials for anyone who holds it, and the rustdoc
  says to use it only in apps the developer runs.
- NB99-2's and NB99-I2's bounds, above.

### TURN under relay-only in Rust (NB-101) — NB101-1

Since NB-101, a `Privacy::RelayOnly` node with TURN servers of its own and
`PathKind::Turn` in the policy (`Traversal::turn_private`) runs TURN, and links to peers
but its relay through its own allocations as well as through the relay. Offers to those
peers carry the relayed address alone, with no port prediction, and its invites carry it
too; offers to the relay carry the node's addresses and no relayed one, as it links
directly. Dials and `Node::connect` take `Turn` and `RelayPeers` (the relay) in the
policy's order, `RelayPeers` last if left out, through hops of the node's own
allocations only (`Hop::Turn`), never from its socket. In the core, `Address::own_turn`
marks such a hop, and relay-only takes a path through one as hidden, as it does a relay:
a dial given one goes through those and its relays alone, a handshake through one is
answered, and a link on one takes no candidates and drops, before its replay window and
decryption, what the peer sends from anywhere else. Turning relay-only on, or TURN on
under it, moves a link that shows the node's address onto an allocation at once
(`Endpoint::retag`) and dials it again there and through the relay; a link whose
allocation goes moves to the relay or closes with `PathOff`. Without `Turn` in the
policy, relay-only is as before. NB-100, on the same branch, fixed NB99-I2
([TURN client in Rust](#turn-client-in-rust-nb-099--nb99-1-nb99-2)).

Reviewed 2026-11-05 by reading `2448dc5` (NB-100 `1551da7`, the core `308f462`,
traversal and node `9b1f91e`, on `d919967`) against the card's rule that nothing to a
peer but the relay leaves the node's socket, at every place that sends, with probes on
loopback sockets and a loopback TURN server and unit probes in the traversal's tests, in
a copy of the tree, `d919967` run beside them. The fixes, `016c77f` and `ee3ef67` (docs
`bed6f86`), were re-checked the same day with the same probes and one more, `2448dc5`
run beside them, and a trial of NB101-I2's fix. The probes aren't in the tree; the
author's tests like them are, and check each datagram's source at the peer's socket
(`Node::note_sources`, feature `testing`).

**What holds.**
- The core's relay-only checks take an own-allocation hop as hidden and nothing else:
  `Net` addresses and a peer's relayed ones (`Hop::Relayed`) stay blocked. No spray or
  guess goes out on an own-allocation link or dial, an HS1 is answered directly only
  through the allocation or the relay, a quiet link falls back only to `Via(relay)`, and
  no OBSERVED frame is sent.
- `Hop::Turn` never reaches the socket: the node hands it to the TURN client, which
  drops it with no allocation, so a lost allocation pushes nothing onto the socket.
- Offers are sealed for each peer, so the node's addresses reach the relay alone.
- Switching to relay-only closes the links to relay peers but the relay (with TURN on,
  without a Close); `keep_relay_links` doesn't dial them again under relay-only, and
  router port mapping stays off.
- `Endpoint::abandon` (`disconnect` with no Close) is called only from
  `Traversal::hang_up`, and only under relay-only with TURN, for a peer but the relay on
  a direct path that isn't an own allocation; every other close still sends its Close.
- NB-100: `relayed_of` maps an address to its claimants; `hop`, `retag` and `allows`
  check the pair, an offer drops only its sender's earlier claims, and the 256 cap
  counts claims.

**NB101-1 — `connect` to a relay peer's invite dialled from the socket (Low).** At
`2448dc5`, `Node::connect` under relay-only with TURN took the step-by-step dial through
the allocations and the relay for any invite but a relay peer's
(`Node::set_relay_peers`), and with TURN on skipped the old `Via(relay)` substitution,
so a relay peer that isn't the node's relay was dialled at its invite's addresses from
the node's socket. The node refused the link, but its HS1s had gone out, so that peer
learnt the node's address, which relay-only promises it won't. Reproduction: on
loopback, A relay-only with no relay and `[Turn]`, its allocation up on a loopback TURN
server, and P's invite pointing at a sniffer socket; `A.set_relay_peers([P])`, then
`A.connect(P)`. 3 HS1s reached the sniffer from A's socket; an ordinary peer's invite
gave 0 (its 3 came from the relayed address), and `d919967` failed with `NoRelay` and
sent nothing. Fix: a relay peer's invite takes the stepped dial too.

**Status: fixed** by `ee3ef67` before NB-101 merged, so never on main; reviewed
2026-11-05. Measured with the same probe, `2448dc5` beside it: 0 datagrams from A's
socket, the 3 HS1s from the relayed address; unfixed, 3 from the socket. The author's
loopback test (`relay_only_connects_to_an_invite_through_its_allocation`) links to a
relay peer's invite on `Turn`, with nothing at the peer's socket from A's.

**NB101-I1 — A link no allocation relays to stayed on the socket (Info).** At `2448dc5`,
`hide()`, which takes a link that shows the node's address off it when relay-only comes
on with TURN, retagged it onto an own-allocation hop only at the same address, then
dialled it again through the allocations and the relay, the link kept meanwhile. With no
hop at that address (an IPv6 link and IPv4 allocations, or no allocation up while a
relay is linked), the link stayed on the socket until the new dial landed or 6 s passed
(`MOVE_WAIT`), and its close sent a Close from there. Reproduction, a traversal unit
test: a link at `[2001:db8::2]:7000`, an IPv4 allocation, the peer's `203.0.113.2:7000`
known; `set_privacy(RelayOnly)`, then an app send. The path stayed `Net`, and 10
datagrams left the socket over 6 s (the data, its retransmits, the Close at 6,000 ms),
then `PathOff`. The peer knew the address already, and on main such a link stays where
it is, hence Info. Fix: retag onto any own-allocation hop, else close the link and dial
afresh through the relay.

**Status: fixed** by `016c77f` and `ee3ef67` before NB-101 merged, so never on main;
reviewed 2026-11-05. `hide()` retags onto the hop at the link's address, else onto the
first own-allocation hop, at another of the peer's addresses; with none, it closes the
link and dials afresh through the relay, if one is linked. Such closes, and relay peers'
at the switch, send no Close (`Endpoint::abandon`). Measured with the same probe,
`2448dc5` beside it: retagged onto the allocation's hop to `203.0.113.2:7000` at once,
and 0 datagrams from the socket; unfixed, 10. Residual: NB101-I2 (fixed).

**NB101-I2 — A link mid-move when relay-only comes on stays on the socket (Info).**
`hide()` returns early for a link whose move is under way (`moving`), before its retag.
So a link at a peer's address that a policy change is moving when relay-only comes on
with TURN (`set_policy` with its path off, such as `[Turn]`: the link is dialled again
through the allocation) stays on the socket until the move lands or 6 s pass, and the
app's data on it, with its retransmits, goes from there. Measured at `bed6f86` with a
traversal unit probe, the move unanswered: the path stayed `Net`, and 9 datagrams left
the socket over 4.2 s, then `PathOff` at 6 s; at `2448dc5`, 10, its Close included. With
the peer answering, the window is the move's round trip through TURN. An app that turns
on TURN-only privacy with `set_policy([Turn])` and then `set_privacy(RelayOnly)` meets
it; the other order doesn't. The peer knew the address already, hence Info. Fix: retag
(or close) before the `moving` guard, and for a move under way mark the link to close if
the move fails, with no second dial. Tried in a copy: 0 datagrams from the socket, the
link still closed at 6 s, and the crate's unit tests and its TURN, policy, privacy and
relay-peer tests pass.

**Status: fixed** by `807d66a` (NB-108), reviewed 2026-11-05: the fix tried above.
`hide()` retags the link onto an own-allocation hop, or closes it, before the `moving`
guard; a link whose move is under way is then marked to close if that move fails or is
late, and isn't dialled again. Measured with the same probe, `272c7b0` beside it: the
path went to `Turn` at once, 0 datagrams left the socket after the switch (14 went
through the allocation), and the link closed at 6 s; unfixed, 9 from the socket. The
author's test (`relay_only_takes_a_link_mid_move_off_its_socket`), with the switch 1 s
into the move, asserts the same and fails on `272c7b0`. A moving link with no allocation
up is now closed at the switch, with no Close, and the relay, if one is linked, is added
to its move's dial. A second probe, with no relay, a relay the move's dial didn't name
and one it did, gave 0 datagrams of the link from the socket after the switch; unfixed,
9 each. That dial runs on: the first residual below.

**Residuals, accepted.**
- A dial under way when relay-only comes on, that neither an allocation nor the relay
  can take, runs on to its end at addresses the peer had from earlier offers, and a link
  it makes is refused, with a Close from the socket. The guides name this exception. A
  policy change's move with no allocation up, whose dial named no relay, is one, even
  with a relay linked: `hide()` closes the link and adds the relay to the move's dial,
  which keeps its other targets, as a direct dial given a relay does under relay-only.
  Measured (NB-108): the move's HS1s to the peer's relayed address went on from the
  socket, 4 after the switch (at 1.5, 3.5, 7.5 and 15.5 s of the dial's 23.5 s), as on
  `272c7b0`; the dial had sent it 2 before.
- A link closed without a Close: the peer keeps its end until its idle timeout, and its
  packets to our socket count as an unknown session and get no answer. With a relay, the
  fresh dial through it replaces that end.
- Under handshake load, an HS1 at the node's socket gets a cookie before any relay-only
  check (`on_hs1`, as before NB-101). Only a sender that has the address reaches the
  socket, but it can spoof the source the cookie goes to, and so show the address to a
  third party, as it could by telling it.
- The TURN server sees our address and every peer's, as for NB-099, and the relayed
  address hides ours only if the server isn't on the node's own host or behind its NAT:
  there it carries our public IP.

### Bluetooth LE (NB-110, NB-112) — NB110-1…NB110-12, NB112-1

Since NB-110, a node given a radio (`Builder::bluetooth`, `peerwire` feature `ble`)
links with peers in Bluetooth LE range over GATT, beside its UDP paths, and finds room
members over the radio; feature `ble-bluez` adds the Linux driver, over BlueZ's D-Bus
API. The design is in [Bluetooth](../concepts/bluetooth.md): an advert with a random `n`
and a 4-byte tag under the room's ble key, and no `PeerId` or topic; a room proof on
each connection (a challenge, an answer and a proof under that key, with no clock), then
each end's `PeerId` and its signature over both challenges; signals sealed under a key
of the connection's own; the framing codec in `peerwire-core`; and the core's rules for
a connection no peer is bound to yet (`Endpoint::handle_datagram_unbound`). A Bluetooth
hop ranks as a relayed address does, last, and isn't in `Policy::default()`. The ESP32's
NimBLE driver (NB-110c) and the browser's (NB-110d) aren't on main yet, and none of it
is in a release.

Reviewed 2026-11-05 in nine passes: security's requirements, BLE-R1…R15 (NB-110b), read
against main `25c92d0` and the `bluer` 0.17.4 and ESP-IDF 5.5.5 sources; the spec at
`ac2d952` (NB110-1…NB110-5); the code from `ebf3530` to `3ae92bc` (NB110-6…NB110-8,
I2…I5); and re-checks of `287ebd8` (NB110-9, I6…I9), `d202fd3` (NB110-10, I10),
`b95141b` (NB110-11) and `708038f` (NB110-12, T1); then NB-112 at `86fee9d` (NB110-12
and T1 fixed; NB112-1, I1, I2) and its fix `680e273`, both by reading, as their D-Bus
paths need an adapter. Probes ran in a copy of the tree, each beside its control on the
unfixed head: on real Nodes over the in-process fake radios, and in the node's simulated
air. BlueZ's behaviour was read from its 5.87 source. The probes aren't in the tree; the
author's tests of the node's findings are. Nothing was run over a real radio, as the
review host has none: the radio checks named below are owed on our own hardware.

**What holds.**
- The advert carries the flags, `n` and the tag alone (R1, R2), and `n`, the tag and the
  address change together (on Linux, see NB110-4). A scanner takes at most 50 adverts a
  second and connects to none whose tag isn't its room's.
- Before the room is proven a connection carries the challenge, the answer and the proof
  alone, and anything else closes it; a peripheral sends a non-member nothing it could
  use. The labels (`pw-ble-adv`, `-c`, `-p`, `-k`, `-id`) are distinct, each end's fresh
  challenge is in what the other MACs and signs, so no proof moves to another
  connection, and the identity key signs nothing else, so a `pw-ble-id` signature is no
  other signed message. A proof in the receiver's own name, or a `PeerId` that isn't a
  valid point, fails.
- Signals are sealed with AES-256-GCM under the connection's key, with a nonce of the
  direction and `seq`, and `seq` must go up. No topic goes over the air, and inside,
  every check of the other carriers stands (the hello key, room proofs, freshness, the
  budgets).
- The framing codec (`Chunker`, `Reassembler`) holds one partial message per direction,
  at most 1,233 B, and drops it on a gap, a repeat, an overflow or 2 s of silence.
  `fuzz_ble_framing` (`43c8974`, merged `009ce41`) runs it through loss, copies, swaps,
  delays and byte mutations at every MTU from 0 to 600.
- A connection's address (`Hop::Ble`) is never offered, sent in an `OBSERVED` or relay
  frame, or decoded from one, and its datagrams never reach the UDP socket. No race,
  punch answer or late packet moves a link onto Bluetooth, and no peer relays across it
  (R11, R12). Under `Privacy::RelayOnly` no link goes over the radio (R13).
- Budgets: signals per connection and new handshakes across the radio, each dropped over
  its budget before any cryptography; identity checks across the radio, before the
  signature is checked; and a close after 32 messages dropped. Linux's `rx` takes only
  writes without response at offset 0, from the served central; nothing is readable
  (R7).
- No `pw_*` function changed, so no C ABI review was owed.

**NB110-1 — An unbound connection could take a peer's link (Low).** In the spec at
`ac2d952` a connection was bound to a peer by the first packet that authenticated on it,
checked after the core had taken the packet. So a data packet of Alice's that the
receiver hadn't seen yet, delivered on a new connection Z, roamed her link from her own
connection Y onto Z (two Bluetooth hops are one kind), and an HS2 finished a dial on any
path; Z then bound to Alice, and the one connection per peer rule closed Y. Such a
packet comes from sniffing the link layer, which isn't encrypted, while jamming the
receiver. Effect: Alice's Bluetooth path cut, its contents still sealed; with no Wi-Fi,
the whole link. Reproduction, for the code: fake radios, Alice linked on Y; her next
data packet taken off Y and delivered on a new Z binds Z and closes Y. Fix: while
unbound, take only an HS1, an HS2 or cookie for a dial that targets the connection, and
data for a session whose path it already is, and bind on a fresh packet of a session on
it, never on an HS1.

**Status: fixed** by `7ae7ac8` and `4cb542f` before NB-110 merged (`74c7452`), so never
on main; reviewed 2026-11-05. The author's test sends a member's copy of Alice's packet
over its own connection: dropped unread, with nothing moved, bound or closed; and an HS1
binds no connection (`an_unbound_channel_binds_on_a_fresh_packet_not_an_hs1`).

**NB110-2 — Replayed hellos held every unbound slot (Low).** At `ac2d952` an unbound
connection stayed open while a checked signal came over it every 60 s, and a central
over the 4 such slots (2 on an ESP32) was refused. A device that collected members'
sealed hellos, off its own connections or by sniffing, and sent them back within their
freshness held every slot, so no browser or native central could connect. Fix: a
newcomer closes the oldest unbound connection instead of being refused. At `ebf3530` two
gaps were left: a node's own connects never closed one, and the oldest went first even
when it was proven and a newer one wasn't. Fix: a standby that proves the room into the
last unbound slot closes the weakest other (unproven, then with no proven peer, then the
oldest), for connections made and taken.

**Status: fixed** by `4cb542f` and `33b1f4f` before NB-110 merged, so never on main;
reviewed 2026-11-05.

**NB110-3 — Unbound connections had no handshake budget (Low).** At `ac2d952` the limits
covered signals only. On an ESP32 (`max_pending_handshakes` 8, cookie mark 6) a member
on one unbound connection could send distinct HS1s and fill all 8 half-open slots for 10
s, so every new handshake was dropped, UDP's too. Fix: one distinct HS1 per unbound
connection per 10 s, its resends passing, and, as a member can reconnect after each, a
budget across the radio: `unbound` at once, then one per `hs1_gap`.

**Status: fixed** by `fee4639`, `ebf3530` and `33b1f4f` (`Limits::SMALL.hs1_gap` 5 s)
before NB-110 merged, so never on main; reviewed 2026-11-05. Residual: Bluetooth can
hold at most 4 of an ESP32-C3's 8 half-open handshakes, leaving UDP 4 (**theoretical**:
`unbound` + `handshake_timeout` / `hs1_gap` = 2 + 10 s / 5 s), and the budget is shared
(NB110-I5). The UDP form is NB110-I1.

**NB110-4 — The address and the tag rotated apart (Low).** At `ac2d952` the tag changed
every 15 minutes and the address on the stack's own timer, so each change of one was
bridged by the other, unchanged, and a listener could link the whole chain; and BlueZ
advertises from the adapter's fixed address unless privacy is on. Fix: each `advertise`
with new data comes from a new random address, so the address, `n` and tag change
together.

**Status: fixed** on the node's side by `ec79f1a` and `fee4639` before NB-110 merged;
reviewed 2026-11-05. On Linux BlueZ picks the address, and the driver can't: with
`Privacy=device` it advertises from a resolvable private address that changes every 15
minutes, on its own timer, and without it from the adapter's own address, which tracks
the machine. Accepted as documented in [Bluetooth](../concepts/bluetooth.md).

**NB110-5 — The host's name as the GAP name (Low).** BlueZ serves the adapter's alias,
the host name unless set, as the GAP device name to any central that connects. Fix: the
spec names the GAP name `Peerwire`; on Linux, `BlueZ::names_this_host` says when the
alias is the host name, for the app to warn, and the docs show how to set a neutral one.

**Status: fixed** by `ec79f1a` and `4cb542f` before NB-110 merged; reviewed 2026-11-05.
`ble_chat` prints the alias and warns. On Linux the name stays the system's: accepted as
documented.

**NB110-6 — Connection rules acted on what any member claimed (Low).** At `ebf3530` a
hello's sender was only room-proven, and a central's answer named its own advert's `n`
under the ble key alone, so a member could name anyone. Three rules acted on such
claims: the oldest connection claiming a peer won, so a newer one carrying that peer's
signals closed; a central's claimed `n` kept this node from connecting to that advert,
and for 60 s after; and claims pushed real connections out of a peer's dial targets.
Reproduction, real Nodes over the fake radios: (c) Mallory takes Alice's hello off a
connection to Alice and replays it on her own connection to Bob, made before Alice's, so
Bob closes Alice's connection the moment her hello comes over it: no link in 25 s,
against 2.04 s with Mallory silent; (b) Mallory names each one's advert `n` to the
other: no link in 20 s, against 2.04 s. Effect: any member in range keeps two others
from linking over Bluetooth, and from linking at all with no Wi-Fi. Fix: each end proves
its `PeerId` on the connection, by a signature over both challenges, and only a peer
proven so, or a session bound to the connection, counts for anything; the answer names
no advert.

**Status: fixed** by `33b1f4f` before NB-110 merged; reviewed 2026-11-05. (c) links in
89 ms on `287ebd8`; (b) is moot, as the answer carries no `n` and only adverts this end
connected to count. The author's test `a_member_cannot_pass_on_anothers_identity` covers
a member passing on another's proof. Accepted with it: a signature rather than a sealed
box (NB110-I6), and the first proven connection to a peer stays while a second closes at
the end that made it.

**NB110-7 — A central with no room key kept a desktop pair apart (Low).** At `ebf3530` a
node connected to no advert while any central connected to it was unproven, up to 10 s,
and on a desktop (one central) an unproven central marked the advert full, so a second
was refused before eviction ran. Reproduction (a): a device with no room key connects to
both nodes, never answers, and reconnects whenever it is closed: no link in 25 s, with
one central or three, against 2.04 s with no such device. With the hold-off gone and 3 s
to prove (`33b1f4f`), three centrals linked in 83 ms, but a desktop pair still didn't:
the refused central then held that advert 60 s. Fix: a newcomer over the limits takes
the place of a central that hasn't proven the room after 1 s, the advert's full flag
counts proven centrals only, and a central whose connection closed before any challenge
came holds that advert 1–4 s at random, not 60 s (a fixed 3 s locks in phase with a
device that cycles on the 3 s deadline).

**Status: fixed** by `33b1f4f` and `d202fd3` before NB-110 merged; reviewed 2026-11-05.
(a) with desktop limits links in 2.45 s and 8.57 s (two runs), against 83 ms; the
author's tests cover it. Residual: NB110-I8.

**NB110-8 — The BlueZ driver bound its one central by the first write (Low).** At
`ebf3530` the driver served as its central whichever device wrote first. So a device
that wrote first took over an honest central's `rx` side (a junk write closed it; with
the room key it proved in its place), and a central that subscribed and never wrote
wasn't disconnected when the node closed it. Fix: serve the subscriber BlueZ hands over
with its address (`AcquireNotify`), take writes from that device alone, and disconnect
it on close.

**Status: fixed** by `287ebd8` before NB-110 merged; reviewed 2026-11-05, by reading.
One BlueZ behaviour is a radio check: BlueZ hands over a socket only for a subscription
that comes while none is open, and only counts any other, so a second central that
subscribes while one is served, and stays silent, is never seen by the driver; it gets
the served central's notifications, which are sealed (sizes and timing only). Whether
the next central is served once the first is closed is the check (S1 served, S2 silent,
S1 closed, then H); if it isn't, NB110-8 reopens, and the fix is to register the GATT
application again when the served central's session ends.

**NB110-9 — A desktop refused every central while its advert said it took them (Low).**
At `287ebd8` a central over the limits could close only a connection that hadn't proven
the room after 1 s, while the full flag left out two others: a central whose peer hadn't
proven who it is, and a second connection to a peer. So a member holding two connections
to Alice, one each way, with the larger `PeerId` so that the tie-break closed neither,
or one that proves the room and never who it is and reconnects when closed, kept Alice's
advert "not full" and every central refused; members whose `n` is below hers must
connect to her, so they never linked. Reproduction, in the simulated air: (d) the two
connections: no link in 150 s, against 140 ms; (e) no identity: no link in 150 s (49
reconnects), against 120 ms. Fix: a central over the limits closes exactly what the full
flag leaves out: a connection whose peer hasn't proven who it is, after 1 s, and a
second connection to a peer, the first proven staying.

**Status: fixed** by `d202fd3` before NB-110 merged; reviewed 2026-11-05: (d) links in
100 ms and (e) in 4.05 s, and both are tests.

**NB110-10 — A replayed advert held the real one for 60 s (Low).** On main from
`74c7452`, a central held an advert by its `n` alone. A device with no room key copies
Alice's current advert, which anyone in range hears; Bob's `n` is the smaller, so the
copy draws Bob's connect. The device then sends one challenge and nothing more, so Bob
closes it at the 3 s deadline and holds the advert 60 s, or lets the connect fail, also
held 60 s. Meanwhile Bob doesn't connect to the real Alice, and she doesn't connect to
him. Reproduction, probe (f) in the simulated air, the copy heard first, until Alice and
Bob link: 0.19 s with no copy, 5.59 s with a silent one, 63.34 s with one challenge. At
a win rate p over Alice's own advert the expected delay was 63 s × p/(1−p), about 9.5
min at p = 0.9 (**theoretical**). Fix: hold by device and `n`.

**Status: fixed** by `b95141b`, merged `6efda8b`, before `ble` was published; reviewed
2026-11-05. The author's tests run (f) at each place a hold is set: the pair links in
3.34 s, 340 ms and 190 ms. An honest peer changes its address and `n` together, so
nothing changes for it. Residual: NB110-11's.

**NB110-11 — A connect that never completed blocked a node (Low).** On main from
`74c7452`, a connect under way was given up only after 30 s, with no hold, and until
then it blocked its `n` and counted as an unbound connection. At that limit the node
made no other connect and refused every central, as a connect under way is nothing it
can close, and a connection proving the room into the last slot closed the node's own
newest one. On Linux, BlueZ's connect and service discovery are bounded only by bluer's
120 s timeouts, and a peripheral that answers each request just inside the 30 s ATT
timeout keeps discovery going; adverts never expire, so recorded ones serve.
Reproduction, probe (g) in the simulated air, copies heard first whose connects never
complete: (g1) one copy of Alice's advert: Alice and Bob don't link in 150 s; (g2) 4
recorded adverts: neither Alice nor Carol links with Bob; against 0.2 s. Fix: give a
connect up as a failed one, holding that device's advert; count connects under way
against the node's own connects alone; and in the driver, one deadline for the whole
setup and a disconnect on every failure, which also stops a connect BlueZ still has
under way.

**Status: fixed** by `708038f`, merged `51b4e8f`, before `ble` was published; reviewed
2026-11-05. (g1) and (g2) link Alice and Bob in 12.4 s, and Carol and Bob in 0.2 s; the
author's tests are (g1) and (g2) on a radio that stalls. BlueZ 5.87's source confirms
that a `Disconnect` stops a pending `Connect`. A connect that comes up late may put a
node over a limit by at most min(P, K), P its connects under way and K the centrals it
took meanwhile: 1 on a desktop, 2 on an ESP32 (**theoretical**); an extra connection
that doesn't prove the room closes within 3 s.

Residual, **theoretical**: a copy heard first still wins some of Bob's connects, and
each costs that connect the full wait: 10 s, the driver's own deadline, as since NB-112
(`86fee9d`) it reports a failure before its disconnect; it was 12 s before, when the
report waited on the disconnect, 2 s or more past that deadline, and the node's 12 s
give-up came first. At a win rate p the expected wait is `10 s × p/(1−p)`: 1.5 min at
p = 0.9 (1.8 min at 12 s). It needs no fresh address: the hold is by device and `n`, so
the same address with another recorded `n` is drawn again. A copy that sends one
challenge and nothing more costs a race up to 3 s.

**NB110-12 — The BlueZ driver waits on each disconnect (Low).** At `51b4e8f` the
driver's command loop awaits each `Disconnect` (`bluez.rs`). BlueZ 5.87 answers one for
a connected device only once the link is down, after a 2 s timer and the link layer's
terminate, and bluer waits up to 120 s. A connect queued behind it starts late, but the
node's 12 s counts from its own call, so when the delay plus the setup passes 12 s the
connect is given up and that member held 60 s; advertise and scan commands wait in the
same queue. Any device in range can cause it with no room key: it connects to the node's
peripheral and subscribes to `tx`, the node closes it at 3 s, and the loop waits on its
disconnect for D ≈ 2 s + 2 connection intervals, which the device, as the link's
central, picks (up to 4 s): D ≤ 10 s on a stock controller. A connect asked for
meanwhile is given up when D + S > 12 s, S being the honest setup time, and a device
that subscribes again each time the slot frees keeps the loop waiting about D/(D + 3 s)
of the time, 77% at D = 10 s (all **theoretical**). A second effect, Info: the served
central's task may clear the peripheral slot before the disconnect reaches it, which
then finds nothing and leaves the link up, so that device can subscribe again at once.
Fix: spawn the D-Bus `Disconnect`, so the loop never waits on a remote link; have the
served central's task drop the link itself; and send `ConnectFailed` before awaiting the
disconnect.

**Status: fixed** by `86fee9d` (NB-112), before `ble-bluez` was published; reviewed
2026-11-05. `Driver::disconnect` takes the connection out of the driver's books at once
and closes the link in a task of its own, so the command loop awaits no disconnect; the
served central's task drops its link itself once the node closes it, which ends the
second effect; and a connect that fails or passes its 10 s reports `ConnectFailed`
before it disconnects. The author's test covers the books; that the loop never waits
follows from `disconnect` no longer being `async`.

**NB110-T1 — No test pins a connect under way against the connection limit (Info).** A
mutant of `708038f` that leaves connects under way out of the connection term of the
node's check before it connects passes every BLE test (25 unit tests, and the 5 in
`tests/ble.rs`). Fix: a test that, with `connections` − 1 bound connections open, a node
starts one connect, not two.

**Status: fixed** by `86fee9d` (NB-112); reviewed 2026-11-05. In
`connects_under_way_count_against_the_connections_too` a node with 2 of 3 connections
bound starts one connect, not two; read against the mutant, it fails.

**NB112-1 — A burst of service adverts pushed members out of the scan (Low).** NB-112
drops the UUID discovery filter (BlueZ 5.87, below), so the Linux driver's scan hears
every LE advert, and at `86fee9d` it followed at most 256 devices for changes to their
adverts. At that cap a device advertising the service took the place of the oldest
followed, a member when all were of the service, and any device counts as of the service
once it puts the service UUID in its adverts, with no room key. A Linux member without
`Privacy=device` advertises from the adapter's own address, so BlueZ keeps it as one
device, and its new `n` or a repeat reaches the scan only as a change to that device,
heard only while it is followed. Reproduction, by reading (no adapter on the review
host): a board advertises the service from 256 fresh addresses in a few seconds; Alice
and Bob each stop following the other, and BlueZ, which still hears them, never adds
them again, so once their link drops they don't link over Bluetooth again until a scan
restarts or one goes quiet for 30 s, long after the board has gone. Fix: never drop a
device that has advertised the service for another; note one left out, and once a device
of the service could be followed, list BlueZ's devices again, at most every 30 s.

**Status: fixed** by `680e273` before NB-112 merged, so never on main; reviewed
2026-11-05. In the author's test a member followed stays through a burst from 512
addresses, and one left out is listed again at once when there is room, then not again
for 30 s. Once a burst ends a member is followed again within about 60 s: the burst's
devices leave BlueZ 30 s after they were last heard, and a listing runs at once, then
30 s on (**theoretical**). A listing reads each device not followed once, in the scan's
own task, at most every 30 s. Residual: while a burst holds all 256 slots with devices
of the service, a member that arrives is reported once, then not followed, so a retry
after a hold waits for the burst to end; holding them takes about 9 adverts a second
(**theoretical**, 256 addresses / 30 s) from a device that could jam the radio anyway.

**NB110-I1 — On the ESP32 port one UDP source fills every half-open slot (Info).** On
main since the port, over UDP, with no Bluetooth: `ports/esp32` sets
`max_pending_handshakes: 8`, so the cookie mark is 6. Below it an HS1 takes a half-open
entry with no cookie and counts against no source (NB24-1's fix, as the source may be
spoofed); above it each source may hold 4. So one address that knows the node's `PeerId`
fills 6 + 2 = 8, and every new handshake is dropped, even with a good cookie, for 10 s,
again and again; live links are unaffected. It is R1 of
[SEC-4](#sec-4--half-open-handshake-table-exhaustion-medium--nb-024) at a small size:
the default 4096 takes 256 sources. Reproduction, a core unit probe on `25c92d0`: with
8, one source took 8 of 12 entries and an honest dial from another address got no HS2;
with 64, it took 12 and the honest dial got its HS2. Fix: a lower mark for small tables
(a quarter of the cap below 64, so one source holds 2 + 4 = 6 of 8), or a larger cap if
the heap allows.

**Status: open** (Info), reported on NB-110 at `25c92d0`.

**Info, the rest.**
- **NB110-I2 — Pairing on Linux.** BlueZ, not the driver, answers a central's pairing
  request, through the desktop's agent, so a device nearby may raise a pairing prompt,
  or get a bond (a radio check). Documented by `33b1f4f`: nothing in Peerwire pairs or
  needs it, so refuse such a prompt. The ESP32 driver refuses pairing itself (NB-110c).
- **NB110-I3 — A proven connection reset the hello back-off on every carrier.** A member
  reconnecting every 2 s kept the node's Nostr hellos at one per 2 s, not one per 30 s:
  15 times as many (**theoretical**, 30 s / 2 s). **Fixed** by `33b1f4f`: a new
  connection gets a hello of its own, and the other carriers' keep their schedule.
- **NB110-I4 — Connection addresses wrap.** The numbers in `Hop::Ble` wrap at 2^24, so
  one is reused only after 16.7 million more connections. Documented by `33b1f4f`.
- **NB110-I5 — The radio's handshake budget is shared.** A member that reconnects with
  new handshakes takes honest peers' share: one per 5 s on an ESP32. Accepted: Bluetooth
  gives no source to budget by.
- **NB110-I6 — The identity proof isn't deniable.** A member keeps a transferable
  Ed25519 signature showing that a `PeerId` was on a connection with a challenge the
  member chose, where hellos (an HMAC), offers (Noise X) and sessions (Noise IK) are
  deniable. Accepted. If deniability is wanted later: exchange `PeerId`s, then MAC the
  transcript under a DH of the two static keys.
- **NB110-I7 — A relay with no room key can keep a pair cut.** A device that replays a
  member's advert and passes every byte between two connections joins two members, as
  the spec says. Once a session binds those connections, nothing closes them while the
  radio keeps them up, so if the relay then stops passing bytes the pair stays off
  Bluetooth while it is in range: the central doesn't connect to an `n` it is connected
  to, and a second connection to the peer closes. Open; hardening: close a bound
  connection whose peer has had no link for about 30 s, with a short hold.
- **NB110-I8 — A device cycling inside the 1 s grace.** A device with no room key that
  closes its own connection inside the grace and reconnects at once keeps each newcomer
  that lands in its grace refused. In the simulated air, which leaves no gap between
  connections, a desktop pair with such a device on both got no link in 150 s (probe
  a2). Each refusal costs a newcomer 1–4 s, not 60 s, so on a radio the pair links once
  a try lands in a gap between the device's connections; the gap is a radio check.
  Accepted: such a device can jam the radio anyway.
- **NB110-I9 — An idle link on Linux.** A device that connects to a desktop and never
  subscribes holds one of the adapter's links, and the driver can't tell it from the
  host's own devices, so nothing closes it. How many the adapter holds before it stops
  advertising is a radio check. The ESP32 driver closes such a link after 5 s (NB-110c).
  Open.
- **NB110-I10 — Two edges of the unbound limit.** The first, a desktop's own connect
  under way turning centrals away, is fixed by `708038f` with NB110-11. The second
  stays: a central over the limits can close this end's own connection whose peer's
  identity proof runs past the 1 s grace, and that close holds the advert 60 s. Proofs
  take about 0.1 s in the simulated air; the time over a radio is a check. Open.
- **NB112-I1 — Connects left under way at BlueZ.** When the driver left an adapter that
  was still there, it reported its connects under way failed but left them pending in
  BlueZ: harmless, as that adapter was no longer used. **Fixed** by `680e273`: each is
  disconnected there too.
- **NB112-I2 — An adapter its user turned off came back on.** At `86fee9d` the driver
  powered on each adapter it set up on, so one turned off that came back (a USB reset, a
  restart of `bluetoothd`) was powered again. **Fixed** by `680e273`: the driver powers
  an adapter on only at start, and one that is off stays off until its user powers it
  on; BlueZ's `AutoEnable`, on by default, powers on one it finds.

**BlueZ 5.87 (NB-113).** `bluetoothd` from BlueZ 5.87 without upstream commit
`82af2beaf` (on BlueZ's master, in no release yet) crashes when a scan with a UUID
discovery filter is running and an advert or scan response that lists a service UUID
reaches the filter's check: it calls the filter's UUID text as code, so the daemon dies,
with nothing written first, and the host has no Bluetooth until it restarts. BlueZ 5.86
and earlier are unaffected. Until NB-112 the Linux driver's scan set such a filter, for
the service UUID, so on 5.87 a nearby advert or scan response that listed the service
UUID (a peer's scan response does, where its stack has one) crashed it. NB-112
(`bb4857c`) dropped the filter: the driver scans for every LE advert and matches the
service itself (NB112-1). That doesn't make 5.87 safe: any local program that can reach
`bluetoothd` on the system bus can set such a filter, and another app's UUID filter
running beside the node's scan, which has none, can let any advert that lists a UUID
crash it. The fix is BlueZ's: a later release, or a 5.87 build that carries `82af2beaf`.

**Residuals, accepted.**
- **Presence.** Anyone in range sees that a Peerwire peer is near (the service UUID in
  the advert), with an address, `n` and tag that change at least every 15 minutes, and
  whether it is full; with the room's name it can tell a member of that room, as it
  could from a Nostr relay. On Linux the address and the tag change on separate timers
  (NB110-4).
- **Adverts don't expire.** The tag covers `n` alone, with no time, so anyone can record
  an advert and replay it, anywhere and at any time, to draw connects: NB110-10 and
  NB110-11 bound what each costs. The full flag is outside the tag, so a copy can say
  either.
- **Sizes and timing.** Nothing is paired, so the link layer isn't encrypted: a sniffer
  that follows a connection sees the room proof, sealed signals and Noise packets, their
  sizes and timing, not their contents or a lasting identifier.
- **Jamming and cuts.** Anyone in range can jam the radio, cut a connection or stop
  carrying it, as an on-path attacker can on any network, and push a link onto Bluetooth
  by stopping UDP, which it could stop anyway.
- **No proof of distance.** A relay can join two connections (NB110-I7), so nothing may
  treat a peer linked over Bluetooth as near or trusted.
- **One central at a time on Linux.** BlueZ sends a notification to every subscribed
  central (R8), so a desktop serves one: while that one is proven its advert says full,
  and one that doesn't prove makes way after 1 s and is closed at 3 s (NB110-7,
  NB110-I8); a silent second subscriber is NB110-8's radio check.
- **Standbys turn over.** With more members in range than unbound slots, a standby
  closed to make room holds that advert 60 s, then can connect again and close another.
- **`ble-bluez`'s dependencies.** The Linux driver brings about 50 crates (`bluer` and
  D-Bus) and builds `libdbus`, which is C, from source. It is opt-in; feature `ble`
  alone adds no dependency.
- **The ESP32's clock.** Room hellos must be within a minute of the receiver's clock, so
  a board with no Wi-Fi joins over Bluetooth only with a clock set since power-on
  (NB-093).
- **Owed with NB-110c and NB-110d:** the ESP32 driver's pairing refusal (R5), its random
  draws and clock (R15), its 5 s subscribe deadline, NimBLE's connection cap and its 10
  s connect deadline; a phone at an ATT MTU of 23 (R14).
- **Radio checks owed on our own hardware:** the honest connect time (S above),
  NB110-8's subscribers, the gap between a device's reconnects (NB110-I8), the idle
  links an adapter holds (NB110-I9), the time from connect to a proven identity
  (NB110-I10), and the driver's recovery from a refused advert and a USB reset (NB-112).

### `@peerwire/react` hooks (NB-049) — RH-1…RH-6, NB53-1

`@peerwire/react` (`packages/react`, 0.1.0 published from `dec1554`) wraps a room in
React hooks. Its store (`store.ts`, one per room) keeps shared keys (one value per key
for the whole room, last writer wins, replayed to each member that joins) and per-peer
values (each member's latest value per key, such as a cursor). Both travel as room
messages with a `$peerwire` field. Reviewed 2026-10-24 at `d369c8d` and re-checked at
`8126119` by reading the code, with Node probes on fake rooms, the real hooks on jsdom,
and two loopback rooms.

**Trust statement.** Every member is trusted alike to write, as in the room itself: any
member can overwrite any shared key, write well-typed nonsense, or show members different
values by sending a write to some of them only (`to`). What the store defends is each
member's app and resources. A member must not be able to crash another's app, make its
own writes fail, use up the caps for everyone, or make catch-up costly. Shared values
outlive their writer and every member replays them to each joiner, so a bad value there
repeats on every join, where a bad chat message (RX-1) happens once.

What holds:
- **Message checks.** Keys are at most 256 characters and values at most 64 KiB as JSON
  (`MAX_VALUE_LENGTH`). Clocks and sequence numbers must be safe integers of at least 1.
  A writer named in a catch-up must be a PeerId. Catch-up arrays hold at most 1024
  entries. State lives in `Map`s, so keys such as `__proto__` are harmless, and the member
  list and per-peer views the hooks return are frozen.
- **Attribution.** Per-peer values, also inside a catch-up, are charged to the peer that
  sent the message, never to a claimed id. A shared value's writer `w` can be claimed in
  a catch-up, on purpose: that's how a joiner gets a value whose writer has left. A
  claimed writer matters only in a tie, and a member can win any key outright with a
  later clock. So `w` must never be shown to users as "who wrote it" (nothing does today).
- **Checks.** An `accept` check that throws counts as failed. A shared value that fails
  the check reads as `initial`, the same on every member.
- **Internal messages.** `useMessages` and `useRoomEvent` hide `$peerwire` messages. Code
  that listens to the room directly sees them.

**RH-2 — The default shared-state check was one level deep (Medium).** Without `accept`,
`useSharedState(key, initial)` checked only that a value had the same top-level kind as
`initial`, and a `null` initial took anything. A member wrote `[{"text":{"x":1}}]` to an
array key. Every app that rendered `t.text` threw while rendering; with no error
boundary the app unmounted and its cleanup left the room. The value persisted, so each
joiner got it in catch-up and crashed the same way, until the key was written again or
every member holding it had left. Reproduced with the real hooks on jsdom. **Fixed** by
`78c4ebe` and `8126119`: without `accept`, `initial` must be a string, number or boolean
(a TypeScript overload, and a `TypeError` at render otherwise). Arrays, objects and
`null` need `accept`. Member data in `useMessages`, `usePeerState` and `usePeers` is
typed `unknown` unless checked, so TypeScript flags rendering an unchecked value. The
README tells apps to check every field they render, however deep, and to put an error
boundary inside `RoomProvider`; the example has one. `test/hooks.mjs` covers the nested
case. What is left is the app's to get right: an `accept` shallower than what the app
renders. An error boundary catches errors in render and effects, not in event handlers.

**RH-1 — One far-future clock froze every shared key (Low).** The store kept one clock
for all keys and raised it to any member's clock. One write with a clock of
`Number.MAX_SAFE_INTEGER`, to any key, made every later `setShared` throw "clock
exhausted", for every key, on every member. Catch-up carried it to joiners, so it
outlived the writer, and a setter called in an effect unmounted the app. **Fixed** by
`78c4ebe` before the first publish, because the clock is part of the messages between
versions. Each key has its own hybrid clock: a write is stamped
`max(Date.now(), current + 1)`, and a member's clock is taken only up to `Date.now()` +
`MAX_CLOCK_DRIFT` (5 min), in writes and in catch-up versions alike. Nothing throws for a
clock any more. A bound relative to our own last clock was rejected: a joiner starts at
0, and members would stop agreeing. Residual:
- **Clock skew near the bound (Info, accepted).** When honest members' clocks differ by
  δ, a write stamped within δ of `now + MAX_CLOCK_DRIFT` is taken by the members whose
  clocks are ahead and ignored by those behind. No write is sent twice, so they disagree
  on that key until a write that every member takes beats that stamp. Writes from the
  members that took it qualify about δ later; writes from the others only once their own
  clocks pass the stamp, up to `MAX_CLOCK_DRIFT` + δ later. A member can do this on
  purpose by stamping near the bound, which gives it no more than a write sent to some
  members only. A member whose clock runs more than `MAX_CLOCK_DRIFT` ahead has all its
  shared writes ignored (the README says so).

**RH-3 — All members shared the caps, first come, first served (Low).** One 82 KB message
with 1024 junk keys meant no member kept any member's new shared key, even after the
writer left. One member could take every per-peer slot. A local write past the cap was
kept and sent, but every other member dropped it, so the writer silently disagreed with
the room. **Fixed** by `78c4ebe`. Per-peer values have a quota per member, ours included:
`MAX_PEER_KEYS` (64) and `MAX_PEER_LENGTH` (256 KiB), charged to the sender and freed
when it leaves. Shared state holds 1024 keys and `MAX_STORE_LENGTH` (1 MiB) in all. When
it is full, a key a mounted hook reads, or our own write, pushes out the oldest keys no
hook reads; a local write that still doesn't fit is dropped with a warning (RH-5).
Accepted: what is pushed out is local, so members can hold different unread keys; a
joiner gets the newest version any member still holds. A hook that mounts later for a
key that was pushed out reads `initial` until the key is written again, while others hold
a value (a route change or a virtualized list can do this). A member can force it by
filling the unread space, which is no more than overwriting the key, as it can anyway.
"Oldest" is insertion order, so long-held app data goes before fresh junk.

**RH-4 — Each join cost every member its whole state twice (Low).** On a join both sides
sent everything and asked, and the answer to an ask sent everything again: 100 KB of
shared state cost 216 KB to one joiner (2.16×). At the old 16 MiB cap that was up to
32 MiB per member per join, more with repeated asks, and minutes of the pipes' shared
budget. **Fixed** by `78c4ebe` (a change to the catch-up messages, before the first
publish): catch-up by digest. A join sends the versions held (`[key, clock, writer]`)
and asks. The answer carries only the values the asker's digest lacks, plus our
per-peer values, and never one already sent to that member. It asks back when the asker
holds a version we lack or hasn't answered us yet, which covers a first message lost to
the room's welcome race (on `@peerwire/js` 0.1.1). A store answers at most 4 asks per
member while it stays, and keeps at most 1024 of the versions a member told it. Per
membership, a member can now draw from us at most 1 MiB of values once, 4 digests (up to
1024 versions, about 350 KB each) and our per-peer values (256 KiB) once: about 2.5 MB,
where it was 4 × 16 MiB. A 100 KB store now costs 1.18× per joiner. Every member still
sends its state to each joiner, which is slow over a relayed link or the pipe; the
README says to keep shared state small.

**Fixed in 0.1.1.** `@peerwire/react` 0.1.0 was published with these four. `12974c4`
(merged `41cc24a`, `@peerwire/react` 0.1.1) fixes all of them, re-checked 2026-10-26 by
reading the code, with Node probes on `dist/store.js` built from that commit. None
changed the messages:
- **RH-5 (Low): member data could still make our own shared write throw.** In an app
  whose hooks read more than 16 shared keys (`MAX_STORE_LENGTH / MAX_VALUE_LENGTH`),
  members can fill the keys it reads with 64 KiB values. Then our write to a new key, or
  one that grows a value, has nothing it may push out, and in 0.1.0 `setShared` throws
  `RangeError`. Called from an effect, that shows the error boundary's fallback, or
  unmounts the app without one. From an event handler it is an uncaught error and the
  write is lost. It is RH-1's kind (member data causing a throw in app code), narrower.
  **Fixed**: a write that doesn't fit is dropped with a `console.warn`, as every member
  drops a member's write that doesn't fit, and `setShared` returns `false`. The store
  works out what it would push out before it pushes anything, so a dropped write sends
  nothing and changes nothing. It still throws, before any change, for the app's own
  mistakes: a value over 64 KiB as JSON or a key over 256 characters. An app on 0.1.0
  that reads many shared keys can catch the `RangeError` around its setter calls, or
  upgrade. Residual (Info): the warning comes once per dropped call, so an app that
  writes on every frame to a store members have filled logs on every frame; warning once
  per key is planned for 0.1.2.
- **Catch-up message size (Low, via OUT-1).** In 0.1.0 a catch-up message holds at most
  64 values but has no byte cap, so near a full store one can pass the reliable lane's
  1 MiB (non-ASCII text takes up to 3 bytes per character). The store splits a message
  when the room throws `RangeError`, but the room doesn't throw when that peer's outbox
  holds frames (OUT-1, below): it drops the message silently, and the store has marked
  those values as sent. The joiner then lacks them from us until they are written again;
  another member's answer may still bring them. **Fixed**: each catch-up message, digests
  included, is cut at 512 KiB (`CHUNK_BYTES`), counting 3 bytes per UTF-16 code unit of
  each entry as JSON. UTF-8 takes at most that, since `JSON.stringify` escapes lone
  surrogates, so a message stays under 512 KiB plus its envelope, and one entry alone (a
  64 KiB value under a 256-character key) under about 200 KiB. A probe with worst-case
  `€` keys and values, measured as the room frames it, found 394,963 bytes at most,
  against the 1,048,576 limit. The split on `RangeError` stays as a fallback. OUT-1
  itself is fixed in `room.js` by `324cb8f` (merged `8271f1f`), so `setLive`'s move to
  the reliable lane gets the throw too.
- **RH-6 (Info): catch-up records kept replaced values alive.** For each member the store
  kept the entries it last sent it (`sentShared`, `sentLive`). When a key was written
  again, the old value stayed in memory while that member stayed: at most one old value
  per key per member, up to about 1 MiB of shared and 256 KiB of per-peer values for each
  member, beyond the store itself, if keys kept being rewritten between joins. **Fixed**:
  the records hold each entry's version (clock and writer, or sequence number) instead,
  which compares the same, because a write never replaces an entry with an equal version.
  A key pushed out of the store is removed from every member's record, as before.
- **N1 (nit, cost only):** an asker whose digest already held a newer version of a key
  still got our older value, which it ignored, because `tell()` recorded the asker's
  version only when it wasn't newer than ours. **Fixed**: `tell()` records it either way,
  so the answer carries only that key's version. It still asks back only for a digest
  version we lack, and still keeps at most 1024 told versions per member.

**NB53-1 — A member's value nested deeply could throw out of the store (Low).** Found in
the review of NB-053 (DOC-7), with Node probes on the store's build. `merge` and
`mergeLive` measured a member's shared or per-peer value with `JSON.stringify`, and
nothing checked its depth, so a member could send one about 500000 deep (DOC-7). On Node
22.23 (measured):
- a `set`, `live` or `sync` holding a value 100000 deep (about 200 KB as JSON, under the
  reliable lane's 1 MiB) threw `RangeError` out of the store's message listener.
  `room.js` catches a listener's throw and throws it again from a `setTimeout`, so it is
  an uncaught exception: a Node process ends, and a browser logs it;
- in a `sync`, the entries after it and the answer to its ask were lost;
- on Node 26.7, whose `JSON.stringify` doesn't recurse, a value 6000 deep (about 12 KB)
  was kept and sent on to each joiner in catch-up. A joiner on Node 22 then threw the
  same way, so the value outlived its sender.

Low: it needs a member, and of the engines measured only Node 22's `JSON.stringify`
throws (Chromium 153 doesn't; Firefox and Safari weren't measured). Where it throws, any
member of a room can end a Node app in it.

**Status: fixed** by `0af9af3` and `5edfff0` (NB-054), merged `fd9d5fd`, reviewed
2026-10-30, by reading the code and re-running the probes on Node 22 and 26 against a
build of `0af9af3`: nothing threw, the entry after the deep one was kept, and the value
6000 deep was neither kept nor sent on.
- `shallow()` checks a member's value with `doc.js`'s depth rule (the top level is 1; 64
  deep is kept, 65 dropped), in a loop, before anything stringifies it. A value past it
  is dropped like any value that is off.
- Our own value past 64 throws `RangeError` from `setShared` and `setLive`, since
  members would drop it. `MAX_VALUE_DEPTH` is exported.
- Each entry of a catch-up is merged on its own, so one that throws costs none of the
  others. A listener's error is thrown again once the answer is sent.
- `store.mjs` stubs `JSON.stringify` to throw past 4000 levels, so its checks bite on
  Node 26 too. With `5edfff0` it also sends the deep value down a middle branch of an
  array and of an object, and has a per-peer listener throw in a catch-up (NB54-T1, a
  test gap: three mutants of the walk and of the catch-up passed without them).
- `5edfff0` also notes in `index.d.ts` that `peer-join`'s presence and `getPresence()`
  are member JSON, which can nest deeply (NB54-I2).

Residuals (accepted):
- **Old versions.** Apps on `@peerwire/react` 0.1.1 or earlier, and on the
  `@peerwire/js` 0.2.0 or earlier they run on, stay exposed until they upgrade: to
  NB53-1 on Node 22, and to NB53-I2 without its note. The first release of each after
  `fd9d5fd` should say so in its notes. `room.doc()` hasn't been published, so no
  release has DOC-7.
- **NB53-I2 (Info).** Room `message` and `presence` values reach the app as parsed, up
  to about 500000 deep (DOC-7). The js README and `index.d.ts` say so since `0af9af3`:
  an app that stringifies member data, or walks it by recursion, must bound its depth
  first.

**NB54-I1 — The depth checks walked with `for…in` (Info).** Found in the review of
NB-054. `shallow()` and `doc.js`'s `freezeJson` walked each array and object with
`for…in`:
- it makes a string of every array index. Timing each walk as it was and as it is, on a
  freshly parsed 1 MiB `[0,0,…]` (Node 22.23, median of 7): `freezeJson` took 60.5 ms
  and `shallow()` 55.4 ms, against 9.9 ms to parse it. A member's value costs that
  before the 64 KiB cap drops it;
- it walks the enumerable keys an object inherits, which `JSON.stringify` skips. On a
  page whose own script put an enumerable object on `Object.prototype`, every object
  nested past 64 through it. The store dropped every member's object or array value,
  the doc every frame holding one, our own `setShared({})` and `map(…).set(k, {})`
  threw, and `freezeJson` froze the page's object. A key under a number on
  `Array.prototype` or `Object.prototype`, holding a value nested past 64, made `[]` or
  `{"a":0}` drop.

Info: the cost is linear and needs a member. The keys need the page's own script, since
a member's JSON can't put one on a prototype (`"__proto__"` parses as an own key). And
neither walk had been published.

**Status: fixed** by `d6e66f3` (NB-056), merged `e43391c`, reviewed 2026-10-30. Both
walks go over arrays by index and objects by `Object.keys`, as `JSON.stringify` does, so
they see what will be serialized; the depth rule is unchanged. The same 1 MiB array now
takes 5.5 ms in `freezeJson` and 0.5 ms in `shallow()`. An array of empty arrays or
objects costs 30 to 80% less than it did, and a wide object the same. A `null` inside
is passed over as before; that check is now load-bearing (`Object.keys(null)` throws),
and the tests hold nested `null`s. `store.mjs` and `doc_crdt.mjs` put an object on
`Object.prototype` for a run, and check that a 64-deep middle branch is kept. A probe of
both walks found an own `"__proto__"` key 65 deep still dropped.

### Bounds and residuals from reviews (NB-014…NB-043)

This section records what each earlier review established about resource bounds,
and what it accepted. Values are from main `7b716a2`, with the receive window and the
congestion rule for tail losses from NB-086 (`b2ae061`, reviewed 2026-11-03).

**Core sessions**
- **Receive memory per link.**
  - Reliable lane: `RECV_WINDOW` 4096 fragments of `FRAGMENT` 1100 B (1024 before
    NB-086). The sender keeps the span of reliable fragments in flight under it, as it
    does the urgent span, so none is sent only to be dropped.
  - Urgent lane: `URGENT_RECV_WINDOW` 64 fragments, with messages capped at
    `MAX_URGENT` (64 KiB).
  - Fragments longer than `FRAGMENT`, and reassembly past `max_message` (1 MiB by
    default), are refused.
  - The bound tested in `fuzz.rs` (`hostile_peer_pins_bounded_receive_memory`) is
    5,687,912 B per link: 4095 · 1100 B + 1 MiB reliable (5.55 MB) and 63 · 1100 B +
    64 KiB urgent (0.13 MB). Before NB-086 it was about 2.31 MB.
    `docs/guides/always-on-peer.md` sizes `PEERWIRE_MAX_PEERS` from it: about 6.1 MB
    allocated per link, so 256 peers can pin about 1.6 GB, and the compose file's
    128 MB is the worst case for about 19.
  - **NB86-I1 (Info, accepted):** the window is the receiver's, so this bound grew
    2.46× on every node, whatever its `reliable_window`. A peer still sends every
    byte it pins, over its own authenticated links only; size `max_links` to the
    memory there is.
- **Retransmission.**
  - RTO is 20 ms to 5 s; backoff `rto·2^min(tries-1, 8)`, at most 10 s.
  - After `MAX_TRIES` (16) the link is dead.
  - ACKs report a 64-fragment SACK bitmap. Since PERF-10, a fragment beyond the
    bitmap's reach doesn't time out while the hole below it is repaired.
- **Congestion control.**
  - A loss halves the window (`LOSS_CUT`).
  - Queueing delay above 25 ms cuts it to 0.85 (`DELAY_CUT`).
  - A loss with under 5 ms of queueing cuts it to 0.8 (`RANDOM_LOSS_CUT`, Veno);
    PERF-10 deepens that to 0.5 on a timeout in the same round trip.
  - A tail loss, a first send whose timer runs out after an ACK acknowledged new
    data, is cut as fast retransmit cuts, not as a timeout (NB-086, as RACK-TLP
    does); a resend that times out is still a timeout, with back-off.
  - The minimum window is `MIN_CWND` (4).
  - Congestion state is per link, so a peer that games it affects only its own link.
- **Links.**
  - `max_links` is unlimited by default, and the `peerwire-peer` server sets it to
    256.
  - Half-open handshakes don't count towards `max_links`.
  - An HS1 is refused before the DH when half-open handshakes reach
    `max_pending_handshakes` (4096) or links reach `max_links`.
  - An unannounced responder past the cap is dropped at its first data packet.
  - From three quarters of `max_pending_handshakes` (3072), an HS1 from a UDP
    address takes no state until it echoes a cookie, and each source holds at
    most 4 half-open handshakes (SEC-4, NB-024, since `208659b`).
- **Relay.**
  - 256 KiB/s with a 64 KiB burst per peer, and a 256-packet queue.
  - The total (`relay_total_rate`) is unlimited by default, and `peerwire-peer`
    sets it to 8 MiB/s.

**JS endpoint and rooms**
- **Lifetime.**
  - `leave()` sends `Bye` to each linked peer on the unreliable lane, then
    disconnects them.
  - On `pagehide` it waits at most 20 ms for that.
  - A received `Bye` is inside the Noise session, so it only ever drops the
    sender's own link. Rooms accept it before the room proof, which is harmless for
    the same reason.
- **Sends.** `sendRetry` keeps at most one message above the 16 KiB scratch buffer
  between tries. The room's outbox has no cap. It holds only what the local
  application sends, so that is application policy.
  - **OUT-1 (Low; `@peerwire/js` 0.2.0 and earlier):** `room.send()` throws
    `RangeError` for a message too large for its lane only when that peer's outbox is
    empty. When it holds frames (more than the 4 MiB send buffer unacknowledged to that
    peer: a slow or relayed link, or a burst), `write()` queues the frame unchecked and
    `flush()` later drops it as too large. The caller sees no error, the peer never gets
    the message, and later messages still arrive. Found in the `@peerwire/react` review:
    its catch-up split relied on the throw in 0.1.0 (capped by size in 0.1.1, so it no
    longer does), and `setLive`'s move from the unreliable to the reliable lane still
    does. Reproduced with two loopback rooms: a 1 MiB + 1 KiB send threw with an empty
    outbox, and with 9 frames queued it didn't throw and never arrived. Fix: in
    `write()`, check the frame against the lane's limit before queueing and throw the
    same `RangeError`; for a broadcast, check once before writing to any member, so a
    throw doesn't leave the message sent to some members only. **Fixed** by `324cb8f`
    (merged `8271f1f`), reviewed 2026-10-26: `write()` checks the frame against
    `MAX_FRAME` (reliable 1 MiB, unreliable 1100 B, urgent 64 KiB, the core's own
    limits) before it queues, so the throw comes whatever the outbox holds. A broadcast
    throws at its first member, before writing to any.
  - **OUT-1a (Low):** a numeric lane other than 0, 1 or 2 skipped that check (the core
    sends it unreliable), so it still dropped silently. **Fixed** by `131f9f5` (merged
    `8271f1f`): any other lane throws `TypeError`.
  - **OUT-1b (Low, older):** `setPresence()` and `joinRoom({ presence })` stored a
    presence before checking its size. An oversized one then threw each time a member
    joined later, so held frames and presence were lost both ways. **Fixed** by
    `131f9f5`: the presence frame is built and checked first, and stored only if it
    fits.
  - **OUT-1c (Low, older):** a presence that JSON can't encode (`undefined`, a function,
    a `Symbol`) went out as an empty body, which members dropped, so members who joined
    later never listed us. **Fixed** by `f5c1f45` (merged `f28f027`): it throws
    `TypeError`.
  - **OUT-1d (Info, React):** with OUT-1b and OUT-1c, a presence changed during
    `usePeerwireRoom`'s join to one the room refuses threw inside the join's handler.
    The hook stayed `joining` with no room, the room stayed joined, and the rejection
    went unhandled. **Fixed** by `d29d200` (merged `7bf37e4`): the hook leaves the room
    and reports the error.
- **Signals.**
  - Malformed or unproven signals are counted in `badSignals` (a counter only, with
    no action).
  - An `ice` or `ice-batch` message holds at most 32 candidates, and at most 32 are
    held before the description.
  - Residuals:
    - No total cap on remote candidates per link but the predicted ones, at most
      128 since `0a8a796` (NB46-I1).
    - No cap on signals held while an offer's proof is checked (one async check, so
      the window is short).
- **Fallback pipes (NB-043).**
  - `MAX_PIPE_PEERS` 256, evicted oldest first. Since `a283a01` an evicted permission
    drops its pipe unless a session uses it (NB43-1).
  - Outgoing pipes share 6 batches/s (burst 12), in batches of 16 KiB, each through
    one relay, the relays taking turns. Before NB-006i it was 3/s (burst 6), each batch
    through every relay.
  - Each pipe's queue is 128 KiB (64 KiB before NB-006i), dropping the oldest.
  - At close, each pipe's queue goes as one last batch past the budget, so a peer sees
    our `Bye` at once instead of timing out (`414d69f`, merged `3ce0cba`). A closed
    Nostr transport stops reading at once and closes its sockets after the publishes
    already handed to it.
    - **PBF-1 (Info):** that close burst was one batch per pipe, so up to 256 batches
      of 16 KiB with pipes made by holders of the room key (NB43-I2). **Fixed** by
      `344e90e` (merged `fc94833`): at most `PIPE_CLOSE_BATCHES` (16), from the pipes
      that queued last; the rest time out at the peer as before.
    - **PBF-2 (Info, test gap):** no test checked that a closed Nostr transport reads
      nothing. **Fixed** by `344e90e` (`room_leave.mjs`).
    - Residual: Nostr publishes are hashed asynchronously, so on `pagehide` the last
      batch never goes out, and a tab closed over a Nostr pipe still times out at the
      peer (30 s).
  - Incoming batches are capped at 64 items but not rate-limited.
  - Residuals:
    - **NB43-I1:** a relaying member can roam a link onto its own pipe, because a
      direct-to-direct roam needs no path validation. Since Fast lane 2 this reached
      pipe links too (FL2-2). **Fixed in JS** by `63e5b72` and `ddae245`: each datagram
      names the peer whose channel it came on, and a packet for another peer is dropped
      before any roam. Narrowed on native UDP, where one socket serves every peer, by
      Fast lane 3 (`0a84b7d`, `475c442`): probes move no link, and a roam needs a packet
      newer than the newest over the link's path, also right after a punch. What is
      left: a party that holds back the peer's newest packet and sends it from its own
      address moves the link there until the peer's next packet moves it back.
      [Latency races](#latency-races-fast-lane-3--fl3-1fl3-3).
    - **NB43-I2:** an insider holding the room key can churn the pipe permissions,
      take the shared budget, and send 64 HS1s per batch. Card NB-043b; it supports
      SEC-4.
    - **NB43-I3:** the Nostr backoff never decayed; fixed in `a283a01`.
    - **NB43-I4:** relays see session-long timing and volume of piped links, and
      every direct member learns that a pair failed WebRTC.

**Signalling**
- **JS Nostr.**
  - Content is capped at 64 KiB.
  - 4096 event ids are remembered (oldest forgotten), and each must be 64 hex
    characters.
  - Each relay is paced on its own, at 4 events a second with a burst of 8 (NB-006h).
    One that refuses an event for its rate halves that rate (to no less than 0.5 a
    second) and is held for 1 s, doubling to 30 s. One that bans us is sent nothing for
    30 s, doubling to 10 min, and that starts over after 10 quiet minutes
    ([Nostr relay pacing](#nostr-relay-pacing-and-pipe-striping-nb-006h-nb-006i--nb6h-1-nb6h-2-nb6i-1)).
  - Closed sockets are redialled from 1 s, doubling to 30 s, starting over only after a
    socket open 30 s (NB-058).
  - Residual: JS checks neither the event id hash nor the signature, so a relay can
    present one event under many ids. Offers are sealed and hellos proven, and
    replays fall under SEC-6, so the cost is duplicate work. Rust checks the id.
- **Rust Nostr.**
  - Websocket messages and frames are capped at 128 KiB.
  - The inbox holds 1024 events, then drops.
  - 4096 event ids are remembered.
  - At most 64 publishes are queued while a relay is down.

**Addresses we give out**
- **NB-041.** Offers to a relay peer name no links. Residual: under `Direct`, a relay
  peer sees our LAN addresses; `PublicOnly` hides them.
- **NB-042.**
  - **NB42-I1 and NB42-I2:** mapped `::ffff:` addresses compare as IPv4, and NAT64
    `64:ff9b::/96` is judged by its IPv4; fixed in NB-042b (`1529b54`).
  - **NB42-I3:** Godot always binds `[::]`, so a host with global IPv6 and no inbound
    firewall is reachable unsolicited, so it meets handshake floods directly
    (SEC-4, fixed by NB-024; R1 remains).
  - **Residual NB42b-I1:** `is_public` doesn't unwrap NAT64, so a NAT64 address
    embedding a private IPv4 still gets a guess window.
  - **Residual NB42b-I2:** the local-use prefix `64:ff9b:1::/48` isn't covered.
- **`73aa866` (private interface addresses).** A socket bound to any interface also
  offers up to 3 RFC 1918 interface addresses. Container and VM bridges are skipped
  by name, but VPN tunnels (`wg*`, `tun*`, `utun*`, `ppp*`) are not. So under
  `Direct` a peer can learn, for example, a corporate VPN address. Suggested: skip
  those interface names too.
- **MIC-3 (Low, privacy): `public-only` in `@peerwire/js` 0.1.1 and earlier gave out
  local addresses.**
  - **What:** once a browser shows a page's local addresses (the page has microphone
    or camera permission), a server-reflexive candidate's related address (`raddr`,
    `rport`) is the local socket it was sent from. So each interface with a STUN
    mapping gave away its local address, in the description and in trickled and late
    candidates. When no STUN answer came before the description was taken (on Nostr,
    within `GATHER_MAX_MS`, 2 s), gathering had also written a host address into `c=`,
    the m-line port and `a=rtcp:`.
  - **Who:** `joinRoom({ privacy: 'public-only' })`, and `WebRtcTransport` under
    `'public-only'` or `'relay-only'`. An app with camera or microphone permission
    (voice, video, meetings) that chose `public-only` gave every peer that got its
    description (in a room, every member) its LAN, VPN tunnel and container bridge
    addresses.
  - **Fixed** by `6303bd8`: `withoutHostCandidates` and the transport's sent
    candidates clear the related address to `0.0.0.0 0`, and `c=`, the m-line port and
    `a=rtcp:` go back to `0.0.0.0` and `9`, as before gathering. ICE reads none of
    these. The clearing doesn't rely on the runtime hiding anything, so it also holds
    in runtimes that never hide local addresses (we haven't checked which ones fill
    `raddr` without a permission). Suggested: the next npm release notes tell apps on
    0.1.1 that use `public-only` to upgrade.
  - **Not covered:** `'direct'` gives out every local address the runtime shows. ICE
    checks leave from local sockets, so a peer on the same network can still see a
    local address as a peer-reflexive candidate. Since `9f98608` and `dc85a1a`, the JS
    docs say what a microphone or camera grant does to `'direct'` and `'public-only'`:
    the grant is stored per origin, so it also shows the addresses on later visits and
    on pages that never ask, and `'public-only'` still sends the public address of the
    network under a VPN.
- **MIC-5 (Info):** `withoutHostCandidates` splits on `\r\n` only, so an SDP with bare
  `\n` line ends comes out unchanged, host candidates and all. Browsers write `\r\n`;
  a custom `RTCPeerConnection` (Node, native) might not. Suggested: split on `\n` as
  `stripCandidates` does (NB48b-3), and match `typ host` before any `\r`. **Fixed** by
  `1385715` (merged `82be3bd`), reviewed 2026-10-26: `withoutHostCandidates` splits on
  `\n` as `stripCandidates` does, and a rejected m-section keeps port 0 in both.
  `privacy.mjs` runs the gathered-SDP case with both line ends.
- **IPv6 in browsers (NB-079; field test 2026-11-03, docs `9ea2200`).** IPv6 has no
  NAT, so the public address a browser gives out over IPv6 is the device's own, not
  one a carrier NAT shares among many subscribers. In the field test two Android
  Chrome phones on Jio mobile data, one page without microphone permission, linked
  directly IPv6 host to IPv6 host
  ([IPv6](../concepts/nat-traversal.md#ipv6-dual-stack)). What a member learns of a
  browser peer on IPv6 (in a room, each member it links with; in Net Lab's global
  room, anyone who opens the page):
  - **`'direct'` without a grant:** the address of the default route. Android Chrome
    lists it unhidden as a `host` candidate, and STUN over IPv6 repeats it as a
    `srflx` one (the page without permission sent one each of `host` IPv4, `host`
    IPv6, `srflx` IPv4 and `srflx` IPv6, and no mDNS name; its `host` IPv4 was
    `192.0.0.4`, the 464XLAT address every such phone has). A browser that hides
    `host` candidates behind mDNS names sends the address all the same, as `srflx`:
    libwebrtc keeps a `srflx` candidate equal to a `host` one it hides (not tested
    here, as no test host has global IPv6).
  - **`'direct'` with a grant:** every interface's IPv6 address as a `host`
    candidate (mobile data, Wi-Fi, a VPN tunnel, ULAs), as for IPv4 (MIC-1).
  - **`'public-only'` without a grant:** the same address. `withoutHostCandidates`
    and `candidateFor` drop `typ host` lines only, and the `srflx` one is that
    address. So on IPv6, `'public-only'` keeps from members only the 464XLAT
    address, the same on every such phone.
  - **`'public-only'` with a grant:** the browser may send no IPv6 `srflx`, as it
    doesn't repeat a `host` candidate it no longer hides, and then the description
    names no IPv6 address. But our ICE checks to a member's IPv6 candidates leave
    from that address, so a member that offers one learns it as a peer-reflexive
    candidate (MIC-3's "Not covered" case, which on IPv6 isn't limited to the same
    network), and the direct IPv6 path may stay. Not field-tested.
  - **`'relay-only'`:** the relay peer only, as under `'public-only'`.
  - Calls send their media candidates through the same filters, so call members
    learn the same. A Rust node's `PublicOnly` offers its global IPv6 address too;
    `RelayOnly` offers none.
  - **What the address tells.** The carrier and a rough place, as a public IPv4
    address does, and more: on mobile data the /64 is the phone's own (3GPP gives
    each data connection its own /64, RFC 6459), so a member can tell the same
    phone across rooms and rejoins for as long as that connection lasts. Temporary
    addresses (RFC 8981) change only the low 64 bits, by default about once a day;
    without them those bits can stay the same across networks. And unless the
    carrier filters inbound IPv6, the address reaches the phone unasked, which a
    carrier-NAT IPv4 address doesn't.
  - **NB79-I1 (Info, privacy, open): on IPv6, `'public-only'` doesn't keep the
    device's own address from members.** The docs say so since `9ea2200` (NAT
    traversal, the JavaScript reference, the privacy guide). Dropping IPv6 `srflx`
    candidates under `'public-only'` would keep it only from members that offer no
    IPv6 candidate themselves: one that does learns it from our checks, as above,
    and any member on IPv6 under `'direct'`, the default, does. Keeping it from
    members takes both halves: send no IPv6 candidate and add none of a member's,
    so our ICE never sends from IPv6. That loses the direct IPv6 path, on mobile
    data often the only direct one (NB-079). Suggested: keep `'public-only'` as it
    is, since what it sends is what STUN sees and the docs now say what that is on
    IPv6; if apps need the address kept from members, add the two-sided filter as
    its own choice, and consider it for Net Lab's global room.

**Demos**
- No CSP is sent by `crew-server.mjs`, which serves Net Lab and Crew. Only the Docker
  examples' nginx config sets one (`script-src 'self' 'wasm-unsafe-eval'`,
  `connect-src 'self' wss:`, `frame-ancestors 'none'`). Net Lab escapes everything a
  peer sends before it reaches `innerHTML` (checked for NB-047 and NB-048), so a CSP
  is defence in depth. Suggested: send the same CSP from `crew-server.mjs`.
- Net Lab caps its event log at 500, the DOM log at 300 and chat at 200. The
  per-peer route history has no cap.
- **README examples (`9e4640b`, reviewed 2026-10-24).** "Using with React" in
  `packages/js/README.md` (since NB-066 [Your own hook](../guides/react.md#your-own-hook))
  joins in an effect and leaves in its cleanup. A room `message` is whatever the member
  chose to send: text, bytes or any JSON.
  - **RX-1 (Low):** the example's hook stored every message and rendered `{m.data}`. A
    member who sent an object made React throw while rendering, which unmounted the
    app, and the cleanup then left the room. So one broadcast object took every other
    member running the example out of the room. Reproduced on loopback (Vite dev server,
    BroadcastChannel). **Fixed** by `b0d354b`: the hook keeps string messages only, and
    a bullet tells apps to check a message's type before they store or render it.
  - **RX-2 (Info):** the message list had no cap, so a member flooding the room grew it
    without bound. **Fixed** by `b0d354b`: the hook keeps the last 100.
  - The ten-line chat writes messages with `textContent`, so an object only shows as
    text.
- **Chat demo (`demo/chat.html`, NB-007, merged `92dfa42`, reviewed 2026-10-27).** A
  room per invite link or QR code. The room key comes from the room's name, so anyone
  who can guess the name can join and read along.
  - **CHAT-1 (Low, privacy):** the demo took any name and gave one like `#family` the
    same "only those with the link" hint as a random one. Such a room is a public
    lobby. **Fixed** by `dfa8ea0`: a name that isn't at least 22 characters of
    base64url is flagged as guessable in the invite hint, the header and the chat.
  - **CHAT-2 (Low):** the QR code throws past its largest size (2331 bytes), and the
    page called it with no catch while joining, so a long room name (or the whole query
    copied into the link) left the page silently never joining, with the composer still
    on. **Fixed** by `dfa8ea0`: a room name is capped at 256 bytes percent-encoded, with
    a message when a link's is refused; the invite carries only `?signal=local` from the
    query; a QR code that can't be drawn is left out; and anything joining throws shows
    as an error with the composer off.
  - **CHAT-3 (Info):** the welcome said messages go straight from peer to peer with no
    server. **Fixed** by `dfa8ea0`: it says messages may pass through another member or
    the signalling relays, end-to-end encrypted either way.
- **Microphone permission (Net Lab's "Use microphone" button, `e450baf`).** Chrome
  hides a page's local addresses (mDNS names, the default route only) until the
  origin has microphone or camera permission. After that, every new connection lists
  every interface (RFC 8828 mode 1). The button asks only for that permission and
  stops the tracks at once: nothing keeps the stream or uses the audio (checked in
  Chromium). Presence carries a `mic` boolean, to room members only.
  - **MIC-1 (Low, privacy):** after a grant, new links give every room member the
    device's real local addresses (LAN, hotspot, VPN tunnel, container bridges). Each
    interface's STUN mapping can also show the public address of a network other than
    the default route, such as the connection under a VPN, or mobile data while on
    Wi-Fi. In the global room that is anyone. The grant is stored per origin, so
    later visits and the other demos on that origin do the same without a tap.
    `privacy: 'public-only'` drops the local (`host`) candidates, and since `6303bd8`
    every other local address too (MIC-3), but still sends each interface's public
    mapping. Any app that asks for the microphone or camera (voice, video) gets the
    same under `privacy: 'direct'`, the default. **Fixed** for Net Lab by `d78d046`:
    the global room is joined with `privacy: 'public-only'` while the microphone is
    allowed, also after a grant made in another tab, in the site settings or while
    joining. The button and its text say that named and private rooms get the local
    addresses until the permission is removed in the site settings. `877d02c` sends a
    birthday punch's offer without candidates (the hard side never used them), and
    `6303bd8` takes the default address out of both punch descriptions. Residual MIC-4.
  - **MIC-2 (Info):** a birthday punch closed while its `openFleet`, `openSpray` or
    certificate is still pending (a rejoin, its timeout, the peer's abort) throws
    when that resolves. The connections it just opened are left open, and the
    one-fleet slot is already free. Nothing is sent. **Fixed** by `9282d36`: after
    each await of a punch's setup, a punch that has ended closes what it just opened
    and returns. `691dec0` takes one offer per punch on the hard side and one answer
    on the easy side, marked before the first await. Before, each further offer from
    the peer in a punch the user had accepted opened another fleet of up to
    `FLEET_SIZE` connections, and closing the punch closed only the last: a Low
    bypass of NB48-1's one fleet at a time.
  - **MIC-4 (Low, privacy):** in the global room, the button re-tests the network
    after the grant (`reprobe()`: 2.5 s after the first STUN answer, at most 8 s) and
    only then rejoins with `public-only`; `guardGlobal` leaves it to the button. A
    member who links in that window gets the real local addresses. In Chromium, with
    a 2.5 s re-test, a tab that joined during it got all four of the other tab's local
    addresses, and none with the rejoin first. **Fixed** by `134fed3`: the button
    rejoins as soon as `getUserMedia` resolves, then re-tests. Leaving the old room
    closes its connections synchronously, so a description still gathering there is
    never sent. Accepted residual: the grant is stored before `getUserMedia`
    resolves, so during the device's open time a member joining the old room could
    still be described with the local addresses, if gathering also finishes in it.
  - **Retries (Fast lane 1, `9acde3d`, `d888522`; reviewed 2026-10-24).** When the
    page gets microphone or camera permission, a `'direct'` room now retries WebRTC
    with each peer it reaches by fallback (`retryOnGrant`, default true). So those peers
    get the local addresses within seconds too, not only peers that link later.
    `'public-only'` retries leave them out, as first dials do (`retry_direct.mjs`
    checks this in Chromium with a stored grant). In named and private rooms, Net Lab's
    button now retries (`room.retryDirect()`) instead of rejoining. Only peers on a
    fallback get a new connection, and ids stay the same. The global room joins with
    `retryOnGrant: false` and still rejoins first. MIC-4's accepted window also covers
    a retry begun in it (by the backoff, a network change or the peer's ask). Leaving
    the old room closes such a retry, as it closes a first dial, unless gathering
    finishes first.

---

## Invite strings (`pw:`)

An invite (`crates/peerwire-core/src/invite.rs`) is `pw:` + unpadded base64url of
`version | peer(32) | addrs | relays`. It carries **no secret**: the PeerId is a
public key and the addresses are routing hints. Security properties:

- **Confidentiality: none, by design.** Anyone who sees an invite learns the
  PeerId and candidate IP addresses of the inviter. An invite posted publicly is
  a standing version of SEC-3 (IP disclosure) and SEC-2 (identity disclosure):
  treat invite links as semi-private, and prefer LAN/relay candidates over a
  home IP when sharing broadly.
- **Integrity: not self-authenticating, but safe.** An invite is unsigned, so a
  carrier can swap the addresses or relays. This does not break authentication:
  the handshake still runs Noise IK to the PeerId in the invite, so a swapped
  address only reaches the real peer or fails. A carrier that swaps the *PeerId*
  produces an invite to a different identity — the recipient connects to whoever
  the carrier chose, exactly as if the carrier had sent its own invite. Invites
  therefore inherit the trust of the channel they arrive on; a QR shown in person
  or a sealed signaling message is trustworthy, a plaintext forum post is not.
- **Parsing is safe.** `from_bytes` is a bounds-checked reader, refuses more than
  16 entries, rejects trailing bytes, and validates each PeerId is a real Ed25519 point.
  Reviewed, no issues; covered going forward by fuzzing (NB-014).
- **Canonical.** Each invite has exactly one link: base64url decoding refuses a last
  symbol with its unused low bits set, and a count over 16 is refused rather than
  clamped. Nothing keys on the link string today, but a future dedupe, cache or
  allow-list on it cannot be bypassed with a lookalike. `fuzz_invites` asserts
  `to_link(parse(s)) == s` for every accepted `s`.

No new finding; the exposure is documented so apps set expectations for where an
invite may be posted.

## Godot binding (NB-020, planned — security requirements)

The coming `bindings/godot` crate exposes `PeerwireMultiplayerPeer`
(`MultiplayerPeerExtension`) so Godot's high-level multiplayer — `@rpc`,
`MultiplayerSynchronizer`, `MultiplayerSpawner` — runs over Peerwire. Peerwire
delivers *authenticated, confidential bytes* to that layer, but everything Godot
does *above* the `MultiplayerPeer` is outside Peerwire's guarantees. This section
sets the requirements the binding must meet; they are tracked as SEC-9…SEC-11
(proposed follow-ups on NB-020, owner @peerwire-backend).

### SEC-9 — Untrusted packets crossing into GDScript (High, design) — NB-029

Godot's high-level multiplayer deserializes incoming packets into Variants and
dispatches `@rpc` methods by name. Variant decoding of `Object`/`Callable`/
`RID` from the network is a known remote-code-execution surface (object
instantiation, resource loading). Because Peerwire is pure P2P, **every connected
peer is a remote attacker** at this layer.

**Requirements.**
- The binding must configure the `SceneMultiplayer` with
  `allow_object_decoding = false` (Godot's default, but assert it) so peers
  cannot ship encoded `Object`s.
- Document loudly that `@rpc("any_peer")` methods receive attacker-controlled
  arguments and must validate them and check `multiplayer.get_remote_sender_id()`.
- Surface Peerwire's per-lane size caps to the `MultiplayerPeer` so an oversized or
  malformed Godot packet is rejected at the boundary, not deep in a decoder.
- A Rust panic in the cdylib must not unwind into Godot: catch/abort at the FFI
  edge (godot-rust convention), so a malformed packet can't turn a panic into UB.

### SEC-10 — Godot peer-ID spoofing and mapping collisions (High, design) — NB-029

Godot peer IDs are 32-bit ints; Peerwire PeerIds are 256-bit Ed25519 keys. The
binding derives a stable non-zero Godot ID from each PeerId. A 32-bit space makes
collisions reachable (birthday bound ~2^16 peers, but an attacker can *grind*
PeerIds offline to target a specific victim's Godot ID).

**Requirements.**
- The PeerId → Godot-ID map is authoritative per session and **collisions must be
  rejected**, not silently merged: a joiner whose derived ID clashes with a live
  peer is renumbered or refused, never allowed to inherit another peer's ID.
- `_get_packet_peer()` must return the Godot ID of the *authenticated Peerwire
  source* of that packet only; it must never be derivable from packet contents an
  attacker controls.
- Host is Godot ID 1: the binding must bind ID 1 to the actual host identity and
  reject any joiner that tries to present as 1.
- Grinding resistance: either widen the effective namespace (keep the full PeerId
  as the identity and treat the 32-bit ID as a non-security-bearing handle the app
  must not use for authorization), or document that `is_multiplayer_authority()`
  and peer-ID checks are **not** a security boundary — the PeerId is.

### SEC-11 — Host authority over RPCs in a serverless mesh (Medium, design) — NB-029

Godot's model assumes peer 1 is a trusted server/authority. Peerwire has no
inherent authority: a peer can send any packet to any peer, and
`MultiplayerSpawner`/`MultiplayerSynchronizer` let an "authority" spawn/despawn
nodes and set properties on others' scene trees. A malicious peer that the app
treats as an authority corrupts every client.

**Requirements.**
- `docs/guides/godot.md` must state that Godot's authority model is only as trustworthy
  as the app's topology; in an open mesh, `@rpc("authority")` is not a security
  control.
- Recommend a pattern: designate the host as the only synchronizer/spawner
  authority and have it validate inputs, or use `peerwire-game`'s deterministic
  lockstep (where every peer recomputes state and desync is detected) instead of
  trusting an authority's state pushes.
- The binding should not auto-relay packets between peers on behalf of the Godot
  layer unless the app opts in (Peerwire relay is already gated and rate-limited;
  don't bypass it).

**Status of SEC-9…SEC-11 as of `ccc3b79`.** The binding shipped (NB-020) and was
hardened (NB-030). `docs/guides/godot.md` (Security) now states each requirement: keep
`allow_object_decoding` off (the demo asserts it), treat `@rpc("any_peer")`
arguments as attacker-controlled, ids are labels (the host renumbers collisions,
only the dialled host is 1, authorize by `get_peer_key`), and Godot's authority
is only as trustworthy as the peer made authority (use the host, or lockstep on
`peerwire-game`). SEC-11 is a design property: it stays a documented residual, not
a bug. NB-037's room-key proof does not change it. It only limits who can join a
room to holders of the room key, and any of them can still be malicious. Godot's
`server_relay` is on by default; `docs/guides/godot.md` names its cost, how to turn it
off, and that a relayed packet's sender id is the host's word (the host can forge
it).

**NB-029 audit (`72c7e57`, `bindings/godot/src/{peer,session}.rs`).** Each
requirement against the code:

- SEC-9: the demo asserts `allow_object_decoding` is off (`main.gd`).
  `get_max_packet_size()` is the current mode's cap, `put` refuses larger, and
  received packets wait in a queue capped at 4096 packets and 16 MiB. gdext 0.5.5
  runs every `#[func]` and virtual method under `catch_unwind`
  (`godot-core::private::handle_panic`), so a panic is logged as a Godot error
  and never unwinds into the engine; no panic is reachable from peer input in
  `session.rs`.
- SEC-10: `derive_id` never yields 0 or 1, and `IdMap::assign` moves a colliding
  key to the next derived id (`colliding_ids_are_moved`). A packet's peer is
  looked up from the authenticated Peerwire sender; a frame from a peer with no id
  is dropped. A joiner takes ASSIGN only from the host it dialled, only once,
  and never 1; a host ignores ASSIGN (`spoofed_frames_are_dropped`).
- SEC-11: documented, as above; nothing in the binding forwards packets itself.
- **N29-1 (Low) — fixed.** The data frame's mode byte is the sender's word and was
  not checked against the lane the message arrived on: a peer could label
  unreliable-lane packets `RELIABLE`, and `get_packet_mode()` reported a
  guarantee the packet never had. Repro: `on_message(host, Lane::Unreliable,
  encode_data(Reliable, ..))` queued a `RELIABLE` packet. Fix: a frame whose
  `mode.lane()` differs from the delivering lane is dropped
  (`spoofed_frames_are_dropped`, which fails with the check removed).
- **Residual.** An admitted joiner can fill the 4096-packet queue every frame,
  and Godot decodes everything it is handed: a CPU cost bounded by `max_peers`
  and each link's bandwidth, not a memory one. The host's remedy is
  `disconnect_peer` (and a ban by key); per-peer rate limits are app policy.

## Relation to the Protocol Zero red-team notes

Peerwire was bootstrapped from the Protocol Zero SDK, whose adversarial notes
(`p0/protocol-zero/REDTEAM.md`) cover a related dead-drop/signaling stack. The
mappings that carry over and are already handled in Peerwire:

- **A1 sender forgery → handled.** Peerwire signaling uses the authenticated sealed
  form (`Noise_X`, sender PeerId bound), the equivalent of P0's
  `sealAuthenticated`. A forged/anonymous seal fails to open under a claimed
  identity (`sealed.rs` test).
- **A8 packet injection/replay → handled.** AEAD per data packet + the 2048 nonce
  window; nonce recorded only after authentication (SEC/THREAT items above).
- **A7 stale/replayed signaling → partially: SEC-6.** Peerwire has no in-band
  signaling-replay guard; carry P0's lesson (bind a fresh session id) into the fix.
- **A11 metadata / A12 forward secrecy → OPEN here too: SEC-3/5 and SEC-7.** The
  same open problems (relay-visible timing/IP, static-key exposure) apply; Peerwire's
  per-session Nostr key and rotating-tag ideas mirror P0's mitigations.
- **The dead drop → NB-019.** `openInbox` leaves sealed messages on the relays as
  P0's dead drop does, with no recipient tag, a fresh signer for each drop and the app
  named inside the seal: see
  [Offline messages](#offline-messages-to-a-peerid-openinbox-nb-019--nb19-1nb19-5).
- **A5 spam / A13 Sybil → relevant to the signaling layer**; the half-open cap
  with its handshake cookies (SEC-4) and relay rate limiting are the Peerwire
  analogues. A PoW-gated first contact (as in P0 A5) is a candidate hardening if
  public rooms are abused.

## Residual risks accepted for v0.1

- Traffic analysis by a global passive adversary (packet sizes/timing) is not
  addressed.
- Metadata at the signaling layer (who talks to whom, when) is minimized but not
  hidden from a relay that correlates tags and timing. Over `LanSignaling` every
  device on the LAN sees what a relay sees, and the sender's IP and MAC addresses.
- On the docs and demos site, what the account's other Pages sites can do on the shared
  origin `avinashid.github.io`, a microphone grant given to Net Lab included (accepted
  by the owner; NB16-2).
- With a radio (`ble`, NB-110), what anyone in Bluetooth range sees and can do: that a
  Peerwire peer is near, the sizes and timing of its connections, and jamming or cutting
  them. A link over Bluetooth proves nothing about distance.
- The security of the underlying crypto crates is assumed.

## Review method

Manual review of every parser and the handshake/relay/signaling state machines in
the commit at `cf9cbb0`, plus targeted Node reproduction for SEC-1. Continuous
coverage of the parsers is tracked under fuzzing (NB-014).

Later reviews (each change touching crypto, the wire format, relaying, signalling,
traversal or the FFI) read the diff against the threat model above and reproduce
each finding on loopback, in the simulator or with unit-level probes in a scratch
checkout. The probes stay out of the tree unless the fix adopts them as tests. The
hard-NAT review (2026-10-23) used a `punch_case` probe for NB44-1, a `Node` probe
for NB44-2 and a Chromium fleet probe for NB48-1. The LAN review (2026-11-01) used
loopback multicast probes for NB64-1, NB64-2 and NB64-6, and two emulated ESP32 chips on
a hub on 127.0.0.1 for NB62-2. The NB-092 review (2026-11-03) used loopback probes for
NB92-1 and NB92-2, and two containers on a Docker bridge for NB92-3.
The Bluetooth review (NB-110, 2026-11-05) used Node probes over the in-process fake
radios and probes in the node's simulated air, and read BlueZ 5.87's source; nothing ran
over a real radio.
