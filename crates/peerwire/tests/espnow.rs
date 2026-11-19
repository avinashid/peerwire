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
/// it now, move it back after four, so messages get through each way within seconds, with no link lost.
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
    // A message each way every second, until one came each way: each end's move back takes 4 real frames from the
    // other, and an end that sent only acknowledgements could take a while to send 4, longer in a loaded debug run,
    // where its move back came after the check at times.
    let started = Instant::now();
    let mut after = vec![Vec::new(), Vec::new()];
    let mut got_at = None;
    let mut n = 0u32;
    let came =
        |ev: &[Event], from: PeerId| ev.iter().any(|e| matches!(e, Event::Message { peer, .. } if *peer == from));
    while started.elapsed() < Duration::from_secs(30) && got_at.is_none() {
        n += 1;
        let _ = a.send(&pb, Lane::Reliable, format!("after {n}").as_bytes());
        let _ = b.send(&pa, Lane::Reliable, format!("after {n}").as_bytes());
        pump(&mut [&mut a, &mut b], &mut after, 1, |_| false);
        if came(&after[0], pb) && came(&after[1], pa) {
            got_at = Some(started.elapsed());
        }
    }
    let (sa, sb) = (wa.stats(), wb.stats());
    eprintln!("a message each way after the forged resync: {got_at:?}; resyncs a {} b {}", sa.resyncs, sb.resyncs);
    assert!(got_at.is_some_and(|t| t < Duration::from_secs(5)), "a message each way within seconds: {got_at:?}");
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

/// NB135-7 (the security review's repros, inverted): four data frames forged 65 numbers ahead of the last one
/// overheard, to one end of an idle link, move that end there, and its next 64 real frames, the session's keepalives,
/// landed behind it and were dropped as copies, so the link timed out (within 6 s with quick timers). The window behind
/// a new place is now empty, so they are taken, and only the real frame with the forged number is lost.
fn four_forged_frames_past_the_window_on_an_idle_link(to_accepter: bool) {
    let air = FakeAir::new();
    let ((mut a, wa), (mut b, wb)) = (quick_node(&air, A), quick_node(&air, B));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    let room = if to_accepter { "espnow-r3-accepter" } else { "espnow-r3-opener" };
    a.join_room("pw-test", room);
    b.join_room("pw-test", room);
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa)), "linked");
    pump(&mut [&mut a, &mut b], &mut ev, 1, |_| false);
    let (accepts, last) = overheard(&air);
    let (accepter, opener_id, accepter_id) = *accepts.last().unwrap();
    let opener = if accepter == A { B } else { A };
    let (from, to, id) = if to_accepter { (opener, accepter, accepter_id) } else { (accepter, opener, opener_id) };
    let seq = last.get(&id).copied().unwrap_or(0).wrapping_add(65);
    let f = frame::Frame::Data { to: id, seq, first: true, last: true, total: 1, payload: &[0] };
    let stat = |w: &Watch| (w.stats().old, w.stats().resyncs);
    let ((old_a, resyncs_a), (old_b, resyncs_b)) = (stat(&wa), stat(&wb));
    for _ in 0..4 {
        air.inject(from, to, &f.encode());
    }
    let mut after = Vec::new();
    pump(&mut [&mut a, &mut b], &mut after, 6, |_| false);
    let churn = |e: &&Event| matches!(e, Event::Disconnected { .. } | Event::Connected { .. });
    assert_eq!(after.iter().flatten().filter(churn).count(), 0, "no link lost: {after:?}");
    assert!(on_radio(&a, &pb) && on_radio(&b, &pa));
    let ((old_a2, resyncs_a2), (old_b2, resyncs_b2)) = (stat(&wa), stat(&wb));
    let lost = old_a2 - old_a + old_b2 - old_b;
    eprintln!(
        "idle link, 4 forged frames at +65 to the {}: real frames dropped as old {lost}",
        if to_accepter { "accepter" } else { "opener" }
    );
    assert_eq!(resyncs_a2 - resyncs_a + resyncs_b2 - resyncs_b, 1, "the forged frames moved that end");
    assert!(lost <= 1, "the real frame of the forged one's number at most: {lost}");
    a.send(&pb, Lane::Reliable, b"after the forged frames").unwrap();
    b.send(&pa, Lane::Reliable, b"and back").unwrap();
    pump(&mut [&mut a, &mut b], &mut after, 2, |_| false);
    assert!(got(&after[1], pa, b"after the forged frames") && got(&after[0], pb, b"and back"));
}

#[test]
fn sec_four_forged_frames_past_the_window_to_the_accepter_cost_one_frame() {
    four_forged_frames_past_the_window_on_an_idle_link(true);
}

#[test]
fn sec_four_forged_frames_past_the_window_to_the_opener_cost_one_frame() {
    four_forged_frames_past_the_window_on_an_idle_link(false);
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

/// Round 4 (the security review's repros, inverted): 32 data frames forged to one end of a live link (`to_opener`, or
/// the accepter), with the ids and numbers overheard, each a whole message of the node's framing of a kind a proven
/// link drops and counts (a challenge): that end closes the connection at its drop limit (`MAX_DROPS`). Default timers,
/// so a session left idle times out after 30 s. Each end sends a message a second; how long until one got through
/// each way on a new connection, if within `secs`, and the events meanwhile.
fn forged_messages_on_a_live_link(to_opener: bool, secs: u64) -> (Option<Duration>, Vec<Event>) {
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    let room = if to_opener { "espnow-r4-drops-opener" } else { "espnow-r4-drops-accepter" };
    a.join_room("pw-test", room);
    b.join_room("pw-test", room);
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa)), "linked");
    pump(&mut [&mut a, &mut b], &mut ev, 1, |_| false);
    let (accepts, last) = overheard(&air);
    let (accepter, opener_id, accepter_id) = *accepts.last().unwrap();
    let opener = if accepter == A { B } else { A };
    let (from, to, id) = if to_opener { (accepter, opener, opener_id) } else { (opener, accepter, accepter_id) };
    let base = last.get(&id).copied().unwrap_or(0);
    for i in 0..32u32 {
        // A chunk that is a whole message (first and last, a counter of its own), kind 3, and 16 bytes.
        let value = [&[0xc0 | ((20 + i) as u8 & 0x3f), 3][..], &[7; 16]].concat();
        let seq = base.wrapping_add(1 + i);
        let f = frame::Frame::Data { to: id, seq, first: true, last: true, total: value.len() as u16, payload: &value };
        air.inject(from, to, &f.encode());
    }
    let (t, mut after) = (Instant::now(), Vec::new());
    let mut k = 0u32;
    while t.elapsed() < Duration::from_secs(secs) {
        k += 1;
        let (to_b, to_a) = (format!("a {k}"), format!("b {k}"));
        let _ = a.send(&pb, Lane::Reliable, to_b.as_bytes());
        let _ = b.send(&pa, Lane::Reliable, to_a.as_bytes());
        let mut e = Vec::new();
        pump(&mut [&mut a, &mut b], &mut e, 1, |_| false);
        let through = got(&e[1], pa, to_b.as_bytes()) && got(&e[0], pb, to_a.as_bytes());
        after.extend(e.into_iter().flatten());
        if through && overheard(&air).0.len() > accepts.len() {
            return (Some(t.elapsed()), after);
        }
    }
    (None, after)
}

