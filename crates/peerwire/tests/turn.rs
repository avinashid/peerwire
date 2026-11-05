//! NB-099: links through a TURN server, on loopback: the simulator's sans-IO TURN server (`peerwire_sim::turn`) on
//! UDP sockets of its own, one for the server and one per allocation, polled with the nodes. The client's protocol
//! (challenges, stale nonces, refreshes, permissions, channels, restarts) is tested against the same server in
//! `src/turn.rs`; across NATs, in `peerwire-sim/tests/turn.rs`.
use peerwire::{
    ConnectError, DisconnectReason, Error, Event, Lane, MemoryHub, Node, Path, PathKind, PeerId, Policy, Privacy,
    Status, TurnServer,
};
use peerwire_sim::turn::{Out, ServerStats, TurnServer as Server};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A TURN server on loopback (or at another of this host's addresses).
struct MockTurn {
    server: Server,
    sock: UdpSocket,
    relays: Arc<Mutex<Vec<UdpSocket>>>,
    epoch: Instant,
    /// Where the datagrams to its relayed addresses came from.
    heard: std::collections::BTreeSet<SocketAddr>,
}

impl MockTurn {
    fn new() -> MockTurn {
        MockTurn::on(Ipv4Addr::LOCALHOST.into())
    }

    /// The server and its relayed addresses at `ip`.
    fn on(ip: IpAddr) -> MockTurn {
        let sock = UdpSocket::bind((ip, 0)).unwrap();
        sock.set_nonblocking(true).unwrap();
        let relays: Arc<Mutex<Vec<UdpSocket>>> = Arc::default();
        let held = relays.clone();
        let next = move || {
            let s = UdpSocket::bind((ip, 0)).ok()?;
            s.set_nonblocking(true).ok()?;
            let at = s.local_addr().ok()?;
            held.lock().unwrap().push(s);
            Some(at)
        };
        let server = Server::new(sock.local_addr().unwrap(), "peerwire.test", next).user("app", "secret");
        MockTurn { server, sock, relays, epoch: Instant::now(), heard: Default::default() }
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
                self.heard.insert(from);
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
    n.note_sources();
    n
}

/// A `RelayOnly` node (through `via`, if any) with TURN on `turn` and a policy of `order`, in the room.
fn relay_only(hub: &MemoryHub, turn: &MockTurn, via: Option<PeerId>, order: &[PathKind]) -> Node {
    let b = Node::builder().signaling(hub.connect()).turn_servers([turn.creds()]);
    let b = b.policy(Policy::new(order.iter().copied())).privacy(Privacy::RelayOnly { via });
    let mut n = b.bind("127.0.0.1:0").unwrap();
    n.join_room("pw-turn", "room");
    n.note_sources();
    n
}

/// Whether `n`'s socket read a datagram from `from`'s since it started noting them.
fn heard(n: &Node, from: &Node) -> bool {
    n.sources().contains(&from.local_addr())
}

/// This host's address on its default route, which a node bound to any interface offers first (as `Node` finds
/// it: a UDP socket connected to a documentation address sends nothing); `None` with loopback only.
fn host_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    Some(s.local_addr().ok()?.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
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

/// One server as both STUN and TURN, as a self-hosted coturn usually is: its Binding answers reach STUN, so a learns
/// its address from it (the one the server sees), and a links through it. On this host's own address, as a node
/// bound to loopback asks no STUN server; b asks none.
#[test]
fn one_server_for_stun_and_turn() {
    let Some(ip) = host_ip() else { return eprintln!("skipped: no address but loopback on this host") };
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::on(ip));
    let a = Node::builder().signaling(hub.connect()).stun_servers([turn.addr()]).turn_servers([turn.creds()]);
    let mut a = a.policy(Policy::new([PathKind::Turn])).bind("0.0.0.0:0").unwrap();
    let mut b = Node::builder().signaling(hub.connect()).stun_servers(Vec::<String>::new()).bind("0.0.0.0:0").unwrap();
    for n in [&mut a, &mut b] {
        n.join_room("pw-turn", "room");
    }
    let mut ev = Vec::new();
    let done = |_: &MockTurn, n: &[&mut Node]| n[0].public_addr().is_some() && linked(n[0], n[1]);
    let up = pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, done);
    assert!(up, "public {:?}, {:?} {ev:?}", a.public_addr(), turn.stats());
    assert_eq!(a.public_addr(), Some(SocketAddr::new(ip, a.local_addr().port())), "as the server saw it");
    assert_eq!((a.path_kind(&b.peer_id()), b.path_kind(&a.peer_id())), (Some(PathKind::Turn), Some(PathKind::Turn)));
    let st = turn.stats();
    assert!(st.bindings > 0 && st.allocations == 1 && st.auth_failures == 0, "{st:?}");
    talk(&mut turn, &mut a, &mut b);
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

/// NB-101: under `Privacy::RelayOnly` TURN runs only with `Turn` in the policy: with `[Direct, RelayPeers]` nothing
/// reaches the server; with `Turn` in, the allocation is made, and with it out again, released.
#[test]
fn relay_only_runs_turn_only_with_turn_in_the_policy() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let mut a = relay_only(&hub, &turn, None, &[PathKind::Direct, PathKind::RelayPeers]);
    let mut ev = Vec::new();
    pump(&mut turn, &mut [&mut a], &mut ev, 1.0, |_, _| false);
    assert_eq!(turn.stats(), ServerStats::default());
    a.set_policy(Policy::new([PathKind::Turn])).unwrap();
    assert!(pump(&mut turn, &mut [&mut a], &mut ev, 5.0, |_, n| n[0].turn_status()[0].1.is_some()));
    a.set_policy(Policy::new([PathKind::RelayPeers])).unwrap();
    assert!(pump(&mut turn, &mut [&mut a], &mut ev, 5.0, |t, _| t.stats().deallocations == 1));
}

/// NB-101: a is `RelayOnly` with TURN (`[Turn]`), b has no policy and no TURN. They link through a's allocation, `Turn`
/// at both ends. b's socket hears nothing from a's, nor a's from b's: b dials every address it is offered, so a's
/// offers carried none of a's. a's socket hears from its TURN server alone. `Turn` out of a's policy, with no relay the
/// link has nowhere to go and closes with `PathOff`, still without a datagram from a's socket.
#[test]
fn relay_only_links_through_its_allocation_alone() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let mut a = relay_only(&hub, &turn, None, &[PathKind::Turn]);
    let mut b = node(&hub, None, None);
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, |_, n| linked(n[0], n[1])), "{ev:?}");
    let (aid, bid) = (a.peer_id(), b.peer_id());
    assert_eq!((a.path_kind(&bid), b.path_kind(&aid)), (Some(PathKind::Turn), Some(PathKind::Turn)));
    talk(&mut turn, &mut a, &mut b);
    let (relayed, server) = (a.turn_status()[0].1.unwrap(), turn.addr().parse::<SocketAddr>().unwrap());
    assert!(b.sources().contains(&relayed), "b heard a's allocation: {:?}", b.sources());
    assert!(!heard(&b, &a) && !heard(&a, &b), "{:?} {:?}", b.sources(), a.sources());
    assert_eq!(a.sources(), [server]);
    assert!(!turn.heard.contains(&a.local_addr()), "{:?}", turn.heard);
    a.set_policy(Policy::new([PathKind::RelayPeers])).unwrap();
    let mut ev = Vec::new();
    let gone = |_: &MockTurn, n: &[&mut Node]| n[0].status(&n[1].peer_id()) == Status::Disconnected;
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, gone), "{ev:?}");
    assert!(closed(&ev[0], bid, DisconnectReason::PathOff), "{ev:?}");
    pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 1.0, |_, _| false);
    assert!(!heard(&b, &a), "{:?}", b.sources());
}

