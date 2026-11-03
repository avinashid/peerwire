//! Signaling on one local network, with no internet: UDP multicast and broadcast.
//!
//! [`LanSignaling`] sends each message as one datagram to an organisation-local
//! multicast group ([`LanSignaling::GROUP`] by default) with a TTL of 1, so it never
//! leaves the link, and the same datagram to the subnet broadcast address of each
//! private interface: some home routers never deliver multicast to small Wi-Fi devices
//! (an ESP32 joins with IGMPv2, which they ignore) but pass broadcast. Every carrier on
//! the LAN hears it, other processes on the same machine too, and each keeps only the
//! topics it subscribed to; a frame heard on both legs is kept once. Rooms
//! ([`Node::join_room`](crate::Node::join_room)) then work as they do over Nostr: peers
//! on one Wi-Fi find each other and link directly, a PC, a container or an ESP32 alike.
//!
//! Like any carrier it is untrusted. Hellos and offers are sealed and proved as they
//! are over a relay (see [`traversal`](crate::traversal)), so a neighbour on the LAN
//! sees topics, timing and the sender's IP, and can't read or forge them; a hello it
//! replays within a minute only makes members offer again. Each socket is bound to the
//! group or to a broadcast address, so it takes only datagrams sent there, not unicast
//! from off the link (except on Windows, which can bind neither: one socket on any
//! address hears both legs).
//!
//! A message is at most [`LanSignaling::MAX_PAYLOAD`] bytes, in one datagram: nothing is
//! fragmented (ESP-IDF's lwIP doesn't reassemble IP fragments). Browsers can't send
//! UDP; they find each other with invites, BroadcastChannel or Nostr.
//!
//! A host firewall must let in UDP to the group's port (20567 by default), or the node
//! hears no one; some guest and public Wi-Fi networks keep clients apart, and there it
//! finds no one either.

use crate::signaling::{Signaling, Topic};
use rand_core::{OsRng, RngCore};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashSet, VecDeque};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 4] = b"PWL1";
/// Magic, sender tag, topic.
const HEADER: usize = 4 + 8 + 32;
/// Largest datagram sent or accepted: one packet on any Ethernet or Wi-Fi link.
const MAX_FRAME: usize = 1400;
/// Datagrams read in one [`poll`](Signaling::poll) before it gives up for now, so a
/// flood of frames for other topics can't hold the caller.
const MAX_READS: usize = 64;
/// Most broadcast addresses one carrier sends to (a host with many bridges has many).
const MAX_LEGS: usize = 4;
/// Frames remembered, and for how long, to drop the copy the other leg delivers: both
/// copies arrive within milliseconds, and a frame sent again on purpose differs (rooms
/// seal each one with a fresh nonce).
const SEEN: usize = 16;
const SEEN_FOR: Duration = Duration::from_secs(1);

/// A [`Signaling`] carrier over UDP multicast on the local network.
///
/// ```no_run
/// use peerwire::{LanSignaling, Node};
///
/// let mut node = Node::builder().signaling(LanSignaling::new()?).bind("0.0.0.0:0")?;
/// node.join_room("my-app", "kitchen"); // links with every node in "kitchen" on this LAN
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct LanSignaling {
    /// The group's socket, which sends every frame, then one bound to each broadcast
    /// address (none on Windows, where the first hears broadcasts too).
    socks: Vec<UdpSocket>,
    /// Where each frame goes: the group, then each broadcast address.
    to: Vec<SocketAddrV4>,
    /// The socket the next [`poll`](Signaling::poll) reads first, so a flood on one
    /// leg can't starve the other.
    turn: usize,
    /// Tags this carrier's frames, so it drops its own when the group loops them back.
    sender: [u8; 8],
    topics: HashSet<Topic>,
    /// Hashes of frames lately heard, and when.
    seen: VecDeque<(u64, Instant)>,
    hasher: RandomState,
    buf: Vec<u8>,
}

