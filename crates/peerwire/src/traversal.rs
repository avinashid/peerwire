//! NAT traversal, sans-IO: STUN, room presence and candidate exchange over a
//! [`Signaling`](crate::Signaling) carrier, next to a core [`Endpoint`].
//!
//! [`Node`](crate::Node) drives one of these on its UDP socket; the simulator drives
//! the same code over simulated NATs. Like the core it never reads a clock or a
//! socket: the host passes time, datagrams and signaling messages in, and drains
//! STUN datagrams and signaling actions out.
//!
//! How two peers behind NATs meet:
//! 1. Each asks STUN servers, on the session's own socket, which public address
//!    its NAT gave it (`public_addr`), and from two servers guesses the NAT type.
//! 2. A peer that learns of another (a room `hello`, or an invite) sends it an
//!    *offer*: its candidate addresses, NAT type and connected peers, sealed to
//!    the recipient's [`PeerId`] and published to that peer's topic.
//! 3. The recipient dials every candidate and answers with its own; the first
//!    side dials on the answer. Both dial at once, so each NAT has already let
//!    the other side's packets in: the handshake punches through cone NATs.
//! 4. A symmetric NAT picks a new port for each peer, so the address it offers is
//!    not the one its packets come from. If it hands ports out in sequence, it
//!    offers a prediction and the other side probes the ports around it.
//! 5. Where punching can't work (symmetric against port-filtering NATs), each also
//!    dials through a peer both are connected to ([`Path::Via`]); core moves the
//!    link off the relay if a direct path turns up. With no such peer the dial
//!    fails with [`Failure::Symmetric`], unless the node has its own TURN server and
//!    its [`Policy`] lists [`PathKind::Turn`] (feature `turn`): then it dials through
//!    its allocation there, and offers the relayed address ([`Hop`]).
//!
//! Rooms are closed by their name. The room *topic* is a hash every relay sees, so
//! it proves nothing; a separate *room key*, hashed from the name under another
//! label, is known only to those given the name. Hellos and room offers carry an
//! HMAC under that key, and a node in a room links only with peers that proved it
//! (or that it dialled itself, from an invite). A room whose name can be guessed is
//! a public lobby. Hellos carry the sender's clock and are dropped when stale. They
//! are sealed (AES-256-GCM) under a third key from the name, as `nostr.js` seals
//! them, so a relay that sees the topic doesn't learn who is in the room (SEC-2).
//!
//! Outside rooms, a node acts on an offer only from a peer that holds its invite
//! (the offer carries an HMAC under the invite's key) or that it dialled itself:
//! knowing a [`PeerId`] is not enough to make a node dial back or reveal its
//! addresses.
//!
//! [`Privacy`] decides which addresses a node gives out: all of them, public ones
//! only, or none (every link but the one to its relay goes through the relay).

use crate::signaling::{hex, unhex, Topic};
#[cfg(feature = "turn")]
use crate::turn::TurnServer;
use crate::turn::{TurnEvent, TurnSet};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use peerwire_core::{sealed, stun, Address, ConnectError, Endpoint, Event, Identity, Instant, Path, PeerId, Transmit};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// How often to ask STUN servers again (keeps the NAT mapping and `public_addr` fresh).
const STUN_INTERVAL: Duration = Duration::from_secs(25);
/// First STUN retransmit; doubles, `STUN_TRIES` sends in all.
const STUN_RTO: Duration = Duration::from_millis(500);
const STUN_TRIES: u32 = 3;
/// Room hellos repeat at 2 s, 4 s, ... up to every 30 s, for peers that missed one.
const HELLO_FIRST: Duration = Duration::from_secs(2);
const HELLO_MAX: Duration = Duration::from_secs(30);
/// Least time between two offers to one peer.
const OFFER_GAP: Duration = Duration::from_secs(2);
/// Offers older or newer than this (by the sender's clock) are dropped.
const FRESH_MS: u64 = 60_000;
/// Hellos answered and offers acted on each have a budget: a burst of `BURST`, then
/// one per `REFILL`, so a flood can't make a node dial or publish without bound,
/// and a flood of one kind doesn't starve the other. Proofs are checked first, so
/// forged ones cost no token.
const BURST: u32 = 30;
const REFILL: Duration = Duration::from_secs(2);
/// Offers acted on per sender: a burst, then one per `SENDER_REFILL`, so one peer
/// can't spend the whole offer budget.
const SENDER_BURST: u32 = 8;
const SENDER_REFILL: Duration = Duration::from_secs(1);
/// Messages on this node's inbox opened (NB64-2): a burst, then one per `INBOX_REFILL`.
/// Opening one costs a Noise_X open (210 µs measured on an aarch64 server core) before
/// any proof can be checked, and its topic is on the wire, so anyone who can publish to
/// it (a LAN neighbour, at line rate) could otherwise burn CPU without bound. Real
/// offers fit well inside: those acted on are capped at `BURST`, then one per `REFILL`.
const INBOX_BURST: u32 = 64;
const INBOX_REFILL: Duration = Duration::from_millis(50);
const _: () = assert!(INBOX_BURST > BURST, "a burst of real offers is opened before their budget runs out");
/// Most offer nonces remembered (they are only kept for offers acted on).
const MAX_NONCES: usize = 1024;
const MAX_CANDIDATES: usize = 8;
const MAX_RELAYS: usize = 8;
/// Relays tried per dial.
const VIA_PER_DIAL: usize = 2;
const MAX_OBSERVED: usize = 4;
/// Peers forgotten after this long without news (unless connected).
const FORGET: Duration = Duration::from_secs(600);
/// Offer versions: 1 plain (only to peers that dialled us), 2 in a room, with a
/// room proof, 3 with an invite proof (from a peer dialling us from our invite).
const OFFER: u8 = 1;
const ROOM_OFFER: u8 = 2;
const INVITE_OFFER: u8 = 3;
const PROOF_LEN: usize = 32;
/// A symmetric NAT whose two STUN mappings are 1 to `MAX_STEP` ports apart, upwards,
/// looks sequential: its next mapping is predictable.
const MAX_STEP: u16 = 8;
/// Ports probed around a predicted one. The NAT may have handed out a few more
/// since STUN asked, and ports go up, so the window leans forward.
const GUESS_BEFORE: i32 = 4;
const GUESS_AFTER: i32 = 27;
/// Starts the port prediction entry in an offer's address list. Decoders that
/// predate it skip it, as an address they can't read.
const PREDICTION_TAG: [u8; 2] = [0xfe, 1];
/// Starts the entry saying how a symmetric NAT maps (NB-083), skipped the same way.
const MAPPING_TAG: [u8; 2] = [0xfe, 2];
/// Starts an entry holding a relayed address of the sender's (its TURN allocation's, NB-099), skipped the same way.
const RELAYED_TAG: [u8; 2] = [0xfe, 3];
/// Relayed addresses an offer carries, and peers' relayed addresses remembered.
const MAX_RELAYED: usize = 2;
const MAX_RELAYED_OF: usize = 256;
/// Paths through TURN a dial takes at most.
const MAX_TURN_TARGETS: usize = 8;
/// Birthday punching, for a symmetric NAT with random ports against a cone: the
/// symmetric side opens `BIRTHDAY_SOCKETS` sockets towards the cone's address, each
/// with its own mapping, and the cone side probes `BIRTHDAY_PROBES` ports of the
/// symmetric side's IP, each once, and stops at the first that links. One of them
/// hits a mapping with probability 1 - (1 - 256/64512)^2048, about 99.97%
/// (theoretical); on average the hit comes after 64512/256 = 252 probes.
pub const BIRTHDAY_SOCKETS: u16 = 256;
pub const BIRTHDAY_PROBES: u16 = 2048;
/// After the offer, an ordinary punch gets this long first. The cone side starts
/// later, so the sockets are open when its probes arrive, though offers reach the
/// two sides a signaling delay apart.
const BIRTHDAY_OPEN_AFTER: Duration = Duration::from_millis(1000);
const BIRTHDAY_SPRAY_AFTER: Duration = Duration::from_millis(1500);
/// How long the extra sockets wait for a probe.
const BIRTHDAY_OPEN_FOR: Duration = Duration::from_secs(10);
/// One attempt per peer per `BIRTHDAY_COOLDOWN`; per node, a burst of
/// `BIRTHDAY_BURST`, then one per `BIRTHDAY_REFILL`.
const BIRTHDAY_COOLDOWN: Duration = Duration::from_secs(300);
const BIRTHDAY_BURST: u32 = 4;
const BIRTHDAY_REFILL: Duration = Duration::from_secs(30);
/// Per destination IP across all peers (NB44-1): at most `BIRTHDAY_IP_MAX` punches per
/// `BIRTHDAY_IP_WINDOW`, and only one running at a time (another to the same IP waits
/// `BIRTHDAY_IP_BUSY`, longer than a punch runs). The per-peer cooldown alone doesn't
/// bound traffic at one victim, because a room-key holder can mint any number of
/// PeerIds and aim them all at that IP; this caps what lands on any one address.
const BIRTHDAY_IP_MAX: usize = 2;
const BIRTHDAY_IP_WINDOW: Duration = Duration::from_secs(300);
const BIRTHDAY_IP_BUSY: Duration = Duration::from_secs(20);
/// Behind a NAT that maps per IP and port (`Symmetric`), facing a [`NatType::SymmetricIp`]
/// peer's sockets (NB-083), a node sprays only this many probes, as each costs it a port
/// kept for its mapping timeout (NB83-1): a try costs it about 1026 ports, not 2306. One
/// hits 256 sockets with probability 1 - (1 - 256/64512)^1024, about 98.3% (theoretical).
const PER_PORT_PROBES: u16 = 1024;
/// Such a spray could still use up much of the block of ports a carrier NAT gives its
/// user, so a node starts at most one per this, whichever peers ask: on top of the
/// limits above, which bound the punches of every kind.
const PER_PORT_PUNCH_GAP: Duration = Duration::from_secs(300);
/// Two-sided prediction, for two sequential symmetric NATs: both probe the other's
/// predicted ports in order, the side with the lower PeerId at `next + k·step`, the
/// other at `next + 2k·step`. Every probe goes to a new address, so each costs the
/// sender's NAT its next port. If the lower side's NAT handed out `dl` ports since
/// its prediction and the other's `dh`, the lower side's probe `dl + 2·dh` and the
/// other's probe `dl + dh` go from each other's destination to each other's
/// source: their mappings line up and one of the two gets through. The counts
/// cover `dl + 2·dh < 96` (and so `dl + dh < 64`): up to 31 ports on each side,
/// whichever has the lower PeerId. Same limits as a birthday punch.
pub const LOCKSTEP_LOW: u16 = 96;
pub const LOCKSTEP_HIGH: u16 = 64;
const LOCKSTEP_AFTER: Duration = Duration::from_millis(1000);
/// A link that [`Traversal::set_policy`] moves off a path that is now off gets this long to move (core punches for
/// `punch_duration`, 5 s by default), then is closed.
const MOVE_WAIT: Duration = Duration::from_secs(6);
/// Every [`PathKind::bit`]: a dial step with no policy.
const ALL_KINDS: u8 = 0b1_1111;

/// What STUN says about this node's NAT.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NatType {
    /// No STUN answer yet, or only one server answered.
    #[default]
    Unknown,
    /// No NAT: the public address is the socket's own.
    Open,
    /// The NAT keeps one public port for every destination (full, restricted or
    /// port-restricted cone). Punching works with other cone and open peers, and
    /// usually with symmetric ones, but not always: a port-restricted cone only
    /// lets in packets from ports it has sent to, so it can't punch a symmetric
    /// NAT that picks its ports at random. STUN can't tell the cone kinds apart.
    Cone,
    /// The NAT picks a new public port per destination: punching works towards
    /// open, full-cone and restricted-cone peers, towards port-restricted ones
    /// only if the ports come in a predictable sequence, and two symmetric peers
    /// need a relay, unless both are sequential, or one is [`SymmetricIp`](NatType::SymmetricIp)
    /// and lets in any port of an IP it has sent to.
    Symmetric,
    /// A symmetric NAT that picks a new public port per destination IP, not per
    /// IP and port: two ports of one STUN server saw one mapping, two servers two.
    /// Against another symmetric peer this side opens extra sockets and the peer
    /// probes them (both do both if the peer is `SymmetricIp` too; two that predict
    /// their ports sweep in lockstep instead), which links if this NAT also lets in
    /// any port of an IP it has sent to. Offers carry it as [`Symmetric`](NatType::Symmetric)
    /// plus a tag that older peers skip, so they see `Symmetric`. Needs a STUN
    /// server listed on two ports of one IP ([`DEFAULT_STUN_SERVERS`](crate::DEFAULT_STUN_SERVERS) does).
    SymmetricIp,
}

impl NatType {
    /// [`Symmetric`](NatType::Symmetric) or [`SymmetricIp`](NatType::SymmetricIp): a new
    /// public port per destination.
    fn symmetric(self) -> bool {
        matches!(self, NatType::Symmetric | NatType::SymmetricIp)
    }
}

/// How a node takes part in a room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A full mesh: connect to every peer and host in the room.
    Peer,
    /// A star centre: connect to every client and peer.
    Host,
    /// A star leaf: connect to hosts only.
    Client,
}

/// Which of its addresses a node gives out. Peers it links with directly still see
/// the address its packets come from; only [`Privacy::RelayOnly`] hides that.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Privacy {
    /// Every candidate (public, LAN, loopback): the lowest latency, and peers on
    /// one LAN link over it.
    #[default]
    Direct,
    /// Public addresses only: LAN, loopback, link-local, CGNAT (100.64/10) and
    /// other private ranges are left out of invites and offers. Two peers behind
    /// one NAT then link only if it hairpins, or through a relay.
    PublicOnly,
    /// No addresses at all, except to the relay: every other link goes through
    /// it ([`Path::Via`]), and never moves to a direct path. The relay is `via`,
    /// or, in a host/client room, the host. Costs the relay's upload (it carries
    /// every packet twice) and a detour in latency.
    RelayOnly { via: Option<PeerId> },
}

/// A way a link can reach its peer, as a [`Policy`] orders them. More kinds may come, so a `match` on one outside
/// this crate needs a `_` arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PathKind {
    /// Straight to the peer's addresses: those its offers and invites carry (LAN, public from STUN, mapped by its
    /// router, seen by its peers), the ports around one its NAT will likely pick, and the punches that move a link
    /// on a relay to one of them.
    Direct,
    /// The punches planned for symmetric NATs, which an ordinary one can't link: a birthday punch (extra sockets
    /// and a spray of probes, [`BIRTHDAY_SOCKETS`] and [`BIRTHDAY_PROBES`]) or a lockstep sweep. Each costs NAT
    /// ports, and they are rate-limited.
    Birthday,
    /// Through another peer linked to both ([`Path::Via`]), such as a room member. A peer relays only over its
    /// direct links, so with `Direct` and `Birthday` off only relay peers can relay.
    Peers,
    /// Through this node's own always-on relay peers ([`Node::set_relay_peers`](crate::Node::set_relay_peers)), as
    /// you would run your own TURN server. Left out, they aren't linked at all.
    RelayPeers,
    /// Through a TURN server (feature `turn`): this node's own (allocated only with this kind in the order; see
    /// [`Node::set_turn_servers`](crate::Node::set_turn_servers)), or the peer's relayed address on its own. A link
    /// on one moves only to `Direct` or `Birthday`, when ranked earlier, never to a relay peer.
    Turn,
}

impl PathKind {
    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Which paths a node's links may take, in the order it prefers them ([`Node::set_policy`](crate::Node::set_policy)).
/// A node with none takes every path, as [`Node`](crate::Node) always has: a dial tries the direct and relayed
/// paths at once and the planned punches a second later, and a relayed link moves to a direct path that works.
///
/// With a policy, a dial takes the kinds in `order` one step at a time: each next one starts once the earlier ones
/// have had `fallback` to link, at once if an earlier one has nothing to try (no relay linked to both, say).
/// `Direct` and `Birthday` side by side are one step, as the planned punches wait a second for an ordinary one
/// anyway. A link moves only to a kind earlier in the order: one through a relay ranked before `Direct` and
/// `Birthday` never punches (nor answers the peer's punches), so `[Peers, Direct]` keeps a relayed link relayed,
/// and `[RelayPeers]` links through relay peers alone. The node still answers a dial on any path it allows (a peer
/// with another policy may dial it directly first), and refuses links on paths it doesn't. Under
/// [`Privacy::RelayOnly`] links go through its relay whatever the policy says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The kinds of path allowed, most preferred first. A kind left out is off. Not empty, each kind at most once.
    pub order: Vec<PathKind>,
    /// Router port mapping ([`Builder::port_mapping`](crate::Builder::port_mapping)), which runs only with
    /// `Direct` in `order`. Default: off.
    pub port_mapping: bool,
    /// How long each step of a dial has to link before the next one starts too. Default: [`Policy::FALLBACK`].
    pub fallback: Duration,
}

impl Policy {
    /// The default `fallback`: a direct punch between two NATs it can get through mostly links within 2.5 s in the
    /// simulator's NAT matrix.
    pub const FALLBACK: Duration = Duration::from_secs(2);

    /// A policy with this `order`, no port mapping and the default `fallback`.
    pub fn new(order: impl IntoIterator<Item = PathKind>) -> Policy {
        Policy { order: order.into_iter().collect(), ..Policy::default() }
    }

    /// Whether `kind` is in the order.
    pub fn allows(&self, kind: PathKind) -> bool {
        self.order.contains(&kind)
    }

    /// Whether the order lists at least one kind, and none twice.
    pub fn is_valid(&self) -> bool {
        let mut seen = 0u8;
        for k in &self.order {
            if seen & k.bit() != 0 {
                return false;
            }
            seen |= k.bit();
        }
        seen != 0
    }

    /// Where `kind` comes in the order, or `None` if it is off.
    fn rank(&self, kind: PathKind) -> Option<usize> {
        self.order.iter().position(|k| *k == kind)
    }

    /// Where a direct path ranks: the earlier of `Direct` and `Birthday`.
    fn direct_rank(&self) -> Option<usize> {
        [self.rank(PathKind::Direct), self.rank(PathKind::Birthday)].into_iter().flatten().min()
    }

    /// Whether links through a relay of `kind` stay on it: it ranks before both direct kinds.
    fn keeps(&self, kind: PathKind) -> bool {
        self.rank(kind).is_some_and(|r| self.direct_rank().is_none_or(|d| r < d))
    }

    /// The steps of a dial, as bits of [`PathKind::bit`]: each kind in order, `Direct` and `Birthday` side by side
    /// together.
    fn steps(&self) -> Vec<u8> {
        let direct = PathKind::Direct.bit() | PathKind::Birthday.bit();
        let mut steps: Vec<u8> = Vec::new();
        for k in &self.order {
            match steps.last_mut() {
                Some(last) if *last & direct != 0 && k.bit() & direct != 0 => *last |= k.bit(),
                _ => steps.push(k.bit()),
            }
        }
        steps
    }
}

impl Default for Policy {
    /// Every kind but `Turn`, in the order `Node` prefers paths with no policy: `[Direct, Birthday, RelayPeers,
    /// Peers]`. Unlike no policy, the relays wait their turn (`fallback`). List `Turn` to use TURN.
    fn default() -> Policy {
        let order = vec![PathKind::Direct, PathKind::Birthday, PathKind::RelayPeers, PathKind::Peers];
        Policy { order, port_mapping: false, fallback: Policy::FALLBACK }
    }
}

/// Why a dial to a peer failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// No handshake answer on any address: the peer is gone, or a firewall drops UDP.
    Unreachable,
    /// The NAT on `Side` is symmetric (a new public port per destination) and the
    /// other side's router only lets in packets from ports it has sent to, so
    /// punching can't work, and no peer connected to both could relay. Peerwire runs
    /// no TURN server; a third peer in the room that both can reach fixes it, and so
    /// does a TURN server of your own ([`PathKind::Turn`], feature `turn`).
    Symmetric(Side),
}

/// Which end of a link a [`Failure`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// This node.
    Ours,
    /// The peer.
    Theirs,
    /// Both.
    Both,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sym = "is symmetric (a new public port per destination)";
        let filters = "router only lets in packets from ports it has sent to";
        let end = "so hole punching can't work, and no peer linked to both could relay";
        match self {
            Failure::Unreachable => write!(f, "no answer on any address: the peer is gone, or a firewall drops UDP"),
            Failure::Symmetric(Side::Ours) => write!(f, "this side's NAT {sym} and the peer's {filters}, {end}"),
            Failure::Symmetric(Side::Theirs) => write!(f, "the peer's NAT {sym} and this side's {filters}, {end}"),
            Failure::Symmetric(Side::Both) => write!(f, "both NATs are symmetric (a new port per destination), {end}"),
        }
    }
}

/// An address as a host with TURN gives it to the [`Endpoint`]: a network address, a peer reached through this
/// node's allocation on one of its TURN servers, or a peer's relayed address (on the peer's TURN server), sent to
/// from this node's own socket. The last two are [`relayed`](Address::relayed): a link on one punches to the peer's
/// network addresses and moves to the first that answers. On the wire each is its network address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hop<A> {
    Net(A),
    Turn { server: A, peer: A },
    Relayed(A),
}

impl<A> Hop<A> {
    /// The network address: the peer's, for `Turn`.
    pub fn addr(&self) -> &A {
        match self {
            Hop::Net(a) | Hop::Relayed(a) | Hop::Turn { peer: a, .. } => a,
        }
    }
}

impl<A: Address> Address for Hop<A> {
    fn encode(&self, out: &mut Vec<u8>) {
        self.addr().encode(out)
    }
    fn decode(b: &[u8]) -> Option<Self> {
        A::decode(b).map(Hop::Net)
    }
    fn port_offset(&self, delta: i32) -> Option<Self> {
        match self {
            Hop::Net(a) => a.port_offset(delta).map(Hop::Net),
            _ => None,
        }
    }
    fn to_socket_addr(&self) -> Option<std::net::SocketAddr> {
        self.addr().to_socket_addr()
    }
    fn from_socket_addr(addr: std::net::SocketAddr) -> Option<Self> {
        A::from_socket_addr(addr).map(Hop::Net)
    }
    fn relayed(&self) -> bool {
        !matches!(self, Hop::Net(_))
    }
    fn turn_hop(server: &Self, peer: std::net::SocketAddr) -> Option<Self> {
        let Hop::Net(server) = server else { return None };
        Some(Hop::Turn { server: server.clone(), peer: A::from_socket_addr(peer)? })
    }
    fn turn_parts(&self) -> Option<(Self, std::net::SocketAddr)> {
        match self {
            Hop::Turn { server, peer } => Some((Hop::Net(server.clone()), peer.to_socket_addr()?)),
            _ => None,
        }
    }
    fn relayed_hop(addr: &Self) -> Option<Self> {
        match addr {
            Hop::Net(a) => Some(Hop::Relayed(a.clone())),
            _ => None,
        }
    }
}

/// Extra sockets for birthday punching, which the host opens: see
/// [`Traversal::poll_birthday`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Birthday<A> {
    /// The peer the sockets are for.
    pub peer: PeerId,
    /// Where each socket sends `payload` once: the peer's public address.
    pub dst: A,
    pub sockets: u16,
    pub payload: Vec<u8>,
    /// Sockets that have heard nothing by then are closed.
    pub until: Instant,
}

/// A punch planned from the two NAT types, past an ordinary one.
#[derive(Clone, Debug)]
enum Plan<A> {
    /// Birthday punch, behind the symmetric NAT (or the [`NatType::SymmetricIp`] one facing
    /// one that maps per IP and port): open sockets towards the peer's address.
    Open(A),
    /// Birthday punch, behind the cone (or the NAT that maps per IP and port, facing a
    /// `SymmetricIp` one, at most [`PER_PORT_PROBES`]): spray ports of the peer's IP.
    Spray(A),
    /// Two sequential symmetric NATs: probe these, in order ([`LOCKSTEP_LOW`]).
    Sweep(Vec<A>),
    /// Two [`NatType::SymmetricIp`] sides (NB-083): both roles of a birthday punch, the
    /// sockets now, then [`Plan::Then`].
    Both(A),
    /// The spray half of [`Plan::Both`]: part of a punch already started and counted.
    Then(A),
}

/// The destination IP a planned punch aims at (all its targets share one IP), for the
/// NB44-1 per-IP cap. `None` for an address that isn't an IP.
fn plan_ip<A: Address>(side: &Plan<A>) -> Option<std::net::IpAddr> {
    let a = match side {
        Plan::Open(a) | Plan::Spray(a) | Plan::Both(a) | Plan::Then(a) => Some(a),
        Plan::Sweep(v) => v.first(),
    };
    a.and_then(|a| a.to_socket_addr()).map(|s| s.ip().to_canonical())
}

/// Something the host must do on its signaling carrier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    Subscribe(Topic),
    Unsubscribe(Topic),
    Publish(Topic, Vec<u8>),
}

struct Server<A> {
    addr: A,
    tx: stun::TxId,
    sent_at: Instant,
    tries: u32,
    pending: bool,
    mapped: Option<A>,
}

/// A token bucket that starts full: `spent` tokens come back one per refill period.
#[derive(Clone, Copy, Default)]
struct Bucket {
    spent: u32,
    at: Instant,
}

impl Bucket {
    /// Refills, then says whether a token is left.
    fn ready(&mut self, now: Instant, burst: u32, every: Duration) -> bool {
        while self.spent > 0 && now >= self.at + every {
            self.spent -= 1;
            self.at = self.at + every;
        }
        if self.spent == 0 {
            self.at = now;
        }
        self.spent < burst
    }

    fn take(&mut self, now: Instant, burst: u32, every: Duration) -> bool {
        let ok = self.ready(now, burst, every);
        self.spent += ok as u32;
        ok
    }
}

struct Remote<A> {
    offered_at: Option<Instant>,
    /// We received an offer from it.
    heard: bool,
    /// We dialled it from an invite: its plain offers are welcome (in a room too).
    invited: bool,
    /// Its invite's key, which our offers to it prove we hold.
    key: Option<[u8; 32]>,
    /// The role it proved in the current room (with the room key).
    member: Option<Role>,
    nat: NatType,
    failure: Option<Failure>,
    /// The links it named in its last offer acted on: if one links with us later,
    /// we dial it through that one (we never name ours to it to make that happen).
    relays: Vec<PeerId>,
    seen_at: Instant,
    /// Its offers acted on.
    offers: Bucket,
    /// A punch to start then (birthday or lockstep), and when the last one started.
    plan: Option<(Instant, Plan<A>)>,
    plan_at: Option<Instant>,
    /// The addresses in its last offer acted on that this node can dial: a link moved off a relay punches to them.
    addrs: Vec<A>,
    /// The relayed addresses (its TURN allocations') in its last offer acted on.
    relayed: Vec<A>,
    /// A dial to it under a [`Policy`], step by step.
    ladder: Option<Ladder<A>>,
    /// Its link is moving off a path that is off, and is closed if it hasn't by then.
    moving: Option<Instant>,
    /// A dial that moves its link ([`Endpoint::redial`]) is under way: its `ConnectFailed` is this node's own.
    redial: bool,
}

/// A dial run in the steps of a [`Policy`]: the offer it dials, the step to start next, and when.
struct Ladder<A> {
    offer: Offer<A>,
    next: usize,
    at: Instant,
    /// The offer stands for an invite, whose addresses the host checked: dialled as they are.
    invite: bool,
}

impl<A> Default for Remote<A> {
    fn default() -> Self {
        Remote {
            offered_at: None,
            heard: false,
            invited: false,
            key: None,
            member: None,
            nat: NatType::Unknown,
            failure: None,
            relays: Vec::new(),
            seen_at: Instant::ZERO,
            offers: Bucket::default(),
            plan: None,
            plan_at: None,
            addrs: Vec::new(),
            relayed: Vec::new(),
            ladder: None,
            moving: None,
            redial: false,
        }
    }
}

struct Room {
    topic: Topic,
    key: [u8; 32],
    /// What hellos are sealed under ([`room_enc_key`]).
    enc: [u8; 32],
    role: Role,
    /// Link only with this peer (a client that knows its host's key).
    expect: Option<PeerId>,
    next_hello: Instant,
    interval: Duration,
}

