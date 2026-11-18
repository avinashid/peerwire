//! Connections over ESP-NOW, sans-IO: the host hands in the frames received, the radio's verdict on each frame sent,
//! and the time, and takes out the frames to send and the [`RadioEvent`]s for the node. `docs/concepts/espnow.md` has
//! the protocol and what a forged frame can do.

pub use super::frame::MAX_VALUE;
use super::frame::{self, Frame, Malformed, DATA_HEADER, MAX_ADVERT, V1_FRAME, V2_FRAME};
use super::{Mac, Settings, Stats, BROADCAST};
use crate::ble::{ConnId, DeviceId, RadioEvent};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// A data frame up to this many past the last one taken is taken; the ones between are lost.
pub const WINDOW: u32 = 64;
/// Data frames in a row further ahead than [`WINDOW`], after which the receiver takes the next as its new place: its
/// sender lost more than a window of frames. One forged far ahead is dropped.
pub const RESYNC_AFTER: u32 = 4;
/// Events queued for the node at most: adverts, and values. Connections coming and going are never dropped.
const MAX_SEEN_QUEUED: usize = 16;
const MAX_VALUES_QUEUED: usize = 128;
/// Refusals (a CLOSE for an accept no open waits for) go at most this often, so a flood of frames from made-up
/// addresses gets few answers.
const REFUSE_GAP: Duration = Duration::from_millis(100);
/// A CLOSE is taken only if nothing comes on the connection for this long after the PROBE it sends: a peer that still
/// has the connection answers at once. So a forged CLOSE, which anyone in range can send, closes nothing.
pub const CLOSE_CHECK: Duration = Duration::from_millis(500);
/// A connection answers PROBEs at most this often.
const PROBE_GAP: Duration = Duration::from_millis(100);
/// Frames handed to the ether whose verdict hasn't come, at most; one whose verdict hasn't come in this long is taken
/// as lost, so an ether that drops a verdict can't stop the engine sending.
const MAX_FLIGHT: usize = 64;
const FLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

/// The device a node is told of for a MAC address, and back.
pub fn device_of(mac: Mac) -> DeviceId {
    let mut b = [0u8; 8];
    b[2..].copy_from_slice(&mac);
    DeviceId(u64::from_be_bytes(b))
}

/// The MAC address of a device from [`device_of`]: its low 48 bits.
pub fn mac_of(device: DeviceId) -> Mac {
    let b = device.0.to_be_bytes();
    [b[2], b[3], b[4], b[5], b[6], b[7]]
}

/// A group (multicast or broadcast) address: never a frame's sender, nor connected to.
fn group(mac: Mac) -> bool {
    mac[0] & 1 != 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// This end opened it and waits for the accept: OPEN again at `next`, given up at `deadline`.
    Opening {
        next: Instant,
        deadline: Instant,
    },
    Open,
}

struct Conn {
    mac: Mac,
    /// This end opened it (the node's central).
    central: bool,
    device: Option<DeviceId>,
    /// This end's id for it, which frames to this end carry, and the other end's (0 until its accept).
    mine: u64,
    theirs: u64,
    state: State,
    /// The longest frame this end sends on it: the shorter of the two ends' longest.
    frame: usize,
    /// The next data frame's number, and the last one taken.
    tx_seq: u32,
    rx_last: u32,
    /// Data frames in a row further than [`WINDOW`] ahead.
    ahead: u32,
    /// A value whose first frame came and whose last hasn't, and how long its frames say it is.
    partial: Vec<u8>,
    assembling: bool,
    total: usize,
    /// When a frame for this end last came on it, and when one was last queued on it.
    heard: Instant,
    sent: Instant,
    /// Frames in a row the radio said it couldn't deliver.
    failures: u32,
    /// When a CLOSE came that the other end hasn't answered the probe of yet; when this end last answered a probe.
    closing: Option<Instant>,
    answered: Option<Instant>,
    /// Values queued or in flight.
    values: usize,
}

