//! A [`Node`]'s side of WebRTC links to browsers (feature `webrtc`, [`Builder::webrtc`](super::Builder::webrtc)):
//! the connections ([`Rtcs`]) on the node's socket, their signals over its carrier, and room.js's room layer
//! ([`Members`]) on every link to a browser, whatever its path. A child of `node`, so it reaches the node's fields.

use super::{about, Addr, Node};
use crate::signaling::Topic;
use crate::traversal::{Hop, Privacy, Traversal};
use crate::webrtc::room::{self, Members, Step};
use crate::webrtc::{Room, Rtcs};
use peerwire_core::{DisconnectReason, Event as Ev, Lane, Path, PeerId, SendError};
use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Room frames waiting for a reliable lane's send window, over all browsers.
const MAX_OUTBOX: usize = 1024;
/// Source addresses whose local address the node remembers (a socket bound to any interface).
const MAX_ROUTES: usize = 256;

/// The WebRTC connections, the room layer, and what waits to go.
pub(super) struct Browsers {
    rtcs: Rtcs,
    members: Members,
    /// Room frames that met a full send window, in order.
    outbox: VecDeque<(PeerId, Vec<u8>)>,
    /// Connections to end once what the core sent them has gone (its close, after a goodbye): each peer's, if it is
    /// still the one named then (a new offer may have replaced it meanwhile).
    closing: Vec<(PeerId, Option<SocketAddr>)>,
    /// The local address each source reaches this node's socket at, when it is bound to any interface.
    routes: HashMap<IpAddr, IpAddr>,
    /// The public address the connections were last given.
    public: Option<SocketAddr>,
    /// Host candidates.
    local: Vec<SocketAddr>,
}

impl Browsers {
    /// A datagram the socket (bound to `bound`) read from `source`: taken if it is a browser's ICE or DTLS.
    pub(super) fn receive(&mut self, wall: Instant, bound: SocketAddr, source: SocketAddr, data: &[u8]) -> bool {
        if self.rtcs.counts().0 == 0 {
            return false;
        }
        let routes = &mut self.routes;
        self.rtcs.receive(wall, source, || local_for(bound, routes, source), data)
    }

    /// A browser's WebRTC signal: an offer is answered if it proves the room key and its sender is a member this
    /// node links with (the traversal counts it a browser from then on).
    pub(super) fn signal(
        &mut self,
        now: peerwire_core::Instant,
        wall: Instant,
        trav: &mut Traversal<Addr>,
        me: PeerId,
        from: PeerId,
        body: &[u8],
    ) {
        let mut check = Check { trav, me, now };
        self.rtcs.signal(wall, from, body, &mut check);
    }

    /// The core's datagram for the DataChannel named `at`.
    pub(super) fn send(&mut self, at: &SocketAddr, payload: &[u8]) {
        self.rtcs.send(at, payload);
    }

    /// Whether `peer`'s link goes through a TURN server (one of its relay candidates).
    pub(super) fn relayed(&self, peer: &PeerId) -> bool {
        self.rtcs.relayed(peer)
    }
}

/// The traversal's room, for the offers [`Rtcs`] takes.
struct Check<'a> {
    trav: &'a mut Traversal<Addr>,
    me: PeerId,
    now: peerwire_core::Instant,
}

impl Room for Check<'_> {
    fn offer_proved(&self, from: &PeerId, proof: &[u8]) -> bool {
        self.trav.room_pair_ok(b"pw-rtc-offer", from, &self.me, proof)
    }

    fn admit(&mut self, from: &PeerId) -> bool {
        self.trav.admit_browser(self.now, *from)
    }
}

