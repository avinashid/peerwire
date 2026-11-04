# NAT traversal

How two `peerwire` nodes behind NATs find and reach each other with no server of their own. The code is
`crates/peerwire/src/traversal.rs` (sans-IO, deterministic), driven by `Node` on its UDP socket and by the
simulator in `crates/peerwire-sim/tests/traversal.rs`. The session core (Noise IK, punching, peer relay) is
unchanged; this layer feeds it addresses.

## Pieces

| Piece | What it does |
|---|---|
| STUN | Binding requests on the session's own socket, so the reflexive address is the session's NAT mapping |
| Signaling | A publish/subscribe carrier keyed by 32-byte topics (`trait Signaling`): `MemoryHub` in-process, `NostrSignaling` over Nostr relays (feature `nostr`) |
| Rooms | Presence on a room topic; peers that hear each other swap sealed offers |
| Offers | Candidate addresses, NAT type and connected peers, sealed to one peer |
| Dual stack | One `[::]` socket for IPv4 and IPv6: global IPv6 host addresses are offered next to IPv4 ones |
| Port mapping | Opt-in (`Builder::port_mapping`): the router forwards a public port to the socket, by PCP, NAT-PMP or UPnP-IGD |
| Birthday punching | A random-port symmetric NAT opens 256 extra sockets towards a cone peer, which sprays ports of its IP until one hits |
| Lockstep prediction | Two sequential symmetric NATs probe each other's predicted ports in an order that makes their new mappings line up |
| Dialling | Both sides call `Endpoint::connect_with` on every candidate (plus `Path::Via` a mutual peer): a simultaneous open |

## STUN

