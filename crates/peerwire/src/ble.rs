//! Bluetooth LE (feature `ble`): links with peers nearby over a radio, with no network (`docs/concepts/bluetooth.md`).
//!
//! A node given a [`Radio`] ([`Builder::bluetooth`](crate::Builder::bluetooth)) advertises while it is in a room,
//! connects to the members it hears, proves the room on each connection, and then carries room hellos, offers and
//! datagrams over it: a Bluetooth connection is one more path for a link, ranked below every UDP one
//! ([`PathKind::Bluetooth`](crate::PathKind::Bluetooth)). The node does all of that itself; a driver only advertises,
//! scans, connects and moves GATT values. Feature `ble-bluez` has the Linux driver, `BlueZ` (`ble::BlueZ`).
//!
//! Like any carrier it is untrusted: everything that crosses it is sealed and proved as it is over a relay, and a
//! listener on the radio sees adverts that change every 15 minutes, sizes and timing, not who is in the room.

use crate::signaling::Topic;
use crate::traversal::Hop;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use peerwire_core::ble::{
    hs1_id, value_len, Chunker, Reassembler, KIND_ANSWER, KIND_CHALLENGE, KIND_DATAGRAM, KIND_PROOF, KIND_SIGNAL,
    MAX_MESSAGE,
};
use peerwire_core::{Endpoint, Instant, PeerId};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::task::Waker;
use std::time::Duration;

#[cfg(feature = "ble-bluez")]
pub use crate::bluez::BlueZ;
pub use peerwire_core::ble::{MIN_ATT_MTU, RX_UUID, SERVICE_UUID, TX_UUID};

/// A device a radio heard advertising, by the driver's own number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(pub u64);

/// A GATT connection, by the driver's own number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnId(pub u32);

/// What a radio holds at once ([`Radio::limits`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Connections, in either role.
    pub connections: usize,
    /// Of those, ones not yet proven or bound to a peer: more are closed at once.
    pub unbound: usize,
    /// Of those, ones a central made to this radio.
    pub centrals: usize,
    /// GATT values queued to send on one connection: more are dropped, never waited for.
    pub queue: usize,
}

impl Limits {
    /// A desktop's: 8 connections, 4 unbound, one central (BlueZ sends a notification to every subscribed central),
    /// 64 values queued.
    pub const DESKTOP: Limits = Limits { connections: 8, unbound: 4, centrals: 1, queue: 64 };
    /// A small board's, such as an ESP32's: 3 connections, 2 unbound, 16 values queued.
    pub const SMALL: Limits = Limits { connections: 3, unbound: 2, centrals: 3, queue: 16 };
}

impl Default for Limits {
    fn default() -> Limits {
        Limits::DESKTOP
    }
}

/// A Bluetooth LE radio as a node drives it: it serves the GATT service ([`SERVICE_UUID`], with [`RX_UUID`] and
/// [`TX_UUID`]), advertises, scans, connects and moves GATT values, and never blocks. Nothing is paired or bonded.
pub trait Radio: Send {
    /// Advertises with `data` as the service data, replacing the last; `None` stops. Serves the GATT service, and
    /// accepts centrals, while advertising. Each call with data should come from a new random address.
    fn advertise(&mut self, data: Option<&[u8]>);
    /// Starts or stops scanning, passively, for the service's adverts.
    fn scan(&mut self, on: bool);
    /// Connects to `device` (from [`RadioEvent::Seen`]) as a central, subscribes to `tx`, and asks for the largest
    /// ATT MTU.
    fn connect(&mut self, device: DeviceId);
    /// Sends one value of at most ATT MTU - 3 bytes on `conn`: a write without response to `rx` as the central, a
    /// notification on `tx` to that one central as the peripheral. `false` if it can't take it now (its buffer is
    /// full): the node keeps it and tries again later.
    fn send(&mut self, conn: ConnId, value: &[u8]) -> bool;
    /// Closes `conn`.
    fn disconnect(&mut self, conn: ConnId);
    /// The next event, without blocking.
    fn poll(&mut self) -> Option<RadioEvent>;
    /// What it holds at once. [`Limits::DESKTOP`] unless the driver says otherwise.
    fn limits(&self) -> Limits {
        Limits::DESKTOP
    }
    /// Takes a waker to wake whenever it queues an event, so a node asleep until its next timer
    /// ([`Node::set_waker`](crate::Node::set_waker), [`AsyncNode`](crate::AsyncNode)) polls at once. A driver that
    /// never wakes it still works: a node with a radio polls at least every 50 ms.
    fn set_waker(&mut self, waker: Waker) {
        let _ = waker;
    }
}

/// What a [`Radio`] reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RadioEvent {
    /// An advert of the service, and its service data.
    Seen { device: DeviceId, data: Vec<u8> },
    /// A connection is up: one this radio made to `device`, or a central's (`None`), with its ATT MTU.
    Connected { conn: ConnId, device: Option<DeviceId>, mtu: u16 },
    /// A connect to `device` failed.
    ConnectFailed { device: DeviceId },
    /// The ATT MTU of `conn` changed.
    Mtu { conn: ConnId, mtu: u16 },
    /// A value the other end sent on `conn`: a write to `rx`, or a notification on `tx`.
    Value { conn: ConnId, value: Vec<u8> },
    /// `conn` closed.
    Disconnected { conn: ConnId },
}

/// Adverts change this often, with a new `n`, tag and address.
const ROTATE: Duration = Duration::from_secs(15 * 60);
/// A connection that hasn't proven the room by then closes.
const PROVE_WITHIN: Duration = Duration::from_secs(10);
/// A proven connection bound to no peer stays open while a signal that proves a room member comes over it this often.
const UNBOUND_FOR: Duration = Duration::from_secs(60);
/// No connect again to an advert whose last connection closed or failed for this long.
const RECONNECT_HOLD: Duration = Duration::from_secs(60);
/// A connect the driver never answers is given up after this.
const CONNECT_WAIT: Duration = Duration::from_secs(30);
/// Adverts looked at: a burst, then one per `ADVERT_REFILL` (50 a second).
const ADVERT_BURST: u32 = 50;
const ADVERT_REFILL: Duration = Duration::from_millis(20);
/// An advert heard again from the same device with the same bytes within this is skipped.
const SEEN_FOR: Duration = Duration::from_secs(10);
const MAX_SEEN: usize = 256;
/// One distinct HS1 per unbound connection in this window, its resends passing (BLE-R9): a half-open handshake's
/// life (`Config::handshake_timeout`), so such a connection holds at most one at a time, and a dial again after the
/// last one ended (the network went, say) gets through.
const HS1_WINDOW: Duration = Duration::from_secs(10);
/// HS1s on a bound connection: a burst, then one per `BOUND_HS1_REFILL`.
const BOUND_HS1_BURST: u32 = 4;
const BOUND_HS1_REFILL: Duration = Duration::from_secs(5);
/// Signals taken per connection: a burst, then one per `SIGNAL_REFILL`; the rest dropped before any cryptography.
const SIGNAL_BURST: u32 = 16;
const SIGNAL_REFILL: Duration = Duration::from_millis(500);
/// Messages dropped or refused on a connection before it closes.
const MAX_DROPS: u64 = 32;
const CHALLENGE: usize = 16;
const MAC: usize = 32;
/// The advert's service data: flags, `n`, tag.
const ADVERT_LEN: usize = 9;
const FULL: u8 = 1;
/// A signal's inner kinds: a room hello, or a message sealed to the receiver.
const INNER_ROOM: u8 = 1;
const INNER_PEER: u8 = 2;