impl LanSignaling {
    /// The default group: 239.255.80.87 ("PW"), port 20567, in the organisation-local
    /// scope that routers don't forward.
    pub const GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 80, 87), 20567);
    /// The largest message [`publish`](Signaling::publish) sends; larger ones are dropped.
    pub const MAX_PAYLOAD: usize = MAX_FRAME - HEADER;

    /// A carrier on [`GROUP`](LanSignaling::GROUP), on the interface the system picks
    /// for multicast (normally the default route's), and on the broadcast address of
    /// each private interface.
    pub fn new() -> io::Result<LanSignaling> {
        LanSignaling::on(LanSignaling::GROUP, Ipv4Addr::UNSPECIFIED)
    }

    /// A carrier on `group` (an IPv4 multicast address and port), listening and sending
    /// on the interface whose address is `interface` (`0.0.0.0`: the system's choice),
    /// and on that interface's broadcast address (`0.0.0.0`: each private interface's).
    /// A device with several interfaces names the LAN's here; tests name `127.0.0.1` to
    /// stay on loopback, which has no broadcast address. ESP-IDF can't list interfaces:
    /// an ESP32 names its own address and adds its subnet with
    /// [`broadcast`](LanSignaling::broadcast).
    pub fn on(group: SocketAddrV4, interface: Ipv4Addr) -> io::Result<LanSignaling> {
        if !group.ip().is_multicast() {
            return Err(io::Error::new(ErrorKind::InvalidInput, "not an IPv4 multicast group"));
        }
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        // Other carriers on this machine listen on the same port.
        s.set_reuse_address(true)?;
        // Bound to the group, the socket takes only datagrams sent to it: TTL 1 keeps
        // our frames on the link, and this keeps unicast from anywhere else out.
        #[cfg(not(windows))]
        let ip = *group.ip();
        #[cfg(windows)]
        let ip = Ipv4Addr::UNSPECIFIED;
        s.bind(&SocketAddrV4::new(ip, group.port()).into())?;
        s.join_multicast_v4(group.ip(), &interface)?;
        if !interface.is_unspecified() {
            s.set_multicast_if_v4(&interface)?;
        }
        s.set_multicast_ttl_v4(1)?;
        s.set_multicast_loop_v4(true)?;
        // It sends the broadcast leg too (lwIP sends from a socket bound to a
        // multicast address with the interface's, but not from one bound to a
        // broadcast address), and TTL 1 keeps that on the link as well.
        s.set_broadcast(true)?;
        s.set_ttl_v4(1)?;
        s.set_nonblocking(true)?;
        let mut c = LanSignaling {
            socks: vec![s.into()],
            to: vec![group],
            turn: 0,
            sender: sender_tag(),
            topics: HashSet::new(),
            seen: VecDeque::with_capacity(SEEN),
            hasher: RandomState::new(),
            buf: vec![0; MAX_FRAME + 1],
        };
        for b in broadcasts(interface) {
            // Best effort: the multicast leg works without it.
            let _ = c.add_broadcast(b);
        }
        Ok(c)
    }

    /// Also sends each frame to `addr`, a subnet's broadcast address such as
    /// `192.168.1.255`, and hears frames sent to it. [`on`](LanSignaling::on) adds
    /// these itself where it can list interfaces; an ESP32 names its own. At most four;
    /// one named twice is added once.
    pub fn broadcast(mut self, addr: Ipv4Addr) -> io::Result<LanSignaling> {
        self.add_broadcast(addr)?;
        Ok(self)
    }

    fn add_broadcast(&mut self, addr: Ipv4Addr) -> io::Result<()> {
        if addr.is_multicast() || addr.is_unspecified() {
            return Err(io::Error::new(ErrorKind::InvalidInput, "not a broadcast address"));
        }
        let to = SocketAddrV4::new(addr, self.to[0].port());
        if self.to.contains(&to) {
            return Ok(());
        }
        if self.to.len() > MAX_LEGS {
            return Err(io::Error::new(ErrorKind::InvalidInput, "too many broadcast addresses"));
        }
        // Bound to the broadcast address, the socket takes only datagrams sent to it,
        // as the group's does. Windows can't bind one: the group's socket, bound to
        // any address there, hears broadcasts already.
        #[cfg(not(windows))]
        {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            s.set_reuse_address(true)?;
            // lwIP delivers broadcasts only to sockets that allow them, if built to check.
            s.set_broadcast(true)?;
            s.bind(&to.into())?;
            s.set_nonblocking(true)?;
            self.socks.push(s.into());
        }
        self.to.push(to);
        Ok(())
    }

    /// Whether the frame hashed to `h` is new: not heard in the last [`SEEN_FOR`] (as
    /// the other leg's copy is).
    fn fresh(&mut self, h: u64) -> bool {
        let now = Instant::now();
        if self.seen.iter().any(|&(k, at)| k == h && now.duration_since(at) < SEEN_FOR) {
            return false;
        }
        if self.seen.len() == SEEN {
            self.seen.pop_front();
        }
        self.seen.push_back((h, now));
        true
    }
}

