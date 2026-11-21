//! Pipes ([`PathKind::Signaling`](crate::PathKind::Signaling)): a peer's datagrams carried over the signaling
//! carrier, sealed to it on its inbox topic, in the shape a browser's `'signaling'` path sends them
//! (`packages/js/src/webrtc.js`): `{"kind":"dg","d":["<base64>",…]}`. A browser, which can't send UDP, links with a
//! native peer this way. Slow (a relay's round trip each way) and rate-limited: a last resort.
//!
//! The node names each pipe by a made-up address ([`Hop::Pipe`](crate::Hop::Pipe)) that the endpoint dials and
//! hears like any other. What the endpoint sends there waits [`FLUSH`] for more, then goes as one batch of at most
//! [`BATCH`] bytes of datagrams; all pipes together send [`RATE`] batches a second (a burst of [`BURST`]). Past
//! [`QUEUE`] bytes waiting on a pipe, the oldest datagram goes, as from a full UDP buffer: the endpoint resends what
//! it must. Batches are taken only from room members (`Traversal::handle_signal`), at most [`MAX_PIPES`] peers at
//! once.
//!
//! With every pipe taken by a linked peer, a newcomer's batch is held ([`Pipes::hold`]) rather than dropped, and let
//! in ([`Pipes::admit`]) as soon as a pipe frees: its link ended, or its peer has gone quiet, heard nothing from for
//! [`quiet`] while the node missed nothing sent to it. A page closed without a goodbye then gives its pipe to the next
//! page in seconds, not at the link's timeout (NB-136); a linked peer heard from lately never does.

use crate::signaling::{b64, unb64};
use peerwire_core::{Config, Instant, PeerId, MAX_DATAGRAM};
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

const SMALL: bool = cfg!(target_os = "espidf");
/// How long a datagram waits for others to share its batch.
pub(crate) const FLUSH: Duration = Duration::from_millis(40);
/// Bytes of datagrams in one batch sent: one full datagram. A batch to an ESP32 has to fit the 4 KB relay message
/// its carrier takes (`nostr.rs` `Limits::SMALL`), and one full datagram, base64'd, sealed and base64'd again in a
/// signed event, makes about 2.8 KB.
pub(crate) const BATCH: usize = MAX_DATAGRAM;
/// Datagrams in one batch, sent or taken: JavaScript's `MAX_PIPE_ITEMS`.
pub(crate) const MAX_ITEMS: usize = 64;
/// Batches all pipes together send a second, and the burst.
pub(crate) const RATE: u64 = 4;
pub(crate) const BURST: u64 = 8;
/// Bytes waiting on one pipe.
pub(crate) const QUEUE: usize = if SMALL { 4 << 10 } else { 16 << 10 };
/// Pipes at once. A new one past this replaces the one used longest ago whose peer isn't linked, if not dialled within
/// [`GRACE`]; with none, the new peer's batch is held ([`Pipes::hold`]). Each linked page costs a board 3 to 5 KB of
/// heap, and with 4 linked its largest free block bottomed at 36,864 B (NB-133's run): a 16 KiB TLS record takes a
/// block of 32 KiB while it comes in (NB133-2), so a fifth would leave too little. 4 there (NB-136).
pub(crate) const MAX_PIPES: usize = if SMALL { 4 } else { 64 };
/// A pipe whose peer hasn't linked over it, dialled this lately, doesn't give way to a newcomer: a handshake is under
/// way on it, its answer a relay's round trip or two off. Two newcomers took each other's pipe before either linked
/// (NB-136). One whose link has ended gives way at once.
const GRACE: Duration = Duration::from_secs(5);
/// A pipe whose peer isn't linked, with nothing waiting, is closed once unused this long ([`Pipes::prune`]).
pub(crate) const IDLE: Duration = Duration::from_secs(60);
/// Newcomers whose batches are held while every pipe is taken ([`Pipes::hold`]), and how long one is held after the
/// newcomer's last batch: a dial sends its handshake again 0.5, 1.5, 3.5 and 7.5 s in, and a page gives up at 15 s.
const MAX_HELD: usize = 2;
const HOLD: Duration = Duration::from_secs(10);
/// The head and tail of a batch, as `JSON.stringify` writes `{ kind: 'dg', d }`.
const HEAD: &str = r#"{"kind":"dg","d":["#;
const TAIL: &str = "]}";
/// A token, in the bucket's units (micro-tokens).
const TOKEN: u64 = 1_000_000;

/// How long a linked peer must have said nothing over its pipe, while the node missed nothing sent to it, to count as
/// gone ([`Pipes::admit`]): halfway from `keepalive` to `idle_timeout`, 20 s by default. A peer that is there sends
/// something at least each `keepalive` (10 s), and once it has been silent that long the node pings it each
/// `probe_interval` (1 s), every ping answered: so this is 10 s of pings unanswered, 10 s before the link would time
/// out anyway.
pub(crate) fn quiet(config: &Config) -> Duration {
    config.keepalive + config.idle_timeout.saturating_sub(config.keepalive) / 2
}