- Servers: `Builder::stun_servers`, default `stun.l.google.com:19302`, `stun.cloudflare.com:3478` and
  `stun.l.google.com:3478`, resolved once at bind, each name once, so Google's two entries are one IP (sockets bound
  to loopback skip STUN). Google's server answers on both ports; Cloudflare's also answers on 53, which some
  networks intercept as DNS, so it isn't asked there (`cargo run -p peerwire-core --example stun_probe -- <host:port>
  ...` asks any list, each name resolved once). A dual-stack socket asks over IPv4 only (see
  [IPv6](#ipv6-dual-stack)).
- A round every 25 s (it also keeps the mapping alive); each request is retransmitted at 0.5 s and 1 s, then
  dropped until the next round.
- A response counts only if it comes from the queried server's address and carries the request's random 96-bit
  transaction id.
- NAT type (RFC 5780's mapping test, from the answers): no answer, or one, gives `Unknown`. A mapped address equal to
  a local one is `Open`. Two servers at different IPs reporting the same mapping is `Cone`, different ones
  `Symmetric`. One server's two ports reporting the same mapping while the other IP got another is `SymmetricIp`: a
  new port per destination IP, not per IP and port (NB-083); two ports of one IP that differ are `Symmetric`. One
  IP's two ports agreeing while no other IP answered says nothing about mapping per IP, so that is `Unknown`. A
  list whose servers all share one IP (mock servers on loopback) is judged as before NB-083: one mapping for all is
  `Cone`, else `Symmetric`.
- `Cone` covers full, restricted and port-restricted cones, which STUN can't tell apart. So `Cone` doesn't
  promise a punch: a port-restricted cone only lets in packets from ports it has sent to, so against a
  symmetric NAT whose ports can't be predicted it needs a [birthday punch](#birthday-punching). Neither can STUN
  tell how a symmetric NAT filters: `SymmetricIp` NATs that let in any port of an IP they sent to and ones that
  don't look the same.

## Port prediction

A symmetric NAT gives each destination a new public port, so the port it offers (from STUN) is not the one its
packets to the peer come from, and a port-restricted peer drops them. Many symmetric NATs hand ports out in
sequence, though:

- **Prediction.** The ports the NAT handed out to the STUN servers, each new one once, in the order asked: if they
  share an IP and each is 1 to 8 above the one before, the NAT looks sequential with the first two's step, and the
  next mapping is likely `last + step` (`Traversal::predicted_addr`). With the default servers a NAT that maps per
  IP and port hands out three, one that maps per IP two. Random-port NATs give no prediction.
- **Offer.** The prediction rides in the offer's address list as a tagged entry (below), only when the public
  address leads the list. It takes one of the 8 slots.
- **Probing.** A receiver whose NAT is neither open nor symmetric (open and symmetric ones gain nothing, and a
  symmetric one would spend a port per probe; two sequential symmetric NATs use
  [lockstep prediction](#lockstep-prediction) instead), given an offer whose first address is public and dialable, hands
  the core 32 *guesses*: ports `next - 4·step` to
  `next + 27·step`, leaning forward because the NAT may have given out more since STUN asked
  (`Endpoint::set_guesses`). Guesses are probe-only, and probed three times:
  - while dialling, on the first three handshake sends (at 0, 0.5 and 1.5 s);
  - while relayed, on three punch rounds, every 5th one (over about 1 s);
  - the same guesses set again (a repeated offer) change nothing; new ones get three probes, and only the new
    ones are sent to at once;
  - a guess becomes the path only when the peer answers from it, and guesses are dropped once the path is
    direct;
  - relay-only links that go through a relay ignore them.
- **Cost.** At most 32 guesses (`MAX_GUESSES` is 64) per offer acted on, each probed 3 times: 96 datagrams to
  ports of the one IP the offer named (see the reflection bound below).
- Datagrams are told apart by their first byte (RFC 7983): STUN goes to the traversal, everything else to the
  endpoint.

## Birthday punching

A symmetric NAT with random ports gives no prediction, and a port-restricted cone drops every packet from a port it
hasn't sent to, so the two can't punch with one socket each (NB-044). With many sockets on the symmetric side they
can, by the birthday paradox, as Tailscale does:

- **Who does what.** Decided from the two NAT types in the offer:
  - this node `Symmetric` or `SymmetricIp` with no prediction, the peer `Cone`: it opens 256 extra sockets
    (`BIRTHDAY_SOCKETS`) and sends one packet from each to the peer's public address. Each gets its own mapping,
    at a random port, that lets the peer's address in;
  - this node `Cone`, the peer `Symmetric` or `SymmetricIp` with no prediction: it sprays up to 2048 ports
    (`BIRTHDAY_PROBES`) of the peer's IP, each once, in a random order (`Endpoint::spray`);
  - a `SymmetricIp` side and another symmetric one, unless both predict: the `SymmetricIp` side opens as above and the
    other sprays, at most 1024 ports from a NAT that maps per IP and port; two `SymmetricIp` sides both do both (see
    [two-sided birthday punch](#two-sided-birthday-punch)). Two that predict sweep in [lockstep](#lockstep-prediction)
    instead.

  A probe that lands on one of the 256 mappings reaches its socket. The answer leaves from that socket, and the
  cone lets it in, as it has just sent to that port. The link takes that path.
- **When.** Only towards a public IPv4 address (the offer's first, from STUN), and only while the peer is not
  linked directly. The symmetric side opens its sockets 1 s after the offer and the cone side sprays from 1.5 s,
  so an ordinary punch goes first, and the sockets are open when the probes arrive though offers reach the two
  sides a signaling delay apart. Not under `RelayOnly`.
- **Dialling or relayed.** While dialling, the extra sockets send the dial's HS1 and the spray is the handshake. A
  dial that wins the simultaneous open answers a peer's HS1 on a path it already dials at once (at most once per
  `handshake_retry`), so a hit links without waiting for the next retry. On a link already relayed (through a
  room peer or a [relay peer](#relay-peers)), both send punches over it, and the link moves to the direct path
  as any punch does.
- **Odds** (theoretical). Each probe hits one of 256 mappings among 64512 ports with p = 256/64512, so all 2048
  miss with probability (1 - p)^2048 ≈ 0.03%. The spray stops at the first hit, after 1/p ≈ 252 probes on average.
- **Time.** Probes go 16 every 30 ms (about 530 a second), so a full spray takes about 3.8 s; in the simulator
  the pair links in about 2.25 s after joining (median).
- **The host's part** (`Traversal::poll_birthday`, done by `Node`): it opens the sockets on the main socket's
  address, hands the endpoint what they receive, and sends to an address out of the first extra socket that heard
  from it (its route), everything else from the main socket. Only a packet of the punch's peer that authenticated
  on a link whose direct path is that address makes a route, or keeps one (NB44-2, NB44c-1, NB44c-2): not junk, an
  HS1 sent again while its handshake is half-open, an HS2 sent again, or another peer's handshake. The peer's HS1
  makes a route only for the answer to it, until the socket's `until`, and ends no other route: Noise IK's first
  message has no freshness check, so a copy of one reads again once its handshake is no longer half-open
  (NB85-1). Its HS2 to our dial, or a Data packet, makes the route that lasts. A peer has one route: a new one ends
  its route at another address, and that socket closes as one that is no route. Sockets that are no route close
  after 10 s, a route once it has heard nothing for 60 s. At most 512 are open at once, and while any are,
  `Node::next_timeout` is at most 5 ms, because a host waiting on `Node::socket` can't see them.
- **Limits.** One try per peer per 5 minutes; per node, a burst of 4, then one per 30 s; and, per destination IP
  across all peers (NB44-1), only one punch to an IP at a time and at most 2 per 5 minutes, so a flood of offers
  from many PeerIds (a room-key holder can mint them freely) naming one victim can't make the node punch it four
  times over. The spray aims at the IP STUN saw, so it assumes the symmetric NAT maps every socket to that one IP
  (RFC 4787 "paired" pooling); a carrier NAT that hands each mapping an IP from a pool defeats it. Not yet measured
  on real networks.
- **Cost.** The symmetric side: 256 sockets for 10 s and 256 packets of about 130 bytes (the HS1), about 33 KB.
  The cone side: up to 2048 handshakes or punches to ports of one IP, about 270 KB over 3.8 s, about 32 KB on
  average. Its own router keeps a connection-tracking entry per probe for a while; a router with a small table
  may drop new flows meanwhile. A random-port symmetric NAT can't punch another symmetric NAT unless one of them
  maps per destination IP: see [two symmetric NATs](#two-symmetric-nats).

### In browsers

A browser's only sockets are inside RTCPeerConnections, so `packages/js/src/birthday.js` punches with those (NB-048):

- **The symmetric side** opens 256 connections (`FLEET_SIZE`) that share one certificate and one ICE username and
  password, so the cone side's one offer gets one answer for all of them. Each is given one remote candidate, the
  cone side's public address, so its checks open a mapping towards it. The first whose channel opens keeps the link;
  the rest close.
- **The cone side** opens one connection and adds the symmetric side's public IPv4 at 1024 ports (`SPRAY_PORTS`, in
  the native spray's order) as low-priority candidates. ICE checks about 20 a second.
- **Odds** (theoretical). About 1024 · 256 / 64512 ≈ 4 tries land on a mapping, so all miss with probability
  (1 - 256/64512)^1024 ≈ 1.7%, and the first lands after about 252 tries, some 13 s.
- **Addresses.** Neither side checks a candidate from the other's description: both are stripped before use, and
  the one address each side aims at comes from elsewhere (below), public IPv4 only. In a room that is the peer's own
  candidates on the failed link, which the peer writes, so a member can name a third party's address; a page aims at
  most 2 punches at any one IP per 5 minutes, whichever rooms and peers ask (NB84-1, as NB44-1 does natively).
- **In rooms** (NB-084, `src/punch.js`). `joinRoom` runs one punch per peer it reaches by fallback when the WebRTC
  link that failed showed this side and that peer as a cone and a symmetric NAT with no prediction: one IPv4 address
  with one port, and one with two ports more than 8 apart (two STUN servers answering). The cone side offers over the
  room's encrypted link, with the public address of the connection it offers. The symmetric side opens its fleet only
  if its own view agrees and the peer's candidates on the failed link named that address's IP, then answers; the
  cone side sprays the IP the peer's candidates named. The channel takes the link over (`room.adoptChannel`), and
  the fallback carries it until then. One punch at a time per page, the next at least 30 s after one that ran; at
  most 2 at any one IP per 5 minutes; once per peer while in the room; 60 s at most, after which, and when either
  side leaves, it closes what it opened. A pair with one public IP on both sides gets none (a NAT doesn't send back
  to itself). `birthday: false` turns it off. Tests: `room_birthday.mjs` (the cases, with fake connections),
  `room_birthday_browser.mjs` (real connections in headless Chromium, the NATs played by a wrapper),
  `birthday_fleet.mjs` (the fleet alone).
- **Net Lab's button** runs the same punch between two members, its roles from Net Lab's network probe (a cellular
  hint when neither reads symmetric), and only on a click. Net Lab's global room, which anyone can join, runs no
  punch of its own. In one field test (October 2026: a Windows PC behind a symmetric carrier NAT and an Android
  phone on mobile data) the channel formed in 11.1 s.

## Lockstep prediction

Two symmetric NATs that hand out ports in sequence can't punch with ordinary probes: each new destination costs
the sender's NAT its next port, so neither side's offered port is the one its probes come from (NB-045). If each
probes the other's predicted ports in a known order, the new mappings line up:

- **Who does what.** When this node and the peer both offered a [prediction](#port-prediction), each sweeps the
  peer's predicted ports at the peer's public IP (`Endpoint::sweep`), every probe to a new address, in order:
  - the side with the lower PeerId probes `next + k·step` for k = 0 to 95 (`LOCKSTEP_LOW`);
  - the other probes `next + 2k·step` for k = 0 to 63 (`LOCKSTEP_HIGH`).
- **Why it meets.** Each NAT may have handed out ports since STUN asked (the dial's own new destinations, other
  traffic, other users of a carrier NAT): `dl` on the lower side, `dh` on the other. The lower side's probe `i`
  then leaves from its `next + (dl + i)·step` and goes to the other's `next + i·step`; the other's probe `j` leaves
  from its `next + (dh + j)·step` and goes to the lower side's `next + 2j·step`. For `i = dl + 2·dh` and
  `j = dl + dh` each probe goes from the other's destination to the other's source, so whichever arrives second
  finds the mapping open, and the answer goes back through the same pair. Equal strides would meet only when
  `dl = -dh`; the unequal ones meet for any `dl, dh ≥ 0` with `dl + 2·dh < 96`: up to about 31 ports on each side,
  whichever has the lower PeerId.
- **When.** 1 s after the offer, while dialling or on a relayed link (probes are the handshake, or punches), only
  towards a public IPv4 address, not under `RelayOnly`, and within the same limits as a
  [birthday punch](#birthday-punching): once per peer per 5 minutes, a node budget of 4 then one per 30 s.
- **Cost.** At most 96 handshakes (about 12 KB) to ports of one IP, and as many ports of the sender's own NAT,
  which it keeps for its mapping timeout. In the simulator the pair links 1.51 s after joining.
- **Limits.** A NAT whose ports jump by more than the step, or that many other users share fast enough to hand out
  more than about 31 ports while the probes go, still fails, as do NATs that predict no port (random ones). So
  can a sequential NAT that filters by address only (`sym-seq-adf` below), sweeping at stride 2 (the higher
  PeerId) against one that filters by port: it lets in the other side's probes that reach its new mappings out of
  step, answers each from yet another port, which the other side drops, and each answer takes its next port and
  pushes its sweep further out of step. In the simulator 16 of 25 such tries link, against 25 of 25 with it the
  lower PeerId and 50 of 50 with both filtering by address
  (`cargo test -p peerwire-sim --release --test traversal address_filtering -- --nocapture`). Not yet measured on
  real networks.

## Two symmetric NATs

No punch links a random-port symmetric NAT and another symmetric one when both map per IP and port (NB-080): in the
[matrix](#coverage), `sym-random` against `sym-random`, `sym-seq` and `cgnat(pr+sym)`, and their variants that
filter by IP. Those links go through a peer both sides are linked to: a third room peer, a
[relay peer](#relay-peers) (`Node::set_relay_peers`), or, between browsers, another member or the signalling relays
([through any NAT](../reference/javascript.md#through-any-nat)). A NAT that maps one port per destination IP and
lets in any port of an IP it has sent to (`sym-ip-adf`) is the exception: STUN tells it apart (`SymmetricIp`), and
a [two-sided birthday punch](#two-sided-birthday-punch) links it with every symmetric kind (NB-083).

- **Mapping and filtering** (RFC 4787). A NAT's *mapping* says which destinations share one outside port: all, those
  at one IP, or only one IP and port. Its *filtering* says whom a mapping lets in: anyone, any port of an IP it has
  sent to, or only the IPs and ports it has sent to. The simulator's kinds (`peerwire-sim`'s `NatConfig`):

  | Kind | Mapping | Ports | Filtering | STUN says |
  |---|---|---|---|---|
  | `full-cone` | one for all | kept | anyone | `Cone` |
  | `restricted` | one for all | kept | per IP | `Cone` |
  | `port-restr` | one for all | kept | per IP and port | `Cone` |
  | `sym-seq` | per IP and port | in sequence | per IP and port | `Symmetric`, with a prediction |
  | `sym-random` | per IP and port | random | per IP and port | `Symmetric` |
  | `cgnat(pr+pr)` | `port-restr` behind a `port-restr` carrier NAT | | | `Cone` |
  | `cgnat(pr+sym)` | `port-restr` behind a `sym-seq` carrier NAT | | | `Symmetric`, with a prediction |
  | `sym-seq-adf` | per IP and port | in sequence | per IP | `Symmetric`, with a prediction |
  | `sym-rand-adf` | per IP and port | random | per IP | `Symmetric` |
  | `sym-ip-adf` | per IP | random | per IP | `SymmetricIp` |
  | `sym-ip-apdf` | per IP | random | per IP and port | `SymmetricIp` |
  | `ipseq-adf` | per IP | in sequence | per IP | `SymmetricIp`, with a prediction |
  | `ipseq-apdf` | per IP | in sequence | per IP and port | `SymmetricIp`, with a prediction |

  Every symmetric kind before NB-080 filters by IP and port. `NatConfig::address_filtered` makes any kind filter by IP
  only, and `NatConfig::address_dependent` maps per IP. The two `ipseq` kinds are tested outside the matrix. The
  simulator's first STUN server answers on a second port, as Google's does, so [STUN](#stun) tells `sym-ip` from
  `sym-random`; no public server tells filtering apart, so `sym-ip-apdf` is `SymmetricIp` too.
- **Why per-port mapping on both sides stops a punch.** A link needs, on each NAT, the mapping that carries its
  packets out to let the other side's in: A's mapping at port P made for B's port Q, and B's at Q made for A's P.
  Each side picks only where it sends; its NAT picks the port. A probe that gets in is answered to its source, a new
  destination, so the answer leaves from a new port, and a NAT that filters by port drops it. Filtering by IP on one
  side gains nothing, then. On both sides the answer gets in, but the core moves a direct link to the address of its
  newest packets, so the link follows every answer to a new port, and every packet makes a new mapping on both NATs.
- **Odds** (theoretical), per try of a punch in which both sides open k = 256 sockets (`BIRTHDAY_SOCKETS`) and probe
  m = 2048 (`BIRTHDAY_PROBES`) of the N = 64512 ports:
  - random against random, both filtering by port: both ports must be guessed, 1/N² per pair of probes, so about
    m²/N² ≈ 0.1%;
  - random against sequential: the random side can aim at the other's predicted ports, but that side must still guess
    the random one, so at most M/N for M aimed probes: 3.2% for 2048 (about 270 KB), and 50% would take N·ln 2 ≈
    44700 probes (about 5.8 MB). The 256 sockets aimed at predicted ports instead: k/N ≈ 0.4%;
  - filtering by IP on one side only: the same, as the answer leaves from a new port;
  - filtering by IP on both sides: the probes meet, about m·(k + m/2)/N ≈ 40 times, but the link chases ports
    (above);
  - one port per destination IP and filtering by IP (`sym-ip-adf`), against any symmetric NAT: the answer leaves from
    the mapping the probe hit, as against a cone, so 1 - (1 - k/N)^m ≈ 99.97%, or 98.3% for the m = 1024 that a NAT
    mapping per IP and port sprays in the [punch](#two-sided-birthday-punch) built for it; filtering by port
    (`sym-ip-apdf`) instead, about m²/N² again.
- **For two NATs that map per IP and port: considered, not built.** STUN can't tell how they filter, so a planner
  for them would have both sides open the sockets and probe, and it would pay for nothing: 0 of 50 tries over 25
  seeds each way round for `sym-random` against `sym-random` and `cgnat(pr+sym)`, and for the variants that filter
  by IP on one side (1 of 50, by luck, for `sym-random` against `sym-seq`). Per try each side sends 256 handshakes
  (about 33 KB) and up to 2048 probes (about 270 KB over 3.8 s), and holds up to about 2300 ports of its own NAT for
  the NAT's mapping timeout (about 4600 mappings a try over the two NATs). That can use up the block of ports a
  carrier NAT gives its user, and so fail the user's other new connections meanwhile. With both sides filtering by
  IP the links come up, 50 of 50, but make 99 mappings and 100 roams in 10 s at 10 packets a second each way.
  Keeping them would take a change to the core's path model: sending to one address while hearing the peer from
  another, over a mapping kept open by inbound packets alone, which RFC 4787 leaves optional (REQ-6). From
  `cargo test -p peerwire-sim --release --test traversal two_sided -- --nocapture`, which drives both roles from the
  test.

### Two-sided birthday punch

A NAT that maps one port per destination IP (NB-083) answers a probe that gets in from the mapping the probe hit,
as a cone does: the answer goes to the probe's source, a new port at the same IP, so it leaves through the same
mapping, and the prober's NAT lets it in, as it has just sent there. So such a NAT can take the cone's seat in a
birthday punch against any symmetric NAT, if it lets in any port of an IP it has sent to.

- **Who does what.** Between a `SymmetricIp` node and a symmetric peer whose offer says how its NAT maps (the mapping
  entry, see [Messages](#messages)), when at least one of the two has no prediction: the `SymmetricIp` side opens 256
  extra sockets towards the other's public address 1 s after the offer, and the other side sprays ports of its IP from
  1.5 s, as in a [birthday punch](#birthday-punching). A side that maps per IP and port (`Symmetric`) sprays at most
  1024 ports (`PER_PORT_PROBES`) and opens no sockets: each probe costs it a port, and its own sockets would let a probe
  in only if its NAT filtered by IP (NB83-1). Two `SymmetricIp` sides each do both, as one punch (`Plan::Both`, then
  `Plan::Then`), with the full 2048-port spray, which costs each NAT one port, so either side's sockets can take the
  other's probes. A `sym-ip` NAT whose second STUN port didn't answer reads as `Symmetric` and sprays, so it links only
  if the other side lets in any port of an IP it has sent to.
- **Two that predict** sweep in [lockstep](#lockstep-prediction) instead, as before NB-083, whatever their offers say
  of how they map: a `sym-ip` NAT that hands out ports in sequence predicts them, and STUN read it as a sequential
  `Symmetric` NAT before. Each side sees the same two predictions, so both pick the same plan. The sweep links such
  pairs every time, while a punch with sockets lets no probe into a side that filters by port.
- **Older peers.** A node from before NB-083 reads `SymmetricIp` as `Symmetric` and plans nothing for two symmetric
  NATs, so towards a peer whose offer doesn't say how its NAT maps, a `SymmetricIp` node is `Symmetric`, as before: no
  such punch, and lockstep prediction with a sequential peer when both predict.
- **Limits.** Those of a [birthday punch](#birthday-punching): only towards a public IPv4 address, while the peer isn't
  linked directly, not under `RelayOnly`, once per peer per 5 minutes, the node's budget (a burst of 4, then one per
  30 s), and the per-IP cap (NB44-1); the two halves of a two-sided punch are one punch. On top of that, a node behind a
  NAT that maps per IP and port pays one of its NAT's ports per probe, up to 1026 a try with the dial's two, so it
  sprays at most once per 5 minutes, whichever peers ask (`PER_PORT_PUNCH_GAP`).
- **Odds** (theoretical). 1024 probes hit one of 256 sockets among 64512 ports with probability 1 - (1 - 256/64512)^1024
  ≈ 98.3%, 2048 with 99.97%. The spray steps through the ports at one random stride from a random start
  (`Endpoint::spray`), as good as random draws against sockets at random ports. Against the consecutive ports a
  sequential NAT gives the sockets, some strides leave gaps wider than 256 among the first 1024 steps, so more tries
  miss (`ipseq-adf` × `sym-random` below).
- **Cost.** The `SymmetricIp` side sends 256 handshakes (about 33 KB), and its NAT makes 258 mappings: one per socket,
  and two for the dial to the peer's public and LAN addresses. The other side sprays up to 1024 probes (about 135 KB
  over 1.9 s), or 2048 from a `SymmetricIp` NAT, and stops at the first hit: behind a NAT that maps per IP and port each
  probe is a new mapping, up to 1026 with the dial's, while behind a `SymmetricIp` one the whole spray shares the
  dial's, as it goes to one IP. NAT mappings a try in the simulator, over 25 seeds each way round, on the first kind's
  side and on the second's:

  | Pair | Direct | Connect median, max | Mappings a try |
  |---|---|---|---|
  | `sym-ip-adf` × `sym-seq`, `sym-random`, `sym-seq-adf`, `sym-rand-adf` | 49/50 each | 2.19 s, 3.54 s | 258 + 279 |
  | `sym-ip-adf` × `cgnat(pr+sym)` | 49/50 | 2.27 s, 3.33 s | 258 + 263 |
  | `sym-ip-adf` × `sym-ip-adf` | 50/50 | 2.21 s, 2.81 s | 258 + 258 |
  | `sym-ip-adf` × `sym-ip-apdf` | 50/50 | 2.36 s, 3.32 s | 258 + 258 |
  | `sym-ip-apdf` × `sym-seq`, `sym-random`, `cgnat(pr+sym)`, `sym-seq-adf`, `sym-rand-adf` | 0/50 each | | 258 + 1026 |
  | `sym-ip-apdf` × `sym-ip-apdf` | 0/50 | | 258 + 258 |
  | `sym-random` × `sym-random` (nothing planned) | 0/50 | | 2 + 2 |
  | `ipseq-adf` × `sym-random` | 46/50 | 2.24 s, 3.71 s | 258 + 293 |
  | `ipseq-adf` × `sym-ip-apdf` | 50/50 | 2.24 s, 3.45 s | 258 + 258 |
  | `ipseq-apdf` × `sym-ip-adf` | 50/50 | 2.27 s, 4.19 s | 258 + 258 |
  | `ipseq-apdf` × `sym-random` | 0/50 | | 258 + 1026 |
  | `ipseq-adf`, `ipseq-apdf` × a kind that predicts (lockstep) | 50/50 each | 1.51 to 1.53 s, 1.53 s | 2 + 2 to 60 |

  The last row is six pairs: `ipseq-adf` against `sym-seq` and `ipseq-apdf`, and `ipseq-apdf` against `sym-seq`,
  `sym-seq-adf`, `cgnat(pr+sym)` and itself. Before NB-083 the planner gave those pairs the same lockstep sweep, and the
  four `ipseq` pairs above them nothing. Every link that comes up holds: no new mappings and no roams over 10 s of
  traffic at 10 packets a second each way. `sym-ip-apdf` lets no probe in, so its tries cost the other side a full
  spray: 1026 mappings, under the limits above. From
  `cargo test -p peerwire-sim --release --test traversal sym_ip -- --nocapture`. Not yet measured on real networks, and
  how many real NATs map per IP, and of those how many filter by IP, is unknown.

## IPv6 (dual stack)

Mobile carriers often give a phone a global IPv6 address next to an IPv4 one behind a carrier-grade NAT, which is
often symmetric (NB-042). IPv6 has no NAT, so a simultaneous dial only has to get through each side's stateful
firewall. A firewall lets a packet in once its side has sent one to that address and port, and both sides send at
once: the punch through a port-restricted cone, with no port to guess.

- **Binding.** `Node::bind("[::]:0")` binds one socket with `IPV6_V6ONLY` off, for both families.
  - Where IPv6 is missing (the socket can't be made or bound), it binds `0.0.0.0` on the same port instead and
    works as an IPv4 node. A port already in use fails as before.
  - `0.0.0.0` binds IPv4 only; a given address binds that address only.
  - The Godot binding binds `[::]`.
- **Addresses.** IPv4 peers reach a dual-stack socket as `::ffff:a.b.c.d`. The node turns them back into IPv4
  addresses on receipt and maps IPv4 destinations on send. A peer then has one address whichever socket sees it,
  and filters see `127.0.0.1` rather than `::ffff:127.0.0.1`.
- **Candidates.** Bound to any interface, a dual-stack node offers, after the STUN, mapped and observed addresses:
  the address of each family's default route, then both loopbacks.
  - The IPv6 address is the one the kernel picks as the source towards the internet: global (or ULA), never
    link-local. With no NAT, that is the address peers see.
  - `PublicOnly` offers a global IPv6 address but not a ULA one; `RelayOnly` offers none.
- **STUN** is asked over IPv4 only. Answers over both families would be two different mappings, which reads as a
  symmetric NAT, and an IPv6 answer would only repeat the host address. The NAT type and port prediction are
  about IPv4.
- **Dialling.** In offers and invites alike, a node skips candidates of a family its socket can't send to: an IPv4
  socket skips IPv6 addresses, one bound to `::1` skips IPv4 ones. The rest are dialled at once, as before. The
  link takes the first path whose handshake answers, whichever its family.
- **Port mapping** maps the IPv4 side of a dual-stack socket. IPv6 isn't mapped, and no firewall pinhole is asked
  for (PCP can, but the simultaneous dial already opens one).
- **What a peer learns:** the IPv6 address, like a public IPv4 one, in sealed offers and in invites.
  - It can name the device more closely than an address shared behind a carrier NAT.
  - Without privacy extensions (RFC 8981), its last 64 bits can stay the same across networks.
  - `RelayOnly` hides it, as it hides every address.
- **Browsers** gather IPv6 candidates themselves, and `webrtc.js` passes them on unfiltered. Android Chrome lists its
  mobile-data interface's real addresses even without microphone permission, the global IPv6 one included, and
  STUN over IPv6 (both default servers have one) answers with that same address as a `srflx` candidate.
  - **Field test** (3 Nov 2026): two Android Chrome phones on Jio mobile data, one page without microphone
    permission, linked directly IPv6 host to IPv6 host, 0.2 s after the answer and 1.2 s after joining, at 31 ms
    ICE RTT and 36 to 248 ms DataChannel pings (25 sent, none lost). Both carrier NATs mapped as cones and the IPv4
    public pair answered its checks too; ICE chose IPv6.
  - Under `'public-only'` the host candidates go, but the IPv6 `srflx` one is the device's own address: peers
    still learn it, and the direct IPv6 path may stay (not field-tested). A page with the permission may send no
    IPv6 `srflx`, as it repeats a host candidate the browser no longer hides.
- **Not done:**
  - IPv6 STUN;
  - more than one IPv6 address per host;
  - NAT64 / 464XLAT discovery, so an IPv6-only network reaches IPv4-only peers only through its own translator.

## Router port mapping

Opt-in, off by default: `Node::builder().port_mapping(true)`. For a node behind a symmetric NAT facing a peer whose
router filters by port, where punching can't work and no third peer can relay (NB-038). The code is
`crates/peerwire/src/portmap.rs`: a sans-IO `Mapper` (the protocols, leases and fallback order, tested
deterministically) run by one thread with its own UDP socket, so the node's socket and `poll` are untouched.

- **Order.** PCP MAP (RFC 6887), then NAT-PMP (RFC 6886), both to the default gateway's port 5351, then UPnP-IGD:
  an SSDP search, the device description, `GetExternalIPAddress`, then `AddPortMapping`. Each UDP request is sent
  up to 3 times (at 0, 0.25 and 0.75 s); SSDP answers are waited for 3 s. A refusal moves on at once, as does a
  NAT-PMP-only server answering PCP. If all fail, the state is `Unavailable` and it starts over every 5 minutes.
- **Gateway.** The IPv4 default route from `/proc/net/route` on Linux; elsewhere `.1` in the LAN's /24 (UPnP
  finds the router by SSDP either way). IPv4 sockets only, and not ones bound to loopback.
- **Lease.** 2 hours asked for, renewed at half of what the router grants. PCP renewals reuse the nonce and suggest
  the port held. A UPnP port already taken on the router is retried on 2 random high ports; a router that only
  allows permanent UPnP leases gets one, deleted with the node. A PCP or NAT-PMP answer with lifetime 0 means no
  mapping, and counts as a refusal. A failed renewal drops the address and starts over.
  Dropping the node deletes the mapping. It waits for that: usually a few milliseconds, at most 7 s (an HTTP
  exchange under way, then the delete's own). A UPnP chain stopped half-way (description, address, Add retries)
  sends nothing more after the exchange under way. A PCP or NAT-PMP map request still unanswered at the stop may
  have made a mapping whose answer is never read, so it gets a delete too, sent once (NPM-3).
- **After a crash.** A node that never drops leaves the mapping on the router until its lease runs out (2 hours),
  or for good under UPnP's permanent-lease fallback, until the router restarts or someone deletes it.
- **Only public addresses.** If the router's external address is carrier-grade NAT (100.64/10) or private, another
  NAT sits beyond it: the state is `NotPublic { addr }`, nothing is offered, and the mapping is deleted (NAT-PMP
  and UPnP check the address before mapping).
- **As a candidate.** `Traversal::set_mapped` puts the mapped address after the STUN address and before observed
  and LAN ones. It goes wherever public addresses go: invites, offers in `Direct` and `PublicOnly`, and the public
  answer to invite holders (NB-033). It stays behind the STUN address so that port prediction, which keys off
  the first address, still works.
- **Skipped under `RelayOnly`.** A relay-only node gives out no address, so it doesn't ask the router at all
  (NPM-1): with `port_mapping(true)` it binds in state `RelayOnly` and sends nothing. `Node::set_privacy` to
  `RelayOnly` stops the worker, which deletes the mapping on its own thread without blocking the caller; leaving
  `RelayOnly` starts a new worker, first waiting (up to 7 s, as on drop) for the old one's delete to go out.
- **State.** `Node::port_mapping()` is `Off`, `Trying`, `Mapped { addr, protocol }`, `NotPublic { addr, protocol }`,
  `Unavailable` or `RelayOnly`, updated by `poll` and `set_privacy`.
- **Limits.** Whether a mapping gets a port-restricted peer through also depends on the router sending replies
  from the mapped port; only a field test shows it for a given router.

## Topics and the room key

Topics use the same derivation as `packages/js/src/nostr.js`, so Rust and browser peers can share relays:

- room: `sha256("peerwire/room/" + app_id + "/" + room)`; a JS peer in room `"<app_id>/<room>"` uses the same topic;
- peer inbox: `sha256("peerwire/peer/" + lowercase hex of the 32-byte PeerId)`.

Every relay subscriber sees topics, so a topic proves nothing. The **room key**,
`sha256("peerwire/room-key/" + app_id + "/" + room)`, is the same name hashed under another label. It can't be
derived from the topic, so only peers given the name hold it. Hellos and room offers carry HMAC-SHA256 proofs
under it. The room name works as the room's password, and a name others can guess makes a public lobby. The
topic is an unsalted SHA-256 of the name, so whoever sees it can test guesses offline at hash speed: a private
room needs a name with **at least 128 random bits**, such as 22 or more random base64 characters or 32 hex digits.

## Messages

**Hello** (room topic, sealed to the room, NB-022b). The JSON shape of `nostr.js`, with a marker for UDP peers and
a room proof:

```json
{"t":"hello","from":"<64 hex PeerId>","meta":{"nb":1,"role":"peer|host|client","ts":<Unix ms>,"p":"<64 hex>"}}
```

- It is sealed as `nostr.js` `sealRoom` seals a browser's hello: the payload is a random 12-byte nonce, then
  `AES-256-GCM(hello key, nonce, JSON)` with its 16-byte tag and no associated data, and the Nostr `content` is
  its base64. The hello key is `sha256("peerwire/room-enc/" + app + "/" + room)`, a third label over the name, so
  a relay that sees the topic can't read who is in the room. A test vector checks both stacks give the same bytes.
- A payload that doesn't open under the hello key is dropped: a plaintext hello, one for another room, an altered
  one, or one longer than the longest hello (215 bytes) sealed, which isn't decrypted at all.
- `p = HMAC(room key, "pw-hello" || PeerId || role byte || ts u64 LE)`.
- Parsing is strict: exact prefix, 64 hex digits, known role, 1 to 19 decimal digits of `ts`, 64 hex digits of
  proof, exact suffix.
- A hello with a wrong or missing proof, or with `|ts - now| > 60 s`, is dropped before anything else happens.
  Browser hellos lack `meta.nb` and are ignored; `room.js` doesn't dial a hello with `meta.nb` over WebRTC.
- A sealed hello is 316 base64 characters (320 for `client`), a browser's 304 (308): its length, topic and timing
  show a relay that a member announced itself, whether it is native or a browser and whether it is a client, but
  not who (NB22-I1, an accepted residual: hiding it would pad both stacks to one length).
- A hello can be replayed within its minute, but only as itself: it names its sender. The offer it triggers is
  sealed to that sender, and the dial authenticates the sender's key.

**Offer** (peer topic): `sealed::seal(me, to, …)` (`Noise_X_25519_ChaChaPoly_BLAKE2s`, sender authenticated),
around:

```text
ver   0x01 (plain), 0x02 (room) or 0x03 (invite)
      reply(0|1)  role(0 peer,1 host,2 client)  nat(0 unknown,1 open,2 cone,3 symmetric)
