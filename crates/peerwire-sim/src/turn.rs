//! A TURN server (RFC 8656 over UDP) for tests and the simulator, sans-IO: long-term credentials (static users, or
//! a shared REST secret), allocations with a lifetime, permissions (5 minutes) and channels (10 minutes) that
//! expire, Send and Data indications, ChannelData both ways, a nonce that can go stale and a restart that forgets
//! everything. Not a production server: one allocation per client address, at most `MAX_ALLOCATIONS`, UDP only.

use peerwire_core::stun::{self, attr, method, Class, Message, MessageBuilder};
use peerwire_core::Instant;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

pub const PERMISSION_LIFETIME: Duration = Duration::from_secs(300);
pub const CHANNEL_LIFETIME: Duration = Duration::from_secs(600);
pub const DEFAULT_LIFETIME: u32 = 600;
pub const MAX_LIFETIME: u32 = 3600;
pub const MAX_ALLOCATIONS: usize = 64;

/// What the server sends: to a client from its own address, or to a peer from a relayed address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    ToClient { to: SocketAddr, data: Vec<u8> },
    ToPeer { from: SocketAddr, to: SocketAddr, data: Vec<u8> },
}

/// What the server did, for tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServerStats {
    pub allocations: u64,
    pub refreshes: u64,
    pub deallocations: u64,
    pub permissions: u64,
    pub channel_binds: u64,
    /// 401 and 438 answers.
    pub challenges: u64,
    /// Requests whose credentials didn't check out.
    pub auth_failures: u64,
    pub sends: u64,
    pub channel_data_in: u64,
    pub data_indications: u64,
    pub channel_data_out: u64,
    /// Packets to or from a peer with no permission, or on no allocation.
    pub dropped: u64,
}

struct Alloc {
    relayed: SocketAddr,
    expires: Instant,
    username: String,
    perms: HashMap<IpAddr, Instant>,
    channels: HashMap<u16, (SocketAddr, Instant)>,
}

impl Alloc {
    fn permits(&self, now: Instant, ip: IpAddr) -> bool {
        self.perms.get(&ip).is_some_and(|&until| now < until)
    }

    fn channel_of(&self, now: Instant, peer: SocketAddr) -> Option<u16> {
        self.channels.iter().find(|(_, &(p, until))| p == peer && now < until).map(|(&ch, _)| ch)
    }
}

pub struct TurnServer {
    /// Where clients reach it.
    pub addr: SocketAddr,
    pub realm: String,
    pub stats: ServerStats,
    users: HashMap<String, String>,
    secret: Option<Vec<u8>>,
    /// Unix seconds at `Instant::ZERO`, for REST usernames' expiry.
    unix_at_zero: u64,
    nonce: String,
    allocs: HashMap<SocketAddr, Alloc>,
    relays: HashMap<SocketAddr, SocketAddr>,
    next_relay: Box<dyn FnMut() -> Option<SocketAddr> + Send>,
}

impl TurnServer {
    /// A server at `addr` whose allocations get the addresses `next_relay` gives.
    pub fn new(
        addr: SocketAddr,
        realm: &str,
        next_relay: impl FnMut() -> Option<SocketAddr> + Send + 'static,
    ) -> TurnServer {
        TurnServer {
            addr,
            realm: realm.into(),
            stats: ServerStats::default(),
            users: HashMap::new(),
            secret: None,
            unix_at_zero: 0,
            nonce: "nonce-0".into(),
            allocs: HashMap::new(),
            relays: HashMap::new(),
            next_relay: Box::new(next_relay),
        }
    }

    pub fn user(mut self, name: &str, password: &str) -> TurnServer {
        self.users.insert(name.into(), password.into());
        self
    }

    /// Takes REST credentials (`expiry:user`, the password from `stun::rest_password`) under `secret`, with
    /// `unix_at_zero` the Unix time at `Instant::ZERO`.
    pub fn rest_secret(mut self, secret: &[u8], unix_at_zero: u64) -> TurnServer {
        (self.secret, self.unix_at_zero) = (Some(secret.to_vec()), unix_at_zero);
        self
    }

    /// A new nonce: requests with the old one get 438 (Stale Nonce).
    pub fn set_nonce(&mut self, nonce: &str) {
        self.nonce = nonce.into();
    }

    /// Forgets every allocation, as a restarted server does.
    pub fn restart(&mut self) {
        self.allocs.clear();
        self.relays.clear();
    }