struct Pipe {
    addr: SocketAddr,
    q: VecDeque<Vec<u8>>,
    bytes: usize,
    /// When the next batch goes, if datagrams wait.
    due: Option<Instant>,
    used: Instant,
    /// When it last sent a batch, as a count of batches sent (0: never): the pipe served longest ago goes first.
    served: u64,
    /// Its peer was linked when last seen ([`Pipes::prune`]), since its last batch that came while it wasn't.
    linked: bool,
    /// When it was made, or its peer, linked over it before, dialled again: [`GRACE`] counts from here, not from its
    /// last batch, which anyone who saw it can post again (the seal proves who sent a batch, not when) (NB136-1).
    since: Instant,
}

impl Pipe {
    /// Whether it gives way to a newcomer, its peer not linked: unless a handshake may be under way on it, its peer
    /// never linked over it since its last dial and that dial's first batch within [`GRACE`].
    fn spare(&self, now: Instant) -> bool {
        self.linked || now.saturating_duration_since(self.since) >= GRACE
    }
}

/// A newcomer's last batch, held while every pipe is taken ([`Pipes::hold`]).
struct Held {
    peer: PeerId,
    datagrams: Vec<Vec<u8>>,
    at: Instant,
}

/// A held batch given a pipe ([`Pipes::admit`]): its peer, the pipe's address and the batch's datagrams.
pub(crate) struct Admitted {
    pub(crate) peer: PeerId,
    pub(crate) addr: SocketAddr,
    pub(crate) datagrams: Vec<Vec<u8>>,
    /// The linked peer gone quiet whose pipe it took, and for how long it had been quiet: the node ends its link.
    pub(crate) gone: Option<(PeerId, Duration)>,
}

/// The node's pipes, by peer and by the address that names each.
pub(crate) struct Pipes {
    by_peer: HashMap<PeerId, Pipe>,
    by_addr: HashMap<SocketAddr, PeerId>,
    next: u32,
    /// Batches sent so far, for `Pipe::served`.
    sent: u64,
    /// The rate bucket all pipes share, in micro-tokens, and when it was last filled.
    credit: u64,
    filled: Instant,
    /// [`MAX_PIPES`], but in tests.
    max: usize,
    /// Newcomers' batches held, the one held longest ago first.
    held: VecDeque<Held>,
    /// When the node last may have missed a batch on its way in ([`missed`](Pipes::missed)): silence before it
    /// says nothing of a peer.
    missed: Option<Instant>,
    /// The carrier's count of messages it may have missed ([`Signaling::missed`](crate::Signaling::missed)), as
    /// last seen.
    carrier_missed: u64,
}

impl Default for Pipes {
    fn default() -> Self {
        Pipes {
            by_peer: HashMap::new(),
            by_addr: HashMap::new(),
            next: 0,
            sent: 0,
            credit: BURST * TOKEN,
            filled: Instant::ZERO,
            max: MAX_PIPES,
            held: VecDeque::new(),
            missed: None,
            carrier_missed: 0,
        }
    }
}

impl Pipes {
    /// Holds `peer`'s batch while every pipe is taken by a linked peer ([`open`](Pipes::open) gave it none), until
    /// [`admit`](Pipes::admit) finds it one or [`HOLD`] passes: the newcomer's last batch alone, up to [`BATCH`]
    /// bytes of its datagrams (a dial's handshake is one). Past [`MAX_HELD`] newcomers, the one held longest ago
    /// goes. Whether `peer` was held already.
    pub(crate) fn hold(&mut self, now: Instant, peer: PeerId, mut datagrams: Vec<Vec<u8>>) -> bool {
        let mut n = 0;
        let keep = datagrams.iter().take_while(|d| {
            n += d.len();
            n <= BATCH
        });
        let keep = keep.count().max(1);
        datagrams.truncate(keep);
        let again = self.held.iter().position(|h| h.peer == peer).and_then(|i| self.held.remove(i)).is_some();
        if self.held.len() >= MAX_HELD {
            self.held.pop_front();
        }
        self.held.push_back(Held { peer, datagrams, at: now });
        again
    }

    /// Notes that the node may have missed a batch on its way in at `now`: its inbox dropped a message unread
    /// (`inbox`, when it last did), or the carrier's count of messages it may have missed (`carrier`) moved since
    /// the last call. A peer's silence before then is no sign it has gone ([`admit`](Pipes::admit)).
    pub(crate) fn missed(&mut self, now: Instant, carrier: u64, inbox: Option<Instant>) {
        if carrier != self.carrier_missed {
            self.carrier_missed = carrier;
            self.missed = Some(now);
        }
        self.missed = self.missed.max(inbox);
    }