ts    u64 LE      sender's Unix time in ms
nonce u64 LE      random
n     u8 (<= 8)   then n x [len u8][Address::encode], or one port prediction entry:
                  [5][0xfe 0x01][step u8][next port u16 LE]
                  or, from a symmetric NAT, one entry for how it maps (NB-083):
                  [3][0xfe 0x02][1 per destination IP | 0 per IP and port]
m     u8 (<= 8)   then m x 32-byte PeerId (peers the sender is connected to)
proof 32 B        version 2: HMAC(room key, "pw-offer" || sender || recipient || everything above)
                  version 3: HMAC(recipient's invite key, "pw-invite" || sender || recipient || everything above)
```

The invite key is 32 random bytes per node, carried in its invites (`Node::invite`, invite version 2). A node
sends version 3 to a peer whose invite it dialled, version 2 in a room, and version 1 otherwise (answers to a
peer that dialled it). The address list is filtered by the sender's [privacy mode](#privacy-modes).

The decoder rejects:
- an unknown version, flags other than 0 or 1, unknown roles or NAT types;
- n or m above 8;
- truncation and trailing bytes.

Addresses that fail to decode are skipped. Decoders from before port prediction read the prediction entry as
an address that fails to decode, so old and new nodes still talk; the prediction is just unused. A decoded
prediction with step 0 or above 8 is dropped.

The mapping entry works the same way. A `SymmetricIp` node sends `nat` 3 (symmetric) and the entry with 1; a
`Symmetric` one sends it with 0, saying it will do its part of a
[two-sided birthday punch](#two-sided-birthday-punch). Decoders from before NB-083 skip it, so they read
`SymmetricIp` as `Symmetric`; offers from them have no entry, which reads as `Symmetric` from a peer that won't do
its part. The entry counts only with `nat` 3 and only as exactly `[0xfe 0x02]` and 0 or 1, the first one if there
are more; anything else is skipped. Offers from NATs that aren't symmetric carry no entry, so they are as before,
byte for byte. With both entries, a full list keeps 6 addresses.

## Rules

- **Roles.** Peer↔Peer, Peer↔Host, Host↔Client. Clients never link to clients, and hosts never to hosts. Outside
  a room (an invite join), any role is accepted.
- **Which offers count.** A version-2 offer only in a room, with its proof checking out; a version-3 offer only
  with a proof under this node's own invite key; a version-1 offer only from a peer this node dialled itself with
  `connect` (its answer). Anything else, or a version 2 or 3 too short to hold a proof, is dropped unread.
- **Membership.** In a room, a node takes only version-2 offers whose proof checks out, plus the answers of peers
  it dialled, so invite dials still punch.
  - A peer whose hello or offer proved the key is a member in the role it proved.
  - `Node` closes, unseen, any link from a peer that is neither a member in a role it links with nor one it
    dialled. Messages and `AddrObserved` from such a peer never surface.
  - A client can pin its host (`Node::join_room_with(.., Some(host))`, Godot `join_room_host`). It then ignores
    every other member.
  - The Godot room client takes as its host only a peer that proved the `host` role.
- **Discovery.** On a proven hello from a peer it isn't linked to, a node sends an offer with `reply=1`. It sends
  at most one offer per peer per 2 s. Hellos repeat at 2, 4, 8, 16, then every 30 s, for peers that missed one.
- **On an offer.** These checks run in order, each one dropping the offer:
  1. the proof, or the dialled-peer check for version 1;
  2. the role;
  3. `|ts - now| <= 60 s`;
  4. a nonce already seen from that sender in the last 2 min;
  5. the sender's limit, and the node's offer budget unless this node dialled the sender.

  The nonce is stored only for an offer that passes all of them, and at most 1024 are kept. The node then dials
  every candidate that is *dialable* and not one of its own addresses, plus `Via(r)` for up to 2 listed peers it
  is itself connected to, its own [relay peers](#relay-peers) first. If already linked, it calls `add_candidates`
  instead (punching a relayed link). If `reply=1`, it answers with its own offer (`reply=0`). A relay-only node
  instead dials anyone but its relay as `Via(relay)` only (see below).
- **Dialable addresses.** Never port 0, unspecified, multicast or broadcast. Never loopback (including
  `::ffff:127.0.0.1`), unless the node is itself bound to loopback. The same filter applies to addresses peers
  report seeing (`AddrObserved`) and to STUN answers. An offer can't aim a node at services on its own host.
  Addresses of a family the socket can't send to are skipped too (`Traversal::reaches`). Offered and observed
  `::ffff:a.b.c.d` addresses count as `a.b.c.d`, so a mapped copy of the node's own LAN address is not dialled
  and an observed one is not kept twice. A NAT64 address (`64:ff9b::/96`) is judged by the IPv4 address it
  embeds: `64:ff9b::127.0.0.1` is loopback.
- **Invites.** `Node::connect` dials the invite's addresses, then sends the host a version-3 offer (proved with
  the invite's key) with `reply=1`, so the host dials back at the same time. An invite without a key (version 1)
  still dials, but gets no dial-back. A host that is announcing in a room turns away invite
  joiners: in a room, knowing the name is the admission.
- **New links.** When a link comes up, peers that offered to this node but are still unlinked get a fresh offer
  listing the new peer, which may be the relay they need.
- **Budgets.** Each budget is a burst of 30, then one per 2 s:
  - hellos answered;
  - offers acted on.

  On top of that, each sender's offers get a burst of 8, then one per second, so one key can't spend the node's
  offer budget alone. Proofs are checked before any token is taken, so forged hellos and offers cost nothing.
  Answers from peers this node dialled skip the node's offer budget (the sender's limit still applies), so
  others, even invite holders, can't crowd out the punch of a dial this node made. The
  limits bound what floods can make a node dial, seal or publish, and are ample for the room sizes this layer
  targets (no mesh limit yet).
- **Failure.** When a dial ends in `ConnectFailed`, `failure(peer)` is `Symmetric(side)` if one side is
  symmetric (`Symmetric` or `SymmetricIp`) and the other isn't open, and `Unreachable` otherwise. `side` is
  `Ours`, `Theirs` or `Both`. Its `Display` is a sentence for users, such as "the peer's NAT is symmetric (a new
  public port per destination) and this side's router only lets in packets from ports it has sent to, so hole
  punching can't work, and no peer linked to both could relay". Godot shows it as `get_connect_error_message()`.

## Relay peers

Two peers that can't punch (two symmetric NATs, as on many phone hotspots and carrier NATs, or a random-port
symmetric NAT against a port-restricted cone when the [birthday punch](#birthday-punching) misses) link through a
peer both are linked to. In a Godot star or a room of
two there is no such peer, so `Node::set_relay_peers(&[invite])` (or `Builder::relay_peers`, Godot
`set_relay_peers`) adds one: an always-on peer such as [`peerwire-peer`](../../crates/peerwire-peer), by its invite
(NB-041). Peerwire runs no TURN server: a relay peer is an ordinary Peerwire node that forwards sealed packets
with the core's peer relay. A Rust node can use a TURN server of your own instead, or as well (`PathKind::Turn`,
NB-099; see [choosing paths](../guides/connection-paths.md)).

- **The link.** `Node` dials each relay peer when the list is set and keeps the link. When it drops, or a dial
  fails, it dials again at once, then after 1 s, 2 s, 4 s, ... up to every 30 s; `next_timeout` wakes the host for
  it. The wait starts over at 1 s only once a link has held for 30 s, so a relay peer that hangs up on every link
  costs a handshake and an offer every 30 s, not every second. Setting a new list closes links to peers no longer
  on it.
- **Out of the app's sight.** Relay links raise no events and are not in `Node::peers`; `Node::relay_peers` shows
  them with their status. The Godot binding never takes a relay peer for a player. A relay peer is dialled from an
  invite, so a room admits it like any peer the node dialled.
- **Used first, direct still tried.** Offers name the node's linked relay peers first among its links (at most 8
  are named), and a dial tries the node's own relay peers first among the relays an offer names (up to 2). The
  direct candidates are dialled alongside, and the core moves a relayed link to a direct path when one starts
  working, as for any relayed link.
- **Not under `RelayOnly`**, which links through its own relay only: relay peers are not dialled then, and
  `Node::set_privacy` to `RelayOnly` closes the relay-peer links already up (not the one to its own relay, if that
  is a relay peer). Switching out of it dials them again.
- **Rate.** A relay peer forwards at most `Config::relay_rate` per requesting peer (`PEERWIRE_RELAY_RATE`,
  256 KiB/s by default) and `PEERWIRE_RELAY_TOTAL` (8 MiB/s in `peerwire-peer`) for all together. The per-peer
  budget is the sum over everything one player sends through it, to every peer: **theoretical**
  262144 B/s ÷ 60 ticks ≈ 4.3 KB per tick at 60 Hz. In the simulator two `sym-random` peers relay 1 KB unreliable
  every 10 ms each way (about 100 KB/s, 5 s) with every packet delivered and no relay drops:
  `cargo test -p peerwire-sim --release --test traversal symmetric_pair_links_through_a_relay_peer -- --nocapture`.
  A host relaying snapshots to many players can need more: raise `PEERWIRE_RELAY_RATE`.
- **Cost.** Latency `rtt(A,R) + rtt(R,B)`, so run the relay peer near the players, and its upload carries every
  relayed byte again.
- **What a relay peer learns**: each linked node's IP and port, the PeerIds of the pairs it relays between, and
  their packets' sizes and timing. It can't read or forge them (they are sealed end to end), but it can drop or
  delay them, and it can refuse to relay. A node names its relay peers in offers to room members and to peers it
  dialled, as it names any link. The offers a node sends a relay peer name none of its links, but they carry its
  addresses as its privacy mode allows, so that a relay on the LAN or loopback still links: under `Direct` the
  relay peer learns the node's **LAN addresses** too (`PublicOnly` hides them).

## Privacy modes

`Privacy` (`Builder::privacy`, `Node::set_privacy`, `Traversal::set_privacy`) decides which addresses a node
gives out. Changing it affects the next invites and offers; links already up keep their path.

- **`Direct`** (default): every candidate. The public one from STUN, ones peers observed, the LAN and loopback.
- **`PublicOnly`**: no loopback, RFC 1918, CGNAT (100.64/10), link-local, IPv6 ULA, unspecified, multicast or
  broadcast address, including private ones peers observed. Two peers on one LAN then link through their NAT
  if it hairpins, or through a relay.
- **`RelayOnly { via }`**: no addresses at all, except to the relay, which gets the public ones (it links
  directly, so it sees the address anyway). The relay is `via`, else the room's pinned host, else the first
  peer that proved the Host role and was dialled. That pick sticks while its dial or link lasts (a later Host, of
  any PeerId, is dialled through it, never directly) and is dropped when the dial fails, the link goes down or the
  node joins another room. Offers name only the relay as a possible relay, never the node's other links.
  - Without a pinned host, the relay is whoever answers first as Host, and anyone who knows the room name can do
    that and so learn the node's IP: pin the host (`join_room_with(.., Some(host))`, Godot `join_room_host`)
    when its key is known. The Godot binding pins `via` to the host a client adopts.
  - The node's invite names the relay with the relay's address: its holders learn the relay's IP.
  - Offers from anyone but the relay are dialled as `Via(relay)` only. If the relay isn't linked yet, the dial
    waits and happens when it links. `Node::connect` dials `Via(relay)` only, and fails with
    `ConnectError::NoRelay` while no relay is linked. It never falls back to a direct path.
  - `Node` closes, unseen, a new direct link from anyone but the relay.
  - The endpoint is relay-only (`Endpoint::set_relay_only`). Its relayed links take no candidates (from offers,
    direct packets or the relay's origin tags), never punch, never answer a punch and never move to a direct
    path. A dial through a relay alone (given the relay before any direct address of its own, or with a relay
    when relay-only is turned on) drops its direct targets and ignores an answer at the endpoint's own address. A
    direct dial later given a relay keeps its direct targets. A direct handshake is answered only from a peer it
    is itself dialling directly or already has a direct link with. It sends no OBSERVED frames or origin tags.
  - It sends its relayed packets in `FWD_QUIET` frames: the relay forwards them in `RELAYED` frames with no
    origin address. (`FWD` asks the relay to say where it sees the sender, so that the far end can punch.) A
    build older than `FWD_QUIET` takes frame type 11 as a protocol violation and drops the link. In the same way
    a build from before the urgent stream drops a link on its first `Urgent` message (frame types 12 and 13).
  - Cost: latency `rtt(A,R) + rtt(R,B)`, and the relay uploads every relayed byte again (capped by
    `Config::relay_rate`, 256 KiB/s per requesting peer by default, and `Config::relay_total_rate` for all peers
    together, unlimited by default).

Per-mode outcome in the simulator: a room with a public host and peers A and B over 36 pairs of NAT kinds (each
unordered pair once) plus two peers behind one non-hairpinning NAT, from `cargo test -p peerwire-sim --release --test
traversal privacy_modes_matrix -- --nocapture`:

```text
                            Direct: direct 34, relayed  3, failed 0
                        PublicOnly: direct 33, relayed  4, failed 0
           RelayOnly { via: None }: direct  0, relayed 37, failed 0
```

## What others learn or can do

- **Relays and anyone watching them** see:
  - room topics and each peer's inbox topic (a hash of its public key);
  - sealed hellos: their length, which tells native from browser and a client from a host or peer, not the PeerId;
  - source IPs at the relay, and message timing.

  They can't read hellos or offers, and addresses never leave sealed messages. Without the room name they can't
  join a room or pose as its host. They can replay a member's hello, which only makes members offer to that member
  again.
- **Knowing a PeerId is not enough** (NB-032, NB-033(b)). PeerIds appear in room hellos (to anyone with the
  room's name) and invites, so treat them as public. But a node acts on an offer, by dialling back and answering
  with its own, only if the offer proves the room key, proves its invite key, or comes from a peer it dialled.
  Whoever does get an answer learns:
  - the node's candidate addresses, as its privacy mode allows (in `Direct`, **LAN addresses** too);
  - **up to 8 PeerIds** it is linked to (only its relay, under `RelayOnly`).

  Only room members proved by the room key and peers the node dialled itself get both. Anyone else it answers,
  such as a stranger holding its invite, gets its **public addresses only and no PeerIds** (NB-033(b)), even
  after a dial to it failed, since a holder can make its own dial fail. Relay fallback still works: a sender names
  its own links in its offer, and the node dials it through whichever of them it is, or later becomes, linked to.
  A node with no public or observed address (LAN or loopback only, no STUN) answers such a holder with an empty
  address list, which is harmless: the holder dials the invite's addresses. The invite itself still carries LAN
  addresses in `Direct`: use `PublicOnly` to keep them out of both. The invite key is a capability: anyone holding
  the invite has it, and it doesn't rotate yet. So an invite works as a ticket to make the node dial back and link,
  and a PeerId does not.
- **Addresses a node dials or adopts** from offers, from peers' OBSERVED frames and from STUN are never port 0,
  unspecified, multicast, broadcast or link-local, and loopback only if the node is bound to loopback (NB-033(a)).
  An offer can't turn a node into a prober of its own host. It can still name LAN addresses, which the node dials
  with Noise handshakes: at most 8 per accepted offer.
- **Peers linked directly** see each other's source IP:port, whatever the mode; `Event::Connected { path }`,
  `PathChanged` and `Node::path` show it. Only `RelayOnly` avoids direct links. A relay sees both ends.
- **Replay.** A captured offer is dropped once it is older than 60 s or its nonce was seen. Within that window, a
  replay to a node that never saw it (for example, one that restarted) causes one more dial to the original
  sender's addresses.
- **Reflection.** Anyone who can get an offer acted on can make a node dial dialable addresses: at most 8 per
  offer, about 6 handshake datagrams each (around 100 B) over 30 s. That is roughly 4.8 KB per accepted offer,
  within the budgets above. An offer with a port prediction adds up to 32 guessed ports on the public IP it
  offered, probed 3 times each: about 9.6 KB more per accepted offer, to that IP. Only an offer with a new
  prediction adds probes; a repeated one adds none.
- **Birthday spray** (NB-044). An offer that says `Symmetric` with no prediction, to a node behind a cone, makes it
  spray up to 2048 ports of the offer's first address's IP: about 270 KB over 3.8 s, unless a probe links first.
  Only a room member or a peer the node dialled can get an offer acted on, the address must be public IPv4, and
  it happens once per peer per 5 minutes and at most 4 times at once per node, then once per 30 s. Per victim IP
  the cap is tighter (NB44-1): one spray at a time and at most 2 per 5 minutes across all of that node's peers, so
  a single address receives at most about 550 KB per 5 minutes **from one node** however many PeerIds that node
  names it with. The cap is per node, so N nodes recruited with one offer each still contribute that much apiece.
  The probes are Noise
  handshakes or sealed punches, about 130 bytes each (~270 KB for a full 2048-probe spray, ~270× the offer that
  triggered it — a reflection amplifier, which is why the per-IP cap bounds what reaches one victim). An offer
  saying `Cone` makes a symmetric node send 256 handshakes (about 33 KB) to the offer's address, from 256 sockets
  it keeps for 10 s.
- **Two-sided birthday punch** (NB-083). An offer that says `SymmetricIp`, to a symmetric node, or `Symmetric` with the
  mapping entry, to a `SymmetricIp` node, and that leaves out the prediction (or goes to a node without one; with both,
  it is the lockstep sweep below), makes it do one or both of the above: a `SymmetricIp` node facing a `Symmetric` offer
  opens 256 sockets with 256 handshakes; a `Symmetric` node facing a `SymmetricIp` offer sprays up to 1024 ports of the
  offer's IP (about 135 KB); two `SymmetricIp` nodes do both, with the full spray, about 300 KB, as one punch. All under
  the same limits and per-IP cap, so what lands on one address is as before, but two `SymmetricIp` nodes add the
  opener's 256 handshakes, about 34 KB (+12.5%, NB83-I1). New is what a node behind a NAT that maps per IP and port
  pays: before NB-083 it never sprayed, and each probe now costs its NAT a port, up to 1026 a try, held
  for the NAT's mapping timeout. That is still much of the 512 to 2048 ports a carrier NAT may give its user (NB44-3),
  so such a node sprays at most once per 5 minutes, whichever peers ask (`PER_PORT_PUNCH_GAP`), and opens no sockets for
  it (NB83-1): about 1030 ports at a time from this at most, beside the 256 a try that an offer saying `Cone` could
  already cost it under the node budget. A `SymmetricIp` node's spray costs its NAT one port, as every probe goes to one
  IP.
- **Lockstep sweep** (NB-045). An offer with a prediction, to a node behind a sequential symmetric NAT, makes it
  send up to 96 handshakes (about 12 KB) to ports of the offer's IP, under the same limits as the birthday spray.
- **Nostr relays** (`NostrSignaling`):
  - Events are checked against their NIP-01 id before deduplication, so a relay can't present one event as many.
    Signatures aren't checked: offers are sealed and authenticate themselves, and hellos carry room proofs.
  - Websocket messages and frames above 128 KiB close the connection.
  - Events waiting to be polled are capped at 1024; past that they are dropped.
  - `wss://` uses rustls with the webpki roots and hostname checks.
  - On `ws://` (a LAN or loopback relay), anyone on the path can drop or inject events. Offers stay sealed and
    hellos stay proven, so that costs availability, not membership.
- **Router port mapping** (opt-in) opens a public UDP port to the node's socket for as long as it lives; anyone
  who finds it reaches only the Noise endpoint. On the LAN:
  - PCP and NAT-PMP answers count only from the gateway's address, and PCP answers only with the request's random
    96-bit nonce, protocol and port;
  - an SSDP answer is used only if its description URL names the host that answered, by IPv4 literal, and that host
    isn't public. So a LAN responder can't send the node's HTTP requests to a third party;
  - control URLs on another host, and service types or paths with spaces, quotes or control characters, are
    refused, so nothing from the description can break out of a request line or header;
  - HTTP answers are capped at 64 KiB, and each exchange (connect, request and answer) at 3 s overall, so a
    gateway that drips its answer a byte at a time can't hold the mapping thread, or the delete on drop.

  Accepted residual risk: a rogue host on the LAN, or one spoofing another's address, can
  - receive the mapping requests, by answering SSDP before the router;
  - report a false mapped address. That adds a wrong candidate: the node's peers dial it, a few Noise initiations
    per peer per room, and no one can complete a handshake there. Any room member can already cause as much by
    listing an address in its own offer;
  - by spoofing an SSDP answer's source as another LAN host, make the node send that host's private `ip:port` one
    fixed GET for a path it chose, then SOAP POSTs if it answers like a gateway.
- **STUN servers** see the node's public address. A forged STUN answer needs the 96-bit transaction id and the
  server's source address.

## Dependencies

Router port mapping adds no crates: its HTTP, SSDP and XML handling is a few hand-written functions.

Crates added to `peerwire` by NAT traversal:
- `sha2` and `hmac`: RustCrypto, MIT/Apache-2.0.
- Under the `nostr` feature only: `tungstenite`, `rustls` (ring provider), `k256` and `serde_json` (MIT and/or
  Apache-2.0, ISC for ring).
- **`webpki-roots` 1 is CDLA-Permissive-2.0**, because it carries Mozilla's root store as data. It is a
  permissive data licence and needs no notice in binaries. Anyone redistributing the source should keep its
  licence file. Builds without `nostr` don't include it.

## Coverage

- `crates/peerwire/src/traversal.rs` unit tests:
  - hello and offer round trips (versions 1 to 3), hello timestamps, truncation, browser hellos, roles;
  - the hello seal: the same bytes as `nostr.js` for a fixed vector (also in `packages/js/test/rust_hello.mjs`), a
    fresh nonce each and no PeerId in the clear; plaintext, other-room, altered, cut or extended hellos dropped;
  - offer contents in each privacy mode, the public-address classifier, relay-only dials going only through the
    relay (and the room host as the relay), a relay-only client dialling exactly one of two Hosts directly
    (the relay sticks), stale hellos, strangers' plain offers ignored, dialled peers answered
    once the node budget is spent;
  - room proofs: wrong key, topic used as the key, wrong role, pinned host, invited peers;
  - sealed offers turning into dials, and replay and stale drops;
  - separate budgets under hello and offer floods, the per-sender limit, nonces kept only for offers acted on;
  - the dialable-address filter;
  - STUN classification over two, three and four servers (`SymmetricIp` from one IP's two ports, a silent server
    on either IP, lists at one IP only judged as before) and retransmits, port prediction from the ports handed
    out;
  - predictions round-tripping in offers, and cone (not open or symmetric) receivers probing them;
  - the mapping entry (NB-083): round trips, a full list, entries that don't count, and main's offer codec from
    before NB-083 (copied into the tests) reading `SymmetricIp` as `Symmetric` with everything else intact, while
    every offer without the entry is what it sent, byte for byte;
  - birthday punches planned from the NAT types: a spray of every probe, each port once, from a cone towards a
    random-port symmetric peer, sockets with the dial's HS1 the other way round; none towards a private address,
    open or symmetric peers, or under `RelayOnly`; once per peer, at most 4 at once;
  - punches with a `SymmetricIp` side: its sockets, and from a symmetric peer that maps per IP and port a spray of
    `PER_PORT_PROBES` and no sockets; both on each of two `SymmetricIp` nodes; each with and without predictions,
    lockstep instead when both predict, whether or not the peer says; none towards a peer that doesn't say and doesn't
    predict, between two `Symmetric` nodes, towards a private address or under `RelayOnly`; once per peer and once per
    IP; one spray per 5 minutes behind a NAT that maps per IP and port;
  - lockstep sweeps: the peer's predicted ports in order, `next + k·step` (96) from the lower PeerId and
    `next + 2k·step` (64) from the other, a step-3 NAT, none unless both sides predict;
  - failure messages naming the symmetric side.
- `crates/peerwire/src/node.rs` `stun_names_resolve_once_and_the_defaults_ask_one_server_on_two_ports`.
- `crates/peerwire/src/portmap.rs` unit tests: PCP mapping, renewal at half the lease and delete; NAT-PMP after a
  PCP version refusal; UPnP after PCP and NAT-PMP refusals, with port conflicts and permanent leases; the timeout
  schedule through every protocol and the 5-minute retry; a failed renewal starting over; CGNAT and private
  external addresses not advertised; SSDP answers pointing elsewhere refused; description parsing and chunked
  HTTP; a stop with a PCP or NAT-PMP map request unanswered deleting it. Mock PCP, NAT-PMP and UPnP gateways on
  loopback drive the real thread to a mapping, a renewal and the delete on drop, and a worker dropped before a
  slow gateway answers still sends the delete. `node.rs` checks a mapped address appears in `addresses()` and invites, and that a relay-only
  node asks the gateway nothing, deletes the mapping when switched to `RelayOnly` and maps again when switched
  out; `traversal.rs` where it ranks and which privacy modes give it out.
- `crates/peerwire/tests/room_security.rs`:
  - the security review's `sec_room_topic_only` repro, which no longer links;
  - links from non-members closed unseen;
  - members linking, and a pinned host.
- `crates/peerwire-core/src/fuzz.rs` `fuzz_birthday_spray`: a spray probes `min(count, MAX_SPRAY)` ports, each once,
  from 1024 up, at most 16 per batch, over random seeds, bases and counts.
- `crates/peerwire/src/node.rs` `birthday_sockets_carry_the_link_they_hear`: over loopback, a peer that reaches one
  extra socket links through it, every reply leaves from it, and the rest close.
  `birthday_sockets_ignore_unauthenticated_floods` and `birthday_sockets_take_only_their_peer_authenticated`: junk,
  an HS2 of junk from the linked peer's own address, and another peer's handshake make no route, and the sockets
  close at `until`. `birthday_sockets_keep_one_route_per_peer`: a peer sending from one port after another to
  another socket each time keeps one route, beside another peer's.
  `birthday_sockets_route_a_copied_hs1_only_until_until` and
  `birthday_sockets_keep_the_route_of_a_copied_hs1_from_the_peer`: a copy of the peer's HS1, sent once the link is
  up from another host or from the peer's own address, is answered, but the peer's route stays and the socket the
  copy reached closes at `until` (NB85-1).
- `crates/peerwire-sim/src/nat.rs` `mapping_and_filtering_per_kind`: each kind's mapping (the port towards another
  port of one IP, and towards another IP) and filtering (which of those a mapping lets in), as in the table in
  [two symmetric NATs](#two-symmetric-nats).
- `crates/peerwire-sim/tests/traversal.rs`:
  - NAT types from simulated STUN (the first server on two ports, as in `DEFAULT_STUN_SERVERS`), and which kinds
    predict;
  - the room-of-two matrix over 12 NAT kinds with 150 ms signaling latency, each failure naming the right side, and
    every pair linking unless both NATs are symmetric, not both sequential and neither `sym-ip-adf` (bar the case in
    lockstep's limits);
  - the NB-080 kinds against cones and public peers, and `sym-seq-adf` against sequential NATs, over 25 seeds each
    way round: all direct but `sym-seq-adf` sweeping at stride 2 against a sequential NAT that filters by port
    (`cargo test -p peerwire-sim --release --test traversal address_filtering -- --nocapture`);
  - the `sym-ip` kinds against every symmetric kind, with each side's NAT mappings a try, and the `ipseq` kinds,
    which sweep in lockstep with a peer that predicts and link 50/50 as before
    ([two-sided birthday punch](#two-sided-birthday-punch));
  - a punch for two NATs that map per IP and port in which both sides open sockets and probe, driven from the test
    ([considered, not built](#two-symmetric-nats));
  - sequential symmetric NATs against port-restricted ones, over 10 seeds each way round: 80/80 direct;
  - a symmetric pair relayed through a third room peer;
  - birthday punching (`Sim::open_ports`: extra sockets with routes, as `Node` keeps them): random-port symmetric
    against port-restricted and `cgnat(pr+pr)` cones, 100/100 direct over 25 seeds each way round, connect
    median 2.25 s, max 4.07 s (`cargo test -p peerwire-sim --release --test traversal birthday -- --nocapture`);
    a link relayed through a relay peer moves to direct in 1.56 to 2.96 s, and only the routed socket stays open;
  - lockstep prediction: `sym-seq` and `cgnat(pr+sym)` pairs, 60/60 direct over 10 seeds each way round (1.51 s),
    also with up to 28 ports used up on both NATs before the sweep (`room_pair_with`), and still failing at 60;
    a sequential pair relayed through a relay peer moves to direct in 1.1 s
    (`cargo test -p peerwire-sim --release --test traversal lockstep -- --nocapture`);
  - IPv6 (`Sim::add_ipv6`: a global address behind a stateful firewall): a pair behind random-port symmetric NATs
    links directly over IPv6, and with IPv6 on one side only fails as before; port-restricted and full-cone peers
    offer both families and link on the first to answer;
  - an invite join that punches only with signaling;
  - the per-mode matrix above.
- `crates/peerwire/tests/privacy.rs`: invites per mode and switched at runtime, `NoRelay`, relay-only room peers
  linking `Via(host)` with every event and every sealed offer checked for addresses, relay-only invites.
- `crates/peerwire-sim/tests/nat.rs` `guessed_ports_punch_a_sequential_symmetric_nat`: guesses punch from a
  dial and from a relayed link; a relay-only link ignores them.
- `crates/peerwire-sim/tests/nat.rs` `relay_only_links_stay_relayed`: no packet between the two ends, either side
  dialling, and no punch.
- `crates/peerwire/tests/dual_stack.rs`, over 127.0.0.1 and ::1: a `[::]` node links an IPv4 and an IPv6 peer on
  one socket, each seen at its plain address (no `::ffff:`); IPv4 and `::1` sockets skip the family they can't
  reach (`NoPath`); two dual-stack nodes link on the first answer; STUN goes to the IPv4 server only.
  `node.rs` checks which local addresses each binding lists.
- `crates/peerwire/tests/nat_loopback.rs` (mock STUN servers, `MemoryHub`; an invite without its key gets no
  dial-back) and `crates/peerwire/tests/nostr.rs` (local
  relay; `--ignored`: public relays and STUN, run by `scripts/gate.sh --nostr`).

Room of two, signaling only, from `cargo test -p peerwire-sim --release --test traversal -- --nocapture`. D is direct,
with ms to connect. F S is a failure reported as `Symmetric` on both sides, as A reports it: `S<` A's NAT is the
symmetric one, `S^` B's, `S2` both. For the first eight kinds, port prediction raised this from 43/64 direct, birthday
punching from 51/64 (the four `sym-random` against port-restricted cells, at 2 to 2.4 s), lockstep prediction from 55/64
(the four sequential symmetric pairs, at 1.5 s); what still fails is a random-port symmetric NAT against another
symmetric one, which [no punch links](#two-symmetric-nats). The last four kinds (NB-080) link with every cone and public
peer, and `sym-seq-adf` with the sequential kinds, but not in this seed as A against `sym-seq` (see lockstep's limits).
`sym-ip-adf` links with every symmetric kind too (NB-083, its sockets taking the other side's probes, see
[two-sided birthday punch](#two-sided-birthday-punch)): the 13 cells of its row and column that failed before. The third
STUN query moved the birthday cells' times, as it shifts the simulator's random draws; no other cell changed:

```text
         A \ B        public     full-cone    restricted    port-restr       sym-seq    sym-random  cgnat(pr+pr) cgnat(pr+sym)   sym-seq-adf  sym-rand-adf    sym-ip-adf   sym-ip-apdf
        public         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360
     full-cone         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360         D 360
    restricted         D 510         D 510         D 510         D 530         D 530         D 860         D 510         D 530         D 530         D 880         D 860         D 860
    port-restr         D 510         D 510         D 510         D 530         D 510        D 2420         D 530         D 530         D 530        D 2240        D 2430        D 2370
       sym-seq         D 510         D 530         D 510         D 510        D 1510          F S2         D 510        D 1510        D 1510          F S2        D 2010          F S2
    sym-random         D 510         D 530         D 510        D 2010          F S2          F S2        D 2040          F S2          F S2          F S2        D 2630          F S2
  cgnat(pr+pr)         D 530         D 510         D 530         D 530         D 530        D 2430         D 530         D 530         D 530        D 2120        D 2030        D 3330
 cgnat(pr+sym)         D 510         D 530         D 530         D 530        D 1510          F S2         D 530        D 1510        D 1510          F S2        D 2640          F S2
   sym-seq-adf         D 530         D 530         D 530         D 510          F S2          F S2         D 530        D 1510        D 1380          F S2        D 3560          F S2
  sym-rand-adf         D 530         D 530         D 510        D 1980          F S2          F S2        D 2000          F S2          F S2          F S2        D 2180          F S2
    sym-ip-adf         D 510         D 530         D 510        D 2150        D 2000        D 2130        D 2700        D 2970        D 2100        D 2120        D 2070        D 1970
   sym-ip-apdf         D 530         D 510         D 510        D 2340          F S2          F S2        D 2120          F S2          F S2          F S2        D 2070          F S2
direct: 59/64 of the first 8 kinds, 116/144 of all 12
```

The simulated NATs follow RFC 4787 behaviours. Real carrier-grade NATs, mobile networks and firewalls that block
UDP have not been tested.
