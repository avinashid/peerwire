//! NB-097: a node's connection policy (`Node::set_policy`) and live server swaps, on loopback sockets with an
//! in-process signaling hub. What each policy does across NATs is in `peerwire-sim/tests/traversal.rs`.
use peerwire::{
    Config, ConnectError, DisconnectReason, Error, Event, Identity, Invite, Lane, MemoryHub, Node, Path, PathKind,
    PeerId, Policy, Status,
};
use std::net::SocketAddr;
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

/// The peer `e` is about.
fn about(e: &Event) -> PeerId {
    match e {
        Event::Connected { peer, .. }
        | Event::Message { peer, .. }
        | Event::Disconnected { peer, .. }
        | Event::PathChanged { peer, .. }
        | Event::ConnectFailed { peer } => *peer,
        Event::AddrObserved { by, .. } => *by,
    }
}

/// Polls `nodes` until the last, dialling, links to `to`, and has it send `data` at once: polled last, it sends
/// before `to` reads the link's first packet, so `to` reads the two in one poll. Then polls them all for a second;
/// their events, as `pump`.
fn link_and_send(nodes: &mut [&mut Node], to: PeerId, data: &[u8]) -> Vec<Vec<Event>> {
    let (mut ev, last) = (Vec::new(), nodes.len() - 1);
    assert!(pump(nodes, &mut ev, 5, |n| n[last].status(&to) == Status::Connected));
    nodes[last].send(&to, Lane::Reliable, data).unwrap();
    nodes[last].poll();
    pump(nodes, &mut ev, 1, |_| false);
    ev
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
/// the direct links its peer dials again, with the messages that come in with them (NB97-1); with no policy it
/// links again.
#[test]
fn direct_off_closes_direct_links_and_refuses_new_ones() {
    let hub = MemoryHub::new();
    let (mut a, mut b) = linked_pair(on(&hub), on(&hub));
    let (aid, bid) = (a.peer_id(), b.peer_id());
    a.set_policy(Policy::new([PathKind::RelayPeers])).unwrap();
    let got: Vec<Event> = std::iter::from_fn(|| a.try_recv_event()).collect();
    assert!(closed(&got, bid, DisconnectReason::PathOff), "{got:?}");
    assert_eq!(a.status(&bid), Status::Disconnected);
    // b dials a's invite directly and sends as it links: its handshake completes, and a closes the link unseen,
    // with the message.
    let mut ev = Vec::new();
    pump(&mut [&mut a, &mut b], &mut ev, 1, |n| n[1].status(&aid) == Status::Disconnected);
    b.connect(&a.invite()).unwrap();
    let ev = link_and_send(&mut [&mut a, &mut b], aid, b"over a path a has off");
    assert!(closed(&ev[1], aid, DisconnectReason::ClosedByPeer), "{:?}", ev[1]);
    assert!(!ev[0].iter().any(|e| about(e) == bid), "nothing about b reaches a's app: {:?}", ev[0]);
    assert_eq!(a.peers().count(), 0);

    a.set_policy(None).unwrap();
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 10, |n| n[0].status(&bid) == Status::Connected));
}

/// A new link's `AddrObserved`, which the core raises before its `Connected`, waits for that `Connected` to be
/// admitted: with no policy the answering end still gets the two in that order, and the dialling end the one its
/// peer's first packet raises on a link already up.
#[test]
fn observed_addresses_reach_the_app_as_before() {
    let (mut a, mut b) = (Node::bind("127.0.0.1:0").unwrap(), Node::bind("127.0.0.1:0").unwrap());
    let (aid, bid) = (a.peer_id(), b.peer_id());
    b.connect(&a.invite()).unwrap();
    let mut ev = Vec::new();
    pump(&mut [&mut a, &mut b], &mut ev, 1, |_| false);
    let at = |ev: &[Event], f: &dyn Fn(&Event) -> bool| ev.iter().position(f);
    let observed = at(&ev[0], &|e| matches!(e, Event::AddrObserved { by, .. } if *by == bid));
    let connected = at(&ev[0], &|e| matches!(e, Event::Connected { peer, .. } if *peer == bid));
    assert!(observed.is_some() && connected.is_some() && observed < connected, "{:?}", ev[0]);
    assert!(at(&ev[1], &|e| matches!(e, Event::AddrObserved { by, .. } if *by == aid)).is_some(), "{:?}", ev[1]);
}

