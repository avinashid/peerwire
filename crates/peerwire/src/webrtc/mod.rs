//! WebRTC to browsers (feature `webrtc`): a native node links with `joinRoom` pages as room.js links pages, over one
//! WebRTC DataChannel per page, and runs the same core packets over it, so a page sees an ordinary peer. See
//! `docs/concepts/native-webrtc.md`.
//!
//! [`Rtcs`] holds one [`str0m`] connection (ICE, DTLS, SCTP) per browser, sans-IO like the rest of the node: the host
//! hands it what its socket reads that is the browsers' (ICE checks, DTLS records: [`Rtcs::receive`] takes only
//! those), their signals, the core's datagrams for them and the time, and sends what comes out ([`Rtcs::poll_udp`],
//! [`Rtcs::poll_signal`]) and hands the core what the DataChannels carry ([`Rtcs::poll_datagram`]).
//!
//! The signals are webrtc.js's, sealed to the peer on its inbox topic like every signal: `offer` (with `p`, the
//! `pw-rtc-offer` room proof), `answer`, `ice`/`ice-batch`. Its DataChannel is webrtc.js's: negotiated, id 0,
//! unordered, no retransmits, label `peerwire`. The smaller id dials, as in room.js. An offer opens a connection only
//! if it proves the room key; an answer or candidates count only for a connection under way. Each side's DTLS
//! fingerprint is in its description, which only that side's sealed signal carries (Noise X: the sender's PeerId is
//! authenticated), and the core's handshake over the channel must then be with that same PeerId (the host hands its
//! datagrams to the endpoint bound to it): so the DataChannel is bound to the PeerId twice over.
//!
//! [`room`] is room.js's room layer on these links: the link proof, presence and goodbye.

pub(crate) mod room;
#[cfg(test)]
mod tests;

use peerwire_core::{PeerId, MAX_DATAGRAM};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, CandidateKind, Event as RtcEvent, IceConnectionState, Input, Output, Rtc};

/// WebRTC connections at once. An offer past this is refused, and a dial isn't made.
pub(crate) const MAX_CONNS: usize = 64;
/// The longest description (SDP) taken, in bytes: a browser's offer with its candidates is a few KB.
pub(crate) const MAX_SDP: usize = 32 * 1024;
/// Candidates in one `ice-batch`, and held before a description, as webrtc.js `MAX_ICE_BATCH`.
pub(crate) const MAX_ICE_BATCH: usize = 32;
/// Remote candidates one connection takes, in its descriptions and signals together.
pub(crate) const MAX_CANDIDATES: usize = 64;
/// The longest candidate line taken.
pub(crate) const MAX_CANDIDATE: usize = 512;
/// A DataChannel not open this long after its offer or answer: the connection is dropped (webrtc.js waits
/// `RTC_FALLBACK_MS`, 8 s, then falls back; a page dials it again at its next hello).
pub(crate) const OPEN_WITHIN: Duration = Duration::from_secs(15);
/// An offer with no answer is sent again at these times after the first, as webrtc.js `OFFER_RESEND_MS`.
pub(crate) const OFFER_RESEND: [Duration; 2] = [Duration::from_millis(3000), Duration::from_millis(6000)];
/// Above this many bytes buffered in a connection's SCTP association, the core's datagrams are dropped, as from a
/// full UDP buffer, and its congestion control backs off (webrtc.js `MAX_BUFFERED`).
pub(crate) const MAX_BUFFERED: usize = 1 << 20;
/// Offers, answers and copies of either to one peer: a burst, then one per `PUBLISH_EVERY` (webrtc.js
/// `publishBurst`, `publishEveryMs`), so no peer can keep this node publishing to the relays.
pub(crate) const PUBLISH_BURST: f64 = 6.0;
pub(crate) const PUBLISH_EVERY: Duration = Duration::from_secs(5);
/// A connection is replaced by a new offer from its peer, or restarts ICE for a restart offer, at most once per this
/// long (webrtc.js `retryAnswerMs`).
pub(crate) const REPLACE_AFTER: Duration = Duration::from_secs(2);
/// The label webrtc.js gives its one DataChannel.
const LABEL: &str = "peerwire";

/// The DataChannel every link uses: negotiated out of band, id 0, unordered, no retransmits (webrtc.js `link`).
fn channel_config() -> ChannelConfig {
    ChannelConfig {
        label: LABEL.into(),
        ordered: false,
        reliability: Reliability::MaxRetransmits { retransmits: 0 },
        negotiated: Some(0),
        protocol: String::new(),
    }
}