/// NB-101: a and b both `RelayOnly` with TURN, each with an allocation of its own on one server: each dials the
/// other's relayed address through its own, the link is `Turn` at both ends, each socket hears only the server, and
/// the server's relayed addresses hear from neither socket, only from each other.
#[test]
fn two_relay_only_nodes_link_through_both_allocations() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let mut a = relay_only(&hub, &turn, None, &[PathKind::Turn]);
    let mut b = relay_only(&hub, &turn, None, &[PathKind::Turn]);
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, |_, n| linked(n[0], n[1])), "{ev:?}");
    assert_eq!((a.path_kind(&b.peer_id()), b.path_kind(&a.peer_id())), (Some(PathKind::Turn), Some(PathKind::Turn)));
    talk(&mut turn, &mut a, &mut b);
    let server = turn.addr().parse::<SocketAddr>().unwrap();
    assert_eq!((a.sources(), b.sources()), (vec![server], vec![server]));
    let relayed: Vec<SocketAddr> = [&a, &b].iter().filter_map(|n| n.turn_status()[0].1).collect();
    assert!(!turn.heard.is_empty() && turn.heard.iter().all(|h| relayed.contains(h)), "{:?}", turn.heard);
    assert_eq!(turn.stats().allocations, 2);
}

/// NB-101: a is `RelayOnly { via: r }` with TURN (`[Turn]`), r has no policy, and b dials TURN before peers
/// (`[Direct, Turn, Peers]`). a links r directly, as before, and b through its allocation. a's server dropped, the
/// link to b moves to r (`Via` at both ends), and b's socket hears nothing from a's, before or after.
#[test]
fn a_lost_allocation_moves_relay_only_links_to_the_relay() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let mut r = node(&hub, None, None);
    let mut a = relay_only(&hub, &turn, Some(r.peer_id()), &[PathKind::Turn]);
    let mut ev = Vec::new();
    let ready = |_: &MockTurn, n: &[&mut Node]| linked(n[0], n[1]) && n[1].turn_status()[0].1.is_some();
    assert!(pump(&mut turn, &mut [&mut r, &mut a], &mut ev, 10.0, ready), "{ev:?}");
    let mut b = node(&hub, None, Some(Policy::new([PathKind::Direct, PathKind::Turn, PathKind::Peers])));
    let (rid, aid, bid) = (r.peer_id(), a.peer_id(), b.peer_id());
    let mut ev = Vec::new();
    let all = |_: &MockTurn, n: &[&mut Node]| linked(n[0], n[1]) && linked(n[1], n[2]) && linked(n[0], n[2]);
    assert!(pump(&mut turn, &mut [&mut r, &mut a, &mut b], &mut ev, 10.0, all), "{ev:?}");
    assert_eq!((a.path_kind(&rid), a.path(&rid)), (Some(PathKind::Direct), Some(Path::Direct(r.local_addr()))));
    assert_eq!(a.path_kind(&bid), Some(PathKind::Turn));
    a.set_turn_servers([]);
    let mut ev = Vec::new();
    let via = |_: &MockTurn, n: &[&mut Node]| {
        n[1].path(&n[2].peer_id()) == Some(Path::Via(n[0].peer_id()))
            && n[2].path(&n[1].peer_id()) == Some(Path::Via(n[0].peer_id()))
    };
    assert!(pump(&mut turn, &mut [&mut r, &mut a, &mut b], &mut ev, 10.0, via), "{ev:?}");
    assert!(!closed(&ev[1], bid, DisconnectReason::PathOff), "{ev:?}");
    a.send(&bid, Lane::Reliable, b"to b").unwrap();
    b.send(&aid, Lane::Reliable, b"to a").unwrap();
    let mut ev = Vec::new();
    pump(&mut turn, &mut [&mut r, &mut a, &mut b], &mut ev, 3.0, |_, _| false);
    let got = |ev: &[Event], what: &[u8]| ev.iter().any(|e| matches!(e, Event::Message { data, .. } if data == what));
    assert!(got(&ev[1], b"to a") && got(&ev[2], b"to b"), "through r: {ev:?}");
    assert!(!heard(&b, &a) && b.status(&aid) == Status::Connected, "{:?}", b.sources());
}

