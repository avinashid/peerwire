//! NB-097: a node's connection policy (`Node::set_policy`) and live server swaps, on loopback sockets with an
//! in-process signaling hub. What each policy does across NATs is in `peerwire-sim/tests/traversal.rs`.
use peerwire::{
    Config, ConnectError, DisconnectReason, Error, Event, Lane, MemoryHub, Node, PathKind, PeerId, Policy, Status,
};
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

/// Two nodes in one room, linked (directly, on loopback).
fn linked_pair(a: Node, b: Node) -> (Node, Node) {
    let (mut a, mut b) = (a, b);
    for n in [&mut a, &mut b] {
        n.join_room("pw-policy", "room");
    }
    let (aid, bid) = (a.peer_id(), b.peer_id());
    let mut ev = Vec::new();
    let linked = |n: &[&mut Node]| n[0].status(&bid) == Status::Connected && n[1].status(&aid) == Status::Connected;
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 10, linked));
    (a, b)
}

fn on(hub: &MemoryHub) -> Node {
    Node::builder().signaling(hub.connect()).bind("127.0.0.1:0").unwrap()
}

fn closed(events: &[Event], from: PeerId, why: DisconnectReason) -> bool {
    events.iter().any(|e| matches!(e, Event::Disconnected { peer, reason } if *peer == from && *reason == why))
}

#[test]
fn policies_are_checked() {
    use PathKind::*;
    let mut n = Node::bind("127.0.0.1:0").unwrap();
    assert_eq!(n.policy(), None);
    for bad in [Policy::new([]), Policy::new([Direct, Peers, Direct])] {
        assert_eq!(n.set_policy(bad.clone()), Err(Error::InvalidPolicy));
        let err = Node::builder().policy(bad).bind("127.0.0.1:0").err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
    assert_eq!(n.policy(), None, "a bad policy changes nothing");
    let p = Policy::new([Peers, Direct]);
    n.set_policy(p.clone()).unwrap();
    assert_eq!(n.policy(), Some(&p));
    n.set_policy(None).unwrap();
    assert_eq!(n.policy(), None);
    let b = Node::builder().policy(p.clone()).bind("127.0.0.1:0").unwrap();
    assert_eq!(b.policy(), Some(&p));
}

/// Swapping the signaling carrier keeps the links, and peers meet over the new one only.
#[test]
fn signaling_swaps_live_and_links_stay() {
    let (old, new) = (MemoryHub::new(), MemoryHub::new());
    let (mut a, mut b) = linked_pair(on(&old), on(&old));
    let (aid, bid) = (a.peer_id(), b.peer_id());
    a.set_signaling(new.connect());
    b.set_signaling(new.connect());
    let mut c = on(&new);
    c.join_room("pw-policy", "room");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b, &mut c], &mut ev, 10, |n| n[2].peers().count() == 2), "met on the new hub");
    assert_eq!((a.status(&bid), b.status(&aid)), (Status::Connected, Status::Connected));
    assert!(!ev.iter().flatten().any(|e| matches!(e, Event::Disconnected { .. })), "{ev:?}");
    a.send(&bid, Lane::Reliable, b"same link").unwrap();
    let mut ev = Vec::new();
    pump(&mut [&mut a, &mut b], &mut ev, 1, |_| false);
    assert!(ev[1].iter().any(|e| matches!(e, Event::Message { data, .. } if data == b"same link")));
    // The old hub hears from them no more: a newcomer there meets nobody.
    let mut d = on(&old);
    d.join_room("pw-policy", "room");
    let mut ev = Vec::new();
    assert!(!pump(&mut [&mut a, &mut b, &mut d], &mut ev, 3, |n| n[2].peers().count() > 0));
}