/// NB135-I7: 32 forged messages to the accepter of a live link close the connection there. The opener holds that
/// advert 1 to 4 s (NB135-I6) and connects again, but the session stayed on the closed connection until it timed out
/// (30 s). Each end now dials it again over the new connection as soon as that proves the peer: back within seconds.
#[test]
fn sec_thirty_two_forged_messages_to_the_accepter_of_a_link_cost_seconds() {
    let (back, after) = forged_messages_on_a_live_link(false, 20);
    eprintln!("32 forged messages to the accepter: back each way on a new connection after {back:?}");
    let timed_out = |e: &&Event| matches!(e, Event::Disconnected { reason: peerwire::DisconnectReason::Timeout, .. });
    assert!(back.is_some_and(|t| t < Duration::from_secs(10)), "back within seconds: {back:?}");
    assert_eq!(after.iter().filter(timed_out).count(), 0, "with no link timed out: {after:?}");
}

/// NB135-8 (the security review's repro, inverted): 32 forged messages to the opener of a live link close the
/// connection there, and the opener held that advert a minute: no radio connection for 60.1 s. Over ESP-NOW, whose
/// frames anyone can forge, it now holds it 1 to 4 s, and the link moves to the new connection at once (NB135-I7).
#[test]
fn sec_thirty_two_forged_messages_to_the_opener_of_a_link_cost_seconds() {
    let (back, after) = forged_messages_on_a_live_link(true, 20);
    eprintln!("32 forged messages to the opener: back each way on a new connection after {back:?}");
    let timed_out = |e: &&Event| matches!(e, Event::Disconnected { reason: peerwire::DisconnectReason::Timeout, .. });
    assert!(back.is_some_and(|t| t < Duration::from_secs(10)), "back within seconds: {back:?}");
    assert_eq!(after.iter().filter(timed_out).count(), 0, "with no link timed out: {after:?}");
}

/// Polls `a`, then `b`, one at a time, until a poll has put an ACCEPT on `air`: the accepter's, which sends the
/// room's challenge with it, so the opener has heard neither yet.
fn until_accepted(a: &mut Node, b: &mut Node, air: &FakeAir, secs: u64) -> bool {
    let accepted = || !overheard(air).0.is_empty();
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        for n in [&mut *a, &mut *b] {
            n.poll();
            if accepted() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

/// NB135-8 (the security review's repros, inverted): one data frame forged as the room's challenge goes on the air,
/// with the ids overheard, its value a whole message of the node's framing that the room proof doesn't expect (a
/// datagram of one byte), to the opener or to the accepter. That end closes the connection, and the opener held the
/// advert a minute, as for any close once the challenge had come: the pair linked again after 60.3 s (sent to the
/// opener) and 60.7 s (to the accepter), where with no forged frame it links 55 ms after the challenge. Over ESP-NOW,
/// whose frames anyone can forge, it now holds it 1 to 4 s.
fn one_forged_message_during_the_proof(to_opener: bool) {
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    let room = if to_opener { "espnow-r4-proof-opener" } else { "espnow-r4-proof-accepter" };
    a.join_room("pw-test", room);
    b.join_room("pw-test", room);
    assert!(until_accepted(&mut a, &mut b, &air, 15), "an ACCEPT");
    let (accepter, opener, opener_id) = challenge_heard(&air).expect("the room's challenge, with the ACCEPT");
    let (_, _, accepter_id) = overheard(&air).0[0];
    // A chunk that is a whole message (first and last, with the counter the receiver takes next): kind 0, one byte.
    let (from, to, id, seq, value) = match to_opener {
        true => (accepter, opener, opener_id, 1, [0xc1u8, 0, 1]),
        false => (opener, accepter, accepter_id, 0, [0xc0u8, 0, 1]),
    };
    let f = frame::Frame::Data { to: id, seq, first: true, last: true, total: 3, payload: &value };
    air.inject(from, to, &f.encode());
    let (t, mut ev) = (Instant::now(), Vec::new());
    let linked = pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa));
    let took = t.elapsed();
    let to_whom = if to_opener { "opener" } else { "accepter" };
    eprintln!("one forged message during the room proof, to the {to_whom}: linked {linked} after {took:?}");
    assert!(overheard(&air).0.len() >= 2, "the forged message closed the connection: linked on a new one");
    assert!(linked && took < Duration::from_secs(8), "linked again within seconds: {took:?}");
}

#[test]
fn sec_one_forged_message_during_the_proof_to_the_opener_costs_seconds() {
    one_forged_message_during_the_proof(true);
}

#[test]
fn sec_one_forged_message_during_the_proof_to_the_accepter_costs_seconds() {
    one_forged_message_during_the_proof(false);
}

fn other(m: Mac) -> Mac {
    if m == A {
        B
    } else {
        A
    }
}

/// The highest data number `from` sent to connection id `id`, as tried on the air.
fn highest(air: &FakeAir, from: Mac, id: u64) -> Option<u32> {
    air.heard()
        .iter()
        .filter(|(f, _, _)| *f == from)
        .filter_map(|(_, _, x)| match frame::parse(x) {
            Ok(frame::Frame::Data { to, seq, .. }) if to == id => Some(seq),
            _ => None,
        })
        .max()
}

/// The counter of the node's framing on the last value `from` sent to connection id `id`.
fn last_counter(air: &FakeAir, from: Mac, id: u64) -> u8 {
    let last = air.heard().iter().rev().find_map(|(f, _, x)| match frame::parse(x) {
        Ok(frame::Frame::Data { to, first: true, payload, .. }) if *f == from && to == id => payload.first().copied(),
        _ => None,
    });
    last.unwrap_or(0) & 0x3f
}

/// What the security review forged: a datagram as it is that parses as an HS1 (kind, sender index, 32 bytes as its
/// ephemeral key: 37 bytes, no valid Noise message), whole in one chunk of the node's framing (first, last, a counter
/// of its own), as a link message.
fn forged_hs1(n: u8) -> Vec<u8> {
    let hs1 = [&[0x11u8, n, 0, 0, 0x5e][..], &[n; 32]].concat();
    [&[0xc0 | (63 - (n & 7)), 0][..], &hs1].concat()
}

/// A forged HS1 to `to`'s connection `id` as from `from`, 40 numbers past the last `from` sent on it.
fn inject_hs1(air: &FakeAir, from: Mac, to: Mac, id: u64, n: u8) {
    let seq = highest(air, from, id).unwrap_or(0).wrapping_add(40);
    let v = forged_hs1(n);
    let f = frame::Frame::Data { to: id, seq, first: true, last: true, total: v.len() as u16, payload: &v };
    air.inject(from, to, &f.encode());
}