/// NB-101: a starts with `Privacy::Direct` and `[Direct, Turn]` (its allocation up), linked to b directly; b has no
/// policy. a turns `RelayOnly` on: the link goes through a's allocation at once, and a dials b again there, which
/// replaces it. From the switch on, b's socket hears nothing from a's.
#[test]
fn switching_to_relay_only_takes_a_direct_link_onto_turn() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let mut a = node(&hub, Some(&turn), Some(Policy::new([PathKind::Direct, PathKind::Turn])));
    let mut b = node(&hub, None, None);
    let mut ev = Vec::new();
    let up = |_: &MockTurn, n: &[&mut Node]| {
        let direct = n[0].path_kind(&n[1].peer_id()) == Some(PathKind::Direct);
        linked(n[0], n[1]) && direct && n[0].turn_status()[0].1.is_some()
    };
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, up), "{ev:?}");
    assert!(heard(&b, &a), "linked directly");
    // What a has sent so far, b has read.
    a.poll();
    b.poll();
    b.note_sources();
    a.set_privacy(Privacy::RelayOnly { via: None });
    let bid = b.peer_id();
    assert_eq!(a.path_kind(&bid), Some(PathKind::Turn), "at once");
    let mut ev = Vec::new();
    pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 3.0, |_, _| false);
    let up_again =
        |ev: &[Event], to: PeerId| ev.iter().any(|e| matches!(e, Event::Connected { peer, .. } if *peer == to));
    assert!(up_again(&ev[0], bid) && up_again(&ev[1], a.peer_id()), "dialled again: {ev:?}");
    let relayed = a.turn_status()[0].1.unwrap();
    assert_eq!((a.path_kind(&bid), b.path(&a.peer_id())), (Some(PathKind::Turn), Some(Path::Direct(relayed))));
    talk(&mut turn, &mut a, &mut b);
    assert!(!heard(&b, &a), "{:?}", b.sources());
}

