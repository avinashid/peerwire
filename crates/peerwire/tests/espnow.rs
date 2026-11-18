//! NB-135: rooms over ESP-NOW, on stations in one process (`espnow::FakeAir`). Nodes with no carrier but an ESP-NOW
//! radio find each other, prove the room and link over it, also when frames are lost or a v1 radio meets a v2 one; a
//! forged CLOSE, sent with ids overheard on the air, closes nothing.
use peerwire::espnow::{frame, EspNow, FakeAir, Mac, Settings, Watch, BROADCAST, V1_FRAME, V2_FRAME};
use peerwire::{Config, Event, Lane, Node, PathKind, PeerId, Policy, Status};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const A: Mac = [0x02, 0x50, 0x57, 0, 0, 0x0a];
const B: Mac = [0x02, 0x50, 0x57, 0, 0, 0x0b];
const C: Mac = [0x02, 0x50, 0x57, 0, 0, 0x0c];

/// Polls every node until `done`, keeping their events; false on timeout.
fn pump(
    nodes: &mut [&mut Node],
    events: &mut Vec<Vec<Event>>,
    secs: u64,
    mut done: impl FnMut(&[&mut Node]) -> bool,
) -> bool {
    events.resize_with(nodes.len(), Vec::new);
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        for (i, n) in nodes.iter_mut().enumerate() {
            n.poll();
            events[i].extend(std::iter::from_fn(|| n.try_recv_event()));
        }
        if done(nodes) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

/// A node at `mac` on `air` whose radio takes frames of up to `max_frame` bytes, with no carrier but the radio.
fn node(air: &FakeAir, mac: Mac, max_frame: usize) -> Node {
    let radio = EspNow::new(air.ether(mac, max_frame), Settings::DEFAULT);
    Node::builder()
        .bluetooth(radio)
        .policy(Policy::new([PathKind::Bluetooth]))
        .stun_servers(Vec::<String>::new())
        .bind("127.0.0.1:0")
        .unwrap()
}

fn on_radio(n: &Node, p: &PeerId) -> bool {
    n.status(p) == Status::Connected && n.path_kind(p) == Some(PathKind::Bluetooth)
}

fn got(events: &[Event], from: PeerId, data: &[u8]) -> bool {
    events.iter().any(|e| matches!(e, Event::Message { peer, data: d, .. } if *peer == from && d == data))
}

#[test]
fn a_room_links_over_espnow_alone_v1_with_v2() {
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V2_FRAME));
    let mut stranger = node(&air, C, V2_FRAME);
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "espnow");
    b.join_room("pw-test", "espnow");
    stranger.join_room("pw-test", "another room");
    let mut ev = Vec::new();
    let started = Instant::now();
    let up = |n: &[&mut Node]| on_radio(n[0], &pb) && on_radio(n[1], &pa);
    assert!(pump(&mut [&mut a, &mut b, &mut stranger], &mut ev, 15, up), "linked over ESP-NOW");
    eprintln!("linked over ESP-NOW in {:?}", started.elapsed());

    // Both lanes, a message of many values (each up to 512 bytes, in three v1 frames).
    let big: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
    a.send(&pb, Lane::Reliable, &big).unwrap();
    b.send(&pa, Lane::Unreliable, b"back over ESP-NOW").unwrap();
    pump(&mut [&mut a, &mut b, &mut stranger], &mut ev, 3, |_| false);
    assert!(got(&ev[1], pa, &big), "3000 bytes reliable");
    assert!(got(&ev[0], pb, b"back over ESP-NOW"));
    assert!(air.heard().iter().all(|(from, to, f)| *from == C || *to == BROADCAST || f.len() <= V1_FRAME), "v1 frames");
    assert_eq!(stranger.peers().count(), 0, "another room's node never links");
}

#[test]
fn a_link_carries_its_messages_through_lost_frames() {
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "espnow-lossy");
    b.join_room("pw-test", "espnow-lossy");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa)), "linked");
    // One try in ten lost, and one in fifty arriving unacknowledged (so it comes twice): the radio tries again, the
    // receiver drops the copies, and the reliable lane resends what was lost for good.
    air.set_loss(100, 20);
    let msgs: Vec<Vec<u8>> = (0..20u8).map(|i| vec![i; 1500]).collect();
    for m in &msgs {
        a.send(&pb, Lane::Reliable, m).unwrap();
    }
    let all = |ev: &Vec<Vec<Event>>| msgs.iter().all(|m| got(&ev[1], pa, m));
    let (mut ev2, started) = (vec![Vec::new(), Vec::new()], Instant::now());
    while !all(&ev2) && started.elapsed() < Duration::from_secs(30) {
        pump(&mut [&mut a, &mut b], &mut ev2, 1, |_| false);
    }
    assert!(all(&ev2), "20 messages of 1500 bytes through the loss");
    eprintln!("20 messages of 1500 bytes through 10% loss in {:?}", started.elapsed());
    assert!(on_radio(&a, &pb) && on_radio(&b, &pa), "still linked");
}

