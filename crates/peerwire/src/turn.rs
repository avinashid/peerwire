//! A TURN client (RFC 8656 over UDP), sans-IO: one allocation on one server ([`TurnClient`]). The host hands it what
//! the server sends and sends what it gives; [`Traversal`](crate::Traversal) runs one per server.
//!
//! - Long-term credentials: the first request is challenged (401) for the realm and nonce, and every request after
//!   it carries MESSAGE-INTEGRITY under MD5(user:realm:pass); a 438 (Stale Nonce) takes the new nonce. Credentials
//!   are a username and password, or minted per allocation from a REST secret ([`TurnServer::rest`]).
//! - The allocation is refreshed at half its lifetime; one lost (the server restarted: 437, or no answer to a
//!   refresh) is made again, with a new relayed address.
//! - Permissions for the peers' IPs ([`TurnClient::permit`]) and a channel for each peer address it sends to, each
//!   refreshed every 4 minutes while wanted (permitted or sent to in the last 10 minutes), at most 64 of each.
//!   Until its channel is bound, a peer gets Send indications.
//! - Only what comes from the server's address counts, and a success response only under the key: a forged one is
//!   ignored, and the request is retransmitted (0.5 s, doubling, 5 tries).
//!
//! Credentials, the key and MESSAGE-INTEGRITY are never in a `Debug` output or an error.

use peerwire_core::stun::{self, attr, method, Class, Message, MessageBuilder, TxId};
use peerwire_core::{Address, Instant, Transmit};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// The first retransmission timeout of a request; it doubles each try.
const RTO: Duration = Duration::from_millis(500);
/// Sends of a request before it has failed.
const MAX_TRIES: u32 = 5;
/// Challenges (401, 438) answered for one request.
const MAX_CHALLENGES: u32 = 2;
/// The lifetime asked for, in seconds; refreshed at half of what the server grants.
const LIFETIME: u32 = 600;
/// After a failed allocation, the wait before the next, doubling to `MAX_RETRY`.
const RETRY: Duration = Duration::from_secs(5);
const MAX_RETRY: Duration = Duration::from_secs(300);
/// Permissions last 5 minutes and channels 10; both are refreshed after 4.
const REFRESH: Duration = Duration::from_secs(240);
/// A permission or channel is kept this long after it was last wanted.
const KEEP: Duration = Duration::from_secs(600);
pub const MAX_PERMISSIONS: usize = 64;
pub const MAX_CHANNELS: usize = 64;

/// A TURN server, and the credentials for it ([`Builder::turn_servers`](crate::Builder::turn_servers)). Its
/// `Debug` output shows the address only.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnServer {
    pub(crate) addr: String,
    pub(crate) creds: Credentials,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Credentials {
    Password { username: String, password: String },
    Rest { secret: Vec<u8>, user: String, ttl: Duration },
}

impl TurnServer {
    /// A server at `addr` (`host:port`, as for STUN servers; a `turn:` prefix is dropped), with a username and
    /// password: long-term credentials, as coturn's users and TURN services such as Cloudflare's hand out. For
    /// short-lived ones, mint them where the secret is (your backend), and pass them in again with
    /// [`Node::set_turn_servers`](crate::Node::set_turn_servers) before they expire.
    pub fn new(addr: impl Into<String>, username: impl Into<String>, password: impl Into<String>) -> TurnServer {
        let creds = Credentials::Password { username: username.into(), password: password.into() };
        TurnServer { addr: strip(addr.into()), creds }
    }

    /// A server at `addr` that takes time-limited credentials (the "TURN REST API" coturn's `static-auth-secret`
    /// and others take), minted here for each allocation from the shared `secret`: the username is `expiry:user`,
    /// with `expiry` `ttl` from now in Unix seconds, and the password base64(HMAC-SHA1(secret, username)). An
    /// allocation lasts until its credentials expire; then a new one is made.
    ///
    /// **The secret mints credentials for anyone who holds it.** Use this only in apps you run yourself, such as
    /// an always-on peer. An app you ship to others should get credentials minted elsewhere and pass them with
    /// [`TurnServer::new`].
    pub fn rest(
        addr: impl Into<String>,
        secret: impl AsRef<[u8]>,
        user: impl Into<String>,
        ttl: Duration,
    ) -> TurnServer {
        let creds = Credentials::Rest { secret: secret.as_ref().to_vec(), user: user.into(), ttl };
        TurnServer { addr: strip(addr.into()), creds }
    }

    /// The address, as given (without `turn:`).
    pub fn addr(&self) -> &str {
        &self.addr
    }
}

fn strip(addr: String) -> String {
    addr.strip_prefix("turn:").map(str::to_string).unwrap_or(addr)
}

impl fmt::Debug for TurnServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.creds {
            Credentials::Password { .. } => "password",
            Credentials::Rest { .. } => "rest",
        };
        f.debug_struct("TurnServer").field("addr", &self.addr).field("credentials", &kind).finish()
    }
}

/// What an allocation did, for the traversal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TurnEvent {
    /// Allocated, at this relayed address.
    Allocated(SocketAddr),
    /// The allocation is gone (lost, refused, or released); another is asked for unless released.
    Lost,
    /// The server refused an allocation with this error code (401: credentials; `None`: no answer).
    Refused(Option<u16>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Allocating,
    Up { relayed: SocketAddr, expires: Instant, refresh_at: Instant },
    Down { retry_at: Instant },
    Closed,
}

