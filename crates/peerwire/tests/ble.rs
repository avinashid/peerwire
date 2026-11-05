//! NB-110: rooms over Bluetooth LE, on radios in one process (`ble::FakeAir`). Nodes with no carrier but the radio
//! find each other, prove the room and link over it; UDP wins when it works, a link that loses UDP comes back over
//! the radio, and moves to UDP again once it works.
use peerwire::ble::{FakeAir, Limits};
use peerwire::{Config, Event, Lane, Node, PathKind, PeerId, Policy, Privacy, Status};
use std::time::{Duration, Instant};

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

/// A node on `air` with no signaling carrier but its radio, on loopback.
fn node(air: &FakeAir, policy: Option<Policy>, config: Config) -> Node {
    let mut b = Node::builder().bluetooth(air.radio(Limits::DESKTOP)).config(config).stun_servers(Vec::<String>::new());
    if let Some(p) = policy {
        b = b.policy(p);
    }
    b.bind("127.0.0.1:0").unwrap()
}

fn kind(n: &Node, p: &PeerId) -> Option<PathKind> {
    (n.status(p) == Status::Connected).then(|| n.path_kind(p)).flatten()
}

fn got(events: &[Event], from: PeerId, data: &[u8]) -> bool {
    events.iter().any(|e| matches!(e, Event::Message { peer, data: d, .. } if *peer == from && d == data))
}

#[test]
fn a_room_links_over_bluetooth_alone() {
    let air = FakeAir::new(23);
    let only = Some(Policy::new([PathKind::Bluetooth]));
    let (mut a, mut b) = (node(&air, only.clone(), Config::default()), node(&air, only, Config::default()));
    let mut stranger = node(&air, None, Config::default());
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "ble");
    b.join_room("pw-test", "ble");
    stranger.join_room("pw-test", "another room");
    let mut ev = Vec::new();
    let started = Instant::now();
    let up = |n: &[&mut Node]| kind(n[0], &pb) == Some(PathKind::Bluetooth) && kind(n[1], &pa).is_some();
    assert!(pump(&mut [&mut a, &mut b, &mut stranger], &mut ev, 10, up), "linked over the radio");
    eprintln!("linked over Bluetooth in {:?}", started.elapsed());
    assert_eq!(kind(&b, &pa), Some(PathKind::Bluetooth));
    let at = a.path(&pb).unwrap();
    assert!(matches!(at, peerwire::Path::Direct(s) if s.ip().to_string().starts_with("0.") && s.port() == 0), "{at:?}");

    // Both lanes, a message far over one GATT value (20 bytes at this MTU).
    let big: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
    a.send(&pb, Lane::Reliable, &big).unwrap();
    b.send(&pa, Lane::Unreliable, b"back over the radio").unwrap();
    let both = |_: &[&mut Node]| false;
    pump(&mut [&mut a, &mut b, &mut stranger], &mut ev, 3, both);
    assert!(got(&ev[1], pa, &big), "3000 bytes reliable");
    assert!(got(&ev[0], pb, b"back over the radio"));
    assert_eq!((a.ble_counts(), b.ble_counts()), ((1, 1, 1), (1, 1, 1)), "one connection, proven and bound");
    assert_eq!(stranger.peers().count(), 0);
    assert_eq!(stranger.ble_counts(), (0, 0, 0), "another room's node never connects");
}

#[test]
fn udp_wins_when_it_works() {
    let air = FakeAir::new(185);
    let (mut a, mut b) = (node(&air, None, Config::default()), node(&air, None, Config::default()));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "ble-udp");
    b.join_room("pw-test", "ble-udp");
    let mut ev = Vec::new();
    // The radio carries the hellos and offers; the dial goes over both; the link ends on UDP.
    let on_udp =
        |n: &[&mut Node]| kind(n[0], &pb) == Some(PathKind::Direct) && kind(n[1], &pa) == Some(PathKind::Direct);
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, on_udp), "{:?} {:?}", kind(&a, &pb), kind(&b, &pa));
    a.send(&pb, Lane::Reliable, b"over udp").unwrap();
    pump(&mut [&mut a, &mut b], &mut ev, 1, |_| false);
    assert!(got(&ev[1], pa, b"over udp"));
}

/// The network goes: the link times out on UDP and comes back over the radio, with no hello to wait for. The
/// network comes back: the link punches to UDP and moves there.
#[test]
fn a_link_falls_back_to_bluetooth_and_back_to_udp() {
    let air = FakeAir::new(185);
    let quick =
        Config { idle_timeout: Duration::from_secs(3), keepalive: Duration::from_millis(500), ..Config::default() };
    let (mut a, mut b) = (node(&air, None, quick.clone()), node(&air, None, quick));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "ble-back");
    b.join_room("pw-test", "ble-back");
    let mut ev = Vec::new();
    let on = |k: PathKind| move |n: &[&mut Node]| kind(n[0], &pb) == Some(k) && kind(n[1], &pa) == Some(k);
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, on(PathKind::Direct)), "on UDP first");

    a.set_udp_off(true);
    b.set_udp_off(true);
    let gone = Instant::now();
    let back = pump(&mut [&mut a, &mut b], &mut ev, 20, on(PathKind::Bluetooth));
    assert!(back, "back over the radio: {:?} {:?} {:?} {:?}", a.ble_counts(), b.ble_counts(), a.stats(), b.stats());
    eprintln!("UDP gone: on Bluetooth again after {:?}", gone.elapsed());
    a.send(&pb, Lane::Reliable, b"while the wifi is down").unwrap();
    pump(&mut [&mut a, &mut b], &mut ev, 2, |_| false);
    assert!(got(&ev[1], pa, b"while the wifi is down"));

    a.set_udp_off(false);
    b.set_udp_off(false);
    let back = Instant::now();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 25, on(PathKind::Direct)), "moved to UDP");
    eprintln!("UDP back: moved off Bluetooth after {:?}", back.elapsed());
    let moved = |e: &Event| matches!(e, Event::PathChanged { .. });
    assert!(ev.iter().flatten().any(moved), "a move, not a reconnect");
}

#[test]
fn relay_only_keeps_the_radio_off() {
    let air = FakeAir::new(185);
    let quiet = Node::builder().bluetooth(air.radio(Limits::DESKTOP)).privacy(Privacy::RelayOnly { via: None });
    let mut a = quiet.stun_servers(Vec::<String>::new()).bind("127.0.0.1:0").unwrap();
    let mut b = node(&air, None, Config::default());
    a.join_room("pw-test", "ble-quiet");
    b.join_room("pw-test", "ble-quiet");
    let mut ev = Vec::new();
    pump(&mut [&mut a, &mut b], &mut ev, 2, |_| false);
    assert_eq!(air.advert(0), None, "no advert");
    assert_eq!((air.links(), a.peers().count()), (0, 0));
}