impl Node {
    /// Starts the browsers' side, if asked for and not relay-only.
    pub(super) fn start_browsers(&mut self, on: bool) {
        if !on || matches!(self.privacy(), Privacy::RelayOnly { .. }) {
            return;
        }
        let local = self.browser_candidates(self.privacy());
        self.trav.set_browsers(true);
        self.browsers = Some(Box::new(Browsers {
            rtcs: Rtcs::new(local.clone()),
            members: Members::new(),
            outbox: VecDeque::new(),
            closing: Vec::new(),
            routes: HashMap::new(),
            public: None,
            local,
        }));
    }

    /// The host candidates under `privacy`: the socket at each of the node's addresses (loopback only when bound to
    /// it); public ones alone under [`Privacy::PublicOnly`], as room.js's `'public-only'` gives no host candidate.
    fn browser_candidates(&self, privacy: Privacy) -> Vec<SocketAddr> {
        let all = super::local_addresses(self.local);
        let loopback = self.local.ip().is_loopback();
        let keep = |a: &SocketAddr| {
            let ip = a.ip().to_canonical();
            !ip.is_unspecified()
                && (loopback || !ip.is_loopback())
                && (!matches!(privacy, Privacy::PublicOnly) || crate::traversal::is_public(a))
        };
        all.into_iter().map(|a| SocketAddr::new(a.ip().to_canonical(), a.port())).filter(keep).collect()
    }

    /// The privacy changed: relay-only ends every browser link (ICE gives out addresses), and takes none; public-only
    /// gives new connections public candidates alone.
    pub(super) fn privacy_browsers(&mut self, privacy: Privacy) {
        let relay_only = matches!(privacy, Privacy::RelayOnly { .. });
        let local = self.browser_candidates(privacy);
        let Some(b) = self.browsers.as_mut() else { return };
        self.trav.set_browsers(!relay_only);
        if relay_only {
            b.rtcs.close_all();
        }
        b.local = local.clone();
        b.rtcs.set_local(local);
    }

    /// The browsers' work in a poll: dials the traversal names, the connections' timers, what they opened, carried
    /// and closed.
    pub(super) fn poll_browsers(&mut self, now: peerwire_core::Instant, wall: Instant) {
        let Some(b) = self.browsers.as_mut() else { return };
        let me = self.ep.peer_id();
        while let Some(peer) = self.trav.poll_browser_dial() {
            if self.ep.is_connected(&peer) || b.rtcs.has(&peer) {
                continue;
            }
            if let Some(proof) = self.trav.room_pair(b"pw-rtc-offer", &me, &peer) {
                b.rtcs.dial(wall, peer, &proof);
            }
        }
        // The public address STUN gave, as a server-reflexive candidate for connections made from now on.
        let public = self.trav.public_addr().map(|a| *a.addr()).filter(|_| !self.local.ip().is_loopback());
        if public != b.public {
            b.public = public;
            let base = public.and_then(|p| b.local.iter().find(|l| l.is_ipv4() == p.is_ipv4()).copied());
            b.rtcs.set_public(public.zip(base));
        }
        if b.rtcs.poll_timeout().is_some_and(|t| t <= wall) {
            b.rtcs.handle_timeout(wall);
        }
        while let Some((peer, at, dialer)) = b.rtcs.poll_opened() {
            // As webrtc.js: a link already up (over a pipe, say) moves onto the channel; else the dialler dials.
            if self.ep.is_connected(&peer) {
                self.ep.add_candidates(now, &peer, &[Hop::Rtc(at)]);
            } else if dialer {
                let _ = self.ep.connect(now, Hop::Rtc(at), peer);
            }
        }
        while let Some((at, peer, d)) = b.rtcs.poll_datagram() {
            self.ep.handle_datagram_from(now, Hop::Rtc(at), &peer, &d);
        }
        let mut lost = Vec::new();
        while let Some(closed) = b.rtcs.poll_closed() {
            lost.push(closed);
        }
        // A connection that ended under a link on it (its channel closed, ICE lost): the link ends now, not at its
        // idle timeout, with nothing sent (nothing can go), and is reported so, as a pipe's peer gone quiet is.
        for (peer, at) in lost {
            if self.ep.path(&peer) == Some(&Path::Direct(Hop::Rtc(at))) {
                self.ep.abandon(&peer);
                self.take_event(now, wall, Ev::Disconnected { peer, reason: DisconnectReason::Timeout }, &[]);
            }
        }
    }