#[derive(Clone, Debug)]
enum Ask {
    Allocate(u32),
    Refresh(u32),
    Permission(IpAddr),
    Bind(SocketAddr, u16),
}

struct Req {
    ask: Ask,
    payload: Vec<u8>,
    tries: u32,
    next_at: Instant,
    challenges: u32,
}

/// A permission or channel: installed (`refresh_at` set) or not yet, asked for or not, wanted until `keep_until`.
#[derive(Clone, Copy, Debug)]
struct Grant {
    refresh_at: Option<Instant>,
    asked: bool,
    keep_until: Instant,
}

impl Grant {
    fn new(now: Instant) -> Grant {
        Grant { refresh_at: None, asked: false, keep_until: now + KEEP }
    }

    fn due(&self, now: Instant) -> bool {
        !self.asked && self.refresh_at.is_none_or(|t| now >= t)
    }
}

pub(crate) struct TurnClient<A> {
    server: A,
    creds: Credentials,
    /// Unix milliseconds at `Instant::ZERO`, for minting REST credentials.
    unix_ms: u64,
    rng: ChaCha20Rng,
    /// The username and password of this allocation.
    login: Option<(String, String)>,
    realm: Option<String>,
    nonce: Option<String>,
    key: Option<[u8; 16]>,
    state: State,
    /// Allocations refused in a row.
    failures: u32,
    /// When the credentials of this allocation expire (REST): no lifetime goes past it.
    login_until: Option<Instant>,
    perms: BTreeMap<IpAddr, Grant>,
    chans: BTreeMap<SocketAddr, (u16, Grant)>,
    next_channel: u16,
    reqs: BTreeMap<TxId, Req>,
    out: VecDeque<Transmit<A>>,
    events: VecDeque<TurnEvent>,
}

impl<A: Address> TurnClient<A> {
    /// A client of `server` that asks for an allocation at once.
    pub fn new(now: Instant, server: A, turn: &TurnServer, unix_ms: u64, seed: [u8; 32]) -> TurnClient<A> {
        let mut c = TurnClient {
            server,
            creds: turn.creds.clone(),
            unix_ms,
            rng: ChaCha20Rng::from_seed(seed),
            login: None,
            realm: None,
            nonce: None,
            key: None,
            state: State::Allocating,
            failures: 0,
            login_until: None,
            perms: BTreeMap::new(),
            chans: BTreeMap::new(),
            next_channel: 0x4000,
            reqs: BTreeMap::new(),
            out: VecDeque::new(),
            events: VecDeque::new(),
        };
        c.allocate(now);
        c
    }

    pub fn server(&self) -> &A {
        &self.server
    }

    fn creds(&self) -> &Credentials {
        &self.creds
    }

    /// The relayed address, while allocated.
    pub fn relayed(&self) -> Option<SocketAddr> {
        match self.state {
            State::Up { relayed, .. } => Some(relayed),
            _ => None,
        }
    }

    /// Asks for a new allocation, with fresh credentials.
    fn allocate(&mut self, now: Instant) {
        self.reqs.clear();
        self.chans.clear();
        self.next_channel = 0x4000;
        for p in self.perms.values_mut() {
            (p.refresh_at, p.asked) = (None, false);
        }
        (self.realm, self.nonce, self.key) = (None, None, None);
        (self.login, self.login_until) = match &self.creds {
            Credentials::Password { username, password } => (Some((username.clone(), password.clone())), None),
            Credentials::Rest { secret, user, ttl } => {
                let expiry = (self.unix_ms + now.as_micros() / 1000) / 1000 + ttl.as_secs();
                let username = format!("{expiry}:{user}");
                let password = stun::rest_password(secret, &username);
                (Some((username, password)), Some(now + *ttl))
            }
        };
        self.state = State::Allocating;
        self.request(now, Ask::Allocate(self.lifetime(now)), 0);
    }

    /// The lifetime to ask for: `LIFETIME`, or less, to end when the credentials do. A server keeps an allocation
    /// it can no longer refresh until its lifetime ends, and refuses another from the same address until then.
    fn lifetime(&self, now: Instant) -> u32 {
        let left = self.login_until.map_or(u64::MAX, |u| u.saturating_duration_since(now).as_secs());
        (u64::from(LIFETIME).min(left) as u32).max(1)
    }

    /// The allocation granted `lifetime` seconds from `now`: refreshed at half of it, unless the credentials end
    /// by then, when it lapses and another is made.
    fn granted(&mut self, now: Instant, relayed: SocketAddr, lifetime: u32) {
        let lifetime = Duration::from_secs(lifetime.max(2).into());
        let expires = now + lifetime;
        let refresh_at = match self.login_until {
            Some(u) if expires >= u => expires,
            _ => now + lifetime / 2,
        };
        self.state = State::Up { relayed, expires, refresh_at };
    }

    fn request(&mut self, now: Instant, ask: Ask, challenges: u32) {
        let mut tx = [0u8; 12];
        self.rng.fill_bytes(&mut tx);
        let payload = self.build(&ask, &tx);
        self.out.push_back(Transmit { dst: self.server.clone(), payload: payload.clone() });
        self.reqs.insert(tx, Req { ask, payload, tries: 1, next_at: now + RTO, challenges });
    }