impl Conn {
    fn open(&self) -> bool {
        self.state == State::Open
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Control,
    Open,
    Accept,
    Advert,
    Keepalive,
    Data { end: bool },
}

/// A frame waiting for the ether.
struct Out {
    to: Mac,
    frame: Vec<u8>,
    conn: Option<ConnId>,
    kind: Kind,
}

/// A frame the ether took, waiting for its verdict.
struct Sent {
    conn: Option<ConnId>,
    end: bool,
    at: Instant,
}

/// ESP-NOW connections, emulated: every connection the node sees, with its own ids, sequence numbers and timers.
pub struct Engine {
    settings: Settings,
    /// The longest frame this end sends and takes: 250 bytes on ESP-NOW v1, up to 1470 on v2.
    max_frame: usize,
    rng: ChaCha20Rng,
    advert: Option<Vec<u8>>,
    next_advert: Instant,
    scanning: bool,
    conns: BTreeMap<ConnId, Conn>,
    next_conn: u32,
    tx: VecDeque<Out>,
    flight: VecDeque<Sent>,
    events: VecDeque<RadioEvent>,
    seen_queued: usize,
    values_queued: usize,
    refused_at: Option<Instant>,
    stats: Stats,
}

impl Engine {
    /// An engine whose ether carries frames of up to `max_frame` bytes (250 to 1470), drawing its ids and jitter from
    /// `seed`, which must be secret and fresh: the ids are what tell a forged frame from one a peer sent.
    pub fn new(settings: Settings, max_frame: usize, seed: [u8; 32], now: Instant) -> Engine {
        Engine {
            settings,
            max_frame: max_frame.clamp(V1_FRAME, V2_FRAME),
            rng: ChaCha20Rng::from_seed(seed),
            advert: None,
            next_advert: now,
            scanning: false,
            conns: BTreeMap::new(),
            next_conn: 1,
            tx: VecDeque::new(),
            flight: VecDeque::new(),
            events: VecDeque::new(),
            seen_queued: 0,
            values_queued: 0,
            refused_at: None,
            stats: Stats::default(),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn stats(&self) -> Stats {
        Stats { connections: self.conns.len(), ..self.stats }
    }

    /// The longest frame this end takes.
    pub fn max_frame(&self) -> usize {
        self.max_frame
    }

    /// Advertises `data` (at most 31 bytes) every [`Settings::advert_every`], from the first [`timers`](Self::timers)
    /// on; `None` stops. Opens are accepted only while advertising.
    pub fn advertise(&mut self, now: Instant, data: Option<&[u8]>) {
        self.tx.retain(|o| o.kind != Kind::Advert);
        self.advert = None;
        match data {
            Some(d) if !d.is_empty() && d.len() <= MAX_ADVERT => {
                self.advert = Some(d.to_vec());
                self.next_advert = now;
            }
            Some(_) => self.events.push_back(RadioEvent::Advertising(false)),
            None => {}
        }
    }

    /// Reports adverts heard ([`RadioEvent::Seen`]) while on.
    pub fn scan(&mut self, on: bool) {
        self.scanning = on;
    }

    /// Opens a connection to `device`: OPEN every [`Settings::open_every`] until it is accepted
    /// ([`RadioEvent::Connected`]), refused or [`Settings::connect_within`] passes ([`RadioEvent::ConnectFailed`]).
    pub fn connect(&mut self, now: Instant, device: DeviceId) {
        let mac = mac_of(device);
        if group(mac) || device.0 >> 48 != 0 || self.conns.len() >= self.settings.limits.connections {
            self.events.push_back(RadioEvent::ConnectFailed { device });
            return;
        }
        if self.conns.values().any(|c| c.central && c.mac == mac && !c.open()) {
            return; // one under way
        }
        let (conn, mine) = (self.new_conn(), self.new_id());
        let deadline = now + self.settings.connect_within;
        let state = State::Opening { next: now + self.settings.open_every, deadline };
        self.conns.insert(conn, self.conn(mac, true, Some(device), mine, 0, state, now));
        self.queue_open(now, conn);
    }

    /// Queues `value` on `conn`, split into frames as long as the connection takes. False if the connection has
    /// [`Limits::queue`](crate::ble::Limits::queue) values waiting, or isn't open: the node tries again later. An
    /// empty value, or one over 512 bytes, is dropped.
    pub fn send(&mut self, now: Instant, conn: ConnId, value: &[u8]) -> bool {
        let queue = self.settings.limits.queue;
        let Some(c) = self.conns.get_mut(&conn).filter(|c| c.open()) else { return false };
        if value.is_empty() || value.len() > MAX_VALUE {
            self.stats.values_refused += 1;
            return true;
        }
        if c.values >= queue {
            return false;
        }
        let room = c.frame - DATA_HEADER;
        let n = value.len().div_ceil(room);
        for (i, payload) in value.chunks(room).enumerate() {
            let last = i + 1 == n;
            let total = value.len() as u16;
            let f = Frame::Data { to: c.theirs, seq: c.tx_seq, first: i == 0, last, total, payload };
            c.tx_seq = c.tx_seq.wrapping_add(1);
            self.tx.push_back(Out { to: c.mac, frame: f.encode(), conn: Some(conn), kind: Kind::Data { end: last } });
        }
        c.values += 1;
        c.sent = now;
        self.stats.values_out += 1;
        true
    }

    /// Closes `conn`, telling the other end, with no event to the node.
    pub fn disconnect(&mut self, now: Instant, conn: ConnId) {
        let Some(c) = self.conns.remove(&conn) else { return };
        self.purge(conn);
        if c.theirs != 0 {
            self.queue(now, c.mac, Frame::Close { to: c.theirs }, None, Kind::Control);
        }
    }

    /// A frame from `from`, as the ether received it.
    pub fn handle(&mut self, now: Instant, from: Mac, bytes: &[u8]) {
        self.stats.frames_in += 1;
        let f = match frame::parse(bytes) {
            Ok(f) => f,
            Err(Malformed::NotOurs) => {
                self.stats.not_ours += 1;
                return;
            }
            Err(_) => {
                self.stats.malformed += 1;
                return;
            }
        };
        if group(from) {
            self.stats.malformed += 1;
            return;
        }
        match f {
            Frame::Advert { data } => {
                if !self.scanning {
                    return;
                }
                if self.seen_queued >= MAX_SEEN_QUEUED {
                    self.stats.events_dropped += 1;
                    return;
                }
                self.seen_queued += 1;
                self.events.push_back(RadioEvent::Seen { device: device_of(from), data: data.to_vec() });
            }
            Frame::Open { from: theirs, rx_max } => self.on_open(now, from, theirs, rx_max),
            Frame::Accept { to, from: theirs, rx_max } => self.on_accept(now, from, to, theirs, rx_max),
            Frame::Data { to, seq, first, last, total, payload } => {
                self.on_data(now, from, to, seq, (first, last, total), payload)
            }
            Frame::Keepalive { to } => {
                if self.live(now, from, to).is_none() {
                    self.stats.unknown += 1;
                }
            }
            Frame::Probe { to } => match self.live(now, from, to) {
                Some(conn) => {
                    let c = &self.conns[&conn];
                    if c.answered.is_none_or(|t| now.saturating_duration_since(t) >= PROBE_GAP) {
                        let (mac, theirs) = (c.mac, c.theirs);
                        self.conns.get_mut(&conn).expect("live").answered = Some(now);
                        self.queue(now, mac, Frame::Keepalive { to: theirs }, Some(conn), Kind::Keepalive);
                    }
                }
                None => self.stats.unknown += 1,
            },
            // Not taken at its word: anyone in range can send one. The connection is closed only if the other end
            // doesn't answer the probe ([`CLOSE_CHECK`]).
            Frame::Close { to } => {
                match self.find(from, to).and_then(|conn| Some((conn, self.conns.get_mut(&conn)?))) {
                    Some((conn, c)) if c.open() && c.closing.is_none() => {
                        c.closing = Some(now);
                        let (mac, theirs) = (c.mac, c.theirs);
                        self.queue(now, mac, Frame::Probe { to: theirs }, Some(conn), Kind::Control);
                    }
                    Some((_, c)) if c.open() => {}
                    _ => self.stats.unknown += 1,
                }
            }
        }
    }

    /// The open connection with `mac` that this end knows as `mine`, which a frame just proved alive: a CLOSE that
    /// came on it is then denied.
    fn live(&mut self, now: Instant, mac: Mac, mine: u64) -> Option<ConnId> {
        let conn = self.find(mac, mine)?;
        let c = self.conns.get_mut(&conn).filter(|c| c.open())?;
        c.heard = now;
        if c.closing.take().is_some() {
            self.stats.closes_denied += 1;
        }
        Some(conn)
    }

    fn on_open(&mut self, now: Instant, mac: Mac, theirs: u64, rx_max: u16) {
        // Again: the accept went missing.
        let again = self.conns.iter().find(|(_, c)| !c.central && c.mac == mac && c.theirs == theirs);
        if let Some((&conn, c)) = again {
            let mine = c.mine;
            if !self.tx.iter().any(|o| o.conn == Some(conn) && o.kind == Kind::Accept) {
                let rx_max = self.max_frame as u16;
                self.queue(now, mac, Frame::Accept { to: theirs, from: mine, rx_max }, Some(conn), Kind::Accept);
            }
            return;
        }
        // Not advertising, or no room: silence, as from a Bluetooth peripheral that takes no connection. A refusal
        // would be one more frame to forge; the opener gives up after `connect_within`, and the node's adverts say
        // when it is full.
        let limits = self.settings.limits;
        let centrals = self.conns.values().filter(|c| !c.central).count();
        if self.advert.is_none() || self.conns.len() >= limits.connections || centrals >= limits.centrals {
            self.stats.opens_refused += 1;
            return;
        }
        let (conn, mine) = (self.new_conn(), self.new_id());
        let mut c = self.conn(mac, false, None, mine, theirs, State::Open, now);
        c.frame = usize::from(rx_max).min(self.max_frame);
        self.conns.insert(conn, c);
        let rx_max = self.max_frame as u16;
        self.queue(now, mac, Frame::Accept { to: theirs, from: mine, rx_max }, Some(conn), Kind::Accept);
        self.events.push_back(RadioEvent::Connected { conn, device: None, mtu: self.settings.mtu });
    }

    fn on_accept(&mut self, now: Instant, mac: Mac, to: u64, theirs: u64, rx_max: u16) {
        let Some(conn) = self.find(mac, to) else {
            // An accept for an open given up, or never sent: closed at the other end too.
            self.stats.unknown += 1;
            if self.may_refuse(now) {
                self.queue(now, mac, Frame::Close { to: theirs }, None, Kind::Control);
            }
            return;
        };
        let (mtu, max_frame) = (self.settings.mtu, self.max_frame);
        let Some(c) = self.conns.get_mut(&conn) else { return };
        if c.open() {
            // The same accept again is the other end alive; another is forged or stale, and ignored.
            if c.theirs == theirs {
                self.live(now, mac, to);
            } else {
                self.stats.unknown += 1;
            }
            return;
        }
        (c.theirs, c.state, c.frame) = (theirs, State::Open, usize::from(rx_max).min(max_frame));
        (c.heard, c.sent) = (now, now);
        self.tx.retain(|o| !(o.conn == Some(conn) && o.kind == Kind::Open));
        self.events.push_back(RadioEvent::Connected { conn, device: c.device, mtu });
    }

    /// A data frame: `part` is its flags (first, last) and its value's length.
    fn on_data(&mut self, now: Instant, mac: Mac, to: u64, seq: u32, part: (bool, bool, u16), payload: &[u8]) {
        let (first, last, total) = (part.0, part.1, usize::from(part.2));
        let Some(conn) = self.find(mac, to) else {
            self.stats.unknown += 1;
            return;
        };
        if self.live(now, mac, to).is_none() {
            self.stats.unknown += 1; // data before the accept
            return;
        }
        let Some(c) = self.conns.get_mut(&conn) else { return };
        // Where it falls: the last one again, or behind it; in the window; or further ahead.
        let d = seq.wrapping_sub(c.rx_last);
        if d == 0 || d > 1 << 31 {
            self.stats.old += 1;
            return;
        }
        if d > WINDOW {
            c.ahead += 1;
            if c.ahead < RESYNC_AFTER {
                self.stats.ahead += 1;
                return;
            }
            self.stats.resyncs += 1;
        }
        c.ahead = 0;
        if d > 1 {
            self.stats.gaps += 1;
            c.assembling = false;
        }
        c.rx_last = seq;
        if first {
            c.partial.clear();
            (c.assembling, c.total) = (true, total);
        } else if !c.assembling {
            self.stats.orphans += 1; // the rest of a value whose start was lost
            return;
        }
        // Every part says the same length, and the parts add up to it: a part cut short or forged can't pass.
        if total != c.total
            || c.partial.len() + payload.len() > total
            || last && c.partial.len() + payload.len() != total
        {
            self.stats.malformed += 1;
            c.partial = Vec::new();
            c.assembling = false;
            return;
        }
        c.partial.extend_from_slice(payload);
        if !last {
            return;
        }
        c.assembling = false;
        let value = std::mem::take(&mut c.partial);
        if self.values_queued >= MAX_VALUES_QUEUED {
            self.stats.events_dropped += 1;
            return;
        }
        self.values_queued += 1;
        self.stats.values_in += 1;
        self.events.push_back(RadioEvent::Value { conn, value });
    }

    /// The ether's verdict on the oldest frame it took and hasn't given one for: delivered (acknowledged, or a
    /// broadcast that went) or not.
    pub fn outcome(&mut self, ok: bool) {
        let Some(s) = self.flight.pop_front() else {
            self.stats.stray_verdicts += 1;
            return;
        };
        if !ok {
            self.stats.tx_failed += 1;
        }
        let max = self.settings.max_failures;
        let Some(conn) = s.conn else { return };
        let Some(c) = self.conns.get_mut(&conn) else { return };
        if s.end {
            c.values = c.values.saturating_sub(1);
        }
        if ok {
            c.failures = 0;
            return;
        }
        c.failures += 1;
        if c.failures >= max && c.open() {
            self.stats.closed_failing += 1;
            self.close(conn);
        }
    }

    /// Adverts, opens and keepalives that are due, and connections given up or gone quiet.
    pub fn timers(&mut self, now: Instant) {
        // A verdict that never came: the frame is taken as lost.
        while self.flight.front().is_some_and(|s| now.saturating_duration_since(s.at) >= FLIGHT_TIMEOUT) {
            self.stats.lost_verdicts += 1;
            self.outcome(false);
        }
        if let Some(data) = self.advert.clone().filter(|_| now >= self.next_advert) {
            if !self.tx.iter().any(|o| o.kind == Kind::Advert) {
                self.queue(now, BROADCAST, Frame::Advert { data: &data }, None, Kind::Advert);
            }
            // ±12.5% so two ends don't keep colliding.
            let every = self.settings.advert_every;
            let jitter = every / 4 * (self.rng.next_u32() % 1000) / 1000;
            self.next_advert = now + every - every / 8 + jitter;
        }
        let (keepalive, timeout, open_every) =
            (self.settings.keepalive, self.settings.timeout, self.settings.open_every);
        let mut gone = Vec::new();
        let mut closed = Vec::new();
        let mut quiet = Vec::new();
        let mut opens = Vec::new();
        let mut keep = Vec::new();
        for (&conn, c) in &mut self.conns {
            match c.state {
                State::Opening { deadline, .. } if now >= deadline => gone.push(conn),
                State::Opening { next, deadline } if now >= next => {
                    c.state = State::Opening { next: now + open_every, deadline };
                    opens.push(conn);
                }
                State::Opening { .. } => {}
                State::Open if c.closing.is_some_and(|t| now.saturating_duration_since(t) >= CLOSE_CHECK) => {
                    closed.push(conn)
                }
                State::Open if now.saturating_duration_since(c.heard) >= timeout => quiet.push(conn),
                State::Open if now.saturating_duration_since(c.sent) >= keepalive => {
                    c.sent = now;
                    keep.push((conn, c.mac, c.theirs));
                }
                State::Open => {}
            }
        }
        for conn in gone {
            self.close(conn);
        }
        for conn in closed {
            self.stats.closed_by_peer += 1;
            self.close(conn);
        }
        for conn in quiet {
            self.stats.closed_quiet += 1;
            self.close(conn);
        }
        for conn in opens {
            if !self.tx.iter().any(|o| o.conn == Some(conn) && o.kind == Kind::Open) {
                self.queue_open(now, conn);
            }
        }
        for (conn, mac, to) in keep {
            self.queue(now, mac, Frame::Keepalive { to }, Some(conn), Kind::Keepalive);
        }
    }

    /// The next frame for the ether, if there is room for one in flight. [`commit`](Self::commit) once the ether took
    /// it; leave it if not, and it is offered again.
    pub fn peek(&self) -> Option<(Mac, &[u8])> {
        if self.flight.len() >= MAX_FLIGHT {
            return None;
        }
        self.tx.front().map(|o| (o.to, &o.frame[..]))
    }

    /// The ether took the frame [`peek`](Self::peek) gave.
    pub fn commit(&mut self, now: Instant) {
        let Some(o) = self.tx.pop_front() else { return };
        self.stats.frames_out += 1;
        let end = matches!(o.kind, Kind::Data { end: true });
        self.flight.push_back(Sent { conn: o.conn, end, at: now });
    }

    pub fn poll_event(&mut self) -> Option<RadioEvent> {
        let e = self.events.pop_front()?;
        match e {
            RadioEvent::Seen { .. } => self.seen_queued -= 1,
            RadioEvent::Value { .. } => self.values_queued -= 1,
            _ => {}
        }
        Some(e)
    }

    /// When [`timers`](Self::timers) next has something to do.
    pub fn next_timeout(&self) -> Option<Instant> {
        let s = &self.settings;
        let advert = self.advert.as_ref().map(|_| self.next_advert);
        let conns = self.conns.values().map(|c| match c.state {
            State::Opening { next, deadline } => next.min(deadline),
            State::Open => {
                let t = (c.heard + s.timeout).min(c.sent + s.keepalive);
                c.closing.map_or(t, |at| t.min(at + CLOSE_CHECK))
            }
        });
        let flight = self.flight.front().map(|f| f.at + FLIGHT_TIMEOUT);
        conns.chain(advert).chain(flight).min()
    }

    fn new_conn(&mut self) -> ConnId {
        let conn = ConnId(self.next_conn);
        self.next_conn = self.next_conn.wrapping_add(1).max(1);
        conn
    }

    /// A fresh id: random, never 0, and not one of this end's open connections'.
    fn new_id(&mut self) -> u64 {
        loop {
            let id = self.rng.next_u64();
            if id != 0 && !self.conns.values().any(|c| c.mine == id) {
                return id;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn conn(
        &self,
        mac: Mac,
        central: bool,
        device: Option<DeviceId>,
        mine: u64,
        theirs: u64,
        state: State,
        now: Instant,
    ) -> Conn {
        Conn {
            mac,
            central,
            device,
            mine,
            theirs,
            state,
            frame: V1_FRAME,
            tx_seq: 0,
            rx_last: u32::MAX,
            ahead: 0,
            partial: Vec::new(),
            assembling: false,
            total: 0,
            heard: now,
            sent: now,
            failures: 0,
            closing: None,
            answered: None,
            values: 0,
        }
    }

    /// The connection with `mac` that this end knows as `mine`.
    fn find(&self, mac: Mac, mine: u64) -> Option<ConnId> {
        self.conns.iter().find(|(_, c)| c.mine == mine && c.mac == mac).map(|(&conn, _)| conn)
    }

    fn queue_open(&mut self, now: Instant, conn: ConnId) {
        let Some(c) = self.conns.get(&conn) else { return };
        let (mac, mine, rx_max) = (c.mac, c.mine, self.max_frame as u16);
        self.queue(now, mac, Frame::Open { from: mine, rx_max }, Some(conn), Kind::Open);
    }

    /// Queues `f` for `to`: a keepalive is due on `conn` only after [`Settings::keepalive`] with nothing queued.
    fn queue(&mut self, now: Instant, to: Mac, f: Frame, conn: Option<ConnId>, kind: Kind) {
        if let Some(c) = conn.and_then(|conn| self.conns.get_mut(&conn)) {
            c.sent = now;
        }
        self.tx.push_back(Out { to, frame: f.encode(), conn, kind });
    }

    /// Whether a refusal may go now ([`REFUSE_GAP`]).
    fn may_refuse(&mut self, now: Instant) -> bool {
        if self.refused_at.is_some_and(|t| now.saturating_duration_since(t) < REFUSE_GAP) {
            return false;
        }
        self.refused_at = Some(now);
        self.stats.refusals += 1;
        true
    }

    /// Drops the frames `conn` has waiting for the ether.
    fn purge(&mut self, conn: ConnId) {
        self.tx.retain(|o| o.conn != Some(conn));
    }

    /// `conn` went, as the node hears: [`RadioEvent::Disconnected`], or [`RadioEvent::ConnectFailed`] for an open
    /// that wasn't accepted.
    fn close(&mut self, conn: ConnId) {
        let Some(c) = self.conns.remove(&conn) else { return };
        self.purge(conn);
        self.events.push_back(match (c.open(), c.device) {
            (false, Some(device)) => RadioEvent::ConnectFailed { device },
            _ => RadioEvent::Disconnected { conn },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Mac = [0x02, 0, 0, 0, 0, 0x0a];
    const B: Mac = [0x02, 0, 0, 0, 0, 0x0b];
    const X: Mac = [0x02, 0, 0, 0, 0, 0x66];
    const ADVERT: [u8; 9] = [0, 1, 2, 3, 4, 5, 6, 7, 8];

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn engine(seed: u8, max_frame: usize, t: Instant) -> Engine {
        Engine::new(Settings::DEFAULT, max_frame, [seed; 32], t)
    }

    /// Two engines, `a` at A and `b` at B, and frames between them, every one delivered and acknowledged unless
    /// `drop` says otherwise; what went is in `wire`.
    struct Pair {
        a: Engine,
        b: Engine,
        t: Instant,
        wire: Vec<(Mac, Mac, Vec<u8>)>,
    }

    impl Pair {
        fn new(a_frame: usize, b_frame: usize) -> Pair {
            let t = Instant::now();
            Pair { a: engine(1, a_frame, t), b: engine(2, b_frame, t), t, wire: Vec::new() }
        }

        /// Moves frames both ways until neither has one for the ether.
        fn shuttle(&mut self) {
            self.shuttle_with(|_, _, _| true);
        }

        /// The same, delivering only the frames `deliver` passes (all acknowledged).
        fn shuttle_with(&mut self, mut deliver: impl FnMut(Mac, Mac, &[u8]) -> bool) {
            loop {
                let mut moved = false;
                for from_a in [true, false] {
                    let (tx, rx, from, to) = match from_a {
                        true => (&mut self.a, &mut self.b, A, B),
                        false => (&mut self.b, &mut self.a, B, A),
                    };
                    while let Some((dest, f)) = tx.peek() {
                        let f = f.to_vec();
                        tx.commit(self.t);
                        tx.outcome(true);
                        if (dest == to || dest == BROADCAST) && deliver(from, dest, &f) {
                            rx.handle(self.t, from, &f);
                        }
                        self.wire.push((from, dest, f));
                        moved = true;
                    }
                }
                if !moved {
                    return;
                }
            }
        }

        fn later(&mut self, d: Duration) {
            self.t += d;
            self.a.timers(self.t);
            self.b.timers(self.t);
        }

        /// `a` connects to `b`, which advertises; the two connections.
        fn link(&mut self) -> (ConnId, ConnId) {
            self.b.advertise(self.t, Some(&ADVERT));
            self.a.scan(true);
            self.later(ms(0));
            self.shuttle();
            assert_eq!(events(&mut self.a), [RadioEvent::Seen { device: device_of(B), data: ADVERT.to_vec() }]);
            self.a.connect(self.t, device_of(B));
            self.shuttle();
            let Some(RadioEvent::Connected { conn: ca, device: Some(d), mtu: 515 }) = self.a.poll_event() else {
                panic!("a connected")
            };
            assert_eq!(d, device_of(B));
            let Some(RadioEvent::Connected { conn: cb, device: None, mtu: 515 }) = self.b.poll_event() else {
                panic!("b accepted")
            };
            (ca, cb)
        }
    }

    fn events(e: &mut Engine) -> Vec<RadioEvent> {
        std::iter::from_fn(|| e.poll_event()).collect()
    }

    /// The connections that went, by the events (adverts heard and the rest left out).
    fn gone(e: &mut Engine) -> Vec<RadioEvent> {
        events(e)
            .into_iter()
            .filter(|e| matches!(e, RadioEvent::Disconnected { .. } | RadioEvent::ConnectFailed { .. }))
            .collect()
    }

    fn values(e: &mut Engine) -> Vec<Vec<u8>> {
        events(e)
            .into_iter()
            .filter_map(|e| match e {
                RadioEvent::Value { value, .. } => Some(value),
                _ => None,
            })
            .collect()
    }

    /// The ids of `a`'s first connection: its own, and `b`'s, from the frames on the wire.
    fn ids(p: &Pair) -> (u64, u64) {
        p.wire.iter().find_map(accept).expect("an accept went")
    }

    /// The same for the last connection.
    fn ids_last(p: &Pair) -> (u64, u64) {
        p.wire.iter().rev().find_map(accept).expect("an accept went")
    }

    fn accept((_, _, f): &(Mac, Mac, Vec<u8>)) -> Option<(u64, u64)> {
        match frame::parse(f) {
            Ok(Frame::Accept { to, from, .. }) => Some((to, from)),
            _ => None,
        }
    }

    fn value(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn a_connection_opens_and_carries_values_both_ways() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, cb) = p.link();
        let big = value(MAX_VALUE, 1);
        let before = p.wire.len();
        assert!(p.a.send(p.t, ca, &big));
        assert!(p.b.send(p.t, cb, b"back"));
        p.shuttle();
        assert_eq!(p.wire.len() - before, 3 + 1, "512 bytes in three v1 frames, 4 bytes in one");
        assert!(p.wire.iter().all(|(_, _, f)| f.len() <= V1_FRAME));
        assert_eq!(values(&mut p.b), [big]);
        assert_eq!(values(&mut p.a), [b"back".to_vec()]);
        assert_eq!(p.a.stats().values_out, 1);
    }

    #[test]
    fn frames_are_as_long_as_the_shorter_end_takes() {
        let mut p = Pair::new(V2_FRAME, V2_FRAME);
        let (ca, _) = p.link();
        let before = p.wire.len();
        assert!(p.a.send(p.t, ca, &value(MAX_VALUE, 2)));
        p.shuttle();
        assert_eq!(p.wire.len() - before, 1, "v2 at both ends: one frame");
        assert_eq!(values(&mut p.b).len(), 1);

        let mut p = Pair::new(V2_FRAME, V1_FRAME);
        let (ca, cb) = p.link();
        assert!(p.a.send(p.t, ca, &value(MAX_VALUE, 3)) && p.b.send(p.t, cb, &value(MAX_VALUE, 4)));
        p.shuttle();
        assert!(p.wire.iter().all(|(_, _, f)| f.len() <= V1_FRAME), "v2 to v1 sends v1 frames");
        assert_eq!((values(&mut p.a).len(), values(&mut p.b).len()), (1, 1));
    }

    #[test]
    fn duplicated_reordered_and_truncated_frames_lose_a_value_at_most() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, _) = p.link();
        // Three frames of one value, held back.
        assert!(p.a.send(p.t, ca, &value(MAX_VALUE, 5)));
        let mut held = Vec::new();
        p.shuttle_with(|_, _, f| {
            held.push(f.to_vec());
            false
        });
        assert_eq!(held.len(), 3);
        let feed = |p: &mut Pair, fs: &[&[u8]]| {
            for f in fs {
                p.b.handle(p.t, A, f);
            }
            values(&mut p.b)
        };
        // Each frame twice: one value, the copies dropped.
        assert_eq!(feed(&mut p, &[&held[0], &held[0], &held[1], &held[1], &held[2], &held[2]]).len(), 1);
        assert_eq!(p.b.stats().old, 3);
        // Again, the same frames: all stale.
        assert!(feed(&mut p, &[&held[0], &held[1], &held[2]]).is_empty());
        // Reordered: the middle first, then the rest. The value is lost, the next one comes.
        assert!(p.a.send(p.t, ca, &value(MAX_VALUE, 6)));
        held.clear();
        p.shuttle_with(|_, _, f| {
            held.push(f.to_vec());
            false
        });
        assert!(feed(&mut p, &[&held[1], &held[0], &held[2]]).is_empty());
        // Truncated: malformed, and the value it was part of is lost; the next one comes.
        assert!(p.a.send(p.t, ca, &value(MAX_VALUE, 7)));
        held.clear();
        p.shuttle_with(|_, _, f| {
            held.push(f.to_vec());
            false
        });
        assert!(feed(&mut p, &[&held[0], &held[1][..40], &held[2]]).is_empty());
        assert!(p.a.send(p.t, ca, b"after"));
        p.shuttle();
        assert_eq!(values(&mut p.b), [b"after".to_vec()]);
        let s = p.b.stats();
        assert!(s.malformed >= 1 && s.gaps >= 1 && s.orphans >= 1, "{s:?}");
    }

    #[test]
    fn a_close_is_taken_only_when_the_other_end_doesnt_answer_its_probe() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, cb) = p.link();
        let (_, b_id) = ids(&p);
        // From another address, or with another id: nothing.
        p.b.handle(p.t, X, &Frame::Close { to: b_id }.encode());
        p.b.handle(p.t, A, &Frame::Close { to: b_id ^ 1 }.encode());
        assert!(p.b.peek().is_none() && p.b.stats().unknown == 2);
        // As if from A, with the id overheard: b probes, a answers, nothing closes.
        p.b.handle(p.t, A, &Frame::Close { to: b_id }.encode());
        p.shuttle();
        p.later(CLOSE_CHECK * 2);
        p.shuttle();
        assert!(gone(&mut p.a).is_empty() && gone(&mut p.b).is_empty());
        assert_eq!(p.b.stats().closes_denied, 1);
        assert!(p.a.send(p.t, ca, b"still up"));
        p.shuttle();
        assert_eq!(values(&mut p.b), [b"still up".to_vec()]);
        // The node closes a's end: a CLOSE, which b's probe confirms, as a no longer has the connection.
        p.a.disconnect(p.t, ca);
        p.shuttle();
        assert!(gone(&mut p.b).is_empty(), "not before the probe goes unanswered");
        p.later(CLOSE_CHECK);
        assert_eq!(gone(&mut p.b), [RadioEvent::Disconnected { conn: cb }]);
        assert!(gone(&mut p.a).is_empty(), "no event for the end that closed");
        assert_eq!(p.b.stats().closed_by_peer, 1);
        // A forged CLOSE when a has really gone (the probe unanswered) closes b's end, as a's timeout would.
        let (_, cb) = p.link();
        let (_, b_id) = ids_last(&p);
        p.b.handle(p.t, A, &Frame::Close { to: b_id }.encode());
        p.shuttle_with(|from, _, _| from != B);
        p.later(CLOSE_CHECK);
        assert_eq!(gone(&mut p.b), [RadioEvent::Disconnected { conn: cb }]);
    }

    #[test]
    fn forged_data_loses_a_window_at_most_and_never_stalls_the_connection() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, _) = p.link();
        let (_, b_id) = ids(&p);
        let data =
            |seq: u32| Frame::Data { to: b_id, seq, first: true, last: true, total: 6, payload: b"forged" }.encode();
        // Far ahead: dropped, one at a time.
        p.b.handle(p.t, A, &data(10_000));
        assert!(p.a.send(p.t, ca, b"0"));
        p.shuttle();
        assert_eq!(values(&mut p.b), [b"0".to_vec()]);
        // At the window's edge: taken (the node drops it, unproven), and the next frames from a look stale until
        // a's numbers pass it.
        p.b.handle(p.t, A, &data(WINDOW));
        assert_eq!(values(&mut p.b), [b"forged".to_vec()]);
        let mut got = 0;
        for i in 0..(WINDOW + 8) {
            assert!(p.a.send(p.t, ca, format!("{i}").as_bytes()));
            p.shuttle();
            got += values(&mut p.b).len();
        }
        assert_eq!(got, 8, "the window's frames lost, then all");
        // A sender that lost more than a window is taken up again after a few frames.
        let mut q = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, _) = q.link();
        for _ in 0..(WINDOW + 10) {
            assert!(q.a.send(q.t, ca, b"lost"));
            q.shuttle_with(|_, _, _| false);
        }
        let mut got = 0;
        for _ in 0..RESYNC_AFTER + 2 {
            assert!(q.a.send(q.t, ca, b"kept"));
            q.shuttle();
            got += values(&mut q.b).len();
        }
        assert_eq!(got, 3, "the fourth frame ahead is taken, and the ones after");
        assert_eq!(q.b.stats().resyncs, 1);
    }