/// STUN, presence and candidate exchange for one [`Endpoint`]. See the module docs.
pub struct Traversal<A: Address> {
    me: Identity,
    id: PeerId,
    inbox: Topic,
    rng: ChaCha20Rng,
    unix_ms: u64,
    local: Vec<A>,
    /// The socket is bound to loopback: loopback candidates are real.
    loopback: bool,
    servers: Vec<Server<A>>,
    next_round: Instant,
    public: Option<A>,
    nat: NatType,
    /// The port a sequential symmetric NAT will likely pick next, at `public`'s IP.
    prediction: Option<Prediction>,
    /// A port the router forwards to this node (router port mapping), if public.
    mapped: Option<A>,
    observed: Vec<A>,
    room: Option<Room>,
    remotes: BTreeMap<PeerId, Remote<A>>,
    nonces: HashMap<(PeerId, u64), Instant>,
    hellos: Bucket,
    offers: Bucket,
    /// Inbox messages opened, proved or not.
    opened: Bucket,
    privacy: Privacy,
    /// The room Host a relay-only node picked as its relay (with no `via` and no
    /// pinned host): the first it dialled. Kept while that dial or link lasts.
    picked: Option<PeerId>,
    /// Whoever holds this, from our invite, may ask us to dial back.
    invite_key: [u8; 32],
    /// Always-on peers kept linked as relays: named first in offers, tried first as vias.
    relay_peers: Vec<PeerId>,
    /// Planned punches started, node-wide.
    planned: Bucket,
    /// Recent birthday-punch starts per destination IP, for the NB44-1 per-IP cap.
    punch_ips: HashMap<std::net::IpAddr, VecDeque<Instant>>,
    /// When this node last started a spray as `Symmetric` ([`PER_PORT_PUNCH_GAP`]).
    per_port_punch: Option<Instant>,
    /// The paths links may take; `None`: every one, as before policies.
    policy: Option<Policy>,
    /// The endpoint's kept relays ([`Endpoint::set_keep_relayed`]) need setting again (the policy or relay peers
    /// changed).
    keep_stale: bool,
    /// Peers whose links this node closed because their path is off ([`Traversal::poll_dropped`]).
    dropped: VecDeque<PeerId>,
    /// This node's TURN servers and allocations.
    turn: TurnSet<A>,
    /// The privacy changed: TURN starts or stops at the next timer, at once.
    turn_stale: bool,
    /// Peers' relayed addresses, from their offers: what comes from one is from a [relayed hop](Address::relayed_hop).
    relayed_of: HashMap<A, PeerId>,
    sockets: VecDeque<Birthday<A>>,
    transmits: VecDeque<Transmit<A>>,
    signals: VecDeque<Signal>,
}

impl<A: Address> Traversal<A> {
    /// `local`: the addresses the socket can be reached at on its own networks.
    /// `stun_servers`: servers to ask (empty: no STUN). `unix_ms`: wall-clock time,
    /// in Unix milliseconds, at [`Instant::ZERO`] (offers carry it, against replay).
    /// `seed`: fresh random bytes.
    pub fn new(identity: &Identity, seed: [u8; 32], unix_ms: u64, local: Vec<A>, stun_servers: Vec<A>) -> Self {
        let id = identity.peer_id();
        let inbox = Topic::peer(&id);
        let servers = stun_servers
            .into_iter()
            .map(|addr| Server { addr, tx: [0; 12], sent_at: Instant::ZERO, tries: 0, pending: false, mapped: None })
            .collect();
        let is_loopback = |a: &A| a.to_socket_addr().is_some_and(|s| s.ip().to_canonical().is_loopback());
        let mut rng = ChaCha20Rng::from_seed(seed);
        let mut turn_seed = seed; // its own stream: the traversal's draws stay as they were
        turn_seed[31] ^= 0x75;
        let mut invite_key = [0u8; 32];
        rng.fill_bytes(&mut invite_key);
        Traversal {
            me: identity.clone(),
            id,
            inbox,
            rng,
            unix_ms,
            loopback: !local.is_empty() && local.iter().all(is_loopback),
            local,
            servers,
            next_round: Instant::ZERO,
            public: None,
            nat: NatType::Unknown,
            prediction: None,
            mapped: None,
            observed: Vec::new(),
            room: None,
            remotes: BTreeMap::new(),
            nonces: HashMap::new(),
            hellos: Bucket::default(),
            offers: Bucket::default(),
            opened: Bucket::default(),
            privacy: Privacy::Direct,
            picked: None,
            invite_key,
            relay_peers: Vec::new(),
            planned: Bucket::default(),
            punch_ips: HashMap::new(),
            per_port_punch: None,
            policy: None,
            keep_stale: false,
            dropped: VecDeque::new(),
            turn: TurnSet::new(unix_ms, turn_seed),
            turn_stale: false,
            relayed_of: HashMap::new(),
            sockets: VecDeque::new(),
            transmits: VecDeque::new(),
            signals: VecDeque::from([Signal::Subscribe(inbox)]),
        }
    }

    /// Whether the socket can send to `a`: its family is one of the local
    /// addresses' (a dual-stack socket has both). Non-IP addresses always pass.
    pub fn reaches(&self, a: &A) -> bool {
        let v6 = |x: &A| x.to_socket_addr().map(|s| s.ip().to_canonical().is_ipv6());
        match v6(a) {
            Some(fam) => self.local.is_empty() || self.local.iter().any(|l| v6(l).is_none_or(|f| f == fam)),
            None => true,
        }
    }

    /// The public address STUN reported, if any.
    pub fn public_addr(&self) -> Option<&A> {
        self.public.as_ref()
    }

    /// What STUN says about the NAT.
    pub fn nat_type(&self) -> NatType {
        self.nat
    }

    /// Where a symmetric NAT that hands out ports in sequence will likely map
    /// this node's next new destination, from the step between its two STUN
    /// mappings. Offers carry it, and cone peers probe the ports around it.
    pub fn predicted_addr(&self) -> Option<A> {
        let (public, p) = (self.public.as_ref()?, self.prediction?);
        public.port_offset(i32::from(p.next) - i32::from(public.to_socket_addr()?.port()))
    }

    /// Why the last dial to `peer` failed, until it connects.
    pub fn failure(&self, peer: &PeerId) -> Option<Failure> {
        self.remotes.get(peer).and_then(|r| r.failure)
    }

    /// The addresses given out (in invites), as [`Privacy`] allows: public (STUN),
    /// mapped by the router, observed by peers, local. None under [`Privacy::RelayOnly`].
    pub fn candidates(&self) -> Vec<A> {
        match self.privacy {
            Privacy::Direct => self.addresses(false),
            Privacy::PublicOnly => self.addresses(true),
            Privacy::RelayOnly { .. } => Vec::new(),
        }
    }

    /// The addresses offered to `to`: [`candidates`](Traversal::candidates), but
    /// a relay-only node's relay gets its public ones (it links directly).
    fn candidates_for(&self, to: &PeerId) -> Vec<A> {
        match self.privacy {
            Privacy::RelayOnly { .. } if self.relay() == Some(*to) => self.addresses(true),
            _ => self.candidates(),
        }
    }

    fn addresses(&self, public_only: bool) -> Vec<A> {
        let mut out: Vec<A> = Vec::new();
        for a in self.public.iter().chain(&self.mapped).chain(&self.observed).chain(&self.local) {
            if !out.contains(a) && out.len() < MAX_CANDIDATES && (!public_only || is_public(a)) {
                out.push(a.clone());
            }
        }
        out
    }

    /// A public address the router forwards to this node's socket (from router
    /// port mapping), offered after the STUN address. Ignored unless it is public.
    pub fn set_mapped(&mut self, addr: Option<A>) {
        self.mapped = addr.filter(|a| is_public(a) && dialable(a, false));
    }

    /// Which addresses this node gives out.
    pub fn privacy(&self) -> Privacy {
        self.privacy
    }

    /// Sets which addresses this node gives out from now on (offers and invites
    /// made later), and, for [`Privacy::RelayOnly`], keeps `ep`'s relayed links
    /// relayed. Links already up keep their path.
    pub fn set_privacy(&mut self, ep: &mut Endpoint<A>, privacy: Privacy) {
        self.privacy = privacy;
        self.turn_stale = true; // TURN is off under relay-only
        ep.set_relay_only(matches!(privacy, Privacy::RelayOnly { .. }));
    }

    /// The peer a [`Privacy::RelayOnly`] node links through: `via`, else the room's
    /// pinned host, else the first peer that proved the Host role and was dialled.
    /// That pick sticks while its dial or link lasts: another Host can't take over.
    pub fn relay(&self) -> Option<PeerId> {
        let Privacy::RelayOnly { via } = self.privacy else { return None };
        via.or_else(|| self.room.as_ref()?.expect).or(self.picked)
    }

    /// Always-on peers (such as `peerwire-peer`) this node keeps linked so that
    /// peers it can't punch to can reach it through one: its offers name them
    /// first among its links, and its dials try them first among the relays an
    /// offer names. Linking with them is the host's job ([`Node`](crate::Node) does it).
    pub fn set_relay_peers(&mut self, peers: Vec<PeerId>) {
        self.relay_peers = peers;
        self.keep_stale = true;
    }

    /// The paths links may take; `None`: every one.
    pub fn policy(&self) -> Option<&Policy> {
        self.policy.as_ref()
    }

    /// Sets the paths links may take (`None`: every one, as before policies), for new dials at once; dials under
    /// way take their next steps by it, from its first. A link on a path it turns off moves: one through a relay
    /// punches to the peer's addresses if `Direct` is on and offers again, so the peer punches too (and a planned
    /// punch may follow, with `Birthday` on); and one with an allowed relay linked to both (named in the peer's last
    /// offer, or a relay peer) is dialled again through it ([`Endpoint::redial`]), keeping the old link until the
    /// new one replaces it. A link that had nowhere to go, whose redial failed, or that is still on its path after
    /// 6 s is closed: see [`poll_dropped`](Traversal::poll_dropped). Links to relay peers and those under
    /// [`Privacy::RelayOnly`] are left alone. The host checks [`Policy::is_valid`].
    pub fn set_policy(&mut self, now: Instant, ep: &mut Endpoint<A>, policy: Option<Policy>) {
        self.policy = policy;
        self.keep_stale = true;
        self.sync_keep(ep);
        self.sync_turn(now, ep);
        let ladders: Vec<PeerId> = self.remotes.iter().filter(|(_, r)| r.ladder.is_some()).map(|(p, _)| *p).collect();
        for r in self.remotes.values_mut() {
            r.moving = None;
        }
        for peer in ladders {
            if self.policy.is_some() {
                if let Some(l) = self.remotes.get_mut(&peer).and_then(|r| r.ladder.as_mut()) {
                    (l.next, l.at) = (0, now);
                }
                self.climb(now, ep, peer);
            } else if let Some(l) = self.remotes.get_mut(&peer).and_then(|r| r.ladder.take()) {
                // Every path at once, as an offer is dialled with no policy.
                self.dial_step(now, ep, peer, &l.offer, ALL_KINDS, l.invite);
            }
        }
        let linked: Vec<PeerId> = ep.connected_peers().copied().collect();
        let direct = self.policy.as_ref().is_some_and(|p| p.allows(PathKind::Direct));
        for peer in linked {
            match ep.path(&peer) {
                Some(path) if !self.allows(&peer, path) => self.move_off(now, ep, peer),
                // A link at a relayed address the policy no longer keeps there punches to the peer's addresses,
                // and asks for its offer, so the peer punches too: it moves once one answers.
                Some(Path::Direct(a)) if direct && a.relayed() && !self.kept(&Path::Direct(a.clone())) => {
                    let addrs = self.remotes.get(&peer).map(|r| r.addrs.clone()).unwrap_or_default();
                    ep.add_candidates(now, &peer, &addrs);
                    let room = self.room_role(&peer).is_some();
                    self.offer(now, ep, peer, true, room);
                }
                _ => {}
            }
        }
    }

    /// The next peer whose link this node closed because the policy turned its path off and no allowed path took
    /// it over (see [`set_policy`](Traversal::set_policy)). The endpoint raises no event for it: the host reports it
    /// ([`Node`](crate::Node) as [`Event::Disconnected`] with [`DisconnectReason::PathOff`]).
    ///
    /// [`DisconnectReason::PathOff`]: peerwire_core::DisconnectReason::PathOff
    pub fn poll_dropped(&mut self) -> Option<PeerId> {
        self.dropped.pop_front()
    }

    /// Whether the policy lets a link to `peer` take `path`. Any path with no policy, under
    /// [`Privacy::RelayOnly`], and to a relay peer (the host links those, or not, by [`PathKind::RelayPeers`]).
    /// An address `peer` offered as its relayed one is TURN's, as its link there is once retagged (NB99-I1).
    pub fn allows(&self, peer: &PeerId, path: &Path<A>) -> bool {
        let Some(p) = &self.policy else { return true };
        if matches!(self.privacy, Privacy::RelayOnly { .. }) || self.relay_peers.contains(peer) {
            return true;
        }
        match path {
            Path::Direct(a) if a.relayed() || self.relayed_of.get(a) == Some(peer) => p.allows(PathKind::Turn),
            Path::Direct(_) => p.allows(PathKind::Direct) || p.allows(PathKind::Birthday),
            Path::Via(r) => p.allows(self.relay_kind(r)),
        }
    }

    /// A link to `peer` came up or moved to `path`: if the policy has that path off, the link moves (or goes);
    /// if not, any move is over. Whether the path is allowed.
    fn follow_policy(&mut self, now: Instant, ep: &mut Endpoint<A>, peer: PeerId, path: &Path<A>) -> bool {
        if !self.allows(&peer, path) {
            self.move_off(now, ep, peer);
            return false;
        }
        if let Some(r) = self.remotes.get_mut(&peer) {
            r.moving = None;
        }
        true
    }

    /// The kind of path a link through relay `r` takes.
    fn relay_kind(&self, r: &PeerId) -> PathKind {
        match self.relay_peers.contains(r) {
            true => PathKind::RelayPeers,
            false => PathKind::Peers,
        }
    }

    /// Whether a link on `path` stays on its relay: the policy ranks the relay's kind (TURN's, at a relayed address)
    /// before both direct kinds.
    fn kept(&self, path: &Path<A>) -> bool {
        let kind = match path {
            Path::Via(r) => self.relay_kind(r),
            Path::Direct(a) if a.relayed() => PathKind::Turn,
            Path::Direct(_) => return false,
        };
        self.policy.as_ref().is_some_and(|p| p.keeps(kind))
    }

    /// Tells the endpoint which relayed links stay relayed, if the policy or the relay peers changed.
    fn sync_keep(&mut self, ep: &mut Endpoint<A>) {
        if self.keep_stale {
            self.keep_stale = false;
            let keeps = |k| self.policy.as_ref().is_some_and(|p| p.keeps(k));
            let (listed, others) = (keeps(PathKind::RelayPeers), keeps(PathKind::Peers));
            ep.set_keep_relayed(&self.relay_peers, listed, others);
            ep.set_keep_turn(keeps(PathKind::Turn));
        }
    }

    /// Replaces the STUN servers (empty: none) and asks the new ones at once. The public address and NAT type stay
    /// until they answer; with none, they are forgotten.
    pub fn set_stun_servers(&mut self, servers: Vec<A>) {
        let server =
            |addr| Server { addr, tx: [0; 12], sent_at: Instant::ZERO, tries: 0, pending: false, mapped: None };
        self.servers = servers.into_iter().map(server).collect();
        self.next_round = Instant::ZERO;
        if self.servers.is_empty() {
            (self.public, self.nat, self.prediction) = (None, NatType::Unknown, None);
        }
    }

    /// Subscribes the inbox and the room's topic again and says hello in the room at once, then at 2 s, 4 s, ...:
    /// for a new signaling carrier, which has heard nothing yet.
    pub fn resubscribe(&mut self, now: Instant) {
        self.signals.push_back(Signal::Subscribe(self.inbox));
        if let Some(r) = self.room.as_mut() {
            self.signals.push_back(Signal::Subscribe(r.topic));
            (r.interval, r.next_hello) = (HELLO_FIRST, now + HELLO_FIRST);
        }
        self.announce(now);
    }

    /// The key this node's invites carry: an offer proved with it is from a peer
    /// holding one, which may ask this node to dial back.
    pub fn invite_key(&self) -> [u8; 32] {
        self.invite_key
    }

    /// Joins a room: announces this node on the room topic now and now and then,
    /// and dials the peers that answer (as `role` allows) once they prove they know
    /// the room's name. With `host`, links with that peer only. Leaves any other room.
    pub fn join_room(&mut self, now: Instant, app_id: &str, room: &str, role: Role, host: Option<PeerId>) {
        self.leave_room();
        self.picked = None;
        let topic = Topic::room(app_id, room);
        let (key, enc) = (room_key(app_id, room), room_enc_key(app_id, room));
        self.signals.push_back(Signal::Subscribe(topic));
        self.room =
            Some(Room { topic, key, enc, role, expect: host, next_hello: now + HELLO_FIRST, interval: HELLO_FIRST });
        self.announce(now);
    }

    /// Publishes this node's hello on the room topic, sealed for the room.
    fn announce(&mut self, now: Instant) {
        let Some(r) = &self.room else { return };
        let mut nonce = [0; NONCE_LEN];
        self.rng.fill_bytes(&mut nonce);
        let h = hello(&r.key, &self.id, r.role, self.wall_ms(now));
        self.signals.push_back(Signal::Publish(r.topic, seal_room(&r.enc, nonce, &h)));
    }

    /// Stops announcing and answering in the current room. Links stay up.
    pub fn leave_room(&mut self) {
        if let Some(r) = self.room.take() {
            self.signals.push_back(Signal::Unsubscribe(r.topic));
        }
        for r in self.remotes.values_mut() {
            r.member = None;
        }
    }

    /// The role `peer` proved in the current room, if it did.
    pub fn room_role(&self, peer: &PeerId) -> Option<Role> {
        self.room.as_ref()?;
        self.remotes.get(peer)?.member
    }

    /// Whether a link with `peer` belongs: outside a room, any; in one, a peer that
    /// proved the room key in a role this node links with, or one it dialled itself.
    pub fn admits(&self, peer: &PeerId) -> bool {
        let Some(room) = &self.room else { return true };
        let Some(r) = self.remotes.get(peer) else { return false };
        r.invited
            || r.member.is_some_and(|role| allowed(Some(room.role), role) && room.expect.is_none_or(|h| h == *peer))
    }

    /// Sends `peer` an offer asking for one back, so a dial to it punches through
    /// NATs: call it after dialling a peer from an invite, with the invite's `key`
    /// (without one, the peer ignores the offer unless it dialled us too). Its
    /// offers are welcome from then on, in a room too, though it never proved the
    /// room key.
    pub fn introduce(&mut self, now: Instant, ep: &Endpoint<A>, peer: PeerId, key: Option<[u8; 32]>) {
        let r = self.remotes.entry(peer).or_default();
        (r.invited, r.key) = (true, key.or(r.key));
        self.offer(now, ep, peer, true, false);
    }

    /// Takes a datagram if it is STUN (returns `true`); otherwise it is for the endpoint.
    pub fn handle_stun(&mut self, from: &A, data: &[u8]) -> bool {
        if !stun::is_stun(data) {
            return false;
        }
        let loopback = self.loopback;
        for s in self.servers.iter_mut().filter(|s| s.pending && &s.addr == from) {
            let m = stun::parse_binding_response(data, &s.tx).and_then(A::from_socket_addr);
            if let Some(m) = m.filter(|m| dialable(m, loopback)) {
                s.mapped = Some(m);
                s.pending = false;
            }
        }
        self.classify();
        true
    }

    /// Handles a message from the signaling carrier.
    pub fn handle_signal(&mut self, now: Instant, ep: &mut Endpoint<A>, topic: Topic, payload: &[u8]) {
        self.sync_keep(ep);
        if let Some(room) = self.room.as_ref().filter(|r| r.topic == topic) {
            // A hello that doesn't open under the room's key (plaintext, another room's, altered) is dropped.
            let Some(body) = open_room(&room.enc, payload) else { return };
            let Some((from, role, ts, proof)) = parse_hello(&body) else { return };
            let fresh = self.wall_ms(now).abs_diff(ts) <= FRESH_MS;
            if fresh && mac_ok(&room.key, &[b"pw-hello", from.as_bytes(), &[role as u8], &ts.to_le_bytes()], &proof) {
                self.on_hello(now, ep, from, role);
            }
        } else if topic == self.inbox {
            // Spent before the open, the costly part, so a flood of junk waits like the rest.
            if !self.opened.take(now, INBOX_BURST, INBOX_REFILL) {
                return;
            }
            let Some((from, body)) = sealed::open(&self.me, payload) else { return };
            // Room and invite offers end in an HMAC over sender, recipient and body,
            // under the room key or this node's invite key.
            let (body, proof) = match body.first() {
                Some(&(ROOM_OFFER | INVITE_OFFER)) if body.len() > PROOF_LEN => {
                    let (b, p) = body.split_at(body.len() - PROOF_LEN);
                    (b, Some(p))
                }
                _ => (&body[..], None),
            };
            let parts = |label: &'static [u8]| [label, from.as_bytes(), self.id.as_bytes(), body];
            let welcome = match (body.first(), proof) {
                (Some(&ROOM_OFFER), Some(p)) => {
                    self.room.as_ref().is_some_and(|r| mac_ok(&r.key, &parts(b"pw-offer"), p))
                }
                (Some(&INVITE_OFFER), Some(p)) => mac_ok(&self.invite_key, &parts(b"pw-invite"), p),
                // Plain offers only from peers this node dialled itself.
                (Some(&OFFER), None) => self.remotes.get(&from).is_some_and(|r| r.invited),
                // A room or invite offer too short to carry a proof.
                _ => false,
            };
            if let Some(offer) = Offer::decode(body).filter(|_| welcome) {
                self.on_offer(now, ep, from, offer);
            }
        }
    }

    /// Feeds an event from the endpoint (call for every one, before handling it).
    pub fn handle_event(&mut self, now: Instant, ep: &mut Endpoint<A>, event: &Event<A>) {
        self.sync_keep(ep);
        match event {
            Event::Connected { peer, path, .. } => {
                if let Some(r) = self.remotes.get_mut(peer) {
                    (r.failure, r.ladder, r.redial) = (None, None, false);
                }
                self.retag(now, ep, peer, path);
                if !self.follow_policy(now, ep, *peer, path) {
                    return; // on a path that is off: no relay for others
                }
                // A new link may be the relay a waiting dial needed: offer again, with it listed.
                let waiting: Vec<PeerId> = self
                    .remotes
                    .iter()
                    .filter(|(p, r)| {
                        *p != peer
                            && r.heard
                            && !ep.is_connected(p)
                            && r.offered_at.is_none_or(|t| now - t >= OFFER_GAP)
                    })
                    .map(|(p, _)| *p)
                    .collect();
                for p in waiting {
                    let room = self.room_role(&p).is_some();
                    self.offer(now, ep, p, true, room);
                }
                // A peer that named this link as a relay is still waiting: dial it through
                // the link (a relay-only node dials only through its relay, below).
                if !matches!(self.privacy, Privacy::RelayOnly { .. }) {
                    let waiting: Vec<PeerId> = self
                        .remotes
                        .iter()
                        .filter(|(p, r)| {
                            r.heard && r.relays.contains(peer) && !ep.is_connected(p) && self.via_due(r, peer)
                        })
                        .map(|(p, _)| *p)
                        .collect();
                    for p in waiting {
                        let _ = ep.connect_with(now, p, &[Path::Via(*peer)]);
                    }
                }
                // A relay-only node's relay just linked: dial the peers waiting for it.
                if self.relay() == Some(*peer) {
                    let waiting: Vec<PeerId> =
                        self.remotes.iter().filter(|(p, r)| r.heard && !ep.is_connected(p)).map(|(p, _)| *p).collect();
                    for p in waiting {
                        let _ = ep.connect_with(now, p, &[Path::Via(*peer)]);
                    }
                }
            }
            // A dial that moved a link off a path that is off failed: the link is still there, so it goes.
            Event::ConnectFailed { peer } if self.remotes.get(peer).is_some_and(|r| r.redial) => {
                if let Some(r) = self.remotes.get_mut(peer) {
                    r.redial = false;
                }
                if ep.is_connected(peer) && ep.path(peer).is_some_and(|path| !self.allows(peer, path)) {
                    self.close(ep, *peer);
                }
            }
            Event::ConnectFailed { peer } => {
                if let Some(r) = self.remotes.get_mut(peer) {
                    r.ladder = None;
                }
                if self.picked == Some(*peer) {
                    self.picked = None; // another Host may be tried
                }
                let mine = self.nat;
                let r = self.remotes.entry(*peer).or_default();
                r.seen_at = now;
                let sym = |a: NatType, b: NatType| a.symmetric() && b != NatType::Open;
                r.failure = Some(match (sym(mine, r.nat), sym(r.nat, mine)) {
                    (true, true) => Failure::Symmetric(Side::Both),
                    (true, false) => Failure::Symmetric(Side::Ours),
                    (false, true) => Failure::Symmetric(Side::Theirs),
                    (false, false) => Failure::Unreachable,
                });
            }
            Event::Disconnected { peer, .. } => {
                if self.picked == Some(*peer) {
                    self.picked = None;
                }
                if let Some(r) = self.remotes.get_mut(peer) {
                    r.moving = None;
                }
            }
            Event::PathChanged { peer, path } => {
                self.retag(now, ep, peer, path);
                self.follow_policy(now, ep, *peer, path);
            }
            Event::AddrObserved { addr, .. } => {
                let addr = &canonical(addr.clone());
                // A peer that sees us on loopback is on this host, and where our TURN server relays from isn't our
                // NAT's mapping (NB-099).
                let real = dialable(addr, false);
                let ours = self.turn.relayed().contains(addr);
                if real && !ours && !self.observed.contains(addr) && !self.local.contains(addr) {
                    if self.observed.len() >= MAX_OBSERVED {
                        self.observed.remove(0);
                    }
                    self.observed.push(addr.clone());
                }
            }
            _ => {}
        }
    }

    /// A link to `peer` came up or moved to `path`: if that is a relayed address the peer offered, which its packets
    /// reached the link at before the offer did, the endpoint takes it as one ([`Endpoint::retag`]).
    fn retag(&mut self, now: Instant, ep: &mut Endpoint<A>, peer: &PeerId, path: &Path<A>) {
        let Path::Direct(at) = path else { return };
        if self.relayed_of.get(at) == Some(peer) {
            if let Some(hop) = A::relayed_hop(at) {
                ep.retag(now, peer, at, hop);
            }
        }
    }

    /// Fires due timers: STUN queries and retransmits, room hellos, planned punches, the steps of dials under a
    /// [`Policy`] and links that haven't moved off a path it turned off.
    pub fn handle_timeout(&mut self, now: Instant, ep: &mut Endpoint<A>) {
        self.sync_keep(ep);
        self.sync_turn(now, ep);
        self.turn.handle_timeout(now);
        self.turn_events(now, ep);
        if !self.servers.is_empty() && now >= self.next_round {
            self.next_round = now + STUN_INTERVAL;
            for i in 0..self.servers.len() {
                let mut tx = [0u8; 12];
                self.rng.fill_bytes(&mut tx);
                let s = &mut self.servers[i];
                (s.tx, s.sent_at, s.tries, s.pending) = (tx, now, 1, true);
                self.transmits.push_back(Transmit { dst: s.addr.clone(), payload: stun::binding_request(&tx) });
            }
        }
        for s in self.servers.iter_mut().filter(|s| s.pending) {
            if now >= s.sent_at + STUN_RTO * (1 << (s.tries - 1)) {
                if s.tries >= STUN_TRIES {
                    s.pending = false;
                } else {
                    (s.tries, s.sent_at) = (s.tries + 1, now);
                    self.transmits.push_back(Transmit { dst: s.addr.clone(), payload: stun::binding_request(&s.tx) });
                }
            }
        }
        if let Some(r) = self.room.as_mut().filter(|r| now >= r.next_hello) {
            r.interval = (r.interval * 2).min(HELLO_MAX);
            r.next_hello = now + r.interval;
            self.announce(now);
        }
        self.start_plans(now, ep);
        let due = |r: &Remote<A>| r.ladder.as_ref().is_some_and(|l| l.at <= now);
        let due: Vec<PeerId> = self.remotes.iter().filter(|(_, r)| due(r)).map(|(p, _)| *p).collect();
        for peer in due {
            self.climb(now, ep, peer);
        }
        let late: Vec<PeerId> =
            self.remotes.iter().filter(|(_, r)| r.moving.is_some_and(|t| t <= now)).map(|(p, _)| *p).collect();
        for peer in late {
            if let Some(r) = self.remotes.get_mut(&peer) {
                r.moving = None;
            }
            if ep.path(&peer).is_some_and(|path| !self.allows(&peer, path)) {
                self.close(ep, peer);
            }
        }
        self.remotes.retain(|p, r| now - r.seen_at < FORGET || ep.is_connected(p));
        let remotes = &self.remotes;
        self.relayed_of.retain(|_, p| remotes.contains_key(p));
        self.nonces.retain(|_, t| now - *t < Duration::from_millis(2 * FRESH_MS));
        self.punch_ips.retain(|_, times| {
            while times.front().is_some_and(|t| now - *t >= BIRTHDAY_IP_WINDOW) {
                times.pop_front();
            }
            !times.is_empty()
        });
    }