    fn build(&self, ask: &Ask, tx: &TxId) -> Vec<u8> {
        let m = match *ask {
            Ask::Allocate(lifetime) => MessageBuilder::new(method::ALLOCATE, Class::Request, tx)
                .attr(attr::REQUESTED_TRANSPORT, &stun::TRANSPORT_UDP)
                .u32(attr::LIFETIME, lifetime),
            Ask::Refresh(lifetime) => {
                MessageBuilder::new(method::REFRESH, Class::Request, tx).u32(attr::LIFETIME, lifetime)
            }
            Ask::Permission(ip) => MessageBuilder::new(method::CREATE_PERMISSION, Class::Request, tx)
                .xor_address(attr::XOR_PEER_ADDRESS, SocketAddr::new(ip, 0)),
            Ask::Bind(peer, ch) => MessageBuilder::new(method::CHANNEL_BIND, Class::Request, tx)
                .channel(ch)
                .xor_address(attr::XOR_PEER_ADDRESS, peer),
        };
        let m = match (&self.login, &self.realm, &self.nonce, &self.key) {
            (Some((user, _)), Some(realm), Some(nonce), Some(key)) => m
                .attr(attr::USERNAME, user.as_bytes())
                .attr(attr::REALM, realm.as_bytes())
                .attr(attr::NONCE, nonce.as_bytes())
                .integrity(key),
            _ => m,
        };
        m.fingerprint().finish()
    }

    /// Takes a datagram from the server: a response it settles (`None`), or a peer's packet relayed to us, with
    /// the peer's address. Anything else (a channel not bound, a peer not permitted, no allocation) is dropped.
    pub fn handle(&mut self, now: Instant, b: &[u8]) -> Option<(SocketAddr, Vec<u8>)> {
        if let Some((ch, payload)) = stun::parse_channel_data(b) {
            let (peer, _) = self.chans.iter().find(|(_, (n, g))| *n == ch && g.refresh_at.is_some())?;
            return self.relayed().map(|_| (*peer, payload.to_vec()));
        }
        let m = Message::parse(b)?;
        match m.class {
            Class::Indication if m.method == method::DATA => {
                let (peer, data) = (m.xor_address(attr::XOR_PEER_ADDRESS)?, m.attr(attr::DATA)?);
                let permitted = self.perms.contains_key(&peer.ip()) || self.chans.contains_key(&peer);
                (self.relayed().is_some() && permitted).then(|| (peer, data.to_vec()))
            }
            Class::Success | Class::Error => {
                self.on_response(now, &m);
                None
            }
            _ => None,
        }
    }

    /// Whether a datagram from the server is this client's: anything but a STUN message that is neither a Data
    /// indication nor the answer to one of its requests. Those, such as the answer to a Binding request when the
    /// server is one of the node's STUN servers too, are for the node's STUN client.
    pub fn takes(&self, b: &[u8]) -> bool {
        if !stun::is_stun(b) {
            return true;
        }
        Message::parse(b).is_some_and(|m| match m.class {
            Class::Indication => m.method == method::DATA,
            Class::Success | Class::Error => self.reqs.contains_key(&m.tx),
            Class::Request => false,
        })
    }

    fn on_response(&mut self, now: Instant, m: &Message) {
        let Some(req) = self.reqs.get(&m.tx) else { return };
        if m.class == Class::Success {
            // Only under our key: a forged answer leaves the request to be retransmitted.
            if !self.key.is_some_and(|k| m.verify(&k)) || m.unknown_required() {
                return;
            }
            let req = self.reqs.remove(&m.tx).expect("looked up");
            return self.on_success(now, req.ask, m);
        }
        let code = m.error_code();
        let challenge = matches!(code, Some(401 | 438)) && req.challenges < MAX_CHALLENGES;
        // A 401 to a request signed under this nonce: the credentials are wrong.
        let signed_with =
            |n: &Option<String>| m.attr(attr::NONCE).is_some_and(|x| n.as_deref().map(str::as_bytes) == Some(x));
        let wrong = code == Some(401) && self.key.is_some() && signed_with(&self.nonce);
        let req = self.reqs.remove(&m.tx).expect("looked up");
        match (m.attr(attr::REALM), m.attr(attr::NONCE), self.login.clone()) {
            (Some(realm), Some(nonce), Some((user, pass))) if challenge && !wrong => {
                let realm = String::from_utf8_lossy(realm).into_owned();
                self.key = Some(stun::long_term_key(&user, &realm, &pass));
                (self.realm, self.nonce) = (Some(realm), Some(String::from_utf8_lossy(nonce).into_owned()));
                self.request(now, req.ask, req.challenges + 1);
            }
            _ => self.failed(now, req.ask, code),
        }
    }