/// What the host checks for [`Rtcs::signal`]: the room proof of an offer, and whether its sender is a room member
/// this node links with (the traversal's room, which it then counts as a browser).
pub(crate) trait Room {
    /// Whether `proof` (32 bytes) is the `pw-rtc-offer` room proof from `from` to this node.
    fn offer_proved(&self, from: &PeerId, proof: &[u8]) -> bool;
    /// `from` proved the room key in an offer: whether this node links with it (it is a member from now on).
    fn admit(&mut self, from: &PeerId) -> bool;
}

/// One browser's WebRTC connection.
struct Conn {
    rtc: Rtc,
    /// The address that names it ([`Hop::Rtc`](crate::Hop::Rtc)).
    at: SocketAddr,
    /// This node made the offer (its id is the smaller).
    dialer: bool,
    /// Its DataChannel, once made (an answerer makes it once it has answered).
    chan: Option<ChannelId>,
    open: bool,
    /// Dialer: the offer, waiting for its answer.
    pending: Option<SdpPendingOffer>,
    /// Dialer: the offer's signal, sent again at `resend` while unanswered.
    offer_msg: Option<Vec<u8>>,
    resend: VecDeque<Instant>,
    /// Answerer: the description of the offer answered, and the answer's signal, sent again for a copy of it.
    offer_sdp: Option<String>,
    answer_msg: Option<Vec<u8>>,
    /// Answerer: the ICE username fragment of the offer answered, which a restart offer must change.
    ufrag: Option<String>,
    /// Dialer: candidates that came before the answer.
    held: Vec<Candidate>,
    /// Remote candidates taken so far, and the addresses of the relay (TURN) ones among them.
    candidates: usize,
    relays: HashSet<SocketAddr>,
    /// Where its packets go now.
    dest: Option<SocketAddr>,
    made: Instant,
    /// Not open by then: dropped.
    deadline: Instant,
    /// str0m's next timeout.
    next: Instant,
    dead: bool,
}

/// Tokens of a peer's publish budget.
struct Budget {
    tokens: f64,
    at: Instant,
}

/// What the connections' output leaves for the host.
#[derive(Default)]
struct Out {
    udp: VecDeque<(SocketAddr, Vec<u8>)>,
    datagrams: VecDeque<(SocketAddr, PeerId, Vec<u8>)>,
    opened: VecDeque<(PeerId, SocketAddr, bool)>,
}

/// Every browser's WebRTC connection, on the node's socket. See the module docs.
pub(crate) struct Rtcs {
    conns: HashMap<PeerId, Conn>,
    by_at: HashMap<SocketAddr, PeerId>,
    next_at: u32,
    /// Host candidates (this node's socket at each of its addresses), and its public address (STUN's).
    local: Vec<SocketAddr>,
    public: Option<(SocketAddr, SocketAddr)>,
    budgets: HashMap<PeerId, Budget>,
    out: Out,
    signals: VecDeque<(PeerId, Vec<u8>)>,
    closed: VecDeque<(PeerId, SocketAddr)>,
    /// Signals refused (malformed, unproven, over a cap or a budget), and datagrams dropped for a full buffer.
    pub(crate) refused: u64,
    pub(crate) congested: u64,
}

impl Rtcs {
    /// No connection yet. `local`: the host candidates, the node's socket at each address it is reachable at.
    pub(crate) fn new(local: Vec<SocketAddr>) -> Rtcs {
        Rtcs {
            conns: HashMap::new(),
            by_at: HashMap::new(),
            next_at: 0,
            local,
            public: None,
            budgets: HashMap::new(),
            out: Out::default(),
            signals: VecDeque::new(),
            closed: VecDeque::new(),
            refused: 0,
            congested: 0,
        }
    }

    /// The host candidates for connections made from now on.
    pub(crate) fn set_local(&mut self, local: Vec<SocketAddr>) {
        self.local = local;
    }

    /// The node's public address, as STUN saw it, and the host one it maps (its base): a server-reflexive candidate
    /// for connections made from now on.
    pub(crate) fn set_public(&mut self, public: Option<(SocketAddr, SocketAddr)>) {
        self.public = public;
    }

