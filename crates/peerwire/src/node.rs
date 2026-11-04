use crate::portmap::{self, PortMapping, Worker};
use crate::signaling::Signaling;
use crate::traversal::{Birthday, Failure, Hop, NatType, PathKind, Policy, Privacy, Role, Signal, Traversal};
#[cfg(feature = "turn")]
use crate::turn::TurnServer;
use crate::Event;
use peerwire_core::Event as Ev;
use peerwire_core::{
    Config, ConnectError, DisconnectReason, Endpoint, Identity, Invite, Lane, Path, PeerId, PeerStats, SendError,
    Stats, Transmit,
};
use rand_core::{OsRng, RngCore};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// What the node's endpoint and traversal take as addresses ([`Hop`]): TURN hops besides network addresses.
type Addr = Hop<SocketAddr>;

/// Most datagrams read in one [`Node::poll`], so a flood can't stall the caller's frame.
const MAX_READS_PER_POLL: usize = 4096;
/// Most signaling messages handled in one poll.
const MAX_SIGNALS_PER_POLL: usize = 256;
/// Datagrams kept while the socket's send buffer is full; older ones are dropped, as UDP would.
const MAX_PENDING_SENDS: usize = 1024;
/// Signaling carriers are polled, so a host sleeping on `next_timeout` wakes at least this often.
const SIGNAL_POLL: Duration = Duration::from_millis(50);
/// A relay peer that drops is dialled again at once, then after 1 s, 2 s, ... up to 30 s; the
/// wait starts over once a link has held for 30 s.
const RELAY_REDIAL: Duration = Duration::from_secs(1);
const RELAY_REDIAL_MAX: Duration = Duration::from_secs(30);
/// Most extra sockets open at once, for birthday punching (two punches' worth).
const MAX_EXTRA_SOCKETS: usize = 512;
/// Extra sockets to release when a bind runs out of file descriptors, so the rest
/// of the process keeps headroom instead of sitting at the ceiling (NB44-3).
const FD_RESERVE: usize = 64;
/// Most routes held at once: one per open extra socket, so a flood can't grow the
/// map (NB44-2). `close_extra` prunes it to the open sockets anyway.
const MAX_ROUTES: usize = MAX_EXTRA_SOCKETS;
/// An extra socket that carries a route is closed once it has heard nothing for this long.
const ROUTE_IDLE: Duration = Duration::from_secs(60);
/// While extra sockets are open, a host sleeping on `next_timeout` wakes at least
/// this often to read them (it can only wait on the main socket).
const EXTRA_POLL: Duration = Duration::from_millis(5);

/// Public STUN servers [`Node::bind`] asks for the node's public address. Google's is
/// asked on two ports of one IP, which tells a NAT that maps per destination IP
/// ([`NatType::SymmetricIp`](crate::NatType::SymmetricIp)) from one that maps per IP and port.
pub const DEFAULT_STUN_SERVERS: &[&str] =
    &["stun.l.google.com:19302", "stun.cloudflare.com:3478", "stun.l.google.com:3478"];

/// Where a node stands with one peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not connected, and not dialling.
    Disconnected,
    /// Dialling: the handshake is under way.
    Connecting,
    /// The encrypted link is up.
    Connected,
}