    #[test]
    fn a_forged_accept_ends_within_two_timeouts() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        p.b.advertise(p.t, Some(&ADVERT));
        p.a.connect(p.t, device_of(B));
        // The OPEN reaches B, whose ACCEPT is held back; someone who overheard the OPEN answers first, as B.
        p.shuttle_with(|from, _, _| from == A);
        let Some(RadioEvent::Connected { conn: cb, .. }) = p.b.poll_event() else { panic!("b accepted") };
        let Ok(Frame::Open { from: a_id, .. }) = frame::parse(&p.wire[0].2) else { panic!() };
        p.a.handle(p.t, B, &Frame::Accept { to: a_id, from: 0x1234, rx_max: 250 }.encode());
        let Some(RadioEvent::Connected { conn: ca, .. }) = p.a.poll_event() else { panic!("a took the forged one") };
        let real = p.wire.iter().find(|(from, _, _)| *from == B).map(|w| w.2.clone()).unwrap();
        p.a.handle(p.t, B, &real);
        assert!(events(&mut p.a).is_empty(), "the real accept comes too late, and is ignored");
        assert!(p.a.send(p.t, ca, b"lost"));
        p.shuttle();
        assert!(values(&mut p.b).is_empty(), "a sends to the forged id");
        let (mut a_gone, mut b_gone) = (None, None);
        for s in 1..=12 {
            p.later(ms(1_000));
            p.shuttle();
            for (e, at, conn) in [(&mut p.a, &mut a_gone, ca), (&mut p.b, &mut b_gone, cb)] {
                if gone(e).contains(&RadioEvent::Disconnected { conn }) {
                    *at = Some(s);
                }
            }
        }
        // b hears nothing valid from a, and closes after its timeout; a heard b's keepalives till then, and closes a
        // timeout after the last.
        assert_eq!((b_gone, a_gone), (Some(5), Some(9)));
    }