/// NB-101: a `RelayOnly` node with TURN and no relay connects to b's invite through its allocation (`Turn`), and b's
/// socket hears nothing from a's; with no TURN server it fails with `NoRelay`, as before. a's own invite carries its
/// relayed address alone, and d, which holds it, links with a through a's allocation, without a datagram from a's
/// socket.
#[test]
fn relay_only_connects_to_an_invite_through_its_allocation() {
    let (hub, mut turn) = (MemoryHub::new(), MockTurn::new());
    let a = Node::builder().signaling(hub.connect()).turn_servers([turn.creds()]).policy(Policy::new([PathKind::Turn]));
    let mut a = a.privacy(Privacy::RelayOnly { via: None }).bind("127.0.0.1:0").unwrap();
    let mut b = Node::builder().signaling(hub.connect()).bind("127.0.0.1:0").unwrap();
    b.note_sources();
    let mut ev = Vec::new();
    assert!(pump(&mut turn, &mut [&mut a], &mut ev, 5.0, |_, n| n[0].turn_status()[0].1.is_some()));
    let bid = a.connect(&b.invite()).unwrap();
    assert!(pump(&mut turn, &mut [&mut a, &mut b], &mut ev, 10.0, |_, n| linked(n[0], n[1])), "{ev:?}");
    assert_eq!(a.path_kind(&bid), Some(PathKind::Turn));
    talk(&mut turn, &mut a, &mut b);
    assert!(!heard(&b, &a), "{:?}", b.sources());
    let mut c = Node::builder().privacy(Privacy::RelayOnly { via: None }).bind("127.0.0.1:0").unwrap();
    assert!(matches!(c.connect(&b.invite()), Err(Error::Connect(ConnectError::NoRelay))));
    let invite = peerwire::Invite::<SocketAddr>::parse(&a.invite()).unwrap();
    assert_eq!(invite.addrs, [a.turn_status()[0].1.unwrap()]);
    let mut d = Node::builder().signaling(hub.connect()).bind("127.0.0.1:0").unwrap();
    d.note_sources();
    let aid = d.connect(&a.invite()).unwrap();
    assert!(pump(&mut turn, &mut [&mut a, &mut d], &mut ev, 10.0, |_, n| linked(n[0], n[1])), "{ev:?}");
    assert_eq!(a.path_kind(&d.peer_id()), Some(PathKind::Turn));
    assert!(d.status(&aid) == Status::Connected && !heard(&d, &a), "{:?}", d.sources());
}
