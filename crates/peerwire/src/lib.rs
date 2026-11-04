//! `peerwire`: peer-to-peer nodes over UDP, with no server.
//!
//! A [`Node`] owns one non-blocking UDP socket and one identity, and drives a
//! [`peerwire_core::Endpoint`] from it, on the thread that polls it: call
//! [`Node::poll`] from your loop (a game's frame tick, a timer, an event loop)
//! and, without blocking, it drains the socket, fires timers, sends what is
//! queued and queues events for [`Node::try_recv_event`]. Two parts start
//! threads of their own: router port mapping ([`Builder::port_mapping`]) runs
//! on one, and `NostrSignaling` runs one per relay.
//!
//! ```no_run
//! use peerwire::{Event, Lane, Node};
//!
//! let mut host = Node::bind("[::]:0")?;
//! let invite = host.invite(); // hand this "pw:…" string to the other peer
//! # let mut guest = Node::bind("[::]:0")?;
//! let peer = guest.connect(&invite)?;
//! loop {
//!     guest.poll();
//!     while let Some(e) = guest.try_recv_event() {
//!         if let Event::Connected { peer, .. } = e {
//!             guest.send(&peer, Lane::Reliable, b"hello")?;
//!         }
//!     }
//!     # break;
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Through NATs
//!
//! A node asks STUN servers for its public address ([`Node::public_addr`],
//! [`Node::nat_type`]) and puts it in its invites. Give it a [`Signaling`] carrier
//! ([`Builder::signaling`]) and peers swap addresses over it, so both dial at once
//! and punch through their NATs: [`Node::join_room`] finds and connects the peers
//! of a room, and [`Node::connect`] punches too. Where NATs can't punch (a
//! symmetric one against one that filters by port), a peer connected to both
//! relays, and [`Node::failure`] says which side was symmetric. See [`traversal`].
//! With [`Builder::port_mapping`] a node also asks its router (PCP, NAT-PMP or
//! UPnP-IGD) to forward a public port to it, and offers that address too.
//! The `nostr` feature adds `NostrSignaling`, over public Nostr relays. On one LAN,
//! with no internet, [`LanSignaling`] carries rooms over UDP multicast.
//!
//! # Privacy
//!
//! Directly linked peers see each other's IP address. [`Privacy`] ([`Builder::privacy`],
//! [`Node::set_privacy`]) picks what a node gives out besides: every address, public
//! ones only (no LAN or loopback), or none, linking only through a relay
//! ([`Privacy::RelayOnly`]). Invites carry a key, and a node dials back only peers
//! that hold its invite, share its room, or that it dialled: a PeerId alone gets
//! nothing.
//!
//! With the `tokio` feature, [`AsyncNode`] wraps a `Node` and waits on the socket
//! and timers itself: `node.next_event().await`.

#[cfg(feature = "tokio")]
mod async_node;
mod lan;
mod node;
#[cfg(feature = "nostr")]
pub mod nostr;
mod portmap;
mod signaling;
pub mod traversal;

#[cfg(feature = "tokio")]
pub use async_node::AsyncNode;
pub use lan::LanSignaling;
pub use node::{Builder, Error, Node, Status, DEFAULT_STUN_SERVERS};
#[cfg(feature = "nostr")]
pub use nostr::NostrSignaling;
pub use peerwire_core::{
    Config, ConnectError, DisconnectReason, Identity, Invite, Lane, PeerId, PeerStats, SendError, Stats,
    MAX_UNRELIABLE, MAX_URGENT,
};
pub use portmap::{MapProtocol, PortMapping};
pub use signaling::{MemoryHub, MemorySignaling, MultiSignaling, Signaling, Topic};
pub use traversal::{Failure, NatType, PathKind, Policy, Privacy, Role, Side};

/// An event from a [`Node`]: a peer connected or left, a message arrived, a link moved.
pub type Event = peerwire_core::Event<std::net::SocketAddr>;

/// How a link reaches its peer: a UDP address, or through a relaying peer.
pub type Path = peerwire_core::Path<std::net::SocketAddr>;

// The README examples run as doc tests (`cargo test --doc`).
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

// The Rust samples of the repo's README.md and docs/, one item per page (NB-070): doc tests that compile and never
// run, as many are host loops or dial public relays. A plain `cargo test` leaves them out; scripts/gate.sh and CI
// build them with RUSTDOCFLAGS="--cfg docs_samples --test-runtool true". They build as this crate's tests do: `tokio`
// and `nostr` on, with peerwire_core, peerwire_sim and peerwire_sync beside it. A sample may hide its set-up behind
// `# ` lines; one that can't build is ```rust,ignore with the reason beside it. rustdoc compiles a bare ``` block as
// Rust, so every block on these pages names its language (```text). A failing sample's line in its page is the line
// cargo reports, less that of the item's #[doc], plus one. packages/js/test/docs_samples.mjs fails for a page with a
// ```rust block that isn't listed here.
#[cfg(all(doctest, docs_samples))]
mod doc_samples {
    #[doc = include_str!("../../../README.md")]
    struct Readme;
    #[doc = include_str!("../../../docs/getting-started/quick-start.md")]
    struct GettingStartedQuickStart;
    #[doc = include_str!("../../../docs/guides/lan-discovery.md")]
    struct GuidesLanDiscovery;
    #[doc = include_str!("../../../docs/guides/messaging.md")]
    struct GuidesMessaging;
    #[doc = include_str!("../../../docs/guides/privacy.md")]
    struct GuidesPrivacy;
    #[doc = include_str!("../../../docs/guides/rooms.md")]
    struct GuidesRooms;
    #[doc = include_str!("../../../docs/guides/shared-documents.md")]
    struct GuidesSharedDocuments;
    #[doc = include_str!("../../../docs/reference/rust.md")]
    struct ReferenceRust;
}