    /// Gives the batch held longest ([`hold`](Pipes::hold)) a pipe if one is free or frees: the peer's own if it has
    /// one by now, a new one under [`MAX_PIPES`], else the pipe of a peer not linked that gives way (its link ended,
    /// or no handshake under way: [`GRACE`]), the one used longest ago, else the pipe of a linked peer gone quiet, the
    /// one heard from longest ago. `heard` gives when a pipe's peer was last heard from if it is linked
    /// (`Endpoint::last_heard`), `None` if not; a linked peer has gone quiet once that, or the last time the node may
    /// have missed something ([`missed`](Pipes::missed)), whichever is later, is `quiet` ago ([`quiet`]). A linked
    /// peer heard from lately never gives way. Held batches [`HOLD`] old, or whose peer `takes` refuses now, are
    /// dropped. `None` if none is held, or no pipe frees.
    pub(crate) fn admit(
        &mut self,
        now: Instant,
        quiet: Duration,
        takes: impl Fn(&PeerId) -> bool,
        heard: impl Fn(&PeerId) -> Option<Instant>,
    ) -> Option<Admitted> {
        self.held.retain(|h| now.saturating_duration_since(h.at) < HOLD && takes(&h.peer));
        let peer = self.held.front()?.peer;
        let mut gone = None;
        if !self.by_peer.contains_key(&peer) && self.by_peer.len() >= self.max {
            let spare = |(p, x): &(&PeerId, &Pipe)| heard(p).is_none() && x.spare(now);
            let unlinked = self.by_peer.iter().filter(spare).min_by_key(|(_, x)| x.used);
            let victim = match unlinked.map(|(p, _)| *p) {
                Some(p) => p,
                None => {
                    let missed = self.missed;
                    let since = |p: &PeerId| heard(p).map(|t| (*p, t.max(missed.unwrap_or(t))));
                    let quietest = self.by_peer.keys().filter_map(since).min_by_key(|(_, t)| *t);
                    let (p, t) = quietest.filter(|(_, t)| now.saturating_duration_since(*t) >= quiet)?;
                    gone = Some((p, now.saturating_duration_since(heard(&p).unwrap_or(t))));
                    p
                }
            };
            self.close(&victim);
        }
        let h = self.held.pop_front()?;
        let addr = self.open(now, h.peer, |_| true)?;
        Some(Admitted { peer: h.peer, addr, datagrams: h.datagrams, gone })
    }

    /// Sets the most pipes at once, [`MAX_PIPES`] by default: a test's.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn set_max(&mut self, max: usize) {
        self.max = max.max(1);
    }

    /// The address of `peer`'s pipe, made if it has none. `None` if [`MAX_PIPES`] are open and none gives way, every
    /// one's peer `linked` or a handshake under way on it ([`GRACE`]): then [`hold`](Pipes::hold) its batch.
    pub(crate) fn open(&mut self, now: Instant, peer: PeerId, linked: impl Fn(&PeerId) -> bool) -> Option<SocketAddr> {
        if let Some(p) = self.by_peer.get_mut(&peer) {
            p.used = now;
            // From a peer not linked: a dial, perhaps again.
            if p.linked && !linked(&peer) {
                (p.linked, p.since) = (false, now);
            }
            return Some(p.addr);
        }
        if self.by_peer.len() >= self.max {
            let spare = |(p, x): &(&PeerId, &Pipe)| !linked(p) && x.spare(now);
            let old = self.by_peer.iter().filter(spare).min_by_key(|(_, x)| x.used).map(|(p, _)| *p)?;
            self.close(&old);
        }
        // What it sent before, held, is old now.
        self.held.retain(|h| h.peer != peer);
        // 0.0.0.0/8, port 1: no network's, and apart from a Bluetooth connection's (port 0) in logs.
        self.next = self.next.wrapping_add(1) & 0x00ff_ffff;
        if self.next == 0 {
            self.next = 1;
        }
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(self.next), 1));
        self.by_addr.insert(addr, peer);
        let pipe =
            Pipe { addr, q: VecDeque::new(), bytes: 0, due: None, used: now, served: 0, linked: false, since: now };
        self.by_peer.insert(peer, pipe);
        Some(addr)
    }

    /// Closes the pipes whose peer isn't `linked`, with nothing waiting, unused [`IDLE`] long. Each page load is a
    /// new peer, so its pipe was otherwise kept until `MAX_PIPES` others pushed it out (NB-132). One whose link
    /// just ended keeps what it queued (the close) until sent, and a peer dialling again within `IDLE` keeps its
    /// address. It notes which pipes' peers are linked, for [`GRACE`]: the node calls it after each poll.
    pub(crate) fn prune(&mut self, now: Instant, linked: impl Fn(&PeerId) -> bool) {
        let by_addr = &mut self.by_addr;
        self.by_peer.retain(|peer, p| {
            let is = linked(peer);
            p.linked |= is;
            let keep = !p.q.is_empty() || now - p.used < IDLE || is;
            if !keep {
                by_addr.remove(&p.addr);
            }
            keep
        });
    }

    /// Forgets `peer`'s pipe and what waits on it.
    pub(crate) fn close(&mut self, peer: &PeerId) {
        if let Some(p) = self.by_peer.remove(peer) {
            self.by_addr.remove(&p.addr);
        }
    }

    /// Queues `datagram` on the pipe named `addr`; dropped if there is none.
    pub(crate) fn send(&mut self, now: Instant, addr: &SocketAddr, datagram: Vec<u8>) {
        let Some(p) = self.by_addr.get(addr).and_then(|peer| self.by_peer.get_mut(peer)) else { return };
        p.bytes += datagram.len();
        p.q.push_back(datagram);
        while p.bytes > QUEUE && p.q.len() > 1 {
            p.bytes -= p.q.pop_front().map_or(0, |d| d.len());
        }
        p.used = now;
        // A full batch goes as soon as the rate allows.
        let full = p.bytes >= BATCH || p.q.len() >= MAX_ITEMS;
        p.due = Some(if full { now } else { p.due.unwrap_or(now + FLUSH) });
    }

    /// The next batch due, if the rate allows one: its peer, and its body to seal to it. Of the pipes with one due,
    /// the one served longest ago goes first, so busy pipes take turns.
    pub(crate) fn take(&mut self, now: Instant) -> Option<(PeerId, String)> {
        let (peer, _) = self
            .by_peer
            .iter()
            .filter_map(|(k, p)| Some((*k, (p.served, p.due?))))
            .filter(|(_, (_, d))| *d <= now)
            .min_by_key(|(_, key)| *key)?;
        let gained = now.saturating_duration_since(self.filled).as_micros() as u64 * RATE;
        self.credit = (self.credit + gained).min(BURST * TOKEN);
        self.filled = now;
        if self.credit < TOKEN {
            return None;
        }
        self.credit -= TOKEN;
        let p = self.by_peer.get_mut(&peer)?;
        let mut body = String::from(HEAD);
        let (mut n, mut items) = (0, 0);
        while let Some(d) = p.q.front() {
            if items == MAX_ITEMS || (items > 0 && n + d.len() > BATCH) {
                break;
            }
            let d = p.q.pop_front().unwrap_or_default();
            if items > 0 {
                body.push(',');
            }
            body.push('"');
            body.push_str(&b64(&d));
            body.push('"');
            (n, items) = (n + d.len(), items + 1);
        }
        body.push_str(TAIL);
        p.bytes -= n;
        p.due = (!p.q.is_empty()).then_some(now);
        self.sent += 1;
        p.served = self.sent;
        Some((peer, body))
    }

    /// When the next batch is due, if any waits: not before the rate has a token for it, so a host that sleeps
    /// until then doesn't spin while batches wait for the rate (NB126-3).
    pub(crate) fn next_due(&self) -> Option<Instant> {
        let due = self.by_peer.values().filter_map(|p| p.due).min()?;
        let short = TOKEN.saturating_sub(self.credit).div_ceil(RATE);
        Some(due.max(self.filled + Duration::from_micros(short)))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_peer.len()
    }
}