/// NB135-9 (the security review's repros, inverted): the nodes are polled one at a time until each end has sent its
/// identity proof on the connection, and then one HS1 is forged to each end as from the other, with the ids and
/// numbers overheard. Each took the slot of a connection no peer was bound to yet, and the radio's budget, so the real
/// handshake was dropped: linked after 15.5 s, against 63 ms with none; and with one forged pair every 10.05 s, never
/// in 70 s. Datagrams over ESP-NOW are now sealed under the link key, so the forged ones are dropped, uncounted, and
/// take nothing: linked within seconds. With `sustain`, a pair goes again 10.05 s after the first, and the link stays.
/// How long until linked, after the first pair.
fn forged_hs1s_after_the_proof(room: &str, sustain: bool) -> Option<Duration> {
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", room);
    b.join_room("pw-test", room);
    assert!(until_accepted(&mut a, &mut b, &air, 15), "an ACCEPT");
    let (accepter, opener_id, accepter_id) = overheard(&air).0[0];
    let opener = other(accepter);
    let (mut to_acc, mut to_op, mut n) = (false, false, 0u8);
    let end = Instant::now() + Duration::from_secs(15);
    while !(to_acc && to_op) && Instant::now() < end {
        for node in [&mut a, &mut b] {
            node.poll();
            if !to_acc && highest(&air, accepter, opener_id).is_some_and(|s| s >= 2) {
                inject_hs1(&air, opener, accepter, accepter_id, n);
                (n, to_acc) = (n + 1, true);
            }
            if !to_op && highest(&air, opener, accepter_id).is_some_and(|s| s >= 1) {
                inject_hs1(&air, accepter, opener, opener_id, n);
                (n, to_op) = (n + 1, true);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(to_acc && to_op, "both ends proven");
    assert!(!on_radio(&a, &pb) && !on_radio(&b, &pa), "not linked before the forged HS1s");
    let t = Instant::now();
    let mut ev = Vec::new();
    let up = |n: &[&mut Node]| on_radio(n[0], &pb) && on_radio(n[1], &pa);
    let linked = pump(&mut [&mut a, &mut b], &mut ev, 10, up).then(|| t.elapsed());
    eprintln!("one forged HS1 each way after the proof: linked after {linked:?}");
    assert!(a.ble_forged() >= 1 && b.ble_forged() >= 1, "dropped by the links as forged");
    if sustain && linked.is_some() {
        let mut after = Vec::new();
        pump(&mut [&mut a, &mut b], &mut after, 11, |_| t.elapsed() >= Duration::from_millis(10_050));
        let (forged_a, forged_b) = (a.ble_forged(), b.ble_forged());
        inject_hs1(&air, opener, accepter, accepter_id, n);
        inject_hs1(&air, accepter, opener, opener_id, n.wrapping_add(1));
        pump(&mut [&mut a, &mut b], &mut after, 1, |_| false);
        let churn = |e: &&Event| matches!(e, Event::Disconnected { .. } | Event::Connected { .. });
        assert_eq!(after.iter().flatten().filter(churn).count(), 0, "the link stays: {after:?}");
        assert!(on_radio(&a, &pb) && on_radio(&b, &pa), "still linked");
        assert!(a.ble_forged() > forged_a && b.ble_forged() > forged_b, "the second pair dropped as forged too");
    }
    linked
}

#[test]
fn sec_one_forged_hs1_to_each_end_after_the_proof_costs_nothing() {
    let linked = forged_hs1s_after_the_proof("espnow-r5-hs1", false);
    assert!(linked.is_some_and(|t| t < Duration::from_secs(3)), "linked within seconds: {linked:?}");
}

#[test]
fn sec_forged_hs1s_every_ten_seconds_cost_nothing() {
    let linked = forged_hs1s_after_the_proof("espnow-r5-hs1-again", true);
    assert!(linked.is_some_and(|t| t < Duration::from_secs(3)), "linked within seconds: {linked:?}");
}

/// Polls every node for `ms` milliseconds, keeping their events.
fn pump_ms(nodes: &mut [&mut Node], events: &mut Vec<Vec<Event>>, ms: u64) {
    events.resize_with(nodes.len(), Vec::new);
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        for (i, n) in nodes.iter_mut().enumerate() {
            n.poll();
            events[i].extend(std::iter::from_fn(|| n.try_recv_event()));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// NB135-9, and the call that a sealed datagram that doesn't open isn't counted towards the link's drops: if it were,
/// 32 forged datagrams would close a proven connection. 40 are forged to each end of a live link while it carries
/// messages, one at a time, with the ids and numbers overheard, each a whole message of the node's framing of kind 0
/// with a made-up seq and tag. The links drop them all as forged, nothing closes or moves, and every message sent
/// meanwhile arrives, in order: each forged frame costs the real frame of its number on the air (NB135-6), which the
/// reliable lane sends again. Each pair is forged once the round's message has come each way and no real frame waits
/// on the air, with a number above any the receiver took, a forged one's too: a slow debug build sent nothing new
/// between two pairs at times, the second took the first's number, and the receiver dropped it as a copy.
#[test]
fn sec_forged_datagrams_on_a_live_link_close_nothing() {
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "espnow-r5-datagrams");
    b.join_room("pw-test", "espnow-r5-datagrams");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa)), "linked");
    pump(&mut [&mut a, &mut b], &mut ev, 1, |_| false);
    let accepts = overheard(&air).0;
    let (accepter, opener_id, accepter_id) = *accepts.last().unwrap();
    let opener = other(accepter);
    let msgs: Vec<Vec<u8>> = (0..40).map(|i| format!("message {i}").into_bytes()).collect();
    let mut after = vec![Vec::new(), Vec::new()];
    let mut forged: HashMap<u64, u32> = HashMap::new();
    for (i, m) in msgs.iter().enumerate() {
        a.send(&pb, Lane::Reliable, m).unwrap();
        b.send(&pa, Lane::Reliable, m).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while !(got(&after[1], pa, m) && got(&after[0], pb, m) && air.waiting() == 0) {
            assert!(Instant::now() < end, "message {i} each way");
            pump_ms(&mut [&mut a, &mut b], &mut after, 1);
        }
        for (from, to, id) in [(accepter, opener, opener_id), (opener, accepter, accepter_id)] {
            // The receiver's next number, past the last forged, and a counter that is neither the last real one
            // nor the next, so the framing takes it as a whole message.
            let next = highest(&air, from, id).unwrap_or(0).wrapping_add(1);
            let seq = forged.get(&id).map_or(next, |f| next.max(f + 1));
            forged.insert(id, seq);
            let c = (last_counter(&air, from, id) + 10) & 0x3f;
            let value = [&[0xc0 | c, 0][..], &(1_000 + i as u64).to_le_bytes(), &[0x5a; 40]].concat();
            let total = value.len() as u16;
            let f = frame::Frame::Data { to: id, seq, first: true, last: true, total, payload: &value };
            air.inject(from, to, &f.encode());
        }
    }
    let all = |ev: &[Event], from: PeerId| msgs.iter().all(|m| got(ev, from, m));
    let t = Instant::now();
    while !(all(&after[0], pb) && all(&after[1], pa)) && t.elapsed() < Duration::from_secs(10) {
        pump(&mut [&mut a, &mut b], &mut after, 1, |_| false);
    }
    pump(&mut [&mut a, &mut b], &mut after, 1, |_| false);
    eprintln!("40 forged datagrams each way: dropped as forged a {} b {}", a.ble_forged(), b.ble_forged());
    let order = |ev: &[Event], from: PeerId| -> Vec<Vec<u8>> {
        ev.iter()
            .filter_map(|e| match e {
                Event::Message { peer, data, .. } if *peer == from => Some(data.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(order(&after[1], pa), msgs, "every message one way, in order");
    assert_eq!(order(&after[0], pb), msgs, "and the other");
    let churn = |e: &&Event| matches!(e, Event::Disconnected { .. } | Event::Connected { .. });
    assert_eq!(after.iter().flatten().filter(churn).count(), 0, "nothing closed or moved: {after:?}");
    assert_eq!(overheard(&air).0.len(), accepts.len(), "on the same connection");
    assert!(a.ble_forged() >= 40 && b.ble_forged() >= 40, "every forged one reached a link and was dropped there");
}

/// NB135-10 (the security review's repro, inverted): right after the ACCEPT and the room's challenge, `k` data frames
/// are forged to the opener with the numbers 1 to `k`, each a chunk that is no message (dropped and counted, `k` < 32,
/// so no close). The accepter's proof, identity proof and hellos then come with numbers already taken and are dropped
/// as copies, so the opener misses the proof's deadline, and it held the advert a minute: linked again after 63.2 s
/// with 8. Over ESP-NOW it now holds it 1 to 4 s.
fn proof_numbers_taken(k: u32) -> (bool, Duration) {
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", &format!("espnow-r5-proof-{k}"));
    b.join_room("pw-test", &format!("espnow-r5-proof-{k}"));
    assert!(until_accepted(&mut a, &mut b, &air, 15));
    let (accepter, opener_id, _) = overheard(&air).0[0];
    let opener = other(accepter);
    for seq in 1..=k {
        let f = frame::Frame::Data { to: opener_id, seq, first: true, last: true, total: 1, payload: &[0x01] };
        air.inject(accepter, opener, &f.encode());
    }
    let t = Instant::now();
    let mut ev = Vec::new();
    let linked = pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa));
    assert!(overheard(&air).0.len() >= 2, "on a new connection");
    (linked, t.elapsed())
}

#[test]
fn sec_frames_forged_with_the_numbers_of_the_proof_cost_seconds() {
    for k in [8, 20] {
        let (linked, took) = proof_numbers_taken(k);
        eprintln!("{k} frames forged with the proof's numbers: linked {linked} after {took:?}");
        assert!(linked && took < Duration::from_secs(10), "{k}: linked again within seconds: {took:?}");
    }
}

/// NB135-10 (the security review's repro, inverted): 32 messages forged to the accepter of a proven connection that
/// no handshake has bound yet (a standby), with the ids and numbers overheard, make it close the connection at its drop
/// limit. The opener held the advert a minute, as for any standby its other end closed: linked again after 60.9 s.
/// With no link to that peer elsewhere, the connection was the way to it, and it now holds it 1 to 4 s. The handshake's
/// datagrams are lost on the air until then, as a board's slower handshake (156 ms on a C3) leaves the standby open
/// longer than `FakeAir`'s.
#[test]
fn sec_thirty_two_forged_messages_to_the_accepter_of_a_standby_cost_seconds() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    let lost = Arc::new(AtomicU32::new(0));
    let count = lost.clone();
    air.set_hole(Some(Box::new(move |_, _, f: &[u8]| {
        // A value that starts a message of the node's framing (its first chunk) of kind 0: a datagram.
        let starts = |v: &[u8]| v.len() > 1 && v[0] & 0x80 != 0 && v[1] == 0;
        let datagram =
            matches!(frame::parse(f), Ok(frame::Frame::Data { first: true, payload, .. }) if starts(payload));
        if datagram {
            count.fetch_add(1, Ordering::Relaxed);
        }
        datagram
    })));
    a.join_room("pw-test", "espnow-r5-standby");
    b.join_room("pw-test", "espnow-r5-standby");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, |_| lost.load(Ordering::Relaxed) > 0), "a handshake tried");
    pump(&mut [&mut a, &mut b], &mut ev, 1, |_| false);
    assert_eq!((a.ble_counts(), b.ble_counts()), ((1, 1, 0), (1, 1, 0)), "a standby: proven, bound to no peer");
    assert!(!on_radio(&a, &pb) && !on_radio(&b, &pa), "not linked");
    let (accepts, last) = overheard(&air);
    let (accepter, _, accepter_id) = accepts[0];
    let opener = other(accepter);
    let base = last.get(&accepter_id).copied().unwrap_or(0);
    for i in 0..32u32 {
        // A chunk that is a whole message (first and last, a counter of its own), kind 3, and 16 bytes.
        let value = [&[0xc0 | ((20 + i) as u8 & 0x3f), 3][..], &[7; 16]].concat();
        let (seq, total) = (base.wrapping_add(1 + i), value.len() as u16);
        let f = frame::Frame::Data { to: accepter_id, seq, first: true, last: true, total, payload: &value };
        air.inject(opener, accepter, &f.encode());
    }
    let at = usize::from(accepter == B);
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 2, |n| n[at].ble_counts().0 == 0), "the accepter closed it");
    air.set_hole(None);
    let t = Instant::now();
    let linked = pump(&mut [&mut a, &mut b], &mut ev, 15, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa));
    eprintln!("32 forged messages to the accepter of a standby: linked {linked} after {:?}", t.elapsed());
    assert!(linked && t.elapsed() < Duration::from_secs(10), "linked again within seconds: {:?}", t.elapsed());
    assert!(overheard(&air).0.len() > accepts.len(), "on a new connection");
}

