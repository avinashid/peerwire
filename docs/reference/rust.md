# Rust crates

Peerwire's Rust side is one Cargo workspace: seven crates and a Godot binding. Most applications need only `peerwire`,
a `Node` on one UDP socket with STUN, rooms, signaling and NAT traversal, built on the sans-IO `peerwire-core`. This
page lists the crates, shows how to depend on them, and gives the public API of `peerwire`, with a short section on
each of the others. How the parts fit: [architecture](../concepts/architecture.md).

## Crates

Every crate has the workspace version 0.1.0, edition 2021 and the license `MIT OR Apache-2.0` (root `Cargo.toml`).
None is on crates.io yet.

| Package | Path | What it is | Published |
|---|---|---|---|
| `peerwire` | `crates/peerwire` | Peer-to-peer nodes over UDP with no server: STUN, rooms, signaling and NAT traversal on `peerwire-core`. Start here | Not yet |
| `peerwire-core` | `crates/peerwire-core` | Sans-IO session core: identity, Noise IK links, reliable, urgent and unreliable lanes, congestion control, peer relay, hole punching, invites | Not yet |
| `peerwire-sync` | `crates/peerwire-sync` | Shared documents (maps, sets, logs) that every member holds and that converge, in the frames of `room.doc()` in `@peerwire/js` | Not yet |
| `peerwire-game` | `crates/peerwire-game` | Deterministic fixed-point maths, Hilbert-curve islands and desync detection for multiplayer games | Not yet |
| `peerwire-sim` | `crates/peerwire-sim` | Deterministic network simulator: real endpoints over simulated latency, loss, uplinks and NATs, in virtual time | Not yet |
| `peerwire-peer` | `crates/peerwire-peer` | An optional always-on peer, a binary: keeps one identity across restarts and relays for the peers linked to it | No (`publish = false`) |
| `peerwire-ffi` | `crates/peerwire-ffi` | C ABI (`pw_*`): the zero-import wasm module for browsers, and `cdylib` / `staticlib` for native hosts | Not yet |
| `peerwire-godot` | `bindings/godot` | Godot 4 GDExtension: `PeerwireMultiplayerPeer`, Godot's high-level multiplayer over Peerwire | No (`publish = false`) |

"Not yet": the manifest doesn't forbid publishing, but no release is on crates.io.

## Depending on a crate

Until there is a release, depend on the git repository. Cargo finds each package by name in the workspace:

```toml
[dependencies]
peerwire = { git = "https://github.com/avinashid/peerwire", features = ["tokio", "nostr"] }
```

Add `rev = "<commit>"` to pin a commit. Take every Peerwire crate from the same source and `rev`, so they share one
`peerwire-core` and its types (`PeerId`, `Instant`) are the same types in each.

`AsyncNode` needs a tokio runtime of your own. The examples' tokio dependency (a dev-dependency in
`crates/peerwire/Cargo.toml`) is:

```toml
tokio = { version = "1", default-features = false, features = ["rt", "macros", "time"] }
```

## Features

`peerwire` turns no feature on by default (its `[features]` has no `default`).