#[test]
fn a_forged_close_closes_nothing() {
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "espnow-forged");
    b.join_room("pw-test", "espnow-forged");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa)), "linked");
    // Someone in range overheard the connection's ids, and sends each end a CLOSE as from the other.
    let ids: Vec<(Mac, u64, u64)> = air
        .heard()
        .iter()
        .filter_map(|(from, _, f)| match frame::parse(f) {
            Ok(frame::Frame::Accept { to, from: id, .. }) => Some((*from, to, id)),
            _ => None,
        })
        .collect();
    assert!(!ids.is_empty());
    for (accepter, opener_id, accepter_id) in ids {
        let opener = if accepter == A { B } else { A };
        air.inject(opener, accepter, &frame::Frame::Close { to: accepter_id }.encode());
        air.inject(accepter, opener, &frame::Frame::Close { to: opener_id }.encode());
    }
    let mut after = Vec::new();
    pump(&mut [&mut a, &mut b], &mut after, 3, |_| false);
    let churn =
        |e: &Event| matches!(e, Event::Disconnected { .. } | Event::PathChanged { .. } | Event::Connected { .. });
    assert_eq!(after.iter().flatten().filter(|e| churn(e)).count(), 0, "{after:?}");
    a.send(&pb, Lane::Reliable, b"after the forged close").unwrap();
    pump(&mut [&mut a, &mut b], &mut after, 2, |_| false);
    assert!(got(&after[1], pa, b"after the forged close"));
}

/// The accepter, the opener's id and the accepter's id of each connection opened.
type Accepts = Vec<(Mac, u64, u64)>;

/// The connection ids (opener's, accepter's) and the last data number each end took, overheard on `air`.
fn overheard(air: &FakeAir) -> (Accepts, HashMap<u64, u32>) {
    let heard = air.heard();
    let mut last: HashMap<u64, u32> = HashMap::new();
    for (_, _, f) in &heard {
        if let Ok(frame::Frame::Data { to, seq, .. }) = frame::parse(f) {
            let e = last.entry(to).or_insert(seq);
            if seq.wrapping_sub(*e) < 1 << 31 {
                *e = seq;
            }
        }
    }
    let accepts = heard
        .iter()
        .filter_map(|(from, _, f)| match frame::parse(f) {
            Ok(frame::Frame::Accept { to, from: id, .. }) => Some((*from, to, id)),
            _ => None,
        })
        .collect();
    (accepts, last)
}