/// Turning direct paths off closes a direct link that has nowhere else to go (`PathOff`), and the node refuses
/// the direct links its peer dials again; with no policy it links again.
#[test]
fn direct_off_closes_direct_links_and_refuses_new_ones() {
    let hub = MemoryHub::new();
    let (mut a, mut b) = linked_pair(on(&hub), on(&hub));
    let (aid, bid) = (a.peer_id(), b.peer_id());
    a.set_policy(Policy::new([PathKind::RelayPeers])).unwrap();
    let got: Vec<Event> = std::iter::from_fn(|| a.try_recv_event()).collect();
    assert!(closed(&got, bid, DisconnectReason::PathOff), "{got:?}");
    assert_eq!(a.status(&bid), Status::Disconnected);
    // b dials a's invite directly: its handshake completes, and a closes the link unseen.
    let mut ev = Vec::new();
    pump(&mut [&mut a, &mut b], &mut ev, 1, |n| n[1].status(&aid) == Status::Disconnected);
    b.connect(&a.invite()).unwrap();
    let redialled = |ev: &[Event]| ev.iter().any(|e| matches!(e, Event::Connected { peer, .. } if *peer == aid));
    pump(&mut [&mut a, &mut b], &mut ev, 3, |_| false);
    assert!(redialled(&ev[1]) && closed(&ev[1], aid, DisconnectReason::ClosedByPeer), "{:?}", ev[1]);
    assert!(!ev[0].iter().any(|e| matches!(e, Event::Connected { .. })), "{:?}", ev[0]);
    assert_eq!(a.peers().count(), 0);

    a.set_policy(None).unwrap();
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 10, |n| n[0].status(&bid) == Status::Connected));
}

/// Leaving `RelayPeers` out closes the relay peers' links and stops dialling them; back in, they link again.
#[test]
fn relay_peers_follow_their_kind() {
    let mut relay = Node::bind("127.0.0.1:0").unwrap();
    let rid = relay.peer_id();
    let mut a = Node::builder().relay_peers([relay.invite()]).bind("127.0.0.1:0").unwrap();
    let linked = |n: &[&mut Node]| n[0].relay_peers() == vec![(rid, Status::Connected)];
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut relay], &mut ev, 5, linked));
    a.set_policy(Policy::new([PathKind::Direct])).unwrap();
    assert_eq!(a.relay_peers(), vec![(rid, Status::Disconnected)]);
    let mut ev = Vec::new();
    assert!(!pump(&mut [&mut a, &mut relay], &mut ev, 3, |n| n[0].relay_peers()[0].1 != Status::Disconnected));
    a.set_policy(Policy::new([PathKind::Direct, PathKind::RelayPeers])).unwrap();
    assert!(pump(&mut [&mut a, &mut relay], &mut ev, 5, linked));
}

/// An invite is dialled on the paths the policy allows: with none it has (relays only, an invite with
/// addresses and no relay), the dial fails at once.
#[test]
fn invite_dials_follow_the_policy() {
    let mut b = Node::bind("127.0.0.1:0").unwrap();
    let (bid, invite) = (b.peer_id(), b.invite());
    let mut a = Node::builder().policy(Policy::new([PathKind::RelayPeers])).bind("127.0.0.1:0").unwrap();
    assert_eq!(a.connect(&invite), Err(Error::Connect(ConnectError::NoPath)));
    assert_eq!(a.status(&bid), Status::Disconnected);
    a.set_policy(Policy::new([PathKind::Peers, PathKind::Direct])).unwrap();
    assert_eq!(a.connect(&invite), Ok(bid), "no relay to try: dialled directly at once");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 5, |n| n[0].status(&bid) == Status::Connected));
}

/// A link moved off a path the policy turned off by dialling it again through a relay peer that doesn't reach
/// the peer: the dial fails, the link is closed (`PathOff`), and the failed dial is the node's own (no
/// `ConnectFailed` for the app).
#[test]
fn a_failed_move_closes_the_link_quietly() {
    let hub = MemoryHub::new();
    let mut relay = Node::bind("127.0.0.1:0").unwrap();
    let quick = Config { handshake_retry: Duration::from_millis(50), handshake_attempts: 3, ..Config::default() };
    let a = Node::builder().config(quick).signaling(hub.connect()).relay_peers([relay.invite()]);
    let (mut a, mut b) = linked_pair(a.bind("127.0.0.1:0").unwrap(), on(&hub));
    let (rid, bid) = (relay.peer_id(), b.peer_id());
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b, &mut relay], &mut ev, 5, |n| n[0].relay_peers()[0].1 == Status::Connected));
    a.set_policy(Policy::new([PathKind::RelayPeers])).unwrap();
    assert_eq!(a.status(&bid), Status::Connected, "kept while it moves");
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b, &mut relay], &mut ev, 5, |n| n[0].status(&bid) == Status::Disconnected));
    pump(&mut [&mut a, &mut b, &mut relay], &mut ev, 1, |_| false);
    assert!(closed(&ev[0], bid, DisconnectReason::PathOff), "{:?}", ev[0]);
    assert!(!ev[0].iter().any(|e| matches!(e, Event::ConnectFailed { .. })), "{:?}", ev[0]);
    assert_eq!(a.relay_peers(), vec![(rid, Status::Connected)]);
}