    /// When [`handle_timeout`](Traversal::handle_timeout) is next due.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let stun = (!self.servers.is_empty()).then_some(self.next_round);
        let retry = self.servers.iter().filter(|s| s.pending).map(|s| s.sent_at + STUN_RTO * (1 << (s.tries - 1)));
        let hello = self.room.as_ref().map(|r| r.next_hello);
        let plans = self.remotes.values().filter_map(|r| r.plan.as_ref().map(|b| b.0));
        let steps = self.remotes.values().filter_map(|r| Some(r.ladder.as_ref()?.at));
        let moves = self.remotes.values().filter_map(|r| r.moving);
        let keep = self.keep_stale.then_some(Instant::ZERO); // the relay peers changed: tell the endpoint at once
        let turn = self.turn.poll_timeout().into_iter().chain(self.turn_stale.then_some(Instant::ZERO)).min();
        stun.into_iter().chain(retry).chain(hello).chain(plans).chain(steps).chain(moves).chain(keep).chain(turn).min()
    }

    /// Extra sockets to open for birthday punching. A peer behind a symmetric NAT
    /// with random ports, which a cone peer can't punch to, gets this when an
    /// ordinary punch hasn't linked within a second. The host then:
    /// - opens `sockets` UDP sockets on the main socket's address family, and sends
    ///   `payload` from each, once, to `dst`, so each gets a NAT mapping towards it;
    /// - hands the endpoint what each one receives, as from the main socket;
    /// - sends the endpoint's datagrams to an address one of them heard from out of
    ///   the first one that heard it (its route), and everything else from the main
    ///   socket;
    /// - closes, at `until`, the ones that are no address's route, and a route
    ///   once it has heard nothing for a minute (the link is dead by then).
    ///
    /// Meanwhile the peer sprays ports of this node's IP ([`Endpoint::spray`]); one
    /// that hits a socket's mapping links the two through it. [`Node`](crate::Node)
    /// does all this.
    pub fn poll_birthday(&mut self) -> Option<Birthday<A>> {
        self.sockets.pop_front()
    }

    /// Starts the planned punches that are due, unless the peer linked directly meanwhile.
    fn start_plans(&mut self, now: Instant, ep: &mut Endpoint<A>) {
        let relay_only = matches!(self.privacy, Privacy::RelayOnly { .. });
        let due = |r: &Remote<A>| r.plan.as_ref().is_some_and(|b| b.0 <= now);
        let due: Vec<PeerId> = self.remotes.iter().filter(|(_, r)| due(r)).map(|(p, _)| *p).collect();
        for peer in due {
            let Some((_, side)) = self.remotes.get_mut(&peer).and_then(|r| r.plan.take()) else { continue };
            let direct = matches!(ep.path(&peer), Some(Path::Direct(a)) if !a.relayed());
            // A link kept on its relay (NB-097) takes no punch: none is started, nor counted.
            if direct || relay_only || ep.path(&peer).is_some_and(|p| self.kept(p)) {
                continue;
            }
            if let Plan::Then(addr) = side {
                ep.spray(now, &peer, &addr, BIRTHDAY_PROBES);
                continue;
            }
            // Each probe from behind a NAT that maps per IP and port costs it a port.
            let per_port = matches!(side, Plan::Spray(_)) && self.nat == NatType::Symmetric;
            if per_port && self.per_port_punch.is_some_and(|t| now - t < PER_PORT_PUNCH_GAP) {
                continue;
            }
            // NB44-1: one punch per destination IP at a time, and at most BIRTHDAY_IP_MAX
            // per window, across all peers, so offers from many PeerIds naming one victim
            // can't make the node spray it more than that. Checked before the node token.
            let ip = plan_ip(&side);
            if let Some(ip) = ip {
                let times = self.punch_ips.entry(ip).or_default();
                while times.front().is_some_and(|t| now - *t >= BIRTHDAY_IP_WINDOW) {
                    times.pop_front();
                }
                let busy = times.back().is_some_and(|t| now - *t < BIRTHDAY_IP_BUSY);
                if busy || times.len() >= BIRTHDAY_IP_MAX {
                    continue;
                }
            }
            if !self.planned.take(now, BIRTHDAY_BURST, BIRTHDAY_REFILL) {
                continue;
            }
            if let Some(ip) = ip {
                self.punch_ips.entry(ip).or_default().push_back(now);
            }
            if per_port {
                self.per_port_punch = Some(now);
            }
            let Some(r) = self.remotes.get_mut(&peer) else { continue };
            r.plan_at = Some(now);
            let both = matches!(side, Plan::Both(_));
            match side {
                Plan::Open(dst) | Plan::Both(dst) => {
                    if both {
                        r.plan = Some((now + (BIRTHDAY_SPRAY_AFTER - BIRTHDAY_OPEN_AFTER), Plan::Then(dst.clone())));
                    }
                    if let Some(payload) = ep.opener(now, &peer) {
                        let until = now + BIRTHDAY_OPEN_FOR;
                        self.sockets.push_back(Birthday { peer, dst, sockets: BIRTHDAY_SOCKETS, payload, until });
                    }
                }
                Plan::Spray(addr) => {
                    ep.spray(now, &peer, &addr, if per_port { PER_PORT_PROBES } else { BIRTHDAY_PROBES })
                }
                Plan::Sweep(addrs) => ep.sweep(now, &peer, &addrs),
                Plan::Then(_) => {}
            }
        }
    }

    /// Acts on offer `o` from `from` under the policy. A linked peer whose link may move to a direct path is given
    /// its addresses and guesses (with `Direct` on) and a planned punch (with `Birthday` on). Any other is dialled
    /// step by step ([`climb`](Traversal::climb)); a newer offer from a peer being dialled replaces the one dialled,
    /// and the steps already started dial its paths too.
    fn dial_by_policy(
        &mut self,
        now: Instant,
        ep: &mut Endpoint<A>,
        from: PeerId,
        o: &Offer<A>,
        addrs: &[A],
        guesses: &[A],
    ) {
        let Some(p) = self.policy.clone() else { return };
        if let Some(path) = ep.path(&from) {
            if self.kept(path) {
                return;
            }
            if p.allows(PathKind::Direct) {
                ep.add_candidates(now, &from, addrs);
                if !guesses.is_empty() {
                    ep.set_guesses(now, &from, guesses);
                }
            }
            if p.allows(PathKind::Birthday) {
                self.plan_punch(now, ep, from, o);
            }
            return;
        }
        let r = self.remotes.entry(from).or_default();
        let started = match r.ladder.as_mut() {
            Some(l) => {
                l.offer = o.clone();
                l.next
            }
            None => {
                r.ladder = Some(Ladder { offer: o.clone(), next: 0, at: now, invite: false });
                0
            }
        };
        for step in p.steps().into_iter().take(started) {
            self.dial_step(now, ep, from, o, step, false);
        }
        self.climb(now, ep, from);
    }

    /// Dials `peer` from its invite under the policy, step by step as an offer is ([`Policy`]): directly at
    /// `addrs` (the invite's, as the host checked them) and through `relays` (those it lists). Fails with
    /// [`ConnectError::NoPath`] if no step of the policy has anything to dial, and as
    /// [`Endpoint::connect_with`] does for `peer` itself. Does nothing once linked.
    pub fn dial_invite(
        &mut self,
        now: Instant,
        ep: &mut Endpoint<A>,
        peer: PeerId,
        addrs: Vec<A>,
        relays: Vec<PeerId>,
    ) -> Result<(), ConnectError> {
        // With no targets the endpoint only checks `peer` (this node, or a key that isn't valid), then has no path.
        match ep.connect_with(now, peer, &[]) {
            Err(ConnectError::NoPath) | Ok(()) => {}
            Err(e) => return Err(e),
        }
        if ep.is_connected(&peer) {
            return Ok(());
        }
        let offer = Offer {
            kind: Proof::None,
            reply: false,
            role: Role::Peer,
            nat: NatType::Unknown,
            says_mapping: false,
            ts: 0,
            nonce: 0,
            addrs,
            predict: None,
            relays,
            relayed: Vec::new(),
        };
        let r = self.remotes.entry(peer).or_default();
        r.seen_at = now;
        r.ladder = Some(Ladder { offer, next: 0, at: now, invite: true });
        match self.climb(now, ep, peer) {
            true => Ok(()),
            false => {
                if let Some(r) = self.remotes.get_mut(&peer) {
                    r.ladder = None;
                }
                Err(ConnectError::NoPath)
            }
        }
    }

    /// Whether a dial that moves `peer`'s link off a path the policy turned off ([`Endpoint::redial`]) is under
    /// way: its `ConnectFailed` is this node's own, not the app's.
    pub fn redialing(&self, peer: &PeerId) -> bool {
        self.remotes.get(peer).is_some_and(|r| r.redial)
    }

    /// Starts the steps of the dial to `peer` that are due ([`dial_by_policy`](Traversal::dial_by_policy)): the
    /// next one once the last has had the policy's `fallback`, at once after one with nothing to try. Done once the
    /// peer is linked or every step has started. Whether a step dialled anything.
    fn climb(&mut self, now: Instant, ep: &mut Endpoint<A>, peer: PeerId) -> bool {
        let Some(p) = &self.policy else { return false };
        let (steps, fallback) = (p.steps(), p.fallback);
        let mut dialled = false;
        loop {
            let linked = ep.is_connected(&peer);
            let Some(r) = self.remotes.get_mut(&peer) else { return dialled };
            let Some(l) = r.ladder.as_mut() else { return dialled };
            if linked || l.next >= steps.len() {
                r.ladder = None;
                return dialled;
            }
            if l.at > now {
                return dialled;
            }
            let (step, offer, invite) = (steps[l.next], l.offer.clone(), l.invite);
            l.next += 1;
            if self.dial_step(now, ep, peer, &offer, step, invite) {
                dialled = true;
                if let Some(l) = self.remotes.get_mut(&peer).and_then(|r| r.ladder.as_mut()) {
                    l.at = now + fallback;
                }
            }
        }
    }

    /// Dials `peer`'s paths of the kinds in `step` (bits of [`PathKind::bit`]) from its offer `o`: relays it names
    /// that this node is linked to (relay peers first, two at most), its addresses and the ports around its
    /// prediction, and a planned punch, which dials the address it aims at. `invite`: the addresses are an
    /// invite's, dialled as they are. Whether there was anything to dial.
    fn dial_step(
        &mut self,
        now: Instant,
        ep: &mut Endpoint<A>,
        peer: PeerId,
        o: &Offer<A>,
        step: u8,
        invite: bool,
    ) -> bool {
        let usable =
            |p: &&PeerId| **p != peer && **p != self.id && ep.is_connected(p) && step & self.relay_kind(p).bit() != 0;
        let mut vias: Vec<PeerId> = o.relays.iter().filter(usable).copied().collect();
        vias.sort_by_key(|p| !self.relay_peers.contains(p)); // ours first, else the sender's order
        let mut targets: Vec<Path<A>> = vias.into_iter().take(VIA_PER_DIAL).map(Path::Via).collect();
        let direct = step & PathKind::Direct.bit() != 0;
        if direct {
            let addrs = if invite { o.addrs.clone() } else { self.dial_addrs(o) };
            targets.extend(addrs.into_iter().map(Path::Direct));
        }
        if step & PathKind::Birthday.bit() != 0 {
            if let Some(base) = self.plan_punch(now, ep, peer, o).map(Path::Direct) {
                if !targets.contains(&base) {
                    targets.push(base);
                }
            }
        }
        if step & PathKind::Turn.bit() != 0 {
            let addrs = if invite { o.addrs.clone() } else { self.dial_addrs(o) };
            for t in self.turn_targets(now, &addrs, &o.relayed) {
                if !targets.contains(&t) {
                    targets.push(t);
                }
            }
        }
        if targets.is_empty() {
            return false;
        }
        let _ = ep.connect_with(now, peer, &targets);
        let guesses = if direct { self.guesses(o) } else { Vec::new() };
        if !guesses.is_empty() {
            ep.set_guesses(now, &peer, &guesses);
        }
        true
    }

    /// The addresses in offer `o` this node dials: worth dialling, in a family its socket reaches, not its own.
    fn dial_addrs(&self, o: &Offer<A>) -> Vec<A> {
        let usable = |a: &&A| dialable(*a, self.loopback) && self.reaches(a) && !self.local.contains(a);
        o.addrs.iter().filter(usable).cloned().collect()
    }

    /// The ports to probe around the prediction in offer `o`. A sequential symmetric NAT maps the sender's packets
    /// to us at a port near it: probe those too (pointless from an open or symmetric NAT, which take any packet or
    /// would burn a port per probe). Only around a public address: senders predict only for theirs.
    fn guesses(&self, o: &Offer<A>) -> Vec<A> {
        let loopback = self.loopback;
        match (o.predict, o.addrs.first()) {
            (Some(p), Some(base))
                if self.nat != NatType::Open
                    && !self.nat.symmetric()
                    && is_public(base)
                    && dialable(base, loopback) =>
            {
                p.window(base).into_iter().filter(|a| dialable(a, loopback) && !o.addrs.contains(a)).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Whether a dial to a peer (`r`) may go through `relay`, newly linked: with no policy, yes; with one, if the
    /// relay's kind is on and its step of the dial has started (or every step has).
    fn via_due(&self, r: &Remote<A>, relay: &PeerId) -> bool {
        let Some(p) = &self.policy else { return true };
        let bit = self.relay_kind(relay).bit();
        let Some(step) = p.steps().iter().position(|s| s & bit != 0) else { return false };
        r.ladder.as_ref().is_none_or(|l| step < l.next)
    }

    /// Moves the link to `peer` off a path the policy turned off (see [`set_policy`](Traversal::set_policy)), or
    /// closes it if there is nowhere to go.
    fn move_off(&mut self, now: Instant, ep: &mut Endpoint<A>, peer: PeerId) {
        let (Some(p), Some(path)) = (self.policy.clone(), ep.path(&peer).cloned()) else { return };
        let r = self.remotes.entry(peer).or_default();
        if r.moving.is_some() {
            return;
        }
        let (addrs, named, relayed) = (r.addrs.clone(), r.relays.clone(), r.relayed.clone());
        let mut tried = false;
        let off_relay = matches!(&path, Path::Via(_)) || matches!(&path, Path::Direct(a) if a.relayed());
        if off_relay && (p.allows(PathKind::Direct) || p.allows(PathKind::Birthday)) {
            if p.allows(PathKind::Direct) {
                ep.add_candidates(now, &peer, &addrs);
            }
            // Asks for an offer back: the peer punches to ours, and its offer may plan a punch here.
            let room = self.room_role(&peer).is_some();
            self.offer(now, ep, peer, true, room);
            tried = true;
        }
        let current = match path {
            Path::Via(r) => Some(r),
            Path::Direct(_) => None,
        };
        let mut vias: Vec<PeerId> = Vec::new();
        for v in named.iter().chain(&self.relay_peers) {
            let usable = Some(*v) != current && *v != peer && ep.is_connected(v) && p.allows(self.relay_kind(v));
            if usable && !vias.contains(v) {
                vias.push(*v);
            }
        }
        vias.sort_by_key(|v| p.rank(self.relay_kind(v))); // the preferred kind first
        let mut targets: Vec<Path<A>> = vias.into_iter().take(VIA_PER_DIAL).map(Path::Via).collect();
        // Onto TURN, if it is on and the link isn't there (or is, on a server that is gone).
        let up = self.turn.up();
        let on_live = |a: &A| a.turn_parts().is_none_or(|(s, _)| up.iter().any(|u| u.0 == s));
        let on_turn = matches!(&path, Path::Direct(a) if a.relayed() && on_live(a));
        if p.allows(PathKind::Turn) && !on_turn {
            targets.extend(self.turn_targets(now, &addrs, &relayed));
        }
        let redialled = !targets.is_empty() && ep.redial(now, peer, &targets).is_ok();
        match (tried || redialled, self.remotes.get_mut(&peer)) {
            (true, Some(r)) => (r.moving, r.redial) = (Some(now + MOVE_WAIT), redialled),
            _ => self.close(ep, peer),
        }
    }

    /// Closes the link to `peer`, on a path the policy turned off, for [`poll_dropped`](Traversal::poll_dropped).
    fn close(&mut self, ep: &mut Endpoint<A>, peer: PeerId) {
        ep.disconnect(&peer);
        if let Some(r) = self.remotes.get_mut(&peer) {
            r.moving = None;
        }
        self.dropped.push_back(peer);
    }

    /// Plans a punch with `from` after its offer, if its NAT and ours are a pair an
    /// ordinary punch can't link: a birthday punch for ours symmetric with random
    /// ports and its a cone, or the other way round; a lockstep sweep for two sequential
    /// symmetric NATs (both predict), whatever each maps per; else, for a
    /// [`NatType::SymmetricIp`] side and a symmetric one whose offer says how it maps (so
    /// it does its part), the same birthday punch with the `SymmetricIp` side's sockets,
    /// or both roles on each side for two `SymmetricIp` ones. Aimed at the offer's first
    /// address, which is public when the sender's STUN answered (and which its prediction
    /// is for): that address, if a punch is planned.
    fn plan_punch(&mut self, now: Instant, ep: &Endpoint<A>, from: PeerId, o: &Offer<A>) -> Option<A> {
        let (loopback, relay_only) = (self.loopback, matches!(self.privacy, Privacy::RelayOnly { .. }));
        let v4 = |a: &A| a.to_socket_addr().is_some_and(|s| s.is_ipv4());
        let base = o.addrs.first().filter(|a| is_public(*a) && v4(a) && dialable(*a, loopback))?;
        // To a peer whose offer doesn't say how its NAT maps (one that predates NB-083,
        // or isn't symmetric) this node is Symmetric, as that peer sees it.
        let mine = match self.nat {
            NatType::SymmetricIp if !o.says_mapping => NatType::Symmetric,
            nat => nat,
        };
        let side = match (mine, o.nat) {
            (mine, NatType::Cone) if mine.symmetric() && self.prediction.is_none() => Plan::Open(base.clone()),
            (NatType::Cone, theirs) if theirs.symmetric() && o.predict.is_none() => Plan::Spray(base.clone()),
            // Both predict: the lockstep sweep, whatever either maps per, as before NB-083.
            // Each end sees the same two predictions, so both pick it, or both don't.
            (mine, theirs) if mine.symmetric() && theirs.symmetric() => match (self.prediction, o.predict) {
                (Some(_), Some(p)) => {
                    let (stride, count) = if self.id < from { (1, LOCKSTEP_LOW) } else { (2, LOCKSTEP_HIGH) };
                    Plan::Sweep(p.sweep(base, stride, count))
                }
                // Else the `SymmetricIp` side opens sockets, and a side that maps per IP and
                // port only sprays them: its own sockets would let a probe in only if its NAT
                // filtered by IP, and each costs it a port (NB83-1). Two `SymmetricIp` sides
                // each do both, so either one's sockets can take the other's probes.
                _ => match (mine, theirs) {
                    (NatType::SymmetricIp, NatType::SymmetricIp) => Plan::Both(base.clone()),
                    (NatType::SymmetricIp, _) => Plan::Open(base.clone()),
                    (_, NatType::SymmetricIp) => Plan::Spray(base.clone()),
                    _ => return None,
                },
            },
            _ => return None,
        };
        if relay_only || matches!(ep.path(&from), Some(Path::Direct(a)) if !a.relayed()) {
            return None;
        }
        let r = self.remotes.get_mut(&from)?;
        if r.plan.is_some() {
            return Some(base.clone());
        }
        if r.plan_at.is_some_and(|t| now - t < BIRTHDAY_COOLDOWN) {
            return None;
        }
        let wait = match side {
            Plan::Open(_) | Plan::Both(_) => BIRTHDAY_OPEN_AFTER,
            Plan::Spray(_) | Plan::Then(_) => BIRTHDAY_SPRAY_AFTER,
            Plan::Sweep(_) => LOCKSTEP_AFTER,
        };
        r.plan = Some((now + wait, side));
        Some(base.clone())
    }

    /// The next STUN or TURN datagram to send from the endpoint's socket.
    pub fn poll_transmit(&mut self) -> Option<Transmit<A>> {
        self.transmits.pop_front().or_else(|| self.turn.poll_transmit())
    }

    /// Replaces this node's TURN servers (empty: none). An allocation stays on a server kept with the same
    /// credentials; the others are released, and links through them move to a path the policy allows, or close
    /// ([`poll_dropped`](Traversal::poll_dropped)), as when the policy turns a path off. TURN runs only with
    /// [`PathKind::Turn`] in the policy, and not under [`Privacy::RelayOnly`].
    #[cfg(feature = "turn")]
    pub fn set_turn_servers(&mut self, now: Instant, ep: &mut Endpoint<A>, servers: Vec<(A, TurnServer)>) {
        self.turn.set_servers(now, servers);
        self.sync_turn(now, ep);
    }

    /// Each TURN server, and its relayed address while this node has an allocation there.
    #[cfg(feature = "turn")]
    pub fn turn_status(&self) -> Vec<(A, Option<SocketAddr>)> {
        self.turn.status()
    }

    /// Starts or stops TURN as the servers, the policy and the privacy say, and acts on what the allocations did.
    fn sync_turn(&mut self, now: Instant, ep: &mut Endpoint<A>) {
        self.turn_stale = false;
        let listed = self.policy.as_ref().is_some_and(|p| p.allows(PathKind::Turn));
        let on = self.turn.has_servers() && listed && !matches!(self.privacy, Privacy::RelayOnly { .. });
        self.turn.set_on(now, on);
        self.turn_events(now, ep);
    }

    /// An allocation made: offered, with its relayed address, to the peers still to link with (their dials take
    /// it). One gone: its links move or close, as for a path turned off.
    fn turn_events(&mut self, now: Instant, ep: &mut Endpoint<A>) {
        while let Some((server, e)) = self.turn.poll_event() {
            match e {
                // Also to those offered to before it was made, which haven't answered yet.
                TurnEvent::Allocated(_) => {
                    let met = |r: &Remote<A>| r.heard || r.offered_at.is_some();
                    let waiting: Vec<PeerId> =
                        self.remotes.iter().filter(|(p, r)| met(r) && !ep.is_connected(p)).map(|(p, _)| *p).collect();
                    for p in waiting {
                        let room = self.room_role(&p).is_some();
                        self.offer(now, ep, p, true, room);
                    }
                }
                TurnEvent::Lost => {
                    let through = |a: &A| a.turn_parts().is_some_and(|(s, _)| s == server);
                    let on = |path: &Path<A>| matches!(path, Path::Direct(a) if through(a));
                    let gone: Vec<PeerId> =
                        ep.connected_peers().filter(|p| ep.path(p).is_some_and(on)).copied().collect();
                    for peer in gone {
                        self.move_off(now, ep, peer);
                    }
                }
                TurnEvent::Refused(_) => {}
            }
        }
    }

    /// A datagram from `from`, if from one of this node's TURN servers: `None` if not, or if it is a STUN message the
    /// TURN client didn't ask for (a Binding answer, from a server that is a STUN server too); else what it carried
    /// for the endpoint (a peer's packet, as from its [TURN hop](Address::turn_hop)), if anything. Call it before
    /// [`handle_stun`](Traversal::handle_stun), and give that what it leaves: TURN's answers are STUN messages too.
    pub fn handle_turn(&mut self, now: Instant, from: &A, data: &[u8]) -> Option<Option<(A, Vec<u8>)>> {
        self.turn.handle(now, from, data)
    }

    /// Sends a datagram the endpoint addressed to a [TURN hop](Address::turn_hop), through that server's allocation.
    /// `false` with none (it is lost).
    pub fn send_turn(&mut self, now: Instant, dst: &A, payload: &[u8]) -> bool {
        self.turn.send(now, dst, payload)
    }

    /// The address the endpoint takes `data`, which the socket got from `from`, to be from: a [relayed
    /// hop](Address::relayed_hop) if `from` is a relayed address a peer offered and `data` names that peer's link or
    /// dial ([`Endpoint::peer_of`]); else `from`. Another peer's packets from there stay as they are, whoever claims
    /// the address (NB99-1). A handshake's first message names no link: one it makes there is retagged when it comes
    /// up.
    pub fn hop(&self, ep: &Endpoint<A>, from: A, data: &[u8]) -> A {
        match self.relayed_of.get(&from) {
            Some(p) if ep.peer_of(data) == Some(*p) => A::relayed_hop(&from).unwrap_or(from),
            _ => from,
        }
    }

    /// The paths through TURN to a peer at `addrs` with relayed addresses `relayed`: those, from this node's socket,
    /// and its addresses of a family each of this node's allocations relays to, through it (asking for the
    /// permissions). At most `MAX_TURN_TARGETS`.
    fn turn_targets(&mut self, now: Instant, addrs: &[A], relayed: &[A]) -> Vec<Path<A>> {
        let mut out: Vec<Path<A>> =
            relayed.iter().filter(|a| self.reaches(a)).filter_map(A::relayed_hop).map(Path::Direct).collect();
        let peers: Vec<SocketAddr> = addrs.iter().chain(relayed).filter_map(Address::to_socket_addr).collect();
        let ips: Vec<IpAddr> = peers.iter().map(|s| s.ip()).collect();
        self.turn.permit(now, &ips);
        for (server, at) in self.turn.up() {
            for p in peers.iter().filter(|p| p.is_ipv4() == at.is_ipv4()) {
                out.extend(A::turn_hop(&server, *p).map(Path::Direct));
            }
        }
        out.truncate(MAX_TURN_TARGETS);
        out
    }

    /// The next action for the signaling carrier.
    pub fn poll_signal(&mut self) -> Option<Signal> {
        self.signals.pop_front()
    }

    fn wall_ms(&self, now: Instant) -> u64 {
        self.unix_ms + now.as_micros() / 1000
    }

    fn role(&self) -> Option<Role> {
        self.room.as_ref().map(|r| r.role)
    }

    /// The NAT type from the servers that answered. Two servers at one IP, on two ports,
    /// tell a NAT that maps per destination IP from one that maps per IP and port
    /// (NB-083), when the list spans other IPs too: only a mapping that differs between
    /// IPs and not between one IP's ports is [`NatType::SymmetricIp`], and only one the
    /// same at two IPs is `Cone`. A list at one IP only is judged as before: one mapping
    /// for all is `Cone`, else `Symmetric`.
    fn classify(&mut self) {
        let ip = |a: &A| a.to_socket_addr().map(|s| s.ip().to_canonical());
        let same_ip = |a: &A, b: &A| ip(a).is_some_and(|x| ip(b) == Some(x));
        let answers: Vec<(&A, &A)> = self.servers.iter().filter_map(|s| Some((&s.addr, s.mapped.as_ref()?))).collect();
        let spans = self.servers.iter().any(|s| !same_ip(&s.addr, &self.servers[0].addr));
        // Pairs of answers that agree or not, from two ports of one IP and from two IPs.
        let (mut port_same, mut port_differs, mut ip_same, mut ip_differs) = (false, false, false, false);
        for (i, (s, m)) in answers.iter().enumerate() {
            for (t, n) in &answers[i + 1..] {
                match (same_ip(s, t), m == n) {
                    (true, true) => port_same = true,
                    (true, false) => port_differs = true,
                    (false, true) => ip_same = true,
                    (false, false) => ip_differs = true,
                }
            }
        }
        self.public = answers.first().map(|a| a.1.clone());
        self.nat = match answers.first() {
            None => NatType::Unknown,
            Some(_) if answers.iter().any(|(_, m)| self.local.contains(m)) => NatType::Open,
            Some(_) if answers.len() < 2 => NatType::Unknown,
            Some(first) if !spans => match answers.iter().all(|(_, m)| *m == first.1) {
                true => NatType::Cone,
                false => NatType::Symmetric,
            },
            Some(_) if port_differs => NatType::Symmetric,
            Some(_) if ip_differs && port_same => NatType::SymmetricIp,
            Some(_) if ip_differs => NatType::Symmetric,
            Some(_) if ip_same => NatType::Cone,
            // Only one IP's ports answered: they agree whatever the NAT does per IP.
            Some(_) => NatType::Unknown,
        };
        // Each mapping once, in the order the servers were asked: the ports the NAT handed out.
        let mut handed: Vec<&A> = Vec::new();
        for (_, m) in &answers {
            if !handed.contains(m) {
                handed.push(*m);
            }
        }
        self.prediction = if self.nat.symmetric() { Prediction::from_mappings(&handed) } else { None };
    }

    /// Whether this node, in its room (if any), links with `peer` announcing `role`.
    fn links_with(&self, peer: &PeerId, role: Role) -> bool {
        let expect = self.room.as_ref().and_then(|r| r.expect);
        *peer != self.id && allowed(self.role(), role) && expect.is_none_or(|h| h == *peer)
    }

    /// A hello whose room proof checked out.
    fn on_hello(&mut self, now: Instant, ep: &Endpoint<A>, from: PeerId, role: Role) {
        if !self.links_with(&from, role) {
            return;
        }
        if let Some(r) = self.remotes.get_mut(&from) {
            r.member = Some(role);
        }
        if ep.is_connected(&from) {
            return;
        }
        let recent = self.remotes.get(&from).and_then(|r| r.offered_at).is_some_and(|t| now - t < OFFER_GAP);
        if !recent && self.hellos.take(now, BURST, REFILL) {
            self.offer(now, ep, from, true, true);
            self.remotes.entry(from).or_default().member = Some(role);
        }
    }

    /// An offer that opened, and whose room proof (in a room) checked out.
    fn on_offer(&mut self, now: Instant, ep: &mut Endpoint<A>, from: PeerId, mut o: Offer<A>) {
        o.addrs = o.addrs.into_iter().map(canonical).collect(); // `::ffff:` copies compare as the IPv4 address
        o.relayed = o.relayed.into_iter().map(canonical).filter(|a| dialable(a, self.loopback)).collect();
        let room = o.kind == Proof::Room;
        let role_ok = if room { self.links_with(&from, o.role) } else { from != self.id };
        if !role_ok || self.wall_ms(now).abs_diff(o.ts) > FRESH_MS || self.nonces.contains_key(&(from, o.nonce)) {
            return; // not for us, stale, or a replay
        }
        // The sender's own limit, then the node's, which peers we dialled don't
        // draw on (answers to our own dials can't be crowded out); the nonce is
        // kept only for offers acted on.
        let (mut mine, invited) = self.remotes.get(&from).map_or((Bucket::default(), false), |r| (r.offers, r.invited));
        if !mine.ready(now, SENDER_BURST, SENDER_REFILL) || !(invited || self.offers.take(now, BURST, REFILL)) {
            return;
        }
        mine.take(now, SENDER_BURST, SENDER_REFILL);
        if self.nonces.len() >= MAX_NONCES {
            return;
        }
        self.nonces.insert((from, o.nonce), now);
        let r = self.remotes.entry(from).or_default();
        (r.heard, r.nat, r.seen_at, r.offers) = (true, o.nat, now, mine);
        r.relays = o.relays.iter().filter(|p| **p != from && **p != self.id).take(MAX_RELAYS).copied().collect();
        if room {
            r.member = Some(o.role);
        }
        let relay_only = matches!(self.privacy, Privacy::RelayOnly { .. });
        let by_policy = self.policy.is_some() && !relay_only;
        if !by_policy {
            self.plan_punch(now, ep, from, &o);
        }
        let guesses = self.guesses(&o);
        let addrs = self.dial_addrs(&o);
        if let Some(r) = self.remotes.get_mut(&from) {
            r.addrs.clone_from(&addrs);
            r.relayed.clone_from(&o.relayed);
        }
        // What comes from its relayed addresses is from them; and it may send through our allocations.
        self.relayed_of.retain(|_, p| *p != from);
        for a in o.relayed.iter().take(MAX_RELAYED) {
            if self.relayed_of.len() < MAX_RELAYED_OF {
                self.relayed_of.insert(a.clone(), from);
            }
            // Linked there before this offer said what it is (its dial through its allocation came first): a link
            // through TURN now, which the policy may have off (NB99-I1).
            if let (Some(Path::Direct(at)), Some(hop)) = (ep.path(&from), A::relayed_hop(a)) {
                if at == a {
                    let at = at.clone();
                    ep.retag(now, &from, &at, hop.clone());
                    self.follow_policy(now, ep, from, &Path::Direct(hop));
                }
            }
        }
        let ips: Vec<IpAddr> = addrs.iter().chain(&o.relayed).filter_map(|a| Some(a.to_socket_addr()?.ip())).collect();
        self.turn.permit(now, &ips);
        let mut relay = self.relay();
        let reachable = ep.is_connected(&from) || !addrs.is_empty();
        if relay.is_none()
            && room
            && reachable
            && o.role == Role::Host
            && matches!(self.privacy, Privacy::RelayOnly { .. })
        {
            // The first proven Host this relay-only node dials becomes its relay, and stays.
            // A Host offer with nothing to dial is not picked: no dial means no
            // ConnectFailed to clear the pick, and the real Host would be locked out.
            (self.picked, relay) = (Some(from), Some(from));
        }
        if matches!(self.privacy, Privacy::RelayOnly { .. }) && relay != Some(from) {
            // Relay-only: anyone but the relay is dialled through it, never directly
            // (or once it links, from `handle_event`).
            if let Some(r) = relay.filter(|r| ep.is_connected(r) && !ep.is_connected(&from)) {
                let _ = ep.connect_with(now, from, &[Path::Via(r)]);
            }
        } else if by_policy {
            self.dial_by_policy(now, ep, from, &o, &addrs, &guesses);
        } else {
            if ep.is_connected(&from) {
                ep.add_candidates(now, &from, &addrs);
            } else {
                let mut vias: Vec<PeerId> =
                    o.relays.iter().filter(|p| **p != from && **p != self.id && ep.is_connected(p)).copied().collect();
                vias.sort_by_key(|p| !self.relay_peers.contains(p)); // ours first, else the sender's order
                let mut targets: Vec<Path<A>> = vias.into_iter().take(VIA_PER_DIAL).map(Path::Via).collect();
                // Its relayed addresses too (with no policy this node has no TURN of its own).
                let turn = self.turn_targets(now, &addrs, &o.relayed);
                targets.extend(addrs.into_iter().map(Path::Direct));
                targets.extend(turn);
                if !targets.is_empty() {
                    let _ = ep.connect_with(now, from, &targets);
                }
            }
            if !guesses.is_empty() {
                ep.set_guesses(now, &from, &guesses);
            }
        }
        if o.reply {
            self.offer(now, ep, from, false, room);
        }
    }

    /// Seals an offer to `to` and publishes it on its topic. `room`: a room offer,
    /// with a proof under the room key (sent only while in a room); otherwise
    /// one proved with `to`'s invite key, if we have it.
    fn offer(&mut self, now: Instant, ep: &Endpoint<A>, to: PeerId, reply: bool, room: bool) {
        let invite = self.remotes.get(&to).and_then(|r| r.key);
        let (kind, key, label): (_, _, &[u8]) = match (&self.room, room, invite) {
            (Some(r), true, _) => (Proof::Room, Some(r.key), b"pw-offer"),
            (None, true, _) => return,
            (_, false, Some(k)) => (Proof::Invite, Some(k), b"pw-invite"),
            (_, false, None) => (Proof::None, None, b""),
        };
        // A relay-only node names only its relay, not every peer it links with. Others
        // name their links and LAN addresses only to a room member or a peer they
        // dialled; anyone else, such as a stranger holding our invite, gets public
        // addresses only, even after its dial failed (a holder can make that happen).
        // Relay fallback for it still works: it names its links, and we dial through
        // whichever of them we link with (`handle_event`). A relay peer gets our
        // addresses (a LAN relay is dialled on them) but not who else we link with.
        let full = room || self.remotes.get(&to).is_some_and(|r| r.invited);
        let named = !self.relay_peers.contains(&to);
        let (addrs, relays) = match self.privacy {
            Privacy::RelayOnly { .. } => {
                let relay = self.relay().filter(|r| *r != to && ep.is_connected(r));
                (self.candidates_for(&to), relay.into_iter().collect())
            }
            _ if full => {
                let ours = self.relay_peers.iter().filter(|p| ep.is_connected(p));
                let others = ep.connected_peers().filter(|p| !self.relay_peers.contains(p));
                let links = ours.chain(others).filter(|p| **p != to && named).take(MAX_RELAYS);
                (self.candidates_for(&to), links.copied().collect())
            }
            _ => (self.addresses(true), Vec::new()),
        };
        // The prediction is for the public address, so only when that leads.
        let predict = self.prediction.filter(|_| self.public.is_some() && addrs.first() == self.public.as_ref());
        let mut relayed = self.turn.relayed();
        relayed.truncate(MAX_RELAYED);
        let o = Offer {
            kind,
            reply,
            role: self.role().unwrap_or(Role::Peer),
            nat: self.nat,
            says_mapping: self.nat.symmetric(),
            ts: self.wall_ms(now),
            nonce: self.rng.next_u64(),
            addrs,
            predict,
            relays,
            relayed,
        };
        let mut body = o.encode();
        if let Some(k) = key {
            let proof = mac(&k, &[label, self.id.as_bytes(), to.as_bytes(), &body]);
            body.extend_from_slice(&proof);
        }
        let mut seed = [0u8; 32];
        self.rng.fill_bytes(&mut seed);
        if let Some(s) = sealed::seal(&self.me, &to, seed, &body) {
            self.signals.push_back(Signal::Publish(Topic::peer(&to), s));
        }
        let r = self.remotes.entry(to).or_default();
        (r.offered_at, r.seen_at) = (Some(now), now);
    }
}

/// Whether a node in a room as `mine` (or in none) links with one announcing `theirs`.
fn allowed(mine: Option<Role>, theirs: Role) -> bool {
    matches!(
        (mine, theirs),
        (None, _)
            | (Some(Role::Peer), Role::Peer | Role::Host)
            | (Some(Role::Host), Role::Peer | Role::Client)
            | (Some(Role::Client), Role::Host)
    )
}

/// `a` with an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) as the plain IPv4 one.
fn canonical<A: Address>(a: A) -> A {
    match a.to_socket_addr() {
        Some(s) if s.ip().to_canonical() != s.ip() => {
            A::from_socket_addr(std::net::SocketAddr::new(s.ip().to_canonical(), s.port())).unwrap_or(a)
        }
        _ => a,
    }
}

/// Whether an address from a peer or STUN is worth dialling: never port 0, an
/// unspecified, multicast, broadcast or link-local address, and loopback only for a
/// node bound to loopback (so an offer can't aim a node at services on its own host).
/// A NAT64 address (`64:ff9b::/96`) is judged by the IPv4 address it embeds.
fn dialable<A: Address>(a: &A, loopback: bool) -> bool {
    use std::net::IpAddr;
    let Some(s) = a.to_socket_addr() else { return true };
    let ip = match s.ip().to_canonical() {
        IpAddr::V6(v) if v.segments()[..6] == [0x64, 0xff9b, 0, 0, 0, 0] => {
            IpAddr::V4(std::net::Ipv4Addr::from_bits(v.to_bits() as u32))
        }
        ip => ip,
    };
    let special = match ip {
        IpAddr::V4(v) => v.is_broadcast() || v.is_link_local(),
        IpAddr::V6(v) => v.segments()[0] & 0xffc0 == 0xfe80,
    };
    s.port() != 0 && !ip.is_unspecified() && !ip.is_multicast() && !special && (loopback || !ip.is_loopback())
}

/// Whether an address is on the public internet: not loopback, private (RFC 1918,
/// IPv6 ULA), carrier-grade NAT (100.64/10), link-local or otherwise special.
/// Addresses that aren't IP addresses count as public.
pub(crate) fn is_public<A: Address>(a: &A) -> bool {
    use std::net::IpAddr;
    let Some(s) = a.to_socket_addr() else { return true };
    let ip = s.ip().to_canonical();
    let local = match ip {
        IpAddr::V4(v) => {
            let [a, b, ..] = v.octets();
            v.is_private() || v.is_link_local() || v.is_broadcast() || (a == 100 && b & 0xc0 == 64) || a == 0
        }
        IpAddr::V6(v) => {
            let s0 = v.segments()[0];
            s0 & 0xfe00 == 0xfc00 || s0 & 0xffc0 == 0xfe80
        }
    };
    !(local || ip.is_loopback() || ip.is_unspecified() || ip.is_multicast())
}

/// The key that proves knowledge of a room's name. Not the topic, which every relay
/// sees: the same name hashed under another label.
fn room_key(app_id: &str, room: &str) -> [u8; 32] {
    Sha256::digest(format!("peerwire/room-key/{app_id}/{room}")).into()
}

/// The key a room's hellos are sealed under, as `nostr.js` derives it: the name
/// hashed under a third label, so only those given the name read who is in the room.
fn room_enc_key(app_id: &str, room: &str) -> [u8; 32] {
    Sha256::digest(format!("peerwire/room-enc/{app_id}/{room}")).into()
}

/// AES-GCM nonce and tag lengths.
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

/// `body` sealed for a room as `nostr.js` `sealRoom` seals it: the nonce, then the
/// AES-256-GCM ciphertext and tag, with no associated data. The carrier base64s it.
fn seal_room(key: &[u8; 32], nonce: [u8; NONCE_LEN], body: &[u8]) -> Vec<u8> {
    let ct = Aes256Gcm::new(key.into()).encrypt(&nonce.into(), body).expect("a hello is far below GCM's limit");
    [&nonce[..], &ct].concat()
}

/// The body of a [`seal_room`] payload, if it is one sealed under `key` and unaltered.
/// Anything longer than a sealed hello is refused unread.
fn open_room(key: &[u8; 32], b: &[u8]) -> Option<Vec<u8>> {
    if !(NONCE_LEN + TAG_LEN..=NONCE_LEN + HELLO_MAX_LEN + TAG_LEN).contains(&b.len()) {
        return None;
    }
    let (nonce, ct) = b.split_at(NONCE_LEN);
    Aes256Gcm::new(key.into()).decrypt(Nonce::from_slice(nonce), ct).ok()
}

fn hmac(key: &[u8; 32], parts: &[&[u8]]) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m
}

/// HMAC-SHA256 of `parts`, concatenated, under `key`.
fn mac(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    hmac(key, parts).finalize().into_bytes().into()
}

/// Checks a proof from [`mac`] in constant time.
fn mac_ok(key: &[u8; 32], parts: &[&[u8]], proof: &[u8]) -> bool {
    hmac(key, parts).verify_slice(proof).is_ok()
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Peer => "peer",
        Role::Host => "host",
        Role::Client => "client",
    }
}