    pub(crate) fn has(&self, peer: &PeerId) -> bool {
        self.conns.contains_key(peer)
    }

    /// The address naming `peer`'s connection, if it has one.
    pub(crate) fn at_of(&self, peer: &PeerId) -> Option<SocketAddr> {
        self.conns.get(peer).map(|c| c.at)
    }

    /// Whether `peer`'s packets go to one of its relay (TURN) candidates now: a link through a TURN server.
    pub(crate) fn relayed(&self, peer: &PeerId) -> bool {
        self.conns.get(peer).is_some_and(|c| c.dest.is_some_and(|d| c.relays.contains(&d)))
    }

    /// The connections, and how many are open.
    pub(crate) fn counts(&self) -> (usize, usize) {
        (self.conns.len(), self.conns.values().filter(|c| c.open).count())
    }

    /// A free address in `0.0.0.0/8`, port 2, to name a connection.
    fn new_at(&mut self) -> SocketAddr {
        loop {
            self.next_at = (self.next_at + 1) & 0x00ff_ffff;
            let at = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(self.next_at), 2));
            if self.next_at != 0 && !self.by_at.contains_key(&at) {
                return at;
            }
        }
    }

    /// A new connection's `Rtc`, with this node's candidates.
    fn new_rtc(&self, now: Instant) -> Rtc {
        // The peer's DTLS certificate must match the fingerprint in its description (str0m's default, kept so):
        // that description came sealed from the peer's PeerId, which binds the DTLS session to it.
        let config = Rtc::builder().set_fingerprint_verification(true);
        let mut rtc = config.set_sctp_max_buffered_amount(MAX_BUFFERED + 64 * 1024).build(now);
        for a in &self.local {
            if let Ok(c) = Candidate::host(*a, "udp") {
                rtc.add_local_candidate(c);
            }
        }
        if let Some((public, base)) = self.public.filter(|(p, _)| !self.local.contains(p)) {
            if let Ok(c) = Candidate::server_reflexive(public, base, "udp") {
                rtc.add_local_candidate(c);
            }
        }
        rtc
    }

    /// Dials `peer` (this node's id is the smaller): a connection and its offer, proved with `proof`, the
    /// `pw-rtc-offer` room proof to it. False if it has one, or past [`MAX_CONNS`].
    pub(crate) fn dial(&mut self, now: Instant, peer: PeerId, proof: &[u8; 32]) -> bool {
        if self.conns.contains_key(&peer) || self.conns.len() >= MAX_CONNS {
            return false;
        }
        let mut rtc = self.new_rtc(now);
        let mut api = rtc.sdp_api();
        let chan = api.add_channel_with_config(channel_config());
        let Some((offer, pending)) = api.apply() else { return false };
        let msg = json!({"kind": "offer", "sdp": {"type": "offer", "sdp": offer.to_sdp_string()}, "p": hex(proof)});
        let msg = msg.to_string().into_bytes();
        let at = self.new_at();
        let mut c = Conn {
            rtc,
            at,
            dialer: true,
            chan: Some(chan),
            open: false,
            pending: Some(pending),
            offer_msg: Some(msg.clone()),
            resend: OFFER_RESEND.iter().map(|d| now + *d).collect(),
            offer_sdp: None,
            answer_msg: None,
            ufrag: None,
            held: Vec::new(),
            candidates: 0,
            relays: HashSet::new(),
            dest: None,
            made: now,
            deadline: now + OPEN_WITHIN,
            next: now,
            dead: false,
        };
        drain(peer, &mut c, &mut self.out);
        self.by_at.insert(at, peer);
        self.conns.insert(peer, c);
        self.publish(now, peer, msg);
        true
    }

    /// A signal from `from` (its sealed JSON, as webrtc.js sends it). Anything malformed, unproven, for no
    /// connection or over a cap is dropped and counted in `refused`.
    pub(crate) fn signal(&mut self, now: Instant, from: PeerId, body: &[u8], room: &mut dyn Room) {
        if !self.take_signal(now, from, body, room) {
            self.refused += 1;
        }
    }

    fn take_signal(&mut self, now: Instant, from: PeerId, body: &[u8], room: &mut dyn Room) -> bool {
        // The carrier bounds a signal (a relay event); the description in it is bounded again below.
        let Ok(m) = serde_json::from_slice::<Value>(body) else { return false };
        match m.get("kind").and_then(Value::as_str) {
            Some("offer") => self.on_offer(now, from, &m, room),
            Some("answer") => self.on_answer(now, from, &m),
            Some("ice") => {
                self.on_candidates(now, from, std::slice::from_ref(m.get("candidate").unwrap_or(&Value::Null)))
            }
            Some("ice-batch") => match m.get("c").and_then(Value::as_array) {
                Some(c) if c.len() <= MAX_ICE_BATCH => self.on_candidates(now, from, c),
                _ => false,
            },
            // A page asks the dialling side for an ICE restart or a retry: links here restart nothing, and come
            // back at the next hello instead.
            Some("restart" | "retry") => self.conns.contains_key(&from),
            _ => false,
        }
    }

    /// An offer: a copy of the one answered gets the answer again; a new one, proved, makes a connection (replacing
    /// one that never opened), or restarts ICE on an open one when it is marked `restart`.
    fn on_offer(&mut self, now: Instant, from: PeerId, m: &Value, room: &mut dyn Room) -> bool {
        let Some(sdp) = description(m, "offer") else { return false };
        if let Some(c) = self.conns.get(&from) {
            if c.dialer {
                return false; // the smaller id dials: this one is ours
            }
            if c.offer_sdp.as_deref() == Some(sdp) {
                let again = c.answer_msg.clone();
                return again.is_some_and(|msg| self.publish(now, from, msg));
            }
        }
        let proof = m.get("p").and_then(Value::as_str).and_then(unhex32);
        if !proof.is_some_and(|p| room.offer_proved(&from, &p)) {
            return false;
        }
        let Ok(offer) = SdpOffer::from_sdp_string(&clean_sdp(sdp)) else { return false };
        if let Some(c) = self.conns.get_mut(&from) {
            if m.get("restart").and_then(Value::as_bool) == Some(true) && c.open {
                // An ICE restart, renegotiated on the open connection: only with new ICE credentials, on which str0m
                // drops the old remote candidates and pairs, and at most once per REPLACE_AFTER; its candidates are
                // counted afresh. With the same ones str0m would add its candidates to the old, which nothing caps,
                // and scan them all for each one added (NB162-2).
                let ufrag = ice_ufrag(&offer);
                if ufrag.is_none() || ufrag == c.ufrag || now.saturating_duration_since(c.made) < REPLACE_AFTER {
                    return false;
                }
                let Ok(answer) = c.rtc.sdp_api().accept_offer(offer) else { return false };
                let msg = answer_msg(&answer);
                (c.offer_sdp, c.answer_msg, c.ufrag) = (Some(sdp.to_string()), Some(msg.clone()), ufrag);
                (c.made, c.candidates, c.relays) = (now, candidate_lines(sdp), relay_addrs(sdp));
                drain(from, c, &mut self.out);
                return self.publish(now, from, msg);
            }
            if now.saturating_duration_since(c.made) < REPLACE_AFTER {
                return false;
            }
            self.close(&from);
        }
        if self.conns.len() >= MAX_CONNS || !room.admit(&from) {
            return false;
        }
        let ufrag = ice_ufrag(&offer);
        let mut rtc = self.new_rtc(now);
        let Ok(answer) = rtc.sdp_api().accept_offer(offer) else { return false };
        let msg = answer_msg(&answer);
        let at = self.new_at();
        let mut c = Conn {
            rtc,
            at,
            dialer: false,
            chan: None,
            open: false,
            pending: None,
            offer_msg: None,
            resend: VecDeque::new(),
            offer_sdp: Some(sdp.to_string()),
            answer_msg: Some(msg.clone()),
            ufrag,
            held: Vec::new(),
            candidates: candidate_lines(sdp),
            relays: relay_addrs(sdp),
            dest: None,
            made: now,
            deadline: now + OPEN_WITHIN,
            next: now,
            dead: false,
        };
        drain(from, &mut c, &mut self.out);
        c.chan = Some(c.rtc.direct_api().create_data_channel(channel_config()));
        drain(from, &mut c, &mut self.out);
        self.by_at.insert(at, from);
        self.conns.insert(from, c);
        self.publish(now, from, msg);
        true
    }

    /// The answer to our offer; a copy, or one for no offer of ours, is dropped.
    fn on_answer(&mut self, _now: Instant, from: PeerId, m: &Value) -> bool {
        let Some(sdp) = description(m, "answer") else { return false };
        let Some(c) = self.conns.get_mut(&from).filter(|c| c.dialer && c.pending.is_some()) else { return false };
        let Ok(answer) = SdpAnswer::from_sdp_string(&clean_sdp(sdp)) else { return false };
        let pending = c.pending.take().expect("checked");
        if c.rtc.sdp_api().accept_answer(pending, answer).is_err() {
            c.dead = true;
            return false;
        }
        c.candidates += candidate_lines(sdp);
        c.relays.extend(relay_addrs(sdp));
        (c.offer_msg, c.resend) = (None, VecDeque::new());
        drain(from, c, &mut self.out);
        for cand in std::mem::take(&mut c.held) {
            c.rtc.add_remote_candidate(cand);
            drain(from, c, &mut self.out);
        }
        true
    }

    /// Candidates (`{candidate, sdpMid, sdpMLineIndex}`, as RTCIceCandidate.toJSON gives them) for a connection
    /// under way: each one that names an IP address (not an mDNS name), up to [`MAX_CANDIDATES`]; before our offer's
    /// answer, held (up to [`MAX_ICE_BATCH`]).
    fn on_candidates(&mut self, _now: Instant, from: PeerId, list: &[Value]) -> bool {
        let Some(c) = self.conns.get_mut(&from) else { return false };
        for v in list {
            let Some(line) = v.get("candidate").and_then(Value::as_str).filter(|l| l.len() <= MAX_CANDIDATE) else {
                continue;
            };
            let line = line.trim().trim_start_matches("a=");
            if line.is_empty() || !names_ip(line) || c.candidates >= MAX_CANDIDATES {
                continue;
            }
            let Ok(cand) = Candidate::from_sdp_string(line) else { continue };
            c.candidates += 1;
            if cand.kind() == CandidateKind::Relayed {
                c.relays.insert(cand.addr());
            }
            if c.pending.is_some() {
                if c.held.len() < MAX_ICE_BATCH {
                    c.held.push(cand);
                }
                continue;
            }
            c.rtc.add_remote_candidate(cand);
            drain(from, c, &mut self.out);
        }
        true
    }

    /// A datagram the node's socket read from `source`: taken (true) if it is a browser's, ICE or DTLS on one of these
    /// connections. Anything else is the node's. `local` names the address it came in at, asked only for one taken.
    pub(crate) fn receive(
        &mut self,
        now: Instant,
        source: SocketAddr,
        local: impl FnOnce() -> SocketAddr,
        data: &[u8],
    ) -> bool {
        if self.conns.is_empty() {
            return false;
        }
        // STUN (0-3) and DTLS (20-63) only, by RFC 7983's first byte: the core's packets start 0x11 to 0x15, and
        // its COOKIE and HS1C (0x14, 0x15) are taken here only from an address a connection's ICE checked.
        if !matches!(data.first(), Some(0..=3 | 20..=63)) {
            return false;
        }
        // Which connection takes it (its ICE username, or an address its ICE checked) doesn't depend on where it
        // came in; what it does there does.
        let Ok(r) = Receive::new(Protocol::Udp, source, source, data) else { return false };
        let probe = Input::Receive(now, r);
        let Some(peer) = self.conns.iter().find(|(_, c)| c.rtc.accepts(&probe)).map(|(p, _)| *p) else {
            return false;
        };
        let Ok(r) = Receive::new(Protocol::Udp, source, local(), data) else { return false };
        let c = self.conns.get_mut(&peer).expect("there");
        if c.rtc.handle_input(Input::Receive(now, r)).is_err() {
            c.dead = true;
        }
        drain(peer, c, &mut self.out);
        true
    }

    /// The core's datagram for the connection named `at`, onto its DataChannel; dropped if it isn't open or its
    /// buffer is past [`MAX_BUFFERED`], as a full UDP buffer drops it.
    pub(crate) fn send(&mut self, at: &SocketAddr, payload: &[u8]) {
        let Some(c) = self.by_at.get(at).and_then(|p| self.conns.get_mut(p)) else { return };
        let (Some(chan), true) = (c.chan, c.open) else { return };
        let sent = c
            .rtc
            .channel(chan)
            .is_some_and(|mut ch| ch.buffered_amount() <= MAX_BUFFERED && ch.write(true, payload).unwrap_or(false));
        if !sent {
            self.congested += 1;
        }
        let peer = self.by_at[at];
        drain(peer, c, &mut self.out);
    }

    /// When [`handle_timeout`](Rtcs::handle_timeout) is due next.
    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        let at = |c: &Conn| {
            let mut t = c.next;
            if !c.open {
                t = t.min(c.deadline);
            }
            c.resend.front().map_or(t, |r| t.min(*r))
        };
        self.conns.values().map(at).min()
    }

    /// Fires due timers: str0m's, offers sent again, connections not open in time or gone dead (closed).
    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        let due: Vec<PeerId> = self
            .conns
            .iter()
            .filter(|(_, c)| {
                c.next <= now || (!c.open && c.deadline <= now) || c.resend.front().is_some_and(|t| *t <= now)
            })
            .map(|(p, _)| *p)
            .collect();
        for peer in due {
            let c = self.conns.get_mut(&peer).expect("there");
            if c.next <= now && c.rtc.handle_input(Input::Timeout(now)).is_err() {
                c.dead = true;
            }
            drain(peer, c, &mut self.out);
            let mut again = None;
            while c.resend.front().is_some_and(|t| *t <= now) {
                c.resend.pop_front();
                again = c.offer_msg.clone().filter(|_| c.pending.is_some());
            }
            if !c.open && c.deadline <= now {
                c.dead = true;
            }
            if let Some(msg) = again {
                self.publish(now, peer, msg);
            }
        }
        let dead: Vec<(PeerId, SocketAddr)> =
            self.conns.iter().filter(|(_, c)| c.dead).map(|(p, c)| (*p, c.at)).collect();
        for (peer, at) in dead {
            self.close(&peer);
            self.closed.push_back((peer, at));
        }
    }

    /// Ends `peer`'s connection, if it has one, telling the browser (a DTLS close).
    pub(crate) fn close(&mut self, peer: &PeerId) {
        if let Some(mut c) = self.conns.remove(peer) {
            c.rtc.disconnect();
            drain(*peer, &mut c, &mut self.out);
            self.by_at.remove(&c.at);
        }
    }

    /// Ends every connection.
    pub(crate) fn close_all(&mut self) {
        for p in self.conns.keys().copied().collect::<Vec<_>>() {
            self.close(&p);
        }
    }

    /// Sends `msg` to `peer` within its publish budget (see [`PUBLISH_BURST`]). Whether it went.
    fn publish(&mut self, now: Instant, peer: PeerId, msg: Vec<u8>) -> bool {
        let every = PUBLISH_EVERY.as_secs_f64();
        let fill =
            |b: &Budget| (b.tokens + now.saturating_duration_since(b.at).as_secs_f64() / every).min(PUBLISH_BURST);
        // Peers whose budget is whole again are forgotten: the map holds only those signalled lately.
        self.budgets.retain(|p, b| *p == peer || fill(b) < PUBLISH_BURST);
        let b = self.budgets.entry(peer).or_insert(Budget { tokens: PUBLISH_BURST, at: now });
        (b.tokens, b.at) = (fill(b), now);
        if b.tokens < 1.0 {
            self.refused += 1;
            return false;
        }
        b.tokens -= 1.0;
        self.signals.push_back((peer, msg));
        true
    }

    /// A packet to send on the node's socket: its destination and bytes.
    pub(crate) fn poll_udp(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        self.out.udp.pop_front()
    }

    /// A signal to seal to its peer and publish on its inbox topic.
    pub(crate) fn poll_signal(&mut self) -> Option<(PeerId, Vec<u8>)> {
        self.signals.pop_front()
    }

    /// A datagram a DataChannel carried, for the endpoint: the connection's address, its peer, the bytes.
    pub(crate) fn poll_datagram(&mut self) -> Option<(SocketAddr, PeerId, Vec<u8>)> {
        self.out.datagrams.pop_front()
    }

    /// A DataChannel that opened: its peer, its address, and whether this node dialled (then it dials the core's
    /// handshake over it, as webrtc.js does).
    pub(crate) fn poll_opened(&mut self) -> Option<(PeerId, SocketAddr, bool)> {
        self.out.opened.pop_front()
    }

    /// A connection that ended on its own (its channel closed, ICE lost, or never open in time): its peer and address.
    pub(crate) fn poll_closed(&mut self) -> Option<(PeerId, SocketAddr)> {
        self.closed.pop_front()
    }
}

