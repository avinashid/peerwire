//! Bluetooth LE (feature `ble`): links with peers nearby over a radio, with no network (`docs/concepts/bluetooth.md`).
//!
//! A node given a [`Radio`] ([`Builder::bluetooth`](crate::Builder::bluetooth)) advertises while it is in a room,
//! connects to the members it hears, proves the room and who it is on each connection, and then carries room hellos,
//! offers and datagrams over it: a Bluetooth connection is one more path for a link, ranked below every UDP one
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
use peerwire_core::{Endpoint, Identity, Instant, PeerId};
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
    /// Of those, ones not bound to a peer. Proven ones (standbys) leave one of these free for a newcomer: a connection
    /// that proves the room into the last closes the weakest other (NB110-2).
    pub unbound: usize,
    /// Of those, ones a central made to this radio.
    pub centrals: usize,
    /// GATT values queued to send on one connection: more are dropped, never waited for.
    pub queue: usize,
    /// New handshakes (distinct HS1s) taken on unbound connections, across the radio: `unbound` at once, then one per
    /// this. Each costs the node a Diffie-Hellman before it can tell who sent it (BLE-R9). Identity proofs are checked
    /// at four times that rate, as a check costs about a quarter of what answering a handshake does.
    pub hs1_gap: Duration,
}

impl Limits {
    /// A desktop's: 8 connections, 4 unbound, one central (BlueZ sends a notification to every subscribed central),
    /// 64 values queued, a new handshake per 250 ms.
    pub const DESKTOP: Limits =
        Limits { connections: 8, unbound: 4, centrals: 1, queue: 64, hs1_gap: Duration::from_millis(250) };
    /// A small board's, such as an ESP32's: 3 connections, 2 unbound, 16 values queued, a new handshake per 5 s (an
    /// ESP32-C3 answers one in 156 ms, so about 3% of it).
    pub const SMALL: Limits =
        Limits { connections: 3, unbound: 2, centrals: 3, queue: 16, hs1_gap: Duration::from_secs(5) };
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

/// Adverts change this often, with a new `n`, tag and address. An advert whose connection this end closed as a second
/// one to the same peer isn't connected to again for as long, so until it changes.
const ROTATE: Duration = Duration::from_secs(15 * 60);
/// A connection that hasn't proven the room, and brought the other end's identity proof, by then closes.
const PROVE_WITHIN: Duration = Duration::from_secs(3);
/// A central's connection that hasn't proven the room makes way for a new central once it has had this long (an
/// honest one needs a round trip), so one that never answers can't keep others out.
const GRACE: Duration = Duration::from_secs(1);
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
/// Identity proofs checked across the radio for each new handshake it takes ([`Limits::hs1_gap`]).
const CHECKS_PER_HS1: u32 = 4;
/// Signals taken per connection: a burst, then one per `SIGNAL_REFILL`; the rest dropped before any cryptography.
const SIGNAL_BURST: u32 = 16;
const SIGNAL_REFILL: Duration = Duration::from_millis(500);
/// Messages dropped or refused on a connection before it closes.
const MAX_DROPS: u64 = 32;
const CHALLENGE: usize = 16;
const MAC: usize = 32;
/// An identity proof: the sender's `PeerId` and its signature over the connection's challenges.
const ID_LEN: usize = 32 + 64;
/// The advert's service data: flags, `n`, tag.
const ADVERT_LEN: usize = 9;
const FULL: u8 = 1;
/// A signal's inner kinds: a room hello, a message sealed to the receiver, or the sender's identity proof.
const INNER_ROOM: u8 = 1;
const INNER_PEER: u8 = 2;
const INNER_ID: u8 = 3;

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

/// What an identity proof signs: the connection's challenges, `c_p ‖ c_c`, under a label of its own.
fn id_message(transcript: &[u8; 2 * CHALLENGE]) -> Vec<u8> {
    [&b"pw-ble-id"[..], transcript].concat()
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
    /// Both ends hold the room's ble key: signals are sealed under `key`. `transcript` is `c_p ‖ c_c`.
    Proven { key: [u8; 32], tx: u64, rx: u64, transcript: [u8; 2 * CHALLENGE] },
}

/// Who is at a connection's other end, as far as it is proven on the connection.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Id {
    /// No identity proof yet.
    Unknown,
    /// Its identity proof came, and waits for the radio's budget to be checked.
    Pending(PeerId, [u8; 64]),
    /// Proven on this connection: by its identity proof, or a session bound to it.
    Known(PeerId),
}

struct Conn {
    hop: Hop<SocketAddr>,
    /// Counts the connections made: the newer, the larger.
    made: u32,
    /// This end is the central.
    central: bool,
    /// The `n` of the advert this end connected to, as the central.
    n: Option<u32>,
    mtu: u16,
    chunker: Chunker,
    reassembler: Reassembler,
    queue: VecDeque<Vec<u8>>,
    proof: Proof,
    /// The room it was proven in ([`Ble::gen`]): it carries signals only for that one.
    gen: u32,
    id: Id,
    bound: Option<PeerId>,
    /// The inbox of the peer at the other end, once known: signals for it go here.
    inbox: Option<Topic>,
    opened: Instant,
    /// When its peer was proven ([`Id::Known`]).
    known_at: Instant,
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

    /// The peer proven to be at the other end.
    fn known(&self) -> Option<PeerId> {
        match self.id {
            Id::Known(p) => Some(p),
            _ => None,
        }
    }
}

/// What the node does with what came over the radio.
pub(crate) enum Inbound {
    /// A signal, as from this carrier's `topic`, that came over `conn`.
    Signal { topic: Topic, payload: Vec<u8>, conn: ConnId },
    /// `conn` proved the room: a room hello should go over it ([`Ble::send_room`]).
    Proven(ConnId),
    /// `peer` proved, on the connection itself, that it is at the other end of `via`: it may be dialled over it.
    Identified { peer: PeerId, via: Hop<SocketAddr> },
    /// A connection closed: no dial goes over it any more.
    Closed(Hop<SocketAddr>),
}

/// A node's side of Bluetooth: everything but the driver.
pub(crate) struct Ble {
    radio: Box<dyn Radio>,
    limits: Limits,
    me: Identity,
    /// The time of the last call that brought one.
    now: Instant,
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
    /// New handshakes on unbound connections, across the radio ([`Limits::hs1_gap`]).
    hs1s: Bucket,
    /// Identity proofs checked, across the radio.
    checks: Bucket,
    out: VecDeque<Inbound>,
}

impl Ble {
    pub(crate) fn new(radio: Box<dyn Radio>, me: &Identity, now: Instant) -> Ble {
        let limits = radio.limits();
        Ble {
            radio,
            limits,
            me: me.clone(),
            now,
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
            hs1s: Bucket::default(),
            checks: Bucket::default(),
            out: VecDeque::new(),
        }
    }