/// Why [`Node::connect`], [`Node::set_relay_peers`] or [`Node::set_policy`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The string is not an `pw:` invite.
    InvalidInvite,
    /// The policy's order is empty or lists a kind twice ([`Policy::is_valid`]).
    InvalidPolicy,
    /// The dial didn't start: the invite names this node ([`ConnectError::SelfConnect`])
    /// or a key that isn't valid ([`ConnectError::InvalidPeerId`]), it lists no address
    /// in an IP family this socket reaches and no relay this node is linked to
    /// ([`ConnectError::NoPath`]), or this node is relay-only and no relay is linked
    /// ([`ConnectError::NoRelay`]).
    Connect(ConnectError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidInvite => f.write_str("not a valid pw: invite"),
            Error::InvalidPolicy => f.write_str("a policy lists each kind of path at most once, and at least one"),
            Error::Connect(e) => write!(f, "cannot dial: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Options for [`Node`], from [`Node::builder`].
pub struct Builder {
    identity: Option<Identity>,
    config: Config,
    stun: Vec<String>,
    signaling: Option<Box<dyn Signaling>>,
    privacy: Privacy,
    port_mapping: bool,
    /// PCP/NAT-PMP server and SSDP target, instead of the default gateway's (tests).
    portmap_at: Option<(SocketAddr, SocketAddr)>,
    relay_peers: Vec<String>,
    policy: Option<Policy>,
    #[cfg(feature = "turn")]
    turn: Vec<TurnServer>,
}

impl Builder {
    /// The identity to use (to keep the same [`PeerId`] across runs). Default: a fresh one.
    pub fn identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Session settings. Default: [`Config::default`].
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// STUN servers (`host:port`) to learn the public address from. Default:
    /// [`DEFAULT_STUN_SERVERS`]; empty turns STUN off. Names are resolved in
    /// [`bind`](Builder::bind), each once (one name on two ports is one IP), and
    /// servers that don't resolve are skipped. A server on two ports of one IP, next
    /// to another at a second IP, tells [`NatType::SymmetricIp`](crate::NatType::SymmetricIp)
    /// from `Symmetric`.
    pub fn stun_servers<S: Into<String>>(mut self, servers: impl IntoIterator<Item = S>) -> Self {
        self.stun = servers.into_iter().map(Into::into).collect();
        self
    }

    /// A signaling carrier, for rooms ([`Node::join_room`]) and for invites that
    /// punch through NATs. Default: none (invites work where the host is reachable).
    pub fn signaling(mut self, signaling: impl Signaling + 'static) -> Self {
        self.signaling = Some(Box::new(signaling));
        self
    }

    /// Which addresses the node gives out. Default: [`Privacy::Direct`]. See
    /// [`Node::set_privacy`].
    pub fn privacy(mut self, privacy: Privacy) -> Self {
        self.privacy = privacy;
        self
    }

    /// Asks the LAN router to forward a public UDP port to this node, by PCP,
    /// NAT-PMP or UPnP-IGD (tried in that order), and offers the mapped address
    /// to peers like a public one: a peer can then reach this node through a
    /// symmetric NAT that hole punching can't get through. The mapping is renewed
    /// while the node lives and removed when it is dropped. It runs on a thread of
    /// its own, for IPv4 and dual-stack sockets not bound to loopback. It doesn't run while the
    /// node is under [`Privacy::RelayOnly`], which gives out no address (see
    /// [`PortMapping::RelayOnly`]). See [`Node::port_mapping`]. Default: off.
    /// With a [`policy`](Builder::policy), the policy's `port_mapping` decides instead.
    pub fn port_mapping(mut self, on: bool) -> Self {
        self.port_mapping = on;
        self
    }

    /// Port mapping through a given PCP/NAT-PMP server and SSDP address, for
    /// tests against a fake gateway (feature `testing`).
    #[cfg(any(test, feature = "testing"))]
    #[doc(hidden)]
    pub fn port_mapping_at(mut self, gateway: SocketAddr, ssdp: SocketAddr) -> Self {
        (self.port_mapping, self.portmap_at) = (true, Some((gateway, ssdp)));
        self
    }

    /// Always-on peers to keep linked as relays, by their invites: see
    /// [`Node::set_relay_peers`]. [`bind`](Builder::bind) fails with
    /// [`ErrorKind::InvalidInput`] if one is not an invite. Default: none.
    pub fn relay_peers<S: Into<String>>(mut self, invites: impl IntoIterator<Item = S>) -> Self {
        self.relay_peers = invites.into_iter().map(Into::into).collect();
        self
    }

    /// Which paths links may take, and in what order: see [`Policy`] and [`Node::set_policy`].
    /// [`bind`](Builder::bind) fails with [`ErrorKind::InvalidInput`] if it is not valid. Default: none, every path.
    pub fn policy(mut self, policy: Policy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// This node's own TURN servers, with their credentials ([`TurnServer`]): see [`Node::set_turn_servers`].
    /// Names are resolved in [`bind`](Builder::bind). Default: none.
    #[cfg(feature = "turn")]
    pub fn turn_servers(mut self, servers: impl IntoIterator<Item = TurnServer>) -> Self {
        self.turn = servers.into_iter().collect();
        self
    }

    /// Binds the UDP socket: `"[::]:0"` for any interface, IPv4 and IPv6, and a
    /// free port (see [`Node::bind`]).
    pub fn bind(self, addr: impl ToSocketAddrs) -> io::Result<Node> {
        if self.policy.as_ref().is_some_and(|p| !p.is_valid()) {
            return Err(io::Error::new(ErrorKind::InvalidInput, Error::InvalidPolicy));
        }
        let sock = bind_socket(addr)?;
        sock.set_nonblocking(true)?;
        let local = sock.local_addr()?;
        let dual = local.is_ipv6() && local.ip().is_unspecified();
        let identity = self.identity.unwrap_or_else(|| Identity::generate(&mut OsRng));
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let mut ep = Endpoint::new(&identity, self.config, seed);
        // A socket on loopback can't reach STUN servers anyway. A dual-stack one
        // asks over IPv4 only: its IPv6 address is global already, and answers
        // from both families would read as a symmetric NAT.
        let stun = if local.ip().is_loopback() { Vec::new() } else { resolve(&self.stun, local.is_ipv6() && !dual) };
        OsRng.fill_bytes(&mut seed);
        let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let (local_addrs, stun) = (local_addresses(local).into_iter().map(Hop::Net).collect(), net(stun));
        let mut trav = Traversal::new(&identity, seed, unix_ms, local_addrs, stun);
        trav.set_privacy(&mut ep, self.privacy);
        trav.set_policy(peerwire_core::Instant::ZERO, &mut ep, self.policy);
        #[cfg(feature = "turn")]
        trav.set_turn_servers(peerwire_core::Instant::ZERO, &mut ep, resolve_turn(&self.turn, local));
        let mut node = Node {
            sock,
            local,
            ep,
            trav,
            signaling: self.signaling,
            epoch: Instant::now(),
            status: HashMap::new(),
            events: VecDeque::new(),
            pending: VecDeque::new(),
            buf: Box::new([0; 2048]),
            portmap: None,
            port_mapping: PortMapping::Off, // sync_port_mapping starts it, if asked for and not relay-only
            map_asked: self.port_mapping,
            portmap_at: self.portmap_at,
            releasing: None,
            relay_peers: Vec::new(),
            extra: HashMap::new(),
            routes: HashMap::new(),
            next_extra: 0,
        };
        node.sync_port_mapping();
        node.pump_signals();
        node.set_relay_peers(&self.relay_peers).map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
        Ok(node)
    }
}

/// One identity on one non-blocking UDP socket, driven by [`poll`](Node::poll).
pub struct Node {
    sock: UdpSocket,
    local: SocketAddr,
    ep: Endpoint<Addr>,
    trav: Traversal<Addr>,
    signaling: Option<Box<dyn Signaling>>,
    epoch: Instant,
    status: HashMap<PeerId, Status>,
    events: VecDeque<Event>,
    pending: VecDeque<Transmit<Addr>>,
    buf: Box<[u8; 2048]>,
    portmap: Option<Worker>,
    port_mapping: PortMapping,
    /// [`Builder::port_mapping`]: whether to map with no policy.
    map_asked: bool,
    portmap_at: Option<(SocketAddr, SocketAddr)>,
    /// A worker stopped by a switch to relay-only, deleting its mapping.
    releasing: Option<Worker>,
    relay_peers: Vec<RelayPeer>,
    /// Extra sockets for birthday punching ([`Traversal::poll_birthday`]), by id.
    extra: HashMap<u64, Extra>,
    /// The extra socket that sends to an address: the first to hear from it.
    routes: HashMap<SocketAddr, u64>,
    next_extra: u64,
}

/// An extra socket, opened for birthday punching.
struct Extra {
    sock: UdpSocket,
    /// The peer the punch is to ([`Birthday::peer`]): only a packet of its that
    /// authenticated makes the socket a route, or keeps it one (NB44c-2).
    peer: PeerId,
    /// Closed then, unless it is some address's route.
    until: peerwire_core::Instant,
    /// Last heard from an address it is the route for.
    last_recv: Option<peerwire_core::Instant>,
}

impl Extra {
    fn open(&self, now: peerwire_core::Instant) -> bool {
        match self.last_recv {
            Some(t) => now < t + ROUTE_IDLE,
            None => now < self.until,
        }
    }
}

/// An always-on peer kept linked as a relay ([`Node::set_relay_peers`]).
struct RelayPeer {
    invite: String,
    peer: PeerId,
    /// While unlinked, dial again from then; the wait after that doubles.
    retry_at: Instant,
    wait: Duration,
    /// When the link last came up. The wait goes back to [`RELAY_REDIAL`] only
    /// once a link has held for [`RELAY_REDIAL_MAX`], so a relay that drops
    /// every link at once is dialled less and less often.
    linked_at: Option<Instant>,
}

impl Node {
    /// Binds a UDP socket with a fresh identity, the default [`Config`] and the
    /// [`DEFAULT_STUN_SERVERS`].
    ///
    /// `"[::]:0"` (any interface, a free port) binds one dual-stack socket: the
    /// node offers its IPv4 and its global IPv6 address, and a dial tries both at
    /// once, keeping whichever answers first. That links peers on mobile networks
    /// whose IPv4 is behind a carrier NAT hole punching can't get through, as their
    /// IPv6 has no NAT. On a host without IPv6 it binds `0.0.0.0` on the same port
    /// instead. `"0.0.0.0:0"` binds IPv4 only; a given address binds that one.
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Node> {
        Node::builder().bind(addr)
    }

    /// Binds with a given identity (to keep the same [`PeerId`] across runs) and config.
    pub fn bind_with(addr: impl ToSocketAddrs, identity: Identity, config: Config) -> io::Result<Node> {
        Node::builder().identity(identity).config(config).bind(addr)
    }

    /// Options: identity, config, STUN servers, signaling, privacy, router port
    /// mapping, relay peers and the paths links take. See [`Builder`].
    pub fn builder() -> Builder {
        Builder {
            identity: None,
            config: Config::default(),
            stun: DEFAULT_STUN_SERVERS.iter().map(|s| s.to_string()).collect(),
            signaling: None,
            privacy: Privacy::Direct,
            port_mapping: false,
            portmap_at: None,
            relay_peers: Vec::new(),
            policy: None,
            #[cfg(feature = "turn")]
            turn: Vec::new(),
        }
    }

    /// This node's identity, and the key every peer authenticates it by.
    pub fn peer_id(&self) -> PeerId {
        self.ep.peer_id()
    }

    /// The address the socket is bound to (`0.0.0.0` when `[::]` fell back to IPv4).
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// An `pw:` invite with this node's [`addresses`](Node::addresses) (under
    /// [`Privacy::RelayOnly`], none: its relay instead, for joiners linked to it).
    /// It also carries this node's invite key: a holder may ask it, over
    /// signaling, to dial back. Share it with those you want to link with.
    pub fn invite(&self) -> String {
        let relays = match self.trav.relay() {
            Some(r) => self.ep.path(&r).and_then(|p| p.direct()).map(|a| (r, *a.addr())).into_iter().collect(),
            None => Vec::new(),
        };
        Invite { peer: self.peer_id(), key: Some(self.trav.invite_key()), addrs: self.addresses(), relays }.to_link()
    }

    /// Which addresses this node gives out.
    pub fn privacy(&self) -> Privacy {
        self.trav.privacy()
    }

    /// Changes which addresses this node gives out, from the next offer and
    /// invite on. Links already up are kept, on the path they have: to stop a
    /// peer knowing an address it already has, disconnect it. The exception is
    /// [relay peers](Node::set_relay_peers), which the app can't see: switching
    /// to [`Privacy::RelayOnly`] closes their links (unless one is the relay it
    /// names), and switching out of it dials them again.
    ///
    /// With [`Builder::port_mapping`] on, switching to [`Privacy::RelayOnly`]
    /// removes the router mapping (in the background), and switching out of it
    /// asks the router again; that waits, like dropping the node, up to 7 s for
    /// the last mapping's removal if the router stalls.
    pub fn set_privacy(&mut self, privacy: Privacy) {
        self.trav.set_privacy(&mut self.ep, privacy);
        if matches!(privacy, Privacy::RelayOnly { .. }) {
            let own = self.trav.relay();
            for r in self.relay_peers.iter().filter(|r| Some(r.peer) != own) {
                self.ep.disconnect(&r.peer);
                self.status.remove(&r.peer);
            }
        }
        self.sync_port_mapping();
    }

    /// Runs router port mapping while it is asked for (by the policy, with `Direct` on, or with none by
    /// [`Builder::port_mapping`]), and only while not relay-only.
    fn sync_port_mapping(&mut self) {
        let asked = match self.trav.policy() {
            Some(p) => p.port_mapping && p.allows(PathKind::Direct),
            None => self.map_asked,
        };
        if asked && !matches!(self.privacy(), Privacy::RelayOnly { .. }) {
            if matches!(self.port_mapping, PortMapping::Off | PortMapping::RelayOnly) {
                // The old mapping's delete goes out before the new request.
                drop(self.releasing.take());
                self.portmap = start_port_mapping(self.local, self.portmap_at);
                self.port_mapping = if self.portmap.is_some() { PortMapping::Trying } else { PortMapping::Unavailable };
            }
            return;
        }
        if let Some(w) = self.portmap.take() {
            w.stop();
            self.releasing = Some(w);
        }
        self.port_mapping = if asked { PortMapping::RelayOnly } else { PortMapping::Off };
        self.trav.set_mapped(None);
    }

    /// The paths this node's links may take, if a policy is set ([`set_policy`](Node::set_policy)).
    pub fn policy(&self) -> Option<&Policy> {
        self.trav.policy()
    }

    /// Sets which paths links may take, and in what order ([`Policy`]); `None` takes every path, as with no
    /// policy. New dials follow it at once, and dials under way take their next steps by it.
    ///
    /// A link on a path it turns off moves to one still allowed. A link through a relay punches to a direct path
    /// (with `Direct` on). One with an allowed relay linked to both is dialled again through it: the old link
    /// carries traffic until the new one replaces it, which raises [`Event::Disconnected`] with
    /// [`DisconnectReason::Replaced`] and then [`Event::Connected`], at both ends. A link that can't move, or
    /// hasn't within 6 s, is closed: [`Event::Disconnected`] with [`DisconnectReason::PathOff`]. A new link on a
    /// path the policy has off is closed unseen, with whatever came in with it; if it replaced a link (the peer
    /// restarted, say), that link's [`Event::Disconnected`] says [`DisconnectReason::PathOff`], not `Replaced`. Leaving
    /// [`PathKind::RelayPeers`] out closes the links to [relay peers](Node::set_relay_peers) (they are dialled again
    /// once it is back in), and the policy's `port_mapping` starts or stops router port mapping, as
    /// [`Builder::port_mapping`] does with no policy.
    ///
    /// Fails, changing nothing, with [`Error::InvalidPolicy`] for an empty order or one that lists a kind twice.
    pub fn set_policy(&mut self, policy: impl Into<Option<Policy>>) -> Result<(), Error> {
        let policy = policy.into();
        if policy.as_ref().is_some_and(|p| !p.is_valid()) {
            return Err(Error::InvalidPolicy);
        }
        let now = self.now();
        self.trav.set_policy(now, &mut self.ep, policy);
        if !self.relay_peers_on() {
            let own = self.trav.relay();
            let closed: Vec<PeerId> = self.relay_peers.iter().map(|r| r.peer).filter(|p| Some(*p) != own).collect();
            for peer in closed {
                self.ep.disconnect(&peer);
                self.status.remove(&peer);
            }
        }
        self.sync_port_mapping();
        self.poll();
        Ok(())
    }

    /// Whether relay peers may be linked: the policy, if any, has [`PathKind::RelayPeers`] in.
    fn relay_peers_on(&self) -> bool {
        self.trav.policy().is_none_or(|p| p.allows(PathKind::RelayPeers))
    }

    /// Replaces the STUN servers ([`Builder::stun_servers`]; empty turns STUN off) and asks the new ones at once.
    /// Names are resolved now, on this thread, as [`bind`](Builder::bind) does. Links stay up: the servers only
    /// tell this node its public address and NAT type, which its next offers and invites carry.
    pub fn set_stun_servers<S: Into<String>>(&mut self, servers: impl IntoIterator<Item = S>) {
        let names: Vec<String> = servers.into_iter().map(Into::into).collect();
        let dual = self.local.is_ipv6() && self.local.ip().is_unspecified();
        let stun =
            if self.local.ip().is_loopback() { Vec::new() } else { resolve(&names, self.local.is_ipv6() && !dual) };
        self.trav.set_stun_servers(net(stun));
        self.poll();
    }

    /// Replaces this node's own TURN servers ([`Builder::turn_servers`]; empty: none); names are resolved now.
    ///
    /// TURN runs only with [`PathKind::Turn`] in the [policy](Node::set_policy), and not under
    /// [`Privacy::RelayOnly`]. Then this node keeps an allocation on each server, offers its relayed address to
    /// peers (in an entry nodes without TURN skip), and a dial's `Turn` step goes through it. A link on TURN moves
    /// to a `Direct` or `Birthday` path the policy ranks earlier once one answers, never to a relay peer.
    ///
    /// An allocation stays on a server kept with the same credentials; the others are released, and links through
    /// them move to a path the policy allows, or close with [`DisconnectReason::PathOff`]. New credentials for a
    /// server (renewing short-lived ones before they expire) make a new allocation, at a new relayed address.
    ///
    /// One server can be both STUN and TURN, as coturn is: list it in [`stun_servers`](Builder::stun_servers) too, and
    /// its Binding answers reach STUN as any STUN server's do.
    #[cfg(feature = "turn")]
    pub fn set_turn_servers(&mut self, servers: impl IntoIterator<Item = TurnServer>) {
        let servers: Vec<TurnServer> = servers.into_iter().collect();
        let now = self.now();
        self.trav.set_turn_servers(now, &mut self.ep, resolve_turn(&servers, self.local));
        self.poll();
    }

    /// Each of this node's TURN servers, as resolved, and the relayed address of its allocation there once made.
    #[cfg(feature = "turn")]
    pub fn turn_status(&self) -> Vec<(SocketAddr, Option<SocketAddr>)> {
        self.trav.turn_status().into_iter().map(|(a, r)| (*a.addr(), r)).collect()
    }

    /// Swaps the signaling carrier ([`Builder::signaling`]), to change relays, say: the old one is dropped, and on
    /// the new one this node subscribes to its inbox and its room's topic and says hello in the room at once. Links
    /// stay up. Peers meet over it from then on, so those in the room must move to it too, or share it.
    pub fn set_signaling(&mut self, signaling: impl Signaling + 'static) {
        self.signaling = Some(Box::new(signaling));
        let now = self.now();
        self.trav.resubscribe(now);
        self.pump_signals();
    }

    /// Addresses a peer may reach this node at, as put in [`invite`](Node::invite),
    /// in this order: the public address from STUN, the public port the router
    /// forwards ([`Builder::port_mapping`]), the last 4 addresses linked peers
    /// reported seeing it at, and the bound address (or, when bound to any
    /// interface, the LAN and loopback addresses). At most 8, and only those the
    /// [`Privacy`] mode allows.
    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.trav.candidates().iter().map(|a| *a.addr()).collect()
    }

    /// The public address STUN reported for this socket, once a server has answered.
    pub fn public_addr(&self) -> Option<SocketAddr> {
        self.trav.public_addr().map(|a| *a.addr())
    }

    /// What STUN says about this node's NAT.
    pub fn nat_type(&self) -> NatType {
        self.trav.nat_type()
    }

    /// How router port mapping ([`Builder::port_mapping`]) stands: the mapped
    /// address and the protocol that made it, once the router agreed. Updated by
    /// [`poll`](Node::poll).
    pub fn port_mapping(&self) -> PortMapping {
        self.port_mapping
    }

    /// Dials the peer named in an invite, at every address it lists (and through any
    /// listed relay this node is already connected to). With a signaling carrier it
    /// also sends the peer this node's addresses, so the peer dials back and the
    /// handshake punches through NATs on both sides. Returns the peer's id; an
    /// [`Event::Connected`] or [`Event::ConnectFailed`] follows from [`poll`](Node::poll).
    ///
    /// Under [`Privacy::RelayOnly`] it dials through the relay only (the relay
    /// itself directly), and fails with [`ConnectError::NoRelay`] while no relay
    /// is connected: it never falls back to a direct path.
    ///
    /// Under a [`Policy`] it dials the paths the policy allows, step by step, and
    /// fails with [`ConnectError::NoPath`] if none has anything to dial.
    pub fn connect(&mut self, invite: &str) -> Result<PeerId, Error> {
        let inv: Invite<SocketAddr> = Invite::parse(invite).ok_or(Error::InvalidInvite)?;
        let reaches = |a: &&SocketAddr| self.trav.reaches(&Hop::Net(**a));
        let direct = |a: &SocketAddr| Path::Direct(Hop::Net(*a));
        let mut targets: Vec<Path<Addr>> = inv.addrs.iter().filter(reaches).map(direct).collect();
        for (relay, addr) in &inv.relays {
            if self.ep.is_connected(relay) {
                targets.push(Path::Via(*relay));
                targets.extend(Some(addr).filter(reaches).map(direct));
            }
        }
        let relay = self.trav.relay();
        if matches!(self.privacy(), Privacy::RelayOnly { .. }) && relay != Some(inv.peer) && inv.peer != self.peer_id()
        {
            let r = relay.filter(|r| self.ep.is_connected(r)).ok_or(Error::Connect(ConnectError::NoRelay))?;
            targets = vec![Path::Via(r)];
        }
        let now = self.now();
        let relay_only = matches!(self.privacy(), Privacy::RelayOnly { .. });
        if self.trav.policy().is_some() && !relay_only && !self.is_relay_peer(&inv.peer) {
            let (mut addrs, mut vias) = (Vec::new(), Vec::new());
            for t in targets {
                match t {
                    Path::Direct(a) => addrs.push(a),
                    Path::Via(r) => vias.push(r),
                }
            }
            self.trav.dial_invite(now, &mut self.ep, inv.peer, addrs, vias).map_err(Error::Connect)?;
        } else {
            self.ep.connect_with(now, inv.peer, &targets).map_err(Error::Connect)?;
        }
        if !self.ep.is_connected(&inv.peer) {
            self.status.insert(inv.peer, Status::Connecting);
            // Also marks the peer as welcome while in a room (the offer is dropped with no carrier).
            self.trav.introduce(now, &self.ep, inv.peer, inv.key);
            self.pump_signals();
        }
        Ok(inv.peer)
    }

    /// Keeps a link to each of these always-on peers (by their invites, such as
    /// the one `peerwire-peer` prints), dialling one again when its link drops or
    /// a dial fails, so that peers this node can't punch to (both behind
    /// symmetric NATs, say) still link with it, through one both are linked to
    /// ([`Path::Via`]). Offers name them first among this node's links. Direct
    /// paths are still dialled too, and a relayed link moves to a direct path
    /// when one turns out to work. Replaces the last list (links to peers no
    /// longer on it are closed); empty stops. Fails, changing nothing, if one is
    /// not an invite.
    ///
    /// Relay links carry no app traffic: they raise no [`Event`]s and are not in
    /// [`peers`](Node::peers); [`relay_peers`](Node::relay_peers) shows them.
    /// A room admits them, as peers this node dialled. Not dialled under
    /// [`Privacy::RelayOnly`], which links through its own relay only.
    pub fn set_relay_peers<S: AsRef<str>>(&mut self, invites: &[S]) -> Result<(), Error> {
        let mut new: Vec<RelayPeer> = Vec::new();
        for invite in invites {
            let invite = invite.as_ref().trim();
            let inv: Invite<SocketAddr> = Invite::parse(invite).ok_or(Error::InvalidInvite)?;
            if inv.peer != self.peer_id() && new.iter().all(|r| r.peer != inv.peer) {
                let now = Instant::now();
                let (retry_at, wait, linked_at) = (now, RELAY_REDIAL, None);
                new.push(RelayPeer { invite: invite.to_string(), peer: inv.peer, retry_at, wait, linked_at });
            }
        }
        for old in std::mem::take(&mut self.relay_peers) {
            if new.iter().all(|r| r.peer != old.peer) {
                self.ep.disconnect(&old.peer);
                self.status.remove(&old.peer);
            }
        }
        self.trav.set_relay_peers(new.iter().map(|r| r.peer).collect());
        self.relay_peers = new;
        self.poll(); // dials them
        Ok(())
    }

    /// The relay peers set with [`set_relay_peers`](Node::set_relay_peers), and
    /// where this node stands with each.
    pub fn relay_peers(&self) -> Vec<(PeerId, Status)> {
        self.relay_peers.iter().map(|r| (r.peer, self.status(&r.peer))).collect()
    }

    fn is_relay_peer(&self, peer: &PeerId) -> bool {
        self.relay_peers.iter().any(|r| r.peer == *peer)
    }

    /// Dials the relay peers that are neither linked nor being dialled, once their wait is over.
    fn keep_relay_links(&mut self, now: Instant) {
        if matches!(self.privacy(), Privacy::RelayOnly { .. }) || !self.relay_peers_on() {
            return;
        }
        for i in 0..self.relay_peers.len() {
            let (peer, at) = (self.relay_peers[i].peer, self.relay_peers[i].retry_at);
            if self.status(&peer) == Status::Connected {
                let r = &mut self.relay_peers[i];
                if r.linked_at.is_some_and(|t| now.saturating_duration_since(t) >= RELAY_REDIAL_MAX) {
                    r.wait = RELAY_REDIAL;
                }
            }
            if self.status(&peer) != Status::Disconnected || now < at {
                continue;
            }
            let invite = self.relay_peers[i].invite.clone();
            let _ = self.connect(&invite);
            let r = &mut self.relay_peers[i];
            r.retry_at = now + r.wait;
            r.wait = (r.wait * 2).min(RELAY_REDIAL_MAX);
        }
    }

    /// Joins room `room` of application `app_id` on the signaling carrier, as a mesh
    /// [`Role::Peer`]: this node connects to every other peer in it, and keeps
    /// announcing itself for those that join later. Leaves any other room. Needs a
    /// [`signaling`](Builder::signaling) carrier; without one nobody hears it.
    ///
    /// The room's name is its password: only peers that know it can link in it
    /// (relays see just a hash). While in a room, this node closes links from
    /// anyone else, except peers it dialled itself with [`connect`](Node::connect).
    /// A name others can guess makes a public lobby.
    pub fn join_room(&mut self, app_id: &str, room: &str) {
        self.join_room_as(app_id, room, Role::Peer);
    }

    /// [`join_room`](Node::join_room) with a role: a [`Role::Host`] links with clients
    /// and peers, a [`Role::Client`] with hosts only (a star, as a game server).
    pub fn join_room_as(&mut self, app_id: &str, room: &str, role: Role) {
        self.join_room_with(app_id, room, role, None);
    }

    /// [`join_room_as`](Node::join_room_as), linking with `host` only when given: a
    /// client that knows its host's key can't be taken in by another room member.
    pub fn join_room_with(&mut self, app_id: &str, room: &str, role: Role, host: Option<PeerId>) {
        let now = self.now();
        self.trav.join_room(now, app_id, room, role, host);
        self.pump_signals();
    }

    /// The role `peer` proved it holds in the current room (by knowing its name).
    pub fn room_role(&self, peer: &PeerId) -> Option<Role> {
        self.trav.room_role(peer)
    }

    /// Leaves the room: stops announcing and dialling. Links stay up.
    pub fn leave_room(&mut self) {
        self.trav.leave_room();
        self.pump_signals();
    }

    /// Why the last dial to `peer` failed, after an [`Event::ConnectFailed`]. Its
    /// `Display` is a sentence for users, naming the symmetric side if there is one.
    pub fn failure(&self, peer: &PeerId) -> Option<Failure> {
        self.trav.failure(peer)
    }

    /// Queues a message for `peer`; it goes out on the next [`poll`](Node::poll). Never
    /// blocks: when the reliable send buffer is full it returns
    /// [`SendError::WouldBlock`], and the unreliable lane drops its oldest message.
    pub fn send(&mut self, peer: &PeerId, lane: Lane, data: &[u8]) -> Result<(), SendError> {
        self.ep.send(peer, lane, data)
    }

    /// Closes the link to `peer`, telling it so. No event follows locally.
    pub fn disconnect(&mut self, peer: &PeerId) {
        self.ep.disconnect(peer);
        self.status.remove(peer);
        self.poll();
    }

    /// Where this node stands with `peer`.
    pub fn status(&self, peer: &PeerId) -> Status {
        self.status.get(peer).copied().unwrap_or(Status::Disconnected)
    }

    /// Peers with a live link, except [relay peers](Node::set_relay_peers).
    pub fn peers(&self) -> impl Iterator<Item = &PeerId> {
        self.ep.connected_peers().filter(|p| !self.is_relay_peer(p))
    }

    /// RTT and loss for a connected peer.
    pub fn peer_stats(&self, peer: &PeerId) -> Option<PeerStats> {
        self.ep.peer_stats(peer)
    }

    /// The route a connected peer's link takes: through a relaying peer, or straight to an address, which is where
    /// its packets go. A link through TURN shows the peer's address as the TURN server relays to it (its own, or
    /// the peer's relayed address); [`path_kind`](Node::path_kind) says `Turn`.
    pub fn path(&self, peer: &PeerId) -> Option<crate::Path> {
        self.ep.path(peer).map(plain)
    }

    /// The kind of route a connected peer's link takes ([`PathKind`]): `Direct`, `Birthday` (out of one of this
    /// node's birthday punch sockets, [`via_birthday`](Node::via_birthday)), `Peers` or `RelayPeers` (through
    /// another peer), or `Turn` (through a TURN server: this node's own, or the peer's relayed address).
    pub fn path_kind(&self, peer: &PeerId) -> Option<PathKind> {
        Some(match self.ep.path(peer)? {
            Path::Via(r) if self.is_relay_peer(r) => PathKind::RelayPeers,
            Path::Via(_) => PathKind::Peers,
            Path::Direct(Hop::Net(_)) if self.via_birthday(peer) => PathKind::Birthday,
            Path::Direct(Hop::Net(_)) => PathKind::Direct,
            Path::Direct(_) => PathKind::Turn,
        })
    }

    /// Whether the direct link to `peer` goes out of one of this node's birthday
    /// punch sockets ([`Traversal::poll_birthday`]): the peer reached one, so the
    /// punch made the link. On the peer's side, which probed for it from its main
    /// socket, this is false.
    pub fn via_birthday(&self, peer: &PeerId) -> bool {
        match self.ep.path(peer) {
            Some(Path::Direct(Hop::Net(a))) => self.routes.get(a).is_some_and(|id| self.extra.contains_key(id)),
            _ => false,
        }
    }

    /// How many sockets this node has opened for birthday punching.
    pub fn birthday_sockets_opened(&self) -> u64 {
        self.next_extra
    }

    /// Node-wide counters (packets, drops, relaying, hole punching).
    pub fn stats(&self) -> Stats {
        self.ep.stats()
    }

    /// Does all pending work without blocking: reads every datagram waiting on the
    /// socket and every signaling message, fires due timers, sends what is queued
    /// and queues the resulting events. Call it often (every frame, or when
    /// [`next_timeout`](Node::next_timeout) expires or the socket is readable).
    pub fn poll(&mut self) {
        self.poll_at(Instant::now());
    }

    /// [`poll`](Node::poll) with the time supplied by the caller (it must never go backwards).
    pub fn poll_at(&mut self, now: Instant) {
        self.poll_drained(now);
    }

    /// [`poll_at`](Node::poll_at), returning whether the socket was read until empty
    /// (`false` when the per-poll read cap stopped it first).
    pub(crate) fn poll_drained(&mut self, now: Instant) -> bool {
        let wall = now;
        let now = self.at(now);
        if let Some(s) = self.portmap.as_ref().and_then(|w| w.poll()) {
            self.port_mapping = s;
            self.trav.set_mapped(s.addr().map(Hop::Net));
        }
        let mut drained = false;
        for _ in 0..MAX_READS_PER_POLL {
            match self.sock.recv_from(&mut self.buf[..]) {
                Ok((n, from)) => {
                    // A dual-stack socket sees IPv4 peers as ::ffff:a.b.c.d.
                    let from = Hop::Net(SocketAddr::new(from.ip().to_canonical(), from.port()));
                    // From a TURN server of ours: what a peer sent through it, or its answers to TURN requests (its
                    // Binding answers, as a STUN server of ours too, go on to STUN).
                    match self.trav.handle_turn(now, &from, &self.buf[..n]) {
                        Some(Some((hop, data))) => self.ep.handle_datagram(now, hop, &data),
                        Some(None) => {}
                        None if self.trav.handle_stun(&from, &self.buf[..n]) => {}
                        None => self.ep.handle_datagram(now, self.trav.hop(from), &self.buf[..n]),
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    drained = true;
                    break;
                }
                // ICMP errors surface here on some platforms (ConnectionReset): keep reading.
                Err(_) => continue,
            }
        }
        self.read_extra(now);
        if let Some(sig) = self.signaling.as_mut() {
            for _ in 0..MAX_SIGNALS_PER_POLL {
                let Some((topic, payload)) = sig.poll() else { break };
                self.trav.handle_signal(now, &mut self.ep, topic, &payload);
            }
        }
        if self.ep.poll_timeout().is_some_and(|t| t <= now) {
            self.ep.handle_timeout(now);
        }
        if self.trav.poll_timeout().is_some_and(|t| t <= now) {
            self.trav.handle_timeout(now, &mut self.ep);
        }
        while let Some(b) = self.trav.poll_birthday() {
            self.open_extra(&b);
        }
        self.close_extra(now);
        // The peers whose new link was refused below: what came in with it is dropped too, until a link to the peer
        // is admitted (NB97-1). Those still refused after the pass are closed then.
        let mut refused: Vec<PeerId> = Vec::new();
        // A new link's first packet raises its `AddrObserved`, and the `Replaced` of a link it replaces, before its
        // `Connected`: they wait for the next event, which says whether that link is admitted.
        let mut held: Vec<Ev<Addr>> = Vec::new();
        while let Some(e) = self.ep.poll_event() {
            match e {
                Ev::AddrObserved { .. } | Ev::Disconnected { reason: DisconnectReason::Replaced, .. } => held.push(e),
                // In a room, a link from a peer that never proved the room key, a relay-only node's new direct link
                // to anyone but its relay, or a link on a path the policy has off: closed unseen, with what came in
                // with it. A link it replaced was closed for its path (NB97-I1).
                Ev::Connected { peer, .. } if !self.admits(&peer, &e) => {
                    for h in std::mem::take(&mut held) {
                        match h {
                            Ev::Disconnected { peer: p, .. } if p == peer => {
                                let off = Ev::Disconnected { peer, reason: DisconnectReason::PathOff };
                                self.take_event(now, wall, off, &refused);
                            }
                            h if about(&h) == Some(peer) => {}
                            h => self.take_event(now, wall, h, &refused),
                        }
                    }
                    refused.push(peer);
                }
                e => {
                    if let Ev::Connected { peer, .. } = e {
                        if refused.contains(&peer) {
                            // It replaced the refused link, which the app never saw (NB97-I4).
                            held.retain(|h| !matches!(h, Ev::Disconnected { peer: p, .. } if *p == peer));
                        }
                        refused.retain(|p| *p != peer);
                    }
                    for h in std::mem::take(&mut held) {
                        self.take_event(now, wall, h, &refused);
                    }
                    self.take_event(now, wall, e, &refused);
                }
            }
        }
        for h in held {
            self.take_event(now, wall, h, &refused);
        }
        // Closed only now: `disconnect` closes the peer's link as the core has it, which is a later one if a link
        // announced after the refused one replaced it, and a peer still here has had no link admitted since (NB97-I4).
        for p in &refused {
            self.ep.disconnect(p);
        }
        self.report_dropped();
        self.keep_relay_links(wall);
        self.flush(now);
        self.pump_signals();
        drained
    }

    /// Hands on an event of the core's: to the traversal and the node, then to the app unless it is the node's own
    /// business. Dropped if it is about a peer in `refused` (see `poll_drained`), or one whose link isn't up and
    /// isn't admitted (see `admits`).
    fn take_event(&mut self, now: peerwire_core::Instant, wall: Instant, e: Ev<Addr>, refused: &[PeerId]) {
        let unseen = |p: &PeerId| refused.contains(p) || (self.status(p) != Status::Connected && !self.admits(p, &e));
        if about(&e).is_some_and(|p| unseen(&p)) {
            return;
        }
        // A dial that moved a link off a path the policy turned off failed: the node's own.
        let own = matches!(&e, Ev::ConnectFailed { peer } if self.trav.redialing(peer));
        self.trav.handle_event(now, &mut self.ep, &e);
        if own {
            return;
        }
        self.track(&e);
        let subject = match &e {
            Ev::AddrObserved { by, .. } => by,
            Ev::Connected { peer, .. }
            | Ev::Message { peer, .. }
            | Ev::Disconnected { peer, .. }
            | Ev::PathChanged { peer, .. }
            | Ev::ConnectFailed { peer } => peer,
        };
        if let Some(r) = self.relay_peers.iter_mut().find(|r| r.peer == *subject) {
            if matches!(e, Ev::Connected { .. }) {
                r.linked_at = Some(wall);
            }
            return; // a relay link is the node's own business, not the app's
        }
        self.events.push_back(plain_event(e));
    }

    /// Reports the links the policy closed ([`Traversal::poll_dropped`]): [`Event::Disconnected`] with
    /// [`DisconnectReason::PathOff`].
    fn report_dropped(&mut self) {
        while let Some(peer) = self.trav.poll_dropped() {
            self.status.remove(&peer);
            if !self.is_relay_peer(&peer) {
                self.events.push_back(Ev::Disconnected { peer, reason: DisconnectReason::PathOff });
            }
        }
    }

    /// The next event, if any is queued.
    pub fn try_recv_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// How long until a timer needs [`poll`](Node::poll), if one is armed. A host that
    /// sleeps should wake at this or when the socket is readable, whichever is first.
    /// With a signaling carrier it is at most 50 ms, as carriers are polled.
    pub fn next_timeout(&self) -> Option<Duration> {
        let wall = Instant::now();
        let now = self.at(wall);
        let t = [self.ep.poll_timeout(), self.trav.poll_timeout()].into_iter().flatten().min();
        let t = t.map(|t| t.saturating_duration_since(now));
        let redial = self.relay_peers.iter().filter(|r| self.status(&r.peer) == Status::Disconnected);
        let redial = redial.map(|r| r.retry_at.saturating_duration_since(wall)).min();
        let redial = redial.filter(|_| !matches!(self.privacy(), Privacy::RelayOnly { .. }) && self.relay_peers_on());
        let extra = (!self.extra.is_empty()).then_some(EXTRA_POLL);
        let t = [t, redial, extra].into_iter().flatten().min();
        match self.signaling {
            Some(_) => Some(t.map_or(SIGNAL_POLL, |t| t.min(SIGNAL_POLL))),
            None => t,
        }
    }

    /// The UDP socket, for hosts that wait on its readiness (it is non-blocking).
    /// While a birthday punch keeps extra sockets open (see
    /// [`Traversal::poll_birthday`]), [`next_timeout`](Node::next_timeout) is at most
    /// 5 ms, so a host waiting on this socket still reads them.
    pub fn socket(&self) -> &UdpSocket {
        &self.sock
    }

    fn now(&self) -> peerwire_core::Instant {
        self.at(Instant::now())
    }

    fn at(&self, now: Instant) -> peerwire_core::Instant {
        peerwire_core::Instant::from_micros(now.saturating_duration_since(self.epoch).as_micros() as u64)
    }

    fn flush(&mut self, now: peerwire_core::Instant) {
        while let Some(t) = self.pending.pop_front() {
            if !self.try_send(&t) {
                self.pending.push_front(t);
                return;
            }
        }
        while let Some(t) = self.ep.poll_transmit(now).or_else(|| self.trav.poll_transmit()) {
            if let Hop::Turn { .. } = t.dst {
                self.trav.send_turn(now, &t.dst, &t.payload); // through the allocation: to its server, next
                continue;
            }
            if !self.try_send(&t) {
                if self.pending.len() >= MAX_PENDING_SENDS {
                    self.pending.pop_front();
                }
                self.pending.push_back(t);
            }
        }
    }

    /// Whether an event about a not yet connected `peer` belongs (see `poll_drained`).
    fn admits(&self, peer: &PeerId, e: &Ev<Addr>) -> bool {
        let direct = matches!(e, Ev::Connected { path: Path::Direct(_), .. });
        let relay_only = matches!(self.privacy(), Privacy::RelayOnly { .. });
        let allowed = match e {
            Ev::Connected { path, .. } => self.trav.allows(peer, path),
            _ => true,
        };
        self.trav.admits(peer) && allowed && !(relay_only && direct && self.trav.relay() != Some(*peer))
    }

    /// Hands the traversal's signaling actions to the carrier (dropped without one).
    fn pump_signals(&mut self) {
        while let Some(s) = self.trav.poll_signal() {
            let Some(sig) = self.signaling.as_mut() else { continue };
            match s {
                Signal::Subscribe(t) => sig.subscribe(t),
                Signal::Unsubscribe(t) => sig.unsubscribe(t),
                Signal::Publish(t, payload) => sig.publish(t, &payload),
            }
        }
    }

    /// `false` only when the socket's send buffer is full. Goes out of the extra
    /// socket that is `t.dst`'s route, if any, else the main one.
    fn try_send(&self, t: &Transmit<Addr>) -> bool {
        let route = match &t.dst {
            Hop::Net(a) => self.routes.get(a),
            _ => None,
        };
        let sock = route.and_then(|id| self.extra.get(id)).map_or(&self.sock, |x| &x.sock);
        match sock.send_to(&t.payload, self.wire_addr(*t.dst.addr())) {
            Ok(_) => true,
            Err(e) => e.kind() != ErrorKind::WouldBlock, // unreachable addresses etc.: drop it
        }
    }

    /// `dst` as the socket takes it: IPv4 as `::ffff:a.b.c.d` on an IPv6 socket.
    fn wire_addr(&self, dst: SocketAddr) -> SocketAddr {
        match dst {
            SocketAddr::V4(a) if self.local.is_ipv6() => SocketAddr::new(a.ip().to_ipv6_mapped().into(), a.port()),
            dst => dst,
        }
    }

    /// Opens a birthday punch's sockets, on the main socket's address but any
    /// port, and sends its payload from each (see [`Traversal::poll_birthday`]).
    fn open_extra(&mut self, b: &Birthday<Addr>) {
        let any = SocketAddr::new(self.local.ip(), 0);
        let n = (b.sockets as usize).min(MAX_EXTRA_SOCKETS.saturating_sub(self.extra.len()));
        let mut opened: Vec<u64> = Vec::new();
        for _ in 0..n {
            let sock = match bind_socket(any) {
                Ok(s) => s,
                // NB44-3: out of file descriptors (the macOS default soft limit is
                // 256). Don't sit at the ceiling for the socket's lifetime, which
                // would make the rest of the process fail with EMFILE: release a
                // reserve of the sockets this punch just opened and stop.
                Err(_) => {
                    for id in opened.iter().rev().take(FD_RESERVE) {
                        self.extra.remove(id);
                    }
                    break;
                }
            };
            if sock.set_nonblocking(true).is_err() {
                continue;
            }
            let _ = sock.send_to(&b.payload, self.wire_addr(*b.dst.addr()));
            self.next_extra += 1;
            self.extra.insert(self.next_extra, Extra { sock, peer: b.peer, until: b.until, last_recv: None });
            opened.push(self.next_extra);
        }
    }

    /// Hands the endpoint what the extra sockets heard. The first open one to hear
    /// an authenticated packet of its punch's peer from an address becomes its route;
    /// for an HS1, which may be a copy, only until the socket's `until` (NB85-1).
    fn read_extra(&mut self, now: peerwire_core::Instant) {
        let ids: Vec<u64> = self.extra.keys().copied().collect();
        for id in ids {
            for _ in 0..MAX_READS_PER_POLL {
                let Some(x) = self.extra.get(&id) else { break };
                let peer = x.peer;
                let (n, from) = match x.sock.recv_from(&mut self.buf[..]) {
                    Ok(r) => r,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => continue,
                };
                let from = SocketAddr::new(from.ip().to_canonical(), from.port());
                if peerwire_core::stun::is_stun(&self.buf[..n]) {
                    continue; // STUN is asked on the main socket only
                }
                // NB44-2, NB44c: adopt `from` as a route only once the datagram has
                // authenticated (its crypto ran) to a session of the punch's peer on
                // this direct path. An unauthenticated or spoofed source, a handshake
                // packet sent again, or another peer otherwise made itself a route,
                // redirecting our egress and holding the socket open; now the
                // endpoint just has the datagram.
                let Some((who, fresh)) = self.ep.handle_datagram_direct(now, Hop::Net(from), &self.buf[..n]) else {
                    continue;
                };
                if who != peer {
                    continue;
                }
                if self.routes.len() >= MAX_ROUTES && !self.routes.contains_key(&from) {
                    continue; // bound the map (NB44-2); real routes never reach this
                }
                let open = |r: &u64| self.extra.get(r).is_some_and(|x| x.open(now));
                let route = self.routes.get(&from).copied().filter(open).unwrap_or(id);
                if !fresh {
                    // NB85-1: an HS1, which anyone holding a copy can send once its
                    // handshake is no longer half-open. It routes its answer, the HS2
                    // or our HS1 sent again, but keeps no socket past its `until` and
                    // ends no other route; the peer's next fresh packet adopts one.
                    self.routes.insert(from, route);
                    continue;
                }
                // NB44c-2: one route per peer. Its route at another address goes, and
                // its other sockets close at their `until`, so a peer that sends from
                // one source port after another holds one socket, not one per port.
                let extra = &mut self.extra;
                self.routes.retain(|a, r| *a == from || extra.get(r).is_none_or(|x| x.peer != peer));
                self.routes.insert(from, route);
                for (&i, x) in extra.iter_mut() {
                    if i == route && route == id {
                        x.last_recv = Some(now);
                    } else if i != route && x.peer == peer {
                        x.last_recv = None;
                    }
                }
            }
        }
    }

    /// Closes the extra sockets that are no route by their `until`, and routes
    /// that have heard nothing for [`ROUTE_IDLE`].
    fn close_extra(&mut self, now: peerwire_core::Instant) {
        if self.extra.is_empty() {
            return;
        }
        self.extra.retain(|_, x| x.open(now));
        let extra = &self.extra;
        self.routes.retain(|_, id| extra.contains_key(id));
    }

    /// The local addresses of the open extra sockets, and how many are routes.
    #[cfg(test)]
    fn extra_sockets(&self) -> (Vec<SocketAddr>, usize) {
        let addrs = self.extra.values().filter_map(|x| x.sock.local_addr().ok()).collect();
        (addrs, self.extra.values().filter(|x| x.last_recv.is_some()).count())
    }

    /// Entries in the route map.
    #[cfg(test)]
    fn route_count(&self) -> usize {
        self.routes.len()
    }

    fn track(&mut self, e: &Ev<Addr>) {
        match e {
            Ev::Connected { peer, .. } => {
                self.status.insert(*peer, Status::Connected);
            }
            Ev::Disconnected { peer, .. } | Ev::ConnectFailed { peer } => {
                self.status.remove(peer);
            }
            Ev::Message { .. } | Ev::PathChanged { .. } | Ev::AddrObserved { .. } => {}
        }
    }
}

/// The peer `e` is about; none for a failed dial.
fn about<A>(e: &Ev<A>) -> Option<PeerId> {
    match e {
        Ev::Connected { peer, .. }
        | Ev::Message { peer, .. }
        | Ev::Disconnected { peer, .. }
        | Ev::PathChanged { peer, .. } => Some(*peer),
        Ev::AddrObserved { by, .. } => Some(*by),
        Ev::ConnectFailed { .. } => None,
    }
}

/// Binds `addr`, the first of its addresses that binds. `[::]` binds a dual-stack
/// socket, or `0.0.0.0` on the same port where the host has no IPv6.
fn net(addrs: Vec<SocketAddr>) -> Vec<Addr> {
    addrs.into_iter().map(Hop::Net).collect()
}

/// What the app sees of a path: where its packets go ([`Hop::addr`]).
fn plain(p: &Path<Addr>) -> crate::Path {
    match p {
        Path::Direct(h) => Path::Direct(*h.addr()),
        Path::Via(r) => Path::Via(*r),
    }
}

/// An event of the endpoint's as the app sees it, its paths and addresses as [`plain`] shows them.
fn plain_event(e: Ev<Addr>) -> Event {
    match e {
        Ev::Connected { peer, path, initiator } => Ev::Connected { peer, path: plain(&path), initiator },
        Ev::PathChanged { peer, path } => Ev::PathChanged { peer, path: plain(&path) },
        Ev::AddrObserved { by, addr } => Ev::AddrObserved { by, addr: *addr.addr() },
        Ev::Message { peer, lane, data } => Ev::Message { peer, lane, data },
        Ev::Disconnected { peer, reason } => Ev::Disconnected { peer, reason },
        Ev::ConnectFailed { peer } => Ev::ConnectFailed { peer },
    }
}

/// TURN servers' addresses, resolved as STUN servers' are (a dual-stack socket's over IPv4), but on loopback too;
/// one that doesn't resolve is skipped.
#[cfg(feature = "turn")]
fn resolve_turn(servers: &[TurnServer], local: SocketAddr) -> Vec<(Addr, TurnServer)> {
    let v6 = local.is_ipv6() && !local.ip().is_unspecified();
    let one = |t: &TurnServer| Some((Hop::Net(*resolve(std::slice::from_ref(&t.addr), v6).first()?), t.clone()));
    servers.iter().filter_map(one).collect()
}

fn bind_socket(addr: impl ToSocketAddrs) -> io::Result<UdpSocket> {
    let mut last = io::Error::new(ErrorKind::InvalidInput, "no address to bind");
    for a in addr.to_socket_addrs()? {
        let bound = match a {
            SocketAddr::V6(v6) if v6.ip().is_unspecified() => match dual_stack(a) {
                Err(e) if e.kind() == ErrorKind::AddrInUse => Err(e),
                Err(_) => UdpSocket::bind((Ipv4Addr::UNSPECIFIED, v6.port())),
                ok => ok,
            },
            _ => UdpSocket::bind(a),
        };
        match bound {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// An IPv6 socket that also takes IPv4 (as v4-mapped addresses).
fn dual_stack(addr: SocketAddr) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_only_v6(false)?;
    s.bind(&addr.into())?;
    Ok(s.into())
}

/// The socket's own addresses: the bound one, or, when bound to any interface, the
/// address of each family's default route and loopback (both families for a
/// dual-stack socket; IPv6 only with a route out, which in practice means global),
/// then up to [`MAX_PRIVATE`] other private IPv4 interface addresses. Those are a
/// phone's hotspot side (its default route is cellular) or a second LAN: a peer
/// tethered to it reaches the socket there.
fn local_addresses(local: SocketAddr) -> Vec<SocketAddr> {
    if !local.ip().is_unspecified() {
        return vec![local];
    }
    let families: &[bool] = if local.is_ipv6() { &[false, true] } else { &[false] };
    let at = |ip| SocketAddr::new(ip, local.port());
    let mut out: Vec<SocketAddr> = families.iter().filter_map(|&v6| primary_ip(v6)).map(at).collect();
    out.push(at(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    if local.is_ipv6() {
        out.push(at(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }
    let ifaces = interface_addrs();
    let private = private_v4(ifaces.iter().map(|(name, ip)| (name.as_str(), *ip)));
    for ip in private.into_iter().map(at) {
        if !out.contains(&ip) && out.len() < families.len() + 2 + MAX_PRIVATE {
            out.push(ip);
        }
    }
    out
}

/// How many extra private interface addresses a socket offers (the offer holds 8).
const MAX_PRIVATE: usize = 3;

/// Each interface's name and address.
#[cfg(not(target_os = "espidf"))]
fn interface_addrs() -> Vec<(String, IpAddr)> {
    let ifaces = if_addrs::get_if_addrs().unwrap_or_default();
    ifaces
        .into_iter()
        .map(|i| {
            let ip = i.ip();
            (i.name, ip)
        })
        .collect()
}

/// ESP-IDF has no getifaddrs, and lwIP's UDP connect picks no source address, so a
/// socket bound to any interface offers loopback only: bind the device's own address
/// (as ports/esp32 does).
#[cfg(target_os = "espidf")]
fn interface_addrs() -> Vec<(String, IpAddr)> {
    Vec::new()
}

/// The private IPv4 addresses (RFC 1918) of `(interface, address)` pairs, without
/// container and VM bridges, which no other device can reach.
fn private_v4<'a>(ifaces: impl Iterator<Item = (&'a str, IpAddr)>) -> Vec<IpAddr> {
    const BRIDGES: [&str; 6] = ["docker", "br-", "veth", "virbr", "vmnet", "cni"];
    let mut out = Vec::new();
    for (name, ip) in ifaces {
        let IpAddr::V4(v4) = ip else { continue };
        if v4.is_private() && !BRIDGES.iter().any(|b| name.starts_with(b)) && !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// Starts router port mapping for the socket at `local`, through `at` or else the
/// default gateway. `None` when it can't run: no IPv4, loopback or no LAN address.
fn start_port_mapping(local: SocketAddr, at: Option<(SocketAddr, SocketAddr)>) -> Option<Worker> {
    let (gateway, ssdp) = match at {
        Some(at) => at,
        None => {
            let lan = match local.ip() {
                ip if ip.is_unspecified() => primary_ip(false)?, // dual-stack sockets map their IPv4
                ip => ip,
            };
            let IpAddr::V4(lan) = lan else { return None };
            if lan.is_loopback() {
                return None;
            }
            (SocketAddr::from((portmap::default_gateway(lan), portmap::GATEWAY_PORT)), portmap::SSDP)
        }
    };
    let mut seed = [0u8; 20];
    OsRng.fill_bytes(&mut seed);
    Worker::spawn(local.port(), gateway, ssdp, seed).ok()
}

/// Resolves `host:port` names, keeping the first address of the socket's family.
fn resolve(servers: &[String], v6: bool) -> Vec<SocketAddr> {
    resolve_with(servers, |s| s.to_socket_addrs().ok()?.find(|a| a.is_ipv6() == v6))
}

/// [`resolve`], with `lookup` for DNS. A name is looked up once, so its entries on two
/// ports share one IP though DNS hands out several in turn (NB-083).
fn resolve_with(servers: &[String], mut lookup: impl FnMut(&str) -> Option<SocketAddr>) -> Vec<SocketAddr> {
    let mut names: Vec<(&str, IpAddr)> = Vec::new();
    let mut out = Vec::new();
    for s in servers {
        let (name, port) = s.rsplit_once(':').map_or((s.as_str(), None), |(n, p)| (n, p.parse::<u16>().ok()));
        let known = names.iter().find(|(n, _)| *n == name).map(|&(_, ip)| ip);
        let addr = match (known, port) {
            (Some(ip), Some(port)) => SocketAddr::new(ip, port),
            _ => match lookup(s) {
                Some(a) => a,
                None => continue,
            },
        };
        if known.is_none() {
            names.push((name, addr.ip()));
        }
        out.push(addr);
    }
    out
}

/// The address of the interface the default route leaves by. `connect` on a UDP
/// socket only looks up the route: nothing is sent.
fn primary_ip(v6: bool) -> Option<IpAddr> {
    let (bind, probe): (SocketAddr, SocketAddr) = if v6 {
        ((Ipv6Addr::UNSPECIFIED, 0).into(), (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 9).into())
    } else {
        ((Ipv4Addr::UNSPECIFIED, 0).into(), (Ipv4Addr::new(192, 0, 2, 1), 9).into())
    };
    let s = UdpSocket::bind(bind).ok()?;
    s.connect(probe).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(addr: &str) -> Node {
        Node::builder().stun_servers(Vec::<String>::new()).bind(addr).unwrap()
    }

    #[test]
    fn invite_round_trips_and_lists_loopback() {
        let n = quiet("0.0.0.0:0");
        let inv: Invite<SocketAddr> = Invite::parse(&n.invite()).unwrap();
        assert_eq!(inv.peer, n.peer_id());
        let port = n.local_addr().port();
        assert!(inv.addrs.contains(&SocketAddr::from(([127, 0, 0, 1], port))));
        let bound = Node::bind("127.0.0.1:0").unwrap();
        assert_eq!(bound.addresses(), vec![bound.local_addr()]);
        assert_eq!((bound.public_addr(), bound.nat_type()), (None, NatType::Unknown));
        assert_eq!(bound.next_timeout(), None, "no STUN on loopback, no signaling: nothing to wake for");
    }

    /// NB-083: STUN server names are resolved once each, so one name on two ports is one IP
    /// (the classifier's test for mapping per IP), and the defaults list such a server
    /// next to one at another name.
    #[test]
    fn stun_names_resolve_once_and_the_defaults_ask_one_server_on_two_ports() {
        // A DNS that hands out the next of a name's IPs on every lookup, as round robin does.
        let servers = ["stun.example:19302", "other.example:3478", "stun.example:3478", "stun.example:x"];
        let mut lookups = Vec::new();
        let got = resolve_with(&servers.map(String::from), |s| {
            lookups.push(s.to_string());
            let (name, port) = s.rsplit_once(':')?;
            let n = lookups.len() as u8;
            name.ends_with(".example").then_some(SocketAddr::new([192, 0, 2, n].into(), port.parse().ok()?))
        });
        let at = |n: u8, port| SocketAddr::new([192, 0, 2, n].into(), port);
        assert_eq!(got, vec![at(1, 19302), at(2, 3478), at(1, 3478)], "one IP for one name; a bad port skipped");
        assert_eq!(lookups, ["stun.example:19302", "other.example:3478", "stun.example:x"]);
        // The real resolver, on a name every host has.
        let got = resolve(&["localhost:3478", "localhost:3479"].map(String::from), false);
        assert!(got.len() == 2 && got[0].ip().is_loopback() && got[0].ip() == got[1].ip(), "{got:?}");
        let name = |s: &str| s.rsplit_once(':').unwrap().0.to_string();
        let names: Vec<String> = DEFAULT_STUN_SERVERS.iter().map(|s| name(s)).collect();
        assert_eq!(names.len(), 3);
        assert!(names[0] == names[2] && names[0] != names[1] && DEFAULT_STUN_SERVERS[0] != DEFAULT_STUN_SERVERS[2]);
    }

    /// NB-044: a birthday punch's extra sockets. A peer that reaches one (as a
    /// spray probe would) links through it, every reply leaves from it, so the peer
    /// never sees the main port, and the others close at `until`.
    #[test]
    fn birthday_sockets_carry_the_link_they_hear() {
        let (mut a, mut b) = (quiet("127.0.0.1:0"), quiet("127.0.0.1:0"));
        let until = a.now() + Duration::from_millis(300);
        let opened =
            Birthday { peer: b.peer_id(), dst: Hop::Net(b.local_addr()), sockets: 4, payload: b"x".to_vec(), until };
        a.open_extra(&opened);
        let (socks, _) = a.extra_sockets();
        assert_eq!(socks.len(), 4);
        assert!(socks.iter().all(|s| s.port() != a.local_addr().port()));
        let hit = socks[2];
        let now = b.now();
        b.ep.connect(now, Hop::Net(hit), a.peer_id()).unwrap();
        let t0 = Instant::now();
        while !(a.status(&b.peer_id()) == Status::Connected && b.status(&a.peer_id()) == Status::Connected) {
            assert!(t0.elapsed() < Duration::from_secs(5), "no link");
            b.poll();
            a.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(b.path(&a.peer_id()), Some(Path::Direct(hit)));
        assert!(a.via_birthday(&b.peer_id()), "the opener sees the link on its extra socket");
        assert!(!b.via_birthday(&a.peer_id()), "the prober's link is on its main socket");
        assert_eq!((a.birthday_sockets_opened(), b.birthday_sockets_opened()), (4, 0));
        a.send(&b.peer_id(), Lane::Reliable, b"from the extra socket").unwrap();
        let mut got = Vec::new();
        while t0.elapsed() < Duration::from_secs(5) && got.is_empty() {
            a.poll();
            b.poll();
            while let Some(e) = b.try_recv_event() {
                if let Ev::Message { data, .. } = e {
                    got.push(data);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(got, vec![b"from the extra socket".to_vec()]);
        assert_eq!(b.path(&a.peer_id()), Some(Path::Direct(hit)), "A's replies came from the extra socket");
        assert!(a.next_timeout().is_some_and(|t| t <= EXTRA_POLL), "wakes to read the extra sockets");
        while t0.elapsed() < Duration::from_millis(400) {
            a.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        let (socks, routes) = a.extra_sockets();
        assert_eq!((socks, routes), (vec![hit], 1), "only the route is left");
        assert!(a.via_birthday(&b.peer_id()), "and the link stays on it");
    }

    /// NB44-2: an unauthenticated flood to an extra socket makes no route and does
    /// not keep the socket open; the extra sockets still close at `until`.
    #[test]
    fn birthday_sockets_ignore_unauthenticated_floods() {
        let mut a = quiet("127.0.0.1:0");
        let peer = quiet("127.0.0.1:0").peer_id(); // a peer we never dial
        let until = a.now() + Duration::from_millis(200);
        let dst = Hop::Net("127.0.0.1:9".parse().unwrap());
        let opened = Birthday { peer, dst, sockets: 4, payload: b"x".to_vec(), until };
        a.open_extra(&opened);
        let (socks, _) = a.extra_sockets();
        assert_eq!(socks.len(), 4);
        let target = socks[1];
        for _ in 0..64 {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            s.send_to(b"not a peerwire packet", target).unwrap();
        }
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(60) {
            a.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(a.route_count(), 0, "junk made no route");
        assert_eq!(a.extra_sockets().1, 0, "no extra socket became a route");
        while t0.elapsed() < Duration::from_millis(280) {
            a.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(a.extra_sockets().0.is_empty(), "sockets closed at until, not held by junk");
    }

    /// Polls `nodes` every millisecond until `done` holds (at most 5 s).
    fn poll_until(nodes: &mut [&mut Node], mut done: impl FnMut(&mut [&mut Node]) -> bool) {
        let t0 = Instant::now();
        while !done(nodes) {
            assert!(t0.elapsed() < Duration::from_secs(5), "timed out");
            for n in nodes.iter_mut() {
                n.poll();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// NB44c-1, NB44c-2: extra sockets opened for a punch to b, which a is linked
    /// with, make no route of what reaches them unauthenticated from b's own
    /// address (an HS2 of junk for a's index of the link), nor of a valid
    /// handshake from c, a peer they weren't opened for. They still close at
    /// `until`; c links through a's main socket.
    #[test]
    fn birthday_sockets_take_only_their_peer_authenticated() {
        let (mut a, mut b, mut c) = (quiet("127.0.0.1:0"), quiet("127.0.0.1:0"), quiet("127.0.0.1:0"));
        let (pa, pb, pc) = (a.peer_id(), b.peer_id(), c.peer_id());
        let linked = |x: &Node, y: &Node| x.status(&y.peer_id()) == Status::Connected;
        a.ep.connect(a.now(), Hop::Net(b.local_addr()), pb).unwrap();
        poll_until(&mut [&mut a, &mut b], |n| linked(n[0], n[1]) && linked(n[1], n[0]));
        // A Data packet of b's to a: its receiver index is a's for the link.
        b.ep.send(&pa, Lane::Unreliable, b"x").unwrap();
        let now = b.now();
        let data = std::iter::from_fn(|| b.ep.poll_transmit(now)).find(|t| t.payload[0] & 0x0f == 3).unwrap();
        let junk = [&[(data.payload[0] & 0xf0) | 2, 7, 0, 0, 0][..], &data.payload[1..5], &[0x55; 48]].concat();
        let t0 = Instant::now();
        let until = a.now() + Duration::from_millis(1000);
        a.open_extra(&Birthday { peer: pb, dst: Hop::Net(b.local_addr()), sockets: 4, payload: b"x".to_vec(), until });
        let (socks, _) = a.extra_sockets();
        for _ in 0..3 {
            b.socket().send_to(&junk, socks[0]).unwrap();
        }
        c.ep.connect(c.now(), Hop::Net(socks[1]), pa).unwrap();
        poll_until(&mut [&mut a, &mut b, &mut c], |n| linked(n[0], n[2]) && linked(n[2], n[0]));
        assert!(t0.elapsed() < Duration::from_millis(900), "the test ran past until");
        assert_eq!(c.path(&pa), Some(Path::Direct(a.local_addr())), "c's link is on a's main socket");
        assert_eq!((a.route_count(), a.extra_sockets().1), (0, 0), "no route");
        assert!(!a.via_birthday(&pb) && !a.via_birthday(&pc));
        poll_until(&mut [&mut a, &mut b, &mut c], |_| t0.elapsed() > Duration::from_millis(1100));
        assert!(a.extra_sockets().0.is_empty(), "closed at until");
        assert_eq!((a.status(&pb), a.status(&pc)), (Status::Connected, Status::Connected));
    }

    /// NB44c-2: a peer keeps one route. Packets of b's link, each sent from a new
    /// source port to another of the extra sockets opened for b, move the link (it
    /// roams to the newest), and each makes the socket that heard it the route; the
    /// last one is b's only route, and b's sockets before it close at `until`. c's
    /// route, on a socket of a punch to c open meanwhile, stays.
    #[test]
    fn birthday_sockets_keep_one_route_per_peer() {
        let (mut a, mut b, mut c) = (quiet("127.0.0.1:0"), quiet("127.0.0.1:0"), quiet("127.0.0.1:0"));
        let (pa, pb, pc) = (a.peer_id(), b.peer_id(), c.peer_id());
        let linked = |x: &Node, y: &Node| x.status(&y.peer_id()) == Status::Connected;
        a.ep.connect(a.now(), Hop::Net(b.local_addr()), pb).unwrap();
        a.ep.connect(a.now(), Hop::Net(c.local_addr()), pc).unwrap();
        poll_until(&mut [&mut a, &mut b, &mut c], |n| (1..3).all(|i| linked(n[0], n[i]) && linked(n[i], n[0])));
        // The newest packet `n` sealed for a, sent from a port of its own to `hit`; a reads it.
        let mut froms = Vec::new();
        let mut hop = |a: &mut Node, n: &mut Node, hit: SocketAddr| {
            n.ep.send(&pa, Lane::Unreliable, b"x").unwrap();
            let now = n.now();
            let data = std::iter::from_fn(|| n.ep.poll_transmit(now)).filter(|t| t.payload[0] & 0x0f == 3).last();
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            s.send_to(&data.unwrap().payload, hit).unwrap();
            let from = s.local_addr().unwrap();
            froms.push(s);
            poll_until(&mut [a], |x| x[0].path(&n.peer_id()) == Some(Path::Direct(from)));
        };
        let t0 = Instant::now();
        let until = a.now() + Duration::from_millis(1000);
        a.open_extra(&Birthday { peer: pb, dst: Hop::Net(b.local_addr()), sockets: 4, payload: b"x".to_vec(), until });
        let (socks, _) = a.extra_sockets();
        a.open_extra(&Birthday { peer: pc, dst: Hop::Net(c.local_addr()), sockets: 1, payload: b"x".to_vec(), until });
        let to_c = a.extra_sockets().0.into_iter().find(|s| !socks.contains(s)).unwrap();
        hop(&mut a, &mut c, to_c);
        assert!(a.via_birthday(&pc));
        for &hit in &socks[..3] {
            hop(&mut a, &mut b, hit);
            assert_eq!((a.route_count(), a.extra_sockets().1), (2, 2), "one route each");
            assert!(a.via_birthday(&pb) && a.via_birthday(&pc));
        }
        assert!(t0.elapsed() < Duration::from_millis(900), "the test ran past until");
        poll_until(&mut [&mut a], |_| t0.elapsed() > Duration::from_millis(1100));
        let (mut open, routes) = a.extra_sockets();
        open.sort();
        let mut kept = vec![socks[2], to_c];
        kept.sort();
        assert_eq!((open, routes), (kept, 2), "b's last route and c's are left");
    }

    /// NB85-1: a copy of b's HS1 to a (held by a host on the path, or a member that
    /// relayed b's dial), sent once the link is up to socks[1], an extra socket of a
    /// punch to b, reads again and is answered with an HS2. From m, a host of its
    /// own, the answer leaves socks[1] (which is m's route only until `until`), and
    /// b's route on socks[0] stays. From b's own address (spoofed), the answer
    /// leaves b's route, which stays. Either way socks[1] closes at `until`.
    fn a_copied_hs1_routes_only_its_answer(spoofed: bool) {
        let (mut a, mut b) = (quiet("127.0.0.1:0"), quiet("127.0.0.1:0"));
        let (pa, pb) = (a.peer_id(), b.peer_id());
        let linked = |x: &Node, y: &Node| x.status(&y.peer_id()) == Status::Connected;
        b.ep.connect(b.now(), Hop::Net(a.local_addr()), pa).unwrap();
        let now = b.now();
        let hs1 = std::iter::from_fn(|| b.ep.poll_transmit(now)).find(|t| t.payload[0] & 0x0f == 1).unwrap().payload;
        b.socket().send_to(&hs1, a.local_addr()).unwrap();
        poll_until(&mut [&mut a, &mut b], |n| linked(n[0], n[1]) && linked(n[1], n[0]));
        let t0 = Instant::now();
        let until = a.now() + Duration::from_millis(1000);
        a.open_extra(&Birthday { peer: pb, dst: Hop::Net(b.local_addr()), sockets: 4, payload: b"x".to_vec(), until });
        let (socks, _) = a.extra_sockets();
        // b's link moves to a new port of b's, heard on socks[0]: b's route.
        b.ep.send(&pa, Lane::Unreliable, b"x").unwrap();
        let now = b.now();
        let data = std::iter::from_fn(|| b.ep.poll_transmit(now)).filter(|t| t.payload[0] & 0x0f == 3).last().unwrap();
        let moved = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        moved.send_to(&data.payload, socks[0]).unwrap();
        let b_at = moved.local_addr().unwrap();
        poll_until(&mut [&mut a], |x| x[0].path(&pb) == Some(Path::Direct(b_at)));
        assert!(a.via_birthday(&pb));
        let m = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let src = if spoofed { &moved } else { &m };
        src.set_nonblocking(true).unwrap();
        src.send_to(&hs1, socks[1]).unwrap();
        let (mut got, mut answer) = ([0u8; 2048], None);
        poll_until(&mut [&mut a], |_| {
            while let Ok((n, by)) = src.recv_from(&mut got) {
                if got[0] & 0x0f == 2 {
                    answer = Some((n, by));
                }
            }
            answer.is_some()
        });
        // From m: m's route, beside b's. From b's address: b's route.
        let (by, routes) = if spoofed { (socks[0], 1) } else { (socks[1], 2) };
        assert_eq!(answer, Some((57, by)), "the HS2, and the socket it left");
        assert!(t0.elapsed() < Duration::from_millis(900), "the test ran past until");
        assert_eq!(a.path(&pb), Some(Path::Direct(b_at)), "b's link stays");
        assert!(a.via_birthday(&pb), "on its route");
        assert_eq!((a.route_count(), a.extra_sockets().1), (routes, 1), "a copy holds no socket");
        poll_until(&mut [&mut a], |_| t0.elapsed() > Duration::from_millis(1100));
        assert_eq!(a.extra_sockets(), (vec![socks[0]], 1), "only b's route is left");
        assert_eq!(a.route_count(), 1);
        assert!(a.via_birthday(&pb));
    }

    #[test]
    fn birthday_sockets_route_a_copied_hs1_only_until_until() {
        a_copied_hs1_routes_only_its_answer(false);
    }

    #[test]
    fn birthday_sockets_keep_the_route_of_a_copied_hs1_from_the_peer() {
        a_copied_hs1_routes_only_its_answer(true);
    }

    /// NB-042: any interface lists IPv6 only on a dual-stack socket.
    #[test]
    fn any_interface_lists_both_families_when_dual_stack() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        let v4 = local_addresses(at("0.0.0.0:7"));
        assert!(v4.contains(&at("127.0.0.1:7")) && v4.iter().all(|a| a.is_ipv4()), "{v4:?}");
        let dual = local_addresses(at("[::]:7"));
        assert!(dual.contains(&at("127.0.0.1:7")) && dual.contains(&at("[::1]:7")), "{dual:?}");
        assert_eq!(dual.iter().filter(|a| a.is_ipv4()).count(), v4.len(), "the same IPv4 addresses");
        assert_eq!(local_addresses(at("[::1]:7")), vec![at("[::1]:7")]);
    }

    /// A phone sharing mobile data: its hotspot address is offered, bridges are not.
    #[test]
    fn private_interface_addresses_are_offered() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let ifaces = [
            ("rmnet_data0", ip("192.0.0.4")),
            ("wlan1", ip("10.129.132.170")),
            ("lo", ip("127.0.0.1")),
            ("docker0", ip("172.17.0.1")),
            ("br-1a2b", ip("172.18.0.1")),
            ("eth0", ip("192.168.1.5")),
            ("eth0", ip("2001:db8::5")),
            ("tun0", ip("100.74.51.161")),
            ("wlan1", ip("10.129.132.170")),
        ];
        assert_eq!(private_v4(ifaces.into_iter()), vec![ip("10.129.132.170"), ip("192.168.1.5")]);
        // On this host: the default route's address and loopback come first, at most 3 more.
        let any = local_addresses("0.0.0.0:7".parse().unwrap());
        assert!(any.len() <= 2 + MAX_PRIVATE && any.iter().all(|a| a.port() == 7), "{any:?}");
    }

    #[test]
    fn bad_invites_are_refused() {
        let mut n = Node::bind("127.0.0.1:0").unwrap();
        assert_eq!(n.connect("hello"), Err(Error::InvalidInvite));
        let own = n.invite();
        assert_eq!(n.connect(&own), Err(Error::Connect(ConnectError::SelfConnect)));
        let empty = Invite::<SocketAddr> {
            peer: Node::bind("127.0.0.1:0").unwrap().peer_id(),
            key: None,
            addrs: vec![],
            relays: vec![],
        };
        assert_eq!(n.connect(&empty.to_link()), Err(Error::Connect(ConnectError::NoPath)));
        assert_eq!(n.status(&empty.peer), Status::Disconnected);
    }

    /// NB-038: off by default; once the router maps a port, the node offers it, and
    /// dropping the node removes the mapping.
    #[test]
    fn port_mapping_is_offered_once_mapped_and_removed_on_drop() {
        use crate::portmap::tests::{eventually, lifetime_of, pcp_gateway};
        assert_eq!(quiet("127.0.0.1:0").port_mapping(), PortMapping::Off);
        let on_loopback = Node::builder().stun_servers(Vec::<String>::new()).port_mapping(true).bind("127.0.0.1:0");
        assert_eq!(on_loopback.unwrap().port_mapping(), PortMapping::Unavailable, "no gateway for loopback");
        let ext = Ipv4Addr::new(203, 0, 113, 7);
        let (gw, log) = pcp_gateway(ext, 120);
        let b = Node::builder().stun_servers(Vec::<String>::new()).port_mapping_at(gw, "127.0.0.1:9".parse().unwrap());
        let mut n = b.bind("127.0.0.1:0").unwrap();
        let mapped = SocketAddr::from((ext, 41000));
        // Asking the gateway, or already mapped: bind polls once (set_relay_peers),
        // and a busy host can let the worker get the answer first.
        let pcp = PortMapping::Mapped { addr: mapped, protocol: crate::portmap::MapProtocol::Pcp };
        assert!([PortMapping::Trying, pcp].contains(&n.port_mapping()), "{:?}", n.port_mapping());
        assert!(eventually(|| {
            n.poll();
            n.port_mapping().addr() == Some(mapped)
        }));
        assert_eq!(n.addresses(), vec![mapped, n.local_addr()]);
        let inv: Invite<SocketAddr> = Invite::parse(&n.invite()).unwrap();
        assert!(inv.addrs.contains(&mapped));
        drop(n);
        assert!(eventually(|| log.lock().unwrap().last().is_some_and(|r| lifetime_of(r) == 0)));
    }

    /// NB-097: with a policy, its `port_mapping`, and `Direct`, decide whether the router maps a port, live; with
    /// none again, [`Builder::port_mapping`] does.
    #[test]
    fn port_mapping_follows_the_policy() {
        use crate::portmap::tests::{eventually, lifetime_of, pcp_gateway};
        let ext = Ipv4Addr::new(203, 0, 113, 7);
        let (gw, log) = pcp_gateway(ext, 120);
        let mapped = SocketAddr::from((ext, 41000));
        let b = Node::builder().stun_servers(Vec::<String>::new()).port_mapping_at(gw, "127.0.0.1:9".parse().unwrap());
        let mut n = b.policy(Policy::new([PathKind::Direct])).bind("127.0.0.1:0").unwrap();
        assert_eq!(n.port_mapping(), PortMapping::Off, "the policy doesn't ask for it");
        let map = |n: &mut Node| {
            eventually(|| {
                n.poll();
                n.port_mapping().addr() == Some(mapped)
            })
        };
        n.set_policy(Policy { port_mapping: true, ..Policy::new([PathKind::Direct]) }).unwrap();
        assert!(map(&mut n));
        n.set_policy(Policy { port_mapping: true, ..Policy::new([PathKind::Peers]) }).unwrap();
        assert_eq!(n.port_mapping(), PortMapping::Off, "no direct path to map for");
        assert!(eventually(|| log.lock().unwrap().last().is_some_and(|r| lifetime_of(r) == 0)), "deleted");
        assert!(!n.addresses().contains(&mapped));
        n.set_policy(None).unwrap();
        assert!(map(&mut n), "Builder::port_mapping asked for it");
    }

    /// NPM-1: a relay-only node gives out no address, so it asks the router for
    /// none; switching to relay-only removes a mapping, and switching back maps again.
    #[test]
    fn port_mapping_is_skipped_under_relay_only() {
        use crate::portmap::tests::{eventually, lifetime_of, pcp_gateway};
        let ext = Ipv4Addr::new(203, 0, 113, 7);
        let (gw, log) = pcp_gateway(ext, 120);
        let mut n = Node::builder()
            .stun_servers(Vec::<String>::new())
            .privacy(Privacy::RelayOnly { via: None })
            .port_mapping_at(gw, "127.0.0.1:9".parse().unwrap())
            .bind("127.0.0.1:0")
            .unwrap();
        assert_eq!(n.port_mapping(), PortMapping::RelayOnly);
        std::thread::sleep(std::time::Duration::from_millis(300));
        n.poll();
        assert_eq!(n.port_mapping(), PortMapping::RelayOnly);
        assert!(log.lock().unwrap().is_empty(), "no request to the router");

        let mapped = SocketAddr::from((ext, 41000));
        let map = |n: &mut Node| {
            eventually(|| {
                n.poll();
                n.port_mapping().addr() == Some(mapped)
            })
        };
        n.set_privacy(Privacy::Direct);
        assert_eq!(n.port_mapping(), PortMapping::Trying);
        assert!(map(&mut n));
        assert!(n.addresses().contains(&mapped));

        n.set_privacy(Privacy::RelayOnly { via: None });
        assert_eq!(n.port_mapping(), PortMapping::RelayOnly);
        assert!(eventually(|| log.lock().unwrap().last().is_some_and(|r| lifetime_of(r) == 0)), "deleted");
        n.poll();
        assert_eq!(n.port_mapping(), PortMapping::RelayOnly);
        n.set_privacy(Privacy::RelayOnly { via: Some(n.peer_id()) });
        let requests = log.lock().unwrap().len();

        n.set_privacy(Privacy::PublicOnly);
        assert!(map(&mut n), "mapped again");
        assert!(log.lock().unwrap().len() > requests);
        assert_ne!(lifetime_of(log.lock().unwrap().last().unwrap()), 0);
        assert_eq!(n.addresses(), vec![mapped]);
    }
}