    fn on_success(&mut self, now: Instant, ask: Ask, m: &Message) {
        match ask {
            Ask::Allocate(asked) => {
                let Some(relayed) = m.xor_address(attr::XOR_RELAYED_ADDRESS) else {
                    return self.failed(now, ask, None);
                };
                self.failures = 0;
                self.granted(now, relayed, m.lifetime().unwrap_or(asked));
                self.events.push_back(TurnEvent::Allocated(relayed));
                self.handle_timeout(now); // the permissions asked for meanwhile
            }
            Ask::Refresh(0) => {}
            Ask::Refresh(asked) => {
                if let State::Up { relayed, .. } = self.state {
                    self.granted(now, relayed, m.lifetime().unwrap_or(asked));
                }
            }
            Ask::Permission(ip) => {
                if let Some(p) = self.perms.get_mut(&ip) {
                    (p.refresh_at, p.asked) = (Some(now + REFRESH), false);
                }
            }
            Ask::Bind(peer, ch) => {
                if let Some((_, g)) = self.chans.get_mut(&peer).filter(|(n, _)| *n == ch) {
                    (g.refresh_at, g.asked) = (Some(now + REFRESH), false);
                }
            }
        }
    }

    fn failed(&mut self, now: Instant, ask: Ask, code: Option<u16>) {
        match ask {
            Ask::Allocate(_) => {
                self.failures += 1;
                let wait = RETRY.saturating_mul(1 << (self.failures - 1).min(6)).min(MAX_RETRY);
                self.state = State::Down { retry_at: now + wait };
                self.events.push_back(TurnEvent::Refused(code));
            }
            // Gone at the server (437), or no answer: another allocation.
            Ask::Refresh(0) => {}
            Ask::Refresh(_) => self.lost(now),
            Ask::Permission(ip) => {
                if code == Some(437) {
                    return self.lost(now);
                }
                if let Some(p) = self.perms.get_mut(&ip) {
                    (p.asked, p.refresh_at) = (false, Some(now + RETRY));
                }
            }
            // No channel: Send indications carry the peer's packets.
            Ask::Bind(peer, _) => {
                if code == Some(437) {
                    return self.lost(now);
                }
                self.chans.remove(&peer);
            }
        }
    }

    fn lost(&mut self, now: Instant) {
        if matches!(self.state, State::Up { .. }) {
            self.events.push_back(TurnEvent::Lost);
        }
        self.allocate(now);
    }

    /// Lets the peers at `ips` send to us through the allocation (CreatePermission), for the next 10 minutes and
    /// as long as this is called again; at most `MAX_PERMISSIONS`.
    pub fn permit(&mut self, now: Instant, ips: impl IntoIterator<Item = IpAddr>) {
        for ip in ips {
            let full = self.perms.len() >= MAX_PERMISSIONS;
            match self.perms.get_mut(&ip) {
                Some(p) => p.keep_until = now + KEEP,
                None if !full => {
                    self.perms.insert(ip, Grant::new(now));
                }
                None => {}
            }
        }
        if self.relayed().is_some() {
            self.handle_timeout(now);
        }
    }

    /// Sends `payload` to `peer` through the allocation: over its channel once bound, else in a Send indication,
    /// asking for a channel. `false` with no allocation.
    pub fn send(&mut self, now: Instant, peer: SocketAddr, payload: &[u8]) -> bool {
        if self.relayed().is_none() {
            return false;
        }
        if let Some((ch, g)) = self.chans.get_mut(&peer) {
            g.keep_until = now + KEEP;
            if g.refresh_at.is_some() {
                let data = stun::channel_data(*ch, payload);
                self.out.push_back(Transmit { dst: self.server.clone(), payload: data });
                return true;
            }
        } else if self.chans.len() < MAX_CHANNELS && stun::is_channel(self.next_channel) {
            let ch = self.next_channel;
            self.next_channel += 1;
            self.chans.insert(peer, (ch, Grant { asked: true, ..Grant::new(now) }));
            self.request(now, Ask::Bind(peer, ch), 0);
        }
        let mut tx = [0u8; 12];
        self.rng.fill_bytes(&mut tx);
        let ind = MessageBuilder::new(method::SEND, Class::Indication, &tx)
            .xor_address(attr::XOR_PEER_ADDRESS, peer)
            .attr(attr::DATA, payload)
            .finish();
        self.out.push_back(Transmit { dst: self.server.clone(), payload: ind });
        true
    }