impl Signaling for LanSignaling {
    fn subscribe(&mut self, topic: Topic) {
        self.topics.insert(topic);
    }

    fn unsubscribe(&mut self, topic: Topic) {
        self.topics.remove(&topic);
    }

    fn publish(&mut self, topic: Topic, payload: &[u8]) {
        if let Some(f) = frame(&self.sender, &topic, payload) {
            for to in &self.to {
                // Best effort, as carriers are: with no network yet, or a full send
                // buffer, the message is lost, and rooms repeat their hellos.
                let _ = self.socks[0].send_to(&f, to);
            }
        }
    }

    fn poll(&mut self) -> Option<(Topic, Vec<u8>)> {
        // One datagram from each socket in turn, so a flood on one leg can't starve the
        // other, until all are empty or MAX_READS have been read.
        let (mut reads, mut idle) = (0, 0);
        while reads < MAX_READS && idle < self.socks.len() {
            let i = self.turn;
            self.turn = (i + 1) % self.socks.len();
            let n = match self.socks[i].recv(&mut self.buf) {
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    idle += 1;
                    continue;
                }
                // ICMP errors surface here on some platforms: keep reading.
                Err(_) => 0,
            };
            (reads, idle) = (reads + 1, 0);
            let Some((sender, topic, _)) = parse(&self.buf[..n]) else { continue };
            if sender == self.sender || !self.topics.contains(&topic) {
                continue;
            }
            let h = self.hasher.hash_one(&self.buf[..n]);
            if self.fresh(h) {
                return Some((topic, self.buf[HEADER..n].to_vec()));
            }
        }
        None
    }
}

/// The broadcast addresses of this device's private IPv4 interfaces (RFC 1918 or
/// link-local), or of `interface` alone if it is one.
#[cfg(not(target_os = "espidf"))]
fn broadcasts(interface: Ipv4Addr) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        let if_addrs::IfAddr::V4(a) = i.addr else { continue };
        let private = a.ip.is_private() || a.ip.is_link_local();
        if let Some(b) = a.broadcast.filter(|&b| b != a.ip) {
            if private && (interface.is_unspecified() || a.ip == interface) && !out.contains(&b) {
                out.push(b);
            }
        }
    }
    out
}

/// ESP-IDF has no getifaddrs: the firmware adds its subnet itself.
#[cfg(target_os = "espidf")]
fn broadcasts(_interface: Ipv4Addr) -> Vec<Ipv4Addr> {
    Vec::new()
}

/// Random bytes, mixed with the clock and a counter: an ESP32 that has no entropy yet
/// (before Wi-Fi) must still not share a tag with another carrier. It tells this
/// carrier's frames from others' and protects nothing.
fn sender_tag() -> [u8; 8] {
    // 32-bit: an ESP32-C3 has no 64-bit atomics.
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let mut tag = [0u8; 8];
    OsRng.fill_bytes(&mut tag);
    let mix = nanos ^ u64::from(COUNT.fetch_add(1, Ordering::Relaxed)) << 40;
    (u64::from_le_bytes(tag) ^ mix).to_le_bytes()
}

/// `None` if `payload` won't fit in one frame.
fn frame(sender: &[u8; 8], topic: &Topic, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > LanSignaling::MAX_PAYLOAD {
        return None;
    }
    let mut f = Vec::with_capacity(HEADER + payload.len());
    f.extend_from_slice(MAGIC);
    f.extend_from_slice(sender);
    f.extend_from_slice(&topic.0);
    f.extend_from_slice(payload);
    Some(f)
}