const HELLO_HEAD: &str = r#"{"t":"hello","from":""#;
const HELLO_MID: &str = r#"","meta":{"nb":1,"role":""#;
const HELLO_TS: &str = r#"","ts":"#;
const HELLO_PROOF: &str = r#","p":""#;
const HELLO_TAIL: &str = r#""}}"#;
/// The longest hello: role `client`, a 19-digit `ts`.
const HELLO_MAX_LEN: usize = HELLO_HEAD.len()
    + 64
    + HELLO_MID.len()
    + "client".len()
    + HELLO_TS.len()
    + 19
    + HELLO_PROOF.len()
    + 64
    + HELLO_TAIL.len();

/// The room presence message, in the JSON shape `nostr.js` uses (`{t, from, meta}`),
/// before [`seal_room`] seals it. `meta.nb` marks a UDP peer, so each side tells the
/// other's hellos apart (`room.js` doesn't dial it over WebRTC); `meta.ts` is the
/// sender's Unix time in ms; `meta.p` is the room proof, an HMAC under the room key
/// over the sender, its role and `ts` (so a captured hello goes stale).
fn hello(key: &[u8; 32], id: &PeerId, role: Role, ts: u64) -> Vec<u8> {
    let proof = mac(key, &[b"pw-hello", id.as_bytes(), &[role as u8], &ts.to_le_bytes()]);
    let (id, name, proof) = (hex(id.as_bytes()), role_name(role), hex(&proof));
    format!("{HELLO_HEAD}{id}{HELLO_MID}{name}{HELLO_TS}{ts}{HELLO_PROOF}{proof}{HELLO_TAIL}").into_bytes()
}

fn parse_hello(b: &[u8]) -> Option<(PeerId, Role, u64, [u8; 32])> {
    let s = std::str::from_utf8(b).ok()?;
    let rest = s.strip_prefix(HELLO_HEAD)?;
    let (id_hex, rest) = (rest.get(..64)?, rest.get(64..)?);
    let (role, rest) = rest.strip_prefix(HELLO_MID)?.strip_suffix(HELLO_TAIL)?.split_once(HELLO_TS)?;
    let (ts, proof_hex) = rest.split_once(HELLO_PROOF)?;
    let role = [Role::Peer, Role::Host, Role::Client].into_iter().find(|r| role_name(*r) == role)?;
    if ts.is_empty() || ts.len() > 19 || !ts.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (mut id, mut proof) = ([0u8; 32], [0u8; 32]);
    (unhex(id_hex, &mut id) && unhex(proof_hex, &mut proof)).then_some((PeerId(id), role, ts.parse().ok()?, proof))
}

/// What an offer's proof is under (it follows the body, inside the seal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proof {
    /// Version 1: none.
    None,
    /// Version 2: the room key.
    Room,
    /// Version 3: the recipient's invite key.
    Invite,
}

/// Candidates for one peer. Sealed to it, so only it reads them.
#[derive(Clone, Debug, PartialEq)]
struct Offer<A> {
    kind: Proof,
    /// The sender wants an offer back (it will dial when that arrives).
    reply: bool,
    role: Role,
    nat: NatType,
    /// The sender's NAT is symmetric and the offer says how it maps (an entry that
    /// nodes before NB-083 skip, so they read [`NatType::SymmetricIp`] as `Symmetric`):
    /// the sender does its part of a punch with a `SymmetricIp` peer (NB-083).
    says_mapping: bool,
    /// Sender's Unix time in ms, and a random nonce: replays are dropped.
    ts: u64,
    nonce: u64,
    addrs: Vec<A>,
    /// The sender's NAT's likely next port, at the first address's IP.
    predict: Option<Prediction>,
    /// Peers the sender is connected to, which could relay.
    relays: Vec<PeerId>,
    /// The sender's relayed addresses (its TURN allocations'), in entries old decoders skip.
    relayed: Vec<A>,
}

/// The port a sequential symmetric NAT will likely hand out next, and its step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Prediction {
    next: u16,
    step: u16,
}

impl Prediction {
    /// From the ports a NAT handed out to STUN servers, in the order they were asked, each
    /// once: sequential if each is 1 to `MAX_STEP` above the one before, at one IP. The
    /// step is the first two's, and the next port comes after the last one (with a
    /// third server, NB-083, one more than the first two).
    fn from_mappings<A: Address>(handed: &[&A]) -> Option<Prediction> {
        let (first, second, last) = (*handed.first()?, *handed.get(1)?, *handed.last()?);
        let step = |a: &A, b: &A| {
            let (a, b) = (a.to_socket_addr()?, b.to_socket_addr()?);
            let step = b.port().wrapping_sub(a.port());
            (a.ip() == b.ip() && (1..=MAX_STEP).contains(&step)).then_some(step)
        };
        let ok = handed.windows(2).all(|w| step(w[0], w[1]).is_some());
        let (step, last) = (step(first, second).filter(|_| ok)?, last.to_socket_addr()?.port());
        Some(Prediction { next: last.checked_add(step)?, step })
    }

    /// The addresses to probe: `base`'s IP, at ports around `next`.
    fn window<A: Address>(&self, base: &A) -> Vec<A> {
        let Some(port) = base.to_socket_addr().map(|s| i32::from(s.port())) else { return Vec::new() };
        let at = |k: i32| base.port_offset(i32::from(self.next) + k * i32::from(self.step) - port);
        (-GUESS_BEFORE..=GUESS_AFTER).filter_map(at).collect()
    }

    /// `count` addresses at `base`'s IP for a lockstep sweep: ports `next + k·stride·step`.
    fn sweep<A: Address>(&self, base: &A, stride: i32, count: u16) -> Vec<A> {
        let Some(port) = base.to_socket_addr().map(|s| i32::from(s.port())) else { return Vec::new() };
        let at = |k: i32| base.port_offset(i32::from(self.next) + k * stride * i32::from(self.step) - port);
        (0..i32::from(count)).filter_map(at).collect()
    }

    // [tag: fe 01] [step u8] [next u16 le]
    fn encode(&self) -> [u8; 5] {
        let [lo, hi] = self.next.to_le_bytes();
        [PREDICTION_TAG[0], PREDICTION_TAG[1], self.step as u8, lo, hi]
    }

    fn decode(b: &[u8]) -> Option<Prediction> {
        let &[t0, t1, step, lo, hi] = b else { return None };
        let p = Prediction { next: u16::from_le_bytes([lo, hi]), step: step.into() };
        ([t0, t1] == PREDICTION_TAG && (1..=MAX_STEP).contains(&p.step)).then_some(p)
    }
}

impl<A: Address> Offer<A> {
    // [1|2|3] [flags] [role] [nat] [ts u64 le] [nonce u64 le] [n] n*([len] addr) [m] m*(32 B peer id)
    // A room (2) or invite (3) offer is followed, outside this body, by its 32-byte proof.
    // After the addresses, the n entries may hold a port prediction (see `Prediction::encode`)
    // and, from a symmetric NAT, how it maps: [tag: fe 02] [1 per IP, 0 per IP and port].
    // `nat` is one of the first four `NatType`s: `SymmetricIp` goes as `Symmetric` and the tag.
    fn encode(&self) -> Vec<u8> {
        let role = [Role::Peer, Role::Host, Role::Client].iter().position(|r| *r == self.role).unwrap() as u8;
        let per_ip = self.nat == NatType::SymmetricIp;
        let mapping = per_ip || (self.nat == NatType::Symmetric && self.says_mapping);
        let nat = if per_ip { NatType::Symmetric } else { self.nat };
        let nat = [NatType::Unknown, NatType::Open, NatType::Cone, NatType::Symmetric].iter().position(|n| *n == nat);
        let version = match self.kind {
            Proof::None => OFFER,
            Proof::Room => ROOM_OFFER,
            Proof::Invite => INVITE_OFFER,
        };
        let mut v = vec![version, self.reply as u8, role, nat.unwrap() as u8];
        v.extend_from_slice(&self.ts.to_le_bytes());
        v.extend_from_slice(&self.nonce.to_le_bytes());
        let relayed = &self.relayed[..self.relayed.len().min(MAX_RELAYED)];
        let tagged = self.predict.is_some() as usize + mapping as usize + relayed.len();
        let n = self.addrs.len().min(MAX_CANDIDATES - tagged);
        v.push((n + tagged) as u8);
        for a in self.addrs.iter().take(n) {
            let mut e = Vec::new();
            a.encode(&mut e);
            v.push(e.len() as u8);
            v.extend_from_slice(&e);
        }
        if let Some(p) = self.predict {
            v.push(5);
            v.extend_from_slice(&p.encode());
        }
        if mapping {
            v.extend_from_slice(&[3, MAPPING_TAG[0], MAPPING_TAG[1], per_ip as u8]);
        }
        for a in relayed {
            let mut e = RELAYED_TAG.to_vec();
            a.encode(&mut e);
            v.push(e.len() as u8);
            v.extend_from_slice(&e);
        }
        v.push(self.relays.len().min(MAX_RELAYS) as u8);
        for p in self.relays.iter().take(MAX_RELAYS) {
            v.extend_from_slice(p.as_bytes());
        }
        v
    }

