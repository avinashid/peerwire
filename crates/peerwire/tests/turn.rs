//! NB-099: links through a TURN server, on loopback: the simulator's sans-IO TURN server (`peerwire_sim::turn`) on
//! UDP sockets of its own, one for the server and one per allocation, polled with the nodes. The client's protocol
//! (challenges, stale nonces, refreshes, permissions, channels, restarts) is tested against the same server in
//! `src/turn.rs`; across NATs, in `peerwire-sim/tests/turn.rs`.
use peerwire::{DisconnectReason, Event, Lane, MemoryHub, Node, PathKind, PeerId, Policy, Privacy, Status, TurnServer};
use peerwire_sim::turn::{Out, ServerStats, TurnServer as Server};
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A TURN server on loopback.
struct MockTurn {
    server: Server,
    sock: UdpSocket,
    relays: Arc<Mutex<Vec<UdpSocket>>>,
    epoch: Instant,
}

impl MockTurn {
    fn new() -> MockTurn {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        let relays: Arc<Mutex<Vec<UdpSocket>>> = Arc::default();
        let held = relays.clone();
        let next = move || {
            let s = UdpSocket::bind("127.0.0.1:0").ok()?;
            s.set_nonblocking(true).ok()?;
            let at = s.local_addr().ok()?;
            held.lock().unwrap().push(s);
            Some(at)
        };
        let server = Server::new(sock.local_addr().unwrap(), "peerwire.test", next).user("app", "secret");
        MockTurn { server, sock, relays, epoch: Instant::now() }
    }

    fn addr(&self) -> String {
        self.sock.local_addr().unwrap().to_string()
    }

    fn creds(&self) -> TurnServer {
        TurnServer::new(self.addr(), "app", "secret")
    }

    fn stats(&self) -> ServerStats {
        self.server.stats
    }

    fn poll(&mut self) {
        let now = peerwire_core::Instant::from_micros(self.epoch.elapsed().as_micros() as u64);
        let mut buf = [0u8; 2048];
        let mut out = Vec::new();
        while let Ok((n, from)) = self.sock.recv_from(&mut buf) {
            out.extend(self.server.handle_client(now, from, &buf[..n]));
        }
        for r in self.relays.lock().unwrap().iter() {
            let at = r.local_addr().unwrap();
            while let Ok((n, from)) = r.recv_from(&mut buf) {
                out.extend(self.server.handle_peer(now, at, from, &buf[..n]));
            }
        }
        for o in out {
            match o {
                Out::ToClient { to, data } => {
                    let _ = self.sock.send_to(&data, to);
                }
                Out::ToPeer { from, to, data } => {
                    let relays = self.relays.lock().unwrap();
                    if let Some(r) = relays.iter().find(|r| r.local_addr().ok() == Some(from)) {
                        let _ = r.send_to(&data, to);
                    }
                }
            }
        }
    }
}

/// Polls the server and the nodes until `done`, keeping the nodes' events; false on timeout.
fn pump(
    turn: &mut MockTurn,
    nodes: &mut [&mut Node],
    events: &mut Vec<Vec<Event>>,
    secs: f64,
    mut done: impl FnMut(&MockTurn, &[&mut Node]) -> bool,
) -> bool {
    events.resize_with(nodes.len(), Vec::new);
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        turn.poll();
        for (i, n) in nodes.iter_mut().enumerate() {
            n.poll();
            events[i].extend(std::iter::from_fn(|| n.try_recv_event()));
        }
        if done(turn, nodes) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

fn node(hub: &MemoryHub, turn: Option<&MockTurn>, policy: Option<Policy>) -> Node {
    let mut b = Node::builder().signaling(hub.connect());
    if let Some(t) = turn {
        b = b.turn_servers([t.creds()]);
    }
    if let Some(p) = policy {
        b = b.policy(p);
    }
    let mut n = b.bind("127.0.0.1:0").unwrap();
    n.join_room("pw-turn", "room");
    n
}

fn linked(a: &Node, b: &Node) -> bool {
    a.status(&b.peer_id()) == Status::Connected && b.status(&a.peer_id()) == Status::Connected
}

/// Messages both ways reach the app.
fn talk(turn: &mut MockTurn, a: &mut Node, b: &mut Node) {
    let (aid, bid) = (a.peer_id(), b.peer_id());
    a.send(&bid, Lane::Reliable, b"to b").unwrap();
    b.send(&aid, Lane::Reliable, b"to a").unwrap();
    let got = |ev: &[Event], what: &[u8]| ev.iter().any(|e| matches!(e, Event::Message { data, .. } if data == what));
    let mut ev = Vec::new();
    assert!(pump(turn, &mut [a, b], &mut ev, 5.0, |_, _| false) || true);
    assert!(got(&ev[0], b"to a") && got(&ev[1], b"to b"), "{ev:?}");
}

fn closed(events: &[Event], from: PeerId, why: DisconnectReason) -> bool {
    events.iter().any(|e| matches!(e, Event::Disconnected { peer, reason } if *peer == from && *reason == why))
}

/// a has TURN only (`[Turn]`); b has no policy and no TURN server. a dials b through its allocation and b dials a's
/// relayed address: the link is on TURN at both ends (`path_kind`), carries messages, and a's relayed address isn't
/// among the addresses a gives out.
#[test]
fn a_link_through_turn_alone() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let mut a = node(&hub, Some(&turn), Some(Policy::new([PathKind::Turn])));
    let mut b = node(&hub, None, None);
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, |_, n| linked(n[0], n[1])), "{ev:?}");
    let (aid, bid) = (a.peer_id(), b.peer_id());
    assert_eq!((a.path_kind(&bid), b.path_kind(&aid)), (Some(PathKind::Turn), Some(PathKind::Turn)));
    let relayed = a.turn_status()[0].1.unwrap();
    assert_eq!(a.turn_status()[0].0.to_string(), turn.addr());
    assert_eq!(b.path(&aid), Some(peerwire::Path::Direct(relayed)), "b sends to a's relayed address");
    assert!(!a.addresses().contains(&relayed) && !b.addresses().contains(&relayed));
    talk(&mut turn, &mut a, &mut b);
    let st = turn.stats();
    assert_eq!((st.allocations, st.auth_failures), (1, 0));
    assert!(st.channel_data_in + st.sends > 0 && st.channel_data_out + st.data_indications > 0, "{st:?}");
}