fn parse(d: &[u8]) -> Option<([u8; 8], Topic, &[u8])> {
    if d.len() < HEADER || d.len() > MAX_FRAME || &d[..4] != MAGIC {
        return None;
    }
    let sender = d[4..12].try_into().ok()?;
    let topic = Topic(d[12..HEADER].try_into().ok()?);
    Some((sender, topic, &d[HEADER..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A group port of this test's own, so parallel tests and test processes don't hear each other.
    fn group(test: u16) -> SocketAddrV4 {
        SocketAddrV4::new(*LanSignaling::GROUP.ip(), 30000 + (std::process::id() % 4000) as u16 * 8 + test)
    }

    fn carrier(test: u16) -> LanSignaling {
        LanSignaling::on(group(test), Ipv4Addr::LOCALHOST).expect("multicast on loopback")
    }

    /// Loopback's broadcast address, which Linux has (macOS doesn't).
    #[cfg(target_os = "linux")]
    const LO_BROADCAST: Ipv4Addr = Ipv4Addr::new(127, 255, 255, 255);

    /// A carrier on this test's port, on `group`, with a broadcast leg on loopback.
    #[cfg(target_os = "linux")]
    fn with_broadcast(test: u16, g: Ipv4Addr) -> LanSignaling {
        let on = SocketAddrV4::new(g, group(test).port());
        LanSignaling::on(on, Ipv4Addr::LOCALHOST)
            .and_then(|c| c.broadcast(LO_BROADCAST))
            .expect("broadcast on loopback")
    }

    /// Polls `c` for up to half a second.
    fn next(c: &mut LanSignaling) -> Option<(Topic, Vec<u8>)> {
        let end = Instant::now() + Duration::from_millis(500);
        while Instant::now() < end {
            if let Some(m) = c.poll() {
                return Some(m);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        None
    }

    #[test]
    fn frames_round_trip_and_bad_ones_are_dropped() {
        let (s, t) = ([7u8; 8], Topic([9; 32]));
        let f = frame(&s, &t, b"hi").unwrap();
        assert_eq!(f.len(), HEADER + 2);
        assert_eq!(parse(&f), Some((s, t, &b"hi"[..])));
        let full = frame(&s, &t, &[0; LanSignaling::MAX_PAYLOAD]).unwrap();
        assert_eq!(full.len(), MAX_FRAME);
        assert!(parse(&full).is_some());
        assert_eq!(frame(&s, &t, &[0; LanSignaling::MAX_PAYLOAD + 1]), None, "too big for one datagram");
        assert_eq!(parse(&f[..HEADER - 1]), None, "short");
        let mut bad = f.clone();
        bad[0] = b'X';
        assert_eq!(parse(&bad), None, "wrong magic");
        let mut big = full.clone();
        big.push(0);
        assert_eq!(parse(&big), None, "longer than a frame");
        assert!(LanSignaling::on(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9), Ipv4Addr::LOCALHOST).is_err());
    }

    #[test]
    fn carriers_hear_each_other_on_subscribed_topics_only() {
        let (mut a, mut b, mut c) = (carrier(0), carrier(0), carrier(0));
        let (t, other) = (Topic([1; 32]), Topic([2; 32]));
        a.subscribe(t);
        b.subscribe(t);
        a.publish(t, b"from a");
        a.publish(other, b"not for anyone");
        assert_eq!(next(&mut b), Some((t, b"from a".to_vec())));
        assert_eq!(next(&mut a), None, "not echoed to the sender");
        assert_eq!(next(&mut c), None, "not subscribed");
        b.unsubscribe(t);
        c.publish(t, b"x");
        assert_eq!(next(&mut a), Some((t, b"x".to_vec())));
        assert_eq!(next(&mut b), None);
        // Too big for a frame: dropped, not truncated.
        a.publish(t, &vec![0; LanSignaling::MAX_PAYLOAD + 1]);
        b.subscribe(t);
        assert_eq!(next(&mut b), None);
        a.publish(t, &vec![5; LanSignaling::MAX_PAYLOAD]);
        assert_eq!(next(&mut b), Some((t, vec![5; LanSignaling::MAX_PAYLOAD])));
    }

    #[test]
    fn a_flood_of_junk_costs_a_bounded_read_per_poll() {
        let mut a = carrier(1);
        let t = Topic([3; 32]);
        a.subscribe(t);
        // Raw datagrams from another carrier's socket, which sends on loopback.
        let junk = carrier(1);
        let theirs = frame(&[1; 8], &Topic([4; 32]), b"other topic").unwrap();
        for i in 0..2 * MAX_READS {
            let d: &[u8] = if i % 2 == 0 { b"not a frame" } else { &theirs };
            junk.socks[0].send_to(d, group(1)).unwrap();
        }
        junk.socks[0].send_to(&frame(&[1; 8], &t, b"wanted").unwrap(), group(1)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // Each call reads at most MAX_READS datagrams, so it takes three to reach the wanted one.
        let mut calls = 0;
        let got = loop {
            calls += 1;
            if let Some(m) = a.poll() {
                break m;
            }
            assert!(calls < 10, "the wanted frame never came");
        };
        assert_eq!(got, (t, b"wanted".to_vec()));
        assert_eq!(calls, 3, "{calls} calls read {} datagrams", 2 * MAX_READS + 1);
    }

    /// NB64-1: a frame sent to the carrier's port by unicast, as a host off the link
    /// could, is never delivered, to the group's socket or (NB-092) to the broadcast
    /// leg's; frames sent to the group still are.
    #[cfg(not(windows))]
    #[test]
    fn unicast_to_the_port_is_not_delivered() {
        #[cfg(target_os = "linux")]
        let mut a = with_broadcast(2, *LanSignaling::GROUP.ip());
        #[cfg(not(target_os = "linux"))]
        let mut a = carrier(2);
        let t = Topic([6; 32]);
        a.subscribe(t);
        let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
        stranger.send_to(&frame(&[2; 8], &t, b"unicast").unwrap(), (Ipv4Addr::LOCALHOST, group(2).port())).unwrap();
        // Then one to the group: it arrives, and the unicast one, sent first, never does.
        carrier(2).publish(t, b"multicast");
        assert_eq!(next(&mut a), Some((t, b"multicast".to_vec())));
        assert_eq!(next(&mut a), None);
    }

    /// NB-092: a frame reaches a carrier by the broadcast leg alone, as on a router that
    /// never delivers multicast to the device, and one heard on both legs is kept once.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_broadcast_leg_works_alone_and_copies_are_kept_once() {
        let t = Topic([7; 32]);
        // Each on a group of its own: only broadcast joins them.
        let (mut a, mut b) =
            (with_broadcast(3, Ipv4Addr::new(239, 255, 80, 87)), with_broadcast(3, Ipv4Addr::new(239, 255, 80, 88)));
        let mut deaf =
            LanSignaling::on(SocketAddrV4::new(Ipv4Addr::new(239, 255, 80, 88), group(3).port()), Ipv4Addr::LOCALHOST)
                .unwrap();
        for c in [&mut a, &mut b, &mut deaf] {
            c.subscribe(t);
        }
        a.publish(t, b"by broadcast");
        assert_eq!(next(&mut b), Some((t, b"by broadcast".to_vec())));
        assert_eq!(next(&mut a), None, "not echoed to the sender");
        assert_eq!(next(&mut deaf), None, "no broadcast leg, and not its group");
        // On one group and one broadcast address, both copies arrive and one is kept.
        let (mut c, mut d) =
            (with_broadcast(4, *LanSignaling::GROUP.ip()), with_broadcast(4, *LanSignaling::GROUP.ip()));
        d.subscribe(t);
        c.publish(t, b"twice");
        assert_eq!(next(&mut d), Some((t, b"twice".to_vec())));
        assert_eq!(next(&mut d), None, "the other leg's copy");
        assert_eq!(d.seen.len(), 1);
        c.publish(t, b"another");
        assert_eq!(next(&mut d), Some((t, b"another".to_vec())));
    }

    #[test]
    fn a_frame_heard_again_within_a_second_is_dropped() {
        let mut c = carrier(5);
        assert!(c.fresh(1));
        assert!(!c.fresh(1), "the other leg's copy");
        assert!(c.fresh(2));
        // Heard longer ago than SEEN_FOR: new again.
        if let Some(old) = Instant::now().checked_sub(SEEN_FOR) {
            c.seen[0].1 = old;
            assert!(c.fresh(1));
        }
        // Only the last SEEN are remembered.
        for h in 10..10 + SEEN as u64 {
            assert!(c.fresh(h));
        }
        assert_eq!(c.seen.len(), SEEN);
        assert!(c.fresh(2), "forgotten");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn broadcast_addresses_are_checked_and_capped() {
        assert!(carrier(6).broadcast(*LanSignaling::GROUP.ip()).is_err(), "multicast");
        assert!(carrier(6).broadcast(Ipv4Addr::UNSPECIFIED).is_err());
        let c = carrier(6).broadcast(LO_BROADCAST).and_then(|c| c.broadcast(LO_BROADCAST)).unwrap();
        assert_eq!((c.to.len(), c.socks.len()), (2, 2), "named twice, added once");
        // Any loopback address binds, so these stand in for more interfaces' broadcasts.
        let c = (2..=4).try_fold(c, |c, i| c.broadcast(Ipv4Addr::new(127, 0, 0, i))).unwrap();
        assert_eq!(c.to.len(), 1 + MAX_LEGS);
        assert!(c.broadcast(Ipv4Addr::new(127, 0, 0, 5)).is_err(), "more than MAX_LEGS");
        // Loopback has no broadcast address; whatever else this host has is private.
        assert_eq!(broadcasts(Ipv4Addr::LOCALHOST), Vec::<Ipv4Addr>::new());
        assert_eq!(carrier(7).to.len(), 1, "on loopback: the group only");
    }
}
