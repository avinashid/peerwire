use super::room::{self, Members, Step};
use super::*;
use peerwire_core::Lane;

fn id(n: u8) -> PeerId {
    PeerId([n; 32])
}

/// A room that takes every offer whose proof is `[0xab; 32]`, and counts its admissions.
struct Open {
    admitted: usize,
}

impl Room for Open {
    fn offer_proved(&self, _: &PeerId, proof: &[u8]) -> bool {
        proof == [0xab; 32]
    }
    fn admit(&mut self, _: &PeerId) -> bool {
        self.admitted += 1;
        true
    }
}

/// Two connection sets on loopback addresses, `a` (id 1, at `.1`) dialling `b` (id 2, at `.2`), their packets and
/// signals handed across in memory: runs until `done` or 5 s pass.
struct Pair {
    a: Rtcs,
    b: Rtcs,
    at: [SocketAddr; 2],
    room: Open,
    now: Instant,
    /// The UDP datagrams each side sent to the other: their first byte and length.
    wire: [Vec<(u8, usize)>; 2],
}

impl Pair {
    fn new() -> Pair {
        let at: [SocketAddr; 2] = ["127.0.0.1:41001".parse().unwrap(), "127.0.0.2:41002".parse().unwrap()];
        Pair {
            a: Rtcs::new(vec![at[0]]),
            b: Rtcs::new(vec![at[1]]),
            at,
            room: Open { admitted: 0 },
            now: Instant::now(),
            wire: [Vec::new(), Vec::new()],
        }
    }

    fn step(&mut self) {
        self.now += Duration::from_millis(5);
        let now = self.now;
        while let Some((to, body)) = self.a.poll_signal() {
            assert_eq!(to, id(2));
            self.b.signal(now, id(1), &body, &mut self.room);
        }
        while let Some((to, body)) = self.b.poll_signal() {
            assert_eq!(to, id(1));
            self.a.signal(now, id(2), &body, &mut self.room);
        }
        while let Some((dst, d)) = self.a.poll_udp() {
            if dst == self.at[1] {
                self.wire[0].push((d[0], d.len()));
                self.b.receive(now, self.at[0], || self.at[1], &d);
            }
        }
        while let Some((dst, d)) = self.b.poll_udp() {
            if dst == self.at[0] {
                self.wire[1].push((d[0], d.len()));
                self.a.receive(now, self.at[1], || self.at[0], &d);
            }
        }
        self.a.handle_timeout(now);
        self.b.handle_timeout(now);
    }
}

/// `a` dials `b` and both channels open: the addresses naming the connection on each side.
fn open(p: &mut Pair) -> (SocketAddr, SocketAddr) {
    assert!(p.a.dial(p.now, id(2), &[0xab; 32]));
    let (mut opened_a, mut opened_b) = (None, None);
    for _ in 0..1000 {
        p.step();
        if let Some(o) = p.a.poll_opened() {
            opened_a = Some(o);
        }
        if let Some(o) = p.b.poll_opened() {
            opened_b = Some(o);
        }
        if opened_a.is_some() && opened_b.is_some() {
            break;
        }
    }
    let (Some((pa, at_a, true)), Some((pb, at_b, false))) = (opened_a, opened_b) else {
        panic!("not open: {opened_a:?} {opened_b:?}")
    };
    assert_eq!((pa, pb), (id(2), id(1)));
    (at_a, at_b)
}

/// A datagram each way over the open connection, `a`'s at `at_a` and `b`'s at `at_b`, each arriving whole.
fn carries(p: &mut Pair, at_a: SocketAddr, at_b: SocketAddr) {
    p.a.send(&at_a, b"from a");
    p.b.send(&at_b, &[9; MAX_DATAGRAM]);
    let (mut got_a, mut got_b) = (Vec::new(), Vec::new());
    for _ in 0..200 {
        p.step();
        got_a.extend(std::iter::from_fn(|| p.a.poll_datagram()));
        got_b.extend(std::iter::from_fn(|| p.b.poll_datagram()));
        if !got_a.is_empty() && !got_b.is_empty() {
            break;
        }
    }
    assert_eq!(got_b, [(at_b, id(1), b"from a".to_vec())]);
    assert_eq!(got_a, [(at_a, id(2), vec![9; MAX_DATAGRAM])]);
}