    fn decode(b: &[u8]) -> Option<Self> {
        let (&[version @ (OFFER | ROOM_OFFER | INVITE_OFFER), flags, role, nat], rest) = b.split_first_chunk::<4>()?
        else {
            return None;
        };
        let role = *[Role::Peer, Role::Host, Role::Client].get(role as usize)?;
        let nat = *[NatType::Unknown, NatType::Open, NatType::Cone, NatType::Symmetric].get(nat as usize)?;
        let (ts, rest) = rest.split_first_chunk::<8>()?;
        let (nonce, rest) = rest.split_first_chunk::<8>()?;
        let (&n, mut rest) = rest.split_first()?;
        if n as usize > MAX_CANDIDATES || flags > 1 {
            return None;
        }
        let (mut addrs, mut predict, mut per_ip, mut relayed) = (Vec::new(), None, None, Vec::new());
        for _ in 0..n {
            let (&len, r) = rest.split_first()?;
            let (a, r) = (r.get(..len as usize)?, r.get(len as usize..)?);
            if a.starts_with(&PREDICTION_TAG) {
                predict = predict.or(Prediction::decode(a));
            } else if a.starts_with(&MAPPING_TAG) {
                per_ip = per_ip.or(match a {
                    &[_, _, m @ (0 | 1)] => Some(m == 1),
                    _ => None,
                });
            } else if a.starts_with(&RELAYED_TAG) {
                relayed.extend(A::decode(&a[2..]).filter(|_| relayed.len() < MAX_RELAYED));
            } else {
                addrs.extend(A::decode(a));
            }
            rest = r;
        }
        // How a NAT maps counts only from a symmetric one.
        let says_mapping = nat == NatType::Symmetric && per_ip.is_some();
        let nat = if says_mapping && per_ip == Some(true) { NatType::SymmetricIp } else { nat };
        let (&m, mut rest) = rest.split_first()?;
        if m as usize > MAX_RELAYS || rest.len() != m as usize * 32 {
            return None;
        }
        let mut relays = Vec::new();
        while let Some((p, r)) = rest.split_first_chunk::<32>() {
            relays.push(PeerId(*p));
            rest = r;
        }
        Some(Offer {
            kind: [Proof::None, Proof::Room, Proof::Invite][version as usize - 1],
            reply: flags == 1,
            role,
            nat,
            says_mapping,
            ts: u64::from_le_bytes(*ts),
            nonce: u64::from_le_bytes(*nonce),
            addrs,
            predict,
            relays,
            relayed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerwire_core::Config;
    use peerwire_sim::fuzz::{cases, hex as fuzz_hex, mutate, run, seed, Rng};
    use std::net::SocketAddr;

    fn id(n: u8) -> Identity {
        Identity::from_secret_bytes(&[n; 32])
    }

    #[test]
    fn hello_round_trips_and_ignores_browser_hellos() {
        let p = id(1).peer_id();
        let key = room_key("app", "room");
        for role in [Role::Peer, Role::Host, Role::Client] {
            let ts = 1_800_000_000_123u64;
            let (from, r, t, proof) = parse_hello(&hello(&key, &p, role, ts)).unwrap();
            assert_eq!((from, r, t), (p, role, ts));
            let parts =
                |role: u8, ts: u64| mac_ok(&key, &[b"pw-hello", p.as_bytes(), &[role], &ts.to_le_bytes()], &proof);
            assert!(parts(role as u8, ts));
            assert!(!parts(9, ts), "bound to the role");
            assert!(!parts(role as u8, ts + 1), "bound to the time");
            let other = room_key("app", "other");
            assert!(!mac_ok(&other, &[b"pw-hello", p.as_bytes(), &[role as u8], &ts.to_le_bytes()], &proof));
        }
        let js = format!(r#"{{"t":"hello","from":"{}","meta":{{"role":"player"}}}}"#, hex(p.as_bytes()));
        // Not a number, signed, or too long: refused.
        let h = String::from_utf8(hello(&key, &p, Role::Peer, 5)).unwrap();
        for bad in ["\"ts\":x,", "\"ts\":+5,", "\"ts\":,", "\"ts\":99999999999999999999,"] {
            assert_eq!(parse_hello(h.replace("\"ts\":5,", bad).as_bytes()), None, "{bad}");
        }
        assert_eq!(parse_hello(js.as_bytes()), None);
        assert_eq!(parse_hello(b"{}"), None);
        let h = hello(&key, &p, Role::Peer, 0);
        for cut in 0..h.len() {
            assert_eq!(parse_hello(&h[..cut]), None);
        }
        // The key is not the topic, nor derivable from it.
        assert_ne!(key, Topic::room("app", "room").0);
    }

    /// NB-022b (SEC-2): a hello is sealed exactly as `nostr.js` `sealRoom` seals it, so
    /// the Nostr `content` is the same bytes. The vector was made by `sealRoom` (nonce
    /// 0..11) and is asserted again in packages/js/test/rust_hello.mjs: change both or neither.
    #[test]
    fn hello_seal_matches_nostr_js() {
        const ENC: &str = "7e6a0b7e0ba0d5c263896aa5345c379c8bd23f796815a02a6282b076002fbb96";
        const BODY: &str = concat!(
            r#"{"t":"hello","from":"8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c","#,
            r#""meta":{"nb":1,"role":"peer","ts":1800000000123,"#,
            r#""p":"e72685be158e95e099ef47af264ce803b7380a7e1204708b9c3e582dd43bb2b7"}}"#
        );
        const SEALED: &str = concat!(
            "AAECAwQFBgcICQoLbW0PRSqiDrpAc6clOpp1LqmbpsEwi1V1wdARHJcaKrdJxmyODZ8iKLorORTIX6Vx",
            "8SqfUhbwUB9d/uZyGAkxUyvvZjSlOtZMEp+2Mb2gIdnJdMv2G1Nt0R3eGuOeeypLbPIz3oSh0hb4d+DQ",
            "MCqOucNUEwbFBqFZNWSPsHEajTFV5wIaTVagPhYhSq47vXbL5Yu6HVfna1kQ4yoOgiCbyoqB3bpRUtdn",
            "5dBQT6nGdTxbCDzj7UYsZGHduNlouDgcziOF/y02ZiP4F4134j4u345c0SOyA1AU23udVOprOg==",
        );
        let (key, enc) = (room_key("demo", "lobby"), room_enc_key("demo", "lobby"));
        assert_eq!(hex(&enc), ENC);
        let body = hello(&key, &id(1).peer_id(), Role::Peer, 1_800_000_000_123);
        assert_eq!(std::str::from_utf8(&body), Ok(BODY));
        let sealed = seal_room(&enc, std::array::from_fn(|i| i as u8), &body);
        assert_eq!(crate::nostr::b64(&sealed), SEALED);
        assert_eq!(open_room(&enc, &sealed).as_deref(), Some(BODY.as_bytes()));
    }

    /// NB-022b (SEC-2): a member's hellos are sealed, each under a fresh nonce, with no
    /// PeerId in the clear; and a hello that doesn't open under the room's key is dropped:
    /// in plaintext, sealed for another room, with any byte altered, cut, or extended.
    #[test]
    fn hellos_are_sealed_and_only_sealed_ones_count() {
        let me = id(1).peer_id();
        let (key, enc, topic) = (room_key(ROOM.0, ROOM.1), room_enc_key(ROOM.0, ROOM.1), Topic::room(ROOM.0, ROOM.1));
        let unix_ms = 1_700_000_000_000;
        let local: SocketAddr = "10.0.0.1:7000".parse().unwrap();
        let mut t = Traversal::new(&id(1), [3; 32], unix_ms, vec![local], vec![]);
        let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
        t.join_room(Instant::ZERO, ROOM.0, ROOM.1, Role::Host, None);
        t.handle_timeout(Instant::ZERO + HELLO_FIRST, &mut ep);
        let sent: Vec<Vec<u8>> = std::iter::from_fn(|| t.poll_signal())
            .filter_map(|s| match s {
                Signal::Publish(p, b) if p == topic => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(sent.len(), 2);
        for (b, ts) in sent.iter().zip([unix_ms, unix_ms + 2000]) {
            assert_eq!(open_room(&enc, b), Some(hello(&key, &me, Role::Host, ts)));
            assert_eq!(open_room(&room_enc_key(ROOM.0, "other"), b), None);
            let clear = String::from_utf8_lossy(b);
            assert!(!b.windows(32).any(|w| w == me.as_bytes()) && !clear.contains(&hex(me.as_bytes())));
            assert!(!clear.contains("hello"));
        }
        assert_ne!(sent[0][..NONCE_LEN], sent[1][..NONCE_LEN], "a fresh nonce each");

        // The longest hello that parses fits the length bound; one byte more is refused unread.
        let longest = hello(&key, &me, Role::Client, 9_999_999_999_999_999_999);
        assert_eq!(longest.len(), HELLO_MAX_LEN);
        assert!(open_room(&enc, &seal_room(&enc, [5; NONCE_LEN], &longest)).is_some_and(|b| parse_hello(&b).is_some()));
        assert_eq!(open_room(&enc, &seal_room(&enc, [5; NONCE_LEN], &[0; HELLO_MAX_LEN + 1])), None);

        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        let now = Instant::from_millis(10);
        let m = id(5).peer_id();
        let plain = hello(&key, &m, Role::Peer, 0);
        let good = room_hello(&key, &m, Role::Peer, 0);
        let mut drops = vec![
            plain.clone(),
            seal_room(&room_enc_key(ROOM.0, "other"), [5; NONCE_LEN], &plain),
            seal_room(&topic.0, [5; NONCE_LEN], &plain),
            seal_room(&key, [5; NONCE_LEN], &plain),
            [&good[..], &[0]].concat(),
        ];
        for i in 0..good.len() {
            let mut b = good.clone();
            b[i] ^= 1;
            drops.push(b);
            drops.push(good[..i].to_vec());
        }
        for b in &drops {
            t.handle_signal(now, &mut ep, topic, b);
        }
        assert_eq!((published(&mut t), dials(&mut ep, now), t.room_role(&m)), (0, vec![], None));
        // The same hello, sealed: answered.
        t.handle_signal(now, &mut ep, topic, &good);
        assert_eq!(published(&mut t), 1);
    }

    #[test]
    fn offer_round_trips_and_rejects_garbage() {
        let o: Offer<SocketAddr> = Offer {
            kind: Proof::None,
            reply: true,
            role: Role::Client,
            nat: NatType::Symmetric,
            says_mapping: false,
            ts: 1_700_000_000_123,
            nonce: 42,
            addrs: vec!["203.0.113.5:4000".parse().unwrap(), "[2001:db8::1]:9".parse().unwrap()],
            predict: None,
            relays: vec![id(2).peer_id()],
            relayed: Vec::new(),
        };
        let b = o.encode();
        assert_eq!(Offer::decode(&b), Some(o));
        for cut in 0..b.len() {
            assert_eq!(Offer::<SocketAddr>::decode(&b[..cut]), None, "cut at {cut}");
        }
        let mut long = b.clone();
        long.push(0);
        assert_eq!(Offer::<SocketAddr>::decode(&long), None);
        for (kind, version) in [(Proof::Room, ROOM_OFFER), (Proof::Invite, INVITE_OFFER)] {
            let o: Offer<SocketAddr> = Offer { kind, ..Offer::decode(&b).unwrap() };
            let ob = o.encode();
            assert_eq!(ob[0], version);
            assert_eq!(Offer::decode(&ob), Some(o));
        }
        let mut v4 = b.clone();
        v4[0] = 4;
        assert_eq!(Offer::<SocketAddr>::decode(&v4), None);
    }

    const ROOM: (&str, &str) = ("app", "room");

    /// A sealed offer from `from` to `to`, with a room proof under `key` if given.
    fn offer_from(
        from: &Identity,
        to: &PeerId,
        key: Option<&[u8; 32]>,
        nonce: u64,
        role: Role,
        addrs: Vec<SocketAddr>,
    ) -> Vec<u8> {
        proved_offer(from, to, key.map(|k| (Proof::Room, k)), nonce, role, addrs)
    }

    /// A sealed offer from `from` to `to`, with a proof of `kind` under its key.
    fn proved_offer(
        from: &Identity,
        to: &PeerId,
        proof: Option<(Proof, &[u8; 32])>,
        nonce: u64,
        role: Role,
        addrs: Vec<SocketAddr>,
    ) -> Vec<u8> {
        let kind = proof.map_or(Proof::None, |p| p.0);
        let nat = NatType::Cone;
        let o = Offer {
            kind,
            reply: true,
            role,
            nat,
            says_mapping: false,
            ts: 0,
            nonce,
            addrs,
            predict: None,
            relays: vec![],
            relayed: Vec::new(),
        };
        let mut body = o.encode();
        if let Some((kind, k)) = proof {
            let label: &[u8] = if kind == Proof::Room { b"pw-offer" } else { b"pw-invite" };
            body.extend(mac(k, &[label, from.peer_id().as_bytes(), to.as_bytes(), &body]));
        }
        sealed::seal(from, to, [7; 32], &body).unwrap()
    }

    /// A traversal (`id(1)`) in `ROOM` as `role`, with its endpoint and drained signals.
    fn in_room(role: Role, host: Option<PeerId>, local: SocketAddr) -> (Traversal<SocketAddr>, Endpoint<SocketAddr>) {
        let ia = id(1);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![local], vec![]);
        t.join_room(Instant::ZERO, ROOM.0, ROOM.1, role, host);
        while t.poll_signal().is_some() {}
        (t, Endpoint::new(&ia, Config::default(), [1; 32]))
    }

    /// A hello from `from` with a proof under `key`, sealed for `ROOM` as a member seals it.
    fn room_hello(key: &[u8; 32], from: &PeerId, role: Role, ts: u64) -> Vec<u8> {
        seal_room(&room_enc_key(ROOM.0, ROOM.1), [5; NONCE_LEN], &hello(key, from, role, ts))
    }

    fn published(t: &mut Traversal<SocketAddr>) -> usize {
        std::iter::from_fn(|| t.poll_signal()).filter(|s| matches!(s, Signal::Publish(..))).count()
    }

    fn dials(ep: &mut Endpoint<SocketAddr>, now: Instant) -> Vec<SocketAddr> {
        std::iter::from_fn(|| ep.poll_transmit(now)).map(|t| t.dst).collect()
    }

    /// NB-031: knowing the room topic (what relays see) is not enough to be linked
    /// with, nor to be taken for the host.
    #[test]
    fn room_offers_and_hellos_need_the_room_key() {
        let me = id(1).peer_id();
        let local: SocketAddr = "10.0.0.1:7000".parse().unwrap();
        let there: SocketAddr = "198.51.100.9:4000".parse().unwrap();
        let (key, topic, inbox) = (room_key(ROOM.0, ROOM.1), Topic::room(ROOM.0, ROOM.1), Topic::peer(&me));
        let now = Instant::from_millis(10);
        let (mut t, mut ep) = in_room(Role::Client, None, local);
        let attacker = id(9);
        // A plain offer, and one "proved" with the topic hash as key: dropped.
        t.handle_signal(now, &mut ep, inbox, &offer_from(&attacker, &me, None, 1, Role::Host, vec![there]));
        t.handle_signal(now, &mut ep, inbox, &offer_from(&attacker, &me, Some(&topic.0), 2, Role::Host, vec![there]));
        // Hellos without a proof (the old format), or with a wrong one: ignored.
        let ap = attacker.peer_id();
        let old = format!(r#"{{"t":"hello","from":"{}","meta":{{"nb":1,"role":"host"}}}}"#, hex(ap.as_bytes()));
        t.handle_signal(now, &mut ep, topic, old.as_bytes());
        t.handle_signal(now, &mut ep, topic, &seal_room(&room_enc_key(ROOM.0, ROOM.1), [5; NONCE_LEN], old.as_bytes()));
        t.handle_signal(now, &mut ep, topic, &room_hello(&topic.0, &ap, Role::Host, 0));
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![], 0));
        assert_eq!((t.room_role(&ap), t.admits(&ap)), (None, false));
        // A member's offer: dialled, answered, and its role is known.
        let host = id(2);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&host, &me, Some(&key), 3, Role::Host, vec![there]));
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![there], 1));
        assert_eq!((t.room_role(&host.peer_id()), t.admits(&host.peer_id())), (Some(Role::Host), true));
        // A member's hello in a role this client doesn't link with: no offer.
        t.handle_signal(now, &mut ep, topic, &room_hello(&key, &id(3).peer_id(), Role::Client, 0));
        assert_eq!(published(&mut t), 0);
        // Leaving the room forgets who proved what; outside a room, links are anyone's call.
        t.leave_room();
        assert_eq!((t.room_role(&host.peer_id()), t.admits(&ap)), (None, true));

        // With an expected host, other members are ignored.
        let (mut t, mut ep) = in_room(Role::Client, Some(id(3).peer_id()), local);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&host, &me, Some(&key), 4, Role::Host, vec![there]));
        t.handle_signal(now, &mut ep, topic, &room_hello(&key, &host.peer_id(), Role::Host, 0));
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![], 0));
        t.handle_signal(now, &mut ep, topic, &room_hello(&key, &id(3).peer_id(), Role::Host, 0));
        assert_eq!(published(&mut t), 1);
        assert!(t.admits(&id(3).peer_id()) && !t.admits(&host.peer_id()));
    }

    /// An invite dial still punches from inside a room: the dialled peer's plain
    /// offer is welcome, nobody else's.
    #[test]
    fn invited_peers_may_answer_with_plain_offers_in_a_room() {
        let me = id(1).peer_id();
        let there: SocketAddr = "198.51.100.9:4000".parse().unwrap();
        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        let now = Instant::from_millis(10);
        t.introduce(now, &ep, id(2).peer_id(), None);
        assert_eq!(published(&mut t), 1);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &offer_from(&id(2), &me, None, 1, Role::Peer, vec![there]));
        assert_eq!(dials(&mut ep, now), vec![there]);
        assert!(t.admits(&id(2).peer_id()) && t.room_role(&id(2).peer_id()).is_none());
    }

    /// NB-031: a v2 (room) offer without a verified proof, from an invited peer, is
    /// dropped: it must not give that peer a room role such as Host. Security's
    /// repro is the 22-byte one, short enough to skip the proof split.
    #[test]
    fn short_room_offer_without_proof_is_dropped() {
        let me = id(1).peer_id();
        let (mut t, mut ep) = in_room(Role::Client, None, "10.0.0.1:7000".parse().unwrap());
        let now = Instant::from_millis(10);
        t.introduce(now, &ep, id(2).peer_id(), None);
        while t.poll_signal().is_some() {}
        let offer = |kind, nonce| {
            let o = Offer::<SocketAddr> {
                kind,
                reply: true,
                role: Role::Host,
                nat: NatType::Cone,
                says_mapping: false,
                ts: 0,
                nonce,
                addrs: vec![],
                predict: None,
                relays: vec![],
                relayed: Vec::new(),
            };
            o.encode()
        };
        let mut send = |t: &mut Traversal<SocketAddr>, body: &[u8]| {
            t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed::seal(&id(2), &me, [7; 32], body).unwrap());
            (t.room_role(&id(2).peer_id()), published(t))
        };
        let short = offer(Proof::Room, 1);
        assert_eq!(short.len(), 22);
        assert_eq!(send(&mut t, &short), (None, 0));
        // Padded with 32 bytes that aren't the proof, or as a short v3 (invite) offer: dropped too.
        let padded = [offer(Proof::Room, 2), vec![0; PROOF_LEN]].concat();
        assert_eq!(send(&mut t, &padded), (None, 0));
        assert_eq!(send(&mut t, &offer(Proof::Invite, 3)), (None, 0));
        // With the real proof, the same offer does make it the Host.
        let body = offer(Proof::Room, 4);
        let proof = mac(&room_key(ROOM.0, ROOM.1), &[b"pw-offer", id(2).peer_id().as_bytes(), me.as_bytes(), &body]);
        assert_eq!(send(&mut t, &[body, proof.to_vec()].concat()), (Some(Role::Host), 1));
    }

    /// NB-032: forged hellos cost nothing, and hellos, offers and each sender
    /// spend separate budgets.
    #[test]
    fn hellos_offers_and_senders_have_separate_budgets() {
        let me = id(1).peer_id();
        let (key, topic, inbox) = (room_key(ROOM.0, ROOM.1), Topic::room(ROOM.0, ROOM.1), Topic::peer(&me));
        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        let t0 = Instant::ZERO;
        let member = |n: u8| id(n.wrapping_add(10)).peer_id();
        // 200 forged hellos: nothing sent, no token spent.
        for n in 0..200u8 {
            t.handle_signal(t0, &mut ep, topic, &room_hello(&topic.0, &member(n), Role::Peer, 0));
        }
        assert_eq!(published(&mut t), 0);
        // Real hellos from 100 members: a burst of answers, then none.
        for n in 0..100u8 {
            t.handle_signal(t0, &mut ep, topic, &room_hello(&key, &member(n), Role::Peer, 0));
        }
        assert_eq!(published(&mut t), BURST as usize);
        // Offers have their own budget: still acted on (each answered).
        for n in 0..5u8 {
            let from = id(200 + n);
            t.handle_signal(t0, &mut ep, inbox, &offer_from(&from, &me, Some(&key), 1, Role::Peer, vec![]));
        }
        assert_eq!(published(&mut t), 5);
        // One sender gets SENDER_BURST, and leaves the rest of the budget to others.
        let loud = id(220);
        for nonce in 0..50 {
            t.handle_signal(t0, &mut ep, inbox, &offer_from(&loud, &me, Some(&key), nonce, Role::Peer, vec![]));
        }
        assert_eq!(published(&mut t), SENDER_BURST as usize);
        t.handle_signal(t0, &mut ep, inbox, &offer_from(&id(221), &me, Some(&key), 1, Role::Peer, vec![]));
        assert_eq!(published(&mut t), 1);
        // The sender's limit refills.
        let later = t0 + SENDER_REFILL * 2;
        for nonce in 100..110 {
            t.handle_signal(later, &mut ep, inbox, &offer_from(&loud, &me, Some(&key), nonce, Role::Peer, vec![]));
        }
        assert_eq!(published(&mut t), 2);
        // And the hello budget at one per REFILL.
        let later = t0 + REFILL * 3;
        for n in 100..150u8 {
            t.handle_signal(later, &mut ep, topic, &room_hello(&key, &member(n), Role::Peer, 0));
        }
        assert_eq!(published(&mut t), 3);
    }

    /// NB64-2: inbox messages spend a token before they are opened, so after a burst
    /// of junk a valid offer waits for a refill, and is then acted on.
    #[test]
    fn junk_on_the_inbox_spends_the_open_budget() {
        let me = id(1).peer_id();
        let (key, inbox) = (room_key(ROOM.0, ROOM.1), Topic::peer(&me));
        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        let t0 = Instant::from_millis(10);
        // What a neighbour can send to a topic it read off the wire: it doesn't open.
        for n in 0..INBOX_BURST {
            t.handle_signal(t0, &mut ep, inbox, &[n as u8; 96]);
        }
        let valid = |nonce| offer_from(&id(2), &me, Some(&key), nonce, Role::Peer, vec![]);
        t.handle_signal(t0, &mut ep, inbox, &valid(1));
        assert_eq!(published(&mut t), 0, "no token left to open it");
        // One token back after a refill period: one offer is acted on, the next waits again.
        let later = t0 + INBOX_REFILL;
        t.handle_signal(later, &mut ep, inbox, &valid(2));
        assert_eq!(published(&mut t), 1);
        assert_eq!(t.room_role(&id(2).peer_id()), Some(Role::Peer));
        t.handle_signal(later, &mut ep, inbox, &offer_from(&id(3), &me, Some(&key), 1, Role::Peer, vec![]));
        assert_eq!(published(&mut t), 0);
    }

    /// NB-032(b): nonces are kept only for offers acted on, and the map is capped.
    #[test]
    fn nonces_are_kept_only_for_offers_acted_on() {
        let ia = id(1);
        let me = ia.peer_id();
        let mut ep: Endpoint<SocketAddr> = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec!["10.0.0.1:7000".parse().unwrap()], vec![]);
        let k = t.invite_key();
        for n in 0..200u8 {
            let o = proved_offer(&id(n.wrapping_add(2)), &me, Some((Proof::Invite, &k)), 1, Role::Peer, vec![]);
            t.handle_signal(Instant::ZERO, &mut ep, Topic::peer(&me), &o);
        }
        assert_eq!(t.nonces.len(), BURST as usize);
        assert!(
            MAX_NONCES >= (BURST as usize + (2 * FRESH_MS as usize / REFILL.as_millis() as usize)),
            "covers the window"
        );
    }

    /// NB-033(a): an offer (or an observed address) can't aim a node at its own host,
    /// multicast, broadcast, link-local, the unspecified address or port 0; loopback
    /// only between loopback nodes.
    #[test]
    fn offers_cannot_aim_at_special_addresses() {
        let me = id(1).peer_id();
        let addrs: Vec<SocketAddr> = [
            "198.51.100.7:9",
            "127.0.0.1:5",
            "[::ffff:127.0.0.1]:5",
            "0.0.0.0:5",
            "224.0.0.1:5",
            "[ff02::1]:5",
            "255.255.255.255:5",
            "198.51.100.7:0",
            "169.254.1.1:5",
            "[fe80::1]:5",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        let now = Instant::from_millis(10);
        let run = |local: &str| {
            let ia = id(1);
            let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
            let mut t = Traversal::new(&ia, [3; 32], 0, vec![local.parse().unwrap()], vec![]);
            let k = t.invite_key();
            let o = proved_offer(&id(2), &me, Some((Proof::Invite, &k)), 1, Role::Peer, addrs.clone());
            t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
            let mut d = dials(&mut ep, now);
            d.sort();
            d.dedup();
            d
        };
        assert_eq!(run("10.0.0.1:7000"), vec!["198.51.100.7:9".parse().unwrap()]);
        let on_lo = run("127.0.0.1:7000");
        assert_eq!(on_lo, vec!["127.0.0.1:5".parse().unwrap(), "198.51.100.7:9".parse().unwrap()], "::ffff: folded in");
        // Observed addresses are filtered the same way.
        let ia = id(1);
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec!["10.0.0.1:7000".parse().unwrap()], vec![]);
        for a in &addrs {
            t.handle_event(now, &mut ep, &Event::AddrObserved { by: id(2).peer_id(), addr: *a });
        }
        assert_eq!(t.candidates(), vec!["198.51.100.7:9".parse().unwrap(), "10.0.0.1:7000".parse().unwrap()]);
    }

    /// NB42-I1: an IPv4-mapped copy (`::ffff:a.b.c.d`) of an address counts as
    /// that address: an offer can't make a node dial its own LAN address that
    /// way, nor an observation add a second copy of one it has.
    #[test]
    fn mapped_addresses_count_as_ipv4() {
        let at = |s: &str| -> SocketAddr { s.parse().unwrap() };
        let (me, now) = (id(1).peer_id(), Instant::from_millis(10));
        let (lan, public) = (at("10.0.0.1:7000"), at("198.51.100.7:9"));
        let local = vec![lan, at("[2001:db8::1]:7000")];
        let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
        let mut t = Traversal::new(&id(1), [3; 32], 0, local.clone(), vec![]);
        let k = t.invite_key();
        let o = proved_offer(&id(2), &me, Some((Proof::Invite, &k)), 1, Role::Peer, vec![at("[::ffff:10.0.0.1]:7000")]);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
        assert_eq!(dials(&mut ep, now), vec![], "our own LAN address");
        let o =
            proved_offer(&id(3), &me, Some((Proof::Invite, &k)), 2, Role::Peer, vec![at("[::ffff:198.51.100.7]:9")]);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
        assert_eq!(dials(&mut ep, now), vec![public], "dialled as IPv4");

        for a in ["[::ffff:198.51.100.7]:9", "198.51.100.7:9", "[::ffff:10.0.0.1]:7000"] {
            t.handle_event(now, &mut ep, &Event::AddrObserved { by: id(2).peer_id(), addr: at(a) });
        }
        assert_eq!(t.candidates(), [vec![public], local].concat());
    }

    /// NB42-I2: a NAT64 address (`64:ff9b::/96`) is dialled only if the IPv4
    /// address it embeds would be.
    #[test]
    fn nat64_addresses_are_judged_by_their_ipv4_address() {
        let ok = |s: &str, lo: bool| dialable(&s.parse::<SocketAddr>().unwrap(), lo);
        for a in ["[64:ff9b::c633:6407]:9", "[64:ff9b::a00:1]:9", "[2001:db8::7f00:1]:9"] {
            assert!(ok(a, false), "{a}");
        }
        for a in ["[64:ff9b::7f00:1]:9", "[64:ff9b::a9fe:101]:9", "[64:ff9b::ffff:ffff]:9", "[64:ff9b::e000:1]:9"] {
            assert!(!ok(a, false), "{a}");
        }
        assert!(!ok("[64:ff9b::]:9", true), "embeds 0.0.0.0");
        assert!(ok("[64:ff9b::7f00:1]:9", true), "loopback between loopback nodes, as for IPv4");
    }

    /// Links `ep` (at `at`) with a fresh endpoint for `peer` (at `peer_at`), in memory.
    fn link(ep: &mut Endpoint<SocketAddr>, at: SocketAddr, peer: &Identity, peer_at: SocketAddr, now: Instant) {
        let mut other = Endpoint::new(peer, Config::default(), [9; 32]);
        ep.connect_with(now, peer.peer_id(), &[Path::Direct(peer_at)]).unwrap();
        for _ in 0..8 {
            while let Some(t) = ep.poll_transmit(now) {
                other.handle_datagram(now, at, &t.payload);
            }
            while let Some(t) = other.poll_transmit(now) {
                ep.handle_datagram(now, peer_at, &t.payload);
            }
        }
        assert!(ep.is_connected(&peer.peer_id()));
    }

    /// NB-033(b): a node names its links and LAN addresses only to a room member or a
    /// peer it dialled. An invite holder it never dialled gets its public addresses
    /// and no relays.
    #[test]
    fn invite_holders_get_public_addresses_and_no_relays() {
        let ia = id(1);
        let me = ia.peer_id();
        let (lan, public): (SocketAddr, SocketAddr) =
            ("10.0.0.1:7000".parse().unwrap(), "203.0.113.5:7000".parse().unwrap());
        let (relay, holder, dialled) = (id(3), id(2), id(5));
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![lan], vec![]);
        let now = Instant::from_millis(10);
        link(&mut ep, lan, &relay, "198.51.100.3:9".parse().unwrap(), now);
        t.handle_event(now, &mut ep, &Event::AddrObserved { by: relay.peer_id(), addr: public });
        let k = t.invite_key();
        let o = proved_offer(&holder, &me, Some((Proof::Invite, &k)), 1, Role::Peer, vec![]);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
        let answer = |t: &mut Traversal<SocketAddr>, to: &Identity| {
            let o = sent_offers(t, to);
            assert_eq!(o.len(), 1);
            (o[0].addrs.clone(), o[0].relays.clone())
        };
        assert_eq!(answer(&mut t, &holder), (vec![public], vec![]), "public only, no relays");
        // A peer this node dials gets everything.
        t.introduce(now, &ep, dialled.peer_id(), None);
        assert_eq!(answer(&mut t, &dialled), (vec![public, lan], vec![relay.peer_id()]));
        // Room members get everything too.
        let (mut t, mut ep) = in_room(Role::Peer, None, lan);
        link(&mut ep, lan, &relay, "198.51.100.3:9".parse().unwrap(), now);
        let key = room_key(ROOM.0, ROOM.1);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &offer_from(&holder, &me, Some(&key), 1, Role::Peer, vec![]));
        assert_eq!(answer(&mut t, &holder), (vec![lan], vec![relay.peer_id()]));
    }

    /// NB-038: a router-mapped address is offered like a public one: after the STUN
    /// address and before observed ones, to invite holders (who get public addresses only) and under
    /// `PublicOnly`, never under `RelayOnly`. A mapping that isn't public is ignored.
    #[test]
    fn mapped_address_is_offered_like_a_public_one() {
        let ia = id(1);
        let me = ia.peer_id();
        let (lan, public, mapped): (SocketAddr, SocketAddr, SocketAddr) = (
            "10.0.0.1:7000".parse().unwrap(),
            "203.0.113.5:7000".parse().unwrap(),
            "203.0.113.5:7001".parse().unwrap(),
        );
        let (relay, holder) = (id(3), id(2));
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![lan], vec![]);
        let now = Instant::from_millis(10);
        assert_eq!(t.candidates(), vec![lan]);
        t.set_mapped(Some(mapped));
        assert_eq!(t.candidates(), vec![mapped, lan]);
        link(&mut ep, lan, &relay, "198.51.100.3:9".parse().unwrap(), now);
        t.handle_event(now, &mut ep, &Event::AddrObserved { by: relay.peer_id(), addr: public });
        // An observed address (no STUN here) comes after it: from a symmetric NAT it
        // only works for the peer that saw it.
        assert_eq!(t.candidates(), vec![mapped, public, lan]);
        let k = t.invite_key();
        let o = proved_offer(&holder, &me, Some((Proof::Invite, &k)), 1, Role::Peer, vec![]);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
        let o = sent_offers(&mut t, &holder);
        assert_eq!((o[0].addrs.clone(), o[0].relays.clone()), (vec![mapped, public], vec![]));
        t.set_privacy(&mut ep, Privacy::PublicOnly);
        assert_eq!(t.candidates(), vec![mapped, public]);
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: None });
        assert_eq!(t.candidates(), vec![]);
        t.set_privacy(&mut ep, Privacy::Direct);
        for not_public in ["100.64.0.9:7001", "192.168.0.2:7001", "127.0.0.1:7001"] {
            t.set_mapped(Some(not_public.parse().unwrap()));
            assert_eq!(t.candidates(), vec![public, lan], "{not_public}");
        }
        t.set_mapped(None);
        assert_eq!(t.candidates(), vec![public, lan]);
    }

    /// NB-033 N33-1: an invite holder can make its own dial fail (an address that never
    /// answers), and still gets public addresses and no relays afterwards, both in
    /// answers and in the re-offer when a link comes up.
    #[test]
    fn invite_holder_cannot_force_full_offer() {
        let ia = id(1);
        let me = ia.peer_id();
        let (lan, public): (SocketAddr, SocketAddr) =
            ("10.0.0.1:7000".parse().unwrap(), "203.0.113.5:7000".parse().unwrap());
        let (relay, holder) = (id(3), id(2));
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![lan], vec![]);
        let mut now = Instant::from_millis(10);
        link(&mut ep, lan, &relay, "198.51.100.3:9".parse().unwrap(), now);
        t.handle_event(now, &mut ep, &Event::AddrObserved { by: relay.peer_id(), addr: public });
        let k = t.invite_key();
        let dead: SocketAddr = "192.0.2.77:9".parse().unwrap();
        let o = proved_offer(&holder, &me, Some((Proof::Invite, &k)), 1, Role::Peer, vec![dead]);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
        let answer = |t: &mut Traversal<SocketAddr>| {
            let o = sent_offers(t, &holder);
            assert_eq!(o.len(), 1);
            (o[0].addrs.clone(), o[0].relays.clone())
        };
        assert_eq!(answer(&mut t), (vec![public], vec![]));
        // The real endpoint gives up on the dead address.
        let mut failed = false;
        while !failed {
            now = ep.poll_timeout().expect("a dial pending");
            ep.handle_timeout(now);
            while ep.poll_transmit(now).is_some() {}
            while let Some(e) = ep.poll_event() {
                failed |= matches!(e, Event::ConnectFailed { peer } if peer == holder.peer_id());
                t.handle_event(now, &mut ep, &e);
            }
        }
        assert!(t.failure(&holder.peer_id()).is_some() && ep.is_connected(&relay.peer_id()));
        let o = proved_offer(&holder, &me, Some((Proof::Invite, &k)), 2, Role::Peer, vec![dead]);
        t.handle_signal(now, &mut ep, Topic::peer(&me), &o);
        assert_eq!(answer(&mut t), (vec![public], vec![]), "a failed dial opens nothing");
        let up = Event::Connected { peer: relay.peer_id(), path: Path::Direct(lan), initiator: true };
        t.handle_event(now + OFFER_GAP, &mut ep, &up);
        assert_eq!(answer(&mut t), (vec![public], vec![]), "nor does the re-offer");
    }

    /// NB-033 N33-1: an invite holder that named R as a relay is dialled through R
    /// as soon as this node links with R, though R was not linked when the offer came.
    #[test]
    fn holder_is_dialled_via_a_relay_it_listed_once_linked() {
        let ia = id(1);
        let me = ia.peer_id();
        let lan: SocketAddr = "10.0.0.1:7000".parse().unwrap();
        let (relay, holder, other) = (id(3), id(2), id(4));
        let (at_relay, at_other): (SocketAddr, SocketAddr) =
            ("198.51.100.3:9".parse().unwrap(), "198.51.100.4:9".parse().unwrap());
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![lan], vec![]);
        let now = Instant::from_millis(10);
        let k = t.invite_key();
        let relays = vec![relay.peer_id()];
        let o: Offer<SocketAddr> = Offer {
            kind: Proof::Invite,
            reply: false,
            role: Role::Peer,
            nat: NatType::Cone,
            says_mapping: false,
            ts: 0,
            nonce: 1,
            addrs: vec![],
            predict: None,
            relays,
            relayed: Vec::new(),
        };
        let mut body = o.encode();
        body.extend(mac(&k, &[b"pw-invite", holder.peer_id().as_bytes(), me.as_bytes(), &body]));
        t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed::seal(&holder, &me, [7; 32], &body).unwrap());
        assert_eq!(dials(&mut ep, now), vec![], "nothing to dial yet");
        // A link it didn't name changes nothing.
        link(&mut ep, lan, &other, at_other, now);
        t.handle_event(
            now,
            &mut ep,
            &Event::Connected { peer: other.peer_id(), path: Path::Direct(at_other), initiator: true },
        );
        assert_eq!(dials(&mut ep, now), vec![]);
        // R links: the holder is dialled through it.
        link(&mut ep, lan, &relay, at_relay, now);
        t.handle_event(
            now,
            &mut ep,
            &Event::Connected { peer: relay.peer_id(), path: Path::Direct(at_relay), initiator: true },
        );
        assert_eq!(dials(&mut ep, now), vec![at_relay]);
    }

    /// NB-041: a relay peer is named first in offers, though it linked last and
    /// the node has more links than an offer names, and is tried first among the
    /// relays an offer names, though that offer lists it last. An offer to the
    /// relay peer itself names no links, only addresses (NB41-1).
    #[test]
    fn relay_peers_come_first_in_offers_and_dials() {
        let me = id(1).peer_id();
        let lan: SocketAddr = "10.0.0.1:7000".parse().unwrap();
        let (mut t, mut ep) = in_room(Role::Peer, None, lan);
        let now = Instant::from_millis(10);
        let at = |i: u8| -> SocketAddr { format!("198.51.100.{i}:9").parse().unwrap() };
        for i in 10..20 {
            link(&mut ep, lan, &id(i), at(i), now);
        }
        let relay = id(30);
        link(&mut ep, lan, &relay, at(30), now);
        let key = room_key(ROOM.0, ROOM.1);
        let (member, other) = (id(2), id(3));
        let ask = |t: &mut Traversal<SocketAddr>, ep: &mut Endpoint<SocketAddr>, who: &Identity, nonce: u64| {
            t.handle_signal(now, ep, Topic::peer(&me), &offer_from(who, &me, Some(&key), nonce, Role::Peer, vec![]));
            sent_offers(t, who).remove(0).relays
        };
        let before = ask(&mut t, &mut ep, &member, 1);
        assert_eq!(before.len(), MAX_RELAYS);
        assert!(!before.contains(&relay.peer_id()), "11 links, 8 named: the last one linked is left out");
        t.set_relay_peers(vec![relay.peer_id()]);
        let after = ask(&mut t, &mut ep, &other, 2);
        assert_eq!((after.len(), after[0]), (MAX_RELAYS, relay.peer_id()));

        // An offer naming two links before the relay peer: the relay peer and the first are dialled through.
        let relays = vec![id(10).peer_id(), id(11).peer_id(), relay.peer_id()];
        let o: Offer<SocketAddr> = Offer {
            kind: Proof::Room,
            reply: false,
            role: Role::Peer,
            nat: NatType::Symmetric,
            says_mapping: false,
            ts: 0,
            nonce: 3,
            addrs: vec![],
            predict: None,
            relays,
            relayed: Vec::new(),
        };
        let mut body = o.encode();
        let joiner = id(4);
        body.extend(mac(&key, &[b"pw-offer", joiner.peer_id().as_bytes(), me.as_bytes(), &body]));
        while ep.poll_transmit(now).is_some() {}
        t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed::seal(&joiner, &me, [7; 32], &body).unwrap());
        assert_eq!(dials(&mut ep, now), vec![at(30), at(10)]);

        // The relay peer itself learns none of our links, but our LAN address (a relay on the LAN).
        t.handle_signal(now, &mut ep, Topic::peer(&me), &offer_from(&relay, &me, Some(&key), 4, Role::Peer, vec![]));
        let o = sent_offers(&mut t, &relay).remove(0);
        assert_eq!((o.relays, o.addrs.contains(&lan)), (vec![], true));
    }

    /// NB-097: a policy's steps (`Direct` and `Birthday` side by side are one), the relayed links it keeps (a relay
    /// kind ranked before both direct ones), and the orders it takes.
    #[test]
    fn policy_steps_keeps_and_checks() {
        use PathKind::*;
        let bits = |ks: &[PathKind]| ks.iter().fold(0, |b, k| b | k.bit());
        let steps = |order: &[PathKind]| Policy::new(order.iter().copied()).steps();
        assert_eq!(Policy::default().steps(), vec![bits(&[Direct, Birthday]), bits(&[RelayPeers]), bits(&[Peers])]);
        assert_eq!(steps(&[Birthday, Direct]), vec![bits(&[Direct, Birthday])]);
        assert_eq!(steps(&[Direct, Peers, Birthday]), vec![bits(&[Direct]), bits(&[Peers]), bits(&[Birthday])]);
        let keeps = |order: &[PathKind]| {
            let p = Policy::new(order.iter().copied());
            (p.keeps(RelayPeers), p.keeps(Peers))
        };
        assert_eq!(keeps(&[Direct, RelayPeers, Peers]), (false, false));
        assert_eq!(keeps(&[Peers, Direct]), (false, true));
        assert_eq!(keeps(&[RelayPeers, Birthday, Peers]), (true, false));
        assert_eq!(keeps(&[RelayPeers, Peers]), (true, true));
        assert!(Policy::default().is_valid() && Policy::new([Peers]).is_valid());
        assert!(!Policy::new([]).is_valid() && !Policy::new([Direct, Peers, Direct]).is_valid());
        assert_eq!((Policy::default().port_mapping, Policy::default().fallback), (false, Duration::from_secs(2)));
    }

    /// NB-097: under a policy an offer is dialled a step at a time, each `fallback` after the last: here through a
    /// room member linked to both, through a relay peer and directly, in the policy's order. A kind left out is
    /// never dialled.
    #[test]
    fn offers_are_dialled_in_the_steps_of_the_policy() {
        use PathKind::*;
        let me = id(1).peer_id();
        let lan: SocketAddr = "10.0.0.1:7000".parse().unwrap();
        let at = |i: u8| -> SocketAddr { format!("198.51.100.{i}:9").parse().unwrap() };
        let (key, member, relay, joiner) = (room_key(ROOM.0, ROOM.1), id(30), id(31), id(4));
        let o: Offer<SocketAddr> = Offer {
            kind: Proof::Room,
            reply: false,
            role: Role::Peer,
            nat: NatType::Cone,
            says_mapping: false,
            ts: 0,
            nonce: 1,
            addrs: vec![at(4)],
            predict: None,
            relays: vec![member.peer_id(), relay.peer_id()],
            relayed: Vec::new(),
        };
        let mut body = o.encode();
        body.extend(mac(&key, &[b"pw-offer", joiner.peer_id().as_bytes(), me.as_bytes(), &body]));
        let sealed_offer = sealed::seal(&joiner, &me, [7; 32], &body).unwrap();
        for (order, want) in [
            (vec![Peers, RelayPeers, Direct], vec![vec![at(30)], vec![at(31)], vec![at(4)]]),
            (vec![Direct, Birthday, Peers], vec![vec![at(4)], vec![at(30)], vec![]]),
            (vec![RelayPeers], vec![vec![at(31)], vec![], vec![]]),
        ] {
            let (mut t, mut ep) = in_room(Role::Peer, None, lan);
            let now = Instant::from_millis(10);
            link(&mut ep, lan, &member, at(30), now);
            link(&mut ep, lan, &relay, at(31), now);
            t.set_relay_peers(vec![relay.peer_id()]);
            t.set_policy(now, &mut ep, Some(Policy::new(order.clone())));
            while ep.poll_transmit(now).is_some() {}
            t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed_offer);
            let mut got = vec![dials(&mut ep, now)];
            for k in 1..3 {
                let (before, later) = (now + Duration::from_millis(2000 * k - 1), now + Duration::from_secs(2 * k));
                t.handle_timeout(before, &mut ep);
                assert_eq!(dials(&mut ep, later), Vec::<SocketAddr>::new(), "{order:?}: not before `fallback`");
                t.handle_timeout(later, &mut ep);
                got.push(dials(&mut ep, later));
            }
            assert_eq!(got, want, "{order:?}");
        }
    }

    /// NB-097: for a new carrier, `resubscribe` subscribes the inbox and the room's topic again and says hello in
    /// the room at once; hellos then repeat from 2 s again.
    #[test]
    fn resubscribe_subscribes_again_and_says_hello() {
        let me = id(1).peer_id();
        let (mut t, _) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        let (now, topic) = (Instant::from_millis(90_000), Topic::room(ROOM.0, ROOM.1));
        t.resubscribe(now);
        let signals: Vec<Signal> = std::iter::from_fn(|| t.poll_signal()).collect();
        assert_eq!(signals[..2], [Signal::Subscribe(Topic::peer(&me)), Signal::Subscribe(topic)]);
        assert!(matches!(&signals[2..], [Signal::Publish(to, _)] if *to == topic), "{signals:?}");
        assert_eq!(t.poll_timeout(), Some(now + HELLO_FIRST));
    }

    /// NB-042: an offer's addresses are dialled only in the families the socket
    /// can send to (both on a dual-stack one), all at once.
    #[test]
    fn offers_are_dialled_in_the_families_the_socket_reaches() {
        let at = |s: &str| -> SocketAddr { s.parse().unwrap() };
        let (v4, v6) = (at("198.51.100.9:4000"), at("[2001:db8::9]:4000"));
        let (me, key, now) = (id(1).peer_id(), room_key(ROOM.0, ROOM.1), Instant::from_millis(10));
        let cases = [
            (vec![at("10.0.0.1:7000")], vec![v4]),
            (vec![at("[2001:db8::1]:7000")], vec![v6]),
            (vec![at("10.0.0.1:7000"), at("[2001:db8::1]:7000"), at("127.0.0.1:7000")], vec![v6, v4]),
        ];
        for (local, want) in cases {
            let mut t = Traversal::new(&id(1), [3; 32], 0, local.clone(), vec![]);
            t.join_room(Instant::ZERO, ROOM.0, ROOM.1, Role::Peer, None);
            let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
            let offer = offer_from(&id(2), &me, Some(&key), 1, Role::Peer, vec![v6, v4]);
            t.handle_signal(now, &mut ep, Topic::peer(&me), &offer);
            assert_eq!(dials(&mut ep, now), want, "bound to {local:?}");
            assert!(local.iter().all(|l| t.reaches(l)));
        }
    }

    /// The offers `t` published, opened with `to`'s key and decoded.
    fn sent_offers(t: &mut Traversal<SocketAddr>, to: &Identity) -> Vec<Offer<SocketAddr>> {
        std::iter::from_fn(|| t.poll_signal())
            .filter_map(|s| match s {
                Signal::Publish(topic, p) if topic == Topic::peer(&to.peer_id()) => {
                    let (_, body) = sealed::open(to, &p)?;
                    let cut = if body[0] == OFFER { body.len() } else { body.len() - PROOF_LEN };
                    Offer::decode(&body[..cut])
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn public_addresses() {
        let public = ["198.51.100.7:9", "8.8.8.8:53", "[2001:db8::1]:9", "100.128.0.1:1", "172.32.0.1:1"];
        let local = [
            "10.0.0.1:1",
            "192.168.1.2:1",
            "172.16.0.1:1",
            "127.0.0.1:1",
            "169.254.1.1:1",
            "100.64.0.1:1",
            "100.127.255.1:1",
            "0.0.0.0:1",
            "255.255.255.255:1",
            "224.0.0.1:1",
            "[::1]:1",
            "[fe80::1]:1",
            "[fd00::1]:1",
            "[::ffff:10.0.0.1]:1",
        ];
        for a in public {
            assert!(is_public(&a.parse::<SocketAddr>().unwrap()), "{a}");
        }
        for a in local {
            assert!(!is_public(&a.parse::<SocketAddr>().unwrap()), "{a}");
        }
    }

    /// NB-034: what each mode puts in an offer, read back by its recipient.
    #[test]
    fn privacy_decides_what_offers_carry() {
        let (ia, peer, relay) = (id(1), id(2), id(3));
        let lan: SocketAddr = "10.0.0.1:7000".parse().unwrap();
        let lo: SocketAddr = "127.0.0.1:7000".parse().unwrap();
        let public: SocketAddr = "198.51.100.1:7000".parse().unwrap();
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![lan, lo], vec![]);
        t.public = Some(public);
        t.handle_event(
            Instant::ZERO,
            &mut ep,
            &Event::AddrObserved { by: relay.peer_id(), addr: "192.168.0.9:1".parse().unwrap() },
        );
        let now = Instant::from_millis(10);
        let offer_to = |t: &mut Traversal<SocketAddr>, ep: &mut Endpoint<SocketAddr>, to: &Identity| {
            while t.poll_signal().is_some() {}
            t.introduce(now, ep, to.peer_id(), Some([5; 32]));
            let o = sent_offers(t, to);
            assert_eq!(o.len(), 1);
            assert_eq!(o[0].kind, Proof::Invite, "proved with the invite key");
            (o[0].addrs.clone(), o[0].relays.clone())
        };
        assert_eq!(offer_to(&mut t, &mut ep, &peer).0, vec![public, "192.168.0.9:1".parse().unwrap(), lan, lo]);
        t.set_privacy(&mut ep, Privacy::PublicOnly);
        assert_eq!(offer_to(&mut t, &mut ep, &peer).0, vec![public]);
        assert_eq!(t.candidates(), vec![public]);
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: Some(relay.peer_id()) });
        assert!(ep.relay_only());
        assert_eq!(offer_to(&mut t, &mut ep, &peer), (vec![], vec![]), "nothing, not even the relay while unlinked");
        assert_eq!(offer_to(&mut t, &mut ep, &relay).0, vec![public], "the relay gets the public address");
        assert_eq!(t.candidates(), vec![]);
        t.set_privacy(&mut ep, Privacy::Direct);
        assert!(!ep.relay_only());
    }

    /// A relay-only node dials others only through its relay, whatever their
    /// offers carry, and in a host/client room the host is the relay.
    #[test]
    fn relay_only_dials_through_the_relay() {
        let me = id(1).peer_id();
        let there: SocketAddr = "198.51.100.9:4000".parse().unwrap();
        let (key, inbox) = (room_key(ROOM.0, ROOM.1), Topic::peer(&me));
        let (mut t, mut ep) = in_room(Role::Client, None, "10.0.0.1:7000".parse().unwrap());
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: None });
        assert_eq!(t.relay(), None);
        let now = Instant::from_millis(10);
        // The host proves its role: it is the relay, and dialled directly.
        let host = id(2);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&host, &me, Some(&key), 1, Role::Host, vec![there]));
        assert_eq!(t.relay(), Some(host.peer_id()));
        assert_eq!(dials(&mut ep, now), vec![there]);
        // Pinning a host makes it the relay before it says anything.
        let (mut t, mut ep) = in_room(Role::Client, Some(id(4).peer_id()), "10.0.0.1:7000".parse().unwrap());
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: None });
        assert_eq!(t.relay(), Some(id(4).peer_id()));
        // Anyone else is never dialled at its addresses (the relay isn't linked: no dial at all).
        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: Some(id(3).peer_id()) });
        t.handle_signal(now, &mut ep, inbox, &offer_from(&id(5), &me, Some(&key), 1, Role::Peer, vec![there]));
        assert_eq!(dials(&mut ep, now), vec![]);
        assert_eq!(published(&mut t), 1, "answered, with no address");
    }

    /// NB-034 R1: a relay-only client's relay is the first Host it dials, and stays
    /// so: a second Host (here with the lower PeerId, which used to win) is never
    /// dialled directly, until the first's dial fails.
    #[test]
    fn relay_only_client_dials_exactly_one_host_directly() {
        let me = id(1).peer_id();
        let (key, inbox) = (room_key(ROOM.0, ROOM.1), Topic::peer(&me));
        let (first_at, second_at): (SocketAddr, SocketAddr) =
            ("198.51.100.9:4000".parse().unwrap(), "198.51.100.10:4000".parse().unwrap());
        let (mut first, mut second) = (id(2), id(3));
        if first.peer_id() < second.peer_id() {
            std::mem::swap(&mut first, &mut second);
        }
        let (mut t, mut ep) = in_room(Role::Client, None, "10.0.0.1:7000".parse().unwrap());
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: None });
        let now = Instant::from_millis(10);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&first, &me, Some(&key), 1, Role::Host, vec![first_at]));
        assert_eq!(t.relay(), Some(first.peer_id()));
        assert_eq!(dials(&mut ep, now), vec![first_at]);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&second, &me, Some(&key), 1, Role::Host, vec![second_at]));
        assert_eq!(t.relay(), Some(first.peer_id()), "the relay doesn't switch");
        assert_eq!(dials(&mut ep, now), vec![], "the second Host is not dialled at its address");
        assert!(sent_offers(&mut t, &second).iter().all(|o| o.addrs.is_empty() && o.relays.is_empty()));
        // Once the first dial fails, the next Host may be picked.
        t.handle_event(now, &mut ep, &Event::ConnectFailed { peer: first.peer_id() });
        assert_eq!(t.relay(), None);
        let later = now + SENDER_REFILL;
        t.handle_signal(later, &mut ep, inbox, &offer_from(&second, &me, Some(&key), 2, Role::Host, vec![second_at]));
        assert_eq!(t.relay(), Some(second.peer_id()));
        assert_eq!(dials(&mut ep, later), vec![second_at]);
        // A pinned `via` overrides any pick.
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: Some(first.peer_id()) });
        assert_eq!(t.relay(), Some(first.peer_id()));
    }

    /// A Host offer with no address to dial isn't picked as the relay: nothing
    /// would ever clear the pick, locking the real Host out.
    #[test]
    fn relay_only_client_skips_a_host_it_cannot_dial() {
        let me = id(1).peer_id();
        let (key, inbox) = (room_key(ROOM.0, ROOM.1), Topic::peer(&me));
        let real_at: SocketAddr = "198.51.100.10:4000".parse().unwrap();
        let (empty, real) = (id(2), id(3));
        let (mut t, mut ep) = in_room(Role::Client, None, "10.0.0.1:7000".parse().unwrap());
        t.set_privacy(&mut ep, Privacy::RelayOnly { via: None });
        let now = Instant::from_millis(10);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&empty, &me, Some(&key), 1, Role::Host, vec![]));
        assert_eq!(t.relay(), None);
        assert_eq!(dials(&mut ep, now), vec![]);
        t.handle_signal(now, &mut ep, inbox, &offer_from(&real, &me, Some(&key), 1, Role::Host, vec![real_at]));
        assert_eq!(t.relay(), Some(real.peer_id()));
        assert_eq!(dials(&mut ep, now), vec![real_at]);
    }

    /// NB-032: a stale or replayed-later hello is dropped.
    #[test]
    fn stale_hellos_are_dropped() {
        let (key, topic) = (room_key(ROOM.0, ROOM.1), Topic::room(ROOM.0, ROOM.1));
        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        let late = Instant::ZERO + Duration::from_millis(FRESH_MS + 1);
        t.handle_signal(late, &mut ep, topic, &room_hello(&key, &id(5).peer_id(), Role::Peer, 0));
        assert_eq!(published(&mut t), 0);
        let ts = FRESH_MS + 1;
        t.handle_signal(late, &mut ep, topic, &room_hello(&key, &id(5).peer_id(), Role::Peer, ts));
        assert_eq!(published(&mut t), 1);
    }

    /// NB-032: outside a room, a plain offer from a peer this node never dialled
    /// is ignored (knowing a PeerId gets no dial-back and no addresses), and peers
    /// it dialled are answered even when strangers spent the node's budget.
    #[test]
    fn only_invite_holders_and_dialled_peers_get_answers() {
        let ia = id(1);
        let me = ia.peer_id();
        let there: SocketAddr = "198.51.100.9:4000".parse().unwrap();
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut t = Traversal::new(&ia, [3; 32], 0, vec!["10.0.0.1:7000".parse().unwrap()], vec![]);
        while t.poll_signal().is_some() {}
        let now = Instant::from_millis(10);
        let inbox = Topic::peer(&me);
        t.handle_signal(now, &mut ep, inbox, &proved_offer(&id(2), &me, None, 1, Role::Peer, vec![there]));
        let wrong = proved_offer(&id(2), &me, Some((Proof::Invite, &[0; 32])), 2, Role::Peer, vec![there]);
        t.handle_signal(now, &mut ep, inbox, &wrong);
        // Nor does a room offer outside a room.
        let room =
            proved_offer(&id(2), &me, Some((Proof::Room, &room_key(ROOM.0, ROOM.1))), 3, Role::Peer, vec![there]);
        t.handle_signal(now, &mut ep, inbox, &room);
        assert_eq!((dials(&mut ep, now), published(&mut t)), (vec![], 0));
        // Invite holders spend the node's whole offer budget...
        let k = t.invite_key();
        for n in 0..BURST as u8 + 5 {
            t.handle_signal(
                now,
                &mut ep,
                inbox,
                &proved_offer(&id(100 + n), &me, Some((Proof::Invite, &k)), 1, Role::Peer, vec![]),
            );
        }
        assert_eq!(published(&mut t), BURST as usize);
        // ...but a peer this node dialled is still answered (dialled, no reply asked twice).
        t.introduce(now, &ep, id(9).peer_id(), None);
        assert_eq!(published(&mut t), 1);
        let answer = proved_offer(&id(9), &me, None, 1, Role::Peer, vec![there]);
        t.handle_signal(now, &mut ep, inbox, &answer);
        assert_eq!(dials(&mut ep, now), vec![there]);
    }

    #[test]
    fn roles() {
        use Role::*;
        assert!(allowed(None, Client) && allowed(Some(Peer), Peer) && allowed(Some(Client), Host));
        assert!(allowed(Some(Host), Client) && allowed(Some(Host), Peer) && allowed(Some(Peer), Host));
        assert!(!allowed(Some(Client), Client) && !allowed(Some(Client), Peer) && !allowed(Some(Host), Host));
        assert!(!allowed(Some(Peer), Client));
    }

    /// Two traversals wired to each other by hand: offers are sealed, checked for
    /// freshness and replay, and turn into dials.
    #[test]
    fn offers_dial_and_replays_are_dropped() {
        let (ia, ib) = (id(1), id(2));
        let a_addr: SocketAddr = "198.51.100.1:5000".parse().unwrap();
        let b_addr: SocketAddr = "198.51.100.2:6000".parse().unwrap();
        let mut ea = Endpoint::new(&ia, Config::default(), [1; 32]);
        let mut eb = Endpoint::new(&ib, Config::default(), [2; 32]);
        let base = 1_800_000_000_000;
        let mut ta = Traversal::new(&ia, [3; 32], base, vec![a_addr], vec![]);
        let mut tb = Traversal::new(&ib, [4; 32], base, vec![b_addr], vec![]);
        let now = Instant::from_millis(10);
        assert_eq!(ta.poll_signal(), Some(Signal::Subscribe(Topic::peer(&ia.peer_id()))));
        ta.introduce(now, &ea, ib.peer_id(), Some(tb.invite_key()));
        let Some(Signal::Publish(topic, sealed)) = ta.poll_signal() else { panic!() };
        assert_eq!(topic, Topic::peer(&ib.peer_id()));
        tb.handle_signal(now, &mut eb, topic, &sealed);
        let dial = eb.poll_transmit(now).expect("B dials A's candidate");
        assert_eq!(dial.dst, a_addr);
        tb.poll_signal(); // B's subscribe
        let Some(Signal::Publish(back, answer)) = tb.poll_signal() else { panic!("B answers") };
        assert_eq!(back, Topic::peer(&ia.peer_id()));
        ta.handle_signal(now, &mut ea, back, &answer);
        assert_eq!(ea.poll_transmit(now).map(|t| t.dst), Some(b_addr), "A dials B on the answer");
        assert_eq!(ta.poll_signal(), None, "an answer is not answered");

        // The same sealed offer again: dropped.
        let mut eb2 = Endpoint::new(&ib, Config::default(), [5; 32]);
        tb.handle_signal(now, &mut eb2, topic, &sealed);
        assert!(eb2.poll_transmit(now).is_none());
        // A stale one: dropped.
        let mut tb2 = Traversal::new(&ib, [6; 32], base + 2 * FRESH_MS, vec![b_addr], vec![]);
        tb2.handle_signal(now, &mut eb2, topic, &sealed);
        assert!(eb2.poll_transmit(now).is_none());
    }

    #[test]
    fn stun_classifies_the_nat() {
        let ia = id(1);
        let local: SocketAddr = "10.0.0.2:7000".parse().unwrap();
        let s: [SocketAddr; 4] =
            ["192.0.2.1:3478", "192.0.2.2:3478", "192.0.2.1:3479", "192.0.2.2:3479"].map(|a| a.parse().unwrap());
        let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
        // Each server answers with the port it saw this node at (`None`: no answer).
        let ask = |servers: &[SocketAddr], ports: &[Option<u16>], mapped_ip: [u8; 4]| {
            let mut t = Traversal::new(&ia, [3; 32], 0, vec![local], servers.to_vec());
            let mut ep = Endpoint::new(&ia, Config::default(), [1; 32]);
            assert_eq!(t.poll_timeout(), Some(Instant::ZERO));
            t.handle_timeout(Instant::ZERO, &mut ep);
            let reqs: Vec<_> = std::iter::from_fn(|| t.poll_transmit()).collect();
            assert_eq!(reqs.iter().map(|r| r.dst).collect::<Vec<_>>(), servers);
            for (r, port) in reqs.iter().zip(ports).filter_map(|(r, p)| Some((r, (*p)?))) {
                let tx = stun::parse_binding_request(&r.payload).unwrap();
                let resp = stun::binding_response(&tx, SocketAddr::new(mapped_ip.into(), port));
                assert!(!t.handle_stun(&"192.0.2.99:3478".parse().unwrap(), b"not stun"));
                assert!(t.handle_stun(&r.dst, &resp));
            }
            (t.nat_type(), t.public_addr().copied(), t.predicted_addr())
        };
        let run = |ports: [u16; 2]| ask(&s[..2], &ports.map(Some), [203, 0, 113, 9]);
        assert_eq!(run([4000, 4000]), (NatType::Cone, Some("203.0.113.9:4000".parse().unwrap()), None));
        // NB-035: mappings a small step apart, upwards, predict the next one.
        let at = |port: u16| Some(SocketAddr::new([203, 0, 113, 9].into(), port));
        assert_eq!(run([4000, 4001]), (NatType::Symmetric, at(4000), at(4002)));
        assert_eq!(run([4000, 4008]).2, at(4016));
        for random in [[4000, 4009], [4001, 4000], [4000, 51234], [65534, 65535]] {
            assert_eq!(run(random), (NatType::Symmetric, at(random[0]), None), "{random:?}");
        }
        // NB-083: the first server on a second port too, as `DEFAULT_STUN_SERVERS` lists it.
        use NatType::{Cone, Open, Symmetric, SymmetricIp, Unknown};
        let three = |ports: [Option<u16>; 3]| ask(&s[..3], &ports, [203, 0, 113, 9]);
        let (a, b, c) = (Some(4000), Some(51234), Some(30001));
        assert_eq!(three([a, a, a]), (Cone, at(4000), None));
        // One mapping for both ports of one IP, another for the other IP: one port per IP.
        assert_eq!(three([a, b, a]), (SymmetricIp, at(4000), None));
        assert_eq!(three([a, Some(4001), a]), (SymmetricIp, at(4000), at(4002)));
        // Every destination its own: per IP and port, and a sequence counts the third port.
        assert_eq!(three([a, b, c]), (Symmetric, at(4000), None));
        assert_eq!(three([a, Some(4001), Some(4002)]), (Symmetric, at(4000), at(4003)));
        assert_eq!(three([a, Some(4001), Some(4005)]), (Symmetric, at(4000), at(4006)));
        assert_eq!(three([a, Some(4001), Some(4010)]).2, None, "steps of 1, then 9: not a sequence");
        // Two IPs agreeing but not one IP's two ports: still a new port per destination.
        assert_eq!(three([a, a, c]).0, Symmetric);
        // The other IP silent: one IP's ports agreeing says nothing of mapping per IP.
        assert_eq!(three([a, None, a]), (Unknown, at(4000), None));
        assert_eq!(three([a, None, c]).0, Symmetric);
        // The second port silent: as with two servers.
        assert_eq!(three([a, a, None]).0, Cone);
        assert_eq!(three([a, Some(4001), None]), (Symmetric, at(4000), at(4002)));
        // Two ports at each of two IPs: the same rules.
        assert_eq!(ask(&s, &[a, b, a, b], [203, 0, 113, 9]).0, SymmetricIp);
        assert_eq!(ask(&s, &[a, b, a, c], [203, 0, 113, 9]).0, Symmetric);
        // A list at one IP only, as hosts' tests on loopback use, is judged as before NB-083.
        let one_ip = |ports: [u16; 2]| ask(&[s[0], s[2]], &ports.map(Some), [203, 0, 113, 9]);
        assert_eq!(one_ip([4000, 4000]).0, Cone);
        assert_eq!(one_ip([4000, 4001]), (Symmetric, at(4000), at(4002)));
        // The socket's own address: no NAT.
        assert_eq!(ask(&s[..3], &[Some(7000); 3], [10, 0, 0, 2]).0, Open);
        // Unanswered: retransmitted twice, then given up until the next round.
        let mut t = Traversal::new(&ia, [3; 32], 0, vec![local], vec![s[0]]);
        let mut sent = 0;
        while let Some(at) = t.poll_timeout().filter(|t| *t < Instant::from_millis(20_000)) {
            t.handle_timeout(at, &mut ep);
            sent += std::iter::from_fn(|| t.poll_transmit()).count();
        }
        assert_eq!((sent, t.nat_type()), (3, NatType::Unknown));
        assert_eq!(t.poll_timeout(), Some(Instant::ZERO + STUN_INTERVAL));
    }
    /// NB-035: the prediction rides in the address list as an entry no address
    /// decoder reads, so offers stay readable by nodes that predate it, and a
    /// full list makes room for it.
    #[test]
    fn predictions_round_trip_in_offers() {
        let p = Prediction { next: 20_002, step: 1 };
        let entry = p.encode();
        assert_eq!(Prediction::decode(&entry), Some(p));
        assert_eq!(SocketAddr::decode(&entry), None);
        for bad in [[0xfe, 1, 0, 1, 2], [0xfe, 1, 9, 1, 2], [0xfe, 2, 1, 1, 2]] {
            assert_eq!(Prediction::decode(&bad), None);
        }
        let addrs: Vec<SocketAddr> = (0..8).map(|i| SocketAddr::from(([198, 51, 100, 7], 4000 + i))).collect();
        let o = Offer {
            kind: Proof::None,
            reply: false,
            role: Role::Peer,
            nat: NatType::Symmetric,
            says_mapping: false,
            ts: 5,
            nonce: 6,
            addrs: addrs.clone(),
            predict: Some(p),
            relays: vec![],
            relayed: Vec::new(),
        };
        let b = o.encode();
        let got = Offer::<SocketAddr>::decode(&b).unwrap();
        assert_eq!((got.addrs, got.predict), (addrs[..7].to_vec(), Some(p)));
        assert_eq!(b[20], 8, "eight entries: the most an older decoder takes");
        // The window: the predicted port's IP, leaning forward.
        let w = p.window(&addrs[0]);
        assert_eq!(w.len(), 32);
        assert_eq!((w[0].port(), w[4].port(), w[31].port()), (19_998, 20_002, 20_029));
        assert!(w.iter().all(|a| a.ip() == addrs[0].ip()));
    }

    /// `Offer::encode` as main had it before NB-083 (6cf946c), but for the field it lacked:
    /// what a node that predates `SymmetricIp` sends.
    fn encode_before_nb083(o: &Offer<SocketAddr>) -> Vec<u8> {
        let role = [Role::Peer, Role::Host, Role::Client].iter().position(|r| *r == o.role).unwrap() as u8;
        let nat = [NatType::Unknown, NatType::Open, NatType::Cone, NatType::Symmetric].iter().position(|n| *n == o.nat);
        let version = match o.kind {
            Proof::None => OFFER,
            Proof::Room => ROOM_OFFER,
            Proof::Invite => INVITE_OFFER,
        };
        let mut v = vec![version, o.reply as u8, role, nat.unwrap() as u8];
        v.extend_from_slice(&o.ts.to_le_bytes());
        v.extend_from_slice(&o.nonce.to_le_bytes());
        let room = MAX_CANDIDATES - o.predict.is_some() as usize;
        let n = o.addrs.len().min(room);
        v.push((n + o.predict.is_some() as usize) as u8);
        for a in o.addrs.iter().take(n) {
            let mut e = Vec::new();
            a.encode(&mut e);
            v.push(e.len() as u8);
            v.extend_from_slice(&e);
        }
        if let Some(p) = o.predict {
            v.push(5);
            v.extend_from_slice(&p.encode());
        }
        v.push(o.relays.len().min(MAX_RELAYS) as u8);
        for p in o.relays.iter().take(MAX_RELAYS) {
            v.extend_from_slice(p.as_bytes());
        }
        v
    }

    /// `Offer::decode` as main had it before NB-083 (6cf946c): what such a node reads.
    fn decode_before_nb083(b: &[u8]) -> Option<Offer<SocketAddr>> {
        let (&[version @ (OFFER | ROOM_OFFER | INVITE_OFFER), flags, role, nat], rest) = b.split_first_chunk::<4>()?
        else {
            return None;
        };
        let role = *[Role::Peer, Role::Host, Role::Client].get(role as usize)?;
        let nat = *[NatType::Unknown, NatType::Open, NatType::Cone, NatType::Symmetric].get(nat as usize)?;
        let (ts, rest) = rest.split_first_chunk::<8>()?;
        let (nonce, rest) = rest.split_first_chunk::<8>()?;
        let (&n, mut rest) = rest.split_first()?;
        if n as usize > MAX_CANDIDATES || flags > 1 {
            return None;
        }
        let (mut addrs, mut predict) = (Vec::new(), None);
        for _ in 0..n {
            let (&len, r) = rest.split_first()?;
            let (a, r) = (r.get(..len as usize)?, r.get(len as usize..)?);
            match a.starts_with(&PREDICTION_TAG) {
                true => predict = predict.or(Prediction::decode(a)),
                false => addrs.extend(SocketAddr::decode(a)),
            }
            rest = r;
        }
        let (&m, mut rest) = rest.split_first()?;
        if m as usize > MAX_RELAYS || rest.len() != m as usize * 32 {
            return None;
        }
        let mut relays = Vec::new();
        while let Some((p, r)) = rest.split_first_chunk::<32>() {
            relays.push(PeerId(*p));
            rest = r;
        }
        Some(Offer {
            kind: [Proof::None, Proof::Room, Proof::Invite][version as usize - 1],
            reply: flags == 1,
            role,
            nat,
            says_mapping: false,
            ts: u64::from_le_bytes(*ts),
            nonce: u64::from_le_bytes(*nonce),
            addrs,
            predict,
            relays,
            relayed: Vec::new(),
        })
    }

    /// NB-083: how a symmetric NAT maps rides in the address list as an entry no address
    /// decoder reads, as the prediction does, and the NAT byte stays `Symmetric`. A node
    /// that predates it reads a `SymmetricIp` offer as `Symmetric`, with the same addresses,
    /// prediction and relays; its own offers read as `Symmetric` from a peer that doesn't
    /// say how it maps; and every offer without the entry is what it sent, byte for byte.
    #[test]
    fn offers_say_how_a_symmetric_nat_maps_and_older_nodes_read_them() {
        use NatType::{Cone, Open, Symmetric, SymmetricIp, Unknown};
        let addrs: Vec<SocketAddr> = vec!["203.0.113.5:4000".parse().unwrap(), "[2001:db8::1]:9".parse().unwrap()];
        let offer = |nat, says_mapping, predict| Offer {
            kind: Proof::Room,
            reply: true,
            role: Role::Peer,
            nat,
            says_mapping,
            ts: 1_700_000_000_123,
            nonce: 42,
            addrs: addrs.clone(),
            predict,
            relays: vec![id(2).peer_id()],
            relayed: Vec::new(),
        };
        for predict in [None, Some(Prediction { next: 20_002, step: 1 })] {
            for (nat, entry) in [(SymmetricIp, 1), (Symmetric, 0)] {
                let o = offer(nat, true, predict);
                let b = o.encode();
                assert_eq!(b[3], 3, "the NAT byte says Symmetric");
                assert!(b.windows(4).any(|w| w == [3, 0xfe, 2, entry]), "{nat:?}");
                assert_eq!(Offer::decode(&b).as_ref(), Some(&o));
                let old = decode_before_nb083(&b).expect("an older node reads it");
                assert_eq!(old, Offer { nat: Symmetric, says_mapping: false, ..offer(nat, true, predict) });
            }
            // An older node's offer, and offers from NATs that aren't symmetric: no entry.
            for nat in [Unknown, Open, Cone, Symmetric] {
                let o = offer(nat, false, predict);
                let b = encode_before_nb083(&o);
                assert_eq!(o.encode(), b, "{nat:?}");
                assert_eq!(Offer::decode(&b).as_ref(), Some(&o));
                assert_eq!(offer(nat, nat != Symmetric, predict).encode(), b, "only symmetric NATs say how they map");
            }
        }
        // The entry counts only from a symmetric NAT, and only as `fe 02` and one byte 0 or 1;
        // anything else is skipped, as by older nodes. The first one counts.
        let with = |nat: u8, entries: &[&[u8]]| {
            let mut b = offer(Cone, false, None).encode();
            b[3] = nat;
            for e in entries {
                b[20] += 1;
                let rest = b.split_off(21);
                b.push(e.len() as u8);
                b.extend_from_slice(e);
                b.extend(rest);
            }
            let (new, old) = (Offer::<SocketAddr>::decode(&b).unwrap(), decode_before_nb083(&b).unwrap());
            assert_eq!((&new.addrs, &old.addrs), (&addrs, &addrs));
            (new.nat, new.says_mapping, old.nat)
        };
        assert_eq!(with(2, &[&[0xfe, 2, 1]]), (Cone, false, Cone));
        assert_eq!(with(3, &[&[0xfe, 2, 1]]), (SymmetricIp, true, Symmetric));
        for bad in [&[0xfe, 2, 2][..], &[0xfe, 2], &[0xfe, 2, 1, 0], &[0xfe, 3, 1]] {
            assert_eq!(with(3, &[bad]), (Symmetric, false, Symmetric), "{bad:?}");
        }
        assert_eq!(with(3, &[&[0xfe, 2, 1], &[0xfe, 2, 0]]), (Symmetric, true, Symmetric), "the first: inserted last");
        // A full list makes room for both entries: eight in all, the most an older decoder takes.
        let full: Vec<SocketAddr> = (0..8).map(|i| SocketAddr::from(([198, 51, 100, 7], 4000 + i))).collect();
        let p = Some(Prediction { next: 20_002, step: 1 });
        let b = Offer { addrs: full.clone(), ..offer(SymmetricIp, true, p) }.encode();
        assert_eq!(b[20], 8);
        let got = Offer::<SocketAddr>::decode(&b).unwrap();
        assert_eq!((got.nat, got.addrs, got.predict), (SymmetricIp, full[..6].to_vec(), p));
        assert_eq!(decode_before_nb083(&b).unwrap().addrs, full[..6].to_vec());
    }

    /// NB-035: a cone node probes the ports around a symmetric peer's
    /// prediction; an open or symmetric one doesn't.
    #[test]
    fn cone_nodes_probe_predicted_ports() {
        let me = id(1).peer_id();
        let there: SocketAddr = "198.51.100.9:20000".parse().unwrap();
        let (s1, s2): (SocketAddr, SocketAddr) = ("192.0.2.1:3478".parse().unwrap(), "192.0.2.2:3478".parse().unwrap());
        for (ports, probes) in [([5000, 5000], true), ([5000, 5000 + 1], false), ([7000, 7000], false)] {
            let local: SocketAddr = "10.0.0.2:7000".parse().unwrap();
            let mut t = Traversal::new(&id(1), [3; 32], 0, vec![local], vec![s1, s2]);
            let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
            t.handle_timeout(Instant::ZERO, &mut ep);
            let mapped = if ports[0] == 7000 { [10, 0, 0, 2] } else { [203, 0, 113, 9] };
            for (r, port) in std::iter::from_fn(|| t.poll_transmit()).collect::<Vec<_>>().iter().zip(ports) {
                let tx = stun::parse_binding_request(&r.payload).unwrap();
                t.handle_stun(&r.dst, &stun::binding_response(&tx, SocketAddr::new(mapped.into(), port)));
            }
            let peer = id(2);
            t.introduce(Instant::ZERO, &ep, peer.peer_id(), None);
            let o = Offer {
                kind: Proof::None,
                reply: false,
                role: Role::Peer,
                nat: NatType::Symmetric,
                says_mapping: false,
                ts: 0,
                nonce: 1,
                addrs: vec![there],
                predict: Some(Prediction { next: 20_002, step: 1 }),
                relays: vec![],
                relayed: Vec::new(),
            };
            let sealed = sealed::seal(&peer, &me, [7; 32], &o.encode()).unwrap();
            let now = Instant::from_millis(10);
            t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed);
            let sent = dials(&mut ep, now);
            let nat = t.nat_type();
            assert_eq!(sent.len(), if probes { 32 } else { 1 }, "{nat:?}");
            assert_eq!(sent[0], there);
            if probes {
                // Not the advertised port again; the rest of the window.
                assert!(sent[1..].iter().all(|a| a.ip() == there.ip() && a.port() != 20_000));
                assert!(sent.contains(&"198.51.100.9:20002".parse().unwrap()));
            }
        }
    }

    /// NB-035 security review: repeating an offer adds no probes (S1), guesses go
    /// out on three handshake sends only (S3), and never around a private address (S2).
    #[test]
    fn guesses_are_probed_three_times_and_only_at_public_addresses() {
        let me = id(1).peer_id();
        let (s1, s2): (SocketAddr, SocketAddr) = ("192.0.2.1:3478".parse().unwrap(), "192.0.2.2:3478".parse().unwrap());
        // Every datagram the endpoint sends to `there`'s IP over 40 s, after `offers` copies of one offer.
        let sent_to = |there: SocketAddr, offers: u64| -> usize {
            let local: SocketAddr = "10.0.0.2:7000".parse().unwrap();
            let mut t = Traversal::new(&id(1), [3; 32], 0, vec![local], vec![s1, s2]);
            let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
            t.handle_timeout(Instant::ZERO, &mut ep);
            for r in std::iter::from_fn(|| t.poll_transmit()).collect::<Vec<_>>() {
                let tx = stun::parse_binding_request(&r.payload).unwrap();
                t.handle_stun(&r.dst, &stun::binding_response(&tx, "203.0.113.9:5000".parse().unwrap()));
            }
            assert_eq!(t.nat_type(), NatType::Cone);
            let peer = id(2);
            t.introduce(Instant::ZERO, &ep, peer.peer_id(), None);
            let mut sent = Vec::new();
            for nonce in 1..=offers {
                let o = Offer {
                    kind: Proof::None,
                    reply: false,
                    role: Role::Peer,
                    nat: NatType::Symmetric,
                    says_mapping: false,
                    ts: 0,
                    nonce,
                    addrs: vec![there],
                    predict: Some(Prediction { next: there.port() + 2, step: 1 }),
                    relays: vec![],
                    relayed: Vec::new(),
                };
                let sealed = sealed::seal(&peer, &me, [7; 32], &o.encode()).unwrap();
                let now = Instant::from_millis(10 * nonce);
                t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed);
                sent.extend(dials(&mut ep, now));
            }
            for ms in (100..40_000).step_by(100) {
                ep.handle_timeout(Instant::from_millis(ms));
                sent.extend(dials(&mut ep, Instant::from_millis(ms)));
            }
            sent.iter().filter(|a| a.ip() == there.ip()).count()
        };
        let public: SocketAddr = "198.51.100.9:20000".parse().unwrap();
        let attempts = usize::from(Config::default().handshake_attempts);
        // The candidate on every handshake send, the 31 other guesses on three.
        assert_eq!(sent_to(public, 1), attempts + 31 * 3);
        assert_eq!(sent_to(public, 4), attempts + 31 * 3);
        // A private first address gets no guesses: only the candidate itself.
        assert_eq!(sent_to("192.168.1.50:20000".parse().unwrap(), 4), attempts);
    }

    /// The ports two STUN servers see this node at: a cone, a symmetric NAT with
    /// random ports, and a sequential one with step 2 (next: 5004).
    const CONE: [u16; 2] = [5000, 5000];
    const SYM: [u16; 2] = [5000, 41000];
    const SEQ: [u16; 2] = [5000, 5002];
    /// With the first server on a second port too (NB-083): one port per destination IP,
    /// random or in sequence (next: 5004), and one per IP and port.
    const SYM_IP: [u16; 3] = [5000, 41000, 5000];
    const SEQ_IP: [u16; 3] = [5000, 5002, 5000];
    const SYM_3: [u16; 3] = [5000, 41000, 23000];

    /// What a node's offers from `peers` peers (each sent at every time in
    /// `offers_ms`) start: the extra sockets it asks for, every datagram its
    /// endpoint sends to the offered IP over 6 s, and the first of them. `mine`:
    /// what STUN sees of this node's NAT ([`CONE`], [`SYM`], [`SEQ`], ...); `their`:
    /// the offers' NAT type; `predict`: theirs is sequential with step 1.
    fn birthday_case(
        mine: &[u16],
        their: NatType,
        predict: bool,
        there: &str,
        privacy: Privacy,
        peers: u8,
        offers_ms: &[u64],
    ) -> (Vec<Birthday<SocketAddr>>, usize, Option<Vec<u8>>) {
        let (births, sent) = punch_case(mine, their, predict, there, privacy, (10, peers), offers_ms, false);
        (births, sent.len(), sent.first().map(|t| t.payload.clone()))
    }

    /// [`birthday_case`], with every datagram to the offered IP, in order, and
    /// `peers.1` peers `id(peers.0)`, `id(peers.0 + 1)`, ...; `old`: the peers predate
    /// NB-083, so their offers don't say how their NATs map.
    #[allow(clippy::too_many_arguments)]
    fn punch_case(
        mine: &[u16],
        their: NatType,
        predict: bool,
        there: &str,
        privacy: Privacy,
        peers: (u8, u8),
        offers_ms: &[u64],
        old: bool,
    ) -> (Vec<Birthday<SocketAddr>>, Vec<Transmit<SocketAddr>>) {
        let at = vec![there.parse().unwrap(); usize::from(peers.1)];
        punch_case_at(mine, their, predict, &at, privacy, peers.0, offers_ms, old)
    }

    /// [`punch_case`] with peers `id(first)`, `id(first + 1)`, ... offering `at[0]`, `at[1]`, ...
    #[allow(clippy::too_many_arguments)]
    fn punch_case_at(
        mine: &[u16],
        their: NatType,
        predict: bool,
        at: &[SocketAddr],
        privacy: Privacy,
        first: u8,
        offers_ms: &[u64],
        old: bool,
    ) -> (Vec<Birthday<SocketAddr>>, Vec<Transmit<SocketAddr>>) {
        let me = id(1).peer_id();
        // Two servers at two IPs, then the first one's IP on a second port.
        let servers: [SocketAddr; 3] =
            ["192.0.2.1:3478", "192.0.2.2:3478", "192.0.2.1:3479"].map(|a| a.parse().unwrap());
        let local: Vec<SocketAddr> = vec!["10.0.0.2:7000".parse().unwrap()];
        let mut t = Traversal::new(&id(1), [3; 32], 0, local, servers[..mine.len()].to_vec());
        let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
        t.set_privacy(&mut ep, privacy);
        t.handle_timeout(Instant::ZERO, &mut ep);
        for (i, r) in std::iter::from_fn(|| t.poll_transmit()).collect::<Vec<_>>().into_iter().enumerate() {
            let tx = stun::parse_binding_request(&r.payload).unwrap();
            t.handle_stun(&r.dst, &stun::binding_response(&tx, SocketAddr::from(([203, 0, 113, 9], mine[i]))));
        }
        let nat = match mine {
            [a, b, c] if a == c && a != b => NatType::SymmetricIp,
            _ if mine.iter().all(|m| *m == mine[0]) => NatType::Cone,
            _ => NatType::Symmetric,
        };
        assert_eq!(t.nat_type(), nat);
        assert_eq!(t.predicted_addr().is_some(), *mine == SEQ || *mine == SEQ_IP);
        let ids: Vec<Identity> = (0..at.len() as u8).map(|n| id(first + n)).collect();
        for p in &ids {
            t.introduce(Instant::ZERO, &ep, p.peer_id(), None);
        }
        let (mut births, mut sent, mut nonce) = (Vec::new(), Vec::new(), 0);
        for ms in (0..6000).step_by(10) {
            let now = Instant::from_millis(ms);
            for (p, &there) in ids.iter().zip(at).filter(|_| offers_ms.contains(&ms)) {
                nonce += 1;
                let o = Offer {
                    kind: Proof::None,
                    reply: false,
                    role: Role::Peer,
                    nat: their,
                    says_mapping: their.symmetric() && !old,
                    ts: ms,
                    nonce,
                    addrs: vec![there],
                    predict: predict.then_some(Prediction { next: there.port() + 2, step: 1 }),
                    relays: vec![],
                    relayed: Vec::new(),
                };
                let sealed = sealed::seal(p, &me, [7; 32], &o.encode()).unwrap();
                t.handle_signal(now, &mut ep, Topic::peer(&me), &sealed);
            }
            ep.handle_timeout(now);
            if t.poll_timeout().is_some_and(|at| at <= now) {
                t.handle_timeout(now, &mut ep);
            }
            births.extend(std::iter::from_fn(|| t.poll_birthday()));
            sent.extend(
                std::iter::from_fn(|| ep.poll_transmit(now)).filter(|d| at.iter().any(|a| a.ip() == d.dst.ip())),
            );
        }
        (births, sent)
    }

    /// NB-044: a birthday punch is planned only between a random-port symmetric
    /// NAT and a cone, towards a public IPv4 address, once per peer, within the
    /// node's budget, and never under `RelayOnly`.
    #[test]
    fn birthday_punches_follow_the_nat_types() {
        let (public, private) = ("198.51.100.9:20000", "192.168.1.50:20000");
        let direct = Privacy::Direct;
        // Handshake sends in 6 s: at 0, 0.5, 1.5 and 3.5 s.
        let attempts = 4;
        // Behind a cone, facing a random-port symmetric NAT: spray its IP, no sockets.
        let (b, sent, _) = birthday_case(&CONE, NatType::Symmetric, false, public, direct, 1, &[0]);
        assert!(b.is_empty());
        assert_eq!(sent, attempts + usize::from(BIRTHDAY_PROBES), "every probe, each port once");
        // A predictable one is probed around its prediction instead.
        let (b, sent, _) = birthday_case(&CONE, NatType::Symmetric, true, public, direct, 1, &[0]);
        assert!(b.is_empty() && sent < 200, "{sent}");
        // Behind the symmetric NAT, facing a cone: sockets towards its address, with the dial's HS1.
        let (b, sent, hs1) = birthday_case(&SYM, NatType::Cone, false, public, direct, 1, &[0]);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].dst, b[0].sockets), (public.parse().unwrap(), BIRTHDAY_SOCKETS));
        assert_eq!(b[0].until, Instant::from_millis(1000) + BIRTHDAY_OPEN_FOR);
        assert_eq!(Some(&b[0].payload), hs1.as_ref());
        assert_eq!(sent, attempts, "no spray from the symmetric side");
        // Not for a private address, nor for open or symmetric peers, nor relay-only.
        assert!(birthday_case(&SYM, NatType::Cone, false, private, direct, 1, &[0]).0.is_empty());
        assert_eq!(birthday_case(&CONE, NatType::Symmetric, false, private, direct, 1, &[0]).1, attempts);
        assert!(birthday_case(&SYM, NatType::Open, false, public, direct, 1, &[0]).0.is_empty());
        assert!(birthday_case(&SYM, NatType::Symmetric, false, public, direct, 1, &[0]).0.is_empty());
        let relay_only = Privacy::RelayOnly { via: None };
        assert!(birthday_case(&SYM, NatType::Cone, false, public, relay_only, 1, &[0]).0.is_empty());
        // Offers again within the per-peer cooldown start nothing more.
        assert_eq!(birthday_case(&SYM, NatType::Cone, false, public, direct, 1, &[0, 2000, 4000]).0.len(), 1);
        // NB44-1: many PeerIds (minted freely by a room-key holder) naming ONE victim IP
        // start only one punch at it — the per-IP cap, not the node budget of 4. The open
        // side opens one fleet, and the spray side sprays once.
        assert_eq!(birthday_case(&SYM, NatType::Cone, false, public, direct, 6, &[0]).0.len(), 1);
        // The spray side: 4 PeerIds naming one IP send one spray's worth of probes, not
        // four (before NB44-1 this was ~8,208 datagrams to the victim).
        let four = birthday_case(&CONE, NatType::Symmetric, false, public, direct, 4, &[0]).1;
        let probes = usize::from(BIRTHDAY_PROBES);
        assert!(
            four >= probes && four < 2 * probes,
            "NB44-1: 4 peers at one IP spray once ({four}), not 4x (~{})",
            4 * probes
        );
    }

    /// NB-045: two sequential symmetric NATs sweep each other's predicted ports in
    /// order, the lower PeerId at `next + k·step` (96 of them), the other at
    /// `next + 2k·step` (64); not unless both predict.
    #[test]
    fn lockstep_sweeps_the_peers_predicted_ports_in_order() {
        let (there, direct) = ("198.51.100.9:20000", Privacy::Direct);
        let me = id(1).peer_id();
        let above = (10..60).find(|&n| id(n).peer_id() > me).unwrap();
        let below = (10..60).find(|&n| id(n).peer_id() < me).unwrap();
        for (base, stride, count) in [(above, 1, LOCKSTEP_LOW), (below, 2, LOCKSTEP_HIGH)] {
            let (b, sent) = punch_case(&SEQ, NatType::Symmetric, true, there, direct, (base, 1), &[0], false);
            assert!(b.is_empty());
            // The peer predicted next = 20002, step 1; the dial's own sends go to 20000.
            let swept: Vec<u16> = sent.iter().map(|t| t.dst.port()).filter(|&p| p != 20000).collect();
            let want: Vec<u16> = (0..count).map(|k| 20002 + k * stride).collect();
            assert_eq!(swept, want, "stride {stride}");
        }
        // A NAT stepping by 3: every sweep port is a whole step on, at the base's IP.
        let p = Prediction { next: 40_000, step: 3 };
        let base: SocketAddr = "203.0.113.4:39990".parse().unwrap();
        let ports =
            |stride| p.sweep(&base, stride, 3).iter().map(|a: &SocketAddr| (a.ip(), a.port())).collect::<Vec<_>>();
        let ip = base.ip();
        assert_eq!(ports(1), vec![(ip, 40_000), (ip, 40_003), (ip, 40_006)]);
        assert_eq!(ports(2), vec![(ip, 40_000), (ip, 40_006), (ip, 40_012)]);
        // Without our prediction, or the peer's, nothing but the dial's 4 sends.
        assert_eq!(punch_case(&SYM, NatType::Symmetric, true, there, direct, (10, 1), &[0], false).1.len(), 4);
        assert_eq!(punch_case(&SEQ, NatType::Symmetric, false, there, direct, (10, 1), &[0], false).1.len(), 4);
    }

    /// NB-083: a `SymmetricIp` node facing a symmetric peer whose offer says how it maps opens
    /// sockets towards it at 1 s, and the peer sprays them from 1.5 s, as a cone would; two
    /// `SymmetricIp` nodes each do both. Behind a NAT that maps per IP and port the spray
    /// opens no sockets and sends `PER_PORT_PROBES`, once per 5 minutes (NB83-1). One punch,
    /// under the same limits as a birthday punch. Not with a peer that predates NB-083
    /// (towards which the node is `Symmetric`, as before), nor when both predict (they sweep
    /// in lockstep, as before), and two `Symmetric` nodes still don't spray.
    #[test]
    fn sym_ip_nodes_open_and_symmetric_peers_spray() {
        let (public, private, direct) = ("198.51.100.9:20000", "192.168.1.50:20000", Privacy::Direct);
        let (attempts, probes, per_port) = (4, usize::from(BIRTHDAY_PROBES), usize::from(PER_PORT_PROBES));
        let case = |mine: &[u16], their, predict, there, privacy, peers, offers_ms: &[u64], old| {
            let (b, sent) = punch_case(mine, their, predict, there, privacy, (10, peers), offers_ms, old);
            (b, sent.len())
        };
        let (dst, until) = (public.parse().unwrap(), Instant::from_millis(1000) + BIRTHDAY_OPEN_FOR);
        // What a case towards `public` started: sockets, and datagrams past the dial's sends.
        let what = |(b, sent): (Vec<Birthday<SocketAddr>>, usize)| {
            assert!(b.iter().all(|b| (b.dst, b.until) == (dst, until)));
            (b.iter().map(|b| usize::from(b.sockets)).sum::<usize>(), sent - attempts)
        };
        let sockets = usize::from(BIRTHDAY_SOCKETS);
        let (opens, sprays, both, nothing) = ((sockets, 0), (0, per_port), (sockets, probes), (0, 0));
        use NatType::{Cone, Symmetric, SymmetricIp};
        // When one doesn't predict: the `SymmetricIp` side opens; a side that maps per IP and
        // port sprays `PER_PORT_PROBES`, each port once, and opens nothing; two `SymmetricIp`
        // sides do both, the full spray, as it costs each NAT one port.
        for (mine, their, predict, want) in [
            (&SYM_IP[..], Symmetric, false, opens),
            (&SYM_IP, Symmetric, true, opens),
            (&SEQ_IP, Symmetric, false, opens),
            (&SYM_3, SymmetricIp, false, sprays),
            (&SYM_3, SymmetricIp, true, sprays),
            (&SEQ, SymmetricIp, false, sprays),
            (&SYM_IP, SymmetricIp, false, both),
            (&SYM_IP, SymmetricIp, true, both),
            (&SEQ_IP, SymmetricIp, false, both),
        ] {
            let got = what(case(mine, their, predict, public, direct, 1, &[0], false));
            assert_eq!(got, want, "{mine:?} x {their:?}, predict {predict}");
        }
        // Two that predict sweep in lockstep (96 or 64 probes, by PeerId), as before NB-083,
        // whatever their offers say of how they map.
        for (mine, their, old) in [
            (&SEQ_IP[..], Symmetric, false),
            (&SEQ_IP, Symmetric, true),
            (&SEQ_IP, SymmetricIp, false),
            (&SEQ, SymmetricIp, false),
        ] {
            let (sockets, swept) = what(case(mine, their, true, public, direct, 1, &[0], old));
            assert!(sockets == 0 && [64, 96].contains(&swept), "{mine:?} x {their:?}, old {old}: {swept}");
        }
        // A peer whose offer doesn't say how its NAT maps wouldn't do its part: nothing.
        assert_eq!(what(case(&SYM_IP, Symmetric, false, public, direct, 1, &[0], true)), nothing);
        // Two plain symmetric NATs: nothing, as before.
        assert_eq!(what(case(&SYM_3, Symmetric, false, public, direct, 1, &[0], false)), nothing);
        // Against a cone, a `SymmetricIp` node is the symmetric side of a birthday punch, as before.
        assert_eq!(what(case(&SYM_IP, Cone, false, public, direct, 1, &[0], false)), opens);
        assert_eq!(what(case(&CONE, SymmetricIp, false, public, direct, 1, &[0], false)), (0, probes));
        // The limits: not towards a private address, nor relay-only; once per peer in the
        // cooldown; and many PeerIds naming one IP get one punch there (NB44-1), its spray included.
        assert_eq!(case(&SYM_IP, SymmetricIp, false, private, direct, 1, &[0], false), (vec![], attempts));
        let relay_only = Privacy::RelayOnly { via: None };
        assert!(case(&SYM_IP, SymmetricIp, false, public, relay_only, 1, &[0], false).0.is_empty());
        let quiet = case(&SYM_3, Symmetric, false, public, relay_only, 1, &[0], false).1;
        assert_eq!(case(&SYM_3, SymmetricIp, false, public, relay_only, 1, &[0], false).1, quiet);
        let (b, sent) = case(&SYM_IP, SymmetricIp, false, public, direct, 1, &[0, 2000, 4000], false);
        assert_eq!((b.len(), sent), (1, attempts + probes));
        let (b, sent) = case(&SYM_IP, SymmetricIp, false, public, direct, 6, &[0], false);
        assert_eq!((b.len(), sent), (1, 6 * attempts + probes));
        // Behind a NAT that maps per IP and port, each probe costs a port: one spray per 5
        // minutes, node-wide, though the peers are at three IPs; behind one that maps per IP,
        // the usual limits.
        let ips: Vec<SocketAddr> = (1..=3).map(|i| SocketAddr::from(([198, 51, 100, i], 20_000))).collect();
        let starts = |mine: &[u16], their| punch_case_at(mine, their, false, &ips, direct, 10, &[0], false);
        let (b, sent) = starts(&SYM_3, SymmetricIp);
        assert_eq!((b.len(), sent.len()), (0, 3 * attempts + per_port));
        assert_eq!(starts(&SYM_IP, Symmetric).0.len(), 3);
    }

    /// NB-035: a failure reads as a sentence that names the symmetric side.
    #[test]
    fn failures_say_which_side_is_symmetric() {
        let say = |f: Failure| f.to_string();
        assert!(say(Failure::Unreachable).contains("firewall drops UDP"));
        let ours = say(Failure::Symmetric(Side::Ours));
        assert!(ours.starts_with("this side's NAT is symmetric") && ours.contains("the peer's router only lets in"));
        let theirs = say(Failure::Symmetric(Side::Theirs));
        assert!(
            theirs.starts_with("the peer's NAT is symmetric") && theirs.contains("this side's router only lets in")
        );
        assert!(say(Failure::Symmetric(Side::Both)).starts_with("both NATs are symmetric"));
    }

    fn fuzz_addr(rng: &mut Rng) -> SocketAddr {
        const SPECIAL: [&str; 9] = [
            "127.0.0.1:5",
            "[::ffff:127.0.0.1]:5",
            "0.0.0.0:5",
            "224.0.0.1:5",
            "[ff02::1]:5",
            "255.255.255.255:5",
            "198.51.100.7:0",
            "169.254.1.1:5",
            "[fe80::1]:5",
        ];
        match rng.below(4) {
            0 => rng.pick(&SPECIAL).parse().unwrap(),
            1 => SocketAddr::from((std::net::Ipv4Addr::from(rng.u32()), rng.u32() as u16)),
            2 => SocketAddr::from((std::net::Ipv6Addr::from(u128::from(rng.u64()) << 64 | 1), rng.u32() as u16)),
            _ => SocketAddr::from(([198, 51, 100, rng.byte()], 1 + rng.below(65535) as u16)),
        }
    }

    fn fuzz_offer(rng: &mut Rng, kind: Proof) -> Offer<SocketAddr> {
        let roles = [Role::Peer, Role::Host, Role::Client];
        let nats = [NatType::Unknown, NatType::Open, NatType::Cone, NatType::Symmetric, NatType::SymmetricIp];
        Offer {
            kind,
            reply: rng.one_in(2),
            role: *rng.pick(&roles),
            nat: *rng.pick(&nats),
            says_mapping: rng.one_in(2),
            ts: rng.u64() >> rng.below(64),
            nonce: rng.u64(),
            addrs: (0..rng.below(MAX_CANDIDATES + 1)).map(|_| fuzz_addr(rng)).collect(),
            predict: rng.one_in(2).then(|| Prediction { next: rng.u32() as u16, step: 1 + rng.below(8) as u16 }),
            relays: (0..rng.below(MAX_RELAYS + 1)).map(|_| PeerId([rng.byte(); 32])).collect(),
            relayed: (0..rng.below(MAX_RELAYED + 1)).map(|_| fuzz_addr(rng)).collect(),
        }
    }

    /// NB-014: hellos and offers, mutated; whatever parses round-trips and stays within its limits.
    #[test]
    fn fuzz_hello_and_offer_parsers() {
        let key = room_key(ROOM.0, ROOM.1);
        let ids: Vec<PeerId> = (1..6).map(|n| id(n).peer_id()).collect();
        run(
            "hello",
            cases(30_000),
            |rng| {
                let roles = [Role::Peer, Role::Host, Role::Client];
                let mut b = hello(&key, rng.pick(&ids), *rng.pick(&roles), rng.u64() >> rng.below(64));
                if rng.below(4) != 0 {
                    mutate(rng, &mut b, 400);
                }
                b
            },
            |b| {
                if let Some((from, role, ts, proof)) = parse_hello(b) {
                    let (id, name, p) = (hex(from.as_bytes()), role_name(role), hex(&proof));
                    let again = format!("{HELLO_HEAD}{id}{HELLO_MID}{name}{HELLO_TS}{ts}{HELLO_PROOF}{p}{HELLO_TAIL}");
                    assert_eq!(parse_hello(again.as_bytes()), Some((from, role, ts, proof)));
                }
            },
        );
        let base: SocketAddr = "198.51.100.7:40000".parse().unwrap();
        run(
            "offer",
            cases(30_000),
            |rng| {
                let kinds = [Proof::None, Proof::Room, Proof::Invite];
                let kind = *rng.pick(&kinds);
                let mut b = fuzz_offer(rng, kind).encode();
                if rng.below(4) != 0 {
                    mutate(rng, &mut b, 600);
                }
                b
            },
            |b| {
                if let Some(o) = Offer::<SocketAddr>::decode(b) {
                    assert!(o.addrs.len() <= MAX_CANDIDATES && o.relays.len() <= MAX_RELAYS);
                    assert!(o.relayed.len() <= MAX_RELAYED && o.addrs.len() + o.relayed.len() <= MAX_CANDIDATES);
                    assert_eq!(Offer::decode(&o.encode()).as_ref(), Some(&o));
                    if let Some(p) = o.predict {
                        assert!((1..=MAX_STEP).contains(&p.step));
                        assert!(p.window(&base).len() <= (GUESS_BEFORE + GUESS_AFTER + 1) as usize);
                    }
                }
                if let Some(o) = Offer::<u32>::decode(b) {
                    assert_eq!(Offer::decode(&o.encode()).as_ref(), Some(&o));
                }
            },
        );
    }

    /// NB-014: a room member that holds the room key (and an invite holder) sends
    /// authenticated hellos and sealed, proved offers with random and mutated
    /// bodies. Nothing panics, and nothing makes the endpoint dial a special address.
    #[test]
    fn fuzz_authenticated_signals() {
        fn special(a: &SocketAddr) -> bool {
            let ip = a.ip().to_canonical();
            a.port() == 0
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_loopback()
                || match ip {
                    std::net::IpAddr::V4(v) => v.is_broadcast() || v.is_link_local(),
                    std::net::IpAddr::V6(v) => v.segments()[0] & 0xffc0 == 0xfe80,
                }
        }
        let me = id(1).peer_id();
        let (mut t, mut ep) = in_room(Role::Peer, None, "10.0.0.1:7000".parse().unwrap());
        let (room, inbox) = (Topic::room(ROOM.0, ROOM.1), Topic::peer(&me));
        let (key, invite_key) = (room_key(ROOM.0, ROOM.1), t.invite_key());
        let senders: Vec<Identity> = (2..10).map(id).collect();
        let mut rng = Rng::for_target("signals");
        let mut now = Instant::from_millis(1000);
        let (mut dialled, mut hellos) = (0usize, 0usize);
        for i in 0..cases(1_500) {
            let from = rng.pick(&senders);
            let (topic, payload) = match rng.below(5) {
                0 => {
                    let roles = [Role::Peer, Role::Host, Role::Client];
                    let ts =
                        ((now - Instant::ZERO).as_millis() as u64 + rng.below(120_000) as u64).saturating_sub(60_000);
                    hellos += 1;
                    (room, room_hello(&key, &from.peer_id(), *rng.pick(&roles), ts))
                }
                1 => {
                    let n = rng.below(300);
                    let mut b = rng.bytes(n);
                    let topic = *rng.pick(&[room, inbox]);
                    if rng.one_in(2) && topic == room {
                        // Junk or a mutated hello that opens: parse_hello sees it.
                        if rng.one_in(2) {
                            b = hello(&key, &from.peer_id(), Role::Peer, (now - Instant::ZERO).as_millis() as u64);
                            mutate(&mut rng, &mut b, 300);
                        }
                        b = seal_room(&room_enc_key(ROOM.0, ROOM.1), rng.bytes(NONCE_LEN).try_into().unwrap(), &b);
                    } else if rng.one_in(2) {
                        b = sealed::seal(from, &me, rng.bytes(32).try_into().unwrap(), &b).unwrap();
                    }
                    (topic, b)
                }
                _ => {
                    let kinds = [Proof::None, Proof::Room, Proof::Invite];
                    let kind = *rng.pick(&kinds);
                    let mut o = fuzz_offer(&mut rng, kind);
                    o.ts = (now - Instant::ZERO).as_millis() as u64;
                    let mut body = o.encode();
                    if rng.one_in(3) {
                        mutate(&mut rng, &mut body, 600);
                    }
                    let proof = match kind {
                        Proof::None => None,
                        Proof::Room => {
                            Some(mac(&key, &[b"pw-offer".as_slice(), from.peer_id().as_bytes(), me.as_bytes(), &body]))
                        }
                        Proof::Invite => Some(mac(
                            &invite_key,
                            &[b"pw-invite".as_slice(), from.peer_id().as_bytes(), me.as_bytes(), &body],
                        )),
                    };
                    body.extend(proof.into_iter().flatten());
                    (inbox, sealed::seal(from, &me, rng.bytes(32).try_into().unwrap(), &body).unwrap())
                }
            };
            if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                t.handle_signal(now, &mut ep, topic, &payload)
            })) {
                eprintln!("fuzz signals: case {i} of seed {:#x} failed on {topic:?} {}", seed(), fuzz_hex(&payload));
                std::panic::resume_unwind(e);
            }
            now = now + Duration::from_millis(rng.below(400) as u64);
            ep.handle_timeout(now);
            t.handle_timeout(now, &mut ep);
            while let Some(e) = ep.poll_event() {
                t.handle_event(now, &mut ep, &e);
            }
            while t.poll_signal().is_some() {}
            for d in dials(&mut ep, now) {
                assert!(!special(&d), "case {i}: dialled {d}");
                dialled += 1;
            }
        }
        println!("fuzz signals: {hellos} hellos, {dialled} dials");
        assert!(dialled > 0, "authenticated offers must reach the dialler");
    }

    /// A traversal with TURN (`Hop` addresses) at 192.0.2.10:7000, with an allocation on the simulator's TURN server:
    /// it, its endpoint, and the relayed address.
    #[cfg(feature = "turn")]
    fn with_allocation() -> (Traversal<Hop<SocketAddr>>, Endpoint<Hop<SocketAddr>>, SocketAddr) {
        use peerwire_sim::turn::{Out, TurnServer as Server};
        let (s, me): (SocketAddr, SocketAddr) =
            ("198.51.100.1:3478".parse().unwrap(), "192.0.2.10:7000".parse().unwrap());
        let mut port = 50000;
        let next = move || {
            port += 1;
            Some(SocketAddr::new(s.ip(), port))
        };
        let mut server = Server::new(s, "peerwire.test", next).user("app", "secret");
        let mut t = Traversal::new(&id(1), [1; 32], 1_700_000_000_000, vec![Hop::Net(me)], vec![]);
        let mut ep = Endpoint::new(&id(1), Config::default(), [2; 32]);
        let now = Instant::from_millis(1);
        t.set_policy(now, &mut ep, Some(Policy::new([PathKind::Direct, PathKind::Turn])));
        t.set_turn_servers(now, &mut ep, vec![(Hop::Net(s), TurnServer::new(s.to_string(), "app", "secret"))]);
        while let Some(tx) = t.poll_transmit() {
            for o in server.handle_client(now, me, &tx.payload) {
                if let Out::ToClient { data, .. } = o {
                    assert_eq!(t.handle_turn(now, &Hop::Net(s), &data), Some(None));
                }
            }
        }
        let relayed = t.turn_status()[0].1.expect("allocated");
        (t, ep, relayed)
    }

    /// NB-099: where this node's TURN server relays from isn't where its NAT maps it: an observed address that is
    /// its relayed address isn't taken (another is), and its offers carry it only in the relayed entry, which
    /// decoders that predate it skip as an address they can't read.
    #[cfg(feature = "turn")]
    #[test]
    fn a_relayed_address_stays_out_of_the_nat_view() {
        let (mut t, mut ep, relayed) = with_allocation();
        let (now, by, seen) = (Instant::from_millis(2), id(2).peer_id(), "203.0.113.5:4000".parse().unwrap());
        for addr in [relayed, seen] {
            t.handle_event(now, &mut ep, &Event::AddrObserved { by, addr: Hop::Net(addr) });
        }
        assert!(t.candidates().contains(&Hop::Net(seen)) && !t.candidates().contains(&Hop::Net(relayed)));
        let to = id(3);
        t.handle_timeout(now, &mut ep);
        std::iter::from_fn(|| t.poll_signal()).for_each(drop);
        t.offer(now, &ep, to.peer_id(), false, false);
        let offers: Vec<Offer<Hop<SocketAddr>>> = std::iter::from_fn(|| t.poll_signal())
            .filter_map(|s| match s {
                Signal::Publish(_, p) => Offer::decode(&sealed::open(&to, &p)?.1),
                _ => None,
            })
            .collect();
        assert_eq!(offers.len(), 1);
        assert!(!offers[0].addrs.contains(&Hop::Net(relayed)), "{:?}", offers[0].addrs);
        assert_eq!(offers[0].relayed, [Hop::Net(relayed)]);
        let mut entry = RELAYED_TAG.to_vec();
        relayed.encode(&mut entry);
        assert_eq!(<SocketAddr as Address>::decode(&entry), None);
    }

    /// Two endpoints with `Hop` addresses linked: a (`id(1)`) dials b (`id(2)`) at `b_at`, and b sees a at `a_at`.
    #[cfg(feature = "turn")]
    fn hop_link(a_at: Hop<SocketAddr>, b_at: Hop<SocketAddr>) -> [Endpoint<Hop<SocketAddr>>; 2] {
        let mut eps = [1u8, 2].map(|i| Endpoint::new(&id(i), Config::default(), [i + 2; 32]));
        let mut now = Instant::from_millis(1);
        eps[0].connect(now, b_at, id(2).peer_id()).unwrap();
        for _ in 0..30 {
            while let Some(t) = eps[0].poll_transmit(now) {
                eps[1].handle_datagram(now, a_at, &t.payload);
            }
            while let Some(t) = eps[1].poll_transmit(now) {
                eps[0].handle_datagram(now, b_at, &t.payload);
            }
            now = now + Duration::from_millis(10);
            eps.iter_mut().for_each(|e| e.handle_timeout(now));
        }
        assert!(eps[0].is_connected(&id(2).peer_id()) && eps[1].is_connected(&id(1).peer_id()));
        eps
    }

    /// An offer from `t`'s point of view: plain, with these addresses and relayed addresses, as of `now`.
    #[cfg(feature = "turn")]
    fn hop_offer(
        t: &Traversal<Hop<SocketAddr>>,
        now: Instant,
        addrs: &[&str],
        relayed: &[&str],
    ) -> Offer<Hop<SocketAddr>> {
        let hops = |v: &[&str]| v.iter().map(|a| Hop::Net(a.parse().unwrap())).collect();
        Offer {
            kind: Proof::None,
            reply: false,
            role: Role::Peer,
            nat: NatType::Cone,
            says_mapping: false,
            ts: t.wall_ms(now),
            nonce: now.as_micros(),
            addrs: hops(addrs),
            predict: None,
            relays: vec![],
            relayed: hops(relayed),
        }
    }

    const R: &str = "198.51.100.20:50001";

    /// NB-099: b linked a at an address before a's offer said it is a's relayed one (a's dial through its allocation
    /// beat the offer): the offer retags the link; and the other way round, the link coming up after the offer is
    /// retagged on its `Connected`.
    #[cfg(feature = "turn")]
    #[test]
    fn a_link_at_a_relayed_address_offered_is_retagged() {
        let (r, b): (SocketAddr, SocketAddr) = (R.parse().unwrap(), "203.0.113.2:7000".parse().unwrap());
        let a = id(1).peer_id();
        let now = Instant::from_millis(500);
        let [_, mut eb] = hop_link(Hop::Net(r), Hop::Net(b));
        let mut tb = Traversal::new(&id(2), [5; 32], 1_700_000_000_000, vec![Hop::Net(b)], vec![]);
        let o = hop_offer(&tb, now, &[], &[R]);
        tb.on_offer(now, &mut eb, a, o);
        assert_eq!(eb.path(&a), Some(&Path::Direct(Hop::Relayed(r))), "retagged by the offer");

        let mut tb = Traversal::new(&id(2), [5; 32], 1_700_000_000_000, vec![Hop::Net(b)], vec![]);
        let mut dialler = Endpoint::new(&id(2), Config::default(), [9; 32]);
        let o = hop_offer(&tb, now, &[], &[R]);
        tb.on_offer(now, &mut dialler, a, o);
        assert!(std::iter::from_fn(|| dialler.poll_transmit(now)).any(|t| t.dst == Hop::Relayed(r)), "dialled there");
        let [_, mut eb] = hop_link(Hop::Net(r), Hop::Net(b));
        let up = std::iter::from_fn(|| eb.poll_event()).find(|e| matches!(e, Event::Connected { .. })).unwrap();
        assert_eq!(eb.path(&a), Some(&Path::Direct(Hop::Net(r))));
        tb.handle_event(now, &mut eb, &up);
        assert_eq!(eb.path(&a), Some(&Path::Direct(Hop::Relayed(r))), "retagged on Connected");
    }

    /// NB99-I1: under `[Direct]`, a link at a peer's relayed address is TURN's whichever came first, the link (its
    /// dial through its allocation) or the offer that names the address: the policy reads the retagged path, and
    /// the link moves off it either way. `allows` takes the address as TURN's for that peer only, before the retag.
    #[cfg(feature = "turn")]
    #[test]
    fn a_link_at_a_relayed_address_follows_the_policy_in_either_order() {
        let (r, b): (SocketAddr, SocketAddr) = (R.parse().unwrap(), "203.0.113.2:7000".parse().unwrap());
        let (a, now) = (id(1).peer_id(), Instant::from_millis(500));
        let direct = || Some(Policy::new([PathKind::Direct]));
        let moving = |t: &Traversal<Hop<SocketAddr>>| t.remotes.get(&a).is_some_and(|r| r.moving.is_some());

        let [_, mut eb] = hop_link(Hop::Net(r), Hop::Net(b));
        let mut tb = Traversal::new(&id(2), [5; 32], 1_700_000_000_000, vec![Hop::Net(b)], vec![]);
        tb.set_policy(now, &mut eb, direct());
        for e in std::iter::from_fn(|| eb.poll_event()).collect::<Vec<_>>() {
            tb.handle_event(now, &mut eb, &e);
        }
        assert!(eb.is_connected(&a) && !moving(&tb), "a direct link, as far as b knows");
        tb.on_offer(now, &mut eb, a, hop_offer(&tb, now, &["203.0.113.9:7000"], &[R]));
        assert_eq!(eb.path(&a), Some(&Path::Direct(Hop::Relayed(r))));
        assert!(moving(&tb), "the link first, then the offer");

        let mut tb = Traversal::new(&id(2), [5; 32], 1_700_000_000_000, vec![Hop::Net(b)], vec![]);
        let mut dialler = Endpoint::new(&id(2), Config::default(), [9; 32]);
        tb.set_policy(now, &mut dialler, direct());
        tb.on_offer(now, &mut dialler, a, hop_offer(&tb, now, &["203.0.113.9:7000"], &[R]));
        let at_r = Path::Direct(Hop::Net(r));
        assert!(!tb.allows(&a, &at_r) && tb.allows(&id(3).peer_id(), &at_r));
        let [_, mut eb] = hop_link(Hop::Net(r), Hop::Net(b));
        let up = std::iter::from_fn(|| eb.poll_event()).find(|e| matches!(e, Event::Connected { .. })).unwrap();
        assert!(!moving(&tb));
        tb.handle_event(now, &mut eb, &up);
        assert_eq!(eb.path(&a), Some(&Path::Direct(Hop::Relayed(r))));
        assert!(moving(&tb), "the offer first, then the link");
    }

    /// NB99-1: m (`id(3)`) names v's address as its relayed one. v's packets from there are still from a network
    /// address, under any policy, and v links with this node on the direct path (unfixed: a TURN link, through m
    /// under the default policy, none under `[Direct]`). A claimant's own packets from its relayed address are
    /// relayed; an HS1 from there, which names no link, isn't.
    #[cfg(feature = "turn")]
    #[test]
    fn a_relayed_claim_takes_only_the_claimants_packets() {
        let (a_at, v_at): (SocketAddr, SocketAddr) =
            ("192.0.2.1:7000".parse().unwrap(), "203.0.113.2:7000".parse().unwrap());
        let (a, v, m) = (id(1).peer_id(), id(2).peer_id(), id(3).peer_id());
        for policy in [None, Some(Policy::default()), Some(Policy::new([PathKind::Direct]))] {
            let mut now = Instant::from_millis(500);
            let mut ta = Traversal::new(&id(1), [6; 32], 1_700_000_000_000, vec![Hop::Net(a_at)], vec![]);
            let mut ea = Endpoint::new(&id(1), Config::default(), [3; 32]);
            let mut ev = Endpoint::new(&id(2), Config::default(), [4; 32]);
            ta.set_policy(now, &mut ea, policy.clone());
            ta.on_offer(now, &mut ea, m, hop_offer(&ta, now, &[], &[&v_at.to_string()]));
            ev.connect(now, Hop::Net(a_at), a).unwrap();
            for _ in 0..50 {
                while let Some(t) = ev.poll_transmit(now) {
                    let hop = ta.hop(&ea, Hop::Net(v_at), &t.payload);
                    assert_eq!(hop, Hop::Net(v_at), "{policy:?}");
                    ea.handle_datagram(now, hop, &t.payload);
                }
                while let Some(e) = ea.poll_event() {
                    ta.handle_event(now, &mut ea, &e);
                }
                // To v's socket, as on the wire: the dials to m's claimed address too.
                for t in std::iter::from_fn(|| ea.poll_transmit(now)).filter(|t| *t.dst.addr() == v_at) {
                    ev.handle_datagram(now, Hop::Net(a_at), &t.payload);
                }
                now = now + Duration::from_millis(20);
                ea.handle_timeout(now);
                ev.handle_timeout(now);
            }
            assert_eq!(ea.path(&v), Some(&Path::Direct(Hop::Net(v_at))), "{policy:?}");
            assert!(ev.is_connected(&a), "{policy:?}");
        }

        let r: SocketAddr = R.parse().unwrap();
        let now = Instant::from_millis(500);
        let [mut ea, mut eb] = hop_link(Hop::Net(r), Hop::Relayed(r));
        let mut ta = Traversal::new(&id(1), [6; 32], 1_700_000_000_000, vec![Hop::Net(a_at)], vec![]);
        ta.on_offer(now, &mut ea, id(2).peer_id(), hop_offer(&ta, now, &[], &[R]));
        eb.send(&a, peerwire_core::Lane::Reliable, b"from r").unwrap();
        let data = eb.poll_transmit(now).unwrap().payload;
        assert_eq!(ta.hop(&ea, Hop::Net(r), &data), Hop::Relayed(r), "the claimant's own");
        let mut dialler = Endpoint::new(&id(3), Config::default(), [8; 32]);
        dialler.connect(now, Hop::Net(a_at), a).unwrap();
        let hs1 = dialler.poll_transmit(now).unwrap().payload;
        assert_eq!(ta.hop(&ea, Hop::Net(r), &hs1), Hop::Net(r), "an HS1 names no link");
    }

    /// NB-099: under `[Turn, Direct]` a link at a relayed address is kept there, by the endpoint too: given a direct
    /// candidate, it punches to none. Under `[Direct, Turn]` it is let go, and given the peer's addresses (from its
    /// offer), it punches.
    #[cfg(feature = "turn")]
    #[test]
    fn a_turn_link_is_kept_while_turn_ranks_first() {
        let (a_at, b_at): (SocketAddr, SocketAddr) = ("192.0.2.1:7000".parse().unwrap(), R.parse().unwrap());
        let b = id(2).peer_id();
        let [mut ea, _] = hop_link(Hop::Net(a_at), Hop::Relayed(b_at));
        let mut ta = Traversal::new(&id(1), [6; 32], 1_700_000_000_000, vec![Hop::Net(a_at)], vec![]);
        let mut now = Instant::from_millis(500);
        ta.set_policy(now, &mut ea, Some(Policy::new([PathKind::Turn, PathKind::Direct])));
        let o = hop_offer(&ta, now, &["203.0.113.2:7000"], &[]);
        ta.on_offer(now, &mut ea, b, o);
        ea.add_candidates(now, &b, &[Hop::Net("203.0.113.3:7000".parse().unwrap())]);
        for _ in 0..50 {
            now = now + Duration::from_millis(20);
            ea.handle_timeout(now);
            std::iter::from_fn(|| ea.poll_transmit(now)).for_each(drop);
        }
        assert_eq!(ea.stats().punches_sent, 0, "kept");
        ta.set_policy(now, &mut ea, Some(Policy::new([PathKind::Direct, PathKind::Turn])));
        let sent: Vec<_> = (0..20)
            .flat_map(|_| {
                now = now + Duration::from_millis(20);
                ea.handle_timeout(now);
                std::iter::from_fn(|| ea.poll_transmit(now)).map(|t| t.dst).collect::<Vec<_>>()
            })
            .collect();
        assert!(sent.contains(&Hop::Net("203.0.113.2:7000".parse().unwrap())), "let go, it punches: {sent:?}");
    }

    /// NB-099: an allocation made after this node offered to a peer, which hasn't answered yet, offers again, with
    /// the relayed address.
    #[cfg(feature = "turn")]
    #[test]
    fn an_allocation_made_is_offered_to_peers_offered_to_before() {
        use peerwire_sim::turn::{Out, TurnServer as Server};
        let (s, me): (SocketAddr, SocketAddr) =
            ("198.51.100.1:3478".parse().unwrap(), "192.0.2.10:7000".parse().unwrap());
        let mut server =
            Server::new(s, "peerwire.test", move || Some(SocketAddr::new(s.ip(), 50001))).user("app", "secret");
        let mut t = Traversal::new(&id(1), [1; 32], 1_700_000_000_000, vec![Hop::Net(me)], vec![]);
        let mut ep = Endpoint::new(&id(1), Config::default(), [2; 32]);
        let now = Instant::from_millis(1);
        let to = id(3);
        t.set_policy(now, &mut ep, Some(Policy::new([PathKind::Direct, PathKind::Turn])));
        t.offer(now, &ep, to.peer_id(), true, false);
        t.set_turn_servers(now, &mut ep, vec![(Hop::Net(s), TurnServer::new(s.to_string(), "app", "secret"))]);
        while let Some(tx) = t.poll_transmit() {
            for o in server.handle_client(now, me, &tx.payload) {
                if let Out::ToClient { data, .. } = o {
                    t.handle_turn(now, &Hop::Net(s), &data);
                }
            }
        }
        assert_eq!(t.poll_timeout(), Some(Instant::ZERO), "the allocation's news is due");
        t.handle_timeout(now, &mut ep);
        let offers: Vec<Offer<Hop<SocketAddr>>> = std::iter::from_fn(|| t.poll_signal())
            .filter_map(|s| match s {
                Signal::Publish(_, p) => Offer::decode(&sealed::open(&to, &p)?.1),
                _ => None,
            })
            .collect();
        let relayed: Vec<_> = offers.iter().map(|o| o.relayed.clone()).collect();
        assert_eq!(relayed, [vec![], vec![Hop::Net(SocketAddr::new(s.ip(), 50001))]]);
    }
}
