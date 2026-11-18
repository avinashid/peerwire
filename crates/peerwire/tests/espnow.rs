//! NB-135: rooms over ESP-NOW, on stations in one process (`espnow::FakeAir`). Nodes with no carrier but an ESP-NOW
//! radio find each other, prove the room and link over it, also when frames are lost or a v1 radio meets a v2 one; a
//! forged CLOSE, sent with ids overheard on the air, closes nothing.
use peerwire::espnow::{frame, EspNow, FakeAir, Mac, Settings, BROADCAST, V1_FRAME, V2_FRAME};
use peerwire::{Event, Lane, Node, PathKind, PeerId, Policy, Status};
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