/// `[Turn, Direct]` at both ends keeps the link on TURN: neither punches off it.
#[test]
fn turn_ranked_first_keeps_the_link_there() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let both = || Some(Policy::new([PathKind::Turn, PathKind::Direct]));
    let (mut a, mut b) = (node(&hub, Some(&turn), both()), node(&hub, None, both()));
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, |_, n| linked(n[0], n[1])), "{ev:?}");
    pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 3.0, |_, _| false);
    let (aid, bid) = (a.peer_id(), b.peer_id());
    assert_eq!((a.path_kind(&bid), b.path_kind(&aid)), (Some(PathKind::Turn), Some(PathKind::Turn)));
    assert!(!ev.iter().flatten().any(|e| matches!(e, Event::PathChanged { .. })), "{ev:?}");
    assert_eq!((a.stats().punches_sent, b.stats().punches_sent), (0, 0));
}

/// Live, `Direct` before `Turn`: the TURN link punches off to the direct path, which raises `PathChanged` at both
/// ends though both paths show `Direct(addr)`; `path_kind` says which. Then `Turn` out: the allocation is released.
#[test]
fn a_turn_link_moves_to_direct_when_direct_ranks_first() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let only = || Some(Policy::new([PathKind::Turn]));
    let (mut a, mut b) = (node(&hub, Some(&turn), only()), node(&hub, None, only()));
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, |_, n| linked(n[0], n[1])), "{ev:?}");
    let (aid, bid) = (a.peer_id(), b.peer_id());
    let before = (a.path(&bid), b.path(&aid));
    for n in [&mut a, &mut b] {
        n.set_policy(Policy::new([PathKind::Direct, PathKind::Turn])).unwrap();
    }
    let mut ev = Vec::new();
    let direct = |_: &MockTurn, n: &[&mut Node]| {
        n[0].path_kind(&n[1].peer_id()) == Some(PathKind::Direct)
            && n[1].path_kind(&n[0].peer_id()) == Some(PathKind::Direct)
    };
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, direct), "{ev:?}");
    let changed =
        |ev: &[Event], to: PeerId| ev.iter().any(|e| matches!(e, Event::PathChanged { peer, .. } if *peer == to));
    assert!(changed(&ev[0], bid) && changed(&ev[1], aid), "{ev:?}");
    assert_ne!((a.path(&bid), b.path(&aid)), before);
    assert_eq!(b.path(&aid), Some(peerwire::Path::Direct(a.local_addr())));
    talk(&mut turn, &mut a, &mut b);
    a.set_policy(Policy::new([PathKind::Direct])).unwrap();
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 5.0, |t, _| t.stats().deallocations == 1));
    assert!(linked(&a, &b) && !ev.iter().flatten().any(|e| matches!(e, Event::Disconnected { .. })), "{ev:?}");
}