/// Drains `c`'s output until its next timeout (str0m's rule after each change), into `out`: packets to send, its
/// channel opening and the datagrams it carries. Its channel closing, ICE lost or an error marks it dead.
fn drain(peer: PeerId, c: &mut Conn, out: &mut Out) {
    loop {
        match c.rtc.poll_output() {
            Ok(Output::Timeout(t)) => {
                c.next = t;
                break;
            }
            Ok(Output::Transmit(t)) => {
                c.dest = Some(t.destination);
                out.udp.push_back((t.destination, t.contents.into()));
            }
            Ok(Output::Event(e)) => match e {
                RtcEvent::ChannelOpen(id, _) if Some(id) == c.chan && !c.open => {
                    c.open = true;
                    out.opened.push_back((peer, c.at, c.dialer));
                }
                RtcEvent::ChannelData(d) if Some(d.id) == c.chan && d.binary => {
                    if (1..=MAX_DATAGRAM).contains(&d.data.len()) {
                        out.datagrams.push_back((c.at, peer, d.data));
                    }
                }
                RtcEvent::ChannelClose(id) if Some(id) == c.chan => c.dead = true,
                RtcEvent::IceConnectionStateChange(IceConnectionState::Disconnected) => c.dead = true,
                _ => {}
            },
            Err(_) => {
                c.dead = true;
                break;
            }
        }
    }
}

