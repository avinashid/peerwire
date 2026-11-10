//! NB-110: rooms over Bluetooth LE, on radios in one process (`ble::FakeAir`). Nodes with no carrier but the radio
//! find each other, prove the room and link over it; UDP wins when it works, a link that loses UDP comes back over
//! the radio, and moves to UDP again once it works.
use peerwire::ble::{FakeAir, Limits};
use peerwire::{Config, Event, Lane, Node, PathKind, PeerId, Policy, Privacy, Status};
use std::time::{Duration, Instant, SystemTime};

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

/// `ble_chat --no-udp`: a node whose policy allows Bluetooth alone, beside a peer with no policy and UDP working
/// between them (an ESP32 on the same Wi-Fi, say). The link comes up over the radio and stays there, though the peer
/// dials and punches to its UDP addresses: nothing moves it or reconnects it. Before, the node took the peer's UDP
/// dial and closed it for its policy, and the peer dialled again at once, often on UDP again (1 run in 4 never
/// linked in 15 s); once on the radio, it answered the peer's punches, moved to UDP and was closed there, every 15 s.
#[test]
fn a_bluetooth_only_node_keeps_its_links_on_the_radio() {
    let air = FakeAir::new(185);
    let only = Some(Policy::new([PathKind::Bluetooth]));
    let (mut a, mut b) = (node(&air, only, Config::default()), node(&air, None, Config::default()));
    let (pa, pb) = (a.peer_id(), b.peer_id());
    a.join_room("pw-test", "ble-only");
    b.join_room("pw-test", "ble-only");
    let mut ev = Vec::new();
    let on_radio =
        |n: &[&mut Node]| kind(n[0], &pb) == Some(PathKind::Bluetooth) && kind(n[1], &pa) == Some(PathKind::Bluetooth);
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 15, on_radio), "{:?} {:?} {ev:?}", kind(&a, &pb), kind(&b, &pa));
    // Crossing dials may replace the first link once while it comes up; from here on nothing changes.
    pump(&mut [&mut a, &mut b], &mut ev, 1, |_| false);
    let since: Vec<usize> = ev.iter().map(Vec::len).collect();
    pump(&mut [&mut a, &mut b], &mut ev, 20, |_| false);
    let after: Vec<&Event> = ev.iter().zip(&since).flat_map(|(e, n)| &e[*n..]).collect();
    let churn =
        |e: &&&Event| matches!(e, Event::PathChanged { .. } | Event::Connected { .. } | Event::Disconnected { .. });
    assert_eq!(after.iter().filter(churn).count(), 0, "{after:?}");
    assert_eq!((kind(&a, &pb), kind(&b, &pa)), (Some(PathKind::Bluetooth), Some(PathKind::Bluetooth)));
    a.send(&pb, Lane::Reliable, b"still over the radio").unwrap();
    pump(&mut [&mut a, &mut b], &mut ev, 2, |_| false);
    assert!(got(&ev[1], pa, b"still over the radio"));
}

/// NB-119: a node whose clock is unset (1970: a board with no battery for its clock and no network to set it from)
/// takes the time from a room member over a Bluetooth connection that proved the room, says hello at that time, and
/// links over the radio alone with a peer whose clock is set, which drops hellos from 1970.
#[test]
fn a_node_with_no_clock_takes_the_time_over_bluetooth_and_links() {
    let air = FakeAir::new(119);
    let only = Policy::new([PathKind::Bluetooth]);
    let radio = air.radio(Limits::SMALL);
    let mut board = Node::builder()
        .bluetooth(radio)
        .policy(only.clone())
        .stun_servers(Vec::<String>::new())
        .clock_at(5_000)
        .bind("127.0.0.1:0")
        .unwrap();
    let mut pc = node(&air, Some(only), Config::default());
    let (pboard, ppc) = (board.peer_id(), pc.peer_id());
    assert_eq!(board.clock_from_peer(), None);
    board.join_room("pw-test", "no-clock");
    pc.join_room("pw-test", "no-clock");
    let mut ev = Vec::new();
    let up = |n: &[&mut Node]| {
        kind(n[0], &ppc) == Some(PathKind::Bluetooth) && kind(n[1], &pboard) == Some(PathKind::Bluetooth)
    };
    let started = Instant::now();
    assert!(pump(&mut [&mut board, &mut pc], &mut ev, 15, up), "{:?} {:?}", kind(&board, &ppc), kind(&pc, &pboard));
    eprintln!("a node with no clock linked over Bluetooth in {:?}", started.elapsed());
    let taken = board.clock_from_peer().expect("the time, from the member");
    let off = taken.duration_since(SystemTime::now()).unwrap_or_else(|e| e.duration());
    assert!(off < Duration::from_secs(2), "{off:?} off the system clock");
    assert_eq!(pc.clock_from_peer(), None, "a set clock is never taken");
    board.send(&ppc, Lane::Reliable, b"from a board with no clock").unwrap();
    pump(&mut [&mut board, &mut pc], &mut ev, 2, |_| false);
    assert!(got(&ev[1], pboard, b"from a board with no clock"));
}
