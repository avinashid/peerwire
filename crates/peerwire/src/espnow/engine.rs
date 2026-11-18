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

/// A data frame up to this many past the highest one taken is taken, and so is one up to this many behind it that
/// wasn't taken yet (a straggler, or a real frame a frame forged ahead jumped past), once: so a forged frame in the
/// window costs a connection the real frame of its number, not the window's (NB135-6), as DTLS's and IPsec's
/// anti-replay windows do.
pub const WINDOW: u32 = 64;
/// The frames behind the highest one taken, one bit each.
const _: () = assert!(WINDOW == u64::BITS);
/// Data frames in a row further than [`WINDOW`] ahead, or further than it behind, after which the receiver takes the
/// next as its new place, with none of the window behind it taken (NB135-7): its sender lost more than a window of
/// frames, or frames forged far ahead moved this end on (NB135-1), and its next frames come from where it really is.
/// One far off is dropped.
pub const RESYNC_AFTER: u32 = 4;
/// Events queued for the node at most: adverts, and values. Connections coming and going are never dropped.
const MAX_SEEN_QUEUED: usize = 16;
/// Values at most, and their bytes, so a node that doesn't poll holds 16 KB of them at most (NB135-2).
const MAX_VALUES_QUEUED: usize = 128;
const MAX_VALUE_BYTES: usize = 16 * 1024;
/// Refusals (a CLOSE for an accept no open waits for) go at most this often, so a flood of frames from made-up
/// addresses gets few answers.
const REFUSE_GAP: Duration = Duration::from_millis(100);
/// A CLOSE is taken only if nothing comes on the connection for this long after the PROBE it sends: a peer that still
/// has the connection answers at once. So a forged CLOSE, which anyone in range can send, closes nothing.
pub const CLOSE_CHECK: Duration = Duration::from_millis(500);
/// A second ACCEPT with another id this soon after the first closes the connection (NB135-4): one of the two was
/// forged, and which can't be told. The node's own time for a connection to prove the room.
pub const ACCEPT_RACE: Duration = Duration::from_secs(3);
/// Frames waiting for the ether at most; connection-less ones (a CLOSE for a connection gone, an advert) at most
/// `MAX_LOOSE` of them, and refusals one. A connection's own are capped by its values, and by one of each other kind
/// (NB135-3). The caps count the frames the ether holds too (NB135-15).
const MAX_TX: usize = 256;
const MAX_LOOSE: usize = 4;
/// A connection answers PROBEs at most this often, and only while no KEEPALIVE of its waits for the air (NB135-I5).
const PROBE_GAP: Duration = Duration::from_millis(100);
/// Frames handed to the ether whose verdict hasn't come, at most (NB135-15): what waits for the air beyond these waits
/// here, under the caps, where an ether that took all it could (32 on an ESP32) held frames the caps never saw, a
/// PROBE's answer every 100 ms among them, seconds of them ahead of every real frame. One whose verdict hasn't come in
/// `FLIGHT_TIMEOUT` is taken as lost, so an ether that drops a verdict can't stop the engine sending; the ether's
/// frames hold the air for 3.2 s at most before then (4 frames, 4 tries each, of up to 200 ms each where the ESP32
/// driver hears no verdict; a failed try took ~100 ms on NB-139's boards), so one still on the air isn't taken as lost.
/// 4, not 1 or 2: a board that polls every 10 ms then hands over up to 400 frames a second, past the ~310 the air
/// carries (NB-138).
const MAX_FLIGHT: usize = 4;
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
    /// The next data frame's number; the highest one taken, and which of the [`WINDOW`] behind it were taken (bit
    /// `i` for `rx_top - 1 - i`); and the number of the next part of the value being joined.
    tx_seq: u32,
    rx_top: u32,
    rx_taken: u64,
    rx_next: u32,
    /// Data frames in a row further than [`WINDOW`] ahead, and behind.
    ahead: u32,
    behind: u32,
    /// When the connection opener took the other end's ACCEPT: for [`ACCEPT_RACE`].
    accepted: Option<Instant>,
    /// A value whose first frame came and whose last hasn't, and how long its frames say it is.
    partial: Vec<u8>,
    assembling: bool,
    total: usize,
    /// When a frame for this end last came on it, and when one was last queued on it.
    heard: Instant,
    sent: Instant,
    /// Frames in a row the radio said it couldn't deliver, and whether it has acknowledged one: until it has, the
    /// address may be made up, and frames to it get fewer tries (NB135-3).
    failures: u32,
    acked: bool,
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
    /// A CLOSE for an ACCEPT no open waits for: from anyone, so one try, and one waiting at most (NB135-3).
    Refusal,
    Open,
    Accept,
    Advert,
    Keepalive,
    Data {
        end: bool,
    },
}

/// A frame waiting for the ether, and the tries it gets if the receiving radio doesn't acknowledge it.
struct Out {
    to: Mac,
    frame: Vec<u8>,
    conn: Option<ConnId>,
    kind: Kind,
    tries: u8,
}