/// NB135-1 (the security review's repro, inverted): someone in range overhears a link's ids and data numbers, then
/// sends each end four data frames 2^30 numbers ahead, which move each end's place there. The real frames, far behind
/// it now, move it back after four, so the next message gets through within seconds, with no link lost.
#[test]
fn sec_a_forged_resync_timeline() {
    let air = FakeAir::new();
    let ra = EspNow::new(air.ether(A, V1_FRAME), Settings::DEFAULT);
    let rb = EspNow::new(air.ether(B, V1_FRAME), Settings::DEFAULT);
    let (wa, wb) = (ra.watch(), rb.watch());
    let mk = |r| {
        Node::builder()
            .bluetooth(r)
            .policy(Policy::new([PathKind::Bluetooth]))
            .stun_servers(Vec::<String>::new())
            .bind("127.0.0.1:0")
            .unwrap()
    };
    let (mut a, mut b) = (mk(ra), mk(rb));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "espnow-resync");
    b.join_room("pw-test", "espnow-resync");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa)), "linked");
    b.send(&pa, Lane::Reliable, b"before").unwrap();
    pump(&mut [&mut a, &mut b], &mut ev, 1, |_| false);
    assert!(got(&ev[0], pb, b"before"));
    let (accepts, last) = overheard(&air);
    for (accepter, opener_id, accepter_id) in accepts {
        let opener = if accepter == A { B } else { A };
        for (to_mac, from_mac, id) in [(opener, accepter, opener_id), (accepter, opener, accepter_id)] {
            let base = last.get(&id).copied().unwrap_or(0);
            for i in 0..4u32 {
                let seq = base.wrapping_add(1 << 30).wrapping_add(i);
                let f = frame::Frame::Data { to: id, seq, first: true, last: true, total: 1, payload: &[0] };
                air.inject(from_mac, to_mac, &f.encode());
            }
        }
    }
    let started = Instant::now();
    let mut after = vec![Vec::new(), Vec::new()];
    let mut got_at = None;
    let mut n = 0u32;
    while started.elapsed() < Duration::from_secs(30) && got_at.is_none() {
        n += 1;
        let _ = b.send(&pa, Lane::Reliable, format!("after {n}").as_bytes());
        pump(&mut [&mut a, &mut b], &mut after, 1, |_| false);
        if after[0].iter().any(|e| matches!(e, Event::Message { peer, .. } if *peer == pb)) {
            got_at = Some(started.elapsed());
        }
    }
    let (sa, sb) = (wa.stats(), wb.stats());
    eprintln!("first message after the forged resync: {got_at:?}; resyncs a {} b {}", sa.resyncs, sb.resyncs);
    assert!(got_at.is_some_and(|t| t < Duration::from_secs(5)), "a message within seconds: {got_at:?}");
    assert!(sa.resyncs >= 2 && sb.resyncs >= 2, "moved by the forged frames, then back by the real ones");
    let churn = |e: &&Event| matches!(e, Event::Disconnected { .. } | Event::Connected { .. });
    assert_eq!(after.iter().flatten().filter(churn).count(), 0, "no link lost");
    assert_eq!((sa.connections, sb.connections), (1, 1));
}

/// A node as [`node`]'s, with quick timers: a keepalive after 500 ms of quiet, and a link gone after 3 s with nothing
/// heard; and its radio's stats.
fn quick_node(air: &FakeAir, mac: Mac) -> (Node, Watch) {
    let radio = EspNow::new(air.ether(mac, V1_FRAME), Settings::DEFAULT);
    let watch = radio.watch();
    let quick =
        Config { idle_timeout: Duration::from_secs(3), keepalive: Duration::from_millis(500), ..Config::default() };
    let node = Node::builder()
        .bluetooth(radio)
        .policy(Policy::new([PathKind::Bluetooth]))
        .config(quick)
        .stun_servers(Vec::<String>::new())
        .bind("127.0.0.1:0")
        .unwrap();
    (node, watch)
}

/// NB135-1: a link whose radio connection carries nothing to one end while the radio still has it up (whatever moved
/// its numbers) times out there, and that end closes the connection: the link comes back on a new one, with new ids.
#[test]
fn sec_a_link_stuck_on_its_radio_connection_comes_back_on_a_new_one() {
    let air = FakeAir::new();
    let ((mut a, _), (mut b, _)) = (quick_node(&air, A), quick_node(&air, B));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "espnow-stuck");
    b.join_room("pw-test", "espnow-stuck");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa)), "linked");
    // Data to the end that opened the connection goes nowhere, though its radio acknowledges it and keepalives pass:
    // that end hears nothing of the link.
    let (accepts, _) = overheard(&air);
    let (_, opener_id, _) = *accepts.last().unwrap();
    air.set_hole(Some(Box::new(
        move |_, _, f: &[u8]| matches!(frame::parse(f), Ok(frame::Frame::Data { to, .. }) if to == opener_id),
    )));
    let started = Instant::now();
    let mut after = Vec::new();
    let timed_out = |e: &Event| matches!(e, Event::Disconnected { reason: peerwire::DisconnectReason::Timeout, .. });
    let mut n = 0u32;
    let mut back = None;
    while started.elapsed() < Duration::from_secs(30) && back.is_none() {
        n += 1;
        let _ = a.send(&pb, Lane::Reliable, format!("a {n}").as_bytes());
        let _ = b.send(&pa, Lane::Reliable, format!("b {n}").as_bytes());
        pump(&mut [&mut a, &mut b], &mut after, 1, |_| false);
        let gone = after.iter().flatten().any(timed_out);
        if gone && on_radio(&a, &pb) && on_radio(&b, &pa) {
            back = Some(started.elapsed());
        }
    }
    eprintln!("the link timed out and came back on a new connection after {back:?}");
    assert!(after.iter().flatten().any(timed_out), "the link timed out at the end that heard nothing");
    assert!(back.is_some(), "and came back");
    let (accepts, _) = overheard(&air);
    assert!(accepts.iter().any(|(_, o, _)| *o != opener_id), "on a connection with new ids");
    a.send(&pb, Lane::Reliable, b"on the new connection").unwrap();
    b.send(&pa, Lane::Reliable, b"and back").unwrap();
    let mut last = Vec::new();
    pump(&mut [&mut a, &mut b], &mut last, 3, |_| false);
    assert!(got(&last[1], pa, b"on the new connection") && got(&last[0], pb, b"and back"));
}