/// Whether `body` (JSON from a peer's inbox) is a pipe batch rather than another signal: it starts as one does.
#[cfg(feature = "webrtc")]
pub(crate) fn is_batch(body: &[u8]) -> bool {
    body.starts_with(br#"{"kind":"dg""#)
}

/// The datagrams of a batch `body`, if it is one as [`Pipes::take`] and `webrtc.js` write it: 1 to [`MAX_ITEMS`]
/// base64 datagrams of 1 to [`MAX_DATAGRAM`] bytes. Anything else is `None`: no other JSON is taken.
pub(crate) fn decode(body: &[u8]) -> Option<Vec<Vec<u8>>> {
    let list = std::str::from_utf8(body).ok()?.strip_prefix(HEAD)?.strip_suffix(TAIL)?;
    let mut out = Vec::new();
    for item in list.split(',') {
        let s = item.strip_prefix('"')?.strip_suffix('"')?;
        if out.len() == MAX_ITEMS || s.len() > MAX_DATAGRAM.div_ceil(3) * 4 {
            return None;
        }
        let d = unb64(s).filter(|d| !d.is_empty() && d.len() <= MAX_DATAGRAM)?;
        out.push(d);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> PeerId {
        PeerId([n; 32])
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn batches_round_trip_in_the_javascript_shape() {
        let mut pipes = Pipes::default();
        let t = Instant::ZERO;
        let at = pipes.open(t, peer(1), |_| false).unwrap();
        pipes.send(t, &at, b"one".to_vec());
        pipes.send(t, &at, vec![0xff; 4]);
        assert_eq!(pipes.take(t), None, "waits for more");
        assert_eq!(pipes.next_due(), Some(t + FLUSH));
        let (to, body) = pipes.take(t + FLUSH).unwrap();
        assert_eq!(to, peer(1));
        assert_eq!(body, r#"{"kind":"dg","d":["b25l","/////w=="]}"#);
        assert_eq!(decode(body.as_bytes()), Some(vec![b"one".to_vec(), vec![0xff; 4]]));
        assert_eq!(pipes.next_due(), None);
    }

    #[test]
    fn decode_takes_only_a_batch() {
        let ok = |s: &str| decode(s.as_bytes());
        assert_eq!(ok(r#"{"kind":"dg","d":["AA=="]}"#), Some(vec![vec![0]]));
        for bad in [
            r#"{"kind":"dg","d":[]}"#,
            r#"{"kind":"dg","d":[""]}"#,
            r#"{"kind":"dg","d":["AA==",]}"#,
            r#"{"kind":"dg","d":[AA==]}"#,
            r#"{"kind":"dg","d":["A"]}"#,
            r#"{"kind":"offer","d":["AA=="]}"#,
            r#"{"d":["AA=="],"kind":"dg"}"#,
            r#" {"kind":"dg","d":["AA=="]}"#,
            r#"{"kind":"dg","d":["AA\u003d\u003d"]}"#,
        ] {
            assert_eq!(ok(bad), None, "{bad}");
        }
        let big = format!(r#"{{"kind":"dg","d":["{}"]}}"#, b64(&vec![1; MAX_DATAGRAM + 1]));
        assert_eq!(ok(&big), None, "a datagram over MAX_DATAGRAM");
        let full = format!(r#"{{"kind":"dg","d":["{}"]}}"#, b64(&vec![1; MAX_DATAGRAM]));
        assert_eq!(ok(&full).map(|d| d[0].len()), Some(MAX_DATAGRAM));
        let many = |n: usize| format!(r#"{{"kind":"dg","d":[{}]}}"#, vec![r#""AA==""#; n].join(","));
        assert_eq!(ok(&many(MAX_ITEMS)).map(|d| d.len()), Some(MAX_ITEMS));
        assert_eq!(ok(&many(MAX_ITEMS + 1)), None);
    }

    #[test]
    fn a_batch_holds_one_full_datagram_or_several_small() {
        let mut pipes = Pipes::default();
        let t = Instant::ZERO;
        let at = pipes.open(t, peer(1), |_| false).unwrap();
        for _ in 0..3 {
            pipes.send(t, &at, vec![7; MAX_DATAGRAM]);
        }
        // Full: due at once, one datagram a batch.
        for left in [2, 1, 0] {
            let (_, body) = pipes.take(t).unwrap();
            assert_eq!(decode(body.as_bytes()).unwrap().len(), 1);
            assert_eq!(pipes.by_peer[&peer(1)].q.len(), left);
        }
        // Small ones share a batch, up to BATCH bytes.
        for _ in 0..10 {
            pipes.send(t, &at, vec![1; 200]);
        }
        let (_, body) = pipes.take(t + FLUSH).unwrap();
        assert_eq!(decode(body.as_bytes()).unwrap().len(), BATCH / 200);
        assert!(body.len() < 2 * BATCH, "{}", body.len());
    }

    #[test]
    fn the_rate_is_shared_and_refills() {
        let mut pipes = Pipes::default();
        let t = Instant::ZERO;
        let (a, b) = (pipes.open(t, peer(1), |_| false).unwrap(), pipes.open(t, peer(2), |_| false).unwrap());
        for _ in 0..BURST {
            pipes.send(t, &a, vec![1; MAX_DATAGRAM]);
            pipes.send(t, &b, vec![2; MAX_DATAGRAM]);
        }
        let sent = std::iter::from_fn(|| pipes.take(t)).count() as u64;
        assert_eq!(sent, BURST, "a burst, across both pipes");
        assert_eq!(pipes.take(t + 200 * MS), None, "under a token");
        assert!(pipes.take(t + 250 * MS).is_some(), "one per 1/RATE s");
    }

    /// NB126-3: a batch waiting for the rate is due when the bucket next has a token, not at once, so a host that
    /// sleeps until `next_due` doesn't spin while it waits.
    #[test]
    fn a_batch_waiting_for_the_rate_is_due_with_the_next_token() {
        let mut pipes = Pipes::default();
        let t = Instant::ZERO;
        let at = pipes.open(t, peer(1), |_| false).unwrap();
        for _ in 0..BURST + 2 {
            pipes.send(t, &at, vec![1; MAX_DATAGRAM]);
        }
        assert_eq!(std::iter::from_fn(|| pipes.take(t)).count() as u64, BURST);
        let token = Duration::from_micros(TOKEN / RATE);
        assert_eq!(pipes.next_due(), Some(t + token), "not now: when the next token comes");
        assert_eq!(pipes.take(t + (token - MS)), None);
        assert!(pipes.take(t + token).is_some());
        // Refused part way to a token: due when the rest of it comes.
        assert_eq!(pipes.take(t + token + 100 * MS), None);
        assert_eq!(pipes.next_due(), Some(t + token * 2));
        assert!(pipes.take(t + token * 2).is_some());
        assert_eq!(pipes.next_due(), None);
    }

    /// The rate goes to the pipe whose batch has waited longest: two busy pipes take turns, so one peer can't take
    /// every batch.
    #[test]
    fn busy_pipes_take_turns() {
        let mut pipes = Pipes::default();
        let t = Instant::ZERO;
        let (a, b) = (pipes.open(t, peer(1), |_| false).unwrap(), pipes.open(t, peer(2), |_| false).unwrap());
        for _ in 0..4 {
            pipes.send(t, &a, vec![1; MAX_DATAGRAM]);
        }
        pipes.send(t + MS, &b, vec![2; MAX_DATAGRAM]);
        pipes.send(t + MS, &b, vec![2; MAX_DATAGRAM]);
        let order: Vec<u8> = std::iter::from_fn(|| pipes.take(t + MS * 2)).map(|(p, _)| p.0[0]).collect();
        assert_eq!(order, [1, 2, 1, 2, 1, 1]);
    }

    #[test]
    fn a_pipe_queue_drops_its_oldest_past_the_cap() {
        let mut pipes = Pipes::default();
        let t = Instant::ZERO;
        let at = pipes.open(t, peer(1), |_| false).unwrap();
        for i in 0..(QUEUE / 1000 + 4) {
            pipes.send(t, &at, vec![i as u8; 1000]);
        }
        let p = &pipes.by_peer[&peer(1)];
        assert!(p.bytes <= QUEUE && p.bytes == p.q.iter().map(Vec::len).sum::<usize>());
        assert_eq!(p.q.front().unwrap()[0], 4, "the oldest went");
        pipes.send(t, &"0.0.0.9:1".parse().unwrap(), vec![1]); // no such pipe: dropped
    }

    #[test]
    fn pipes_are_capped_and_a_linked_one_is_kept() {
        let mut pipes = Pipes::default();
        let t = Instant::ZERO;
        let addrs: Vec<_> =
            (0..MAX_PIPES).map(|i| pipes.open(t + MS * i as u32, peer(i as u8), |_| false).unwrap()).collect();
        assert_eq!(pipes.len(), MAX_PIPES);
        assert_eq!(pipes.open(t, peer(0), |_| true), Some(addrs[0]), "the same address again");
        // Every peer linked: no room.
        assert_eq!(pipes.open(t + MS * 100, peer(200), |_| true), None);
        // Peer 0 is linked. The rest dialled within GRACE, so their handshakes may be under way: no room either.
        assert_eq!(pipes.open(t + MS * 100, peer(200), |p| *p == peer(0)), None);
        // Past it, peer 1, used longest ago of the rest, gives way.
        let fresh = pipes.open(t + GRACE + MS * 100, peer(200), |p| *p == peer(0)).unwrap();
        assert!(!addrs.contains(&fresh));
        assert_eq!(pipes.len(), MAX_PIPES);
        assert!(pipes.by_peer.contains_key(&peer(0)) && !pipes.by_peer.contains_key(&peer(1)));
        pipes.send(t, &addrs[1], vec![1]);
        assert_eq!(pipes.next_due(), None, "the closed pipe's address names nothing");
    }

    const S: Duration = Duration::from_secs(1);
    /// [`quiet`] with the default config: 20 s.
    const QUIET: Duration = Duration::from_secs(20);

    /// A board's table, 4 pipes, each taken by a linked peer (0 to 3) at `t`.
    fn full(t: Instant) -> (Pipes, Vec<SocketAddr>) {
        let mut pipes = Pipes::default();
        pipes.set_max(4);
        let addrs = (0..4).map(|i| pipes.open(t, peer(i), |_| true).unwrap()).collect();
        pipes.prune(t, |_| true); // as the node does after each poll: each pipe's peer was seen linked
        assert_eq!(pipes.open(t, peer(9), |_| true), None, "full");
        (pipes, addrs)
    }

    /// `pipes.admit` with every peer taken, and `heard` as given.
    fn admit(pipes: &mut Pipes, now: Instant, heard: impl Fn(&PeerId) -> Option<Instant>) -> Option<Admitted> {
        pipes.admit(now, QUIET, |_| true, heard)
    }

    #[test]
    fn the_quiet_mark_is_halfway_from_the_keepalive_to_the_idle_timeout() {
        assert_eq!(quiet(&Config::default()), QUIET);
        let config = |keepalive, idle_timeout| Config { keepalive, idle_timeout, ..Config::default() };
        assert_eq!(quiet(&config(500 * MS, 5 * S)), 2750 * MS);
        assert_eq!(quiet(&config(10 * S, 5 * S)), 10 * S, "past the idle timeout: the link times out first");
    }

    /// NB-136: with every pipe taken by a linked peer, a newcomer's batch is held, and takes a pipe as soon as one
    /// frees: its peer's link ended, or another left its pipe.
    #[test]
    fn a_newcomer_past_the_cap_is_held_and_takes_the_first_pipe_that_frees() {
        let t = Instant::ZERO + S;
        let (mut pipes, addrs) = full(t);
        assert!(!pipes.hold(t, peer(9), vec![b"hs1".to_vec()]), "held anew");
        let now = t + S;
        assert!(admit(&mut pipes, now, |_| Some(now)).is_none(), "every peer linked and heard");
        assert_eq!(pipes.held.len(), 1);
        // Peer 2's link ends (a goodbye, or the endpoint's timeout): its pipe goes to the newcomer at once.
        let a = admit(&mut pipes, now, |p| (*p != peer(2)).then_some(now)).expect("admitted");
        assert_eq!((a.peer, a.datagrams, a.gone), (peer(9), vec![b"hs1".to_vec()], None));
        assert!(!addrs.contains(&a.addr));
        assert_eq!(pipes.len(), 4);
        assert!(!pipes.by_peer.contains_key(&peer(2)) && pipes.by_peer.contains_key(&peer(9)));
        assert!(pipes.held.is_empty());
        assert!(admit(&mut pipes, now, |_| None).is_none(), "nothing held");
        // A batch of its own once a pipe is free also ends the hold.
        let (mut pipes, _) = full(t);
        pipes.hold(t, peer(9), vec![vec![1]]);
        pipes.close(&peer(0));
        assert!(pipes.open(t, peer(9), |_| true).is_some());
        assert!(pipes.held.is_empty());
    }

    /// NB-136: a linked peer heard from within `QUIET` never gives way, however many newcomers wait and for however
    /// long: a page that is there is heard each keepalive (10 s) at least.
    #[test]
    fn sec_a_linked_peer_heard_lately_never_gives_way() {
        let t = Instant::ZERO + S;
        let (mut pipes, _) = full(t);
        for s in 0..120u32 {
            let now = t + S * s;
            // Each peer is heard each 10 s, at its own second (peer p at p, p + 10, ...); a newcomer, new each
            // second, waits.
            let last = |p: u32| if s < p { 0 } else { s - (s - p) % 10 };
            let heard = |p: &PeerId| Some(t + S * last(u32::from(p.0[0])));
            pipes.hold(now, peer(100 + (s % 50) as u8), vec![vec![1]]);
            assert!(admit(&mut pipes, now, heard).is_none(), "at {s} s");
            assert_eq!(pipes.len(), 4);
            assert!((0..4).all(|i| pipes.by_peer.contains_key(&peer(i))));
        }
        // Heard just under QUIET ago: kept.
        let at = t + S * 200;
        let now = at + (QUIET - Duration::from_millis(1));
        pipes.hold(now, peer(9), vec![vec![1]]);
        assert!(admit(&mut pipes, now, |_| Some(at)).is_none());
    }

    /// NB-136: once a linked peer has said nothing for `QUIET`, it has gone (a page closed with no goodbye): its pipe
    /// goes to a held newcomer, the one heard from longest ago first, and the node is told to end its link.
    #[test]
    fn a_linked_peer_quiet_that_long_gives_way_the_quietest_first() {
        let t = Instant::ZERO + S;
        let (mut pipes, _) = full(t);
        // Peers 0 to 3 were last heard 1, 2, 3 and 4 s in.
        let heard = |p: &PeerId| Some(t + S * u32::from(p.0[0] + 1));
        let early = t + S + (QUIET - Duration::from_millis(1));
        pipes.hold(early, peer(9), vec![vec![1]]);
        assert!(admit(&mut pipes, early, heard).is_none());
        let now = t + S * 2 + QUIET;
        let a = admit(&mut pipes, now, heard).expect("peer 0 has gone");
        assert_eq!((a.peer, a.gone), (peer(9), Some((peer(0), QUIET + S))));
        assert!(!pipes.by_peer.contains_key(&peer(0)));
        // The next newcomer: peer 1, now QUIET quiet, goes; 2 and 3 not yet.
        pipes.hold(now, peer(10), vec![vec![1]]);
        let a = admit(&mut pipes, now, |p| if *p == peer(9) { Some(now) } else { heard(p) }).unwrap();
        assert_eq!((a.peer, a.gone), (peer(10), Some((peer(1), QUIET))));
        pipes.hold(now, peer(11), vec![vec![1]]);
        assert!(admit(&mut pipes, now, |p| if p.0[0] >= 9 { Some(now) } else { heard(p) }).is_none());
        // A peer not linked goes before any linked one, quiet or not.
        let later = now + QUIET * 2;
        pipes.hold(later, peer(11), vec![vec![1]]);
        let a = admit(&mut pipes, later, |p| (*p != peer(10)).then_some(t)).unwrap();
        assert_eq!((a.peer, a.gone), (peer(11), None));
        assert!(!pipes.by_peer.contains_key(&peer(10)) && pipes.by_peer.contains_key(&peer(2)));
    }

    /// NB-136: silence while the node may have missed what a peer sent (its inbox dropped a message unread, or the
    /// carrier's count of what it may have missed moved) is no sign the peer has gone: the quiet mark counts from the
    /// later of the two. A flood that spends the inbox's budget doesn't make a page that is there look gone.
    #[test]
    fn sec_silence_while_the_node_may_have_missed_batches_says_nothing() {
        let t = Instant::ZERO + S;
        let heard = |_: &PeerId| Some(t);
        // A newcomer that dials again and again, admitted at `now` if it can be.
        let wait = |pipes: &mut Pipes, now: Instant| {
            pipes.hold(now, peer(9), vec![vec![1]]);
            admit(pipes, now, heard)
        };
        // The inbox dropped one 5 s after the peers were last heard: they have gone only QUIET after that.
        let (mut pipes, _) = full(t);
        pipes.missed(t + S * 5, 0, Some(t + S * 5));
        assert!(wait(&mut pipes, t + QUIET + S * 4).is_none());
        assert!(wait(&mut pipes, t + QUIET + S * 5).is_some());
        // The carrier's count moved at 7 s: from then. The same count again, later, is nothing new.
        let (mut pipes, _) = full(t);
        pipes.missed(t + S * 7, 3, None);
        pipes.missed(t + S * 9, 3, None);
        assert!(wait(&mut pipes, t + QUIET + S * 6).is_none());
        let a = wait(&mut pipes, t + QUIET + S * 7).unwrap();
        assert_eq!(a.gone.map(|(_, s)| s), Some(QUIET + S * 7), "silent since t: the log says so");
        // A miss that keeps coming keeps every linked peer.
        let (mut pipes, _) = full(t);
        for s in 0..60u32 {
            let now = t + S * s;
            pipes.hold(now, peer(9), vec![vec![1]]);
            pipes.missed(now, 0, Some(now));
            assert!(admit(&mut pipes, now, heard).is_none(), "at {s} s");
        }
    }

    /// NB-136: at most `MAX_HELD` newcomers are held, each its last batch alone and no more than `BATCH` bytes of it,
    /// for `HOLD` after that batch; one its room no longer takes is dropped.
    #[test]
    fn held_batches_are_few_short_and_brief() {
        let t = Instant::ZERO + S;
        let (mut pipes, _) = full(t);
        assert!(!pipes.hold(t, peer(9), vec![vec![1]]));
        assert!(pipes.hold(t, peer(9), vec![vec![2]; 3]), "its later batch, in place of the first");
        assert_eq!(pipes.held.iter().map(|h| (h.peer, h.datagrams.len())).collect::<Vec<_>>(), [(peer(9), 3)]);
        pipes.hold(t, peer(10), vec![vec![3; 1000], vec![4; 232], vec![5; 1]]);
        assert_eq!(pipes.held[1].datagrams.len(), 2, "BATCH bytes");
        pipes.hold(t, peer(11), vec![vec![7; MAX_DATAGRAM], vec![8]]);
        assert_eq!(pipes.held.iter().map(|h| h.peer).collect::<Vec<_>>(), [peer(10), peer(11)], "the oldest went");
        assert_eq!(pipes.held[1].datagrams, [vec![7; MAX_DATAGRAM]]);
        // A pipe frees. Peer 10 isn't taken now (no longer a member, say): dropped, and 11 let in.
        pipes.close(&peer(0));
        let a = pipes.admit(t, QUIET, |p| *p != peer(10), |_| None).unwrap();
        assert_eq!(a.peer, peer(11));
        assert!(pipes.held.is_empty());
        // One held HOLD ago is dropped, though a pipe is free.
        pipes.close(&peer(1));
        pipes.hold(t, peer(12), vec![vec![1]]);
        assert!(pipes.admit(t + HOLD, QUIET, |_| true, |_| None).is_none());
        assert!(pipes.held.is_empty());
        assert_eq!(pipes.len(), 3, "and no pipe was made");
    }

    /// NB-136: two newcomers held while every pipe was taken don't take each other's pipe before either has linked:
    /// a pipe whose peer hasn't linked over it, dialled within `GRACE`, has a handshake under way and doesn't give way.
    /// Past `GRACE` from its dial it does, though its batches came since (NB136-1), and one whose link has ended does
    /// at once.
    #[test]
    fn a_handshake_under_way_keeps_its_pipe_and_a_link_that_ended_gives_way_at_once() {
        let t = Instant::ZERO + S;
        let (mut pipes, _) = full(t);
        pipes.hold(t, peer(9), vec![vec![1]]);
        pipes.hold(t, peer(10), vec![vec![1]]);
        // Peer 0's link ends: 9 takes its pipe at once; 10 waits, though 9 isn't linked yet.
        let linked = |p: &PeerId| p.0[0] < 4 && *p != peer(0);
        let a = pipes.admit(t, QUIET, |_| true, |p| linked(p).then_some(t)).unwrap();
        assert_eq!(a.peer, peer(9));
        assert!(pipes.admit(t, QUIET, |_| true, |p| linked(p).then_some(t)).is_none());
        let soon = t + (GRACE - MS);
        // 9's batch again, or a copy of it: GRACE still counts from its dial.
        assert_eq!(pipes.open(soon, peer(9), linked), Some(a.addr));
        // 10 dials again within GRACE: still no room, by `open` either.
        assert_eq!(pipes.open(soon, peer(10), linked), None);
        pipes.hold(soon, peer(10), vec![vec![1]]);
        assert!(pipes.admit(soon, QUIET, |_| true, |p| linked(p).then_some(soon)).is_none());
        // 9 never answered: GRACE after its dial, 10 takes it.
        let a = pipes.admit(t + GRACE, QUIET, |_| true, |p| linked(p).then_some(t + GRACE)).unwrap();
        assert_eq!(a.peer, peer(10));
        assert!(!pipes.by_peer.contains_key(&peer(9)));
        // 10 links; later its link ends: its pipe gives way at once, though its goodbye just came over it.
        let now = t + GRACE * 2;
        pipes.prune(now, |p| *p != peer(0) && *p != peer(9));
        let with_10 = |p: &PeerId| linked(p) || *p == peer(10);
        assert_eq!(pipes.open(now, peer(10), with_10), Some(a.addr), "its goodbye, while it was linked");
        let fresh = pipes.open(now, peer(11), linked).expect("10's pipe");
        assert!(!pipes.by_peer.contains_key(&peer(10)) && pipes.by_peer[&peer(11)].addr == fresh);
    }
}