#[test]
fn a_dial_opens_the_channel_and_carries_datagrams_both_ways() {
    let mut p = Pair::new();
    let (at_a, at_b) = open(&mut p);
    assert!(!p.a.dial(p.now, id(2), &[0xab; 32]), "one connection per peer");
    assert_eq!(p.room.admitted, 1);
    assert_eq!(p.a.counts(), (1, 1));
    carries(&mut p, at_a, at_b);
    assert!(!p.a.relayed(&id(2)), "host candidates on both sides");
    // Closed on one side, without a word (the node's core says its own close first): the other's ICE sees it go.
    p.a.close(&id(2));
    for i in 0..20_000 {
        p.step();
        if p.b.poll_closed().map(|(peer, _)| peer) == Some(id(1)) {
            println!("closed after {} ms", i * 5);
            assert_eq!(p.b.counts(), (0, 0));
            return;
        }
    }
    panic!("b never saw a close");
}

#[test]
fn offers_need_the_room_proof_and_signals_a_connection() {
    let mut p = Pair::new();
    p.a.dial(p.now, id(2), &[8; 32]); // a wrong proof
    let (_, body) = p.a.poll_signal().unwrap();
    p.b.signal(p.now, id(1), &body, &mut p.room);
    assert_eq!((p.b.counts(), p.room.admitted, p.b.refused), ((0, 0), 0, 1), "unproven: no connection");
    assert!(p.b.poll_signal().is_none(), "and no answer, which would give our addresses");
    let m: Value = serde_json::from_slice(&body).unwrap();
    for bad in [
        json!({"kind": "offer", "sdp": m["sdp"]}),                       // no proof
        json!({"kind": "offer", "sdp": m["sdp"], "p": "ab".repeat(31)}), // short
        json!({"kind": "offer", "sdp": m["sdp"], "p": "ab".repeat(32).to_uppercase()}), // not lowercase
        json!({"kind": "offer", "sdp": {"type": "answer", "sdp": m["sdp"]["sdp"]}, "p": "ab".repeat(32)}),
        json!({"kind": "offer", "sdp": {"type": "offer", "sdp": "x".repeat(MAX_SDP + 1)}, "p": "ab".repeat(32)}),
        json!({"kind": "offer", "sdp": {"type": "offer", "sdp": "v=0 nonsense"}, "p": "ab".repeat(32)}),
        json!({"kind": "answer", "sdp": m["sdp"]}), // no connection to answer
        json!({"kind": "ice-batch", "c": []}),
        json!({"kind": "nonsense"}),
        json!([1, 2]),
    ] {
        p.b.signal(p.now, id(1), bad.to_string().as_bytes(), &mut p.room);
    }
    p.b.signal(p.now, id(1), b"{not json", &mut p.room);
    assert_eq!((p.b.counts(), p.room.admitted, p.b.refused), ((0, 0), 0, 12));
    let good = json!({"kind": "offer", "sdp": m["sdp"], "p": "ab".repeat(32)});
    p.b.signal(p.now, id(1), good.to_string().as_bytes(), &mut p.room);
    assert_eq!((p.b.counts(), p.room.admitted), ((1, 0), 1), "proved: answered");
    assert!(p.b.poll_signal().is_some());
    // A copy of it: the same answer again, within the peer's budget; no second connection.
    for _ in 0..10 {
        p.b.signal(p.now, id(1), good.to_string().as_bytes(), &mut p.room);
    }
    let copies = std::iter::from_fn(|| p.b.poll_signal()).count();
    assert_eq!((copies, p.b.counts().0, p.room.admitted), (PUBLISH_BURST as usize - 1, 1, 1));
    // Too many candidates in a batch: dropped whole.
    let c = json!({"candidate": "candidate:1 1 udp 2113937151 127.0.0.9 5000 typ host", "sdpMid": "0"});
    let big = json!({"kind": "ice-batch", "c": vec![c; MAX_ICE_BATCH + 1]});
    let before = p.b.refused;
    p.b.signal(p.now, id(1), big.to_string().as_bytes(), &mut p.room);
    assert_eq!(p.b.refused, before + 1);
}