const D: Mac = [0x02, 0x50, 0x57, 0, 0, 0x0d];

/// A node as [`node`]'s on v1 frames, and its radio's stats.
fn watched(air: &FakeAir, mac: Mac) -> (Node, Watch) {
    let radio = EspNow::new(air.ether(mac, V1_FRAME), Settings::DEFAULT);
    let watch = radio.watch();
    let node = Node::builder()
        .bluetooth(radio)
        .policy(Policy::new([PathKind::Bluetooth]))
        .stun_servers(Vec::<String>::new())
        .bind("127.0.0.1:0")
        .unwrap();
    (node, watch)
}

/// The `k`th OPEN forged to `to` from a made-up address, with an id of its own: what anyone in range can send.
fn forge_open(air: &FakeAir, to: Mac, k: u64) {
    let from: Mac = [0x02, 0xf0, (k >> 24) as u8, (k >> 16) as u8, (k >> 8) as u8, k as u8];
    air.inject(from, to, &frame::Frame::Open { from: 0x0f00_0000 + k, rx_max: 250 }.encode());
}

/// The `n` of the last advert `mac` broadcast.
fn advert_n(air: &FakeAir, mac: Mac) -> Option<u32> {
    air.heard().iter().rev().find_map(|(from, _, f)| match frame::parse(f) {
        Ok(frame::Frame::Advert { data }) if *from == mac && data.len() == 9 => {
            Some(u32::from_le_bytes([data[1], data[2], data[3], data[4]]))
        }
        _ => None,
    })
}

/// Polls `nodes` once each, keeping their events.
fn poll_all(nodes: &mut [&mut Node], events: &mut Vec<Vec<Event>>) {
    events.resize_with(nodes.len(), Vec::new);
    for (i, n) in nodes.iter_mut().enumerate() {
        n.poll();
        events[i].extend(std::iter::from_fn(|| n.try_recv_event()));
    }
}