    /// The room layer's timers: links whose proof is overdue close, and room frames waiting for send window go.
    pub(super) fn room_timers(&mut self, now: peerwire_core::Instant, _wall: Instant) {
        let Some(b) = self.browsers.as_mut() else { return };
        for peer in b.members.overdue(now) {
            b.members.gone(&peer);
            b.closing.push((peer, b.rtcs.at_of(&peer)));
            self.ep.disconnect(&peer);
            self.status.remove(&peer);
        }
        let mut blocked: Vec<PeerId> = Vec::new();
        let mut left = VecDeque::new();
        while let Some((peer, f)) = b.outbox.pop_front() {
            if blocked.contains(&peer) || !b.members.has(&peer) {
                if b.members.has(&peer) {
                    left.push_back((peer, f));
                }
                continue;
            }
            if self.ep.send(&peer, Lane::Reliable, &f) == Err(SendError::WouldBlock) {
                blocked.push(peer);
                left.push_back((peer, f));
            }
        }
        b.outbox = left;
    }

    /// Sends what the connections have for the socket and the carrier, and ends those closing.
    pub(super) fn flush_browsers(&mut self) {
        let Some(b) = self.browsers.as_mut() else { return };
        for (peer, at) in std::mem::take(&mut b.closing) {
            if b.rtcs.at_of(&peer) == at {
                b.rtcs.close(&peer);
            }
        }
        let v6 = self.local.is_ipv6();
        while let Some((dst, bytes)) = b.rtcs.poll_udp() {
            #[cfg(any(test, feature = "testing"))]
            if self.udp_off {
                continue;
            }
            let dst = match dst {
                SocketAddr::V4(a) if v6 => SocketAddr::new(a.ip().to_ipv6_mapped().into(), a.port()),
                dst => dst,
            };
            let _ = self.sock.send_to(&bytes, dst); // a full buffer drops it, as UDP may
        }
        let Some(sig) = self.signaling.as_mut() else {
            while b.rtcs.poll_signal().is_some() {}
            return;
        };
        while let Some((peer, body)) = b.rtcs.poll_signal() {
            if let Some(sealed) = self.trav.seal_to(&peer, &body) {
                sig.publish(Topic::peer(&peer), &sealed);
            }
        }
    }

    /// How long until the browsers' next timer, if one is armed.
    pub(super) fn browsers_timeout(&self, wall: Instant, now: peerwire_core::Instant) -> Option<Duration> {
        let b = self.browsers.as_ref()?;
        let rtc = b.rtcs.poll_timeout().map(|t| t.saturating_duration_since(wall));
        let room = b.members.poll_timeout().map(|t| t.saturating_duration_since(now));
        let outbox = (!b.outbox.is_empty()).then_some(Duration::from_millis(2));
        [rtc, room, outbox].into_iter().flatten().min()
    }