    /// Joins `room` (its ble key and topic), or leaves (`None`): a new `n`, and connections not bound to a peer close.
    pub(crate) fn set_room(&mut self, now: Instant, room: Option<([u8; 32], Topic)>) {
        self.now = now;
        self.room = room;
        self.gen = self.gen.wrapping_add(1);
        self.n = OsRng.next_u32();
        self.rotate_at = now + ROTATE;
        let unbound: Vec<ConnId> = self.conns.iter().filter(|(_, c)| c.bound.is_none()).map(|(i, _)| *i).collect();
        for c in unbound {
            self.close_for(c, Duration::ZERO);
        }
        self.refresh(now);
    }

    /// Turns the radio off (relay-only) or back on.
    pub(crate) fn set_enabled(&mut self, now: Instant, on: bool) {
        self.now = now;
        self.enabled = on;
        if !on {
            let all: Vec<ConnId> = self.conns.keys().copied().collect();
            for c in all {
                self.close_for(c, Duration::ZERO);
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

    /// A signal on `conn` proved a room member: while unbound, it stays open. Who sent it is only a claim: nothing
    /// here acts on it (NB110-6).
    pub(crate) fn heard(&mut self, conn: ConnId, now: Instant) {
        if let Some(c) = self.conns.get_mut(&conn) {
            c.heard = now;
        }
    }

    /// Handles what the radio reported, and the timers, handing datagrams to `ep`; then sends what is queued.
    pub(crate) fn poll(&mut self, now: Instant, ep: &mut Endpoint<Hop<SocketAddr>>) {
        self.now = now;
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
                if let Some(n) = self.conns.get(&conn).and_then(|c| c.n) {
                    let until = now + RECONNECT_HOLD;
                    let t = self.held.entry(n).or_insert(until);
                    *t = (*t).max(until);
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
        // Not our own. The smaller `n` connects to an advert that isn't full; the larger one does if its own is.
        let theirs_full = data[0] & FULL != 0;
        if n == self.n || theirs_full || !(self.n < n || self.full()) {
            return;
        }
        // Nor one this end is connected or connecting to, or holds. Only adverts it connected to itself count, as
        // nothing a central says of its own is taken (NB110-6): two connections to one peer may happen, and one closes.
        let known = self.conns.values().any(|c| c.n == Some(n)) || self.pending.values().any(|(m, _)| *m == n);
        if known || self.held.get(&n).is_some_and(|t| now < *t) || !self.room_out() {
            return;
        }
        self.pending.insert(device, (n, now));
        self.radio.connect(device);
    }

    /// Connections bound to no peer, with the connects under way.
    fn unbound(&self) -> usize {
        self.conns.values().filter(|c| c.bound.is_none()).count() + self.pending.len()
    }

    /// Whether a connection this end makes fits: one more connection, and one more unbound.
    fn room_out(&self) -> bool {
        self.conns.len() + self.pending.len() < self.limits.connections && self.unbound() < self.limits.unbound
    }

    /// Whether this radio takes no central now (its advert says so): it holds all the connections, or all the
    /// centrals, it can. A central's connection whose peer hasn't proven who it is doesn't count, as it soon has, or
    /// closes, or makes way for a newcomer (NB110-7); nor does a second one to a peer, which the central closes.
    fn full(&self) -> bool {
        let weak = |(i, c): &(&ConnId, &Conn)| !c.central && c.known().is_none_or(|p| self.twin(**i, p));
        let weak = self.conns.iter().filter(weak).count();
        let centrals = self.conns.values().filter(|c| !c.central).count() - weak;
        self.conns.len() + self.pending.len() - weak >= self.limits.connections || centrals >= self.limits.centrals
    }

    /// Whether another connection than `conn` is proven to reach `peer`.
    fn twin(&self, conn: ConnId, peer: PeerId) -> bool {
        self.conns.iter().any(|(i, c)| *i != conn && c.known() == Some(peer))
    }

    /// The connection bound to no peer, among those `which` takes, that makes way for a newcomer: one that hasn't
    /// proven the room before one that has, one whose peer hasn't proven who it is before one whose has, then the
    /// oldest.
    fn weakest(&self, which: impl Fn(ConnId, &Conn) -> bool) -> Option<ConnId> {
        let open = self.conns.iter().filter(|(i, c)| c.bound.is_none() && which(**i, c));
        open.min_by_key(|(_, c)| (c.proven(), c.known().is_some(), c.opened, c.made)).map(|(i, _)| *i)
    }

    /// Makes room for a new central if it can: over a limit, a connection that hasn't proven the room within `GRACE`
    /// closes, a central's for the limits of connections and centrals (NB110-7). Whether the central is taken.
    fn make_way(&mut self, now: Instant) -> bool {
        let stale = |c: &Conn| !c.proven() && now.saturating_duration_since(c.opened) >= GRACE;
        let centrals = self.conns.values().filter(|c| !c.central).count();
        if self.conns.len() + self.pending.len() >= self.limits.connections || centrals >= self.limits.centrals {
            match self.weakest(|_, c| !c.central && stale(c)) {
                Some(w) => self.close(w),
                None => return false,
            }
        }
        if self.unbound() >= self.limits.unbound {
            match self.weakest(|_, c| stale(c)) {
                Some(w) => self.close(w),
                None => return false,
            }
        }
        true
    }

    fn on_connected(&mut self, now: Instant, conn: ConnId, device: Option<DeviceId>, mtu: u16) {
        let n = match device {
            Some(d) => match self.pending.remove(&d) {
                Some((n, _)) => Some(n),
                None => return self.radio.disconnect(conn), // not one we asked for
            },
            None if !self.on || !self.make_way(now) => return self.radio.disconnect(conn),
            None => None,
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
            id: Id::Unknown,
            bound: None,
            inbox: None,
            opened: now,
            known_at: now,
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
            (Proof::Challenged(c_p), KIND_ANSWER) if body.len() == CHALLENGE + MAC => {
                let (c_p, c_c) = (*c_p, <[u8; CHALLENGE]>::try_from(&body[..CHALLENGE]).expect("16 bytes"));
                if !same(&hmac(&key, &[b"pw-ble-c", &c_p, &c_c]), &body[CHALLENGE..]) {
                    return self.close(conn);
                }
                let proof = hmac(&key, &[b"pw-ble-p", &c_c, &c_p]);
                self.send(conn, &[&[KIND_PROOF], &proof]);
                self.proven(now, conn, c_p, c_c);
            }
            (Proof::Awaiting, KIND_CHALLENGE) if body.len() == CHALLENGE => {
                let c_p = <[u8; CHALLENGE]>::try_from(body).expect("16 bytes");
                let mut c_c = [0u8; CHALLENGE];
                OsRng.fill_bytes(&mut c_c);
                let mac = hmac(&key, &[b"pw-ble-c", &c_p, &c_c]);
                c.proof = Proof::Answered(c_p, c_c);
                self.send(conn, &[&[KIND_ANSWER], &c_c, &mac]);
            }
            (Proof::Answered(c_p, c_c), KIND_PROOF) if body.len() == MAC => {
                let (c_p, c_c) = (*c_p, *c_c);
                if !same(&hmac(&key, &[b"pw-ble-p", &c_c, &c_p]), body) {
                    return self.close(conn);
                }
                self.proven(now, conn, c_p, c_c);
            }
            // Before the proof, nothing else: a connection that tries closes.
            (Proof::Challenged(_) | Proof::Awaiting | Proof::Answered(..), _) => self.close(conn),
            (Proof::Proven { .. }, KIND_SIGNAL) => self.on_signal(now, conn, topic, body),
            (Proof::Proven { .. }, KIND_DATAGRAM) if !body.is_empty() => self.on_datagram(now, ep, conn, body),
            (Proof::Proven { .. }, _) => self.dropped(conn, 1),
        }
    }

    /// `conn` proved the room with challenges `c_p` and `c_c`: signals go under its link key, and this end sends its
    /// identity proof over it. A standby that fills the last unbound slot closes the weakest other (NB110-2).
    fn proven(&mut self, now: Instant, conn: ConnId, c_p: [u8; CHALLENGE], c_c: [u8; CHALLENGE]) {
        let Some(((key, _), c)) = self.room.zip(self.conns.get_mut(&conn)) else { return };
        let mut transcript = [0u8; 2 * CHALLENGE];
        transcript[..CHALLENGE].copy_from_slice(&c_p);
        transcript[CHALLENGE..].copy_from_slice(&c_c);
        let link = hmac(&key, &[b"pw-ble-k", &transcript]);
        (c.proof, c.heard) = (Proof::Proven { key: link, tx: 0, rx: 0, transcript }, now);
        let sig = self.me.sign(&id_message(&transcript));
        self.signal(conn, INNER_ID, &[&self.me.peer_id().as_bytes()[..], &sig].concat());
        self.out.push_back(Inbound::Proven(conn));
        if self.unbound() >= self.limits.unbound {
            if let Some(w) = self.weakest(|i, _| i != conn) {
                self.close(w);
            }
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
            INNER_PEER => Topic::peer(&self.me.peer_id()),
            INNER_ID => return self.on_id(conn, &plain[1..]),
            _ => return self.dropped(conn, 1),
        };
        self.out.push_back(Inbound::Signal { topic, payload: plain[1..].to_vec(), conn });
    }

    /// The other end's identity proof, the one a connection takes: checked as the radio's budget allows.
    fn on_id(&mut self, conn: ConnId, body: &[u8]) {
        let Some(c) = self.conns.get_mut(&conn) else { return };
        if body.len() != ID_LEN || c.id != Id::Unknown {
            return self.dropped(conn, 1);
        }
        let peer = PeerId(body[..32].try_into().expect("32 bytes"));
        c.id = Id::Pending(peer, body[32..].try_into().expect("64 bytes"));
        self.check_ids();
    }

    /// Checks the identity proofs that came, the oldest connection's first, as far as the radio's budget goes
    /// (BLE-R9): a proof signed by its `PeerId` over this connection's challenges proves that peer is at the other
    /// end; any other closes the connection.
    fn check_ids(&mut self) {
        let burst = u32::try_from(self.limits.unbound).unwrap_or(u32::MAX).saturating_mul(CHECKS_PER_HS1);
        let gap = self.limits.hs1_gap / CHECKS_PER_HS1;
        loop {
            let waiting = self.conns.iter().filter(|(_, c)| matches!(c.id, Id::Pending(..)));
            let Some((&conn, c)) = waiting.min_by_key(|(_, c)| (c.opened, c.made)) else { return };
            let (Id::Pending(peer, sig), Proof::Proven { transcript, .. }) = (c.id, &c.proof) else { return };
            let message = id_message(transcript);
            if !self.checks.take(self.now, burst, gap) {
                return;
            }
            match peer != self.me.peer_id() && peer.verify(&message, &sig) {
                true => self.reaches(conn, peer, false),
                false => self.close(conn),
            }
        }
    }

    /// `peer` is proven to be at the other end of `conn`: by its identity proof, or by a session that `binds` it. A
    /// peer keeps one connection (NB110-6). A session's is the one, and others to that peer close. Otherwise the first
    /// proven stays: both ends proved it before a second was made, so a second one closes, at the end that made it,
    /// which doesn't connect to that advert again until it changes; the other end leaves it to that one.
    fn reaches(&mut self, conn: ConnId, peer: PeerId, binds: bool) {
        let twins: Vec<ConnId> =
            self.conns.iter().filter(|(i, c)| **i != conn && c.known() == Some(peer)).map(|(i, _)| *i).collect();
        let Some(c) = self.conns.get_mut(&conn) else { return };
        (c.id, c.inbox, c.known_at) = (Id::Known(peer), Some(Topic::peer(&peer)), self.now);
        let (central, via) = (c.central, c.hop);
        if binds {
            c.bound = Some(peer);
            for t in twins {
                self.close_for(t, ROTATE);
            }
        } else if !twins.is_empty() {
            if central {
                self.close_for(conn, ROTATE);
            }
            return self.refresh(self.now);
        }
        self.out.push_back(Inbound::Identified { peer, via });
        self.refresh(self.now);
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
                        _ => {
                            let burst = u32::try_from(self.limits.unbound).unwrap_or(u32::MAX);
                            if !self.hs1s.take(now, burst, self.limits.hs1_gap) {
                                return self.dropped(conn, 1);
                            }
                            c.hs1 = Some((id, now));
                        }
                    }
                }
                if let Some(peer) = ep.handle_datagram_unbound(now, c.hop, d) {
                    self.reaches(conn, peer, true);
                }
            }
        }
        let wrong = ep.stats().rx_wrong_peer - before;
        self.dropped(conn, wrong);
    }

