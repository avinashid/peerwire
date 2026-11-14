# Changelog

Notable changes to Peerwire's packages. The npm packages and the Rust crates follow [semantic
versioning](https://semver.org); before 1.0, a minor version may break the API.

Every security finding behind a fix below is in the [threat model](docs/project/threat-model.md), with its severity
and the commit that fixed it.

## Unreleased: Rust crates

- **Bluetooth LE** (NB-110, [Bluetooth](docs/concepts/bluetooth.md)): `peerwire` feature `ble` (its one dependency
  `log`) gives a node a radio, `Builder::bluetooth`: in a room it advertises, links with the members it hears with no
  network, and ranks the radio below every UDP path (`PathKind::Bluetooth`, not in `Policy::default()`, off under
  `Privacy::RelayOnly`); a link that loses UDP comes back over the radio, and moves to UDP again once it works.
  Feature `ble-bluez` adds the Linux driver, `ble::BlueZ`, which tries again what fails and follows the default
  adapter, and reports a link that drops at once (NB-114). It holds its scan off while it connects, 3 s at a time
  between runs of 2 s: with the scan on, Linux started a connect only at the scan's next report of the device, 10 s
  on, and the connect failed (NB-116). The node and the driver log each connection (at `debug`, and failures at
  `info`), devices named by a hash, never an address. Drivers implement `ble::Radio`; the ESP32's is in
  `peerwire-esp`.
- `ble::BlueZ` keeps to its own links: it removes from BlueZ the devices it connected to that BlueZ didn't know
  before, uses no device that lists HID, and closes only the links it asked BlueZ to make, at once if their device
  lists HID later (NB110-13, NB110-14, NB110-16). Unless `/etc/bluetooth/input.conf` sets `LEAutoSecurity` false,
  BlueZ asks to pair with a device that serves HID, on the driver's links too, and the driver says so as it starts
  (at `info`). A central connect waits its turn at most `SETUP` from asking, reports a failure before the next connect
  may start, and hands the turn on only once the scan has resumed (NB116-2, NB116-3, NB116-4).
- `peerwire-esp`: feature `ble`, the ESP32's driver, `ble::Nimble`, on ESP-IDF's NimBLE (ESP32-C3 and ESP32): three
  links, a new non-resolvable random address with each advert, and no bonds (a pairing hands over no keys, and its
  connection is closed). The `ble` example is a room member that sets its clock over Wi-Fi, then links over
  Bluetooth alone (NB-110c).
- `peerwire-esp`: the `ble_led` example, a room member whose LED a peer in the room switches over Bluetooth
  (`led on`, `led off`, `led toggle`, `led` asks) (NB-125).
- `peerwire-core`: `ble`, the framing codec every host shares (`Chunker`, `Reassembler`, the service's UUIDs);
  `Endpoint::handle_datagram_unbound`, for a channel no peer is bound to yet; `Address::radio`.
- `peerwire-ffi`: feature `ble`, the Bluetooth protocol of one connection for a host whose own stack moves the GATT
  values, such as Web Bluetooth (`pw_ble_new`, `pw_ble_push`, `pw_ble_poll_value`, `pw_ble_poll_event`,
  `pw_ble_signal`, `pw_ble_timeout` and the rest); the wasm build puts it in a module of its own,
  `peerwire_ffi_ble.wasm` (NB-110d). A board with no clock takes the time from a browser's room hello over it, as from
  a native peer's (NB-119). The connection's event queue keeps a `CLOSED` past its limit, and `pw_ble_timeout`'s
  deadline can't wrap or go negative (NB110d-I2).
- **Breaking:** `peerwire::Hop` has a new variant, `Ble`, and is `#[non_exhaustive]` from now on, so a `match` on it
  outside the crate needs a `_` arm (the next release of `peerwire` is 0.2; `peerwire-peer` and `peerwire-esp` move
  with it). `Traversal::handle_signal` returns the member a hello or an offer proved (it returned nothing), and
  `Traversal::radio_reaches`, `Traversal::forget_radio` and `Traversal::room_hello` are new, and so are
  `Node::set_waker` (feature `ble`) and `AsyncNode` waking on its radio.
- **Fix,** `peerwire-esp`: a board whose first NTP server didn't answer within 2 s could hang at boot, with no
  warning and before its node started. The clock's last wait could be under 1 ms, which lwIP takes as no timeout
  (NB-117).
- `peerwire-esp`: a board joining Wi-Fi no longer logs the network's name or its access points' addresses, which
  would tell where it is.
- `peerwire-esp`: a board keeps its identity across restarts (`identity::load_or_create`, a new one with
  `identity::renew`), drawn on the chip from the SAR ADC before any radio is set up, and `console::Console` reads
  commands from its serial console, the USB one on an ESP32-C3 (NB-119). The ESP32's Bluetooth runs without modem
  sleep.
- **No Wi-Fi needed over Bluetooth** (NB-119): a node bound with its clock unset (before 2023-11-14, as on a board
  with no battery for it) takes the time from the first room hello that comes over a Bluetooth connection which
  proved the room, then says hello at that time (not one in 2100 or later); `Node::clock_from_peer` gives it.
  `peerwire-esp`: `clock::set` sets the board's clock from it for one boot (`init` sets it back to 1970 at the next
  unless SNTP set the clock since), a late SNTP answer more than 30 s off the clock restarts the board, whoever set
  it, `wifi::join_within` and `Board::connect_within` give up on a network that isn't there, and the `ble` and
  `ble_led` examples try a saved network for 20 s, then go on over Bluetooth alone.
  `Traversal::handle_radio_signal`, `Traversal::clock_from_peer` and `traversal::CLOCK_SET_AFTER_MS` are new.
- **Links over the relays** (NB-126): a node takes a link over its signaling relays, a pipe
  (`PathKind::Signaling`), as a browser carries one, so a page, which can't send UDP, links with a Rust node or a
  board from anywhere. A node with no policy takes one, and one with a policy only if it lists `PathKind::Signaling`
  (`Policy::default()` doesn't). It takes a pipe only from a peer whose room hello proved the room. `peerwire-esp`:
  the `nostr_led` example, the board's LED from anywhere over `NOSTR_RELAY`. `room_chat`: `PW_RELAYS` replaces the
  default relays. **Fix:** while batches waited for the pipes' rate, `Node::next_timeout` was 0, so a loop sleeping
  on it spun (NB126-3).
- `peerwire-esp`: `NOSTR_RELAY` is `wss://relay.primal.net`, and the `nostr_led` example joins its room over it as a
  `Role::Host`, so the LED page, a client, links with the board, and two phones' pages don't link with each other
  (NB-129). nostr.mom, the relay before, took the board's and the pages' signalling events and passed none on.
- **Fix,** `peerwire`: `nostr::DEFAULT_RELAYS` (and Godot's default relays) are `wss://relay.primal.net`,
  `wss://relay.nostr.net` and `wss://nostr.oxtr.dev`, three operators. nos.lol and nostr.mom, two of the three before,
  now drop our events with no `OK` and pass none on, so members who shared only those never found each other
  (NB-130). A node on the old list still meets one on the new over relay.primal.net.
- **Fix,** `peerwire`: a node's heap no longer grows with every new peer that says hello (NB-132). Each page load is a
  new PeerId, and a node kept each one heard for 10 minutes, with no cap on how many: 330 after 10 minutes of hellos
  at the rate it answers, enough to run an ESP32 out of heap. It now keeps at most 4 for each of
  `Config::max_links` (32 to 1024; 32 on an ESP32), the least lately active first to go, never a linked or invited one.
  `NostrSignaling` keeps the ids it has seen as 16 bytes, not two 64-character strings (~6 KB rather than ~25 KB for
  an ESP32's 128), and a relay pipe whose peer left is closed after 60 s idle. `peerwire-core`: `Endpoint::config`.

## Unreleased: `@peerwire/js`

- **Bluetooth in the browser** (NB-110d, [Bluetooth](docs/concepts/bluetooth.md)): `BluetoothTransport`, a Web
  Bluetooth central (Chrome and Edge, Android Chrome) that links a page with a native room member nearby, such as a
  board running `ble_led`, with no network. The protocol is the wasm's: the module with Bluetooth,
  `peerwire_ffi_ble.wasm` (`BLUETOOTH_WASM`), with `BleLink`. The `led.html` demo switches a board's LED.
  After Bluetooth went off and on, Android Chrome hands back a dead connection the page can't clear: `dial` then
  fails at once with `'stale'`, and reloading the page clears it (NB-127).
- **Native peers over the relays** (NB-126, [RelayTransport](docs/reference/javascript.md#relaytransport)):
  `RelayTransport` links a page with a native room member (a Rust node, a board on Wi-Fi) over the room's Nostr relays
  alone, from anywhere: it says hello as a native peer does, lists the native peers whose hellos prove the room key, and
  dials one over the signalling pipe, with batches of 1232 bytes of datagrams at most (`NATIVE_BATCH`), which a
  board's 4 KB relay frame takes. `WebRtcTransport` has a `pipeBatch` option for it. The `led.html` demo switches a
  board's LED over Bluetooth or over the internet.
  Its `role` option (`'peer'`, the default, `'host'` or `'client'`, as Rust's `Role`) is the role its hello says, and
  it lists only the native peers that role may link with. The `led.html` demo joins as a client, so a second phone's
  page links with the board, not with the first phone's page (NB-129).
- **Fix:** `DEFAULT_RELAYS` are `wss://relay.primal.net`, `wss://relay.nostr.net` and `wss://nostr.oxtr.dev`, three
  operators. nos.lol and nostr.mom, two of the three before, now drop our events (signalling and `openInbox` drops
  alike) with no `OK` and pass none on (NB-130). A page on 0.3 still meets one on the new list over relay.primal.net.

## `@peerwire/js` 0.3.1 (2026-11-05)

- **The doctor:** `npx @peerwire/js doctor` (a `bin` in `@peerwire/js`) and the Rust example `doctor`, which
  replaces `natcheck`, report what a network does to peer-to-peer traffic: NAT mapping over IPv4 and IPv6,
  filtering (RFC 5780, `--stun5780`), router port mapping (Rust) and whether each Nostr relay and the LAN carrier
  (Rust) work, as text or `--json`, with `--redact` to mask public addresses. Nothing is uploaded. The doctor's
  `room` run reads `DOCTOR_PORT`, as natcheck's read `NATCHECK_PORT`. Results from real networks go in
  [Connectivity in the field](docs/project/field-connectivity.md).
- The wasm is rebuilt from main, with the same exports as 0.3.0 (356,902 B). The Rust doctor runs from a clone:
  `cargo run -p peerwire --features nostr --example doctor`.

## peerwire-esp 0.1.1 (2026-11-05)

On crates.io; the ESP32 template's `peerwire-esp = "0.1"` takes it in a new project.

- **A clock set late:** a board whose clock no server set in the 60 s before its node bound restarts once a later
  ask sets it, so the node binds again with the time; before, the node kept 1970 and PCs dropped its hellos until the
  board was reset (NB103-I2).
- **Times before 1970 refused:** an SNTP answer giving a time in 1968–1969 is refused; it wrapped the clock's
  arithmetic, a panic in a debug build (NB103-I1).

## Rust crates 0.1.0 (2026-11-05)

The Godot extension, built from source, has these changes too.

- **On crates.io:** the first release of the Rust crates, 0.1.0: `peerwire`, `peerwire-core`, `peerwire-sync`,
  `peerwire-game`, `peerwire-sim`, `peerwire-ffi`, `peerwire-peer` and `peerwire-esp`. `cargo add peerwire`, and
  `cargo install peerwire-peer` for the always-on peer. They need Rust 1.88 or later. A node built from git before
  29 October 2026 doesn't find a 0.1.0 node in a room, as room hellos are sealed now (Security, below).
- **Connection policy:** `Policy` orders the paths links take (`PathKind::Direct`, `Birthday`, `Peers`,
  `RelayPeers`) and turns the rest off: `Builder::policy`, and `Node::set_policy` live, where a link on a path turned
  off moves or closes with `DisconnectReason::PathOff`. `Node::set_stun_servers` and `Node::set_signaling` swap
  servers live, links kept. No policy behaves as before. `PathKind` is `#[non_exhaustive]`, and code that matches on
  `DisconnectReason` needs the new variant. See [choosing paths](docs/guides/connection-paths.md).
- **TURN:** a TURN client over UDP (RFC 8656; feature `turn`, on by default): `Builder::turn_servers` and
  `Node::set_turn_servers` (live) with a `TurnServer` (`new` for a username and password, `rest` for credentials
  minted from a shared secret), and `PathKind::Turn` in the policy, which alone turns it on. A node offers its
  relayed address in an entry older nodes skip, dials through its allocation, and moves a TURN link to a direct path
  ranked earlier once a punch answers. `Node::turn_status` shows the allocations, `Node::path_kind` the kind of path
  a link takes. No policy behaves as before.
- **TURN under `Privacy::RelayOnly`:** with `Turn` in the policy, a relay-only node links through its own allocations
  as well as its relay, so it needs no relay: offers to peers but the relay carry the relayed address alone, dials
  take `Turn` and the relay in the policy's order, and a link whose allocation goes moves to the relay or closes with
  `PathOff`. Turning relay-only on moves links that show the node's address onto an allocation or the relay at once,
  or closes them unannounced, as it does relay peers' links: from then on nothing to a peer but the relay leaves the
  node's socket, except from a dial already under way that neither an allocation nor the relay can take. That one
  runs on to its end, at addresses the peer had from earlier offers, and a link it makes is refused. Without `Turn`
  in the policy, relay-only is as before.
- **Changed:** `Node::path` returns an owned `Option<Path>` (it was `Option<&Path>`): a link through TURN shows the
  peer's address as relayed, and `Node::path_kind` says `Turn`. Drop a `.cloned()`, or compare with `Some(path)`.
  `peerwire_core::Address` has more provided methods (`relayed`, `turn_hop`, `turn_parts`, `own_turn`,
  `relayed_hop`), and `Endpoint` has `set_keep_turn`, `retag`, `peer_of` and `abandon` (`disconnect` with no Close
  sent). `Traversal::set_privacy` takes the time first, as `set_policy` does: under relay-only with TURN on it moves
  links at once.
- **LAN discovery:** `LanSignaling` runs rooms over UDP multicast and broadcast on the local network, with no
  internet. Broadcast, to the LAN's broadcast address only, reaches devices that a router never gives multicast to,
  such as an ESP32 on Wi-Fi.
- **ESP32:** `peerwire` builds for ESP-IDF; a prototype runs on emulated ESP32-C3s and ESP32s, and links over Wi-Fi
  with a PC from an ESP32-C3 board.
- **`peerwire-esp`:** the ESP32 port as a crate. `Board::connect` joins Wi-Fi, sets the clock by SNTP before the node
  binds and keeps it in step; `Board::node` binds a node sized for the chip with LAN signaling, and
  `Board::node_with_nostr` (feature `nostr`) adds a Nostr relay. The clock no longer comes from lwIP's SNTP, which
  took any answer that reached its port: its own asks must be echoed, and unsynchronised servers are refused.
  `cargo generate --git https://github.com/avinashid/peerwire templates/esp32` starts a project for the ESP32-C3 or
  ESP32. See [ESP32](docs/guides/esp32.md).
- **Shared documents:** `peerwire-sync` gives shared documents in Rust, wire-compatible with `room.doc()`, in the
  Godot binding (`open_doc`) and the C ABI.
- **Harder NATs:**
  - birthday punching and lockstep prediction, so 59 of 64 simulated NAT pairings connect directly;
  - `NatType::SymmetricIp`: STUN asks Google's server on a second port too, which tells a symmetric NAT that maps one
    port per destination IP; against another symmetric NAT it opens sockets and the other side probes them, at most 1024
    probes from a NAT that maps per IP and port (two `SymmetricIp` sides both do both, and two that predict their ports
    keep lockstep prediction), so 116 of 144 simulated pairings connect directly. Godot's `get_nat_type()` can return
    `symmetric-ip`, and code that matches on `NatType` needs the new variant;
  - dual-stack IPv4 and IPv6 candidates;
  - relay peers: an always-on `peerwire-peer` kept linked and used when a punch fails.
- **Faster paths:** a link races its other paths and takes one that's clearly faster.
- **Bulk transfer:** a loss at the end of a burst is repaired as a tail loss (RACK-TLP), not a timeout: 22% faster
  at 1% loss and 100 ms. The receive window is 4096 fragments, so `reliable_window` can go up to 4096 on long, fast
  links; a peer can now make a link hold about 5.7 MB (2.3 MB before).
- **Security:**
  - room hellos are sealed as in JavaScript;
  - handshake cookies under load, and a per-address cap on half-open handshakes;
  - an HS1 must be exactly the right length;
  - incoming offers spend a budget before they're decrypted;
  - a birthday punch's extra sockets take a route only from their own peer's authenticated packets.
- **API changes:** every `pw_*` function refuses a null handle, a null output, or a null input with a length over 0
  (`PW_ERR_INVALID` where it returns a code), and `pw_secret` and `pw_stats` now return `int32_t` (see the
  [C ABI](docs/reference/c-abi.md)); `Endpoint::handle_datagram_direct` returns `Option<(PeerId, bool)>`, the bool
  saying the packet was fresh.
- **Godot:** a hub catches up a stalled member afresh instead of disconnecting it; getters for each link's route and
  address.

## `@peerwire/js` 0.3.0 (2026-11-04)

**Added**

- **Connection policy:** `joinRoom({ policy })` and `room.setPolicy()` choose which paths a link may take
  (`'direct'`, `'turn'`, `'peers'`, `'signaling'`) and in what order, with your own STUN and TURN servers
  (`iceServers`) and Nostr relays (`relays`). Each can change live: links on a path turned off move or drop with
  `'peer-leave'` reason `'policy'`, new servers reach open connections, and a relay swap keeps links. With
  `order: ['turn']`, WebRTC offers relay candidates alone, so members never see your addresses. `room.policy` shows
  the policy, with `iceServers` as urls only. `fallback` still works, as shorthand for the order. See
  [choosing paths](docs/guides/connection-paths.md).
- `room.route(peer)` says `turn: true` for a link whose end runs through a TURN server.

**Fixed**

- A room that left while its hello was being signed no longer posts it on the closed channel (Node 22 threw).

## `@peerwire/js` 0.2.2 (2026-11-03)

**Added**

- **Birthday punching in rooms:** a peer reached by fallback, when one of you is behind a cone NAT and the other
  behind a symmetric one, gets one birthday punch, and the channel it makes takes the link over. One at a time per
  page, at most 2 at any one IP per 5 minutes, once per peer; `birthday: false` turns it off, as Net Lab's global
  room does.

**Changed**

- A loss at the end of a burst of reliable messages is repaired as a tail loss, not a timeout, so bulk transfers
  over lossy routes finish sooner. The receive window is 4096 fragments, so a peer can make a link hold about 5.7 MB
  (2.3 MB before).

**Fixed**

- A peer's port prediction counts only at a public IPv4 address that its own candidates name, and a link adds at
  most 128 predicted candidates.

## `@peerwire/js` 0.2.1 (2026-11-03)

**Fixed**

- `persist`: a save on a connection the browser has just closed, as it does while it clears the site's data, opens
  the store again, once, instead of failing.
- `persist`: an open the browser aborts is tried once more a moment later; an open it refuses still fails at once.

## `@peerwire/js` 0.2.0 (2026-11-01)

**Added**

- **Shared documents:** `room.doc(name)` gives maps, sets and logs that every member holds and that converge.
  - Late joiners catch up, in pieces sized for slow routes.
  - `persist` keeps an encrypted copy on the device; a failing store backs off up to 64 s.
  - `relays: true` keeps a sealed copy on the Nostr relays for members who join while nobody else is online.
- **Calls:** `room.call()` gives audio and video between room members, as a mesh of up to 16 (6 by default),
  signalled over the room's own links.
- **Offline messages:** `openInbox()` sends messages to one PeerId, sealed and left on Nostr relays until it comes
  online.
- **Fallback routes** for peers WebRTC can't link:
  - through another member linked to both, then over the signaling relays themselves (`fallback`, `room.route(peer)`);
  - the room keeps retrying WebRTC while a link runs on a fallback (`room.retryDirect()`);
  - it moves to a member that links to both as soon as one does.
- **Harder NATs in the browser:** ICE restarts, port prediction and birthday punching.
- **Demos:** Net Lab (what your network allows, link by link), a chat with invite links and QR codes, and a meeting
  room of up to 6.

**Changed**

- Room hellos are sealed with AES-256-GCM under a key from the room name, so a relay can't read the PeerIds in a room.
- Default Nostr relays are now `wss://nos.lol`, `wss://relay.primal.net` and `wss://nostr.mom`.
- Nostr signaling:
  - survives a network switch;
  - paces its sends and spreads batches across relays;
  - redials a relay that goes silent, backing off after repeated failures.
- `loadPeerwire` fetches the wasm by URL in browsers, so bundlers' asset URLs (Vite's `?url`) work.

**Fixed**

- A message a `peer-join` handler sends to the newcomer now reaches it.
- `room.send` throws `RangeError` for a message over its lane's limit, even while the outbox is full.
- `'public-only'` strips local addresses from every SDP form browsers produce.
- Many hardening fixes from security reviews, among them bounds on what members can make a peer store or compute.

## `@peerwire/react` 0.1.1 (2026-11-01)

- A shared write that doesn't fit what members can store is dropped with a warning instead of throwing.
- Catch-up messages are cut to 512 KiB, so they always fit a reliable message.
- A member's value nested more than 64 levels deep is dropped before it's stringified.
- `usePeerwireRoom` fails the join with an error if the presence changes while it's joining.

## `@peerwire/react` 0.1.0 (2026-10-24)

First release. React hooks over `@peerwire/js`, in TypeScript: `RoomProvider`, `useRoom`, `usePeers`, `usePresence`,
`useMessages`, `useRoomEvent`, `useSharedState`, `usePeerState` and `usePeerwireRoom`.

## `@peerwire/js` 0.1.1 (2026-10-23)

**Fixed**

- A room's presence waits for the link's own room proof, so a join under load is no longer one-sided (one member
  listing the other but not the reverse).

## `@peerwire/js` 0.1.0 (2026-10-21)

First release.

- The Peerwire core as a ~320 KB WebAssembly module with no imports.
- `joinRoom`:
  - WebRTC DataChannels, with Nostr or BroadcastChannel signaling;
  - presence, and the reliable, unreliable and urgent lanes;
  - closed rooms proved with a key from the room name;
  - a pull-style API for game engines.
- Privacy modes for the transport: `'direct'`, `'public-only'` and `'relay-only'`.
- The lower layers: `loadPeerwire`, `WebRtcTransport` and `NostrSignaling`.
- TypeScript types.