| Feature | Adds | Pulls in |
|---|---|---|
| `tokio` | [`AsyncNode`](#asyncnode) | `tokio` 1 with `net`, `time`, `macros`, `rt` |
| `nostr` | [`NostrSignaling`](#nostrsignaling) and the `peerwire::nostr` module | `tungstenite` 0.24, `rustls` 0.23 (`ring`), `webpki-roots` 1, `k256` 0.13, `serde_json` 1 |
| `testing` | `Builder::port_mapping_at`, port mapping through a fake gateway (hidden from rustdoc). For the bindings' tests only, not for apps | nothing |

`peerwire-ffi` has one feature, `sync`: the shared-document functions (`pw_docs_*`, `pw_doc_*`) on `peerwire-sync`.
It is off by default, and the wasm build leaves it out. The other crates have no features.

## API docs

rustdoc has every public item with its doc comment. Build it and open it locally:

```sh
cargo doc -p peerwire --features tokio,nostr --open
cargo doc -p peerwire-core --open
cargo doc -p peerwire-sync --open
cargo doc -p peerwire-game --open
cargo doc -p peerwire-sim --open
cargo doc -p peerwire-ffi --features sync --open
```

Without the features, `AsyncNode`, `NostrSignaling` and the `pw_docs_*` / `pw_doc_*` functions are left out.

## peerwire

A `Node` owns one non-blocking UDP socket and one identity, and drives a `peerwire_core::Endpoint` from it. Call
`Node::poll` from your loop (a game's frame tick, a timer, an event loop) and read events with
`Node::try_recv_event`. `poll` never blocks. Two parts start threads of their own: router port mapping
([`Builder::port_mapping`](#builder)) runs on one, and `NostrSignaling` runs one per relay. Source:
[`crates/peerwire/src`](../../crates/peerwire/src); crate README: [`crates/peerwire`](../../crates/peerwire/README.md).

### Names

What `use peerwire::…` gives (from `crates/peerwire/src/lib.rs`):

| Name | What it is | Defined in |
|---|---|---|
| `Node`, `Builder`, `Status`, `Error`, `DEFAULT_STUN_SERVERS` | The node, its options, where it stands with a peer, `connect`'s error, the default STUN servers | `src/node.rs` |
| `AsyncNode` (feature `tokio`) | A `Node` driven by tokio | `src/async_node.rs` |
| `Event`, `Path` | Type aliases: `peerwire_core::Event<SocketAddr>` and `peerwire_core::Path<SocketAddr>` | `src/lib.rs` |
| `Signaling`, `Topic`, `MemoryHub`, `MemorySignaling` | The carrier trait, its topics, and the in-process carrier | `src/signaling.rs` |
| `LanSignaling` | A carrier over UDP multicast on one LAN | `src/lan.rs` |
| `NostrSignaling` (feature `nostr`) | A carrier over Nostr relays. The `peerwire::nostr` module also has `DEFAULT_RELAYS`, `KIND` and `NostrStats` | `src/nostr.rs` |
| `Privacy`, `Role`, `NatType`, `Failure`, `Side` | Which addresses a node gives out, room roles, the NAT type, why a dial failed | `src/traversal.rs` |
| `PortMapping`, `MapProtocol` | Where router port mapping stands | `src/portmap.rs` |
| `Config`, `ConnectError`, `DisconnectReason`, `Identity`, `Invite`, `Lane`, `PeerId`, `PeerStats`, `SendError`, `Stats`, `MAX_UNRELIABLE`, `MAX_URGENT` | Re-exported from `peerwire-core` | `crates/peerwire-core/src` |
| `traversal` (module) | The sans-IO NAT traversal layer (`Traversal`) that `Node` drives; the simulator drives it too | `src/traversal.rs` |

### Example: Node

One process hosts and prints its invite; another joins with it and says hello, and the host echoes. This is
[`examples/rust/echo.rs`](../../examples/rust/echo.rs) without its header comment:

```rust
use peerwire::{Event, Lane, Node};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut node = Node::bind("[::]:0")?;
    match std::env::args().nth(1) {
        Some(invite) => {
            node.connect(&invite)?;
        }
        None => println!("{}", node.invite()),
    }
    loop {
        node.poll();
        while let Some(event) = node.try_recv_event() {
            match event {
                Event::Connected { peer, .. } => {
                    println!("connected to {peer}");
                    node.send(&peer, Lane::Reliable, b"hello")?;
                }
                Event::Message { peer, data, .. } => {
                    println!("{peer}: {}", String::from_utf8_lossy(&data));
                    if data.starts_with(b"echo: ") {
                        return Ok(());
                    }
                    node.send(&peer, Lane::Reliable, &[b"echo: ", &data[..]].concat())?;
                }
                Event::ConnectFailed { peer } => return Err(format!("could not reach {peer}").into()),
                _ => {}
            }
        }
        std::thread::sleep(node.next_timeout().unwrap_or(Duration::MAX).min(Duration::from_millis(5)));
    }
}
```

```sh
cargo run -p peerwire --example echo              # prints a pw: invite and echoes back
cargo run -p peerwire --example echo -- pw:…      # joins, sends "hello", prints the echo
```

### Example: AsyncNode

The same with tokio, from [`examples/rust/echo_async.rs`](../../examples/rust/echo_async.rs) (feature `tokio`):

```rust
use peerwire::{AsyncNode, Event, Lane};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut node = AsyncNode::bind("[::]:0").await?;
    match std::env::args().nth(1) {
        Some(invite) => {
            node.connect(&invite)?;
        }
        None => println!("{}", node.invite()),
    }
    loop {
        match node.next_event().await {
            Event::Connected { peer, .. } => {
                println!("connected to {peer}");
                node.send(&peer, Lane::Reliable, b"hello")?;
            }
            Event::Message { peer, data, .. } => {
                println!("{peer}: {}", String::from_utf8_lossy(&data));
                if data.starts_with(b"echo: ") {
                    return Ok(());
                }
                node.send(&peer, Lane::Reliable, &[b"echo: ", &data[..]].concat())?;
            }
            Event::ConnectFailed { peer } => return Err(format!("could not reach {peer}").into()),
            _ => {}
        }
    }
}
```

```sh
cargo run -p peerwire --features tokio --example echo_async           # prints a pw: invite
cargo run -p peerwire --features tokio --example echo_async -- pw:…   # joins and says hello
```

Meeting by room name instead of an invite needs a carrier: see [Signaling](#signaling) and
[`examples/rust/lan_chat.rs`](../../examples/rust/lan_chat.rs).

### Node

One identity on one non-blocking UDP socket (`crates/peerwire/src/node.rs`).

Making one:

| Function | |
|---|---|
| `Node::bind(addr: impl ToSocketAddrs) -> io::Result<Node>` | A fresh identity, `Config::default()` and `DEFAULT_STUN_SERVERS`. `"[::]:0"` binds one dual-stack socket (IPv4 and IPv6, a free port): the node offers its IPv4 and its global IPv6 address, and a dial tries both at once. On a host without IPv6 it binds `0.0.0.0` on the same port instead. `"0.0.0.0:0"` binds IPv4 only; a given address binds that one |
| `Node::bind_with(addr: impl ToSocketAddrs, identity: Identity, config: Config) -> io::Result<Node>` | The same with a given identity (to keep the same `PeerId` across runs) and config |
| `Node::builder() -> Builder` | All the options: see [Builder](#builder) |

Identity and addresses:

| Method | |
|---|---|
| `peer_id(&self) -> PeerId` | This node's identity: the key every peer authenticates it by |
| `local_addr(&self) -> SocketAddr` | The bound address (`0.0.0.0` when `[::]` fell back to IPv4) |
| `invite(&self) -> String` | A `pw:` invite with this node's `addresses()` and its invite key, which lets a holder ask the node, over signaling, to dial back. Under `Privacy::RelayOnly` it lists no address, only its relay |
| `addresses(&self) -> Vec<SocketAddr>` | What the invite lists, in this order: the public address from STUN, the public port the router forwards (`Builder::port_mapping`), the last 4 addresses linked peers reported seeing it at, and the bound address (or, bound to any interface, the LAN and loopback addresses). At most 8, and only those the `Privacy` mode allows |
| `public_addr(&self) -> Option<SocketAddr>` | The public address STUN reported, once a server answered |
| `nat_type(&self) -> NatType` | What STUN says about this node's NAT |
| `privacy(&self) -> Privacy` | Which addresses the node gives out |
| `set_privacy(&mut self, privacy: Privacy)` | Changes that from the next offer and invite on. Links already up keep their path; disconnect a peer to stop it knowing an address it has. Switching to `RelayOnly` closes the links to relay peers (except the relay it names) and removes a router mapping; switching out of it dials them and maps again |
| `port_mapping(&self) -> PortMapping` | Where router port mapping stands; updated by `poll` |

Peers and rooms:

| Method | |
|---|---|
| `connect(&mut self, invite: &str) -> Result<PeerId, Error>` | Dials the invite's peer at every address it lists, and through any listed relay this node is linked to. With a carrier it also sends the peer this node's addresses, so the peer dials back and both punch through their NATs. Returns the peer's id; `Event::Connected` or `Event::ConnectFailed` follows. Under `RelayOnly` it dials through the relay only, and fails with `ConnectError::NoRelay` while none is linked |
| `join_room(&mut self, app_id: &str, room: &str)` | Joins room `room` of application `app_id` as a `Role::Peer` (a full mesh) and leaves any other room. Needs a carrier. Hellos repeat at 2 s, 4 s, … up to every 30 s for peers that join later (`HELLO_FIRST`, `HELLO_MAX` in `traversal.rs`). The room's name is its password: in a room the node closes links from anyone who didn't prove it, except peers it dialled itself |
| `join_room_as(&mut self, app_id: &str, room: &str, role: Role)` | `join_room` with a role: a `Host` links with clients and peers, a `Client` with hosts only (a star) |
| `join_room_with(&mut self, app_id: &str, room: &str, role: Role, host: Option<PeerId>)` | `join_room_as`, linking with `host` only when given, so a client that knows its host's key can't be taken in by another member |
| `room_role(&self, peer: &PeerId) -> Option<Role>` | The role `peer` proved in the current room |
| `leave_room(&mut self)` | Stops announcing and dialling. Links stay up |
| `failure(&self, peer: &PeerId) -> Option<Failure>` | Why the last dial to `peer` failed, after `Event::ConnectFailed` |
| `set_relay_peers<S: AsRef<str>>(&mut self, invites: &[S]) -> Result<(), Error>` | Keeps a link to each of these always-on peers (such as `peerwire-peer`), so peers this node can't punch to still link through one. A dropped one is dialled again at once, then after 1 s, 2 s, … up to 30 s (`RELAY_REDIAL`, `RELAY_REDIAL_MAX`). Replaces the last list (links to peers no longer on it close); empty stops; fails, changing nothing, if one is not an invite. Relay links raise no events and aren't in `peers()`. Not dialled under `RelayOnly` |
| `relay_peers(&self) -> Vec<(PeerId, Status)>` | Those relay peers, and where the node stands with each |

Messages and links:

| Method | |
|---|---|
| `send(&mut self, peer: &PeerId, lane: Lane, data: &[u8]) -> Result<(), SendError>` | Queues a message; it goes out on the next `poll`. Never blocks: with the reliable send buffer full it returns `SendError::WouldBlock`, and the unreliable lane drops its oldest message. `TooLarge` past the lane's limit (see [Lane](#lane)), `NotConnected` with no link |
| `disconnect(&mut self, peer: &PeerId)` | Closes the link, telling the peer. No event follows locally |
| `status(&self, peer: &PeerId) -> Status` | `Disconnected`, `Connecting` or `Connected` |
| `peers(&self) -> impl Iterator<Item = &PeerId>` | Peers with a live link, relay peers left out |
| `peer_stats(&self, peer: &PeerId) -> Option<PeerStats>` | A link's RTT, path RTT, congestion window, fragments sent and retransmitted; `PeerStats::loss()` estimates loss |
| `path(&self, peer: &PeerId) -> Option<&Path>` | The route the link takes: `Path::Direct(addr)` or `Path::Via(relay)` |
| `via_birthday(&self, peer: &PeerId) -> bool` | Whether the direct link leaves from one of this node's birthday-punch sockets |
| `birthday_sockets_opened(&self) -> u64` | How many sockets this node has opened for birthday punching |
| `stats(&self) -> Stats` | Node-wide counters: packets, drops, relaying, hole punching |

Driving it:

| Method | |
|---|---|
| `poll(&mut self)` | Does all pending work without blocking: reads the socket and the carrier, fires due timers, sends what is queued and queues the events. Call it often: every frame, or when `next_timeout` expires or the socket is readable |
| `poll_at(&mut self, now: std::time::Instant)` | `poll` with the caller's time, which must never go backwards |
| `try_recv_event(&mut self) -> Option<Event>` | The next queued event |
| `next_timeout(&self) -> Option<Duration>` | How long until a timer needs `poll`; `None` if none is armed. A host that sleeps wakes at this or when the socket is readable. At most 50 ms with a carrier (`SIGNAL_POLL`), as carriers are polled; at most 5 ms while birthday sockets are open (`EXTRA_POLL`) |
| `socket(&self) -> &UdpSocket` | The non-blocking socket, for hosts that wait on its readiness |

Limits in one `poll` (constants in `node.rs`): at most 4096 datagrams read (`MAX_READS_PER_POLL`) and 256 carrier
messages handled (`MAX_SIGNALS_PER_POLL`). While the socket's send buffer is full, up to 1024 datagrams wait
(`MAX_PENDING_SENDS`); past that the oldest is dropped, as UDP would.

### Builder

Options for a `Node`, from `Node::builder()`. Each takes `self` and returns the builder; `bind` ends the chain.

| Option | Argument | Default | What it does |
|---|---|---|---|
| `identity` | `Identity` | A fresh one, from the OS's randomness | The identity, to keep the same `PeerId` across runs |
| `config` | `Config` | `Config::default()` (see [Config](#config)) | The core's session settings |
| `stun_servers` | `impl IntoIterator<Item = S>`, `S: Into<String>` | `DEFAULT_STUN_SERVERS`: `stun.l.google.com:19302`, `stun.cloudflare.com:3478` | `host:port` servers to learn the public address from; empty turns STUN off. Names are resolved in `bind`, and servers that don't resolve are skipped. A socket bound to loopback asks none; a dual-stack one asks over IPv4 only |
| `signaling` | `impl Signaling + 'static` | None | A carrier, for rooms and for invites that punch through NATs. Without one, invites work where the host is reachable |
| `privacy` | `Privacy` | `Privacy::Direct` | Which addresses the node gives out (see [Privacy](#privacy)) |
| `port_mapping` | `bool` | `false` | Asks the router to forward a public UDP port to this node, by PCP, NAT-PMP or UPnP-IGD (tried in that order), and offers the mapped address like a public one. Runs on a thread of its own, for IPv4 and dual-stack sockets not bound to loopback; renewed while the node lives and removed when it is dropped. Paused under `Privacy::RelayOnly` |
| `relay_peers` | `impl IntoIterator<Item = S>`, `S: Into<String>` | None | Invites of always-on peers to keep linked as relays (see `Node::set_relay_peers`) |

`bind(self, addr: impl ToSocketAddrs) -> io::Result<Node>` binds the socket as `Node::bind` describes. It fails with
`ErrorKind::InvalidInput` if a relay peer is not an invite.

A chain, from the doc example in `crates/peerwire/src/lan.rs` (inside a function that returns a `Result`):

```rust
use peerwire::{LanSignaling, Node};

let mut node = Node::builder().signaling(LanSignaling::new()?).bind("0.0.0.0:0")?;
node.join_room("my-app", "kitchen"); // links with every node in "kitchen" on this LAN
# Ok::<(), std::io::Error>(())
```

#### Config

`peerwire::Config` is `peerwire_core::Config`. Every field is public, so set the ones you need on
`Config::default()`. Defaults from `impl Default for Config` in `crates/peerwire-core/src/endpoint.rs`:

| Field | Default | What it does |
|---|---|---|
| `keepalive` | 10 s | Send a PING if nothing was sent for this long (also keeps NAT mappings open) |
| `idle_timeout` | 30 s | Drop a link if nothing was received for this long |
| `probe_interval` | 1 s | Once nothing was received for `keepalive`, ping this often until the peer is heard or `idle_timeout` passes |
| `handshake_retry` | 500 ms | Wait before the first handshake resend; each wait after it doubles, up to 16 times this. With the defaults a dial that hears nothing sends at 0, 0.5, 1.5, 3.5, 7.5 and 15.5 s and fails at 23.5 s. Once its handshake is through, the dialling side also pings this often until it hears data from the peer |
| `handshake_attempts` | 6 | Handshake sends, the first included, before the dial fails (`Event::ConnectFailed`) |
| `handshake_timeout` | 10 s | How long a responder keeps a half-open handshake waiting for the first data packet |
| `max_pending_handshakes` | 4096 | Cap on half-open responder handshakes. From three quarters of it, a handshake from a UDP address takes no state until it echoes a cookie, and each source holds at most 4 (`PER_SOURCE`) |
| `max_links` | `usize::MAX` | Cap on announced links; links this node dials itself are not capped |
| `max_message` | 1 MiB (`1 << 20`) | Largest `Reliable` message, sent or received; a peer that sends a larger one is dropped |
| `max_send_buffer` | 4 MiB (`4 << 20`) | Per-link bytes queued or unacknowledged, for `Reliable` and for `Urgent` each |
| `reliable_window` | 1024 | Reliable fragments in flight per link |
| `unreliable_queue` | 256 | Unreliable messages queued per link; the oldest is dropped when full |
| `relay` | `true` | Forward packets between linked peers that ask (peer relay) |
| `relay_rate` | 256 KiB/s (`256 << 10`) | Bytes per second each peer may have this node relay |
| `relay_burst` | 64 KiB (`64 << 10`) | The burst on top of `relay_rate` |
| `relay_total_rate` | 0 | Bytes per second relayed for all peers together; 0 is no node-wide limit |
| `relay_queue` | 256 | Relayed packets queued per outgoing link; the oldest is dropped when full |
| `punch_interval` | 100 ms | While a link is relayed and has direct candidates, probe them this often… |
| `punch_duration` | 5 s | …for this long, then stay relayed |
| `punch_spray` | 0 | Also probe this many ports above each candidate, for NATs that allocate ports in sequence; 0 is off |
| `max_candidates` | 8 | Direct candidate addresses kept per link |
| `direct_timeout` | 15 s | A direct path silent this long falls back to the relay, if there is one |
| `share_addresses` | `true` | Tell peers the addresses this node sees them at; off when addresses are local handles, not network addresses |

### AsyncNode

Feature `tokio` (`crates/peerwire/src/async_node.rs`). A `Node` driven by tokio: `next_event().await` waits on the
socket and the node's timers instead of being polled. It derefs (`Deref`, `DerefMut`) to `Node`, so `peer_id`,
`invite`, `status`, `join_room` and the rest work on it as they are.

| Method | |
|---|---|
| `AsyncNode::bind(addr: impl ToSocketAddrs) -> io::Result<AsyncNode>` (async) | Binds like `Node::bind`. Call it inside a tokio runtime with IO and time enabled |
| `AsyncNode::from_node(node: Node) -> io::Result<AsyncNode>` | Drives a node made another way (`Node::builder()`, `Node::bind_with`). Call it inside a tokio runtime |
| `next_event(&mut self) -> Event` (async) | Waits for the next event: reads the socket when readable, fires timers when due. Cancel-safe: it can sit in a `select!`, and no event is lost when it is dropped |
| `connect(&mut self, invite: &str) -> Result<PeerId, Error>` | `Node::connect`, sending the handshake at once |
| `send(&mut self, peer: &PeerId, lane: Lane, data: &[u8]) -> Result<(), SendError>` | `Node::send`, sent at once rather than on the next poll; never blocks |
| `disconnect(&mut self, peer: &PeerId)` | `Node::disconnect` |
| `into_inner(self) -> Node` | The sync node underneath |

To use builder options, build the `Node` and wrap it: `crates/peerwire/tests/async_loopback.rs` binds its host with
`Node::builder().stun_servers(Vec::<String>::new()).bind("0.0.0.0:0")`, wraps it with `AsyncNode::from_node`, and
moves it into `tokio::spawn`. Carriers are polled, not awaited: with one, `next_event` wakes at least every 50 ms
(`Node::next_timeout`).

### Event

`peerwire::Event` is `peerwire_core::Event<SocketAddr>` (`crates/peerwire-core/src/endpoint.rs`).

| Variant | When |
|---|---|
| `Connected { peer: PeerId, path: Path, initiator: bool }` | A link is up. `initiator`: this side sent the handshake that made it |
| `Message { peer: PeerId, lane: Lane, data: Vec<u8> }` | A message arrived. `lane` is `Reliable` or `Unreliable`: urgent messages arrive as `Lane::Reliable` |
| `Disconnected { peer: PeerId, reason: DisconnectReason }` | A link went. `reason` is `Timeout`, `ClosedByPeer`, `Replaced` (the peer completed a new handshake, after a restart or a re-dial: a `Connected` for the new link follows, and reliable data in flight on the old one is lost) or `ProtocolViolation` |
| `ConnectFailed { peer: PeerId }` | A dial ran out of handshake attempts (`Config::handshake_attempts`). `Node::failure` says why |
| `PathChanged { peer: PeerId, path: Path }` | A link moved: relayed to direct after a hole punch, direct to relayed after the direct path went silent, or to a new address (roaming) |
| `AddrObserved { by: PeerId, addr: SocketAddr }` | A directly linked peer reports the address it sees this node at (its public address, behind a NAT) |

A `Node` doesn't pass on everything the core raises. Events about relay peers stay inside it. In a room, a link from
a peer that never proved the room key is closed unseen, and so is a relay-only node's new direct link to anyone but
its relay.

### Lane

`peerwire::Lane` is `peerwire_core::Lane`. Limits from `crates/peerwire-core/src/wire.rs` and `reliable.rs`:

| Variant | Delivery | Largest message |
|---|---|---|
| `Reliable` | Exactly once, in order | `Config::max_message` (1 MiB by default) |
| `Unreliable` | At most once, possibly out of order; the queue drops its oldest when full | 1100 bytes (`MAX_UNRELIABLE`) |
| `Urgent` | Exactly once, in order with other `Urgent` messages but not with `Reliable` ones: its own sequence, sent ahead of queued `Reliable` data, each fragment sent twice. Arrives as `Lane::Reliable` | 64 KiB (`MAX_URGENT`, `64 << 10`), or `Config::max_message` if smaller |

### Privacy

Which of its addresses a node gives out (`Builder::privacy`, `Node::set_privacy`). Peers it links with directly
still see the address its packets come from; only `RelayOnly` hides that. More in [Privacy](../guides/privacy.md)
and [privacy modes](../concepts/nat-traversal.md#privacy-modes).

| Variant | |
|---|---|
| `Direct` (default) | Every candidate: public, LAN, loopback. The lowest latency; peers on one LAN link over it |
| `PublicOnly` | Public addresses only: LAN, loopback, link-local, CGNAT (100.64/10) and other private ranges are left out of invites and offers. Two peers behind one NAT then link only if it hairpins, or through a relay |
| `RelayOnly { via: Option<PeerId> }` | No address at all, except to the relay: every other link goes through it (`Path::Via`) and never moves to a direct path. The relay is `via`, or, in a host/client room, the host. Costs the relay's upload (it carries every packet twice) and a detour in latency |

### Role

How a node takes part in a room (`Node::join_room_as`, `Node::join_room_with`).

| Variant | |
|---|---|
| `Peer` | A full mesh: connect to every peer and host in the room. What `join_room` uses |
| `Host` | A star centre: connect to every client and peer |
| `Client` | A star leaf: connect to hosts only |

### NatType

What STUN says about this node's NAT (`Node::nat_type`). It takes answers from two servers. More in
[NAT traversal](../concepts/nat-traversal.md).

| Variant | |
|---|---|
| `Unknown` (default) | No STUN answer yet, or only one server answered |
| `Open` | No NAT: the public address is the socket's own |
| `Cone` | One public port for every destination (full, restricted or port-restricted cone; STUN can't tell them apart). Punching works with cone and open peers, and usually, not always, with symmetric ones |
| `Symmetric` | A new public port per destination. Punching works towards open, full-cone and restricted-cone peers, towards port-restricted ones only if the ports come in a predictable sequence; two symmetric peers need a relay |

### Failure, Status and Error

`Node::failure(peer)` gives a `Failure` after `Event::ConnectFailed`. Its `Display` is a sentence for users.

| Variant | |
|---|---|
| `Failure::Unreachable` | No handshake answer on any address: the peer is gone, or a firewall drops UDP |
| `Failure::Symmetric(Side)` | The NAT on that side (`Side::Ours`, `Side::Theirs` or `Side::Both`) is symmetric and the other side's router lets in only packets from ports it has sent to, so punching can't work, and no peer linked to both could relay. Peerwire has no TURN server; a third peer that both can reach fixes it |

`Status` (`Node::status`): `Disconnected` (not linked, not dialling), `Connecting` (the handshake is under way) or
`Connected`.

`Error` (`Node::connect`, `Node::set_relay_peers`): `InvalidInvite` when the string is not a `pw:` invite, or
`Connect(ConnectError)` when the dial can't start. `ConnectError` is `SelfConnect` (your own invite),
`InvalidPeerId` (not a valid Ed25519 key), `NoPath` (no address in an IP family the socket reaches, and no relay
this node is linked to) or `NoRelay` (relay-only, and no relay is linked).

### PortMapping

Where router port mapping stands (`Node::port_mapping`). `PortMapping::addr()` gives the mapped address when there
is one. Details: [router port mapping](../concepts/nat-traversal.md#router-port-mapping).

| Variant | |
|---|---|
| `Off` | Not asked for: `Builder::port_mapping` is off |
| `Trying` | Asking the gateway |
| `Mapped { addr: SocketAddr, protocol: MapProtocol }` | The router forwards `addr` to this node, which offers it to peers as a public address |
| `NotPublic { addr: SocketAddr, protocol: MapProtocol }` | The router's own address is not public (CGNAT or a private range): not offered, and the mapping is removed |
| `Unavailable` | No gateway answered, or each protocol was refused. Asked again every 5 minutes (`RETRY` in `portmap.rs`) |
| `RelayOnly` | Asked for, but not run while the node is under `Privacy::RelayOnly` |

`MapProtocol` is `Pcp` (RFC 6887), `NatPmp` (RFC 6886) or `Upnp` (UPnP Internet Gateway Device).

### Signaling

A carrier swaps addresses between peers that can't reach each other yet: a publish/subscribe channel keyed by
`Topic`s (`crates/peerwire/src/signaling.rs`). Hand one to `Builder::signaling`; the node subscribes and publishes
itself. Carriers are untrusted: they see topics, timing and the sender's IP, never addresses or other contents of
peer messages.

```rust
# use peerwire::Topic;
pub trait Signaling: Send {
    fn subscribe(&mut self, topic: Topic);
    fn unsubscribe(&mut self, topic: Topic);
    fn publish(&mut self, topic: Topic, payload: &[u8]);
    fn poll(&mut self) -> Option<(Topic, Vec<u8>)>;
}
```

| Method | |
|---|---|
| `subscribe(topic)` | Starts delivering messages published to `topic` |
| `unsubscribe(topic)` | Stops delivering them |
| `publish(topic, payload)` | Sends `payload` to every other subscriber of `topic` |
| `poll()` | The next message that arrived, if any |

An implementation never blocks: `publish` queues, `poll` returns what has arrived. Delivery is best effort and need
not be ordered; messages are not stored for subscribers that come later.

`Topic(pub [u8; 32])` is a SHA-256, named as in `packages/js/src/nostr.js` so Rust and browser peers can share a
carrier. `Topic::room(app_id, room)` hashes `peerwire/room/{app_id}/{room}` (presence); `Topic::peer(&PeerId)`
hashes `peerwire/peer/{hex peer id}` (a peer's inbox, for offers sealed to it). `to_hex()` and `from_hex(&str)`
convert it.

The carriers:

| Carrier | Feature | Reaches |
|---|---|---|
| `MemorySignaling`, from a `MemoryHub` | none | Peers in one process (tests) |
| `NostrSignaling` | `nostr` | The internet, over public Nostr relays |
| `LanSignaling` | none | One LAN, with no internet |

#### MemoryHub

An in-process hub, for tests and for peers in one process. The hub itself is not a carrier: `MemoryHub::new()`
(also `Default`; a clone shares the hub) makes one, and `hub.connect()` returns a `MemorySignaling`, the carrier. A
carrier delivers to every other carrier on the hub subscribed to the topic, never back to the sender. A dropped
carrier leaves the hub.

#### NostrSignaling

Feature `nostr` (`crates/peerwire/src/nostr.rs`). Signaling over Nostr relays (NIP-01), wire-compatible with
`packages/js/src/nostr.js`: ephemeral events of kind 25059 (`peerwire::nostr::KIND`), which relays forward to live
subscribers and don't store; one `p` tag with the topic in hex; the payload in base64; and a fresh secp256k1 key per
carrier, so nothing links events to the Peerwire identity or to other sessions.

| Item | |
|---|---|
| `NostrSignaling::new<S: AsRef<str>>(relays: impl IntoIterator<Item = S>) -> NostrSignaling` | Connects to the relays (`wss://host[:port][/path]` or `ws://…`) in the background, one thread each, and reconnects with backoff when one drops: 1 s, doubling up to 30 s (`MAX_BACKOFF`) |
| `NostrSignaling::default()` | `new` on `peerwire::nostr::DEFAULT_RELAYS`: `wss://nos.lol`, `wss://relay.primal.net`, `wss://nostr.mom` |
| `stats(&self) -> peerwire::nostr::NostrStats` | `relays_open` (connections open now), `published`, `received`, and `rejected` (events a relay refused with `OK false`) |

`wss://` uses rustls with the webpki roots; `ws://` is plain TCP, for a relay on the LAN or loopback. While a relay
is down, up to 64 events wait for it (`MAX_QUEUED`). Up to 1024 received events wait for `poll` (`MAX_INBOX`); past
that, relays' events are dropped. A websocket message from a relay is at most 128 KiB (`MAX_MESSAGE`).

From `crates/peerwire/README.md`, whose examples run as doc tests (inside a function that returns a `Result`):

```rust
use peerwire::{Node, NostrSignaling};

let mut node = Node::builder()
    .signaling(NostrSignaling::new(["wss://nos.lol", "wss://nostr.mom"]))
    .bind("[::]:0")?;
node.join_room("my-app", "lobby"); // finds and connects the room's peers
# Ok::<(), std::io::Error>(())
```

For tests on a LAN, [`examples/rust/nostr_relay.rs`](../../examples/rust/nostr_relay.rs) is a small relay:
`cargo run -p peerwire --features nostr --example nostr_relay -- 127.0.0.1:7447`.

#### LanSignaling

Signaling on one local network with no internet, over UDP multicast (`crates/peerwire/src/lan.rs`). Each message is
one datagram to the group, with a TTL of 1, so it never leaves the link. Rooms then work as they do over Nostr: peers
on one Wi-Fi find each other and link directly. Guide: [LAN discovery](../guides/lan-discovery.md).

| Item | |
|---|---|
| `LanSignaling::new() -> io::Result<LanSignaling>` | A carrier on `GROUP`, on the interface the system picks for multicast (normally the default route's) |
| `LanSignaling::on(group: SocketAddrV4, interface: Ipv4Addr) -> io::Result<LanSignaling>` | A carrier on `group` (an IPv4 multicast address and port), sending and listening on the interface whose address is `interface` (`0.0.0.0`: the system's choice). `ErrorKind::InvalidInput` if `group` isn't multicast. An ESP32 names its own address here ([ESP32](../guides/esp32.md)) |
| `LanSignaling::GROUP` | `239.255.80.87:20567`, in the organisation-local scope that routers don't forward |
| `LanSignaling::MAX_PAYLOAD` | 1356 bytes: a 1400-byte datagram (`MAX_FRAME`) less the 44-byte header (`HEADER`: magic 4, sender tag 8, topic 32). A larger message is dropped, never fragmented |

The socket is bound to the group address, so it takes only datagrams sent to the group (not on Windows, which can't
bind a multicast address). One `poll` reads at most 64 datagrams (`MAX_READS`). A host firewall must let in UDP to
the group's port, or the node hears no one; some guest and public Wi-Fi networks drop multicast or keep clients
apart. Browsers can't send UDP multicast. A complete program is
[`examples/rust/lan_chat.rs`](../../examples/rust/lan_chat.rs): run
`cargo run -p peerwire --example lan_chat -- kitchen` on two or more machines on one Wi-Fi.

## peerwire-core

The sans-IO session core (`crates/peerwire-core/src`). An `Endpoint<A>` holds one identity and any number of links,
each authenticated and encrypted with `Noise_IK_25519_ChaChaPoly_BLAKE2s`. It never opens sockets, spawns threads or
reads a clock: the host passes datagrams and the time in, and takes datagrams, events and the next deadline out.
`peerwire::Node` is one host; the wasm module (`peerwire-ffi`) and the simulator are others. README:
[`crates/peerwire-core`](../../crates/peerwire-core/README.md); wire format: [protocol](../concepts/protocol.md).

`Endpoint::new(identity: &Identity, config: Config, seed: [u8; 32])` makes one. The `seed` drives index allocation
and Noise ephemeral keys: production hosts pass 32 bytes from the OS CSPRNG, the simulator a fixed one. `A` is any
transport address that implements `Address` (it must encode to at most 19 bytes, `MAX_ADDR_LEN`); `SocketAddr`
does. `Instant` is the host's monotonic time in microseconds, from any epoch, never going backwards
(`Instant::from_micros`, `Instant::from_millis`, `Instant::ZERO`).

### The five calls

| Call | The host calls it |
|---|---|
| `handle_datagram(&mut self, now: Instant, from: A, data: &[u8])` | For every datagram received |
| `handle_timeout(&mut self, now: Instant)` | Once `poll_timeout()` has passed |
| `poll_transmit(&mut self, now: Instant) -> Option<Transmit<A>>` | Until it returns `None`, sending each `Transmit { dst, payload }` |
| `poll_event(&mut self) -> Option<Event<A>>` | Until it returns `None`, handing each event to the app |
| `poll_timeout(&self) -> Option<Instant>` | To learn the earliest time `handle_timeout` should be called; it sleeps until then or the next datagram |

The drive loop, from [`crates/peerwire-core/examples/udp_pair.rs`](../../crates/peerwire-core/examples/udp_pair.rs).
It is shortened: the example's `turn` can also block on the socket until the next deadline.

```rust
use peerwire_core::{Config, Endpoint, Event, Identity, Instant};
use rand_core::{OsRng, RngCore};
use std::net::{SocketAddr, UdpSocket};

fn seed() -> [u8; 32] {
    let mut s = [0u8; 32];
    OsRng.fill_bytes(&mut s);
    s
}

struct Host {
    sock: UdpSocket,
    ep: Endpoint<SocketAddr>,
    start: std::time::Instant,
}

impl Host {
    fn new() -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let identity = Identity::generate(&mut OsRng);
        Host { sock, ep: Endpoint::new(&identity, Config::default(), seed()), start: std::time::Instant::now() }
    }

    fn now(&self) -> Instant {
        Instant::from_micros(self.start.elapsed().as_micros() as u64)
    }

    fn flush(&mut self) {
        let now = self.now();
        if self.ep.poll_timeout().is_some_and(|t| t <= now) {
            self.ep.handle_timeout(now);
        }
        while let Some(t) = self.ep.poll_transmit(now) {
            let _ = self.sock.send_to(&t.payload, t.dst);
        }
    }

    fn turn(&mut self) -> Vec<Event<SocketAddr>> {
        self.flush();
        let mut buf = [0u8; 2048];
        self.sock.set_nonblocking(true).unwrap();
        while let Ok((n, from)) = self.sock.recv_from(&mut buf) {
            self.ep.handle_datagram(self.now(), from, &buf[..n]);
        }
        self.flush();
        std::iter::from_fn(|| self.ep.poll_event()).collect()
    }
}
```

`OsRng` here is `rand_core` 0.6 with its `getrandom` feature. Two endpoints wired to each other in one process, with
no socket at all, are the example in the [core README](../../crates/peerwire-core/README.md), which runs as a doc
test.

### Other Endpoint methods

| Method | |
|---|---|
| `connect(&mut self, now: Instant, addr: A, peer: PeerId) -> Result<(), ConnectError>` | Starts a handshake with `peer` at `addr`. A no-op if already linked or dialling |
| `connect_via(&mut self, now: Instant, relay: PeerId, peer: PeerId, candidates: &[A]) -> Result<(), ConnectError>` | Dials `peer` through `relay` (a linked peer that also has a link to `peer`), and at `candidates` directly. Once up, both sides punch and move off the relay if a direct path works both ways |
| `connect_with(&mut self, now: Instant, peer: PeerId, targets: &[Path<A>]) -> Result<(), ConnectError>` | Sends the handshake along every path at once. Two peers doing this towards each other's public addresses punch through cone NATs |
| `send(&mut self, peer: &PeerId, lane: Lane, data: &[u8]) -> Result<(), SendError>` | Queues a message. `NotConnected`, `TooLarge` past the lane's limit, `WouldBlock` with the send buffer full |
| `disconnect(&mut self, peer: &PeerId)` | Closes the link, telling the peer; no event for `peer` |
| `add_candidates(&mut self, now: Instant, peer: &PeerId, addrs: &[A])` | Adds direct addresses learned later (signaling) for a dialling or relayed peer, and punches towards them |
| `set_guesses(&mut self, now: Instant, peer: &PeerId, addrs: &[A])` | Port-prediction guesses for a peer behind a symmetric NAT; each is probed three times. At most 64 kept (`MAX_GUESSES`) |
| `spray`, `sweep`, `opener` | Birthday and lockstep punching: probing many ports of a peer's IP (at most 4096, `MAX_SPRAY`), and the packet a host sends from many extra sockets |
| `add_relay(&mut self, now: Instant, relay: PeerId, peer: &PeerId)`, `set_slow(&mut self, peer: &PeerId, addr: Option<A>)` | Offer a relay for a link, and mark a slow address (such as a signaling channel carrying datagrams) that a relay beats |
| `set_relay_only(&mut self, on: bool)`, `relay_only(&self) -> bool` | Keep relayed links relayed, so peers reached through a relay never learn this endpoint's address |
| `handle_datagram_from(&mut self, now: Instant, from: A, peer: &PeerId, data: &[u8])` | `handle_datagram` for a channel only `peer` sends on (a WebRTC DataChannel, a signaling pipe): packets on it for another peer's link are dropped unread |
| `handle_datagram_direct(&mut self, now: Instant, from: A, data: &[u8]) -> Option<(PeerId, bool)>` | `handle_datagram`, also giving the peer whose session the datagram authenticated to (its crypto ran: not an HS1 sent again while half-open, or an HS2 sent again), if that session's direct path is now `from`, and whether the datagram was fresh: an HS2 that finished our dial, or a Data packet with a new nonce. An HS1 is not: a copy of one reads again once its handshake is no longer half-open (NB85-1) |
| `peer_id`, `stats`, `session_count`, `connected_peers`, `is_connected`, `path`, `rtt`, `min_rtt`, `path_rtt`, `peer_stats` | This endpoint's id, its counters (`Stats`), and each link's state, route and timings |

The crate also has `Identity` and `PeerId` (Ed25519; `Identity::generate`, `Identity::from_secret_bytes`,
`secret_bytes`, `peer_id`), `Invite` (`Invite::parse` and `to_link` for `pw:` strings; fields `peer`, `key`,
`addrs`, `relays`), the `stun` codec, and `sealed::seal` / `sealed::open` for one-shot messages sealed to a
`PeerId`.

## peerwire-sync

Shared documents: maps, sets and logs that every member of a room holds and that converge. It is `room.doc(name)` of
`@peerwire/js` for Rust, Godot and C hosts, with the same frames byte for byte. Like the core it is sans-IO: the
host passes time and messages in, and drains messages, events and the next timer. README:
[`crates/peerwire-sync`](../../crates/peerwire-sync/README.md); guide:
[shared documents](../guides/shared-documents.md).

- `Docs::new(me: PeerId, seed: [u8; 32])` holds a member's docs. `open(now, name, Options)` opens one and gives a
  `DocId`; `doc(id)` gives a `DocMut` to read and write (`map_set`, `map_delete`, `set_add`, `set_delete`,
  `log_append`, `log_keep`, `get`, `entries`, `sync`, `snapshot`). Values are JSON text.
- Membership: `joined(now, peer, Route)`, `rerouted`, `left`.
- The loop: `receive(now, peer, message)` takes a reliable message if it is a doc's (first byte `DOC_TAG`, 7) and
  returns `false` if it is the app's; `poll_transmit()` gives `(PeerId, Vec<u8>)` to send on `Lane::Reliable`, and
  `retry(peer, message)` takes back one the link refused; `poll_event()` gives `Event::Change`, `Event::Sync` or
  `Event::Overflow` (close that member's link); `handle_timeout(now)` once `poll_timeout()` has passed.
- `Options { copy, hub }`: start from a `snapshot()` kept on the device, or act as the hub of a star.

## peerwire-game

Deterministic building blocks for peer-to-peer games: every peer computes the same state from the same inputs, bit
for bit. The crate uses no floating point and has no networking. README:
[`crates/peerwire-game`](../../crates/peerwire-game/README.md).

- `Q16_16` fixed point (`i32` with 16 fractional bits): `q16_from_i32`, `q16_add`, `q16_mul`, `q16_div`,
  `q16_sqrt` and the rest; `Vec2Q16`, `AabbObstacleQ16` and `has_line_of_sight_q16`.
- `PlayerEntityQ16::step_physics(input_bits, arena_max_q16)` moves a player one tick.
- `partition_into_hilbert_islands(grid_size, players, k_max)` groups nearby players into islands of at most 12
  (`MAX_ISLAND_PLAYERS`).
- `compute_island_state_crc16(tick, players)` and `find_byzantine_desync_peers(peer_crcs)` find peers whose checksum
  differs from the quorum.
- Timing constants for lockstep at 60 Hz: `MAX_UNWITNESSED_DELAY_TICKS` (2), `MIN_BOUNDARY_FLIGHT_TICKS` (4),
  `STATE_CRC_LOOKBACK_TICKS` (8), `ROLLBACK_RING_SIZE` (64).

## peerwire-sim

A deterministic, single-threaded network simulator. Every node is a real `Endpoint`; only the network is simulated,
in virtual time, so the same seed always gives the same run. It is where load, loss and NAT tests run on a small
machine. README: [`crates/peerwire-sim`](../../crates/peerwire-sim/README.md).

- `Sim::new(NetConfig, seed: u64)`; `NetConfig` has `latency` (20 ms by default), `jitter`, `loss`, `duplicate` and
  `in_order`.
- `add_node(Config) -> usize`, `add_nat(NatConfig, parent: Option<usize>) -> usize`, `add_node_behind(nat, Config)`,
  `add_stun_server()`. `NatConfig` has presets: `full_cone()`, `restricted_cone()`, `port_restricted()`,
  `symmetric_sequential()`, `symmetric_random()`.
- `connect(from, to)`, `send(from, to, lane, data)`, `take_events(node)`, and to advance time `step()`,
  `run_for(duration)` and `run_until(limit, done)`.
- Examples: `cargo run --release -p peerwire-sim --example arena` (a super-peer lockstep arena) and `--example bulk`
  (a bulk transfer with urgent messages alongside).

## peerwire-peer

An optional always-on peer: an ordinary `peerwire` node that stays online, so the peers linked to it
(`Node::set_relay_peers`) can relay through it when their NATs can't punch. It is a binary, not a library. It keeps
one identity across restarts, prints its `pw:` invite, and takes its settings from `PEERWIRE_*` environment
variables; by default it binds `127.0.0.1:7700`. README: [`crates/peerwire-peer`](../../crates/peerwire-peer/README.md);
running it in Docker: [always-on peer](../guides/always-on-peer.md); how relaying works:
[relay peers](../concepts/nat-traversal.md#relay-peers).

```sh
PEERWIRE_DATA=./peer-data cargo run --release -p peerwire-peer          # run; prints the peer id and invite
PEERWIRE_DATA=./peer-data cargo run --release -p peerwire-peer -- check # link to it from loopback; exit 0 if it answers
```

## peerwire-ffi and peerwire-godot

`peerwire-ffi` is the C ABI over `peerwire-core`: the one surface for hosts that aren't Rust. Built for
`wasm32-unknown-unknown` it is the zero-import WebAssembly module that `@peerwire/js` loads; built natively it is a
shared or static library for Dart, Swift or C. Its functions: [C ABI](c-abi.md); the browser side:
[JavaScript API](javascript.md).

`peerwire-godot` (`bindings/godot`) builds the GDExtension with `cargo build --release -p peerwire-godot`. It uses
`peerwire` with `nostr`, and `peerwire-sync` for shared documents. Guide: [Godot 4](../guides/godot.md).