/// The room's ble key: the room's name hashed under a label of its own, never the topic's.
pub(crate) fn ble_key(app_id: &str, room: &str) -> [u8; 32] {
    Sha256::digest(format!("peerwire/room-ble/{app_id}/{room}")).into()
}

fn hmac(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// The 9 bytes an advert carries: flags, `n` and the tag for `n` under the ble key.
pub(crate) fn advert(key: &[u8; 32], n: u32, full: bool) -> [u8; ADVERT_LEN] {
    let mut a = [0u8; ADVERT_LEN];
    a[0] = if full { FULL } else { 0 };
    a[1..5].copy_from_slice(&n.to_le_bytes());
    a[5..].copy_from_slice(&hmac(key, &[b"pw-ble-adv", &n.to_le_bytes()])[..4]);
    a
}

/// A token bucket.
#[derive(Clone, Copy, Debug, Default)]
struct Bucket {
    tokens: u32,
    at: Option<Instant>,
}

impl Bucket {
    /// Takes a token: a full `burst` at first, then one more each `refill`.
    fn take(&mut self, now: Instant, burst: u32, refill: Duration) -> bool {
        match self.at {
            None => (self.tokens, self.at) = (burst, Some(now)),
            Some(at) => {
                let fresh = now.saturating_duration_since(at).as_micros() / refill.as_micros().max(1);
                let fresh = u32::try_from(fresh).unwrap_or(u32::MAX);
                if fresh > 0 {
                    self.tokens = self.tokens.saturating_add(fresh).min(burst);
                    self.at = Some(if self.tokens == burst { now } else { at + refill * fresh });
                }
            }
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

/// Where a connection's room proof stands.
enum Proof {
    /// The peripheral sent this challenge.
    Challenged([u8; CHALLENGE]),
    /// The central waits for the challenge.
    Awaiting,
    /// The central answered `c_p` with `c_c`, and waits for the proof.
    Answered([u8; CHALLENGE], [u8; CHALLENGE]),
    /// Both ends hold the room's ble key: signals are sealed under `key`.
    Proven { key: [u8; 32], tx: u64, rx: u64 },
}

struct Conn {
    hop: Hop<SocketAddr>,
    /// Counts the connections made: the newer, the larger.
    made: u32,
    /// This end is the central.
    central: bool,
    /// The `n` of the other end's advert: the one this end connected to, or the one the central named in its answer.
    n: Option<u32>,
    mtu: u16,
    chunker: Chunker,
    reassembler: Reassembler,
    queue: VecDeque<Vec<u8>>,
    proof: Proof,
    /// The room it was proven in ([`Ble::gen`]): it carries signals only for that one.
    gen: u32,
    bound: Option<PeerId>,
    /// The bound peer's inbox, where its signals go.
    inbox: Option<Topic>,
    /// The peer whose signals on it last proved a room member: the other end, unless it lies.
    heard_from: Option<PeerId>,
    opened: Instant,
    /// When the last signal on it proved a room member (or it was proven).
    heard: Instant,
    signals: Bucket,
    /// Unbound: the one HS1 it took, and when.
    hs1: Option<((u32, [u8; 32]), Instant)>,
    /// Bound: the last HS1 it took, and their budget.
    last_hs1: Option<(u32, [u8; 32])>,
    hs1s: Bucket,
    drops: u64,
    /// The reassembler's drops already counted in `drops`.
    counted: u64,
}

impl Conn {
    fn proven(&self) -> bool {
        matches!(self.proof, Proof::Proven { .. })
    }
}

/// What the node does with what came over the radio.
pub(crate) enum Inbound {
    /// A signal, as from this carrier's `topic`, that came over `via`.
    Signal { topic: Topic, payload: Vec<u8>, via: Hop<SocketAddr>, conn: ConnId },
    /// A connection proved the room: a hello should go out soon.
    Proven,
    /// A connection closed: no dial goes over it any more.
    Closed(Hop<SocketAddr>),
}

/// A node's side of Bluetooth: everything but the driver.
pub(crate) struct Ble {
    radio: Box<dyn Radio>,
    limits: Limits,
    me: PeerId,
    /// The room's ble key and topic, while in one.
    room: Option<([u8; 32], Topic)>,
    /// Counts rooms joined: a connection proven in another carries no signals.
    gen: u32,
    /// Off under relay-only.
    enabled: bool,
    /// Advertising and scanning.
    on: bool,
    n: u32,
    /// What it advertises now: `n` and whether full.
    advertised: Option<(u32, bool)>,
    rotate_at: Instant,
    conns: HashMap<ConnId, Conn>,
    by_hop: HashMap<SocketAddr, ConnId>,
    next_hop: u32,
    /// Connects under way: the device and the advert's `n`.
    pending: HashMap<DeviceId, (u32, Instant)>,
    /// Adverts not to connect to again until then.
    held: HashMap<u32, Instant>,
    /// Adverts lately looked at, and whether each was this room's: another room's is skipped, this room's is
    /// judged again without its tag checked again.
    seen: VecDeque<(DeviceId, [u8; ADVERT_LEN], Instant, bool)>,
    adverts: Bucket,
    out: VecDeque<Inbound>,
}

impl Ble {
    pub(crate) fn new(radio: Box<dyn Radio>, me: PeerId, now: Instant) -> Ble {
        let limits = radio.limits();
        Ble {
            radio,
            limits,
            me,
            room: None,
            gen: 0,
            enabled: true,
            on: false,
            n: OsRng.next_u32(),
            advertised: None,
            rotate_at: now + ROTATE,
            conns: HashMap::new(),
            by_hop: HashMap::new(),
            next_hop: 0,
            pending: HashMap::new(),
            held: HashMap::new(),
            seen: VecDeque::new(),
            adverts: Bucket::default(),
            out: VecDeque::new(),
        }
    }

    /// Joins `room` (its ble key and topic), or leaves (`None`): a new `n`, and connections not bound to a peer close.
    pub(crate) fn set_room(&mut self, now: Instant, room: Option<([u8; 32], Topic)>) {
        self.room = room;
        self.gen = self.gen.wrapping_add(1);
        self.n = OsRng.next_u32();
        self.rotate_at = now + ROTATE;
        let unbound: Vec<ConnId> = self.conns.iter().filter(|(_, c)| c.bound.is_none()).map(|(i, _)| *i).collect();
        for c in unbound {
            self.close(c);
        }
        self.refresh(now);
    }

    /// Turns the radio off (relay-only) or back on.
    pub(crate) fn set_enabled(&mut self, now: Instant, on: bool) {
        self.enabled = on;
        if !on {
            let all: Vec<ConnId> = self.conns.keys().copied().collect();
            for c in all {
                self.close(c);
            }
        }
        self.refresh(now);
    }

    /// Hands the radio a waker ([`Radio::set_waker`]).
    pub(crate) fn set_waker(&mut self, waker: Waker) {
        self.radio.set_waker(waker);
    }

    /// The next thing for the node to handle.
    pub(crate) fn take(&mut self) -> Option<Inbound> {
        self.out.pop_front()
    }

    /// A signal on `conn` proved `peer` a room member: it stays open while unbound. A peer keeps one connection, the
    /// one bound to it if any, else the oldest it was heard on: this one closes if it is another.
    pub(crate) fn heard(&mut self, conn: ConnId, now: Instant, peer: PeerId) {
        let Some(age) = self.conns.get(&conn).map(|c| (c.opened, c.made)) else { return };
        let other = |(i, c): (&ConnId, &Conn)| {
            let first = c.bound.is_none() && c.heard_from == Some(peer) && (c.opened, c.made) < age;
            *i != conn && (c.bound == Some(peer) || first)
        };
        if self.conns.iter().any(other) {
            return self.close(conn);
        }
        if let Some(c) = self.conns.get_mut(&conn) {
            (c.heard, c.heard_from) = (now, Some(peer));
        }
    }

    /// Handles what the radio reported, and the timers, handing datagrams to `ep`; then sends what is queued.
    pub(crate) fn poll(&mut self, now: Instant, ep: &mut Endpoint<Hop<SocketAddr>>) {
        // A radio that floods can't hold the node: what is left waits for the next poll.
        for _ in 0..1024 {
            let Some(e) = self.radio.poll() else { break };
            self.on_event(now, ep, e);
        }
        self.timers(now);
        self.send_queued();
    }

    fn on_event(&mut self, now: Instant, ep: &mut Endpoint<Hop<SocketAddr>>, e: RadioEvent) {
        match e {
            RadioEvent::Seen { device, data } => self.on_seen(now, device, &data),
            RadioEvent::Connected { conn, device, mtu } => self.on_connected(now, conn, device, mtu),
            RadioEvent::ConnectFailed { device } => {
                if let Some((n, _)) = self.pending.remove(&device) {
                    self.held.insert(n, now + RECONNECT_HOLD);
                }
            }
            RadioEvent::Mtu { conn, mtu } => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.mtu = mtu;
                }
            }
            RadioEvent::Value { conn, value } => self.on_value(now, ep, conn, &value),
            RadioEvent::Disconnected { conn } => {
                if let Some(c) = self.conns.get(&conn) {
                    if let Some(n) = c.n {
                        self.held.insert(n, now + RECONNECT_HOLD);
                    }
                }
                self.forget(conn);
                self.refresh(now);
            }
        }
    }

    fn on_seen(&mut self, now: Instant, device: DeviceId, data: &[u8]) {
        let Some(((key, _), data)) = self.room.zip(<[u8; ADVERT_LEN]>::try_from(data).ok()) else { return };
        if !self.on || data[0] & !FULL != 0 {
            return;
        }
        let n = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
        match self.seen.iter().find(|(d, a, ..)| *d == device && *a == data) {
            Some(&(.., ours)) if !ours => return,
            Some(_) => {}
            None => {
                if !self.adverts.take(now, ADVERT_BURST, ADVERT_REFILL) {
                    return;
                }
                if self.seen.len() >= MAX_SEEN {
                    self.seen.pop_front();
                }
                let ours = advert(&key, n, data[0] & FULL != 0) == data;
                self.seen.push_back((device, data, now, ours));
                if !ours {
                    return; // another room's
                }
            }
        }
        // Not our own, and not while a central's connection says nothing yet of the advert it is behind.
        let unsure = self.conns.values().any(|c| !c.central && !c.proven());
        if n == self.n || unsure {
            return;
        }
        // The smaller `n` connects to an advert that isn't full; the larger one does if its own is.
        let theirs_full = data[0] & FULL != 0;
        if theirs_full || !(self.n < n || self.full()) {
            return;
        }
        let known = self.conns.values().any(|c| c.n == Some(n)) || self.pending.values().any(|(m, _)| *m == n);
        if known || self.held.get(&n).is_some_and(|t| now < *t) || !self.room_out() {
            return;
        }
        self.pending.insert(device, (n, now));
        self.radio.connect(device);
    }

    /// Whether a connection this end makes fits: one more connection, and one more unbound.
    fn room_out(&self) -> bool {
        let unbound = self.conns.values().filter(|c| c.bound.is_none()).count() + self.pending.len();
        self.conns.len() + self.pending.len() < self.limits.connections && unbound < self.limits.unbound
    }

    /// Whether this radio takes no central now (its advert says so): it holds all the connections it can, or all the
    /// centrals. A central over the unbound limit is taken, and the oldest unbound connection closes for it (NB110-2).
    fn full(&self) -> bool {
        let centrals = self.conns.values().filter(|c| !c.central).count();
        self.conns.len() + self.pending.len() >= self.limits.connections || centrals >= self.limits.centrals
    }

    /// Closes the oldest connection bound to no peer, if this radio holds as many as it may.
    fn evict_unbound(&mut self) {
        let unbound = self.conns.values().filter(|c| c.bound.is_none()).count() + self.pending.len();
        if unbound >= self.limits.unbound {
            let oldest = self.conns.iter().filter(|(_, c)| c.bound.is_none()).min_by_key(|(_, c)| (c.opened, c.made));
            if let Some(&conn) = oldest.map(|(i, _)| i) {
                self.close(conn);
            }
        }
    }

    fn on_connected(&mut self, now: Instant, conn: ConnId, device: Option<DeviceId>, mtu: u16) {
        let n = match device {
            Some(d) => match self.pending.remove(&d) {
                Some((n, _)) => Some(n),
                None => return self.radio.disconnect(conn), // not one we asked for
            },
            None if !self.on || self.full() => return self.radio.disconnect(conn),
            None => {
                self.evict_unbound();
                None
            }
        };
        if self.room.is_none() {
            return self.radio.disconnect(conn);
        }
        self.next_hop = self.next_hop.wrapping_add(1) & 0x00ff_ffff;
        let [_, a, b, c] = self.next_hop.to_be_bytes();
        let at = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(0, a, b, c), 0));
        let mut challenge = [0u8; CHALLENGE];
        OsRng.fill_bytes(&mut challenge);
        let central = device.is_some();
        let c = Conn {
            hop: Hop::Ble(at),
            made: self.next_hop,
            central,
            n,
            mtu,
            chunker: Chunker::new(),
            reassembler: Reassembler::new(),
            queue: VecDeque::new(),
            proof: if central { Proof::Awaiting } else { Proof::Challenged(challenge) },
            gen: self.gen,
            bound: None,
            inbox: None,
            heard_from: None,
            opened: now,
            heard: now,
            signals: Bucket::default(),
            hs1: None,
            last_hs1: None,
            hs1s: Bucket::default(),
            drops: 0,
            counted: 0,
        };
        if let Some(old) = self.by_hop.insert(at, conn) {
            self.forget(old);
        }
        self.conns.insert(conn, c);
        if !central {
            self.send(conn, &[&[KIND_CHALLENGE], &challenge]);
        }
        self.refresh(now);
    }

    fn on_value(&mut self, now: Instant, ep: &mut Endpoint<Hop<SocketAddr>>, conn: ConnId, value: &[u8]) {
        let Some(c) = self.conns.get_mut(&conn) else { return };
        let Some(message) = c.reassembler.push(now, value).map(<[u8]>::to_vec) else {
            return self.count_codec(conn);
        };
        self.count_codec(conn);
        self.on_message(now, ep, conn, &message);
    }

    /// Counts the codec's new drops against `conn`.
    fn count_codec(&mut self, conn: ConnId) {
        let Some(c) = self.conns.get_mut(&conn) else { return };
        let d = c.reassembler.dropped();
        let fresh = d - c.counted;
        c.counted = d;
        self.dropped(conn, fresh);
    }

    /// Counts `n` drops against `conn`, and closes it at `MAX_DROPS`.
    fn dropped(&mut self, conn: ConnId, n: u64) {
        let Some(c) = self.conns.get_mut(&conn) else { return };
        c.drops += n;
        if c.drops >= MAX_DROPS {
            self.close(conn);
        }
    }

    fn on_message(&mut self, now: Instant, ep: &mut Endpoint<Hop<SocketAddr>>, conn: ConnId, m: &[u8]) {
        let Some(((key, topic), c)) = self.room.zip(self.conns.get_mut(&conn)) else { return };
        let (kind, body) = (m[0], &m[1..]);
        match (&c.proof, kind) {
            (Proof::Challenged(c_p), KIND_ANSWER) if body.len() == CHALLENGE + 4 + MAC => {
                let c_p = *c_p;
                let (c_c, n) = (&body[..CHALLENGE], &body[CHALLENGE..CHALLENGE + 4]);
                if !same(&hmac(&key, &[b"pw-ble-c", &c_p, c_c, n]), &body[CHALLENGE + 4..]) {
                    return self.close(conn);
                }
                // The central's advert, which this end then won't connect to: 0 if it has none.
                let n = u32::from_le_bytes(n.try_into().expect("4 bytes"));
                c.n = (n != 0).then_some(n);
                let link = hmac(&key, &[b"pw-ble-k", &c_p, c_c]);
                let proof = hmac(&key, &[b"pw-ble-p", c_c, &c_p]);
                (c.proof, c.heard) = (Proof::Proven { key: link, tx: 0, rx: 0 }, now);
                self.send(conn, &[&[KIND_PROOF], &proof]);
                self.out.push_back(Inbound::Proven);
            }
            (Proof::Awaiting, KIND_CHALLENGE) if body.len() == CHALLENGE => {
                let mut c_p = [0u8; CHALLENGE];
                c_p.copy_from_slice(body);
                let mut c_c = [0u8; CHALLENGE];
                OsRng.fill_bytes(&mut c_c);
                let n = self.n.to_le_bytes();
                let mac = hmac(&key, &[b"pw-ble-c", &c_p, &c_c, &n]);
                c.proof = Proof::Answered(c_p, c_c);
                self.send(conn, &[&[KIND_ANSWER], &c_c, &n, &mac]);
            }
            (Proof::Answered(c_p, c_c), KIND_PROOF) if body.len() == MAC => {
                if !same(&hmac(&key, &[b"pw-ble-p", c_c, c_p]), body) {
                    return self.close(conn);
                }
                let link = hmac(&key, &[b"pw-ble-k", c_p, c_c]);
                (c.proof, c.heard) = (Proof::Proven { key: link, tx: 0, rx: 0 }, now);
                self.out.push_back(Inbound::Proven);
            }
            // Before the proof, nothing else: a connection that tries closes.
            (Proof::Challenged(_) | Proof::Awaiting | Proof::Answered(..), _) => self.close(conn),
            (Proof::Proven { .. }, KIND_SIGNAL) => self.on_signal(now, conn, topic, body),
            (Proof::Proven { .. }, KIND_DATAGRAM) if !body.is_empty() => self.on_datagram(now, ep, conn, body),
            (Proof::Proven { .. }, _) => self.dropped(conn, 1),
        }
    }

    fn on_signal(&mut self, now: Instant, conn: ConnId, room: Topic, body: &[u8]) {
        let Some(c) = self.conns.get_mut(&conn) else { return };
        let fresh_room = c.gen == self.gen;
        if !fresh_room || body.len() < 8 || !c.signals.take(now, SIGNAL_BURST, SIGNAL_REFILL) {
            return self.dropped(conn, 1);
        }
        let Proof::Proven { key, rx, .. } = &mut c.proof else { return };
        let seq = u64::from_le_bytes(body[..8].try_into().expect("8 bytes"));
        // From the other end: the direction it seals under is the other one.
        let dir = if c.central { 1u32 } else { 0 };
        let opened = (seq > *rx).then(|| open(key, dir, seq, &body[8..])).flatten();
        let Some(plain) = opened.filter(|p| !p.is_empty()) else { return self.dropped(conn, 1) };
        *rx = seq;
        let topic = match plain[0] {
            INNER_ROOM => room,
            INNER_PEER => Topic::peer(&self.me),
            _ => return self.dropped(conn, 1),
        };
        let via = c.hop;
        self.out.push_back(Inbound::Signal { topic, payload: plain[1..].to_vec(), via, conn });
    }

    fn on_datagram(&mut self, now: Instant, ep: &mut Endpoint<Hop<SocketAddr>>, conn: ConnId, d: &[u8]) {
        let Some(c) = self.conns.get_mut(&conn) else { return };
        let before = ep.stats().rx_wrong_peer;
        let id = hs1_id(d);
        match c.bound {
            Some(p) => {
                if let Some(id) = id.filter(|id| c.last_hs1 != Some(*id)) {
                    if !c.hs1s.take(now, BOUND_HS1_BURST, BOUND_HS1_REFILL) {
                        return self.dropped(conn, 1);
                    }
                    c.last_hs1 = Some(id);
                }
                ep.handle_datagram_from(now, c.hop, &p, d);
            }
            None => {
                if let Some(id) = id {
                    match c.hs1 {
                        Some((first, at)) if first != id && now.saturating_duration_since(at) < HS1_WINDOW => {
                            return self.dropped(conn, 1);
                        }
                        Some((first, _)) if first == id => {}
                        _ => c.hs1 = Some((id, now)),
                    }
                }
                if let Some(peer) = ep.handle_datagram_unbound(now, c.hop, d) {
                    self.bind(conn, peer);
                }
            }
        }
        let wrong = ep.stats().rx_wrong_peer - before;
        self.dropped(conn, wrong);
    }

    /// Binds `conn` to `peer` for as long as it lasts; another connection bound to `peer`, or bound to no one and
    /// heard from it, closes.
    fn bind(&mut self, conn: ConnId, peer: PeerId) {
        let theirs = |c: &Conn| c.bound == Some(peer) || c.bound.is_none() && c.heard_from == Some(peer);
        let older = self.conns.iter().filter(|(i, c)| **i != conn && theirs(c)).map(|(i, _)| *i);
        let older: Vec<ConnId> = older.collect();
        for o in older {
            self.close(o);
        }
        if let Some(c) = self.conns.get_mut(&conn) {
            (c.bound, c.inbox) = (Some(peer), Some(Topic::peer(&peer)));
        }
    }

    /// Sends a datagram the endpoint addressed to `hop`; dropped if no proven connection is there.
    pub(crate) fn send_datagram(&mut self, hop: &SocketAddr, payload: &[u8]) {
        if let Some(&conn) = self.by_hop.get(hop).filter(|c| self.conns.get(c).is_some_and(Conn::proven)) {
            self.send(conn, &[&[KIND_DATAGRAM], payload]);
        }
    }

    /// Sends a signal the traversal published on `topic`: a room hello on every connection proven in this room, a
    /// message for a peer's inbox on the connection bound to that peer, or else on every one bound to no other.
    pub(crate) fn publish(&mut self, topic: Topic, payload: &[u8]) {
        let Some((_, room)) = self.room else { return };
        let inner = if topic == room { INNER_ROOM } else { INNER_PEER };
        let live = |c: &Conn| c.proven() && c.gen == self.gen;
        let mut to: Vec<ConnId> = Vec::new();
        if inner == INNER_PEER {
            to = self.conns.iter().filter(|(_, c)| live(c) && c.inbox == Some(topic)).map(|(i, _)| *i).collect();
        }
        if to.is_empty() {
            let open = |c: &Conn| inner == INNER_ROOM || c.bound.is_none();
            to = self.conns.iter().filter(|(_, c)| live(c) && open(c)).map(|(i, _)| *i).collect();
        }
        for conn in to {
            let Some(c) = self.conns.get_mut(&conn) else { continue };
            let Proof::Proven { key, tx, .. } = &mut c.proof else { continue };
            *tx += 1;
            let dir = if c.central { 0u32 } else { 1 };
            let mut plain = Vec::with_capacity(1 + payload.len());
            plain.push(inner);
            plain.extend_from_slice(payload);
            let Some(sealed) = seal(key, dir, *tx, &plain) else { continue };
            let seq = tx.to_le_bytes();
            self.send(conn, &[&[KIND_SIGNAL], &seq, &sealed]);
        }
    }

    /// Queues a message, in its chunks, on `conn`: the whole of it, or none if the queue lacks room.
    fn send(&mut self, conn: ConnId, parts: &[&[u8]]) {
        let Some(c) = self.conns.get_mut(&conn) else { return };
        let m = parts.concat();
        let room = value_len(c.mtu) - 1;
        if m.len() > MAX_MESSAGE || c.queue.len() + m.len().div_ceil(room) > self.limits.queue {
            return;
        }
        let queue = &mut c.queue;
        c.chunker.split(&m, c.mtu, |v| {
            queue.push_back(v.to_vec());
            true
        });
    }

    /// Hands the radio what is queued, until it takes no more.
    pub(crate) fn send_queued(&mut self) {
        for (conn, c) in &mut self.conns {
            while let Some(v) = c.queue.front() {
                if !self.radio.send(*conn, v) {
                    break;
                }
                c.queue.pop_front();
            }
        }
    }

    fn timers(&mut self, now: Instant) {
        let late: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| match c.proven() {
                false => now.saturating_duration_since(c.opened) > PROVE_WITHIN,
                true => c.bound.is_none() && now.saturating_duration_since(c.heard) > UNBOUND_FOR,
            })
            .map(|(i, _)| *i)
            .collect();
        for c in late {
            self.close(c);
        }
        self.pending.retain(|_, (_, at)| now.saturating_duration_since(*at) < CONNECT_WAIT);
        self.held.retain(|_, t| now < *t);
        self.seen.retain(|(_, _, at, _)| now.saturating_duration_since(*at) < SEEN_FOR);
        if now >= self.rotate_at {
            self.rotate_at = now + ROTATE;
            self.n = OsRng.next_u32();
            self.advertised = None;
        }
        self.refresh(now);
    }

    /// Advertises and scans as the room and the connections have it, with a new `n` when the advert changes.
    fn refresh(&mut self, _now: Instant) {
        let on = self.enabled && self.room.is_some();
        if on != self.on {
            self.on = on;
            self.radio.scan(on);
            if !on {
                self.advertised = None;
                self.radio.advertise(None);
            }
        }
        let Some((key, _)) = self.room.filter(|_| on) else { return };
        let full = self.full();
        if self.advertised.is_some_and(|(n, f)| n == self.n && f == full) {
            return;
        }
        if self.advertised.is_some() {
            self.n = OsRng.next_u32(); // a new advert, from a new address: a new `n` with it
        }
        self.advertised = Some((self.n, full));
        self.radio.advertise(Some(&advert(&key, self.n, full)));
    }

    /// Closes `conn` and forgets it.
    fn close(&mut self, conn: ConnId) {
        if self.conns.contains_key(&conn) {
            self.radio.disconnect(conn);
            self.forget(conn);
        }
    }

    fn forget(&mut self, conn: ConnId) {
        if let Some(c) = self.conns.remove(&conn) {
            if let Hop::Ble(at) = c.hop {
                self.by_hop.remove(&at);
            }
            self.out.push_back(Inbound::Closed(c.hop));
        }
    }

    /// Connections open, and how many are proven and bound (for tests).
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn counts(&self) -> (usize, usize, usize) {
        let proven = self.conns.values().filter(|c| c.proven()).count();
        (self.conns.len(), proven, self.conns.values().filter(|c| c.bound.is_some()).count())
    }
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// The GCM nonce of signal `seq` sealed by the end at direction `dir` (0 the central, 1 the peripheral).
fn nonce(dir: u32, seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&dir.to_le_bytes());
    n[4..].copy_from_slice(&seq.to_le_bytes());
    n
}