/// NB162-2: a page's offer marked `restart` on an open connection restarts ICE on it only with new ICE credentials,
/// at most once per `REPLACE_AFTER`, its candidates counted afresh. One with the same credentials is no restart to
/// str0m, which would add its candidates to the connection's, uncapped, and scan them all for each one added. A real
/// restart (str0m's own on the dialling side, new credentials) is answered on the same connection, which carries the
/// core's datagrams after it; a copy of it gets the answer again.
#[test]
fn a_restart_offer_needs_new_ice_credentials_and_comes_at_most_once_per_replace_after() {
    let mut p = Pair::new();
    let (at_a, at_b) = open(&mut p);
    let restart = |sdp: &str| {
        let m = json!({"kind": "offer", "sdp": {"type": "offer", "sdp": sdp}, "p": "ab".repeat(32), "restart": true});
        m.to_string().into_bytes()
    };
    let first = p.b.conns[&id(1)].offer_sdp.clone().unwrap();
    let ufrag = first.lines().find_map(|l| l.strip_prefix("a=ice-ufrag:")).unwrap().to_string();
    let (made, held) = (p.b.conns[&id(1)].made, p.b.conns[&id(1)].candidates);
    // New credentials, but within REPLACE_AFTER of the connection's offer: refused.
    assert!(p.now < made + REPLACE_AFTER);
    p.b.signal(p.now, id(1), &restart(&first.replacen("a=ice-ufrag:", "a=ice-ufrag:zz", 1)), &mut p.room);
    assert_eq!((p.b.poll_signal(), p.b.refused), (None, 1));
    // The dialler's offer again, the same credentials, 64 new host candidates each time, before REPLACE_AFTER and
    // after: refused, unanswered, its candidates not counted.
    for i in 0..200 {
        if i == 100 {
            while p.now < made + REPLACE_AFTER {
                p.step();
            }
        }
        let more: String =
            (0..64).map(|k| format!("a=candidate:{k} 1 udp 2113937151 10.0.{i}.{k} 9 typ host\r\n")).collect();
        let before = p.b.refused;
        p.b.signal(p.now, id(1), &restart(&format!("{first}{more}")), &mut p.room);
        assert_eq!(p.b.refused, before + 1, "same ufrag, offer {i}");
    }
    assert!(p.b.poll_signal().is_none(), "no answer");
    assert_eq!((p.b.counts(), p.b.conns[&id(1)].candidates), ((1, 1), held));
    // A new ufrag in the session part without a password there: str0m reads the media section's credentials, the
    // same, and so does the check.
    let t = first.find("\r\nt=").unwrap() + 2;
    let end = t + first[t..].find("\r\n").unwrap() + 2;
    let session = format!("{}a=ice-ufrag:{ufrag}x\r\n{}", &first[..end], &first[end..]);
    assert_eq!(ice_ufrag(&SdpOffer::from_sdp_string(&session).unwrap()).as_deref(), Some(ufrag.as_str()));
    p.b.signal(p.now, id(1), &restart(&session), &mut p.room);
    assert!(p.b.poll_signal().is_none());
    // A real restart, from a's own Rtc, past REPLACE_AFTER: answered on the same connection, its candidates counted
    // afresh (a's one host candidate).
    let c = p.a.conns.get_mut(&id(2)).unwrap();
    let mut api = c.rtc.sdp_api();
    api.ice_restart(true);
    let (offer, pending) = api.apply().unwrap();
    c.pending = Some(pending);
    let offer = offer.to_sdp_string();
    p.b.signal(p.now, id(1), &restart(&offer), &mut p.room);
    let c = &p.b.conns[&id(1)];
    assert!(c.ufrag.as_deref().is_some_and(|u| u != ufrag) && (c.made, c.candidates) == (p.now, 1));
    p.step(); // the answer, to a
    assert!(p.a.conns[&id(2)].pending.is_none(), "a took the answer");
    p.b.signal(p.now, id(1), &restart(&offer), &mut p.room);
    assert!(p.b.poll_signal().is_some(), "a copy gets the answer again");
    // Another with new credentials again, within REPLACE_AFTER of that one: refused.
    let before = p.b.refused;
    p.b.signal(p.now, id(1), &restart(&offer.replacen("a=ice-ufrag:", "a=ice-ufrag:yy", 1)), &mut p.room);
    assert_eq!((p.b.poll_signal(), p.b.refused), (None, before + 1));
    // ICE checks the pair again, and the same connection carries the core's datagrams.
    for _ in 0..200 {
        p.step();
    }
    carries(&mut p, at_a, at_b);
    assert_eq!((p.a.counts(), p.b.counts(), p.room.admitted), ((1, 1), (1, 1), 1), "the same connection");
}