    /// Sends a datagram the endpoint addressed to `hop`; dropped if no proven connection is there.
    pub(crate) fn send_datagram(&mut self, hop: &SocketAddr, payload: &[u8]) {
        if let Some(&conn) = self.by_hop.get(hop).filter(|c| self.conns.get(c).is_some_and(Conn::proven)) {
            self.send(conn, &[&[KIND_DATAGRAM], payload]);
        }
    }

    /// Sends a signal the traversal published on `topic`: a room hello on every connection proven in this room, a
    /// message for a peer's inbox on the connection proven to reach that peer, or else on every one whose peer isn't
    /// known yet.
    pub(crate) fn publish(&mut self, topic: Topic, payload: &[u8]) {
        let Some((_, room)) = self.room else { return };
        let inner = if topic == room { INNER_ROOM } else { INNER_PEER };
        let live = |c: &Conn| c.proven() && c.gen == self.gen;
        let mut to: Vec<ConnId> = Vec::new();
        if inner == INNER_PEER {
            to = self.conns.iter().filter(|(_, c)| live(c) && c.inbox == Some(topic)).map(|(i, _)| *i).collect();
        }
        if to.is_empty() {
            let open = |c: &Conn| inner == INNER_ROOM || c.known().is_none();
            to = self.conns.iter().filter(|(_, c)| live(c) && open(c)).map(|(i, _)| *i).collect();
        }
        for conn in to {
            self.signal(conn, inner, payload);
        }
    }