fn seal(key: &[u8; 32], dir: u32, seq: u64, plain: &[u8]) -> Option<Vec<u8>> {
    Aes256Gcm::new(key.into()).encrypt(Nonce::from_slice(&nonce(dir, seq)), plain).ok()
}

fn open(key: &[u8; 32], dir: u32, seq: u64, sealed: &[u8]) -> Option<Vec<u8>> {
    Aes256Gcm::new(key.into()).decrypt(Nonce::from_slice(&nonce(dir, seq)), sealed).ok()
}

#[cfg(any(test, feature = "testing"))]
pub use fake::{FakeAir, FakeRadio};

/// Radios in one process that hear each other, for tests (feature `testing`).
#[cfg(any(test, feature = "testing"))]
mod fake {
    use super::{ConnId, DeviceId, Limits, Radio, RadioEvent};
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use std::task::Waker;

    /// Radios in one process that hear each other's adverts and connect: [`FakeAir::radio`] makes one. Every value
    /// sent arrives, in order, unless the test cuts a radio off.
    #[derive(Clone)]
    #[doc(hidden)]
    pub struct FakeAir(Arc<Mutex<Air>>);

    struct Air {
        stations: Vec<Station>,
        /// Each end of a connection: (radio, its number) to the other end's.
        links: HashMap<(usize, u32), (usize, u32)>,
        next: u32,
        mtu: u16,
    }