/// NB162-2, a failed try: str0m takes a restart offer's new ICE credentials and its candidates before it can still
/// refuse it (here for having no `a=group`). The try counts all the same, and ends the half-restarted connection;
/// another with the same credentials and 64 more candidates is refused before str0m, unanswered, and no ICE check
/// goes to any of the offers' candidates.
#[test]
fn a_restart_offer_that_str0m_refuses_still_counts_and_ends_the_connection() {
    let mut p = Pair::new();
    let (_, at_b) = open(&mut p);
    let first = p.b.conns[&id(1)].offer_sdp.clone().unwrap();
    while p.now < p.b.conns[&id(1)].made + REPLACE_AFTER {
        p.step();
    }
    assert!(first.contains("\r\na=group:"));
    // New credentials, no a=group, and 64 host candidates on 10.0.{i}.0/24.
    let bad = |i: u8| {
        let lines = first.replacen("a=ice-ufrag:", "a=ice-ufrag:zz", 1);
        let mut sdp: String =
            lines.lines().filter(|l| !l.starts_with("a=group:")).map(|l| l.to_string() + "\r\n").collect();
        sdp.extend((0..64).map(|k| format!("a=candidate:{k} 1 udp 2113937151 10.0.{i}.{k} 9 typ host\r\n")));
        let m = json!({"kind": "offer", "sdp": {"type": "offer", "sdp": sdp}, "p": "ab".repeat(32), "restart": true});
        m.to_string().into_bytes()
    };
    let ours = |p: &mut Pair| p.b.conns.get_mut(&id(1)).map(|c| c.rtc.direct_api().local_ice_credentials());
    let (before, creds) = (p.b.refused, ours(&mut p));
    p.b.signal(p.now, id(1), &bad(1), &mut p.room);
    assert_eq!((p.b.poll_signal(), p.b.refused), (None, before + 1), "refused by str0m");
    assert_ne!(ours(&mut p), creds, "after it restarted ICE, with new credentials of ours");
    let half = p.b.conns.get(&id(1));
    assert!(half.is_none_or(|c| c.dead && c.made == p.now), "the try counts, and the half-restarted connection ends");
    p.b.signal(p.now, id(1), &bad(2), &mut p.room);
    assert_eq!((p.b.poll_signal(), p.b.refused), (None, before + 2), "the same credentials: refused");
    // For 5 s, b alone: nothing goes to the offers' candidates (a half-restarted connection would check them all).
    let (a_at, mut checked) = (p.at[0], Vec::new());
    for _ in 0..1000 {
        p.now += Duration::from_millis(5);
        p.b.handle_timeout(p.now);
        checked.extend(std::iter::from_fn(|| p.b.poll_udp()).map(|(dst, _)| dst).filter(|dst| *dst != a_at));
    }
    assert_eq!(checked, [], "no ICE check to the offers' candidates");
    assert_eq!((p.b.poll_closed(), p.b.counts(), p.room.admitted), (Some((id(1), at_b)), (0, 0), 1), "closed");
}