/// `Turn` taken out live, with only `Direct` left: the TURN link moves to the direct path (`PathChanged`).
#[test]
fn turn_taken_out_moves_the_link() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let only = || Some(Policy::new([PathKind::Turn]));
    let (mut a, mut b) = (node(&hub, Some(&turn), only()), node(&hub, None, only()));
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, |_, n| linked(n[0], n[1])), "{ev:?}");
    for n in [&mut a, &mut b] {
        n.set_policy(Policy::new([PathKind::Direct])).unwrap();
    }
    let mut ev = Vec::new();
    let direct = |_: &MockTurn, n: &[&mut Node]| n[0].path_kind(&n[1].peer_id()) == Some(PathKind::Direct);
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, direct), "{ev:?}");
    assert!(linked(&a, &b) && turn.stats().deallocations == 1, "{ev:?} {:?}", turn.stats());
    let moved = |ev: &[Event]| ev.iter().any(|e| matches!(e, Event::PathChanged { .. }));
    assert!(moved(&ev[0]) && !ev.iter().flatten().any(|e| matches!(e, Event::Disconnected { .. })), "moved: {ev:?}");
}

/// New credentials for a server make a new allocation, at a new relayed address; the old one is released.
#[test]
fn new_credentials_make_a_new_allocation() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    turn.server = std::mem::replace(&mut turn.server, Server::new(turn.sock.local_addr().unwrap(), "x", || None))
        .user("app2", "secret2");
    let mut a = node(&hub, Some(&turn), Some(Policy::new([PathKind::Turn])));
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a], &mut ev, 5.0, |_, n| n[0].turn_status()[0].1.is_some()));
    let first = a.turn_status()[0].1;
    a.set_turn_servers([TurnServer::new(turn.addr(), "app2", "secret2")]);
    let renewed = |t: &MockTurn, n: &[&mut Node]| t.stats().allocations == 2 && n[0].turn_status()[0].1.is_some();
    assert!(pump(&mut turn, &mut [&mut a], &mut ev, 5.0, renewed), "{:?}", turn.stats());
    assert_ne!(a.turn_status()[0].1, first);
    assert_eq!(turn.stats().deallocations, 1);
}

/// The server dropped with `set_turn_servers([])` and nowhere else to go (`[Turn]`): the link closes with `PathOff`,
/// and the allocation is released.
#[test]
fn a_dropped_server_closes_links_with_nowhere_to_go() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let only = || Some(Policy::new([PathKind::Turn]));
    let (mut a, mut b) = (node(&hub, Some(&turn), only()), node(&hub, None, only()));
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, |_, n| linked(n[0], n[1])), "{ev:?}");
    a.set_turn_servers([]);
    let mut ev = Vec::new();
    let gone = |_: &MockTurn, n: &[&mut Node]| n[0].status(&n[1].peer_id()) == Status::Disconnected;
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, gone), "{ev:?}");
    assert!(closed(&ev[0], b.peer_id(), DisconnectReason::PathOff), "{ev:?}");
    assert_eq!((turn.stats().deallocations, a.turn_status()), (1, vec![]));
}

/// With TURN servers but no policy, or with `Turn` in the policy but no servers, nothing is allocated, and peers
/// link as they always have: directly.
#[test]
fn no_policy_or_no_servers_change_nothing() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let mut a = node(&hub, Some(&turn), None);
    let mut b = node(&hub, None, Some(Policy::new([PathKind::Direct, PathKind::Turn])));
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, |_, n| linked(n[0], n[1])), "{ev:?}");
    pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 1.0, |_, _| false);
    assert_eq!(turn.stats(), ServerStats::default(), "nothing reached the server");
    assert_eq!(a.turn_status(), vec![(a.turn_status()[0].0, None)]);
    assert_eq!(a.path_kind(&b.peer_id()), Some(PathKind::Direct));
}

/// Under `Privacy::RelayOnly` TURN stays off, `Turn` in the policy or not; out of it, the allocation is made.
#[test]
fn relay_only_keeps_turn_off() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let b = Node::builder().signaling(hub.connect()).turn_servers([turn.creds()]);
    let b = b.policy(Policy::new([PathKind::Turn])).privacy(Privacy::RelayOnly { via: None });
    let mut a = b.bind("127.0.0.1:0").unwrap();
    let mut ev = Vec::new();
    pump(&mut turn, &mut [&mut a], &mut ev, 1.0, |_, _| false);
    assert_eq!(turn.stats(), ServerStats::default());
    a.set_privacy(Privacy::Direct);
    assert!(pump(&mut turn, &mut [&mut a], &mut ev, 5.0, |_, n| n[0].turn_status()[0].1.is_some()));
    a.set_privacy(Privacy::RelayOnly { via: None });
    assert!(pump(&mut turn, &mut [&mut a], &mut ev, 5.0, |t, _| t.stats().deallocations == 1));
}