/// The accepter's first DATA frame (the room's challenge) to the opener, once on the air: (accepter, opener, opener's
/// id).
fn challenge_heard(air: &FakeAir) -> Option<(Mac, Mac, u64)> {
    let heard = air.heard();
    let (accepter, opener, opener_id) = heard.iter().find_map(|(from, to, f)| match frame::parse(f) {
        Ok(frame::Frame::Accept { to: id, .. }) => Some((*from, *to, id)),
        _ => None,
    })?;
    let data = heard.iter().any(|(from, _, f)| {
        *from == accepter && matches!(frame::parse(f), Ok(frame::Frame::Data { to, .. }) if to == opener_id)
    });
    data.then_some((accepter, opener, opener_id))
}

/// NB135-5 (the security review's repro, inverted): one forged ACCEPT with another id, sent after the real ACCEPT and
/// the room's challenge, within 3 s of the first, closes the connection (NB135-4). The radio reports it dropped, as it
/// says nothing of the other end, so the opener holds the advert 1 to 4 s, not the minute it holds one whose connection
/// closed after the challenge: linked again within seconds.
#[test]
fn sec_one_forged_accept_after_the_challenge_costs_seconds() {
    let air = FakeAir::new();
    let ra = EspNow::new(air.ether(A, V1_FRAME), Settings::DEFAULT);
    let rb = EspNow::new(air.ether(B, V1_FRAME), Settings::DEFAULT);
    let (wa, wb) = (ra.watch(), rb.watch());
    let mk = |r| {
        Node::builder()
            .bluetooth(r)
            .policy(Policy::new([PathKind::Bluetooth]))
            .stun_servers(Vec::<String>::new())
            .bind("127.0.0.1:0")
            .unwrap()
    };
    let (mut a, mut b) = (mk(ra), mk(rb));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "espnow-accept-after-challenge");
    b.join_room("pw-test", "espnow-accept-after-challenge");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |_| challenge_heard(&air).is_some()), "a challenge");
    let (accepter, opener, opener_id) = challenge_heard(&air).unwrap();
    assert!(!on_radio(&a, &pb) && !on_radio(&b, &pa), "not linked yet");
    air.inject(accepter, opener, &frame::Frame::Accept { to: opener_id, from: 0x5ec0_ffee, rx_max: 250 }.encode());
    let t = Instant::now();
    let up = |n: &[&mut Node]| on_radio(n[0], &pb) && on_radio(n[1], &pa);
    let linked = pump(&mut [&mut a, &mut b], &mut ev, 15, up);
    let (sa, sb) = (wa.stats(), wb.stats());
    eprintln!("one forged ACCEPT after the challenge: linked again: {linked} after {:?}", t.elapsed());
    assert_eq!(sa.accept_conflicts + sb.accept_conflicts, 1, "the forged ACCEPT closed the connection");
    assert!(linked && t.elapsed() < Duration::from_secs(8), "linked again within seconds: {:?}", t.elapsed());
    let opened =
        air.heard().iter().filter(|(_, _, f)| matches!(frame::parse(f), Ok(frame::Frame::Accept { .. }))).count();
    assert!(opened >= 2, "on a new connection");
}