    /// An event of the core's about a browser, through room.js's room layer: its link's `Connected` once it has proved
    /// the room key and said its presence (it has joined), its messages once it has (the room layer's own taken
    /// here), and the rest only while it has. `None`: nothing for the app. Others' events pass as they are.
    pub(super) fn room_event(&mut self, now: peerwire_core::Instant, e: Ev<Addr>) -> Option<Ev<Addr>> {
        let peer = match &e {
            Ev::ConnectFailed { peer } => Some(*peer),
            e => about(e),
        };
        let me = self.ep.peer_id();
        // Without browsers on, or about a peer that isn't one: the event as it is.
        let (Some(peer), Some(b)) = (peer, self.browsers.as_mut()) else { return Some(e) };
        if !self.trav.is_browser(&peer) && !b.members.has(&peer) {
            return Some(e);
        }
        match e {
            Ev::Connected { peer, initiator, .. } => {
                // Out of the room (it was left), a browser's link has no room to prove: it closes.
                let Some(proof) = self.trav.room_pair(room::LINK, &me, &peer) else {
                    b.closing.push((peer, b.rtcs.at_of(&peer)));
                    self.ep.disconnect(&peer);
                    self.status.remove(&peer);
                    return None;
                };
                let f = b.members.linked(now, peer, initiator, &proof);
                self.send_room(peer, f);
                None
            }
            Ev::Message { peer, lane, data } => {
                let trav = &self.trav;
                let (step, frames) =
                    b.members.message(&peer, lane, &data, |p| trav.room_pair_ok(room::LINK, &peer, &me, p));
                for f in frames {
                    self.send_room(peer, f);
                }
                match step {
                    Step::Quiet => None,
                    Step::App => Some(Ev::Message { peer, lane, data }),
                    Step::Joined => {
                        let initiator = self.browsers.as_ref().is_some_and(|b| b.members.initiator(&peer));
                        Some(Ev::Connected { peer, path: self.ep.path(&peer)?.clone(), initiator })
                    }
                    Step::Close { left } => {
                        let b = self.browsers.as_mut()?;
                        let joined = b.members.gone(&peer);
                        b.closing.push((peer, b.rtcs.at_of(&peer)));
                        self.ep.disconnect(&peer);
                        self.status.remove(&peer);
                        let reason = if left { DisconnectReason::ClosedByPeer } else { DisconnectReason::Timeout };
                        joined.then_some(Ev::Disconnected { peer, reason })
                    }
                }
            }
            Ev::Disconnected { peer, reason } => {
                let joined = b.members.gone(&peer);
                if reason != DisconnectReason::Replaced {
                    b.closing.push((peer, b.rtcs.at_of(&peer)));
                }
                joined.then_some(Ev::Disconnected { peer, reason })
            }
            Ev::ConnectFailed { peer } => {
                b.closing.push((peer, b.rtcs.at_of(&peer)));
                None
            }
            e => b.members.joined(&peer).then_some(e),
        }
    }

    /// A room frame to `peer` on the reliable lane, after any waiting for it.
    fn send_room(&mut self, peer: PeerId, f: Vec<u8>) {
        let Some(b) = self.browsers.as_mut() else { return };
        if b.outbox.iter().any(|(p, _)| *p == peer)
            || self.ep.send(&peer, Lane::Reliable, &f) == Err(SendError::WouldBlock)
        {
            if b.outbox.len() >= MAX_OUTBOX {
                b.outbox.pop_front();
            }
            b.outbox.push_back((peer, f));
        }
    }

    /// Before the node closes the link to `peer` ([`disconnect`](Node::disconnect)): a browser is told goodbye on the
    /// unreliable lane, as room.js leaves (its link's sealed close follows), and its connection ends once that has gone.
    pub(super) fn bye(&mut self, peer: &PeerId) {
        let Some(b) = self.browsers.as_mut() else { return };
        if !b.members.has(peer) {
            return;
        }
        let _ = self.ep.send(peer, Lane::Unreliable, &[room::BYE]);
        b.members.gone(peer);
        b.closing.push((*peer, b.rtcs.at_of(peer)));
    }

    /// Whether `peer` is a browser (feature `webrtc`): a `joinRoom` page in this node's room. Its link runs room.js's
    /// room layer: the app sees it connected only once it has proved the room key and said its presence
    /// ([`presence`](Node::presence)), and its messages are room.js's frames, the tag byte first (0 bytes, 1 text,
    /// 2 JSON), as [`files`](crate::files) frames its own; send it the same ([`room_text`](Node::room_text)).
    pub fn is_browser(&self, peer: &PeerId) -> bool {
        self.trav.is_browser(peer) || self.browsers.as_ref().is_some_and(|b| b.members.has(peer))
    }

