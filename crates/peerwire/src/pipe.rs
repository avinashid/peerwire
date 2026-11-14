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

use crate::signaling::{b64, unb64};
use peerwire_core::{Instant, PeerId, MAX_DATAGRAM};
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
/// Pipes at once. A new one past this replaces the one used longest ago whose peer isn't linked; with none, the
/// new peer's batch is dropped.
pub(crate) const MAX_PIPES: usize = if SMALL { 4 } else { 64 };
/// A pipe whose peer isn't linked, with nothing waiting, is closed once unused this long ([`Pipes::prune`]).
pub(crate) const IDLE: Duration = Duration::from_secs(60);
/// The head and tail of a batch, as `JSON.stringify` writes `{ kind: 'dg', d }`.
const HEAD: &str = r#"{"kind":"dg","d":["#;
const TAIL: &str = "]}";
/// A token, in the bucket's units (micro-tokens).
const TOKEN: u64 = 1_000_000;

struct Pipe {
    addr: SocketAddr,
    q: VecDeque<Vec<u8>>,
    bytes: usize,
    /// When the next batch goes, if datagrams wait.
    due: Option<Instant>,
    used: Instant,
    /// When it last sent a batch, as a count of batches sent (0: never): the pipe served longest ago goes first.
    served: u64,
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
        }
    }
}

impl Pipes {
    /// The address of `peer`'s pipe, made if it has none. `None` if [`MAX_PIPES`] are open and every one's peer is
    /// `linked`.
    pub(crate) fn open(&mut self, now: Instant, peer: PeerId, linked: impl Fn(&PeerId) -> bool) -> Option<SocketAddr> {
        if let Some(p) = self.by_peer.get_mut(&peer) {
            p.used = now;
            return Some(p.addr);
        }
        if self.by_peer.len() >= MAX_PIPES {
            let old = self.by_peer.iter().filter(|(p, _)| !linked(p)).min_by_key(|(_, x)| x.used).map(|(p, _)| *p)?;
            self.close(&old);
        }
        // 0.0.0.0/8, port 1: no network's, and apart from a Bluetooth connection's (port 0) in logs.
        self.next = self.next.wrapping_add(1) & 0x00ff_ffff;
        if self.next == 0 {
            self.next = 1;
        }
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(self.next), 1));
        self.by_addr.insert(addr, peer);
        let pipe = Pipe { addr, q: VecDeque::new(), bytes: 0, due: None, used: now, served: 0 };
        self.by_peer.insert(peer, pipe);
        Some(addr)
    }

    /// Closes the pipes whose peer isn't `linked`, with nothing waiting, unused [`IDLE`] long. Each page load is a
    /// new peer, so its pipe was otherwise kept until `MAX_PIPES` others pushed it out (NB-132). One whose link
    /// just ended keeps what it queued (the close) until sent, and a peer dialling again within `IDLE` keeps its
    /// address.
    pub(crate) fn prune(&mut self, now: Instant, linked: impl Fn(&PeerId) -> bool) {
        let by_addr = &mut self.by_addr;
        self.by_peer.retain(|peer, p| {
            let keep = !p.q.is_empty() || now - p.used < IDLE || linked(peer);
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
        // Peer 0 is linked: peer 1, used longest ago of the rest, gives way.
        let fresh = pipes.open(t + MS * 100, peer(200), |p| *p == peer(0)).unwrap();
        assert!(!addrs.contains(&fresh));
        assert_eq!(pipes.len(), MAX_PIPES);
        assert!(pipes.by_peer.contains_key(&peer(0)) && !pipes.by_peer.contains_key(&peer(1)));
        pipes.send(t, &addrs[1], vec![1]);
        assert_eq!(pipes.next_due(), None, "the closed pipe's address names nothing");
    }
}