/// NB135-6 (the security review's repros, inverted): one data frame forged 64 numbers ahead, with the ids and numbers
/// overheard, to one end of an idle link, costs that end the real frame of its number, not the next 63. Those are the
/// session's keepalives on an idle link, so the link timed out, and came back a minute later if that end was the
/// accepter. Now nothing times out: with quick timers (a keepalive per 500 ms, gone after 3 s) 63 keepalives lost
/// would be 31 s.
fn one_forged_frame_on_an_idle_link(to_accepter: bool) {
    let air = FakeAir::new();
    let ((mut a, wa), (mut b, wb)) = (quick_node(&air, A), quick_node(&air, B));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    let room = if to_accepter { "espnow-idle-accepter" } else { "espnow-idle-opener" };
    a.join_room("pw-test", room);
    b.join_room("pw-test", room);
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa)), "linked");
    pump(&mut [&mut a, &mut b], &mut ev, 1, |_| false);
    let (accepts, last) = overheard(&air);
    let (accepter, opener_id, accepter_id) = *accepts.last().unwrap();
    let opener = if accepter == A { B } else { A };
    let (from, to, id) = if to_accepter { (opener, accepter, accepter_id) } else { (accepter, opener, opener_id) };
    let seq = last.get(&id).copied().unwrap_or(0).wrapping_add(64);
    let f = frame::Frame::Data { to: id, seq, first: true, last: true, total: 1, payload: &[0] };
    let old = |w: &Watch| w.stats().old;
    let (old_a, old_b) = (old(&wa), old(&wb));
    air.inject(from, to, &f.encode());
    let mut after = Vec::new();
    pump(&mut [&mut a, &mut b], &mut after, 6, |_| false);
    let churn = |e: &&Event| matches!(e, Event::Disconnected { .. } | Event::Connected { .. });
    assert_eq!(after.iter().flatten().filter(churn).count(), 0, "no link lost: {after:?}");
    assert!(on_radio(&a, &pb) && on_radio(&b, &pa));
    let lost = old(&wa) - old_a + old(&wb) - old_b;
    eprintln!(
        "idle link, one forged frame to the {}: real frames dropped as old {lost}",
        if to_accepter { "accepter" } else { "opener" }
    );
    assert!(lost <= 1, "the real frame of the forged one's number at most: {lost}");
    a.send(&pb, Lane::Reliable, b"after the forged frame").unwrap();
    b.send(&pa, Lane::Reliable, b"and back").unwrap();
    pump(&mut [&mut a, &mut b], &mut after, 2, |_| false);
    assert!(got(&after[1], pa, b"after the forged frame") && got(&after[0], pb, b"and back"));
}

#[test]
fn sec_one_forged_frame_to_the_accepter_of_an_idle_link_costs_one_frame() {
    one_forged_frame_on_an_idle_link(true);
}

#[test]
fn sec_one_forged_frame_to_the_opener_of_an_idle_link_costs_one_frame() {
    one_forged_frame_on_an_idle_link(false);
}

/// NB135-I6 (the security review's repro, inverted): data to the end that ACCEPTED goes nowhere, so that end's link
/// times out first and it closes the connection; the opener hears a close of a connection bound to its peer, and holds
/// the advert 1 to 4 s, not a minute: the link is back within seconds, on a new connection.
#[test]
fn sec_a_link_stuck_at_the_accepter_comes_back_within_seconds() {
    let air = FakeAir::new();
    let ((mut a, _), (mut b, _)) = (quick_node(&air, A), quick_node(&air, B));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "espnow-stuck-accepter");
    b.join_room("pw-test", "espnow-stuck-accepter");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa)), "linked");
    let (accepts, _) = overheard(&air);
    let (_, _, accepter_id) = *accepts.last().unwrap();
    air.set_hole(Some(Box::new(
        move |_, _, f: &[u8]| matches!(frame::parse(f), Ok(frame::Frame::Data { to, .. }) if to == accepter_id),
    )));
    let started = Instant::now();
    let mut after = Vec::new();
    let timed_out = |e: &Event| matches!(e, Event::Disconnected { reason: peerwire::DisconnectReason::Timeout, .. });
    let (mut n, mut gone_at, mut back) = (0u32, None, None);
    while started.elapsed() < Duration::from_secs(20) && back.is_none() {
        n += 1;
        let _ = a.send(&pb, Lane::Reliable, format!("a {n}").as_bytes());
        let _ = b.send(&pa, Lane::Reliable, format!("b {n}").as_bytes());
        pump(&mut [&mut a, &mut b], &mut after, 1, |_| false);
        let gone = after.iter().flatten().any(timed_out);
        if gone && gone_at.is_none() {
            gone_at = Some(started.elapsed());
        }
        if gone && on_radio(&a, &pb) && on_radio(&b, &pa) {
            back = Some(started.elapsed());
        }
    }
    eprintln!("stuck at the accepter: timed out after {gone_at:?}, back after {back:?}");
    assert!(gone_at.is_some(), "the link timed out at the end that heard nothing");
    assert!(back.is_some_and(|t| t < Duration::from_secs(12)), "and came back within seconds: {back:?}");
    let (accepts, _) = overheard(&air);
    assert!(accepts.iter().any(|(_, _, id)| *id != accepter_id), "on a connection with new ids");
}