#[test]
fn sdp_candidates_naming_no_ip_are_dropped_and_relays_noted() {
    let sdp = "v=0\r\na=candidate:1 1 udp 2113937151 5d2c1d3a-aaaa.local 54321 typ host\r\n\
               a=candidate:2 1 udp 2113937151 192.0.2.7 54321 typ host\r\n\
               a=candidate:3 1 udp 33562623 198.51.100.9 3478 typ relay raddr 0.0.0.0 rport 0\r\n";
    let clean = clean_sdp(sdp);
    assert!(!clean.contains(".local") && clean.contains("192.0.2.7") && clean.starts_with("v=0\r\n"));
    assert_eq!(candidate_lines(sdp), 2);
    assert_eq!(relay_addrs(sdp), HashSet::from(["198.51.100.9:3478".parse().unwrap()]));
    let many: String =
        (0..MAX_CANDIDATES + 5).map(|i| format!("a=candidate:{i} 1 udp 1 192.0.2.{} 1 typ host\n", i % 250)).collect();
    assert_eq!(clean_sdp(&many).lines().count(), MAX_CANDIDATES);
    assert_eq!(unhex32(&"ab".repeat(32)), Some([0xab; 32]));
    assert_eq!(unhex32(&"AB".repeat(32)), None);
}

// ---------- the room layer ----------

const PROOF: [u8; 32] = [5; 32];