    /// The relayed addresses of the live allocations.
    pub fn relayed(&self) -> Vec<SocketAddr> {
        self.relays.keys().copied().collect()
    }

    /// The client whose allocation has relayed address `relayed`.
    pub fn client_of(&self, relayed: SocketAddr) -> Option<SocketAddr> {
        self.relays.get(&relayed).copied()
    }

    /// The IPs `client`'s allocation permits now.
    pub fn permissions(&self, now: Instant, client: SocketAddr) -> Vec<IpAddr> {
        let Some(a) = self.allocs.get(&client) else { return Vec::new() };
        a.perms.iter().filter(|(_, &until)| now < until).map(|(&ip, _)| ip).collect()
    }

    /// The channels `client`'s allocation has bound now, and their peers.
    pub fn channels(&self, now: Instant, client: SocketAddr) -> Vec<(u16, SocketAddr)> {
        let Some(a) = self.allocs.get(&client) else { return Vec::new() };
        a.channels.iter().filter(|(_, &(_, until))| now < until).map(|(&ch, &(p, _))| (ch, p)).collect()
    }

    fn expire(&mut self, now: Instant) {
        let gone: Vec<SocketAddr> = self.allocs.iter().filter(|(_, a)| now >= a.expires).map(|(&c, _)| c).collect();
        for c in gone {
            if let Some(a) = self.allocs.remove(&c) {
                self.relays.remove(&a.relayed);
            }
        }
    }

    /// A datagram from a client, at the server's address.
    pub fn handle_client(&mut self, now: Instant, from: SocketAddr, b: &[u8]) -> Vec<Out> {
        self.expire(now);
        if let Some((ch, payload)) = stun::parse_channel_data(b) {
            let peer = self.allocs.get(&from).and_then(|a| {
                let (p, until) = *a.channels.get(&ch)?;
                (now < until && a.permits(now, p.ip())).then_some((a.relayed, p))
            });
            return match peer {
                Some((relayed, to)) => {
                    self.stats.channel_data_in += 1;
                    vec![Out::ToPeer { from: relayed, to, data: payload.to_vec() }]
                }
                None => {
                    self.stats.dropped += 1;
                    Vec::new()
                }
            };
        }
        let Some(m) = Message::parse(b) else { return Vec::new() };
        if m.class == Class::Indication && m.method == method::SEND {
            let (Some(a), Some(to), Some(data)) =
                (self.allocs.get(&from), m.xor_address(attr::XOR_PEER_ADDRESS), m.attr(attr::DATA))
            else {
                return Vec::new();
            };
            if !a.permits(now, to.ip()) {
                self.stats.dropped += 1;
                return Vec::new();
            }
            self.stats.sends += 1;
            return vec![Out::ToPeer { from: a.relayed, to, data: data.to_vec() }];
        }
        if m.class != Class::Request {
            return Vec::new();
        }
        let reply = |b: MessageBuilder| vec![Out::ToClient { to: from, data: b.fingerprint().finish() }];
        let error = |code: u16| MessageBuilder::new(m.method, Class::Error, &m.tx).error(code);
        // Long-term credentials: a challenge first, then a request under the key, with the current nonce.
        let (Some(user), Some(_), Some(nonce)) = (m.attr(attr::USERNAME), m.attr(attr::REALM), m.attr(attr::NONCE))
        else {
            self.stats.challenges += 1;
            return reply(self.challenge(error(401)));
        };
        let user = String::from_utf8_lossy(user).into_owned();
        let Some(key) = self.key(now, &user).filter(|k| m.verify(k)) else {
            self.stats.auth_failures += 1;
            return reply(self.challenge(error(401)));
        };
        if nonce != self.nonce.as_bytes() {
            self.stats.challenges += 1;
            return reply(self.challenge(error(438)));
        }
        let ok = MessageBuilder::new(m.method, Class::Success, &m.tx);
        let fail = |code: u16| reply(error(code).integrity(&key));
        if m.method == method::ALLOCATE {
            if self.allocs.contains_key(&from) {
                return fail(437);
            }
            if m.attr(attr::REQUESTED_TRANSPORT) != Some(&stun::TRANSPORT_UDP[..]) {
                return fail(442);
            }
            let Some(relayed) = (self.allocs.len() < MAX_ALLOCATIONS).then(&mut self.next_relay).flatten() else {
                return fail(508);
            };
            let lifetime = m.lifetime().unwrap_or(DEFAULT_LIFETIME).clamp(1, MAX_LIFETIME);
            let alloc = Alloc {
                relayed,
                expires: now + Duration::from_secs(lifetime.into()),
                username: user,
                perms: HashMap::new(),
                channels: HashMap::new(),
            };
            self.allocs.insert(from, alloc);
            self.relays.insert(relayed, from);
            self.stats.allocations += 1;
            let ok = ok
                .xor_address(attr::XOR_RELAYED_ADDRESS, relayed)
                .xor_address(attr::XOR_MAPPED_ADDRESS, from)
                .u32(attr::LIFETIME, lifetime);
            return reply(ok.integrity(&key));
        }
        let Some(a) = self.allocs.get_mut(&from) else { return fail(437) };
        if a.username != user {
            return fail(441);
        }
        match m.method {
            method::REFRESH => {
                let lifetime = m.lifetime().unwrap_or(DEFAULT_LIFETIME).min(MAX_LIFETIME);
                if lifetime == 0 {
                    let relayed = a.relayed;
                    self.allocs.remove(&from);
                    self.relays.remove(&relayed);
                    self.stats.deallocations += 1;
                } else {
                    a.expires = now + Duration::from_secs(lifetime.into());
                    self.stats.refreshes += 1;
                }
                reply(ok.u32(attr::LIFETIME, lifetime).integrity(&key))
            }
            method::CREATE_PERMISSION => {
                let peers: Vec<SocketAddr> = m.xor_addresses(attr::XOR_PEER_ADDRESS).collect();
                if peers.is_empty() {
                    return fail(400);
                }
                for p in peers {
                    a.perms.insert(p.ip(), now + PERMISSION_LIFETIME);
                    self.stats.permissions += 1;
                }
                reply(ok.integrity(&key))
            }
            method::CHANNEL_BIND => {
                let (Some(ch), Some(peer)) = (m.channel(), m.xor_address(attr::XOR_PEER_ADDRESS)) else {
                    return fail(400);
                };
                // A channel stays with one peer, and a peer with one channel, while bound.
                let taken = a.channels.get(&ch).is_some_and(|&(p, until)| p != peer && now < until)
                    || a.channel_of(now, peer).is_some_and(|c| c != ch);
                if taken {
                    return fail(400);
                }
                a.channels.insert(ch, (peer, now + CHANNEL_LIFETIME));
                a.perms.insert(peer.ip(), now + PERMISSION_LIFETIME);
                self.stats.channel_binds += 1;
                reply(ok.integrity(&key))
            }
            _ => fail(400),
        }
    }