    /// Releases the allocation (a Refresh with LIFETIME 0, sent once), and asks for no other.
    pub fn close(&mut self) {
        if self.relayed().is_some() {
            let mut tx = [0u8; 12];
            self.rng.fill_bytes(&mut tx);
            let payload = self.build(&Ask::Refresh(0), &tx);
            self.out.push_back(Transmit { dst: self.server.clone(), payload });
            self.events.push_back(TurnEvent::Lost);
        }
        self.reqs.clear();
        self.state = State::Closed;
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        let state = match self.state {
            State::Up { expires, refresh_at, .. } => Some(expires.min(refresh_at)),
            State::Down { retry_at, .. } => Some(retry_at),
            State::Allocating | State::Closed => None,
        };
        let up = self.relayed().is_some();
        let grants = self.perms.values().chain(self.chans.values().map(|(_, g)| g)).filter(|_| up);
        let grants = grants.filter(|g| !g.asked).map(|g| g.refresh_at.unwrap_or(Instant::ZERO).min(g.keep_until));
        self.reqs.values().map(|r| r.next_at).chain(state).chain(grants).min()
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        let due: Vec<TxId> = self.reqs.iter().filter(|(_, r)| now >= r.next_at).map(|(tx, _)| *tx).collect();
        for tx in due {
            let Some(r) = self.reqs.get_mut(&tx) else { continue };
            if r.tries >= MAX_TRIES {
                let r = self.reqs.remove(&tx).expect("looked up");
                self.failed(now, r.ask, None);
                continue;
            }
            r.tries += 1;
            r.next_at = now + RTO.saturating_mul(1 << (r.tries - 1));
            self.out.push_back(Transmit { dst: self.server.clone(), payload: r.payload.clone() });
        }
        match self.state {
            State::Up { expires, .. } if now >= expires => return self.lost(now),
            State::Up { refresh_at, .. } if now >= refresh_at => {
                if let State::Up { refresh_at, expires, .. } = &mut self.state {
                    *refresh_at = *expires; // asked: the answer sets the next
                }
                let lifetime = self.lifetime(now);
                self.request(now, Ask::Refresh(lifetime), 0);
            }
            State::Down { retry_at, .. } if now >= retry_at => return self.allocate(now),
            _ => {}
        }
        if self.relayed().is_none() {
            return;
        }
        self.perms.retain(|_, p| now < p.keep_until);
        self.chans.retain(|_, (_, g)| now < g.keep_until);
        let perms: Vec<IpAddr> = self.perms.iter().filter(|(_, p)| p.due(now)).map(|(ip, _)| *ip).collect();
        for ip in perms {
            self.perms.get_mut(&ip).expect("listed").asked = true;
            self.request(now, Ask::Permission(ip), 0);
        }
        let chans: Vec<(SocketAddr, u16)> =
            self.chans.iter().filter(|(_, (_, g))| g.due(now)).map(|(p, (ch, _))| (*p, *ch)).collect();
        for (peer, ch) in chans {
            self.chans.get_mut(&peer).expect("listed").1.asked = true;
            self.request(now, Ask::Bind(peer, ch), 0);
        }
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit<A>> {
        self.out.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<TurnEvent> {
        self.events.pop_front()
    }
}

/// This node's TURN servers and their allocations, as the traversal runs them: a [`TurnClient`] for each server
/// while TURN is on (servers set, `Turn` in the policy, not relay-only).
pub(crate) struct TurnSet<A> {
    servers: Vec<(A, TurnServer)>,
    clients: Vec<TurnClient<A>>,
    on: bool,
    unix_ms: u64,
    rng: ChaCha20Rng,
    /// What released clients still had to send.
    out: VecDeque<Transmit<A>>,
    events: VecDeque<(A, TurnEvent)>,
}

impl<A: Address> TurnSet<A> {
    pub fn new(unix_ms: u64, seed: [u8; 32]) -> TurnSet<A> {
        let (servers, clients, out, events) = (Vec::new(), Vec::new(), VecDeque::new(), VecDeque::new());
        TurnSet { servers, clients, on: false, unix_ms, rng: ChaCha20Rng::from_seed(seed), out, events }
    }

    /// Replaces the servers: a client whose server and credentials stay keeps its allocation, the others release
    /// theirs ([`TurnEvent::Lost`]).
    pub fn set_servers(&mut self, now: Instant, servers: Vec<(A, TurnServer)>) {
        self.servers = servers;
        self.sync(now);
    }

    pub fn has_servers(&self) -> bool {
        !self.servers.is_empty()
    }

    pub fn set_on(&mut self, now: Instant, on: bool) {
        if self.on != on {
            self.on = on;
            self.sync(now);
        }
    }

    fn sync(&mut self, now: Instant) {
        let (on, servers) = (self.on, &self.servers);
        let wanted = |c: &TurnClient<A>| on && servers.iter().any(|(a, t)| a == c.server() && t.creds == *c.creds());
        let (keep, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut self.clients).into_iter().partition(wanted);
        self.clients = keep;
        for mut c in gone {
            c.close();
            self.out.extend(std::iter::from_fn(|| c.poll_transmit()));
            let server = c.server().clone();
            self.events.extend(std::iter::from_fn(|| c.poll_event()).map(|e| (server.clone(), e)));
        }
        if !on {
            return;
        }
        for (addr, turn) in self.servers.clone() {
            if !self.clients.iter().any(|c| *c.server() == addr) {
                let mut seed = [0u8; 32];
                self.rng.fill_bytes(&mut seed);
                self.clients.push(TurnClient::new(now, addr, &turn, self.unix_ms, seed));
            }
        }
    }

    /// The relayed addresses of the allocations.
    pub fn relayed(&self) -> Vec<A> {
        self.clients.iter().filter_map(|c| A::from_socket_addr(c.relayed()?)).collect()
    }

    /// The servers this node has an allocation on, and its relayed address there.
    pub fn up(&self) -> Vec<(A, SocketAddr)> {
        self.clients.iter().filter_map(|c| Some((c.server().clone(), c.relayed()?))).collect()
    }

    /// Each server, and its relayed address while allocated.
    pub fn status(&self) -> Vec<(A, Option<SocketAddr>)> {
        let relayed = |a: &A| self.clients.iter().find(|c| c.server() == a).and_then(|c| c.relayed());
        self.servers.iter().map(|(a, _)| (a.clone(), relayed(a))).collect()
    }

    /// Lets the peers at `ips` send through every allocation.
    pub fn permit(&mut self, now: Instant, ips: &[IpAddr]) {
        for c in &mut self.clients {
            c.permit(now, ips.iter().copied());
        }
    }

    /// A datagram from `from`: `None` if it isn't one of the servers, or is a STUN message that server's client
    /// doesn't [take](TurnClient::takes); else what it carried for the endpoint, as from the peer through that server
    /// ([`Address::turn_hop`]), if anything.
    pub fn handle(&mut self, now: Instant, from: &A, data: &[u8]) -> Option<Option<(A, Vec<u8>)>> {
        let c = self.clients.iter_mut().find(|c| c.server() == from && c.takes(data))?;
        Some(c.handle(now, data).and_then(|(peer, payload)| Some((A::turn_hop(from, peer)?, payload))))
    }

    /// Sends `payload` to a TURN hop's peer through that server's allocation. `false` with none.
    pub fn send(&mut self, now: Instant, dst: &A, payload: &[u8]) -> bool {
        let Some((server, peer)) = dst.turn_parts() else { return false };
        self.clients.iter_mut().find(|c| *c.server() == server).is_some_and(|c| c.send(now, peer, payload))
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit<A>> {
        self.out.pop_front().or_else(|| self.clients.iter_mut().find_map(|c| c.poll_transmit()))
    }

    /// Also due at once while an allocation's news (made, lost) waits for the traversal.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let news = !self.events.is_empty() || self.clients.iter().any(|c| !c.events.is_empty());
        let news = news.then_some(Instant::ZERO);
        self.clients.iter().filter_map(|c| c.poll_timeout()).chain(news).min()
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        for c in &mut self.clients {
            if c.poll_timeout().is_some_and(|t| t <= now) {
                c.handle_timeout(now);
            }
        }
    }

    pub fn poll_event(&mut self) -> Option<(A, TurnEvent)> {
        if let Some(e) = self.events.pop_front() {
            return Some(e);
        }
        self.clients.iter_mut().find_map(|c| Some((c.server().clone(), c.poll_event()?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerwire_sim::turn::{Out, TurnServer as Server};

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// The client's, the server's and a peer's addresses.
    const C: &str = "192.0.2.10:5000";
    const S: &str = "198.51.100.1:3478";
    const P: &str = "203.0.113.7:6000";
    const UNIX_MS: u64 = 1_700_000_000_000;

    fn server() -> Server {
        let mut port = 49151u16;
        let next = move || {
            port += 1;
            Some(SocketAddr::new(addr(S).ip(), port))
        };
        Server::new(addr(S), "peerwire.test", next).user("app", "secret")
    }

    /// A client and the server, run in 10 ms steps.
    struct Net {
        s: Server,
        c: TurnClient<SocketAddr>,
        t: Instant,
        /// What reached peers: from which relayed address, to whom, what.
        at_peers: Vec<(SocketAddr, SocketAddr, Vec<u8>)>,
        /// What peers sent us through the server.
        got: Vec<(SocketAddr, Vec<u8>)>,
        events: Vec<TurnEvent>,
        /// The client's datagrams lost on the way to the server.
        lose: bool,
    }

    impl Net {
        fn new(turn: &TurnServer, s: Server) -> Net {
            let t = Instant::from_millis(1);
            let c = TurnClient::new(t, addr(S), turn, UNIX_MS, [7; 32]);
            Net { s, c, t, at_peers: Vec::new(), got: Vec::new(), events: Vec::new(), lose: false }
        }

        fn up() -> Net {
            let mut n = Net::new(&TurnServer::new(S, "app", "secret"), server());
            n.run(100);
            assert!(n.c.relayed().is_some(), "{:?}", n.events);
            n
        }

        fn run(&mut self, ms: u64) {
            for _ in 0..ms / 10 {
                self.flush();
                self.t = self.t + Duration::from_millis(10);
                if self.c.poll_timeout().is_some_and(|w| w <= self.t) {
                    self.c.handle_timeout(self.t);
                }
            }
            self.flush();
        }

        fn flush(&mut self) {
            while let Some(tx) = self.c.poll_transmit() {
                assert_eq!(tx.dst, addr(S));
                if !self.lose {
                    for o in self.s.handle_client(self.t, addr(C), &tx.payload) {
                        self.deliver(o);
                    }
                }
            }
            self.events.extend(std::iter::from_fn(|| self.c.poll_event()));
        }

        fn deliver(&mut self, o: Out) {
            match o {
                Out::ToClient { to, data } => {
                    assert_eq!(to, addr(C));
                    self.got.extend(self.c.handle(self.t, &data));
                }
                Out::ToPeer { from, to, data } => self.at_peers.push((from, to, data)),
            }
        }

        /// A peer at `from` sends `data` to our relayed address.
        fn peer_sends(&mut self, from: SocketAddr, data: &[u8]) {
            let relayed = self.c.relayed().unwrap();
            if let Some(o) = self.s.handle_peer(self.t, relayed, from, data) {
                self.deliver(o);
            }
            self.flush();
        }
    }

    #[test]
    fn allocates_after_the_challenge() {
        let n = Net::up();
        let relayed = n.c.relayed().unwrap();
        assert_eq!(n.events, [TurnEvent::Allocated(relayed)]);
        assert_eq!(n.s.relayed(), [relayed]);
        assert_eq!((n.s.stats.challenges, n.s.stats.allocations, n.s.stats.auth_failures), (1, 1, 0));
    }

    /// Wrong credentials: refused (401), and asked again 5 s later, then 10 s after that.
    #[test]
    fn wrong_credentials_are_refused_and_tried_later() {
        let mut n = Net::new(&TurnServer::new(S, "app", "wrong"), server());
        n.run(100);
        assert_eq!(
            (n.events.clone(), n.c.relayed(), n.s.stats.auth_failures),
            (vec![TurnEvent::Refused(Some(401))], None, 1)
        );
        n.run(5000);
        assert_eq!(n.s.stats.auth_failures, 2);
        n.run(9000);
        assert_eq!(n.s.stats.auth_failures, 2, "the wait doubles");
        n.run(1000);
        assert_eq!(n.s.stats.auth_failures, 3);
    }

    /// The allocation is refreshed at half its lifetime (600 s): alive at 700 s, at the same address.
    #[test]
    fn refreshes_before_its_lifetime_ends() {
        let mut n = Net::up();
        let relayed = n.c.relayed();
        n.run(700_000);
        assert_eq!(
            (n.c.relayed(), n.s.relayed().len(), n.s.stats.refreshes, n.s.stats.allocations),
            (relayed, 1, 2, 1)
        );
    }

    /// A permitted peer's packets come in (a Data indication); ours go out in a Send indication, then on the
    /// channel that asked for, as do the peer's. Permissions are refreshed while wanted: here 10 minutes from the
    /// last `permit` or send, then they expire at the server 5 minutes on.
    #[test]
    fn permissions_and_channels_carry_both_ways() {
        let mut n = Net::up();
        let (peer, relayed) = (addr(P), n.c.relayed().unwrap());
        n.peer_sends(peer, b"before");
        assert!(n.got.is_empty(), "no permission yet: dropped at the server");
        n.c.permit(n.t, [peer.ip()]);
        n.run(100);
        assert_eq!(n.s.permissions(n.t, addr(C)), [peer.ip()]);
        n.peer_sends(peer, b"in a data indication");
        assert_eq!(n.got, [(peer, b"in a data indication".to_vec())]);
        assert!(n.c.send(n.t, peer, b"in a send indication"));
        n.run(100);
        assert_eq!(n.at_peers, [(relayed, peer, b"in a send indication".to_vec())]);
        assert_eq!(n.s.channels(n.t, addr(C)).len(), 1);
        assert!(n.c.send(n.t, peer, b"on the channel"));
        n.peer_sends(peer, b"on the channel too");
        n.flush();
        assert_eq!(n.at_peers.last(), Some(&(relayed, peer, b"on the channel".to_vec())));
        assert_eq!(n.got.last(), Some(&(peer, b"on the channel too".to_vec())));
        let st = n.s.stats;
        assert_eq!((st.data_indications, st.sends, st.channel_data_in, st.channel_data_out), (1, 1, 1, 1));
        n.run(400_000);
        assert_eq!(n.s.permissions(n.t, addr(C)), [peer.ip()], "refreshed at 4 minutes");
        n.run(700_000);
        assert!(n.s.permissions(n.t, addr(C)).is_empty() && n.s.channels(n.t, addr(C)).is_empty());
    }

    /// Unasked for: ChannelData on a channel not bound, and a Data indication from a peer not permitted.
    #[test]
    fn what_was_not_asked_for_is_dropped() {
        let mut n = Net::up();
        assert_eq!(n.c.handle(n.t, &stun::channel_data(0x4000, b"x")), None);
        let ind = MessageBuilder::new(method::DATA, Class::Indication, &[1; 12])
            .xor_address(attr::XOR_PEER_ADDRESS, addr(P))
            .attr(attr::DATA, b"x")
            .finish();
        assert_eq!(n.c.handle(n.t, &ind), None);
        n.c.permit(n.t, [addr(P).ip()]);
        assert_eq!(n.c.handle(n.t, &ind), Some((addr(P), b"x".to_vec())));
    }

    /// The client takes ChannelData, Data indications, the answers to its requests and whatever isn't STUN; it
    /// leaves other STUN messages, such as a Binding answer from a server that is a STUN server too, for STUN.
    #[test]
    fn it_takes_its_own_and_leaves_the_rest_of_stun() {
        let mut n = Net::new(&TurnServer::new(S, "app", "secret"), server());
        let ind = |m| MessageBuilder::new(m, Class::Indication, &[1; 12]).attr(attr::DATA, b"x").finish();
        assert!(n.c.takes(&ind(method::DATA)) && n.c.takes(&stun::channel_data(0x4000, b"x")) && n.c.takes(&[16; 40]));
        assert!(!n.c.takes(&ind(method::SEND)) && !n.c.takes(&stun::binding_request(&[9; 12])));
        let asked = n.c.poll_transmit().unwrap();
        let tx: TxId = asked.payload[8..20].try_into().unwrap();
        for class in [Class::Success, Class::Error] {
            let answer = |tx: &TxId| MessageBuilder::new(method::ALLOCATE, class, tx).finish();
            assert!(n.c.takes(&answer(&tx)) && !n.c.takes(&answer(&[9; 12])), "{class:?}");
        }
        let mut torn = stun::binding_response(&[9; 12], addr(C));
        torn.truncate(torn.len() - 1);
        assert!(stun::is_stun(&torn) && Message::parse(&torn).is_none());
        assert!(!n.c.takes(&stun::binding_response(&[9; 12], addr(C))) && !n.c.takes(&torn));
    }

    /// From a server: a STUN message its client leaves goes back (`None`), for STUN; what isn't STUN is dropped
    /// (`Some(None)`), never handed on. From elsewhere: `None` either way.
    #[test]
    fn the_set_hands_on_stun_its_clients_leave() {
        let (t, mut set) = (Instant::from_millis(1), TurnSet::<SocketAddr>::new(UNIX_MS, [3; 32]));
        set.set_servers(t, vec![(addr(S), TurnServer::new(S, "app", "secret"))]);
        set.set_on(t, true);
        let binding = stun::binding_response(&[9; 12], addr(C));
        assert_eq!(set.handle(t, &addr(S), &binding), None);
        assert_eq!(set.handle(t, &addr(S), &[16; 40]), Some(None));
        assert_eq!((set.handle(t, &addr(P), &binding), set.handle(t, &addr(P), &[16; 40])), (None, None));
    }

    #[test]
    fn a_stale_nonce_is_taken() {
        let mut n = Net::up();
        n.s.set_nonce("nonce-1");
        n.c.permit(n.t, [addr(P).ip()]);
        n.run(100);
        assert_eq!(n.s.permissions(n.t, addr(C)), [addr(P).ip()]);
        assert_eq!(n.s.stats.challenges, 2);
    }

    /// The server restarted: the refresh gets 437, and the client allocates again, at a new address.
    #[test]
    fn a_lost_allocation_is_made_again() {
        let mut n = Net::up();
        let old = n.c.relayed().unwrap();
        n.c.permit(n.t, [addr(P).ip()]);
        n.s.restart();
        n.run(301_000);
        let new = n.c.relayed().unwrap();
        assert_ne!(old, new);
        assert_eq!(n.events, [TurnEvent::Allocated(old), TurnEvent::Lost, TurnEvent::Allocated(new)]);
        assert_eq!(n.s.permissions(n.t, addr(C)), [addr(P).ip()], "permissions asked for again");
    }

    /// Unanswered requests are sent again (0.5, 1, 2, 4 s), then given up: tried again 5 s later.
    #[test]
    fn requests_are_retransmitted_then_given_up() {
        let mut n = Net::new(&TurnServer::new(S, "app", "secret"), server());
        n.lose = true;
        n.run(15_000);
        assert_eq!(n.events, []);
        n.run(1000);
        assert_eq!(n.events, [TurnEvent::Refused(None)]);
        n.lose = false;
        n.run(5000);
        assert!(n.c.relayed().is_some());
    }

    /// A success response not under the key (here another password's) doesn't count: the request goes on.
    #[test]
    fn only_answers_under_the_key_count() {
        let mut n = Net::new(&TurnServer::new(S, "app", "secret"), server());
        let first = n.c.poll_transmit().unwrap();
        for o in n.s.handle_client(n.t, addr(C), &first.payload) {
            n.deliver(o);
        }
        let signed = n.c.poll_transmit().unwrap();
        let tx: TxId = signed.payload[8..20].try_into().unwrap();
        let forged = MessageBuilder::new(method::ALLOCATE, Class::Success, &tx)
            .xor_address(attr::XOR_RELAYED_ADDRESS, addr("203.0.113.66:1"))
            .integrity(&stun::long_term_key("app", "peerwire.test", "guess"))
            .finish();
        assert_eq!(n.c.handle(n.t, &forged), None);
        assert_eq!(n.c.relayed(), None);
        n.run(1000);
        assert_ne!(n.c.relayed(), Some(addr("203.0.113.66:1")));
        assert!(n.c.relayed().is_some());
    }

    /// REST credentials, minted for an hour: the allocation lasts while they are good, then another is made.
    #[test]
    fn rest_credentials_last_their_ttl() {
        let s = server().rest_secret(b"north-star", UNIX_MS / 1000);
        let turn = TurnServer::rest(S, "north-star", "alice", Duration::from_secs(3600));
        let mut n = Net::new(&turn, s);
        n.run(100);
        let first = n.c.relayed().unwrap();
        n.run(3_700_000);
        assert!(n.events.contains(&TurnEvent::Lost), "{:?}", n.events);
        assert!(n.c.relayed().is_some_and(|r| r != first));
        assert_eq!(n.s.stats.allocations, 2);
    }

    #[test]
    fn close_releases_the_allocation() {
        let mut n = Net::up();
        n.c.close();
        n.run(60_000);
        assert_eq!((n.s.relayed(), n.s.stats.deallocations, n.c.relayed()), (vec![], 1, None));
        assert_eq!(n.events.last(), Some(&TurnEvent::Lost));
    }

    #[test]
    fn debug_shows_no_credentials() {
        let a = format!("{:?}", TurnServer::new("turn:turn.example:3478", "alice-77", "s3cr3t-99"));
        let b = format!("{:?}", TurnServer::rest("turn.example:3478", "shared-55", "bob-88", Duration::from_secs(60)));
        assert!(a.contains("turn.example:3478") && !a.contains("turn:turn"), "{a}");
        for s in ["alice-77", "s3cr3t-99", "shared-55", "bob-88"] {
            assert!(!a.contains(s) && !b.contains(s), "{a} {b}");
        }
    }
}