fn joined_member(m: &mut Members, peer: PeerId) {
    let now = peerwire_core::Instant::ZERO;
    assert_eq!(m.linked(now, peer, true, &[1; 32]), room::frame(room::PROOF, &[1; 32]));
    let (step, out) = m.message(&peer, Lane::Reliable, &room::frame(room::PROOF, &PROOF), |p| p == PROOF);
    assert_eq!((step, out), (Step::Quiet, vec![room::frame(room::PRESENCE, b"{}")]), "our presence once proven");
    let (step, _) = m.message(&peer, Lane::Reliable, &room::frame(room::PRESENCE, br#"{"name":"web"}"#), |_| false);
    assert_eq!(step, Step::Joined);
}

#[test]
fn a_browser_joins_after_its_proof_and_presence() {
    let mut m = Members::new();
    let (peer, now) = (id(3), peerwire_core::Instant::ZERO);
    m.linked(now, peer, false, &[1; 32]);
    // Before its proof: nothing is read but a proof or a goodbye.
    for f in [room::frame(room::TEXT, b"hi"), room::frame(room::PRESENCE, b"{}"), room::frame(room::DOC, b"{}")] {
        assert_eq!(m.message(&peer, Lane::Reliable, &f, |_| unreachable!()), (Step::Quiet, Vec::new()));
    }
    assert_eq!(m.overdue(now + room::PROOF_WITHIN), [peer], "unproven past the deadline");
    // A wrong proof closes the link; one of another length too.
    let bad = m.message(&peer, Lane::Reliable, &room::frame(room::PROOF, &[0; 32]), |p| p == PROOF);
    assert_eq!(bad.0, Step::Close { left: false });
    let short = m.message(&peer, Lane::Reliable, &room::frame(room::PROOF, &[5; 31]), |_| true);
    assert_eq!(short.0, Step::Close { left: false });
    let mut m = Members::new();
    joined_member(&mut m, peer);
    assert_eq!((m.presence(&peer), m.initiator(&peer)), (Some(r#"{"name":"web"}"#), true));
    assert!(m.overdue(now + room::PROOF_WITHIN * 2).is_empty());
    // The app's messages pass as they are; JSON only if it parses; the room layer's own stay here.
    let text = room::frame(room::TEXT, b"hello");
    assert_eq!(m.message(&peer, Lane::Unreliable, &text, |_| false).0, Step::App);
    assert_eq!(m.message(&peer, Lane::Reliable, &room::frame(room::BYTES, &[0, 1]), |_| false).0, Step::App);
    assert_eq!(m.message(&peer, Lane::Reliable, &room::frame(room::JSON, b"[1,2]"), |_| false).0, Step::App);
    assert_eq!(m.message(&peer, Lane::Reliable, &room::frame(room::JSON, b"[1,"), |_| false).0, Step::Quiet);
    for tag in [6u8, 8, 9, 10, 11, 200] {
        assert_eq!(m.message(&peer, Lane::Reliable, &[tag, 1, 2], |_| false), (Step::Quiet, Vec::new()));
    }
    // A new presence updates it, with no second join; one that isn't JSON is dropped.
    assert_eq!(
        m.message(&peer, Lane::Reliable, &room::frame(room::PRESENCE, br#"{"name":"w2"}"#), |_| false).0,
        Step::Quiet
    );
    m.message(&peer, Lane::Reliable, &room::frame(room::PRESENCE, b"nope"), |_| false);
    assert_eq!(m.presence(&peer), Some(r#"{"name":"w2"}"#));
    // A doc's ask is answered with nothing, as room.js answers for a doc it hasn't open.
    let ask = room::frame(room::DOC, br#"{"d":"notes","t":2,"q":3,"x":1}"#);
    let (_, out) = m.message(&peer, Lane::Reliable, &ask, |_| false);
    let ans: Value = serde_json::from_slice(&out[0][1..]).unwrap();
    assert_eq!(
        (out[0][0], ans),
        (room::DOC, json!({"d": "notes", "t": 3, "ws": [], "q": 3, "i": 0, "n": 0, "end": 1}))
    );
    for bad in [
        &br#"{"d":"","t":2,"q":3}"#[..],
        br#"{"d":"n","t":1,"q":3}"#,
        br#"{"d":"n","t":2,"q":0}"#,
        br#"{"d":"n","t":2,"q":1.5}"#,
    ] {
        assert!(m.message(&peer, Lane::Reliable, &room::frame(room::DOC, bad), |_| false).1.is_empty());
    }
    let long = format!(r#"{{"d":"{}","t":2,"q":1}}"#, "é".repeat(129));
    assert!(m.message(&peer, Lane::Reliable, &room::frame(room::DOC, long.as_bytes()), |_| false).1.is_empty());
    // Our presence again goes to every proven peer; a goodbye closes the link as one it closed.
    let (f, to) = m.set_presence(r#"{"name":"native"}"#);
    assert_eq!((f, to), (room::frame(room::PRESENCE, br#"{"name":"native"}"#), vec![peer]));
    assert_eq!(m.message(&peer, Lane::Unreliable, &[room::BYE], |_| false).0, Step::Close { left: true });
    assert!(m.gone(&peer) && !m.has(&peer) && !m.gone(&peer));
}

#[test]
fn json_is_read_as_room_js_reads_it() {
    assert_eq!(room::json_text("\u{feff}{\"a\":1}".as_bytes()).as_deref(), Some("{\"a\":1}"));
    assert_eq!(room::json_text(b"\"\xff\"").as_deref(), Some("\"\u{fffd}\""));
    assert_eq!(room::json_text(b"{} x"), None);
    assert_eq!(room::json_text(b""), None);
}

// ---------- the MTU (NB-168) ----------

type CoreEnd = peerwire_core::Endpoint<SocketAddr>;

/// Hands what `from` (at `src`) sends to `to` until it has nothing more, keeping a copy in `out`.
fn hand(now: peerwire_core::Instant, from: &mut CoreEnd, to: &mut CoreEnd, src: SocketAddr, out: &mut Vec<Vec<u8>>) {
    from.handle_timeout(now);
    while let Some(t) = from.poll_transmit(now) {
        to.handle_datagram(now, src, &t.payload);
        out.push(t.payload);
    }
}

/// The datagrams of a bulk transfer on the core's reliable lane, as a big file goes: one endpoint sends another
/// 256 KiB, in memory. Both ways: the handshake and the receiver's ACKs too.
fn core_bulk_datagrams() -> Vec<Vec<u8>> {
    use peerwire_core::{Config, Endpoint, Event, Identity};
    let (ia, ib) = (Identity::from_secret_bytes(&[1; 32]), Identity::from_secret_bytes(&[2; 32]));
    let at: [SocketAddr; 2] = ["127.0.0.1:1".parse().unwrap(), "127.0.0.1:2".parse().unwrap()];
    let mut a: CoreEnd = Endpoint::new(&ia, Config::default(), [3; 32]);
    let mut b: CoreEnd = Endpoint::new(&ib, Config::default(), [4; 32]);
    let mut now = peerwire_core::Instant::ZERO;
    a.connect(now, at[1], ib.peer_id()).unwrap();
    let (msg, mut out, mut sent) = (vec![7u8; 256 << 10], Vec::new(), false);
    for _ in 0..60_000 {
        now = now + Duration::from_millis(1);
        hand(now, &mut a, &mut b, at[0], &mut out);
        hand(now, &mut b, &mut a, at[1], &mut out);
        if !sent && a.is_connected(&ib.peer_id()) {
            a.send(&ib.peer_id(), Lane::Reliable, &msg).unwrap();
            sent = true;
        }
        while let Some(e) = b.poll_event() {
            if matches!(e, Event::Message { data, .. } if data == msg) {
                return out;
            }
        }
    }
    panic!("the transfer didn't end");
}

/// Sends `datagrams` from `a` to `b`, one per step, over a connection whose two ends have str0m MTU target `mtu`, and
/// checks each arrives whole: how many DTLS records of application data (first byte 23, RFC 7983) `a` sent for them.
fn records_for(mtu: usize, datagrams: &[Vec<u8>]) -> (usize, Pair) {
    let mut p = Pair::new();
    (p.a.mtu, p.b.mtu) = (mtu, mtu);
    let (at_a, _) = open(&mut p);
    for _ in 0..20 {
        p.step();
    }
    let records = |p: &Pair| p.wire[0].iter().filter(|(first, _)| *first == 23).count();
    let (before, mut got) = (records(&p), Vec::new());
    for d in datagrams {
        p.a.send(&at_a, d);
        p.step();
        got.extend(std::iter::from_fn(|| p.b.poll_datagram()).map(|(_, _, d)| d));
    }
    assert!(got == datagrams, "each arrives whole, at MTU {mtu}");
    (records(&p) - before, p)
}

/// NB-168: a datagram of a bulk transfer, one full fragment, goes in one SCTP packet, one DTLS record and one UDP
/// datagram, as a browser sends it; str0m's default MTU (1150) split it in two, and so does one byte less than
/// `RTC_MTU`, the smallest that doesn't. Nothing str0m sends here, its handshake included, is over the core's own
/// limit (a burst of tiny datagrams, bundled, can be: see `RTC_MTU`).
#[test]
fn a_bulk_transfers_datagram_goes_in_one_packet() {
    // A full fragment's datagram, the smallest over MAX_UNRELIABLE: most of the transfer's. A few carry an ACK too.
    let bulk = core_bulk_datagrams();
    let size = bulk.iter().map(Vec::len).filter(|len| *len > peerwire_core::MAX_UNRELIABLE).min().unwrap();
    let full: Vec<Vec<u8>> = bulk.iter().filter(|d| d.len() == size).cloned().collect();
    assert!(full.len() * 100 > bulk.len() * 95, "{} of {} datagrams of {size} B", full.len(), bulk.len());
    assert_eq!(size, RTC_MTU - 48 - 40, "the room str0m leaves a DATA chunk at RTC_MTU");

    let (records, p) = records_for(RTC_MTU, &full);
    assert_eq!(records, full.len(), "one record, one packet, for each");
    let largest = p.wire.iter().flatten().map(|(_, len)| *len).max().unwrap();
    assert!(largest <= MAX_DATAGRAM, "the largest UDP datagram either end sent: {largest} B");
    let (records, _) = records_for(RTC_MTU - 1, &full[..20]);
    assert_eq!(records, 40, "a byte less and each goes in two");
}

/// NB-168, what the larger target costs: str0m bundles the datagrams queued behind SCTP's congestion window, and its
/// packer counts neither the last chunk's header nor any chunk's padding. A burst of 3,000 datagrams of 33 B (3 B of
/// padding each) goes in packets past the core's limit at `RTC_MTU` (1245 B), where str0m's default held it to 1193 B,
/// but within the bound worked out in docs/concepts/native-webrtc.md (1285 B); every datagram arrives.
#[test]
fn a_burst_of_small_datagrams_passes_the_target_by_little() {
    let largest = |mtu: usize| {
        let mut p = Pair::new();
        (p.a.mtu, p.b.mtu) = (mtu, mtu);
        let (at_a, _) = open(&mut p);
        let start = p.wire[0].len();
        for _ in 0..3000 {
            p.a.send(&at_a, &[5; 33]);
        }
        for _ in 0..400 {
            p.step();
        }
        assert_eq!((std::iter::from_fn(|| p.b.poll_datagram()).count(), p.a.congested), (3000, 0));
        p.wire[0][start..].iter().map(|(_, len)| *len).max().unwrap()
    };
    assert_eq!(largest(1150), 1193, "str0m's default target");
    assert_eq!(largest(RTC_MTU), 1245);
}
