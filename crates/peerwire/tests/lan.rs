//! NB-064: rooms over `LanSignaling`, multicast on loopback. Nodes that share only a
//! LAN find each other and link, with no STUN, no relay and no invite.
use peerwire::{DisconnectReason, Event, LanSignaling, Lane, Node, PeerId, Status};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

/// A group port of this test process's own, so parallel test runs don't hear each other.
fn carrier() -> LanSignaling {
    let port = 34000 + (std::process::id() % 4000) as u16 * 2;
    LanSignaling::on(SocketAddrV4::new(*LanSignaling::GROUP.ip(), port), Ipv4Addr::LOCALHOST)
        .expect("multicast on loopback")
}

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

fn linked(n: &Node, ids: &[PeerId]) -> bool {
    ids.iter().all(|p| *p == n.peer_id() || n.status(p) == Status::Connected)
}

#[test]
fn a_room_links_over_the_lan_alone() {
    let node = || Node::builder().signaling(carrier()).bind("127.0.0.1:0").unwrap();
    let (mut a, mut b, mut c, mut stranger) = (node(), node(), node(), node());
    let ids = [a.peer_id(), b.peer_id(), c.peer_id()];
    for n in [&mut a, &mut b, &mut c] {
        n.join_room("pw-test", "lan");
    }
    stranger.join_room("pw-test", "another room");
    let mut ev = Vec::new();
    let mesh = |n: &[&mut Node]| n[..3].iter().all(|n| linked(n, &ids));
    let started = Instant::now();
    assert!(pump(&mut [&mut a, &mut b, &mut c, &mut stranger], &mut ev, 10, mesh), "the room's three nodes link");
    eprintln!("3 nodes linked over LanSignaling in {:?}", started.elapsed());

    a.send(&ids[1], Lane::Reliable, b"over the lan").unwrap();
    // Two more seconds: the message arrives, and the stranger has had time to link if it ever would.
    pump(&mut [&mut a, &mut b, &mut c, &mut stranger], &mut ev, 2, |_| false);
    let message =
        |e: &Event| matches!(e, Event::Message { peer, data, .. } if *peer == ids[0] && data == b"over the lan");
    assert!(ev[1].iter().any(message), "B got A's message");
    // Another room on the same LAN hears the hellos' topic, never links.
    assert_eq!(stranger.peers().count(), 0);
    for (i, n) in [&a, &b, &c].into_iter().enumerate() {
        assert_eq!(n.peers().count(), 2, "node {i}: the other two only");
        // Room peers dial each other at once, so a first link can be replaced by the
        // crossing one (`Replaced`, then `Connected` again), as over any carrier; none drops.
        let drops: Vec<_> = ev[i]
            .iter()
            .filter(|e| matches!(e, Event::Disconnected { reason, .. } if *reason != DisconnectReason::Replaced))
            .collect();
        assert!(drops.is_empty(), "node {i}: {drops:?}");
    }
}

/// NB-092: on a network that delivers no multicast (as a router that never gives it to
/// an ESP32), nodes link by the broadcast leg: here each listens on a group of its own,
/// and they share only loopback's broadcast address.
#[cfg(target_os = "linux")]
#[test]
fn a_room_links_by_broadcast_alone() {
    let port = 34001 + (std::process::id() % 4000) as u16 * 2;
    let node = |g: u8| {
        let group = SocketAddrV4::new(Ipv4Addr::new(239, 255, 80, g), port);
        let lan = LanSignaling::on(group, Ipv4Addr::LOCALHOST)
            .and_then(|c| c.broadcast(Ipv4Addr::new(127, 255, 255, 255)))
            .expect("broadcast on loopback");
        Node::builder().signaling(lan).bind("127.0.0.1:0").unwrap()
    };
    let (mut a, mut b) = (node(87), node(88));
    let ids = [a.peer_id(), b.peer_id()];
    a.join_room("pw-test", "broadcast");
    b.join_room("pw-test", "broadcast");
    let mut ev = Vec::new();
    let started = Instant::now();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 10, |n| n.iter().all(|n| linked(n, &ids))), "linked by broadcast");
    eprintln!("2 nodes linked by broadcast alone in {:?}", started.elapsed());
}