    /// A datagram from a peer, at relayed address `relayed`: to its client, if it permits the peer.
    pub fn handle_peer(&mut self, now: Instant, relayed: SocketAddr, from: SocketAddr, b: &[u8]) -> Option<Out> {
        self.expire(now);
        let a = self.relays.get(&relayed).and_then(|c| Some((*c, self.allocs.get(c)?)));
        let Some((client, a)) = a.filter(|(_, a)| a.permits(now, from.ip())) else {
            self.stats.dropped += 1;
            return None;
        };
        let data = match a.channel_of(now, from) {
            Some(ch) => {
                self.stats.channel_data_out += 1;
                stun::channel_data(ch, b)
            }
            None => {
                self.stats.data_indications += 1;
                let tx: stun::TxId = std::array::from_fn(|i| (self.stats.data_indications >> (i % 8 * 8)) as u8);
                MessageBuilder::new(method::DATA, Class::Indication, &tx)
                    .xor_address(attr::XOR_PEER_ADDRESS, from)
                    .attr(attr::DATA, b)
                    .finish()
            }
        };
        Some(Out::ToClient { to: client, data })
    }

    fn challenge(&self, b: MessageBuilder) -> MessageBuilder {
        b.attr(attr::REALM, self.realm.as_bytes()).attr(attr::NONCE, self.nonce.as_bytes())
    }

    /// The long-term key for `user`, if it is one: a static user's, or a REST username not yet expired.
    fn key(&self, now: Instant, user: &str) -> Option<[u8; 16]> {
        let password = match (self.users.get(user), &self.secret) {
            (Some(p), _) => p.clone(),
            (None, Some(secret)) => {
                let expiry: u64 = user.split_once(':')?.0.parse().ok()?;
                if expiry < self.unix_at_zero + now.as_micros() / 1_000_000 {
                    return None;
                }
                stun::rest_password(secret, user)
            }
            (None, None) => return None,
        };
        Some(stun::long_term_key(user, &self.realm, &password))
    }
}