/// A peer that restarts (same key) and dials again replaces its link: the app still gets the old link's `Replaced`,
/// then the new one's `Connected` and what came in with it. Only a refused link's `Replaced` is dropped (NB97-I4).
#[test]
fn a_replaced_link_reaches_the_app_as_before() {
    let key = [5u8; 32];
    let restart = || Node::builder().identity(Identity::from_secret_bytes(&key)).bind("127.0.0.1:0").unwrap();
    let (mut a, mut b) = (Node::bind("127.0.0.1:0").unwrap(), restart());
    let (aid, bid) = (a.peer_id(), b.peer_id());
    b.connect(&a.invite()).unwrap();
    let mut ev = Vec::new();
    assert!(pump(&mut [&mut a, &mut b], &mut ev, 5, |n| n[0].status(&bid) == Status::Connected));
    drop(b);
    let mut b = restart();
    b.connect(&a.invite()).unwrap();
    let ev = link_and_send(&mut [&mut a, &mut b], aid, b"again");
    let links = |e: &&Event| about(e) == bid && !matches!(e, Event::AddrObserved { .. });
    let about_b: Vec<&Event> = ev[0].iter().filter(links).collect();
    assert!(
        matches!(about_b[..], [
            Event::Disconnected { reason: DisconnectReason::Replaced, .. },
            Event::Connected { .. },
            Event::Message { data, .. },
        ] if data == b"again"),
        "{about_b:?}"
    );
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

/// A peer that replaces its link (it restarted) with one through a member, which the policy has off, ends the
/// link: `PathOff` rather than `Replaced`, and nothing that came in on the new link reaches the app (NB97-I1).
#[test]
fn a_replacement_on_a_path_that_is_off_ends_the_link() {
    let key = [7u8; 32];
    let restart = || Node::builder().identity(Identity::from_secret_bytes(&key)).bind("127.0.0.1:0").unwrap();
    let mut a = Node::builder().policy(Policy::new([PathKind::Direct])).bind("127.0.0.1:0").unwrap();
    let (mut r, mut b) = (Node::bind("127.0.0.1:0").unwrap(), restart());
    let (aid, bid, rid) = (a.peer_id(), b.peer_id(), r.peer_id());
    a.connect(&r.invite()).unwrap();
    b.connect(&a.invite()).unwrap();
    let mut ev = Vec::new();
    let up = |n: &[&mut Node]| n[0].status(&bid) == Status::Connected && n[0].status(&rid) == Status::Connected;
    assert!(pump(&mut [&mut a, &mut r, &mut b], &mut ev, 5, up));
    drop(b);
    let mut b = restart();
    b.connect(&r.invite()).unwrap();
    assert!(pump(&mut [&mut a, &mut r, &mut b], &mut ev, 5, |n| n[2].status(&rid) == Status::Connected));
    let through_r = Invite::<SocketAddr> { peer: aid, key: None, addrs: vec![], relays: vec![(rid, r.addresses()[0])] };
    b.connect(&through_r.to_link()).unwrap();
    let ev = link_and_send(&mut [&mut a, &mut r, &mut b], aid, b"through r, which a has off");
    let about_b: Vec<&Event> = ev[0].iter().filter(|e| about(e) == bid).collect();
    assert_eq!(about_b, [&Event::Disconnected { peer: bid, reason: DisconnectReason::PathOff }]);
    assert_eq!(a.status(&bid), Status::Disconnected);
}

/// Two links of one peer (two instances with one key) announced in one read: the first through a member, which
/// the policy has off, the second direct, which replaces it. The refused link's close is the core's current link's,
/// so it waits for the end of the pass: the admitted link stays up at both ends, and the app gets its `Connected`
/// and message, with no `Replaced` for the link it never saw and nothing that came in on that one (NB97-I4).
#[test]
fn refused_link_replaced_by_an_admitted_one_in_one_batch() {
    let key = [9u8; 32];
    let b = || Node::builder().identity(Identity::from_secret_bytes(&key)).bind("127.0.0.1:0").unwrap();
    let mut a = Node::builder().policy(Policy::new([PathKind::Direct])).bind("127.0.0.1:0").unwrap();
    let (mut r, mut b1, mut b2) = (Node::bind("127.0.0.1:0").unwrap(), b(), b());
    let (aid, bid, rid) = (a.peer_id(), b1.peer_id(), r.peer_id());
    a.connect(&r.invite()).unwrap();
    b1.connect(&r.invite()).unwrap();
    let mut ev = Vec::new();
    let up = |n: &[&mut Node]| n[0].status(&rid) == Status::Connected && n[2].status(&rid) == Status::Connected;
    assert!(pump(&mut [&mut a, &mut r, &mut b1], &mut ev, 5, up));
    let step = |n: &mut Node| {
        n.poll();
        std::thread::sleep(Duration::from_millis(20));
    };
    let through_r = Invite::<SocketAddr> { peer: aid, key: None, addrs: vec![], relays: vec![(rid, r.addresses()[0])] };
    b1.connect(&through_r.to_link()).unwrap();
    b2.connect(&a.invite()).unwrap();
    step(&mut b1); // HS1 to r
    step(&mut r); // on to a
    step(&mut b2); // HS1 straight to a
    step(&mut a); // both answered; an answering end announces a link at its first data packet
    step(&mut r); // HS2 to b1
    step(&mut b1); // linked: its first data packet goes to r
    assert_eq!(b1.status(&aid), Status::Connected);
    b1.send(&aid, Lane::Reliable, b"b1 through r, off").unwrap();
    step(&mut b1);
    step(&mut r); // both on to a
    step(&mut b2); // linked: its first data packet goes straight to a
    assert_eq!(b2.status(&aid), Status::Connected);
    b2.send(&aid, Lane::Reliable, b"b2 direct, allowed").unwrap();
    step(&mut b2);
    a.poll(); // one read: b1's link (refused), then b2's, which replaces it
    let batch: Vec<Event> = std::iter::from_fn(|| a.try_recv_event()).collect();
    let direct = batch.iter().any(|e| matches!(e, Event::Connected { path: Path::Direct(_), .. } if about(e) == bid));
    let message = batch.iter().any(|e| matches!(e, Event::Message { data, .. } if data == b"b2 direct, allowed"));
    assert!(direct && message, "the admitted link's Connected and message reach the app: {batch:?}");
    a.send(&bid, Lane::Reliable, b"back to b2").unwrap();
    let mut ev = vec![batch];
    pump(&mut [&mut a, &mut r, &mut b1, &mut b2], &mut ev, 1, |_| false);
    assert!(!ev[0].iter().any(|e| matches!(e, Event::Disconnected { peer, .. } if *peer == bid)), "{:?}", ev[0]);
    assert!(!ev[0].iter().any(|e| matches!(e, Event::Message { data, .. } if data == b"b1 through r, off")));
    assert!(ev[3].iter().any(|e| matches!(e, Event::Message { data, .. } if data == b"back to b2")), "{:?}", ev[3]);
    assert_eq!((a.status(&bid), b2.status(&aid)), (Status::Connected, Status::Connected));
    assert!(a.path(&bid).is_some_and(|p| matches!(p, Path::Direct(_))));
}