    struct Station {
        advert: Option<Vec<u8>>,
        scanning: bool,
        off: bool,
        events: VecDeque<RadioEvent>,
        limits: Limits,
        polls: u64,
        waker: Option<Waker>,
    }

    impl Air {
        /// Queues `e` for radio `i`, and wakes it.
        fn push(&mut self, i: usize, e: RadioEvent) {
            let s = &mut self.stations[i];
            s.events.push_back(e);
            if let Some(w) = &s.waker {
                w.wake_by_ref();
            }
        }
    }

    /// One radio on a [`FakeAir`].
    #[doc(hidden)]
    pub struct FakeRadio {
        air: FakeAir,
        me: usize,
    }

    impl FakeAir {
        /// An empty medium whose connections have ATT MTU `mtu`.
        pub fn new(mtu: u16) -> FakeAir {
            FakeAir(Arc::new(Mutex::new(Air { stations: Vec::new(), links: HashMap::new(), next: 1, mtu })))
        }

        /// A new radio on it, with these limits.
        pub fn radio(&self, limits: Limits) -> FakeRadio {
            let mut air = self.0.lock().unwrap();
            let s = Station {
                advert: None,
                scanning: false,
                off: false,
                events: VecDeque::new(),
                limits,
                polls: 0,
                waker: None,
            };
            air.stations.push(s);
            FakeRadio { air: self.clone(), me: air.stations.len() - 1 }
        }

