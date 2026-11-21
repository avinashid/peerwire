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
                self.b.receive(now, self.at[0], || self.at[1], &d);
            }
        }
        while let Some((dst, d)) = self.b.poll_udp() {
            if dst == self.at[0] {
                self.a.receive(now, self.at[1], || self.at[0], &d);
            }
        }
        self.a.handle_timeout(now);
        self.b.handle_timeout(now);
    }
}

#[test]
fn a_dial_opens_the_channel_and_carries_datagrams_both_ways() {
    let mut p = Pair::new();
    assert!(p.a.dial(p.now, id(2), &[0xab; 32]));
    assert!(!p.a.dial(p.now, id(2), &[0xab; 32]), "one connection per peer");
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
    assert_eq!((pa, pb, p.room.admitted), (id(2), id(1), 1));
    assert_eq!(p.a.counts(), (1, 1));
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
    assert!(!p.a.relayed(&id(2)), "host candidates on both sides");
    // Closed on one side, without a word (the node's core says its own close first): the other's ICE sees it go.
    p.a.close(&id(2));
    for i in 0..20_000 {
        p.step();
        if p.b.poll_closed().is_some() {
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