    /// Sends a room hello on `conn` alone, which has just proven the room ([`Inbound::Proven`]): the members it
    /// reaches hear of this node at once, and no other carrier's hellos come sooner for it.
    pub(crate) fn send_room(&mut self, conn: ConnId, payload: &[u8]) {
        if self.conns.get(&conn).is_some_and(|c| c.proven() && c.gen == self.gen) {
            self.signal(conn, INNER_ROOM, payload);
        }
    }

    /// Seals `inner ‖ payload` under `conn`'s link key, as its next signal, and queues it.
    fn signal(&mut self, conn: ConnId, inner: u8, payload: &[u8]) {
        let Some(c) = self.conns.get_mut(&conn) else { return };
        let Proof::Proven { key, tx, .. } = &mut c.proof else { return };
        *tx += 1;
        let dir = if c.central { 0u32 } else { 1 };
        let mut plain = Vec::with_capacity(1 + payload.len());
        plain.push(inner);
        plain.extend_from_slice(payload);
        let Some(sealed) = seal(key, dir, *tx, &plain) else { return };
        let seq = tx.to_le_bytes();
        self.send(conn, &[&[KIND_SIGNAL], &seq, &sealed]);
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
            .filter(|(_, c)| match c.proven() && c.id != Id::Unknown {
                false => now.saturating_duration_since(c.opened) > PROVE_WITHIN,
                true => c.bound.is_none() && now.saturating_duration_since(c.heard) > UNBOUND_FOR,
            })
            .map(|(i, _)| *i)
            .collect();
        for c in late {
            self.close(c);
        }
        // Two connections to one peer, each made by one end, which each took as its first (they were made at once):
        // once the other end's has been proven that long, the one whose central has the larger `PeerId` closes, at the
        // end that made it.
        let me = self.me.peer_id();
        let theirs = |p: PeerId| {
            let late = |o: &Conn| now.saturating_duration_since(o.known_at) >= PROVE_WITHIN;
            self.conns.values().any(|o| !o.central && o.known() == Some(p) && late(o))
        };
        let lose: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.central && c.bound.is_none() && c.known().is_some_and(|p| me > p && theirs(p)))
            .map(|(i, _)| *i)
            .collect();
        for c in lose {
            self.close_for(c, ROTATE);
        }
        self.check_ids();
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
        self.close_for(conn, RECONNECT_HOLD);
    }

    /// Closes `conn` and forgets it; if this end connected to an advert for it, that isn't connected to again for
    /// `hold`.
    fn close_for(&mut self, conn: ConnId, hold: Duration) {
        let Some(c) = self.conns.get(&conn) else { return };
        if let Some(n) = c.n.filter(|_| !hold.is_zero()) {
            let until = self.now + hold;
            let t = self.held.entry(n).or_insert(until);
            *t = (*t).max(until);
        }
        self.radio.disconnect(conn);
        self.forget(conn);
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

        /// Connections made on it so far.
        pub fn connects(&self) -> u32 {
            (self.0.lock().unwrap().next - 1) / 2
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

    fn room() -> Option<([u8; 32], Topic)> {
        Some((ble_key("app", "room"), Topic::room("app", "room")))
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

    /// Node sides on one air, in the same room, each with an endpoint: `run` polls them all, hands each signal to
    /// `heard` as the node does, publishes a room hello from each every 10 s, and keeps what came.
    struct Air {
        air: FakeAir,
        b: Vec<Ble>,
        eps: Vec<Endpoint<Hop<SocketAddr>>>,
        now: Instant,
        /// Each one's signals: topic and payload.
        got: Vec<Vec<(Topic, Vec<u8>)>>,
        /// Each one's `Identified`: the peer, and the connection.
        ids: Vec<Vec<(PeerId, Hop<SocketAddr>)>>,
        /// Datagrams cross between endpoints.
        datagrams: bool,
    }

    impl Air {
        /// Nodes `1..=limits.len()` (identities `id(i)`), the `i`th with `limits[i - 1]`, in room "room".
        fn new(limits: &[Limits]) -> Air {
            let air = FakeAir::new(23);
            let now = Instant::from_millis(1);
            let b = (1..=limits.len() as u8)
                .map(|i| {
                    let mut b = Ble::new(Box::new(air.radio(limits[usize::from(i) - 1])), &id(i), now);
                    b.set_room(now, room());
                    b
                })
                .collect();
            let eps = (1..=limits.len() as u8).map(|i| Endpoint::new(&id(i), Config::default(), [i; 32])).collect();
            let n = limits.len();
            Air { air, b, eps, now, got: vec![Vec::new(); n], ids: vec![Vec::new(); n], datagrams: false }
        }

        /// Gives node `i` (from 0) the advert value `n`.
        fn set_n(&mut self, i: usize, n: u32) {
            let b = &mut self.b[i];
            (b.n, b.advertised) = (n, None);
            b.refresh(self.now);
        }

        fn run(&mut self, ms: u64) {
            for _ in 0..ms / 10 {
                for i in 0..self.b.len() {
                    while let Some(t) = self.eps[i].poll_transmit(self.now) {
                        if let (Hop::Ble(at), true) = (t.dst, self.datagrams) {
                            self.b[i].send_datagram(&at, &t.payload);
                        }
                    }
                    self.b[i].poll(self.now, &mut self.eps[i]);
                    while let Some(e) = self.b[i].take() {
                        match e {
                            Inbound::Signal { topic, payload, conn } => {
                                self.b[i].heard(conn, self.now);
                                self.got[i].push((topic, payload));
                            }
                            Inbound::Identified { peer, via } => self.ids[i].push((peer, via)),
                            Inbound::Proven(_) | Inbound::Closed(_) => {}
                        }
                    }
                    if (self.now - Instant::from_millis(1)).as_millis().is_multiple_of(10_000) {
                        self.b[i].publish(Topic::room("app", "room"), b"hello");
                    }
                    self.eps[i].handle_timeout(self.now);
                }
                self.now = self.now + Duration::from_millis(10);
            }
        }

        /// Whether nodes `i` and `j` (from 0) hold a connection to each other, each proven to reach the other.
        fn linked(&self, i: usize, j: usize) -> bool {
            let (pi, pj) = (id(i as u8 + 1).peer_id(), id(j as u8 + 1).peer_id());
            let to = |a: usize, b: usize, p: PeerId| {
                self.b[a].conns.iter().any(|(c, x)| self.air.peer_of(a, *c) == Some(b) && x.known() == Some(p))
            };
            to(i, j, pj) && to(j, i, pi)
        }
    }

    /// A central of the test's, by hand over a radio of its own: it knows the room's key, and does as it is told.
    struct Rogue {
        radio: FakeRadio,
        conn: ConnId,
        tx: Chunker,
        rx: Reassembler,
        /// `c_p ‖ c_c`, and the link key, once proven.
        transcript: [u8; 2 * CHALLENGE],
        link: [u8; 32],
        seq: u64,
        /// The node closed it.
        gone: bool,
    }

    impl Rogue {
        /// Connects to radio `to`, which `node` runs, and proves the room. The node's identity proof is in `read`.
        fn prove(air: &FakeAir, to: DeviceId, node: &mut Ble, ep: &mut Endpoint<Hop<SocketAddr>>, t: Instant) -> Rogue {
            let mut radio = air.radio(Limits::DESKTOP);
            radio.connect(to);
            let Some(RadioEvent::Connected { conn, .. }) = radio.poll() else { panic!() };
            let (tx, rx) = (Chunker::new(), Reassembler::new());
            let (transcript, link) = ([0; 2 * CHALLENGE], [0; 32]);
            let mut r = Rogue { radio, conn, tx, rx, transcript, link, seq: 0, gone: false };
            let challenge = r.read(node, ep, t);
            assert_eq!(challenge.len(), 1, "a challenge");
            let (c_p, c_c) = (&challenge[0][1..], [9u8; CHALLENGE]);
            assert_eq!((challenge[0][0], c_p.len()), (KIND_CHALLENGE, CHALLENGE));
            let key = ble_key("app", "room");
            r.send(&[&[KIND_ANSWER][..], &c_c, &hmac(&key, &[b"pw-ble-c", c_p, &c_c])].concat());
            r.transcript[..CHALLENGE].copy_from_slice(c_p);
            r.transcript[CHALLENGE..].copy_from_slice(&c_c);
            r.link = hmac(&key, &[b"pw-ble-k", &r.transcript]);
            r
        }

        fn send(&mut self, m: &[u8]) {
            let (radio, conn) = (&mut self.radio, self.conn);
            self.tx.split(m, 23, |v| radio.send(conn, v));
        }

        /// Polls `node`, and returns the messages that came.
        fn read(&mut self, node: &mut Ble, ep: &mut Endpoint<Hop<SocketAddr>>, t: Instant) -> Vec<Vec<u8>> {
            node.poll(t, ep);
            let mut out = Vec::new();
            while let Some(e) = self.radio.poll() {
                match e {
                    RadioEvent::Value { value, .. } => out.extend(self.rx.push(t, &value).map(<[u8]>::to_vec)),
                    RadioEvent::Disconnected { .. } => self.gone = true,
                    _ => {}
                }
            }
            out
        }

        /// The plaintext of a signal from the node (the peripheral), whose seq is `seq`.
        fn open(&self, m: &[u8]) -> Vec<u8> {
            assert_eq!(m[0], KIND_SIGNAL);
            let seq = u64::from_le_bytes(m[1..9].try_into().unwrap());
            open(&self.link, 1, seq, &m[9..]).expect("a signal under the link key")
        }

        /// Sends `inner ‖ payload` as its next signal.
        fn signal(&mut self, inner: u8, payload: &[u8]) {
            self.seq += 1;
            let sealed = seal(&self.link, 0, self.seq, &[&[inner][..], payload].concat()).unwrap();
            self.send(&[&[KIND_SIGNAL][..], &self.seq.to_le_bytes(), &sealed].concat());
        }

        /// Sends an identity proof: `who`, signed over `transcript`.
        fn prove_id(&mut self, who: &Identity, transcript: &[u8; 2 * CHALLENGE]) {
            let sig = who.sign(&id_message(transcript));
            self.signal(INNER_ID, &[&who.peer_id().as_bytes()[..], &sig].concat());
        }

        fn closed(&mut self) -> bool {
            while let Some(e) = self.radio.poll() {
                self.gone |= matches!(e, RadioEvent::Disconnected { .. });
            }
            self.gone
        }
    }

    fn lone(air: &FakeAir, limits: Limits, n: u8, now: Instant) -> (Ble, Endpoint<Hop<SocketAddr>>) {
        let mut b = Ble::new(Box::new(air.radio(limits)), &id(n), now);
        b.set_room(now, room());
        let mut ep = Endpoint::new(&id(n), Config::default(), [n; 32]);
        b.poll(now, &mut ep);
        (b, ep)
    }

    /// NB110-1: Alice and Bob linked over their connection, and Mallory, a member, connected to Bob too. Mallory
    /// takes Alice's next packet before Bob has it and sends it over her own connection: Bob drops it unread, his
    /// link to Alice stays where it is, Mallory's connection isn't bound to Alice and Alice's isn't closed; the real
    /// packet, when it comes, still reads.
    #[test]
    fn a_member_cannot_move_another_link_onto_its_connection() {
        let limits = Limits { centrals: 3, ..Limits::DESKTOP };
        let mut w = Air::new(&[limits; 3]);
        w.run(2_000);
        assert!(w.b.iter().all(|b| b.counts() == (2, 2, 0)), "each linked to the other two by the radio");
        let step = |w: &mut Air, hold: bool| {
            let mut held = Vec::new();
            for i in 0..3 {
                while let Some(t) = w.eps[i].poll_transmit(w.now) {
                    if let Hop::Ble(at) = t.dst {
                        match hold && i == 0 {
                            true => held.push((at, t.payload)),
                            false => w.b[i].send_datagram(&at, &t.payload),
                        }
                    }
                }
                w.b[i].poll(w.now, &mut w.eps[i]);
                while w.b[i].take().is_some() {}
                w.eps[i].handle_timeout(w.now);
            }
            w.now = w.now + Duration::from_millis(10);
            held
        };
        // Alice dials Bob over both her connections; the one to Bob binds.
        let (alice, bob) = (id(1).peer_id(), id(2).peer_id());
        let hops: Vec<Path<Hop<SocketAddr>>> = w.b[0].conns.values().map(|c| Path::Direct(c.hop)).collect();
        let now = w.now;
        w.eps[0].connect_with(now, bob, &hops).unwrap();
        for _ in 0..200 {
            step(&mut w, false);
        }
        assert!(w.eps[0].is_connected(&bob) && w.eps[1].is_connected(&alice));
        assert_eq!((w.b[0].counts().2, w.b[1].counts().2), (1, 1), "bound at both ends");
        let path = w.eps[1].path(&alice).cloned();
        // Mallory's connection to Bob.
        let to_bob = w.b[2].conns.iter().find(|(i, _)| w.air.peer_of(2, **i) == Some(1)).map(|(_, c)| c.hop);
        let Some(Hop::Ble(to_bob)) = to_bob else { panic!("Mallory is connected to Bob") };
        w.eps[0].send(&bob, Lane::Reliable, b"for bob").unwrap();
        let held = step(&mut w, true);
        assert_eq!(held.len(), 1, "Alice's packet, taken");
        let wrong = w.eps[1].stats().rx_wrong_peer;
        w.b[2].send_datagram(&to_bob, &held[0].1);
        for _ in 0..5 {
            step(&mut w, false);
        }
        assert_eq!(w.eps[1].stats().rx_wrong_peer, wrong + 1, "dropped unread");
        assert_eq!(w.eps[1].path(&alice).cloned(), path, "the link stays where it was");
        assert_eq!(w.b[1].counts(), (2, 2, 1), "Mallory's isn't bound to Alice, and Alice's is still open");
        let mut events = std::iter::from_fn(|| w.eps[1].poll_event());
        assert!(events.all(|e| !matches!(e, peerwire_core::Event::Message { .. })));
        w.b[0].send_datagram(&held[0].0, &held[0].1);
        for _ in 0..5 {
            step(&mut w, false);
        }
        let got = std::iter::from_fn(|| w.eps[1].poll_event())
            .any(|e| matches!(e, peerwire_core::Event::Message { data, .. } if data == b"for bob"));
        assert!(got, "the real packet still reads");
    }

    /// The lead's R9 case (156 ms per HS1 on an ESP32-C3): a member that reconnects after each new handshake gets a
    /// fresh connection each time, so the radio budgets new handshakes on unbound connections across all of them.
    #[test]
    fn new_handshakes_are_budgeted_across_unbound_connections() {
        let air = FakeAir::new(23);
        let now = Instant::from_millis(1);
        let limits = Limits { unbound: 2, centrals: 3, hs1_gap: Duration::from_secs(2), ..Limits::SMALL };
        let (mut node, mut ep) = lone(&air, limits, 1, now);
        let mut sessions = Vec::new();
        for (i, t) in [0u64, 0, 0, 0, 2_000].into_iter().enumerate() {
            let t = now + Duration::from_millis(t);
            let seed = [20 + i as u8; 32]; // each its own ephemeral key: a new handshake, not a resend
            let mut dialler = Endpoint::<Hop<SocketAddr>>::new(&id(10 + i as u8), Config::default(), seed);
            dialler.connect(t, Hop::Ble("0.0.0.9:0".parse().unwrap()), id(1).peer_id()).unwrap();
            let hs1 = dialler.poll_transmit(t).unwrap().payload;
            let mut rogue = Rogue::prove(&air, DeviceId(0), &mut node, &mut ep, t);
            rogue.send(&[&[KIND_DATAGRAM][..], &hs1].concat());
            node.poll(t, &mut ep);
            sessions.push(ep.session_count());
            rogue.radio.disconnect(rogue.conn);
            node.poll(t, &mut ep);
        }
        assert_eq!(sessions, [1, 2, 2, 2, 3], "two at once, then one per 2 s");
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

    /// Who connects turns on random values. Desktops take one central each, so once one has connected to the other,
    /// the other is full and connects back: a second connection, which its maker closes once both ends have proven
    /// who they are, and doesn't make again while that advert lasts (NB110-6). A pair ends with one connection, made
    /// at most twice.
    #[test]
    fn members_connect_once_prove_the_room_and_who_they_are_and_carry_sealed_signals() {
        for _ in 0..10 {
            let mut w = Air::new(&[Limits::DESKTOP; 2]);
            w.run(3_000);
            assert_eq!(w.air.links(), 1, "one connection");
            assert!(w.linked(0, 1), "each proven to reach the other");
            w.run(70_000);
            assert!(w.air.links() == 1 && w.linked(0, 1), "still the one");
            assert!(w.air.connects() <= 2, "{} connections made", w.air.connects());
            let (a, b) = (id(1).peer_id(), id(2).peer_id());
            assert_eq!(w.ids.iter().map(Vec::len).collect::<Vec<_>>(), [1, 1], "each told once");
            assert!(w.ids[0][0].0 == b && w.ids[1][0].0 == a);
            assert!(w.b.iter().all(|b| b.counts() == (1, 1, 0)), "proven, bound to no one");
        }
        let mut w = Air::new(&[Limits::DESKTOP; 2]);
        w.run(3_000);
        let room = Topic::room("app", "room");
        let got = w.got.iter().map(Vec::len).collect::<Vec<_>>();
        w.b[0].publish(room, b"hello from 1");
        w.b[1].publish(Topic::peer(&id(1).peer_id()), b"offer for 1");
        w.b[1].publish(Topic::peer(&id(3).peer_id()), b"offer for 3");
        w.run(100);
        assert_eq!(w.got[1][got[1]..], [(room, b"hello from 1".to_vec())]);
        assert_eq!(w.got[0][got[0]..], [(Topic::peer(&id(1).peer_id()), b"offer for 1".to_vec())], "not 3's");
    }

    #[test]
    fn other_rooms_never_connect() {
        let mut w = Air::new(&[Limits::DESKTOP; 2]);
        w.b[1].set_room(w.now, Some((ble_key("app", "other"), Topic::room("app", "other"))));
        w.run(2000);
        assert_eq!(w.air.links(), 0);
    }

    #[test]
    fn a_central_without_the_room_key_is_closed() {
        let air = FakeAir::new(23);
        let now = Instant::from_millis(1);
        let (mut node, mut ep) = lone(&air, Limits::DESKTOP, 1, now);
        let mut rogue = air.radio(Limits::DESKTOP);
        let mut t = now;
        // A wrong answer closes it at once; one that says nothing, at the deadline.
        for (answer, after) in [(true, 20u64), (false, 3_100)] {
            rogue.connect(DeviceId(0));
            let Some(RadioEvent::Connected { conn, .. }) = rogue.poll() else { panic!() };
            node.poll(t, &mut ep);
            let challenge = rogue.poll();
            assert!(matches!(challenge, Some(RadioEvent::Value { .. })), "{challenge:?}");
            if answer {
                let mut chunker = Chunker::new();
                let mut m = vec![KIND_ANSWER];
                m.extend([0u8; CHALLENGE + MAC]);
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
        let (mut node, mut ep) = lone(&air, Limits::DESKTOP, 1, now);
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

    /// NB110-6: once the room is proven, each end proves who it is on the connection: its `PeerId`, signed over this
    /// connection's challenges. Only that identity reaches anything. A proof made for another connection (a member
    /// passing on one it was sent), one in this node's own name, or none at all, closes the connection.
    #[test]
    fn a_peer_proves_who_it_is_on_the_connection() {
        let air = FakeAir::new(23);
        let mut t = Instant::from_millis(1);
        let (mut node, mut ep) = lone(&air, Limits { centrals: 4, ..Limits::DESKTOP }, 1, t);
        let ids = |node: &mut Ble| {
            std::iter::from_fn(|| node.take())
                .filter_map(|e| match e {
                    Inbound::Identified { peer, .. } => Some(peer),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        // The node's own proof comes right after its proof of the room, and checks out.
        let mut first = Rogue::prove(&air, DeviceId(0), &mut node, &mut ep, t);
        let came = first.read(&mut node, &mut ep, t);
        assert_eq!(came[0][0], KIND_PROOF);
        let proof = first.open(&came[1]);
        assert_eq!((proof[0], proof.len()), (INNER_ID, 1 + ID_LEN));
        let (who, sig) = (PeerId(proof[1..33].try_into().unwrap()), proof[33..].try_into().unwrap());
        assert!(who == id(1).peer_id() && who.verify(&id_message(&first.transcript), &sig));
        first.prove_id(&id(5), &first.transcript.clone());
        first.read(&mut node, &mut ep, t);
        assert_eq!(ids(&mut node), [id(5).peer_id()], "proven on the connection");

        let mut other = Rogue::prove(&air, DeviceId(0), &mut node, &mut ep, t);
        other.prove_id(&id(6), &first.transcript);
        other.read(&mut node, &mut ep, t);
        assert!(other.closed(), "a proof for another connection");
        let mut mine = Rogue::prove(&air, DeviceId(0), &mut node, &mut ep, t);
        mine.prove_id(&id(1), &mine.transcript.clone());
        mine.read(&mut node, &mut ep, t);
        assert!(mine.closed(), "the node's own name");
        let mut silent = Rogue::prove(&air, DeviceId(0), &mut node, &mut ep, t);
        t = t + PROVE_WITHIN + Duration::from_millis(10);
        silent.read(&mut node, &mut ep, t);
        assert!(silent.closed(), "no proof by the deadline");
        assert!(ids(&mut node).is_empty() && node.counts().0 == 1);
    }

    /// NB110-6 (security's case c): Mallory, a member, connects to Alice and takes Alice's identity proof off that
    /// connection, then connects to Bob before Alice does and passes it on. Bob closes Mallory's connection, and
    /// Alice and Bob still link over theirs.
    #[test]
    fn a_member_cannot_pass_on_anothers_identity() {
        let limits = Limits { centrals: 3, ..Limits::DESKTOP };
        let mut w = Air::new(&[limits; 2]);
        w.set_n(0, 2_000);
        w.set_n(1, 1_000);
        let t = w.now;
        let (alice, bob) = w.b.split_at_mut(1);
        let (ea, eb) = w.eps.split_at_mut(1);
        let mut m = Rogue::prove(&w.air, DeviceId(0), &mut alice[0], &mut ea[0], t);
        let came = m.read(&mut alice[0], &mut ea[0], t);
        let proof = m.open(&came[1]);
        let mut to_bob = Rogue::prove(&w.air, DeviceId(1), &mut bob[0], &mut eb[0], t);
        to_bob.signal(INNER_ID, &proof[1..]);
        to_bob.read(&mut bob[0], &mut eb[0], t);
        assert!(to_bob.closed(), "Alice's proof doesn't hold on Mallory's connection");
        w.run(2_000);
        assert!(w.linked(0, 1), "Alice and Bob link");
        let via = |i: usize| w.ids[i].iter().filter(|(p, _)| *p == id(2 - i as u8).peer_id()).count();
        assert_eq!((via(0), via(1)), (1, 1));
    }

    /// NB110-6: two connections to one peer, one made by each end, each of which took its own as the first (made at
    /// once, over a slow radio). Here the node made one to Y's advert and proved Y on it first; then Y's own
    /// connection to the node proved Y too. The node leaves that one to Y, which made it, and it doesn't fill the
    /// advert; once it has been proven `PROVE_WITHIN`, the one whose central has the larger `PeerId` closes, at the
    /// end that made it: the node's own if the node's `PeerId` is the larger, else none here.
    #[test]
    fn twins_made_at_once_settle_on_one() {
        let key = ble_key("app", "room");
        for (me, y) in [(1u8, 2u8), (2, 1)] {
            let air = FakeAir::new(23);
            let t = Instant::from_millis(1);
            let (mut node, mut ep) = lone(&air, Limits::DESKTOP, me, t);
            let (yp, x) = (&mut air.radio(Limits::DESKTOP), DeviceId(0));
            yp.advertise(Some(&advert(&key, u32::MAX, false)));
            let conn = (0..64)
                .find_map(|_| {
                    node.poll(t, &mut ep);
                    std::iter::from_fn(|| yp.poll()).find_map(|e| match e {
                        RadioEvent::Connected { conn, .. } => Some(conn),
                        _ => None,
                    })
                })
                .expect("the node connects to Y's advert");
            // Y's side of the room proof, as the peripheral, then its identity proof.
            let (c_p, mut tx, mut rx) = ([3u8; CHALLENGE], Chunker::new(), Reassembler::new());
            tx.split(&[&[KIND_CHALLENGE][..], &c_p].concat(), 23, |v| yp.send(conn, v));
            node.poll(t, &mut ep);
            let answer = std::iter::from_fn(|| yp.poll())
                .find_map(|e| match e {
                    RadioEvent::Value { value, .. } => rx.push(t, &value).map(<[u8]>::to_vec),
                    _ => None,
                })
                .expect("an answer");
            let c_c: [u8; CHALLENGE] = answer[1..1 + CHALLENGE].try_into().unwrap();
            tx.split(&[&[KIND_PROOF][..], &hmac(&key, &[b"pw-ble-p", &c_c, &c_p])].concat(), 23, |v| yp.send(conn, v));
            let transcript: [u8; 2 * CHALLENGE] = [c_p, c_c].concat().try_into().unwrap();
            let link = hmac(&key, &[b"pw-ble-k", &transcript]);
            let sig = id(y).sign(&id_message(&transcript));
            let proof = [&[INNER_ID][..], id(y).peer_id().as_bytes(), &sig].concat();
            let sealed = seal(&link, 1, 1, &proof).unwrap();
            tx.split(&[&[KIND_SIGNAL][..], &1u64.to_le_bytes(), &sealed].concat(), 23, |v| yp.send(conn, v));
            node.poll(t, &mut ep);
            // Y's own connection to the node, which proves Y as well.
            let mut yc = Rogue::prove(&air, x, &mut node, &mut ep, t);
            yc.prove_id(&id(y), &yc.transcript.clone());
            yc.read(&mut node, &mut ep, t);
            let told: Vec<PeerId> = std::iter::from_fn(|| node.take())
                .filter_map(|e| match e {
                    Inbound::Identified { peer, .. } => Some(peer),
                    _ => None,
                })
                .collect();
            assert_eq!(told, [id(y).peer_id()], "told of the first only");
            assert_eq!(node.counts().0, 2, "Y's left to Y");
            assert_eq!(air.advert(0).map(|a| a[0] & FULL), Some(0), "nor does it fill the advert");
            node.poll(t + PROVE_WITHIN, &mut ep);
            let mine_closes = id(me).peer_id() > id(y).peer_id();
            let gone = std::iter::from_fn(|| yp.poll()).any(|e| matches!(e, RadioEvent::Disconnected { .. }));
            assert_eq!((gone, yc.closed()), (mine_closes, false), "me {me}, Y {y}");
            assert_eq!(node.counts().0, if mine_closes { 1 } else { 2 });
        }
    }

    /// NB110-7 (security's case a): a device with no room key connects to both desktops, which take one central
    /// each, never answers, and connects again each time it is closed. A new central takes the place of one that
    /// hasn't proven the room within `GRACE`, so the two still link; the advert counts no such central.
    #[test]
    fn a_central_that_never_answers_keeps_no_one_apart() {
        let mut w = Air::new(&[Limits::DESKTOP; 2]);
        let mut rogue = w.air.radio(Limits::DESKTOP);
        let mut on: HashMap<ConnId, usize> = HashMap::new();
        let mut tries = 0;
        for _ in 0..12_000 {
            for e in std::iter::from_fn(|| rogue.poll()).collect::<Vec<_>>() {
                match e {
                    RadioEvent::Connected { conn, device: Some(d), .. } => drop(on.insert(conn, d.0 as usize)),
                    RadioEvent::Disconnected { conn } => drop(on.remove(&conn)),
                    _ => {}
                }
            }
            for to in 0..2 {
                if !on.values().any(|t| *t == to) {
                    rogue.connect(DeviceId(to as u64));
                    tries += 1;
                }
            }
            w.run(10);
            if w.linked(0, 1) {
                break;
            }
        }
        assert!(w.linked(0, 1) && tries >= 2, "linked, after {tries} connects of the rogue's");
        let full = |i: usize| w.air.advert(i).is_some_and(|a| a[0] & FULL != 0);
        assert!(full(0) || full(1), "the honest central counts");
    }

    /// NB110-7: a new central over the limits takes the place of the oldest that hasn't proven the room, once it
    /// has had `GRACE`; until then, and in place of one that has, it is refused. The advert counts only centrals
    /// whose peer has proven who it is.
    #[test]
    fn a_new_central_takes_the_place_of_one_that_never_proves_the_room() {
        let air = FakeAir::new(23);
        let t = Instant::from_millis(1);
        let (mut node, mut ep) = lone(&air, Limits::DESKTOP, 1, t);
        let silent = |air: &FakeAir, node: &mut Ble, ep: &mut Endpoint<Hop<SocketAddr>>, t: Instant| {
            let mut r = air.radio(Limits::DESKTOP);
            r.connect(DeviceId(0));
            node.poll(t, ep);
            r
        };
        let gone =
            |r: &mut FakeRadio| std::iter::from_fn(|| r.poll()).any(|e| matches!(e, RadioEvent::Disconnected { .. }));
        let mut first = silent(&air, &mut node, &mut ep, t);
        assert_eq!(air.advert(0).map(|a| a[0] & FULL), Some(0), "an unproven central doesn't fill it");
        let mut early = silent(&air, &mut node, &mut ep, t + GRACE / 2);
        assert!(gone(&mut early) && !gone(&mut first), "refused within the first one's grace");
        let mut later = silent(&air, &mut node, &mut ep, t + GRACE);
        assert!(gone(&mut first) && !gone(&mut later), "takes its place after");
        let at = t + GRACE * 2;
        let mut member = Rogue::prove(&air, DeviceId(0), &mut node, &mut ep, at);
        assert!(gone(&mut later), "so does a member");
        member.prove_id(&id(5), &member.transcript.clone());
        member.read(&mut node, &mut ep, at);
        assert_eq!(air.advert(0).map(|a| a[0] & FULL), Some(FULL), "a proven member's fills it");
        let mut after = silent(&air, &mut node, &mut ep, at + GRACE * 2);
        assert!(gone(&mut after) && !member.closed(), "and isn't displaced");

        // Over the unbound limit, the same: the oldest unproven connection makes way.
        let air = FakeAir::new(23);
        let limits = Limits { unbound: 2, centrals: 8, ..Limits::DESKTOP };
        let (mut node, mut ep) = lone(&air, limits, 2, t);
        let mut rs: Vec<FakeRadio> = (0..2).map(|_| silent(&air, &mut node, &mut ep, t)).collect();
        let mut third = silent(&air, &mut node, &mut ep, t + GRACE);
        assert!(gone(&mut rs[0]) && !gone(&mut rs[1]) && !gone(&mut third), "the oldest went");
    }

    /// NB110-2: Mallory's four members, with adverts above everyone's, take all of Bob's unbound connections, and
    /// keep them alive with hellos. A connection that proves the room into the last unbound slot closes the weakest
    /// other, so Bob keeps one free and still connects to Alice, whose advert is above his.
    #[test]
    fn standbys_leave_a_slot_for_a_newcomer() {
        let roomy = Limits { centrals: 8, unbound: 8, ..Limits::DESKTOP }; // Mallory's never close Bob's
        let mut w = Air::new(&[roomy, roomy, roomy, roomy, Limits::DESKTOP, Limits::DESKTOP]);
        for i in 0..4 {
            w.set_n(i, u32::MAX - i as u32);
        }
        w.set_n(4, 2_000); // Alice
        w.set_n(5, 1_000); // Bob
        w.run(5_000);
        assert!(w.linked(4, 5), "Bob connected to Alice");
        let unbound = w.b[5].conns.values().filter(|c| c.bound.is_none()).count();
        assert!(unbound < Limits::DESKTOP.unbound, "one slot free: {unbound}");
    }

    #[test]
    fn a_link_binds_its_connection_and_datagrams_cross() {
        let mut w = Air::new(&[Limits::DESKTOP; 2]);
        w.run(3_000);
        w.datagrams = true;
        // Each side learns the other's connection from the radio, and dials over it.
        let hops: Vec<Hop<SocketAddr>> = w.ids.iter().map(|ids| ids[0].1).collect();
        let (x, y) = (id(1).peer_id(), id(2).peer_id());
        let now = w.now;
        w.eps[0].connect(now, hops[0], y).unwrap();
        w.run(1_500);
        assert!(w.eps[0].is_connected(&y) && w.eps[1].is_connected(&x));
        let counts: Vec<_> = w.b.iter().map(Ble::counts).collect();
        assert_eq!(counts, vec![(1, 1, 1); 2], "bound both ends");
        w.eps[0].send(&y, Lane::Reliable, &[5u8; 900]).unwrap();
        w.run(200);
        let got = std::iter::from_fn(|| w.eps[1].poll_event())
            .any(|e| matches!(e, peerwire_core::Event::Message { data, .. } if data == vec![5u8; 900]));
        assert!(got, "900 bytes in 45 chunks of 20");
    }
}