        /// Cuts radio `i` off (its connections close, and it hears and is heard by no one), or back on.
        pub fn set_off(&self, i: usize, off: bool) {
            let mut air = self.0.lock().unwrap();
            air.stations[i].off = off;
            if off {
                let ends: Vec<((usize, u32), (usize, u32))> =
                    air.links.iter().filter(|(a, _)| a.0 == i).map(|(a, b)| (*a, *b)).collect();
                for (a, b) in ends {
                    air.links.remove(&a);
                    air.links.remove(&b);
                    air.push(a.0, RadioEvent::Disconnected { conn: ConnId(a.1) });
                    air.push(b.0, RadioEvent::Disconnected { conn: ConnId(b.1) });
                }
            }
        }

        /// Connections open (each counted once).
        pub fn links(&self) -> usize {
            self.0.lock().unwrap().links.len() / 2
        }

        /// The radio at the other end of radio `i`'s connection `conn`.
        pub fn peer_of(&self, i: usize, conn: ConnId) -> Option<usize> {
            self.0.lock().unwrap().links.get(&(i, conn.0)).map(|l| l.0)
        }

        /// What radio `i` advertises.
        pub fn advert(&self, i: usize) -> Option<Vec<u8>> {
            self.0.lock().unwrap().stations[i].advert.clone()
        }
    }