    /// A browser's presence, the JSON it said (room.js `presence`), once it has joined (feature `webrtc`).
    pub fn presence(&self, peer: &PeerId) -> Option<&str> {
        self.browsers.as_ref()?.members.presence(peer)
    }

    /// This node's presence for browsers, JSON (room.js `setPresence`): sent to every browser linked now and to
    /// those that link later. `{}` until set. [`ErrorKind::InvalidInput`] if it isn't JSON, or is over the reliable
    /// lane's largest message (feature `webrtc`; without [`Builder::webrtc`](super::Builder::webrtc) it is kept for
    /// nothing).
    pub fn set_presence(&mut self, json: &str) -> io::Result<()> {
        let max = self.ep.config().max_message;
        if room::json_text(json.as_bytes()).is_none() || 1 + json.len() > max {
            return Err(io::Error::new(ErrorKind::InvalidInput, "presence must be JSON, within the reliable lane"));
        }
        let Some(b) = self.browsers.as_mut() else { return Ok(()) };
        let (f, peers) = b.members.set_presence(json);
        for p in peers {
            self.send_room(p, f.clone());
        }
        Ok(())
    }

    /// The browsers that have joined (feature `webrtc`).
    pub fn browsers(&self) -> Vec<PeerId> {
        self.browsers.as_ref().map_or_else(Vec::new, |b| b.members.members().copied().collect())
    }

    /// Text as a room message to a browser, as room.js frames a string: tag 1, then the UTF-8 (feature `webrtc`).
    pub fn room_text(text: &str) -> Vec<u8> {
        room::frame(room::TEXT, text.as_bytes())
    }

    /// A browser's room message as the app gets it ([`Event::Message`](crate::Event)): its text, if it is text
    /// (tag 1), or JSON (tag 2), as room.js reads it (invalid UTF-8 as U+FFFD) (feature `webrtc`).
    pub fn room_message_text(data: &[u8]) -> Option<String> {
        match data.split_first()? {
            (&(room::TEXT | room::JSON), body) => Some(String::from_utf8_lossy(body).into_owned()),
            _ => None,
        }
    }

    /// WebRTC connections to browsers, and how many have an open DataChannel (feature `webrtc`).
    pub fn webrtc_counts(&self) -> (usize, usize) {
        self.browsers.as_ref().map_or((0, 0), |b| b.rtcs.counts())
    }
}

/// The local address a datagram from `source` reached the socket bound to `bound` at: `bound` itself, unless it is
/// any interface's; then the address the kernel routes `source` from (a UDP `connect`, which sends nothing),
/// remembered. ICE needs it: str0m takes a check only at one of its candidates.
fn local_for(bound: SocketAddr, routes: &mut HashMap<IpAddr, IpAddr>, source: SocketAddr) -> SocketAddr {
    if !bound.ip().is_unspecified() {
        return SocketAddr::new(bound.ip().to_canonical(), bound.port());
    }
    let ip = source.ip().to_canonical();
    if routes.len() >= MAX_ROUTES && !routes.contains_key(&ip) {
        routes.clear();
    }
    let local = *routes.entry(ip).or_insert_with(|| route_to(ip).unwrap_or(ip));
    SocketAddr::new(local, bound.port())
}

/// The local address the kernel sends to `ip` from.
fn route_to(ip: IpAddr) -> Option<IpAddr> {
    if ip.is_loopback() {
        return Some(ip);
    }
    let any: IpAddr = if ip.is_ipv4() { Ipv4Addr::UNSPECIFIED.into() } else { Ipv6Addr::UNSPECIFIED.into() };
    let s = UdpSocket::bind((any, 0)).ok()?;
    s.connect((ip, 9)).ok()?;
    let l = s.local_addr().ok()?.ip();
    (!l.is_unspecified()).then_some(l.to_canonical())
}