/// A frame the ether took, waiting for its verdict: the tag the verdict names it by, its connection and kind, and
/// when it went.
struct Sent {
    tag: u32,
    conn: Option<ConnId>,
    kind: Kind,
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
    /// The tag of the next frame handed to the ether.
    next_tag: u32,
    events: VecDeque<RadioEvent>,
    seen_queued: usize,
    values_queued: usize,
    value_bytes: usize,
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
            next_tag: 0,
            events: VecDeque::new(),
            seen_queued: 0,
            values_queued: 0,
            value_bytes: 0,
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
    ///
    /// The limits don't refuse it: the node counts its own connects, and closes a connection that may be forged to
    /// make room for one (NB135-11), but the radio may hold one it hasn't heard of yet, and a connect failed for that
    /// held the advert a minute. Twice the limits, which the node never reaches, is the backstop.
    pub fn connect(&mut self, now: Instant, device: DeviceId) {
        let mac = mac_of(device);
        if group(mac) || device.0 >> 48 != 0 || self.conns.len() > 2 * self.settings.limits.connections {
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
        let tries = if c.acked { self.settings.tries } else { self.settings.first_tries };
        for (i, payload) in value.chunks(room).enumerate() {
            let last = i + 1 == n;
            let total = value.len() as u16;
            let f = Frame::Data { to: c.theirs, seq: c.tx_seq, first: i == 0, last, total, payload };
            c.tx_seq = c.tx_seq.wrapping_add(1);
            let (frame, kind) = (f.encode(), Kind::Data { end: last });
            self.tx.push_back(Out { to: c.mac, frame, conn: Some(conn), kind, tries });
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
                    let due = c.answered.is_none_or(|t| now.saturating_duration_since(t) >= PROBE_GAP);
                    if due && !self.waiting(conn, Kind::Keepalive) {
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
            if !self.waiting(conn, Kind::Accept) {
                let rx_max = self.max_frame as u16;
                self.queue(now, mac, Frame::Accept { to: theirs, from: mine, rx_max }, Some(conn), Kind::Accept);
            }
            return;
        }
        // Not advertising: silence, as from a Bluetooth peripheral that takes no connection. A refusal would be one
        // more frame to forge; the opener gives up after `connect_within`. At the limits, one more is taken and the
        // node decides, as a Bluetooth stack leaves it to the node: it closes one that makes way for it, such as one
        // an OPEN forged from a made-up address made, or closes this one (NB135-11). Silent there, the radio kept
        // every newcomer out of a node with two such beside two bound, and the newcomer's connect failed after 10 s and
        // held the advert a minute. Past that one, silence, until the node has decided.
        let limits = self.settings.limits;
        let centrals = self.conns.values().filter(|c| !c.central).count();
        if self.advert.is_none() || self.conns.len() > limits.connections || centrals > limits.centrals {
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
                self.queue(now, mac, Frame::Close { to: theirs }, None, Kind::Refusal);
            }
            return;
        };
        let (mtu, max_frame) = (self.settings.mtu, self.max_frame);
        let Some(c) = self.conns.get_mut(&conn) else { return };
        if c.open() {
            // The same accept again is the other end alive. Another, within `ACCEPT_RACE` of the first, means one of
            // the two was forged (it won the race, and this end sends to its id), and which can't be told, whatever
            // came on the connection meanwhile, as that can be forged too (NB135-4): the connection closes. The close
            // says nothing of the other end (`Dropped`), so the node connects again within seconds, though the room's
            // challenge has mostly come on it by then (NB135-5). Later, it is ignored.
            if c.theirs == theirs {
                self.live(now, mac, to);
            } else if c.accepted.is_some_and(|t| now.saturating_duration_since(t) < ACCEPT_RACE) {
                self.stats.accept_conflicts += 1;
                self.close(conn, true);
            } else {
                self.stats.unknown += 1;
            }
            return;
        }
        (c.theirs, c.state, c.frame) = (theirs, State::Open, usize::from(rx_max).min(max_frame));
        c.accepted = Some(now);
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
        // Where it falls against the highest number taken: that one again, or behind it within the window and taken
        // already (a copy: the sender's radio missed the acknowledgement); behind it within the window and not taken
        // yet, taken (NB135-6); in the window ahead, taken, the highest now; or further off, either way, where
        // `RESYNC_AFTER` in a row move this end there, so frames forged far ahead or far behind lose a connection
        // `RESYNC_AFTER` of its real frames, not all (NB135-1).
        let d = seq.wrapping_sub(c.rx_top);
        let (back, behind) = (d.wrapping_neg(), d > 1 << 31);
        let bit = if behind && back <= WINDOW { 1u64 << (back - 1) } else { 0 };
        if d == 0 || c.rx_taken & bit != 0 {
            self.stats.old += 1;
            return;
        }
        if bit != 0 {
            c.rx_taken |= bit;
        } else if behind || d > WINDOW {
            (c.ahead, c.behind) = if behind { (0, c.behind + 1) } else { (c.ahead + 1, 0) };
            if c.ahead.max(c.behind) < RESYNC_AFTER {
                match behind {
                    true => self.stats.old += 1,
                    false => self.stats.ahead += 1,
                }
                return;
            }
            self.stats.resyncs += 1;
            self.stats.gaps += 1;
            // Where the sender is now, with none of the window behind it taken, either way (NB135-7). Counting it all as
            // taken dropped as copies the real frames that land in it, up to a window of them, and copies don't count
            // towards the resync back: after 4 frames forged just past the window ahead, or 4 forged far ahead and then
            // 4 just ahead of the sender's real place, which moved this end back there. Ahead, nothing behind the new
            // place was taken, as the jump is more than a window. A copy of a frame taken before an earlier jump can
            // be taken again, and the session's anti-replay window drops it.
            (c.rx_top, c.rx_taken) = (seq, 0);
        } else {
            if d > 1 {
                self.stats.gaps += 1;
            }
            c.rx_taken = c.rx_taken.checked_shl(d).unwrap_or(0) | 1 << (d - 1);
            c.rx_top = seq;
        }
        (c.ahead, c.behind) = (0, 0);
        // A value's first part starts it; any other must be the next number after its last part taken, else it is
        // part of a value whose start or a part was lost, or forged, and the value being joined, if any, waits on.
        if first {
            c.partial.clear();
            (c.assembling, c.total) = (true, total);
        } else if !c.assembling || seq != c.rx_next {
            self.stats.orphans += 1;
            return;
        }
        c.rx_next = seq.wrapping_add(1);
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
        if self.values_queued >= MAX_VALUES_QUEUED || self.value_bytes + value.len() > MAX_VALUE_BYTES {
            self.stats.events_dropped += 1;
            return;
        }
        self.values_queued += 1;
        self.value_bytes += value.len();
        self.stats.values_in += 1;
        self.events.push_back(RadioEvent::Value { conn, value });
    }

    /// The ether's verdict on the frame it took as `tag` ([`peek`](Self::peek)): delivered (acknowledged, or a
    /// broadcast that went) or not. It is matched by its tag, so a verdict that comes after its frame was taken as lost
    /// (`FLIGHT_TIMEOUT`) is dropped, not taken for the next frame's, and each after it for the one after (NB135-15);
    /// frames handed over before it whose verdicts never came are taken as lost.
    pub fn outcome(&mut self, tag: u32, ok: bool) {
        let Some(i) = self.flight.iter().position(|s| s.tag == tag) else {
            self.stats.stray_verdicts += 1;
            return;
        };
        for _ in 0..i {
            self.stats.lost_verdicts += 1;
            self.land(false);
        }
        self.land(ok);
    }

    /// The verdict on the oldest frame with the ether.
    fn land(&mut self, ok: bool) {
        let Some(s) = self.flight.pop_front() else { return };
        if !ok {
            self.stats.tx_failed += 1;
        }
        let max = self.settings.max_failures;
        let Some(conn) = s.conn else { return };
        let Some(c) = self.conns.get_mut(&conn) else { return };
        if s.kind == (Kind::Data { end: true }) {
            c.values = c.values.saturating_sub(1);
        }
        if ok {
            (c.failures, c.acked) = (0, true);
            return;
        }
        c.failures += 1;
        if c.failures >= max && c.open() {
            self.stats.closed_failing += 1;
            self.close(conn, false);
        }
    }

    /// Adverts, opens and keepalives that are due, and connections given up or gone quiet.
    pub fn timers(&mut self, now: Instant) {
        // A verdict that never came: the frame is taken as lost.
        while self.flight.front().is_some_and(|s| now.saturating_duration_since(s.at) >= FLIGHT_TIMEOUT) {
            self.stats.lost_verdicts += 1;
            self.land(false);
        }
        if let Some(data) = self.advert.clone().filter(|_| now >= self.next_advert) {
            if !self.held().any(|(_, kind)| kind == Kind::Advert) {
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
            self.close(conn, false);
        }
        for conn in closed {
            self.stats.closed_by_peer += 1;
            self.close(conn, false);
        }
        for conn in quiet {
            self.stats.closed_quiet += 1;
            self.close(conn, false);
        }
        for conn in opens {
            if !self.waiting(conn, Kind::Open) {
                self.queue_open(now, conn);
            }
        }
        for (conn, mac, to) in keep {
            if !self.waiting(conn, Kind::Keepalive) {
                self.queue(now, mac, Frame::Keepalive { to }, Some(conn), Kind::Keepalive);
            }
        }
    }

    /// The next frame for the ether, the tries it gets, and the tag its verdict names it by
    /// ([`outcome`](Self::outcome)), if the ether holds fewer than `MAX_FLIGHT` whose verdicts haven't come.
    /// [`commit`](Self::commit) once the ether took it; leave it if not, and it is offered again.
    pub fn peek(&self) -> Option<(Mac, &[u8], u8, u32)> {
        if self.flight.len() >= MAX_FLIGHT {
            return None;
        }
        self.tx.front().map(|o| (o.to, &o.frame[..], o.tries, self.next_tag))
    }

    /// The ether took the frame [`peek`](Self::peek) gave.
    pub fn commit(&mut self, now: Instant) {
        let Some(o) = self.tx.pop_front() else { return };
        self.stats.frames_out += 1;
        self.flight.push_back(Sent { tag: self.next_tag, conn: o.conn, kind: o.kind, at: now });
        self.next_tag = self.next_tag.wrapping_add(1);
    }

    pub fn poll_event(&mut self) -> Option<RadioEvent> {
        let e = self.events.pop_front()?;
        match e {
            RadioEvent::Seen { .. } => self.seen_queued -= 1,
            RadioEvent::Value { ref value, .. } => {
                self.values_queued -= 1;
                self.value_bytes -= value.len();
            }
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
            rx_top: u32::MAX,
            rx_taken: u64::MAX,
            rx_next: 0,
            ahead: 0,
            behind: 0,
            accepted: None,
            partial: Vec::new(),
            assembling: false,
            total: 0,
            heard: now,
            sent: now,
            failures: 0,
            acked: false,
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
    ///
    /// Within the caps (NB135-3): one refusal at most, `MAX_LOOSE` frames of no connection, waiting here or with the
    /// ether (NB135-15), and `MAX_TX` waiting here; what is over goes. A broadcast and a refusal get one try, a frame of
    /// a connection that has had one acknowledged [`Settings::tries`], and any other, to an address that may be made
    /// up, [`Settings::first_tries`].
    fn queue(&mut self, now: Instant, to: Mac, f: Frame, conn: Option<ConnId>, kind: Kind) {
        let over = match kind {
            Kind::Refusal => self.held().any(|(_, k)| k == Kind::Refusal),
            _ if conn.is_none() => self.held().filter(|(c, _)| c.is_none()).count() >= MAX_LOOSE,
            _ => false,
        };
        if over || self.tx.len() >= MAX_TX {
            self.stats.tx_dropped += 1;
            return;
        }
        let mut tries = self.settings.first_tries;
        if let Some(c) = conn.and_then(|conn| self.conns.get_mut(&conn)) {
            c.sent = now;
            if c.acked {
                tries = self.settings.tries;
            }
        }
        if matches!(kind, Kind::Refusal | Kind::Advert) {
            tries = 1;
        }
        self.tx.push_back(Out { to, frame: f.encode(), conn, kind, tries });
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

    /// The connection and kind of each frame not on the air yet: waiting here, or with the ether, whose verdict hasn't
    /// come. The caps count both, so an ether that holds frames doesn't take them out of the caps (NB135-15).
    fn held(&self) -> impl Iterator<Item = (Option<ConnId>, Kind)> + '_ {
        self.tx.iter().map(|o| (o.conn, o.kind)).chain(self.flight.iter().map(|s| (s.conn, s.kind)))
    }

    /// Whether a frame of `kind` of `conn`'s waits for the air, here or with the ether: one of each kind but data at
    /// most (`MAX_TX`).
    fn waiting(&self, conn: ConnId, kind: Kind) -> bool {
        self.held().any(|(c, k)| c == Some(conn) && k == kind)
    }

    /// Drops the frames `conn` has waiting for the ether.
    fn purge(&mut self, conn: ConnId) {
        self.tx.retain(|o| o.conn != Some(conn));
    }

    /// `conn` went, as the node hears: [`RadioEvent::Disconnected`], [`RadioEvent::Dropped`] if this end `dropped` it
    /// for a cause that says nothing of the other end, or [`RadioEvent::ConnectFailed`] for an open that wasn't
    /// accepted.
    fn close(&mut self, conn: ConnId, dropped: bool) {
        let Some(c) = self.conns.remove(&conn) else { return };
        self.purge(conn);
        self.events.push_back(match (c.open(), c.device) {
            (false, Some(device)) => RadioEvent::ConnectFailed { device },
            _ if dropped => RadioEvent::Dropped { conn },
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
                    while let Some((dest, f, _, tag)) = tx.peek() {
                        let f = f.to_vec();
                        tx.commit(self.t);
                        tx.outcome(tag, true);
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

    /// Hands the ether `e`'s next frame at `t`, and its verdict, `ok`: where it went, and the frame.
    fn hand(e: &mut Engine, t: Instant, ok: bool) -> Option<(Mac, Vec<u8>)> {
        let (to, f, _, tag) = e.peek()?;
        let sent = (to, f.to_vec());
        e.commit(t);
        e.outcome(tag, ok);
        Some(sent)
    }

    /// The connections that went, by the events (adverts heard and the rest left out).
    fn gone(e: &mut Engine) -> Vec<RadioEvent> {
        events(e)
            .into_iter()
            .filter(|e| {
                matches!(
                    e,
                    RadioEvent::Disconnected { .. } | RadioEvent::Dropped { .. } | RadioEvent::ConnectFailed { .. }
                )
            })
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

    /// NB135-6 (the security review's finding, inverted): a data frame forged at the window's edge no longer makes the
    /// receiver drop the next 63 real frames as behind it; it costs the real frame of its number.
    #[test]
    fn forged_data_costs_the_real_frame_of_its_number_and_never_stalls_the_connection() {
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
        // At the window's edge: taken (the node drops it, unproven). The real frames behind it are taken once each,
        // and the one with its number is dropped as a copy.
        p.b.handle(p.t, A, &data(WINDOW));
        assert_eq!(values(&mut p.b), [b"forged".to_vec()]);
        let mut got = Vec::new();
        for i in 1..(WINDOW + 8) {
            assert!(p.a.send(p.t, ca, format!("{i}").as_bytes()));
            p.shuttle();
            got.extend(values(&mut p.b));
        }
        let lost: Vec<u32> = (1..WINDOW + 8).filter(|i| !got.contains(&format!("{i}").into_bytes())).collect();
        assert_eq!(lost, [WINDOW], "the real frame the forged one took the number of, and no other");
        assert_eq!(p.b.stats().old, 1);
        // A forged part of a value, while a real one is being joined, takes its number, and the real one goes on.
        assert!(p.a.send(p.t, ca, &value(MAX_VALUE, 8)));
        let mut held = Vec::new();
        p.shuttle_with(|_, _, f| {
            held.push(f.to_vec());
            false
        });
        assert_eq!(held.len(), 3);
        p.b.handle(p.t, A, &held[0]);
        let part = Frame::Data { to: b_id, seq: 2 * WINDOW, first: false, last: false, total: 512, payload: &[0; 9] };
        p.b.handle(p.t, A, &part.encode());
        p.b.handle(p.t, A, &held[1]);
        p.b.handle(p.t, A, &held[2]);
        assert_eq!(values(&mut p.b), [value(MAX_VALUE, 8)]);
        // Frames that come out of order within the window are taken once each.
        let mut q = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, _) = q.link();
        let mut held = Vec::new();
        for i in 0..5u8 {
            assert!(q.a.send(q.t, ca, &[i]));
            q.shuttle_with(|_, _, f| {
                held.push(f.to_vec());
                false
            });
        }
        for i in [4, 0, 2, 2, 1, 4, 3, 0] {
            q.b.handle(q.t, A, &held[i]);
        }
        assert_eq!(values(&mut q.b), [[4], [0], [2], [1], [3]].map(|v| v.to_vec()));
        assert_eq!(q.b.stats().old, 3);
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
    fn a_forged_accept_that_wins_the_race_closes_the_connection_when_the_real_one_comes() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        p.b.advertise(p.t, Some(&ADVERT));
        p.a.connect(p.t, device_of(B));
        // The OPEN reaches B, whose ACCEPT is held back; someone who overheard the OPEN answers first, as B.
        p.shuttle_with(|from, _, _| from == A);
        let Some(RadioEvent::Connected { conn: cb, .. }) = p.b.poll_event() else { panic!("b accepted") };
        let Ok(Frame::Open { from: a_id, .. }) = frame::parse(&p.wire[0].2) else { panic!() };
        p.a.handle(p.t, B, &Frame::Accept { to: a_id, from: 0x1234, rx_max: 250 }.encode());
        let Some(RadioEvent::Connected { conn: ca, .. }) = p.a.poll_event() else { panic!("a took the forged one") };
        // The real accept: within `ACCEPT_RACE`, so the connection closes at once, dropped (it says nothing of B), and
        // the node connects again.
        let real = p.wire.iter().find(|(from, _, _)| *from == B).map(|w| w.2.clone()).unwrap();
        p.a.handle(p.t, B, &real);
        assert_eq!(gone(&mut p.a), [RadioEvent::Dropped { conn: ca }]);
        assert_eq!(p.a.stats().accept_conflicts, 1);
        // b's end hears nothing from a's: gone after its timeout.
        for _ in 0..5 {
            p.later(ms(1_000));
            p.shuttle();
        }
        assert_eq!(gone(&mut p.b), [RadioEvent::Disconnected { conn: cb }]);
        // If the real accept never comes (jammed), a sends to the forged id: b hears nothing valid and closes after
        // its timeout, and a, which heard b's keepalives till then, a timeout after the last.
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        p.b.advertise(p.t, Some(&ADVERT));
        p.a.connect(p.t, device_of(B));
        p.shuttle_with(|from, _, _| from == A);
        let Some(RadioEvent::Connected { conn: cb, .. }) = p.b.poll_event() else { panic!() };
        let Ok(Frame::Open { from: a_id, .. }) = frame::parse(&p.wire[0].2) else { panic!() };
        p.a.handle(p.t, B, &Frame::Accept { to: a_id, from: 0x1234, rx_max: 250 }.encode());
        let Some(RadioEvent::Connected { conn: ca, .. }) = p.a.poll_event() else { panic!() };
        let (mut a_gone, mut b_gone) = (None, None);
        for s in 1..=12 {
            p.later(ms(1_000));
            p.shuttle_with(|from, _, f| from == A || !matches!(frame::parse(f), Ok(Frame::Accept { .. })));
            for (e, at, conn) in [(&mut p.a, &mut a_gone, ca), (&mut p.b, &mut b_gone, cb)] {
                if gone(e).contains(&RadioEvent::Disconnected { conn }) {
                    *at = Some(s);
                }
            }
        }
        assert_eq!((b_gone, a_gone), (Some(5), Some(9)));
        // A second accept with another id later than `ACCEPT_RACE` after the first is ignored.
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, cb) = p.link();
        let (a_id, _) = ids(&p);
        assert!(p.b.send(p.t, cb, b"data"));
        p.shuttle();
        p.later(ACCEPT_RACE);
        p.shuttle();
        p.a.handle(p.t, B, &Frame::Accept { to: a_id, from: 0x1234, rx_max: 250 }.encode());
        assert!(gone(&mut p.a).is_empty());
        assert!(p.a.send(p.t, ca, b"still up"));
        p.shuttle();
        assert_eq!(values(&mut p.b), [b"still up".to_vec()]);
    }

    /// NB135-1 (the security review's repro, inverted): four forged data frames far ahead move the receiver's place
    /// 2^30 frames on, and the real frames, far behind it now, move it back after four: four values lost, not all.
    /// The same the other way.
    /// NB135-4 (the security review's repro, inverted): a forged ACCEPT and then a forged DATA frame, both before the
    /// real ACCEPT, no longer make the real one look like the forgery: a second ACCEPT with another id within
    /// `ACCEPT_RACE` of the first closes the connection, whatever came on it, and the node connects again.
    #[test]
    fn sec_a_forged_accept_and_data_lose_the_race_to_the_real_accept() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        p.b.advertise(p.t, Some(&ADVERT));
        p.a.connect(p.t, device_of(B));
        p.shuttle_with(|from, _, _| from == A);
        let Some(RadioEvent::Connected { .. }) = p.b.poll_event() else { panic!("b accepted") };
        let Ok(Frame::Open { from: a_id, .. }) = frame::parse(&p.wire[0].2) else { panic!() };
        p.a.handle(p.t, B, &Frame::Accept { to: a_id, from: 0x1234, rx_max: 250 }.encode());
        p.a.handle(
            p.t,
            B,
            &Frame::Data { to: a_id, seq: 0, first: true, last: true, total: 1, payload: &[0] }.encode(),
        );
        let Some(RadioEvent::Connected { conn: ca, .. }) = p.a.poll_event() else { panic!("a took the forged one") };
        let real = p.wire.iter().find(|(from, _, _)| *from == B).map(|w| w.2.clone()).unwrap();
        p.later(ACCEPT_RACE - ms(1));
        p.a.handle(p.t, B, &real);
        assert_eq!(gone(&mut p.a), [RadioEvent::Dropped { conn: ca }], "the real accept closes it");
        assert_eq!(p.a.stats().accept_conflicts, 1);
        let (ca, _) = p.link();
        assert!(p.a.send(p.t, ca, b"on the next connection"));
        p.shuttle();
        assert_eq!(values(&mut p.b), [b"on the next connection".to_vec()]);
    }

    #[test]
    fn sec_four_forged_frames_far_ahead_or_behind_lose_four_real_frames_at_most() {
        for far in [1u32 << 30, (1u32 << 30).wrapping_neg()] {
            let mut p = Pair::new(V1_FRAME, V1_FRAME);
            let (ca, _) = p.link();
            let (_, b_id) = ids(&p);
            assert!(p.a.send(p.t, ca, b"before"));
            p.shuttle();
            assert_eq!(values(&mut p.b), [b"before".to_vec()]);
            for i in 0..RESYNC_AFTER {
                let f = Frame::Data { to: b_id, seq: far + i, first: true, last: true, total: 1, payload: &[0] };
                p.b.handle(p.t, A, &f.encode());
            }
            assert_eq!(p.b.stats().resyncs, 1);
            let _ = values(&mut p.b);
            let mut got = Vec::new();
            for _ in 0..60 {
                let _ = p.a.send(p.t, ca, b"real");
                p.later(ms(1_000));
                p.shuttle();
                got.push(values(&mut p.b).len());
            }
            let first = got.iter().position(|n| *n > 0);
            assert_eq!(first, Some(RESYNC_AFTER as usize - 1), "the fourth real frame moves b back: {got:?}");
            assert_eq!(got.iter().sum::<usize>(), 60 - (RESYNC_AFTER as usize - 1), "and every one after comes");
            assert_eq!(p.b.stats().resyncs, 2);
            assert!(gone(&mut p.b).is_empty() && gone(&mut p.a).is_empty(), "nothing closes");
        }
    }

    /// The numbers of the real values `a` sends after "0" that `b` doesn't take, when `RESYNC_AFTER` data frames
    /// forged with `b`'s id at each number of `forged`, in turn, come first: `a` then sends 1 to `n`, one at a time.
    fn real_lost_after(forged: &[u32], n: u32) -> Vec<u32> {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, _) = p.link();
        let (_, b_id) = ids(&p);
        assert!(p.a.send(p.t, ca, b"0"));
        p.shuttle();
        assert_eq!(values(&mut p.b), [b"0".to_vec()]);
        for &seq in forged {
            let f = Frame::Data { to: b_id, seq, first: true, last: true, total: 6, payload: b"forged" };
            for _ in 0..RESYNC_AFTER {
                p.b.handle(p.t, A, &f.encode());
            }
        }
        let _ = values(&mut p.b);
        let mut got = Vec::new();
        for i in 1..=n {
            assert!(p.a.send(p.t, ca, format!("{i}").as_bytes()));
            p.shuttle();
            got.extend(values(&mut p.b));
        }
        (1..=n).filter(|i| !got.contains(&format!("{i}").into_bytes())).collect()
    }

    /// NB135-7 (the security review's repro, inverted): 4 data frames forged just past the window ahead move the
    /// receiver there with none of the window behind it taken, so the sender's next real frames, which land in it, are
    /// taken, and only the one with the forged number is dropped, as a copy: not the 64 of the window (65 of the next
    /// 72 were lost), which on an idle link timed the link out. The same when the receiver is moved back: 4 forged far
    /// ahead, then 4 just ahead of the sender's real place. Wherever 4 forged frames move it, `RESYNC_AFTER` real
    /// frames are lost at most: those far off before the window, and the one with the forged number.
    #[test]
    fn sec_four_frames_just_past_the_window_cost_the_real_frame_of_their_number() {
        assert_eq!(real_lost_after(&[WINDOW + 1], WINDOW + 8), [WINDOW + 1]);
        assert_eq!(real_lost_after(&[1 << 30, WINDOW], WINDOW + 8), [WINDOW]);
        assert_eq!(real_lost_after(&[WINDOW + RESYNC_AFTER], WINDOW + 8), [1, 2, 3, WINDOW + RESYNC_AFTER]);
        for k in WINDOW + 1..=2 * WINDOW + 8 {
            let lost = real_lost_after(&[k], k + 8);
            assert!(lost.len() <= RESYNC_AFTER as usize, "4 forged at {k}: lost {lost:?}");
        }
        for k in 1..=2 * WINDOW + 8 {
            let lost = real_lost_after(&[1 << 30, k], k + 8);
            assert!(lost.len() <= RESYNC_AFTER as usize, "4 forged far ahead, then 4 at {k}: lost {lost:?}");
        }
    }

    /// A residual, documented (espnow.md's forged frames; the threat model's NB135-1): a forger that keeps sending
    /// keeps a receiver out of step. After 4 frames forged far ahead move `b` there, one more forged in `b`'s new
    /// window after every 3 real ones resets the count of real frames far behind, so none of `a`'s is taken while it
    /// goes on; the node's link times out and comes back on a new connection within seconds (NB135-1, NB135-I6). Once
    /// it stops, the fourth real frame moves `b` back. The security review's third round; if this fails, the residual
    /// changed, and the docs should say so.
    #[test]
    fn sec_interleaved_forged_frames_keep_a_receiver_at_the_forged_place() {
        for interleave in [false, true] {
            let mut p = Pair::new(V1_FRAME, V1_FRAME);
            let (ca, _) = p.link();
            let (_, b_id) = ids(&p);
            let data = |seq: u32| {
                Frame::Data { to: b_id, seq, first: true, last: true, total: 6, payload: b"forged" }.encode()
            };
            let far = 1u32 << 30;
            for i in 0..RESYNC_AFTER {
                p.b.handle(p.t, A, &data(far + i));
            }
            let _ = values(&mut p.b);
            let (mut next, mut real) = (far + RESYNC_AFTER, 0);
            for i in 0..300u32 {
                assert!(p.a.send(p.t, ca, b"real"));
                p.shuttle();
                real += values(&mut p.b).iter().filter(|v| v.as_slice() == b"real").count();
                if interleave && i % 3 == 2 {
                    p.b.handle(p.t, A, &data(next));
                    next += 1;
                }
            }
            match interleave {
                true => assert_eq!(real, 0, "no real frame taken while the forger goes on"),
                false => assert_eq!(real, 300 - (RESYNC_AFTER as usize - 1), "3 lost, then all"),
            }
        }
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
            while hand(&mut b, t, true).is_some() {
                answers += 1;
            }
        }
        // The limits, and one more for the node to decide on (NB135-11); this node never decides.
        let limits = Settings::DEFAULT.limits;
        let taken = limits.connections + 1;
        assert_eq!(b.stats().connections, taken);
        assert_eq!(events(&mut b).len(), taken, "one Connected each, no more");
        assert_eq!(b.stats().opens_refused, 500 - taken as u64);
        assert_eq!(answers, taken, "an accept each, and silence for the rest");
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
            while hand(&mut p.a, p.t, false).is_some() {
                opens += 1;
            }
            p.later(ms(500));
        }
        assert_eq!(opens, 20);
        assert_eq!(events(&mut p.a), [RadioEvent::ConnectFailed { device: device_of(B) }]);
        // No such device, or past twice the limits: the limits themselves are the node's to keep (NB135-11).
        let mut e = engine(9, V1_FRAME, p.t);
        for i in 0..2 * Settings::DEFAULT.limits.connections + 2 {
            e.connect(p.t, device_of([2, 0, 0, 0, 1, i as u8]));
        }
        e.connect(p.t, DeviceId(1 << 60));
        e.connect(p.t, device_of(BROADCAST));
        let failed = events(&mut e).iter().filter(|e| matches!(e, RadioEvent::ConnectFailed { .. })).count();
        assert_eq!(failed, 3);
    }

    /// NB135-11 (the security review's finding): a node with two connections bound to peers and two that OPENs forged
    /// from made-up addresses made held the radio's 4, and the radio stayed silent to any other OPEN before the node
    /// could close a forged one for it: a newcomer's connect failed after 10 s, and its node held the advert a minute.
    /// One OPEN over the limits now reaches the node, which decides; past that one, silence until it has. And a
    /// connect of the node's own goes whatever the radio holds, as the node counts its own and makes room for it.
    #[test]
    fn sec_an_open_over_the_limits_reaches_the_node_and_a_connect_of_its_own_goes() {
        let t = Instant::now();
        let limits = Settings::DEFAULT.limits;
        let mut b = engine(2, V1_FRAME, t);
        b.advertise(t, Some(&ADVERT));
        // What went on the air.
        let sent = |b: &mut Engine| {
            let mut out = Vec::new();
            while let Some((_, f)) = hand(b, t, true) {
                out.push(f);
            }
            out
        };
        // An OPEN from a made-up address: the connection the node is told of, if it is answered.
        let open = |b: &mut Engine, i: u8| {
            b.handle(t, [0x02, 0, 0, 0, 7, i], &Frame::Open { from: u64::from(i) + 1, rx_max: 250 }.encode());
            let accepted = sent(b).iter().any(|f| matches!(frame::parse(f), Ok(Frame::Accept { .. })));
            let connected: Vec<ConnId> = events(b)
                .into_iter()
                .filter_map(|e| match e {
                    RadioEvent::Connected { conn, device: None, .. } => Some(conn),
                    _ => None,
                })
                .collect();
            connected.last().copied().filter(|_| accepted)
        };
        let full: Vec<ConnId> = (0..limits.connections as u8).filter_map(|i| open(&mut b, i)).collect();
        assert_eq!(full.len(), limits.connections, "the limits");
        let over = open(&mut b, 100).expect("one over them, for the node to decide on");
        assert_eq!(open(&mut b, 101), None, "and silence past it, until the node has");
        assert_eq!(b.stats().opens_refused, 1);
        // The node closes one that makes way (or the newcomer): one more again.
        b.disconnect(t, full[0]);
        assert!(open(&mut b, 102).is_some(), "one over the limits again");
        b.disconnect(t, over);
        // A connect of the node's own at the limits goes: an OPEN, and no failure.
        assert_eq!(b.stats().connections, limits.connections);
        b.connect(t, device_of(A));
        let opened = sent(&mut b).iter().any(|f| matches!(frame::parse(f), Ok(Frame::Open { .. })));
        assert!(opened, "the connect went");
        assert!(gone(&mut b).is_empty(), "with no ConnectFailed");
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
        while hand(&mut p.a, p.t, false).is_some() {
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
        let mut closes = 0;
        for i in 0..50u64 {
            a.handle(t + ms(i * 10), B, &Frame::Accept { to: 7 + i, from: 9, rx_max: 250 }.encode());
            while let Some((to, f, tries, tag)) = a.peek() {
                assert_eq!((to, frame::parse(f), tries), (B, Ok(Frame::Close { to: 9 }), 1), "one try");
                a.commit(t);
                a.outcome(tag, false);
                closes += 1;
            }
        }
        assert_eq!(closes, 5, "one per 100 ms over 500 ms");
        assert_eq!(a.stats().unknown, 50);
    }

    /// NB135-3 (the security review's repro, inverted): refusals of forged ACCEPTs no longer wait in the engine's
    /// queue with no cap while the ether takes nothing (its one slot busy with frames no radio acknowledges): one
    /// waits at most, and the rest go.
    #[test]
    fn sec_refusals_dont_pile_up_while_the_ether_is_busy() {
        let t0 = Instant::now();
        let mut b = engine(2, V1_FRAME, t0);
        let mut t = t0;
        for i in 0..600u64 {
            t += ms(100);
            let mac = [0x02, 0, 0, (i >> 16) as u8, (i >> 8) as u8, i as u8];
            b.handle(t, mac, &Frame::Accept { to: 0x1000 + i, from: 0x2000 + i, rx_max: 250 }.encode());
            b.timers(t);
        }
        assert_eq!(b.tx.len(), 1, "one refusal waits");
        assert_eq!(b.stats().tx_dropped, 599);
        // CLOSEs of connections the node closed, with the ether still busy: a few wait, the rest go.
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        p.b.advertise(p.t, Some(&ADVERT));
        for i in 0..20u64 {
            p.b.handle(p.t, [0x02, 0, 0, 0, 7, i as u8], &Frame::Open { from: 0x100 + i, rx_max: 250 }.encode());
            for e in events(&mut p.b) {
                if let RadioEvent::Connected { conn, .. } = e {
                    p.b.disconnect(p.t, conn);
                }
            }
        }
        assert!(p.b.tx.iter().filter(|o| o.conn.is_none()).count() <= MAX_LOOSE);
        assert!(p.b.tx.len() <= MAX_TX);
    }

    /// NB135-3: a frame to an address that has never acknowledged one gets `first_tries`, one of a connection that has
    /// had one acknowledged `tries`, and a broadcast one.
    #[test]
    fn frames_to_an_address_that_never_acknowledged_get_fewer_tries() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let s = Settings::DEFAULT;
        p.b.advertise(p.t, Some(&ADVERT));
        p.later(ms(0));
        assert_eq!(p.b.peek().map(|(to, _, tries, _)| (to, tries)), Some((BROADCAST, 1)), "an advert");
        hand(&mut p.b, p.t, true);
        // A replayed advert from an address that isn't there: OPENs with two tries each.
        p.a.connect(p.t, device_of(X));
        assert_eq!(p.a.peek().map(|(to, _, tries, _)| (to, tries)), Some((X, s.first_tries)));
        hand(&mut p.a, p.t, false);
        // A real connection: its frames get two tries until one is acknowledged, then four.
        let (ca, _) = p.link();
        assert!(p.a.send(p.t, ca, b"one"));
        assert_eq!(p.a.peek().map(|(_, _, tries, _)| tries), Some(s.tries), "the ACCEPT's been acknowledged");
        // An ACCEPT to an OPEN's address, which may be made up: two tries.
        let mut b = engine(3, V1_FRAME, p.t);
        b.advertise(p.t, Some(&ADVERT));
        b.handle(p.t, X, &Frame::Open { from: 5, rx_max: 250 }.encode());
        let accept = b
            .peek()
            .map(|(to, f, tries, _)| (to, frame::parse(f).is_ok_and(|f| matches!(f, Frame::Accept { .. })), tries));
        assert_eq!(accept, Some((X, true, s.first_tries)));
    }

    /// NB135-I5 (the security review's repro, inverted): PROBEs on a connection opened from an address that never
    /// acknowledges, one per 100 ms for 3 s while the ether takes nothing, leave one KEEPALIVE waiting, not 30; so
    /// does a connection's own keepalive timer.
    #[test]
    fn sec_probe_answers_wait_one_at_a_time() {
        let t0 = Instant::now();
        let mut b = engine(2, V1_FRAME, t0);
        b.advertise(t0, Some(&ADVERT));
        let x = [0x02, 0, 0, 0, 7, 1];
        b.handle(t0, x, &Frame::Open { from: 0x100, rx_max: 250 }.encode());
        let mine = b.conns.values().next().unwrap().mine;
        let mut t = t0;
        for _ in 0..30 {
            t += ms(100);
            b.handle(t, x, &Frame::Probe { to: mine }.encode());
            b.timers(t);
        }
        let k: Vec<u8> = b.tx.iter().filter(|o| o.kind == Kind::Keepalive).map(|o| o.tries).collect();
        assert_eq!(k, [Settings::DEFAULT.first_tries], "one KEEPALIVE waits, with the tries of an address unproven");
        // Once the ether takes it, the next PROBE is answered.
        while let Some((_, f)) = hand(&mut b, t, false) {
            if matches!(frame::parse(&f), Ok(Frame::Keepalive { .. })) {
                break;
            }
        }
        t += ms(100);
        b.handle(t, x, &Frame::Probe { to: mine }.encode());
        assert_eq!(b.tx.iter().filter(|o| o.kind == Kind::Keepalive).count(), 1);
    }

    /// What `b` of `p` hands an ESP32's ether over `n` milliseconds from now, with `step` run each millisecond before
    /// the timers (the forger, and the node's doings): the ether is a queue of 32 that puts one frame on the air at a
    /// time, 100 ms for a try no radio acknowledges (a board's failed try, NB-139) and 1 ms for one to `a` or a
    /// broadcast, and a peer table of 16 that evicts no address with a frame waiting, as `EspNowEther` keeps them.
    /// `a`'s frames reach `b` at once. The most frames the ether's queue held, and the most addresses its peer table
    /// did; `a`'s events, and when each came.
    fn on_a_board(
        p: &mut Pair,
        n: u64,
        mut step: impl FnMut(&mut Pair, u64),
    ) -> (usize, usize, Vec<(Instant, RadioEvent)>) {
        use crate::espnow::ether::{PeerTable, TxQueue};
        let start = p.t;
        let mut q = TxQueue::new(32);
        let mut peers = PeerTable::new(16);
        let mut air: Option<(Mac, Vec<u8>, Instant)> = None;
        let (mut most, mut table, mut seen) = (0, 0, Vec::new());
        for i in 0..n {
            let now = start + ms(i);
            p.t = now;
            step(p, i);
            p.a.timers(now);
            p.b.timers(now);
            while let Some((to, f, tries, tag)) = p.b.peek() {
                peers.touch_with(to, |m| q.has(*m));
                if !q.push(to, f, tries, tag) {
                    break;
                }
                p.b.commit(now);
            }
            (most, table) = (most.max(q.len()), table.max(peers.len()));
            if air.is_none() {
                if let Some((to, f)) = q.next(now) {
                    let took = if to == A || to == BROADCAST { ms(1) } else { ms(100) };
                    air = Some((to, f.to_vec(), now + took));
                }
            }
            if let Some((to, f, end)) = air.take() {
                if now < end {
                    air = Some((to, f, end));
                } else {
                    if to == A {
                        p.a.handle(now, B, &f);
                    }
                    q.verdict(to == A || to == BROADCAST);
                }
            }
            while let Some((tag, ok)) = q.outcome() {
                p.b.outcome(tag, ok);
            }
            while let Some((dest, f, _, tag)) = p.a.peek() {
                let f = f.to_vec();
                p.a.commit(now);
                p.a.outcome(tag, true);
                if dest == B {
                    p.b.handle(now, A, &f);
                }
            }
            seen.extend(events(&mut p.a).into_iter().map(|e| (now, e)));
            events(&mut p.b);
        }
        (most, table, seen)
    }

    /// NB135-15 (the security review's repro, inverted): connections that OPENs forged from made-up addresses make,
    /// one at a time, a new one as the node closes the last at its 3 s deadline, whose PROBEs the forger sends every
    /// 100 ms; and ACCEPTs forged from a new made-up address every 50 ms, which draw refusals. The radio handed the
    /// ether all it would take, 32 on an ESP32, so the caps saw none of it: a KEEPALIVE answering a PROBE left at once,
    /// and the next PROBE was answered too. The ether's queue held 32, a value from `b` to `a` waited 4.37 s, and a
    /// verdict was taken as lost after 5 s; and frames waiting for more than 20 made-up addresses held ESP-NOW's peer
    /// table, so a frame to a real peer failed. The ether now holds 4 at most, which count against the caps: one PROBE
    /// answer waits at a time, the value comes within a second, no verdict is lost, and the peer table keeps to its 16.
    #[test]
    fn sec_forged_connections_and_refusals_keep_the_ether_to_a_few_frames() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, cb) = p.link();
        events(&mut p.a);
        events(&mut p.b);
        let mut forged: Option<(Mac, ConnId, Instant)> = None;
        let (mut k, mut probes, mut accepts, mut sent_at) = (0u8, 0u32, 0u32, None);
        let (most, table, seen) = on_a_board(&mut p, 20_000, |p, i| {
            let now = p.t;
            match forged {
                None => {
                    k = k.wrapping_add(1);
                    let m = [0x02, 0xf0, 0, 0, 0, k];
                    p.b.handle(now, m, &Frame::Open { from: 0x0f00 + u64::from(k), rx_max: 250 }.encode());
                    let conn = p.b.conns.iter().find(|(_, c)| c.mac == m).map(|(i, _)| *i).expect("taken");
                    assert!(p.b.send(now, conn, &[0x5a; 33])); // the node's challenge
                    forged = Some((m, conn, now));
                }
                Some((m, conn, opened)) => match p.b.conns.get(&conn).map(|c| c.mine) {
                    None => forged = None,
                    Some(_) if now.saturating_duration_since(opened) >= ms(3_000) => {
                        p.b.disconnect(now, conn); // the node's deadline
                        forged = None;
                    }
                    Some(id) if i % 100 == 0 => {
                        p.b.handle(now, m, &Frame::Probe { to: id }.encode());
                        probes += 1;
                    }
                    Some(_) => {}
                },
            }
            if i % 50 == 25 {
                accepts += 1;
                let m = [0x02, 0xe0, 0, 0, (accepts >> 8) as u8, accepts as u8];
                let id = 0x0e00 + u64::from(accepts);
                p.b.handle(now, m, &Frame::Accept { to: id, from: id, rx_max: 250 }.encode());
            }
            if i == 8_000 {
                assert!(p.b.send(now, cb, b"a real value"));
                sent_at = Some(now);
            }
        });
        let got_at = seen.iter().find_map(|(t, e)| match e {
            RadioEvent::Value { value, .. } if value == b"a real value" => Some(*t),
            _ => None,
        });
        let took = got_at.zip(sent_at).map(|(g, s)| g - s);
        let a_lost = seen.iter().any(|(_, e)| *e == RadioEvent::Disconnected { conn: ca });
        let s = p.b.stats();
        eprintln!(
            "forged connections {k}, PROBEs {probes}, forged ACCEPTs {accepts}: the ether's queue held {most} at most, \
             its peer table {table}; a value took {took:?}; a lost the connection {a_lost}; b: {s:?}"
        );
        assert!(most <= MAX_FLIGHT, "the ether held {most}");
        assert!(table <= 16, "the peer table held {table}");
        assert_eq!((s.lost_verdicts, s.stray_verdicts), (0, 0), "every verdict, each for its own frame");
        assert!(!a_lost, "a kept the connection");
        assert!(took.is_some_and(|t| t < ms(1_000)), "a value within a second: {took:?}");
    }

    /// NB135-15: a verdict is matched to its frame by its tag. One that comes after its frame was taken as lost
    /// (`FLIGHT_TIMEOUT`) was taken for the next frame's, and every verdict after it for the frame before its own, until
    /// none was in flight; it is now dropped. And one that skips a frame (an ether that lost that one's) takes the
    /// frames before it as lost, not itself.
    #[test]
    fn sec_a_late_verdict_is_never_taken_for_the_next_frame() {
        let mut p = Pair::new(V1_FRAME, V1_FRAME);
        let (ca, _) = p.link();
        // Four values of a frame each, handed to the ether a second apart.
        let tags: Vec<u32> = (0..4u8)
            .map(|i| {
                assert!(p.a.send(p.t, ca, &[i]));
                let (_, _, _, tag) = p.a.peek().expect("a frame");
                p.a.commit(p.t + ms(1_000 * u64::from(i)));
                tag
            })
            .collect();
        let in_flight = |p: &Pair| p.a.flight.iter().map(|s| s.tag).collect::<Vec<u32>>();
        let values = |p: &Pair| p.a.conns[&ca].values;
        assert_eq!((in_flight(&p), values(&p)), (tags.clone(), 4));
        // The first one's verdict doesn't come in time: taken as lost.
        let (a_id, _) = ids(&p);
        let late = p.t + FLIGHT_TIMEOUT;
        p.a.handle(late, B, &Frame::Keepalive { to: a_id }.encode()); // b is there
        p.a.timers(late);
        assert_eq!(in_flight(&p), tags[1..], "the first taken as lost");
        assert_eq!((p.a.stats().lost_verdicts, values(&p)), (1, 3));
        // It comes late: dropped, not taken for the second's.
        p.a.outcome(tags[0], true);
        assert_eq!(in_flight(&p), tags[1..], "the late verdict took no other frame's");
        assert_eq!((p.a.stats().stray_verdicts, values(&p)), (1, 3));
        // The second's, then the fourth's with the third's lost: each for its own.
        p.a.outcome(tags[1], true);
        assert_eq!((in_flight(&p), values(&p)), (tags[2..].to_vec(), 2));
        p.a.outcome(tags[3], true);
        assert!(in_flight(&p).is_empty());
        assert_eq!((p.a.stats().lost_verdicts, values(&p)), (2, 0));
        p.a.outcome(tags[2], true);
        assert_eq!(p.a.stats().stray_verdicts, 2, "the third's, late, dropped too");
    }

    /// NB135-2: values waiting for a node that doesn't poll are capped in bytes (16 KB), not only in number.
    #[test]
    fn values_waiting_for_a_node_that_doesnt_poll_are_capped_in_bytes() {
        let mut p = Pair::new(V2_FRAME, V2_FRAME);
        let (ca, _) = p.link();
        for i in 0..40u8 {
            assert!(p.a.send(p.t, ca, &value(MAX_VALUE, i)));
            p.shuttle();
        }
        assert_eq!(p.b.value_bytes, MAX_VALUE_BYTES, "32 values of 512 bytes");
        assert_eq!(p.b.stats().events_dropped, 8);
        assert_eq!(values(&mut p.b).len(), 32);
        assert_eq!(p.b.value_bytes, 0);
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
                    // One over the limits from OPENs, for the node to decide on; the node's own connects past them
                    // up to twice the limits (NB135-11).
                    let centrals = e.conns.values().filter(|c| !c.central).count();
                    assert!(centrals <= limits.centrals.min(limits.connections) + 1);
                    assert!(e.conns.len() <= 2 * limits.connections + 1);
                    assert!(e.seen_queued <= MAX_SEEN_QUEUED && e.values_queued <= MAX_VALUES_QUEUED);
                    assert!(e.value_bytes <= MAX_VALUE_BYTES);
                    assert!(e.conns.values().all(|c| c.partial.len() <= MAX_VALUE));
                    assert!(e.flight.len() <= MAX_FLIGHT);
                    assert!(e.events.len() <= MAX_SEEN_QUEUED + MAX_VALUES_QUEUED + 8 * limits.connections + 8);
                }
                if rng.one_in(20) {
                    events(&mut p.a);
                    events(&mut p.b);
                }
            }
        }
    }
}