    impl FakeRadio {
        /// Its number, the [`DeviceId`] others see it as.
        pub fn index(&self) -> usize {
            self.me
        }
    }

    impl Radio for FakeRadio {
        fn advertise(&mut self, data: Option<&[u8]>) {
            self.air.0.lock().unwrap().stations[self.me].advert = data.map(<[u8]>::to_vec);
        }

        fn scan(&mut self, on: bool) {
            self.air.0.lock().unwrap().stations[self.me].scanning = on;
        }

        fn connect(&mut self, device: DeviceId) {
            let mut air = self.air.0.lock().unwrap();
            let to = device.0 as usize;
            let reachable = to != self.me
                && !air.stations[self.me].off
                && air.stations.get(to).is_some_and(|s| s.advert.is_some() && !s.off);
            if !reachable {
                air.push(self.me, RadioEvent::ConnectFailed { device });
                return;
            }
            let (a, b) = (air.next, air.next + 1);
            air.next += 2;
            air.links.insert((self.me, a), (to, b));
            air.links.insert((to, b), (self.me, a));
            let mtu = air.mtu;
            air.push(self.me, RadioEvent::Connected { conn: ConnId(a), device: Some(device), mtu });
            air.push(to, RadioEvent::Connected { conn: ConnId(b), device: None, mtu });
        }

        fn send(&mut self, conn: ConnId, value: &[u8]) -> bool {
            let mut air = self.air.0.lock().unwrap();
            if let Some(&(to, theirs)) = air.links.get(&(self.me, conn.0)) {
                air.push(to, RadioEvent::Value { conn: ConnId(theirs), value: value.to_vec() });
            }
            true
        }

        fn disconnect(&mut self, conn: ConnId) {
            let mut air = self.air.0.lock().unwrap();
            if let Some(other) = air.links.remove(&(self.me, conn.0)) {
                air.links.remove(&other);
                air.push(other.0, RadioEvent::Disconnected { conn: ConnId(other.1) });
            }
        }

        fn poll(&mut self) -> Option<RadioEvent> {
            let mut air = self.air.0.lock().unwrap();
            let me = self.me;
            if let Some(e) = air.stations[me].events.pop_front() {
                return Some(e);
            }
            // Every 16th empty poll, the adverts in range, as a scan hears them again and again.
            air.stations[me].polls += 1;
            if !air.stations[me].scanning || air.stations[me].off || !air.stations[me].polls.is_multiple_of(16) {
                return None;
            }
            let heard: Vec<RadioEvent> = (0..air.stations.len())
                .filter(|&i| i != me && !air.stations[i].off)
                .filter_map(|i| {
                    let data = air.stations[i].advert.clone()?;
                    Some(RadioEvent::Seen { device: DeviceId(i as u64), data })
                })
                .collect();
            air.stations[me].events.extend(heard);
            air.stations[me].events.pop_front()
        }

        fn limits(&self) -> Limits {
            self.air.0.lock().unwrap().stations[self.me].limits
        }

        fn set_waker(&mut self, waker: Waker) {
            self.air.0.lock().unwrap().stations[self.me].waker = Some(waker);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peerwire_core::{Config, Identity, Lane, Path};

    fn id(n: u8) -> Identity {
        Identity::from_secret_bytes(&[n; 32])
    }

    #[test]
    fn the_advert_carries_no_lasting_value_and_a_tag_only_members_can_check() {
        let (key, other) = (ble_key("app", "room"), ble_key("app", "other"));
        let a = advert(&key, 7, false);
        assert_eq!(a.len(), 9);
        assert_eq!((a[0], &a[1..5]), (0, &7u32.to_le_bytes()[..]));
        assert_ne!(advert(&other, 7, false), a, "another room's tag");
        assert_eq!(advert(&key, 7, true)[0], FULL);
        assert_eq!(advert(&key, 7, true)[5..], a[5..], "the flag isn't in the tag");
        let b = advert(&key, 8, false);
        assert!(a[1..].iter().zip(&b[1..]).all(|(x, y)| x != y) || a[5..] != b[5..], "a new n, a new tag");
        let topic = Topic::room("app", "room");
        assert_ne!(ble_key("app", "room"), topic.0, "not the topic");
    }

    #[test]
    fn signals_seal_per_direction_and_sequence() {
        let key = [7u8; 32];
        let s = seal(&key, 0, 1, b"hello").unwrap();
        assert_eq!(open(&key, 0, 1, &s).as_deref(), Some(&b"hello"[..]));
        assert_eq!(open(&key, 1, 1, &s), None, "the other direction's nonce");
        assert_eq!(open(&key, 0, 2, &s), None, "another seq");
        assert_eq!(open(&[8u8; 32], 0, 1, &s), None, "another connection's key");
    }

    #[test]
    fn a_radio_wakes_its_node_when_something_comes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let air = FakeAir::new(23);
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let (mut a, mut b) = (air.radio(Limits::DESKTOP), air.radio(Limits::DESKTOP));
        a.advertise(Some(&[0; ADVERT_LEN]));
        a.set_waker(count.clone().into());
        b.connect(DeviceId(0));
        assert_eq!(count.0.load(Ordering::Relaxed), 1, "a central connected");
        let Some(RadioEvent::Connected { conn, .. }) = b.poll() else { panic!() };
        b.send(conn, b"x");
        assert_eq!(count.0.load(Ordering::Relaxed), 2, "a value came");
    }