/// An answer's signal.
fn answer_msg(answer: &SdpAnswer) -> Vec<u8> {
    json!({"kind": "answer", "sdp": {"type": "answer", "sdp": answer.to_sdp_string()}}).to_string().into_bytes()
}

/// The description of an offer or answer signal (`sdp: {type, sdp}`, RTCSessionDescriptionInit), if it is of `kind`
/// and at most [`MAX_SDP`] bytes.
fn description<'a>(m: &'a Value, kind: &str) -> Option<&'a str> {
    let d = m.get("sdp")?;
    if d.get("type").and_then(Value::as_str).is_some_and(|t| t != kind) {
        return None;
    }
    d.get("sdp").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= MAX_SDP)
}

/// `sdp` with its candidate lines kept only where they name an IP address, at most [`MAX_CANDIDATES`]: a browser
/// names its host addresses by mDNS (`….local`), which this node can't resolve, and str0m's parser takes no name.
fn clean_sdp(sdp: &str) -> String {
    let mut n = 0;
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        if let Some(c) = line.trim_end().strip_prefix("a=") {
            if c.starts_with("candidate:") {
                n += 1;
                if !names_ip(c) || n > MAX_CANDIDATES || c.len() > MAX_CANDIDATE {
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    out
}

/// The ICE username fragment of `offer`, where str0m reads its ICE credentials: the session's, else the first media
/// section's with both a ufrag and a password.
fn ice_ufrag(offer: &SdpOffer) -> Option<String> {
    let creds = offer.session.ice_creds().or_else(|| offer.media_lines.iter().find_map(|m| m.ice_creds()));
    creds.map(|c| c.ufrag)
}

/// Whether candidate line `c` (`candidate:<foundation> <component> <transport> <priority> <address> <port> …`)
/// names an IP address.
fn names_ip(c: &str) -> bool {
    c.split_ascii_whitespace().nth(4).is_some_and(|a| a.parse::<IpAddr>().is_ok())
}

/// The candidate lines of `sdp` that name an IP address.
fn candidate_lines(sdp: &str) -> usize {
    sdp.lines()
        .filter_map(|l| l.trim().strip_prefix("a="))
        .filter(|c| c.starts_with("candidate:") && names_ip(c))
        .count()
}

/// The addresses of the relay (TURN) candidates in `sdp`.
fn relay_addrs(sdp: &str) -> HashSet<SocketAddr> {
    let relay = |c: &str| {
        let f: Vec<&str> = c.split_ascii_whitespace().collect();
        (f.len() >= 8 && f[6] == "typ" && f[7] == "relay")
            .then(|| Some(SocketAddr::new(f[4].parse().ok()?, f[5].parse().ok()?)))
            .flatten()
    };
    sdp.lines().filter_map(|l| l.trim().strip_prefix("a=")).filter_map(relay).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 64 lowercase hex digits as 32 bytes (proof.js `check` takes only those).
fn unhex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 || !b.iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0u8; 32];
    crate::signaling::unhex(s, &mut out).then_some(out)
}