    #[test]
    fn opens_from_many_addresses_take_no_more_than_the_limits() {
        let t = Instant::now();
        let mut b = engine(2, V1_FRAME, t);
        b.advertise(t, Some(&ADVERT));
        let mut answers = 0;
        for i in 0..500u32 {
            let mac = [0x02, 0, 0, 0, (i >> 8) as u8, i as u8];
            b.handle(t + ms(u64::from(i)), mac, &Frame::Open { from: u64::from(i) + 1, rx_max: 250 }.encode());
            while b.peek().is_some() {
                b.commit(t);
                b.outcome(true);
                answers += 1;
            }
        }
        let limits = Settings::DEFAULT.limits;
        assert_eq!(b.stats().connections, limits.connections);
        assert_eq!(events(&mut b).len(), limits.connections, "one Connected each, no more");
        assert_eq!(b.stats().opens_refused, 500 - limits.connections as u64);
        assert_eq!(answers, limits.connections, "an accept each, and silence for the rest");
        // Not advertising: no answer at all.
        let mut quiet = engine(3, V1_FRAME, t);
        quiet.handle(t, A, &Frame::Open { from: 1, rx_max: 250 }.encode());
        assert!(quiet.peek().is_none() && events(&mut quiet).is_empty());
    }

    #[test]
    fn an_open_again_is_answered_again_not_taken_twice() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        p.b.advertise(p.t, Some(&ADVERT));
        p.a.connect(p.t, device_of(B));
        // The first accept is lost; a opens again; b answers again.
        p.shuttle_with(|from, _, _| from != B);
        p.later(ms(500));
        p.shuttle();
        let connected = |e: &mut Engine| events(e).iter().filter(|e| matches!(e, RadioEvent::Connected { .. })).count();
        assert_eq!((connected(&mut p.a), connected(&mut p.b)), (1, 1));
        assert_eq!(p.b.stats().connections, 1);
    }

    #[test]
    fn connects_fail_when_unanswered_or_out_of_room() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        // B never answers: an OPEN every 500 ms, given up after 10 s.
        p.a.connect(p.t, device_of(B));
        let mut opens = 0;
        for _ in 0..21 {
            while p.a.peek().is_some() {
                p.a.commit(p.t);
                p.a.outcome(false);
                opens += 1;
            }
            p.later(ms(500));
        }
        assert_eq!(opens, 20);
        assert_eq!(events(&mut p.a), [RadioEvent::ConnectFailed { device: device_of(B) }]);
        // No room, or no such device.
        let mut e = engine(9, V1_FRAME, p.t);
        for i in 0..Settings::DEFAULT.limits.connections + 1 {
            e.connect(p.t, device_of([2, 0, 0, 0, 1, i as u8]));
        }
        e.connect(p.t, DeviceId(1 << 60));
        e.connect(p.t, device_of(BROADCAST));
        let failed = events(&mut e).iter().filter(|e| matches!(e, RadioEvent::ConnectFailed { .. })).count();
        assert_eq!(failed, 3);
    }

    #[test]
    fn keepalives_keep_a_quiet_connection_and_its_end_closes_it_when_the_other_goes() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, cb) = p.link();
        for _ in 0..20 {
            p.later(ms(500));
            p.shuttle();
        }
        assert!(gone(&mut p.a).is_empty() && gone(&mut p.b).is_empty(), "10 s quiet, kept by keepalives");
        let keepalives = p.wire.iter().filter(|(_, _, f)| matches!(frame::parse(f), Ok(Frame::Keepalive { .. })));
        assert!((16..=24).contains(&keepalives.count()));
        // The node closes a's end: b is told, and a hears nothing of it.
        p.a.disconnect(p.t, ca);
        p.shuttle();
        p.later(CLOSE_CHECK);
        assert!(gone(&mut p.a).is_empty());
        assert_eq!(gone(&mut p.b), [RadioEvent::Disconnected { conn: cb }]);
        // b's end, from a link that went silent: gone after the timeout.
        let (ca, _) = p.link();
        for _ in 0..5 {
            p.later(ms(1_000));
            p.shuttle_with(|from, _, _| from != B);
        }
        assert_eq!(gone(&mut p.a), [RadioEvent::Disconnected { conn: ca }]);
        assert_eq!(p.a.stats().closed_quiet, 1);
    }

    #[test]
    fn frames_the_radio_cant_deliver_close_the_connection_and_free_the_queue() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, _) = p.link();
        let queue = Settings::DEFAULT.limits.queue;
        for _ in 0..queue {
            assert!(p.a.send(p.t, ca, &value(300, 1)));
        }
        assert!(!p.a.send(p.t, ca, b"one more"), "the queue is full");
        // Delivered: the queue frees.
        p.shuttle();
        assert!(p.a.send(p.t, ca, b"one more"));
        p.shuttle();
        // Undelivered: after 8 in a row, the connection is gone.
        for _ in 0..4 {
            assert!(p.a.send(p.t, ca, &value(300, 2)));
        }
        let mut n = 0;
        while p.a.peek().is_some() {
            p.a.commit(p.t);
            p.a.outcome(false);
            n += 1;
        }
        assert_eq!(n, 8, "4 values of 2 frames, the last of them closing the connection");
        assert_eq!(gone(&mut p.a), [RadioEvent::Disconnected { conn: ca }]);
        assert_eq!(p.a.stats().closed_failing, 1);
        assert!(!p.a.send(p.t, ca, b"gone"));
    }

    #[test]
    fn an_accept_for_an_open_given_up_is_closed_and_refusals_are_rate_limited() {
        let t = Instant::now();
        let mut a = engine(1, V1_FRAME, t);
        for i in 0..50u64 {
            a.handle(t + ms(i * 10), B, &Frame::Accept { to: 7 + i, from: 9, rx_max: 250 }.encode());
        }
        let mut closes = 0;
        while let Some((to, f)) = a.peek() {
            assert_eq!((to, frame::parse(f)), (B, Ok(Frame::Close { to: 9 })));
            a.commit(t);
            closes += 1;
        }
        assert_eq!(closes, 5, "one per 100 ms over 500 ms");
        assert_eq!(a.stats().unknown, 50);
    }

    #[test]
    fn adverts_go_while_advertising_and_are_heard_while_scanning() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        p.b.advertise(p.t, Some(&ADVERT));
        for _ in 0..50 {
            p.later(ms(100));
            p.shuttle();
        }
        let adverts = p.wire.iter().filter(|(_, to, _)| *to == BROADCAST).count();
        assert!((9..=12).contains(&adverts), "{adverts} in 5 s");
        assert!(events(&mut p.a).is_empty(), "a isn't scanning");
        p.a.scan(true);
        for _ in 0..40 {
            p.later(ms(500));
            p.shuttle();
        }
        assert_eq!(events(&mut p.a).len(), MAX_SEEN_QUEUED, "adverts queued for a slow node are capped");
        p.b.advertise(p.t, None);
        p.later(ms(2_000));
        p.shuttle();
        assert!(events(&mut p.a).is_empty(), "stopped");
        p.b.advertise(p.t, Some(&[0; MAX_ADVERT + 1]));
        assert_eq!(events(&mut p.b), [RadioEvent::Advertising(false)]);
    }

    /// Any frames, from anyone, at any time, mixed with a real connection's: no panic, and the state stays within its
    /// bounds.
    #[test]
    fn fuzz_engine() {
        use peerwire_sim::fuzz::{cases, mutate, Rng};
        let limits = Settings::DEFAULT.limits;
        let mut rng = Rng::for_target("espnow-engine");
        for _ in 0..cases(40) {
            let mut p = Pair::new(*rng.pick(&[V1_FRAME, V2_FRAME]), *rng.pick(&[V1_FRAME, V2_FRAME]));
            let (ca, cb) = p.link();
            let (a_id, b_id) = ids(&p);
            let seeds: Vec<Vec<u8>> = vec![
                Frame::Data { to: b_id, seq: 3, first: true, last: false, total: 200, payload: &[1; 100] }.encode(),
                Frame::Data { to: b_id, seq: 4, first: false, last: true, total: 200, payload: &[2; 100] }.encode(),
                Frame::Keepalive { to: b_id }.encode(),
                Frame::Close { to: b_id }.encode(),
                Frame::Open { from: a_id, rx_max: 250 }.encode(),
                Frame::Accept { to: a_id, from: b_id, rx_max: 1470 }.encode(),
                Frame::Probe { to: a_id }.encode(),
                Frame::Advert { data: &ADVERT }.encode(),
            ];
            p.a.scan(true);
            for _ in 0..300 {
                match rng.below(8) {
                    0 => p.later(ms(rng.below(1500) as u64)),
                    1 => {
                        let _ = p.a.send(p.t, ca, &value(1 + rng.below(MAX_VALUE), 1));
                        let _ = p.b.send(p.t, cb, &value(1 + rng.below(MAX_VALUE), 2));
                        p.shuttle_with(|_, _, _| !rng.one_in(8));
                    }
                    2 => {
                        let to = if rng.one_in(2) { &mut p.a } else { &mut p.b };
                        to.connect(p.t, device_of([2, 0, 0, 0, 9, rng.byte()]));
                    }
                    _ => {
                        let mut f = rng.pick(&seeds).clone();
                        if rng.one_in(2) {
                            mutate(&mut rng, &mut f, V2_FRAME + 4);
                        }
                        let from = *rng.pick(&[A, B, X, [0x03, 0, 0, 0, 0, 1], BROADCAST]);
                        let to = if rng.one_in(2) { &mut p.a } else { &mut p.b };
                        to.handle(p.t, from, &f);
                    }
                }
                for e in [&p.a, &p.b] {
                    assert!(e.conns.len() <= limits.connections);
                    assert!(e.seen_queued <= MAX_SEEN_QUEUED && e.values_queued <= MAX_VALUES_QUEUED);
                    assert!(e.conns.values().all(|c| c.partial.len() <= MAX_VALUE));
                    assert!(e.flight.len() <= MAX_FLIGHT);
                    assert!(e.events.len() <= MAX_SEEN_QUEUED + MAX_VALUES_QUEUED + 4 * limits.connections + 4);
                }
                if rng.one_in(20) {
                    events(&mut p.a);
                    events(&mut p.b);
                }
            }
        }
    }
}