    /// NB110-1: Alice and Bob linked over their connection, and Mallory, a member, connected to Bob too. Mallory
    /// takes Alice's next packet before Bob has it and sends it over her own connection: Bob drops it unread, his
    /// link to Alice stays where it is, Mallory's connection isn't bound to Alice and Alice's isn't closed; the real
    /// packet, when it comes, still reads.
    #[test]
    fn a_member_cannot_move_another_link_onto_its_connection() {
        let air = FakeAir::new(185);
        let mut now = Instant::from_millis(1);
        let limits = Limits { centrals: 3, ..Limits::DESKTOP };
        let room = Some((ble_key("app", "room"), Topic::room("app", "room")));
        let mut b: Vec<Ble> = (1..=3)
            .map(|i| {
                let mut b = Ble::new(Box::new(air.radio(limits)), id(i).peer_id(), now);
                b.set_room(now, room);
                b
            })
            .collect();
        let mut eps: Vec<Endpoint<Hop<SocketAddr>>> =
            (1..=3).map(|i| Endpoint::new(&id(i), Config::default(), [i; 32])).collect();
        let step = |b: &mut [Ble], eps: &mut [Endpoint<Hop<SocketAddr>>], now: Instant, hold: bool| {
            let mut held = Vec::new();
            for i in 0..b.len() {
                while let Some(t) = eps[i].poll_transmit(now) {
                    if let Hop::Ble(at) = t.dst {
                        match hold && i == 0 {
                            true => held.push((at, t.payload)),
                            false => b[i].send_datagram(&at, &t.payload),
                        }
                    }
                }
                b[i].poll(now, &mut eps[i]);
                while b[i].take().is_some() {}
                eps[i].handle_timeout(now);
            }
            held
        };
        for _ in 0..200 {
            step(&mut b, &mut eps, now, false);
            now = now + Duration::from_millis(10);
        }
        assert!(b.iter().all(|b| b.counts() == (2, 2, 0)), "each linked to the other two by the radio");
        // Alice dials Bob over both her connections; the one to Bob binds.
        let (alice, bob) = (id(1).peer_id(), id(2).peer_id());
        let hops: Vec<Hop<SocketAddr>> = b[0].conns.values().map(|c| c.hop).collect();
        eps[0].connect_with(now, bob, &hops.iter().map(|h| Path::Direct(*h)).collect::<Vec<_>>()).unwrap();
        for _ in 0..200 {
            step(&mut b, &mut eps, now, false);
            now = now + Duration::from_millis(10);
        }
        assert!(eps[0].is_connected(&bob) && eps[1].is_connected(&alice));
        assert_eq!((b[0].counts().2, b[1].counts().2), (1, 1), "bound at both ends");
        let path = eps[1].path(&alice).cloned();
        // Mallory's connection to Bob.
        let to_bob = b[2].conns.iter().find(|(i, _)| air.peer_of(2, **i) == Some(1)).map(|(_, c)| c.hop);
        let Some(Hop::Ble(to_bob)) = to_bob else { panic!("Mallory is connected to Bob") };
        eps[0].send(&bob, Lane::Reliable, b"for bob").unwrap();
        let held = step(&mut b, &mut eps, now, true);
        assert_eq!(held.len(), 1, "Alice's packet, taken");
        let wrong = eps[1].stats().rx_wrong_peer;
        b[2].send_datagram(&to_bob, &held[0].1);
        for _ in 0..5 {
            step(&mut b, &mut eps, now, false);
            now = now + Duration::from_millis(10);
        }
        assert_eq!(eps[1].stats().rx_wrong_peer, wrong + 1, "dropped unread");
        assert_eq!(eps[1].path(&alice).cloned(), path, "the link stays where it was");
        assert_eq!(b[1].counts(), (2, 2, 1), "Mallory's isn't bound to Alice, and Alice's is still open");
        assert!(std::iter::from_fn(|| eps[1].poll_event()).all(|e| !matches!(e, peerwire_core::Event::Message { .. })));
        b[0].send_datagram(&held[0].0, &held[0].1);
        for _ in 0..5 {
            step(&mut b, &mut eps, now, false);
            now = now + Duration::from_millis(10);
        }
        let got = std::iter::from_fn(|| eps[1].poll_event())
            .any(|e| matches!(e, peerwire_core::Event::Message { data, .. } if data == b"for bob"));
        assert!(got, "the real packet still reads");
    }

    #[test]
    fn a_bucket_gives_its_burst_then_refills() {
        let mut b = Bucket::default();
        let t = Instant::from_millis(1000);
        assert!((0..4).all(|_| b.take(t, 4, Duration::from_secs(5))));
        assert!(!b.take(t, 4, Duration::from_secs(5)));
        assert!(!b.take(t + Duration::from_secs(4), 4, Duration::from_secs(5)));
        assert!(b.take(t + Duration::from_secs(5), 4, Duration::from_secs(5)));
        assert!(!b.take(t + Duration::from_secs(5), 4, Duration::from_secs(5)));
    }

    /// Two node sides on one air, in the same room: who connects, the room proof, signals both ways.
    struct Pair {
        air: FakeAir,
        b: [Ble; 2],
        eps: [Endpoint<Hop<SocketAddr>>; 2],
        now: Instant,
        got: [Vec<(Topic, Vec<u8>)>; 2],
    }

    impl Pair {
        fn new(rooms: [&str; 2]) -> Pair {
            let air = FakeAir::new(23);
            let now = Instant::from_millis(1);
            let mk = |i: u8, room: &str| {
                let mut b = Ble::new(Box::new(air.radio(Limits::DESKTOP)), id(i).peer_id(), now);
                b.set_room(now, Some((ble_key("app", room), Topic::room("app", room))));
                b
            };
            let b = [mk(1, rooms[0]), mk(2, rooms[1])];
            let eps = [1, 2].map(|i| Endpoint::new(&id(i), Config::default(), [i; 32]));
            Pair { air, b, eps, now, got: [Vec::new(), Vec::new()] }
        }

        fn run(&mut self, ms: u64) {
            for _ in 0..ms / 10 {
                for i in 0..2 {
                    self.b[i].poll(self.now, &mut self.eps[i]);
                    while let Some(e) = self.b[i].take() {
                        if let Inbound::Signal { topic, payload, conn, .. } = e {
                            self.b[i].heard(conn, self.now, id(2 - i as u8).peer_id());
                            self.got[i].push((topic, payload));
                        }
                    }
                }
                self.now = self.now + Duration::from_millis(10);
            }
        }
    }

    #[test]
    fn members_connect_once_prove_the_room_and_carry_sealed_signals() {
        // Who connects turns on random values: many pairs, never two connections.
        for _ in 0..20 {
            let mut p = Pair::new(["room", "room"]);
            p.run(500);
            assert_eq!(p.air.links(), 1, "one connection, made by the smaller n");
        }
        let mut p = Pair::new(["room", "room"]);
        p.run(500);
        assert!(p.b.iter().all(|b| b.counts() == (1, 1, 0)), "proven, bound to no one");
        let room = Topic::room("app", "room");
        p.b[0].publish(room, b"hello from 1");
        p.b[1].publish(Topic::peer(&id(1).peer_id()), b"offer for 1");
        p.run(100);
        assert_eq!(p.got[1], vec![(room, b"hello from 1".to_vec())]);
        assert_eq!(p.got[0], vec![(Topic::peer(&id(1).peer_id()), b"offer for 1".to_vec())]);
    }

