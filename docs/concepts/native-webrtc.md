# Native WebRTC

A native node and a browser can be in one room. With the `peerwire` feature `webrtc` and
[`Builder::webrtc(true)`](../reference/rust.md#webrtc), a node in a room links with every `joinRoom` page in it over a
WebRTC DataChannel, on its own UDP socket, and runs the same core packets over it. To the page it is an ordinary
peer: it dials it as it dials another page, and lists it once it has proved the room key and said its presence.
Native peers in the room still link with each other over UDP, as before. NB-162.

```rust
use peerwire::{Event, Lane, Node, NostrSignaling};

let mut node = Node::builder().signaling(NostrSignaling::default()).webrtc(true).bind("0.0.0.0:0")?;
node.set_presence(r#"{"name":"kiosk"}"#)?; // what pages see in 'peer-join'
node.join_room("my-app", "Yp1M2aqk0QG2fN9GGu4i3A"); // a roomId from randomRoomId()
loop {
    node.poll();
    while let Some(e) = node.try_recv_event() {
        match e {
            // A page shows up once it has joined: proved the room key, then said its presence.
            Event::Connected { peer, .. } if node.is_browser(&peer) => {
                println!("{peer} joined: {:?}", node.presence(&peer));
                node.send(&peer, Lane::Reliable, &Node::room_text("hello page"))?;
            }
            // Its messages are room.js's frames: a tag byte (0 bytes, 1 text, 2 JSON), then the body.
            Event::Message { peer, data, .. } if node.is_browser(&peer) => {
                if let Some(text) = Node::room_message_text(&data) {
                    println!("{peer}: {text}");
                }
            }
            _ => {}
        }
    }
    # break;
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

The feature is off by default and is in no wasm module, ESP32 build or C ABI: the browser already has WebRTC, and
`peerwire-ffi` doesn't depend on `peerwire`. The Rust [`share`](../../examples/rust/share.rs) and
[`room_chat`](../../examples/rust/room_chat.rs) examples turn it on when built with it
(`--features files,nostr,webrtc`), so the [Share page](../../packages/js/demo/share/index.html) and `share` send
files to each other directly ([Share protocol](share-protocol.md)).

## Finding each other

A room's members announce themselves with a hello on the room topic, sealed under a key from the room's name
([NAT traversal](nat-traversal.md#messages)). Natives and pages say different hellos, and each takes only its own
kind, so a node with browsers on says both:

| Hello | Says it | Taken by |
|---|---|---|
| `{"t":"hello","from":…,"meta":{"nb":1,"role":…,"ts":…,"p":…}}` | every native node | natives: they link over UDP |
| `{"t":"hello","from":…,"meta":{"role":…,"ts":…,"p":…}}` | room.js | room.js, and natives with browsers on: the sender is a browser |
| `{"t":"hello","from":…,"meta":{"role":…,"ts":…,"p":…,"native":1}}` | natives with browsers on | room.js (any version: it reads `role`, `ts` and `p`, and skips only `nb`); no native |

The proof `p` is the same in all three, HMAC-SHA256 under the room key over `pw-hello`, the sender's id, its role
and `ts`. Natives parse hellos strictly: one parser takes exactly the first shape, and the other exactly the second,
so a native never takes another native's browser-shaped hello for a page's, and a node from before NB-162 (or one
without the feature) takes neither browser shape. Pages from before NB-162 take the third as they take any page's:
a native node with WebRTC links with them too, with no change to room.js.

A browser's hello that proves the room key makes it a room member, marked as a browser, unless it has sent this node
a native offer ([Security](#security)); one whose id is larger than ours is dialled, as in room.js (the smaller id
dials). The traversal never sends a browser one of its own offers.

## The link

**Signals.** The node speaks webrtc.js's signals, as JSON sealed to the peer on its inbox topic (Noise X, the
sender's PeerId authenticated), on the same carrier as everything else:

| Signal | |
|---|---|
| `{"kind":"offer","sdp":{"type":"offer","sdp":…},"p":…}` | From the smaller id. `p` is the `pw-rtc-offer` room proof, HMAC under the room key over the label, the sender's id and the recipient's (proof.js `pair`). Sent again 3 s and 6 s on while unanswered |
| `{"kind":"answer","sdp":{"type":"answer","sdp":…}}` | Answers an offer that proved the room key; sent again for a copy of it |
| `{"kind":"ice", "candidate":…}`, `{"kind":"ice-batch","c":[…]}` | A page's candidates found after its description. The node sends none: its description holds them all |
| `{"kind":"restart"}`, `{"kind":"retry"}`, `{"kind":"dg",…}` | A page's ICE restart and retry asks are taken and do nothing (the link comes back at the next hello instead); `dg` is a pipe batch ([below](#what-it-doesnt-do)) |

An offer opens a connection only if it proves the room key, and its sender then becomes a member and a browser; an
answer counts only for an offer of ours still waiting, and candidates only for a connection under way. Anything
else is dropped and counted.

**The connection.** One [str0m](https://github.com/algesten/str0m) `Rtc` per page: ICE (full agent, the dialler
controlling), DTLS 1.2 (dimpl), SCTP, and one DataChannel, webrtc.js's: negotiated, id 0, unordered, no
retransmits, label `peerwire`. Its candidates are the node's own socket at each of its addresses (only those that
are public under [`Privacy::PublicOnly`](../guides/privacy.md), loopback only for a node bound to it) and its public
address from STUN as a server-reflexive one; a node under `Privacy::RelayOnly` does no WebRTC at all, as ICE would
give its addresses out. A page's mDNS host candidates (`….local`) are skipped: the node resolves no names, and the
page's checks reach it all the same. The node's socket carries the connections beside everything else: a datagram
that starts as STUN (0 to 3) or DTLS (20 to 63) does, by RFC 7983, goes to the connection that takes it
(str0m's `accepts`: its ICE username for a check, an address its ICE checked for the rest), and anything else to
the core and the traversal as before. On a socket bound to any interface, the address a datagram came in at is
the one the kernel routes its source from, remembered.

**The core over it.** Once the DataChannel opens, the dialler dials the core's handshake over it (as webrtc.js
does), and each side hands the core the channel's datagrams as from that peer alone
(`handle_datagram_from`): a handshake from any other PeerId over the channel is dropped. The link's address is a
[`Hop::Rtc`](../../crates/peerwire/src/traversal.rs) (`0.0.0.N:2`, as a pipe's is `0.0.0.N:1`), a direct path to the
core and the policy, and `Node::path_kind` says `Direct`, or `Turn` while its packets go to a page's relay (TURN)
candidate, so [Share](share-protocol.md#where-data-may-go) sends pieces over it as a page does over its own
DataChannel. The core's datagrams go on the channel while the connection has at most 1 MiB buffered, as webrtc.js
sends them; past that they are dropped, as a full UDP buffer drops them, and the core's congestion control backs off.
A connection whose channel closes or whose ICE fails ends its link at once (`Disconnected`, `Timeout`).

## The room layer

room.js's room layer runs on every link to a browser, whatever its path (`crates/peerwire/src/webrtc/room.rs`).
Every room message starts with a tag byte:

| Tag | | The node |
|---|---|---|
| `0` bytes, `1` text, `2` JSON | The app's | Passes it to the app as it came, the tag first; JSON only if it parses. `files` frames its own as tag 0 |
| `3` presence | JSON | Notes it; the first makes the peer joined: the app sees `Connected` then |
| `4` goodbye | | Closes the link as one the peer closed (`Disconnected`, `ClosedByPeer`), on any lane |
| `5` link proof | 32 bytes | First thing each way, on the reliable lane: HMAC under the room key over `pw-link`, the sender's id and the recipient's |
| `6` nudge | | Dropped |
| `7` doc frame | JSON | An ask for a doc is answered with nothing, as room.js answers for a doc it hasn't open (doc.js `unopened`); the rest dropped |
| `8` doc frame deflated, `9` call, `10` birthday punch, any other | | Dropped, as an older room.js drops a tag it doesn't know |

On a new link each side sends its link proof. Until the peer's has checked out, only a proof or a goodbye is read,
and a wrong one closes the link; with none in 10 s (room.js `PROOF_MS`) the link closes too. Once it checks out our
presence goes to the peer (`{}` until [`Node::set_presence`](../reference/rust.md#webrtc) sets one); once the peer's
presence comes, it has joined: the app sees its `Connected` then, and its messages from then on, never before. Its
later presence frames update [`Node::presence`](../reference/rust.md#webrtc). [`Node::disconnect`](../reference/rust.md#node)
says goodbye first, on the unreliable lane, then the core's sealed close, as `room.leave()` does.

## Limits

| | Value | |
|---|---|---|
| `MAX_CONNS` | 64 | WebRTC connections at once; an offer past it is refused, and no dial is made |
| `MAX_SDP` | 32 KiB | The longest description taken |
| `MAX_CANDIDATES` | 64 | Remote candidates per connection, descriptions and signals together, counted afresh at an ICE restart; each line at most 512 bytes |
| `MAX_ICE_BATCH` | 32 | Candidates in one `ice-batch` (more: dropped whole), and held before our offer's answer |
| `OPEN_WITHIN` | 15 s | A connection whose DataChannel isn't open by then is dropped; a page dials again at its next hello |
| `PUBLISH_BURST`, `PUBLISH_EVERY` | 6, 5 s | Offers, answers and copies of either to one peer: a burst, then one per 5 s |
| `REPLACE_AFTER` | 2 s | A connection is replaced by a new offer from its peer at most this often, and restarts ICE for a page's restart offer at most this often too, or a failed try (which ends the connection); a restart offer must bring new ICE credentials (its `ice-ufrag`), or it is refused |
| `MAX_BUFFERED` | 1 MiB | Bytes buffered in a connection's SCTP association past which the core's datagrams are dropped |
| `PROOF_WITHIN` | 10 s | A link's proof must come this soon after it connects |
| DataChannel message | 1 to 1,232 B | A larger or empty one, or text, is dropped before the core (str0m takes at most 256 KiB) |
| room frames waiting | 1,024 | Room frames (proofs, presence, doc answers) waiting for send window, over all browsers |

Dials come out of the hellos' budget, which native offers share (30, then one per 2 s), and every signal out of the
inbox's (64, then one per 50 ms), opened before anything is parsed.

`MAX_CONNS` is per node, not per member: a member that proves offers under fresh PeerIds of its own can hold all 64
connections unopened, each for up to `OPEN_WITHIN`, and the inbox's budget opens enough offers to keep them taken.
Pages' offers are then refused, and so are the node's own dials, so pages fall back to pipes over the relays. This is
accepted for v1 (NB162-I1), as with the node's other per-node caps, which a member can fill under keys of its own.

## What it doesn't do

- **ICE restarts and retries.** A page asks its dialler for an ICE restart when its ICE drops, and for a retry while
  it is on a fallback. A native dialler does neither: a link whose ICE fails ends, and the page dials again at its
  next hello (every 15 s); a page's own restart offer to a native answerer is answered on the same connection if it
  brings new ICE credentials, as webrtc.js's does (`createOffer({ iceRestart: true })`), at most once per
  `REPLACE_AFTER` (2 s) for a restart or a failed try.
- **TURN on the node's side.** The node has no TURN for WebRTC: its candidates are its own. A page may still reach it
  through its TURN server, and `path_kind` says `Turn` then.
- **Fallbacks.** When WebRTC can't link a page that dials a node, the page falls back to a pipe over the signalling
  relays, which the node takes from a room member as it takes a `RelayTransport` page's (NB-126): slow, Share sends no
  file over it, and it isn't tested with WebRTC on. A node that dials a page and fails dials again at the page's next
  hello; it offers no relay path of its own.
- **Docs and calls.** The node opens no shared doc and joins no call with pages; it answers doc asks with nothing,
  so a page's catch-up doesn't wait on it.

## Dependency

One direct dependency, `str0m` 0.24.1 (MIT OR Apache-2.0), approved for NB-162; `serde_json` is the `nostr`
feature's already. str0m is sans-IO, as the core is: no threads or sockets, the time and the bytes handed in, which
is how `Node` drives it from its poll. webrtc-rs wasn't needed: it runs on tokio with sockets of its own. 0.25 asks
for Rust 1.89, over the workspace's 1.88.

Its crypto backend is `aws-lc-rs` (str0m's default): DTLS by dimpl, str0m's own DTLS 1.2, on AWS-LC, which `aws-lc-sys`
builds from C with the host's C compiler (gcc here; no cmake, make or other system package). The `rust-crypto`
backend would build AWS-LC all the same in 0.24 (dimpl's `rcgen` feature takes it for the certificate), so it buys
nothing. Each connection makes its own self-signed ECDSA P-256 certificate.

On aarch64 Linux the feature adds 69 crates to the default build (49 to one with `nostr` and `files`), every one
under a permissive licence: 63 MIT and/or Apache-2.0, `aws-lc-rs` ISC AND (Apache-2.0 OR ISC), `aws-lc-sys` ISC,
Apache-2.0, MIT, BSD-3-Clause and MIT-0 (OpenSSL and BoringSSL code), `dunce` CC0-1.0 OR MIT-0 OR Apache-2.0,
`flagset` Apache-2.0, `memchr` Unlicense OR MIT, `unicode-ident` (MIT OR Apache-2.0) AND Unicode-3.0. Each declares
a rust-version of 1.88 or older (`scripts/rust-version.mjs`). A cold release build of the `share` example with it
took 4 min 15 s on the test host.

## Security

What a reviewer should look at, in short; the [security model](security-model.md) holds for the link itself, whose
packets are the core's, sealed end to end by its Noise session whatever carries them.

- **The DTLS fingerprint and the Noise identity.** Each side's description, with its DTLS fingerprint, comes only in
  a signal sealed by its sender's PeerId (Noise X). str0m checks the peer's DTLS certificate against that fingerprint
  (`set_fingerprint_verification(true)`, its default, set explicitly), so the DataChannel is to whoever holds that
  PeerId's signals; and the core's handshake over it must then be with that same PeerId (`handle_datagram_from`), or
  it is dropped. DTLS keeps no secret of the room's: the core's own encryption does. A relay can drop, delay or replay
  signals, never read or alter one.
- **Who gets an answer, and our addresses.** An offer is answered (with this node's candidates) only if it proves the
  room key for this pair (`pw-rtc-offer`); the node offers only to a browser whose hello proved it. Under
  `Privacy::PublicOnly` the candidates are public addresses alone, and under `RelayOnly` there is no WebRTC.
- **Join proof and presence spoofing.** Hellos and offers need the room key; a hello replayed within its minute only
  says its sender is there, and any link to it still needs that sender's key. On the link, nothing but a link proof
  or a goodbye is read until the peer's `pw-link` proof checks out, a wrong one closes it, and none in 10 s closes it;
  presence counts only after that, the app sees a page only once it has joined, and a page's presence is its own,
  JSON checked to parse, which the examples print with control characters shown as U+FFFD. A goodbye closes only its
  sender's link. The browser-shaped hello's `native` mark isn't under the proof; it only keeps natives off each
  other's browser hellos, and a member that strips it can do no more than it can with its own key anyway.
- **A hello naming another member.** A hello's proof is under the room key over the id it names, so any member can
  say a browser's hello for a native's id. A browser hello marks only a peer not already known to be native: one
  that has sent this node a native offer, which only its sender can seal, is never marked, and such an offer clears
  a mark made before it (NB162-1). A hello forged before a native's first offer still marks it until that offer
  comes: meanwhile the node sends it no offer of its own and, with the smaller id, dials it over WebRTC.
- **ICE restarts.** A page's restart offer on an open connection is taken only with new ICE credentials, on which
  str0m drops the connection's old candidates and pairs, at most once per `REPLACE_AFTER` for a restart or a failed
  try, and its candidates are counted afresh (NB162-2). With the same credentials str0m would keep every candidate,
  so the list, and the time each offer takes, would grow without bound. str0m takes an offer's credentials and
  candidates before it can still refuse it (with no `a=group`, say), so a failed try counts too, and ends the
  half-restarted connection, as a failed answer does.
- **Resource caps.** See [Limits](#limits): connections, descriptions, candidates, the publish budget, the time to
  open, the SCTP buffer, the DataChannel's message size, the proof deadline, the room frames waiting; a presence is
  at most the reliable lane's largest message (1 MiB) per linked page. str0m bounds its own STUN queue (100 checks
  before the remote credentials) and sctp-proto its receive window (1 MiB).
- **What parses peer-controlled data.** `serde_json`: signals (after the seal opened: a room member or anyone who
  knows our PeerId and sends to our inbox, within its budget), presence and JSON messages, doc asks. str0m's SDP
  parser (combine): an offer's description only once its room proof checked out, an answer's only from the peer we
  offered to. str0m's candidate parser: candidates for a connection under way. str0m's STUN parser: any datagram
  that starts as STUN, from anyone who can reach the socket, but only while a connection exists (before NB-162 the
  traversal's own STUN parser read these). dimpl's DTLS records and the peer's X.509 certificate: only from an
  address the connection's ICE checked, which takes our ICE password, from the sealed description. sctp-proto: only
  inside DTLS. The room layer's tags, proofs and frames: on the link, sealed by the core.
- **Capability and versions.** Nothing new on the wire of a link: the core's packets, and room.js's frames. The
  browser-shaped hello is the one new message: pages of any version take it, natives of any version ignore it, and
  only nodes with browsers on say it. A page's signals a node doesn't act on (`restart`, `retry`) are taken and
  ignored, unknown kinds are dropped, and unknown room tags too, as room.js drops them. One thing shows: a native
  with browsers on says two hellos each round, and its browser-shaped one is longer than a page's, so a relay that
  sees the room topic can tell such a node is there (beside NB22-I1's residual).

## Testing it

- `cargo test -p peerwire --lib -- webrtc:: browser`: two connections over memory (an offer answered, the channel open,
  datagrams both ways, a close seen by ICE); offers without the room proof, malformed signals and floods; restart
  offers (the same ICE credentials or too soon refused, a real ICE restart answered on the same connection, one str0m
  refuses counted and the connection ended); the room layer (proof, presence, goodbye, doc asks, dropped tags);
  browser hellos in the traversal, ours and theirs, and one forged for a native, before and after it links (two nodes
  over memory).
- `cargo test -p peerwire --test webrtc`: native nodes with WebRTC on and off link with each other over UDP as
  ever, their events and raw messages untouched by the room layer.
- `node packages/js/test/share_webrtc.mjs` (the gate's `share webrtc`): the Share page in headless Chromium and the
  `share` example, and a `joinRoom` page and `room_chat`, in one room over a Nostr relay on loopback
  (`nostr_relay.mjs`), with no STUN server: files both ways byte for byte over the DataChannel (the page says the
  route is `direct`, the example `Direct`), chat both ways, each side's presence, goodbyes seen at once, and the
  relay carrying signalling alone. `--mb=1024` for a large file.

Measured on the test host (4 aarch64 cores, loopback, headless Chromium, 21 November 2026), 1 GiB of random bytes each
way, checked byte for byte, with other gates running beside it: 2.2 MB/s from the example to the page and 5.7 MB/s
from the page to the example, where two tabs of one Chromium shared at 1.5 MB/s the same hour; see
[Share protocol](share-protocol.md#testing-it).