/// NB135-11 (the security review's repro, inverted): OPENs forged from made-up addresses, one to each board every
/// `every`, and two to each as it joins. Each makes a connection whose central never proves the room, and two of them
/// took a board's two unbound slots: they counted against its own connects, so the board whose `n` is the smaller
/// connected to no one and the other waited for it. One OPEN to each every 1.2 s kept the pair apart for good (70 in
/// 40 s), where with none it links in 16 to 56 ms. How long until linked, within `secs`, and the OPENs forged. Once
/// linked, the forger goes on for 3 s, and nothing closes.
fn forged_opens(every: Duration, secs: u64) -> (Option<Duration>, u64) {
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    let room = format!("espnow-r6-opens-{}", every.as_millis());
    let mut k = 0;
    let mut forge = |to: Mac| {
        k += 1;
        forge_open(&air, to, k);
    };
    a.join_room("pw-test", &room);
    forge(A);
    forge(A);
    a.poll();
    b.join_room("pw-test", &room);
    forge(B);
    forge(B);
    let (t, mut next) = (Instant::now(), Instant::now() + every);
    let (mut ev, mut linked) = (Vec::new(), None);
    while t.elapsed() < Duration::from_secs(secs) && linked.is_none_or(|l| t.elapsed() < l + Duration::from_secs(3)) {
        if Instant::now() >= next {
            forge(A);
            forge(B);
            next += every;
        }
        poll_all(&mut [&mut a, &mut b], &mut ev);
        if linked.is_none() && on_radio(&a, &pb) && on_radio(&b, &pa) {
            linked = Some(t.elapsed());
            ev.iter_mut().for_each(Vec::clear);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(forge);
    if linked.is_some() {
        let churn = |e: &&Event| matches!(e, Event::Disconnected { .. } | Event::Connected { .. });
        assert_eq!(ev.iter().flatten().filter(churn).count(), 0, "the link stays: {ev:?}");
        assert!(on_radio(&a, &pb) && on_radio(&b, &pa), "still linked");
    }
    (linked, k)
}

#[test]
fn sec_forged_opens_every_1200_ms_keep_no_pair_apart() {
    let (linked, k) = forged_opens(Duration::from_millis(1200), 15);
    eprintln!("an OPEN forged to each board every 1.2 s: linked after {linked:?}, {k} forged");
    assert!(linked.is_some_and(|t| t < Duration::from_secs(5)), "linked within seconds: {linked:?}");
}

/// NB135-11: twice a second, each OPEN just past the half second, so the one forged two before it is past its first
/// second when it comes and makes way for it: every forged connection is within the second a central's connection had
/// before it made way for a newcomer, so the board that connected was refused again and again, 1 to 4 s apart.
#[test]
fn sec_forged_opens_twice_a_second_keep_no_pair_apart() {
    let (linked, k) = forged_opens(Duration::from_millis(505), 15);
    eprintln!("an OPEN forged to each board every 505 ms: linked after {linked:?}, {k} forged");
    assert!(linked.is_some_and(|t| t < Duration::from_secs(5)), "linked within seconds: {linked:?}");
}

/// NB135-11 (the security review's finding, from the code): X is linked with P and Q over two connections bound to
/// them, and OPENs forged from made-up addresses keep its radio's other two connections taken, a new one as soon as one
/// closes. A newcomer, Y, joins. If Y connects (its `n` the smaller), X's radio stayed silent at its 4 connections
/// before X's node could close a forged one for Y, so Y's connect failed after 10 s and Y held X's advert a minute; if
/// X connects, the forged ones counted against its own connects, and it never did. `y_connects` picks which, by
/// joining Y again, off the air, until its advert's `n` is on that side of X's. How long until X and Y linked, within
/// 15 s.
fn two_bound_and_two_forged(y_connects: bool) -> Option<Duration> {
    let limits = Settings::DEFAULT.limits;
    let air = FakeAir::new();
    let (mut x, wx) = watched(&air, A);
    let (mut p, mut q) = (node(&air, B, V1_FRAME), node(&air, C, V1_FRAME));
    let (px, pp, pq) = (x.peer_id(), p.peer_id(), q.peer_id());
    let room = format!("espnow-r6-bound-{y_connects}");
    // A star: P, Q and Y hear X alone, and X links with one at a time. A node with two connections proving the room at
    // once, of 2 unbound, closes the other when the first proves (NB110-2's rule, as over Bluetooth on a board), and
    // holds that advert a minute if it made that one.
    air.set_hole(Some(Box::new(|from: Mac, to: Mac, _: &[u8]| from != A && to != A)));
    let mut ev = Vec::new();
    x.join_room("pw-test", &room);
    p.join_room("pw-test", &room);
    assert!(pump(&mut [&mut x, &mut p], &mut ev, 15, |n| on_radio(n[0], &pp) && on_radio(n[1], &px)), "X and P");
    q.join_room("pw-test", &room);
    let bound = |n: &[&mut Node]| on_radio(n[0], &pp) && on_radio(n[0], &pq) && n[0].ble_counts().2 == 2;
    assert!(pump(&mut [&mut x, &mut p, &mut q], &mut ev, 15, bound), "X linked with P and Q, bound");
    // The forger: an OPEN to X whenever its radio holds fewer than its 4 connections, right after X's poll.
    let mut k = 0;
    let mut forge = |x: &mut Node, others: &mut [&mut Node], ev: &mut Vec<Vec<Event>>| {
        poll_all(&mut [x], ev);
        if wx.stats().connections < limits.connections {
            k += 1;
            forge_open(&air, A, k);
        }
        poll_all(others, ev);
    };
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        forge(&mut x, &mut [&mut p, &mut q], &mut ev);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(x.ble_counts(), (4, 2, 2), "two bound, two forged");
    let (mut y, _) = watched(&air, D);
    let py = y.peer_id();
    air.set_off(D, true);
    let n_x = advert_n(&air, A).expect("X's advert");
    loop {
        y.join_room("pw-test", &room);
        if (advert_n(&air, D).expect("Y's advert") < n_x) == y_connects {
            break;
        }
    }
    air.set_off(D, false);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(15) {
        forge(&mut x, &mut [&mut p, &mut q, &mut y], &mut ev);
        if on_radio(&x, &py) && on_radio(&y, &px) {
            assert_eq!(advert_n(&air, A), Some(n_x), "X's advert stayed");
            return Some(t.elapsed());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

#[test]
fn sec_a_newcomer_links_with_a_node_holding_two_bound_and_two_forged_connections_within_seconds() {
    let linked = two_bound_and_two_forged(true);
    eprintln!("Y connects to X, with two bound and two forged: linked after {linked:?}");
    assert!(linked.is_some_and(|t| t < Duration::from_secs(8)), "linked within seconds: {linked:?}");
}

#[test]
fn sec_a_node_holding_two_bound_and_two_forged_connections_links_with_a_newcomer_within_seconds() {
    let linked = two_bound_and_two_forged(false);
    eprintln!("X, with two bound and two forged, connects to Y: linked after {linked:?}");
    assert!(linked.is_some_and(|t| t < Duration::from_secs(8)), "linked within seconds: {linked:?}");
}

/// The last advert frame `mac` broadcast, as tried on the air: what anyone in range can copy.
fn advert_frame(air: &FakeAir, mac: Mac) -> Option<Vec<u8>> {
    air.heard().into_iter().rev().find_map(|(from, _, f)| {
        let advert = matches!(frame::parse(&f), Ok(frame::Frame::Advert { .. }));
        (advert && from == mac).then_some(f)
    })
}

/// Whether `from` has tried an OPEN to `to` on the air.
fn opened_to(air: &FakeAir, from: Mac, to: Mac) -> bool {
    let open = |x: &[u8]| matches!(frame::parse(x), Ok(frame::Frame::Open { .. }));
    air.heard().iter().any(|(f, t, x)| *f == from && *t == to && open(x))
}

/// Someone in range with a copy of a room's advert: it sends it to X from an address of its own, again each round
/// until X connects to it, and from a new address as X's connect to the last runs out (`connect_within`, 10 s). With
/// `radios`, each address is a radio of the copier's, which acknowledges frames, as any radio does, and answers none;
/// else a made-up one, which no radio has.
struct Copier {
    copy: Vec<u8>,
    radios: Option<Vec<peerwire::espnow::FakeEther>>,
    k: u64,
    opened_at: Option<Instant>,
    sent: u32,
}

impl Copier {
    fn new(air: &FakeAir, copy: Vec<u8>, radios: bool) -> Copier {
        let mut c = Copier { copy, radios: radios.then(Vec::new), k: 0, opened_at: None, sent: 0 };
        c.next(air);
        c
    }

    /// The address it sends from now.
    fn mac(&self) -> Mac {
        let k = self.k;
        [0x02, 0xe0, (k >> 24) as u8, (k >> 16) as u8, (k >> 8) as u8, k as u8]
    }

    /// A new address, and a radio at it if it has radios.
    fn next(&mut self, air: &FakeAir) {
        self.k += 1;
        let mac = self.mac();
        if let Some(r) = self.radios.as_mut() {
            r.push(air.ether(mac, V1_FRAME));
        }
    }

    fn step(&mut self, air: &FakeAir, x: Mac) {
        match self.opened_at {
            None if opened_to(air, x, self.mac()) => self.opened_at = Some(Instant::now()),
            None => self.send(air, x),
            Some(at) if at.elapsed() >= Duration::from_millis(9_950) => {
                self.next(air);
                self.opened_at = None;
                self.send(air, x);
            }
            Some(_) => {}
        }
    }

    fn send(&mut self, air: &FakeAir, x: Mac) {
        air.inject(self.mac(), x, &self.copy);
        self.sent += 1;
    }
}

/// NB110-11's race over ESP-NOW (the security review's R7-1): X's `n` is below B's, so X is the one to connect. With
/// `copier` (whether it has radios), someone in range copies B's advert and sends it to X before X hears B's own, and
/// from a new address as each of X's connects to a copy runs out. How long until X and B link, within `secs`; the
/// copies sent; and whether X sent an OPEN to any copier's address.
fn copies_of_an_advert(copier: Option<bool>, secs: u64) -> (Option<Duration>, u32, bool) {
    let air = FakeAir::new();
    let (mut x, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (px, pb) = (x.peer_id(), b.peer_id());
    let room = format!("espnow-r7-copies-{copier:?}");
    b.join_room("pw-test", &room);
    b.poll();
    let n_b = advert_n(&air, B).expect("B's advert");
    air.set_off(A, true);
    loop {
        x.join_room("pw-test", &room);
        x.poll();
        if advert_n(&air, A).expect("X's advert") < n_b {
            break;
        }
    }
    let copy = advert_frame(&air, B).expect("B's advert");
    let mut copier = copier.map(|radios| Copier::new(&air, copy, radios));
    air.set_off(A, false);
    if let Some(c) = copier.as_mut() {
        c.send(&air, A);
    }
    let t = Instant::now();
    let (mut ex, mut eb) = (Vec::new(), Vec::new());
    let mut linked = None;
    while t.elapsed() < Duration::from_secs(secs) {
        poll_all(&mut [&mut x], &mut ex);
        if let Some(c) = copier.as_mut() {
            c.step(&air, A);
        }
        poll_all(&mut [&mut b], &mut eb);
        if on_radio(&x, &pb) && on_radio(&b, &px) {
            linked = Some(t.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let sent = copier.as_ref().map_or(0, |c| c.sent);
    let opened = copier.as_ref().is_some_and(|c| {
        (1..=c.k).any(|k| {
            let mac = [0x02, 0xe0, (k >> 24) as u8, (k >> 16) as u8, (k >> 8) as u8, k as u8];
            opened_to(&air, A, mac)
        })
    });
    (linked, sent, opened)
}

/// NB135-12 (the security review's repro, inverted): a copy of B's advert, sent to X from a made-up address before
/// B's own, drew X's connect, which no one answered, and X skipped B's adverts as one it was connecting to already; a
/// copy from a new address as each connect ran out kept X and B apart (not linked in 40 s, 87 copies, against 0.47 s
/// with none). Over ESP-NOW the advert's tag is now bound to the address it is sent from, so a copy from another one
/// isn't taken, from a made-up address or from a radio of the copier's, which acknowledges the OPENs: X never connects
/// to a copy, and links with B as with none.
#[test]
fn sec_copies_of_an_advert_from_other_addresses_keep_no_pair_apart() {
    let (control, _, _) = copies_of_an_advert(None, 15);
    let (made_up, n1, o1) = copies_of_an_advert(Some(false), 15);
    let (radios, n2, o2) = copies_of_an_advert(Some(true), 15);
    eprintln!(
        "no copy: linked after {control:?}; copies from made-up addresses: linked after {made_up:?}, {n1} sent, an \
         OPEN to one {o1}; from radios that acknowledge: linked after {radios:?}, {n2} sent, an OPEN to one {o2}"
    );
    assert!(control.is_some_and(|t| t < Duration::from_secs(2)), "control: {control:?}");
    assert!(made_up.is_some_and(|t| t < Duration::from_secs(3)), "copies from made-up addresses: {made_up:?}");
    assert!(radios.is_some_and(|t| t < Duration::from_secs(3)), "copies from radios: {radios:?}");
    assert!(!o1 && !o2, "no connect to a copy");
}

/// A member's advert as it was on the air, with an `n` above `above`, from a node at `mac` that joined the room off the
/// air and is gone: off, so no radio acknowledges a frame to its address. Its node, never polled again.
fn gone_members_advert(air: &FakeAir, mac: Mac, room: &str, above: u32) -> (Node, Vec<u8>) {
    let mut z = node(air, mac, V1_FRAME);
    air.set_off(mac, true);
    loop {
        z.join_room("pw-test", room);
        z.poll();
        if advert_n(air, mac).expect("its advert") > above {
            return (z, advert_frame(air, mac).expect("its advert"));
        }
    }
}

/// NB135-12, NB110-11's residual over ESP-NOW: a member's advert is bound to its address, so a copy goes only from
/// there, and a replay of one from a member that has gone, from its own address, still draws a connect. X's `n` is
/// below B's, so X is the one to connect, and two such replays, heard first, took both of X's unbound slots: its
/// connects to them went on for 10 s, and X linked with B only then. No radio acknowledges an OPEN to a gone member's
/// address, so those connects now fail at once, and X links with B as with none. With `replays` off, the control.
fn replays_from_gone_members(replays: bool) -> Option<Duration> {
    const Z1: Mac = [0x02, 0x50, 0x57, 0, 0, 0x1a];
    const Z2: Mac = [0x02, 0x50, 0x57, 0, 0, 0x1b];
    let air = FakeAir::new();
    let (mut x, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (px, pb) = (x.peer_id(), b.peer_id());
    let room = format!("espnow-r7-gone-{replays}");
    b.join_room("pw-test", &room);
    b.poll();
    let n_b = advert_n(&air, B).expect("B's advert");
    air.set_off(A, true);
    let n_x = loop {
        x.join_room("pw-test", &room);
        x.poll();
        let n = advert_n(&air, A).expect("X's advert");
        if n < n_b {
            break n;
        }
    };
    let gone = [Z1, Z2].map(|z| (z, gone_members_advert(&air, z, &room, n_x)));
    air.set_off(A, false);
    let t = Instant::now();
    let (mut ev, mut next) = (Vec::new(), t);
    while t.elapsed() < Duration::from_secs(15) {
        if replays && Instant::now() >= next {
            next += Duration::from_millis(100);
            for (z, (_, advert)) in &gone {
                air.inject(*z, A, advert);
            }
        }
        poll_all(&mut [&mut x, &mut b], &mut ev);
        if on_radio(&x, &pb) && on_radio(&b, &px) {
            return Some(t.elapsed());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

#[test]
fn sec_a_connect_no_radio_acknowledges_is_given_up_at_once() {
    let control = replays_from_gone_members(false);
    let linked = replays_from_gone_members(true);
    eprintln!("replays of two gone members' adverts: linked after {linked:?}, against {control:?} with none");
    assert!(control.is_some_and(|t| t < Duration::from_secs(2)), "control: {control:?}");
    assert!(linked.is_some_and(|t| t < Duration::from_secs(3)), "linked within seconds: {linked:?}");
}

/// X linked with P and Q over two connections bound to them, its radio hearing them alone (a star: P, Q and any
/// newcomer hear X alone, as in `two_bound_and_two_forged`), in `room`; and X's radio's stats.
fn linked_with_two(air: &FakeAir, room: &str) -> ((Node, Watch), Node, Node) {
    let (mut x, wx) = watched(air, A);
    let (mut p, mut q) = (node(air, B, V1_FRAME), node(air, C, V1_FRAME));
    let (px, pp, pq) = (x.peer_id(), p.peer_id(), q.peer_id());
    air.set_hole(Some(Box::new(|from: Mac, to: Mac, _: &[u8]| from != A && to != A)));
    let mut ev = Vec::new();
    x.join_room("pw-test", room);
    p.join_room("pw-test", room);
    assert!(pump(&mut [&mut x, &mut p], &mut ev, 15, |n| on_radio(n[0], &pp) && on_radio(n[1], &px)), "X and P");
    q.join_room("pw-test", room);
    let bound = |n: &[&mut Node]| on_radio(n[0], &pp) && on_radio(n[0], &pq) && n[0].ble_counts().2 == 2;
    assert!(pump(&mut [&mut x, &mut p, &mut q], &mut ev, 15, bound), "X linked with P and Q, bound");
    ((x, wx), p, q)
}

/// A newcomer at `mac` in `room`, joined off the air until its advert's `n` is below `n_x`, so it is the one to
/// connect to X; still off.
fn newcomer_below(air: &FakeAir, mac: Mac, room: &str, n_x: u32) -> Node {
    let (mut y, _) = watched(air, mac);
    air.set_off(mac, true);
    loop {
        y.join_room("pw-test", room);
        y.poll();
        if advert_n(air, mac).expect("its advert") < n_x {
            return y;
        }
    }
}

/// NB135-13 (the security review's repro, inverted): X is linked with P and Q, bound, and has a connect under way to
/// Z, a member whose board hung: its radio acknowledges the OPENs, as any radio does, and nothing answers them (the
/// security review's connect was to a copy of an advert from a made-up address, which NB135-12's fix no longer
/// takes). An OPEN is forged to X from a made-up address whenever X's radio holds fewer than its 4 connections and
/// one, the connect under way among them, right after X's poll. X's radio counted the connect under way against its
/// one over, and X's node didn't: the node took the one over without making way, and the radio was silent to every
/// OPEN after it. Y, whose `n` is below X's, is the one to connect: its OPENs went unanswered and its connect failed
/// after 10 s (Y linked after 70.4 s in the review's run). How long until X and Y linked, within `secs`; the OPENs
/// forged, and those X's radio took none of.
fn a_newcomer_beside_a_connect_under_way(secs: u64) -> (Option<Duration>, u64, u64) {
    const Z: Mac = [0x02, 0x50, 0x57, 0, 0, 0x1c];
    let limits = Settings::DEFAULT.limits;
    let air = FakeAir::new();
    let room = "espnow-r7-under-way";
    let ((mut x, wx), mut p, mut q) = linked_with_two(&air, room);
    let px = x.peer_id();
    let n_x = advert_n(&air, A).expect("X's advert");
    // Z's advert, with an `n` above X's; then Z's radio is on and its node never polled again.
    let (_z, z_advert) = gone_members_advert(&air, Z, room, n_x);
    air.set_off(Z, false);
    let mut y = newcomer_below(&air, D, room, n_x);
    let py = y.peer_id();
    let mut k = 0;
    let mut next_advert = Instant::now();
    let mut forge = |x: &mut Node, others: &mut [&mut Node], ev: &mut Vec<Vec<Event>>| {
        if Instant::now() >= next_advert {
            next_advert += Duration::from_millis(100);
            air.inject(Z, A, &z_advert); // as Z's radio would, but for its hang
        }
        poll_all(&mut [x], ev);
        for _ in wx.stats().connections..limits.connections + 1 {
            k += 1;
            forge_open(&air, A, k);
        }
        poll_all(others, ev);
    };
    let mut ev = Vec::new();
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        forge(&mut x, &mut [&mut p, &mut q], &mut ev);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(opened_to(&air, A, Z), "a connect to Z under way");
    eprintln!("before Y: X's node {:?}, its radio {} connections", x.ble_counts(), wx.stats().connections);
    let refused = wx.stats().opens_refused;
    air.set_off(D, false);
    let t = Instant::now();
    let mut linked = None;
    while t.elapsed() < Duration::from_secs(secs) {
        forge(&mut x, &mut [&mut p, &mut q, &mut y], &mut ev);
        if on_radio(&x, &py) && on_radio(&y, &px) {
            linked = Some(t.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(forge);
    (linked, k, wx.stats().opens_refused - refused)
}

#[test]
fn sec_a_newcomer_beside_a_connect_under_way_links_within_seconds() {
    let (linked, k, refused) = a_newcomer_beside_a_connect_under_way(15);
    eprintln!("Y beside X's connect under way: linked after {linked:?}, {k} OPENs forged, {refused} not taken");
    assert!(linked.is_some_and(|t| t < Duration::from_secs(5)), "linked within seconds: {linked:?}");
}

/// NB135-13 (the security review's repro, as it ran): X is linked with P and Q, bound; a copy of an advert of Y's
/// whose `n` was above X's is sent to X from a made-up address, and from a new one as each connect to the last runs
/// out; and an OPEN is forged to X whenever X's radio holds fewer than its 4 connections and one, right after X's
/// poll. Y, whose `n` is now below X's, is the one to connect; the forger stops `forge_for` after Y comes on the air.
/// The copy no longer draws a connect (NB135-12), so the forger's OPEN takes the radio's one over each time its node
/// has decided on the last, ahead of Y's, which go unanswered, and Y's connect fails after 10 s. Y held X's advert a
/// minute after it (linked after 70.4 s); it holds it 1 to 4 s now, and connects again. How long until X and Y linked,
/// within `secs`, and the OPENs forged.
fn a_newcomer_kept_out_by_forged_opens(forge_for: Duration, secs: u64) -> (Option<Duration>, u64) {
    let limits = Settings::DEFAULT.limits;
    let air = FakeAir::new();
    let room = format!("espnow-r7-kept-out-{}", forge_for.as_secs());
    let ((mut x, wx), mut p, mut q) = linked_with_two(&air, &room);
    let px = x.peer_id();
    let n_x = advert_n(&air, A).expect("X's advert");
    let (mut y, _) = watched(&air, D);
    let py = y.peer_id();
    air.set_off(D, true);
    let mut copy = None;
    loop {
        y.join_room("pw-test", &room);
        y.poll();
        let n = advert_n(&air, D).expect("Y's advert");
        if n > n_x && copy.is_none() {
            copy = advert_frame(&air, D);
        } else if n < n_x && copy.is_some() {
            break;
        }
    }
    let mut copier = Copier::new(&air, copy.expect("a copy"), false);
    let mut k = 0;
    let mut forge = |x: &mut Node, others: &mut [&mut Node], ev: &mut Vec<Vec<Event>>, on: bool| {
        poll_all(&mut [x], ev);
        if on {
            copier.step(&air, A);
            for _ in wx.stats().connections..limits.connections + 1 {
                k += 1;
                forge_open(&air, A, k);
            }
        }
        poll_all(others, ev);
    };
    let mut ev = Vec::new();
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        forge(&mut x, &mut [&mut p, &mut q], &mut ev, true);
        std::thread::sleep(Duration::from_millis(2));
    }
    air.set_off(D, false);
    let t = Instant::now();
    let mut linked = None;
    while t.elapsed() < Duration::from_secs(secs) {
        forge(&mut x, &mut [&mut p, &mut q, &mut y], &mut ev, t.elapsed() < forge_for);
        if on_radio(&x, &py) && on_radio(&y, &px) {
            linked = Some(t.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(forge);
    (linked, k)
}

#[test]
fn sec_forged_opens_that_keep_a_newcomer_out_cost_seconds_once_they_stop() {
    let (control, k0) = a_newcomer_kept_out_by_forged_opens(Duration::ZERO, 20);
    let (linked, k) = a_newcomer_kept_out_by_forged_opens(Duration::from_secs(15), 40);
    eprintln!(
        "forger stops as Y comes: linked after {control:?}, {k0} OPENs forged; forger on for 15 s: linked after \
         {linked:?}, {k} OPENs forged"
    );
    assert!(control.is_some_and(|t| t < Duration::from_secs(3)), "control: {control:?}");
    assert!(linked.is_some_and(|t| t < Duration::from_secs(20)), "within seconds of the forger's end: {linked:?}");
}

/// NB135-14 (the security review's repro, inverted): 8 data frames are forged to the opener with the numbers of the
/// accepter's room proof right after the ACCEPT (NB135-10's repro), so the opener's connection doesn't prove the room;
/// then, 1.2 s on, past its `GRACE`, `opens` OPENs are forged to the opener from made-up addresses. With two, the
/// second's central made way, and the oldest connection that hadn't proven the room was the opener's own: it closed
/// holding the accepter's advert a minute (linked again after 61.4 s, against 6.3 s with the 8 frames alone). A forged
/// central now makes way before a connection of the node's own, which misses its deadline and holds the advert 1 to
/// 4 s, as does one of its own that makes way. Whether linked again within `secs`, and how long from the first frame.
fn own_connect_mid_proof_and_forged_opens(opens: u64, secs: u64) -> (bool, Duration) {
    let air = FakeAir::new();
    let (mut a, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    let room = format!("espnow-r7-made-way-{opens}");
    a.join_room("pw-test", &room);
    b.join_room("pw-test", &room);
    assert!(until_accepted(&mut a, &mut b, &air, 15), "an ACCEPT");
    let (accepter, opener_id, _) = overheard(&air).0[0];
    let opener = other(accepter);
    for seq in 1..=8 {
        let f = frame::Frame::Data { to: opener_id, seq, first: true, last: true, total: 1, payload: &[0x01] };
        air.inject(accepter, opener, &f.encode());
    }
    let t = Instant::now();
    let mut ev = Vec::new();
    pump_ms(&mut [&mut a, &mut b], &mut ev, 1_200);
    for k in 0..opens {
        forge_open(&air, opener, 100 + k);
    }
    let linked = pump(&mut [&mut a, &mut b], &mut ev, secs, |n| on_radio(n[0], &pb) && on_radio(n[1], &pa));
    (linked, t.elapsed())
}

#[test]
fn sec_frames_forged_with_the_numbers_of_the_proof_and_two_forged_opens_cost_seconds() {
    let (l0, t0) = own_connect_mid_proof_and_forged_opens(0, 15);
    let (l2, t2) = own_connect_mid_proof_and_forged_opens(2, 15);
    eprintln!("8 proof numbers taken: linked {l0} after {t0:?}; and then 2 forged OPENs: linked {l2} after {t2:?}");
    assert!(l0 && t0 < Duration::from_secs(10), "the 8 frames alone: {t0:?}");
    assert!(l2 && t2 < Duration::from_secs(10), "linked again within seconds: {t2:?}");
}

/// NB135-16 (b), the security review's repro, accepted for v1: a member's advert is bound to its address, and a connect
/// whose OPEN no radio acknowledges fails at once (NB135-12). But a radio at a gone member's address acknowledges the
/// OPENs to it, as any radio does for its own address, and answers none, so a connect to a replay of that member's
/// advert goes on for `connect_within`, 10 s. X's `n` is below B's, so X is the one to connect. A forger with 4 gone
/// members' adverts (each `n` above X's) and a radio at each of their addresses replays them to X whenever fewer than
/// two of X's connects to them are under way, for `forge_for`: X's two connects go to them, and X and B stay apart
/// while it goes on (not linked in the 30 s the review's forger ran) and link as it stops. How long until X and B
/// linked, within `secs`; the replays sent, and X's connects to them.
fn replays_from_gone_members_with_radios_at_their_addresses(
    forge_for: Duration,
    secs: u64,
) -> (Option<Duration>, u32, u32) {
    use std::sync::{Arc, Mutex};
    let air = FakeAir::new();
    let (mut x, mut b) = (node(&air, A, V1_FRAME), node(&air, B, V1_FRAME));
    let (px, pb) = (x.peer_id(), b.peer_id());
    let room = format!("espnow-r8-radios-{}", forge_for.as_secs());
    b.join_room("pw-test", &room);
    b.poll();
    let n_b = advert_n(&air, B).expect("B's advert");
    air.set_off(A, true);
    let n_x = loop {
        x.join_room("pw-test", &room);
        x.poll();
        let n = advert_n(&air, A).expect("X's advert");
        if n < n_b {
            break n;
        }
    };
    let zs: Vec<Mac> = (0..4).map(|i| [0x02, 0x50, 0x57, 0, 1, i]).collect();
    let gone: Vec<(Mac, (Node, Vec<u8>))> = zs.iter().map(|&z| (z, gone_members_advert(&air, z, &room, n_x))).collect();
    for z in &zs {
        air.set_off(*z, false); // a radio of the forger's at the address: it acknowledges, and answers nothing
    }
    // X's OPENs to those addresses, as they go on the air.
    let log = Arc::new(Mutex::new(Vec::<(Mac, Instant)>::new()));
    let (l, watched) = (log.clone(), zs.clone());
    air.set_hole(Some(Box::new(move |from: Mac, to: Mac, f: &[u8]| {
        if from == A && watched.contains(&to) && matches!(frame::parse(f), Ok(frame::Frame::Open { .. })) {
            l.lock().unwrap().push((to, Instant::now()));
        }
        false
    })));
    air.set_off(A, false);
    // When X's connect to each address began, and its last OPEN to it.
    let (mut start, mut last) = (HashMap::<Mac, Instant>::new(), HashMap::<Mac, Instant>::new());
    let (mut sent, mut connects) = (0u32, 0u32);
    let (mut ex, mut eb) = (Vec::new(), Vec::new());
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        poll_all(&mut [&mut x], &mut ex);
        for (z, at) in std::mem::take(&mut *log.lock().unwrap()) {
            if last.get(&z).is_none_or(|l| at.saturating_duration_since(*l) > Duration::from_secs(1)) {
                start.insert(z, at);
                connects += 1;
            }
            last.insert(z, at);
        }
        if t.elapsed() < forge_for {
            let busy = |z: &Mac| start.get(z).is_some_and(|s| s.elapsed() < Duration::from_millis(9_950));
            if zs.iter().filter(|z| busy(z)).count() < 2 {
                for (z, (_, advert)) in gone.iter().filter(|(z, _)| !busy(z)) {
                    air.inject(*z, A, advert);
                    sent += 1;
                }
            }
        }
        poll_all(&mut [&mut b], &mut eb);
        if on_radio(&x, &pb) && on_radio(&b, &px) {
            return (Some(t.elapsed()), sent, connects);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    (None, sent, connects)
}

#[test]
fn sec_replays_from_gone_members_with_radios_at_their_addresses_cost_seconds_once_they_stop() {
    let forge_for = Duration::from_secs(10);
    let (control, _, _) = replays_from_gone_members_with_radios_at_their_addresses(Duration::ZERO, 15);
    let (linked, sent, connects) = replays_from_gone_members_with_radios_at_their_addresses(forge_for, 25);
    eprintln!(
        "no replays: linked after {control:?}; replays for {forge_for:?}: linked after {linked:?}, {sent} sent, \
         {connects} connects to them"
    );
    assert!(control.is_some_and(|t| t < Duration::from_secs(2)), "control: {control:?}");
    assert!(connects >= 2, "the replays drew X's two connects: {connects} connects to them");
    let within = forge_for + Duration::from_secs(5);
    assert!(linked.is_some_and(|t| t < within), "within seconds of the forger's end: {linked:?}");
}