    #[test]
    fn other_rooms_never_connect() {
        let mut p = Pair::new(["room", "other"]);
        p.run(2000);
        assert_eq!(p.air.links(), 0);
    }

    #[test]
    fn a_central_without_the_room_key_is_closed() {
        let air = FakeAir::new(23);
        let now = Instant::from_millis(1);
        let mut node = Ble::new(Box::new(air.radio(Limits::DESKTOP)), id(1).peer_id(), now);
        node.set_room(now, Some((ble_key("app", "room"), Topic::room("app", "room"))));
        let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
        node.poll(now, &mut ep);
        let mut rogue = air.radio(Limits::DESKTOP);
        let mut t = now;
        // A wrong answer closes it at once; one that says nothing, at the deadline.
        for (answer, after) in [(true, 20u64), (false, 10_100)] {
            rogue.connect(DeviceId(0));
            let Some(RadioEvent::Connected { conn, .. }) = rogue.poll() else { panic!() };
            node.poll(t, &mut ep);
            let challenge = rogue.poll();
            assert!(matches!(challenge, Some(RadioEvent::Value { .. })), "{challenge:?}");
            if answer {
                let mut chunker = Chunker::new();
                let mut m = vec![KIND_ANSWER];
                m.extend([0u8; CHALLENGE + 4 + MAC]);
                chunker.split(&m, 23, |v| rogue.send(conn, v));
            }
            t = t + Duration::from_millis(after);
            node.poll(t, &mut ep);
            assert_eq!(rogue.poll(), Some(RadioEvent::Disconnected { conn }));
            assert_eq!(node.counts(), (0, 0, 0));
        }
    }

    #[test]
    fn nothing_but_the_proof_passes_before_it() {
        let air = FakeAir::new(23);
        let now = Instant::from_millis(1);
        let mut node = Ble::new(Box::new(air.radio(Limits::DESKTOP)), id(1).peer_id(), now);
        node.set_room(now, Some((ble_key("app", "room"), Topic::room("app", "room"))));
        let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
        node.poll(now, &mut ep);
        let mut rogue = air.radio(Limits::DESKTOP);
        rogue.connect(DeviceId(0));
        let Some(RadioEvent::Connected { conn, .. }) = rogue.poll() else { panic!() };
        node.poll(now, &mut ep);
        let mut x = Endpoint::<Hop<SocketAddr>>::new(&id(2), Config::default(), [2; 32]);
        x.connect(now, Hop::Ble("0.0.0.9:0".parse().unwrap()), id(1).peer_id()).unwrap();
        let hs1 = x.poll_transmit(now).unwrap().payload;
        let mut chunker = Chunker::new();
        chunker.split(&[&[KIND_DATAGRAM][..], &hs1].concat(), 23, |v| rogue.send(conn, v));
        node.poll(now, &mut ep);
        assert_eq!(ep.session_count(), 0, "the HS1 never reached the endpoint");
        assert_eq!(node.counts(), (0, 0, 0));
    }

    /// NB110-2: a central over the unbound limit is taken, and the oldest unbound connection closes for it, so held
    /// slots can't lock newcomers out; over the limit of centrals it is refused, and the advert says so.
    #[test]
    fn a_central_over_the_unbound_limit_evicts_the_oldest() {
        let air = FakeAir::new(23);
        let now = Instant::from_millis(1);
        let limits = Limits { connections: 8, unbound: 2, centrals: 8, queue: 64 };
        let mut node = Ble::new(Box::new(air.radio(limits)), id(1).peer_id(), now);
        node.set_room(now, Some((ble_key("app", "room"), Topic::room("app", "room"))));
        let mut ep = Endpoint::new(&id(1), Config::default(), [1; 32]);
        node.poll(now, &mut ep);
        let mut rogues: Vec<FakeRadio> = (0..3).map(|_| air.radio(limits)).collect();
        for r in &mut rogues {
            r.connect(DeviceId(0));
            node.poll(now, &mut ep);
        }
        assert_eq!(node.counts().0, 2);
        let events = |r: &mut FakeRadio| std::iter::from_fn(|| r.poll()).collect::<Vec<_>>();
        let gone = |e: &Vec<RadioEvent>| e.iter().any(|e| matches!(e, RadioEvent::Disconnected { .. }));
        let seen: Vec<Vec<RadioEvent>> = rogues.iter_mut().map(events).collect();
        assert_eq!(seen.iter().map(gone).collect::<Vec<_>>(), [true, false, false], "the first one went");
        assert_eq!(air.advert(0).map(|a| a[0] & FULL), Some(0), "not full: it takes centrals");

        let one = Limits { centrals: 2, ..limits };
        let mut node = Ble::new(Box::new(air.radio(one)), id(1).peer_id(), now);
        node.set_room(now, Some((ble_key("app", "room"), Topic::room("app", "room"))));
        node.poll(now, &mut ep);
        let at = DeviceId(4); // the fifth radio on the air
        let mut more: Vec<FakeRadio> = (0..3).map(|_| air.radio(one)).collect();
        for r in &mut more {
            r.connect(at);
            node.poll(now, &mut ep);
        }
        assert_eq!(node.counts().0, 2, "two centrals at most");
        assert_eq!(air.advert(4).map(|a| a[0] & FULL), Some(FULL), "the advert says it is full");
        let seen: Vec<Vec<RadioEvent>> = more.iter_mut().map(events).collect();
        assert_eq!(seen.iter().map(gone).collect::<Vec<_>>(), [false, false, true], "the third is refused");
    }

    #[test]
    fn a_link_binds_its_connection_and_datagrams_cross() {
        let mut p = Pair::new(["room", "room"]);
        p.run(500);
        // Each side learns the other's connection from the radio, and dials over it.
        let hops: Vec<Hop<SocketAddr>> = p.b.iter().map(|b| b.conns.values().next().unwrap().hop).collect();
        let (x, y) = (id(1).peer_id(), id(2).peer_id());
        p.eps[0].connect(p.now, hops[0], y).unwrap();
        for _ in 0..150 {
            for i in 0..2 {
                while let Some(t) = p.eps[i].poll_transmit(p.now) {
                    if let Hop::Ble(at) = t.dst {
                        p.b[i].send_datagram(&at, &t.payload);
                    }
                }
            }
            p.run(10);
            p.eps.iter_mut().for_each(|e| e.handle_timeout(p.now));
        }
        assert!(p.eps[0].is_connected(&y) && p.eps[1].is_connected(&x));
        let counts: Vec<_> = p.b.iter().map(Ble::counts).collect();
        assert_eq!(counts, vec![(1, 1, 1); 2], "bound both ends");
        p.eps[0].send(&y, Lane::Reliable, &[5u8; 900]).unwrap();
        for _ in 0..20 {
            for i in 0..2 {
                while let Some(t) = p.eps[i].poll_transmit(p.now) {
                    if let Hop::Ble(at) = t.dst {
                        p.b[i].send_datagram(&at, &t.payload);
                    }
                }
            }
            p.run(10);
        }
        let got = std::iter::from_fn(|| p.eps[1].poll_event())
            .any(|e| matches!(e, peerwire_core::Event::Message { data, .. } if data == vec![5u8; 900]));
        assert!(got, "900 bytes in 45 chunks of 20");
    }
}
