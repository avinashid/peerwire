//! NB-099: TURN across simulated NATs. The peers here are external nodes ([`Sim::set_external`]) running their own
//! endpoint and traversal with [`Hop`] addresses, as `peerwire::Node` does on a socket, and a TURN server
//! ([`Sim::add_turn_server`]) sits on the internet. Two symmetric NATs with random ports, which no punch gets
//! through, link only through TURN; two NATs that can punch, linked on TURN first, move to the direct path once the
//! policy ranks `Direct` first.
use peerwire::traversal::{Signal, Traversal};
use peerwire::{Hop, PathKind, Policy, Role, Topic, TurnServer};
use peerwire_core::{Address, Config, Endpoint, Event, Instant, Lane, Path, PeerId};
use peerwire_sim::{NatConfig, NetConfig, Sim, SimAddr};
use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;

type Addr = Hop<SimAddr>;
const TICK: Duration = Duration::from_millis(10);
const BASE_MS: u64 = 1_700_000_000_000;

struct Peer {
    node: usize,
    ep: Endpoint<Addr>,
    trav: Traversal<Addr>,
    topics: BTreeSet<Topic>,
    events: Vec<Event<Addr>>,
}

/// Two STUN servers, a TURN server and a signaling broker on the simulated internet.
struct World {
    sim: Sim,
    stun: Vec<SimAddr>,
    turn: SimAddr,
    peers: Vec<Peer>,
    /// (deliver at, peer, topic, payload)
    inflight: VecDeque<(Instant, usize, Topic, Vec<u8>)>,
}

impl World {
    fn new() -> World {
        let mut sim = Sim::new(NetConfig::default(), 7);
        let (first, second) = (sim.add_stun_server(), sim.add_stun_server());
        let turn = sim.add_turn_server("app", "secret");
        World { sim, stun: vec![first, second], turn, peers: Vec::new(), inflight: VecDeque::new() }
    }

    /// A peer behind a NAT like `nat`, under `policy`, with the TURN server if `turn`, in the room.
    fn add(&mut self, nat: NatConfig, turn: bool, policy: Policy) -> usize {
        let n = self.sim.add_nat(nat, None);
        let node = self.sim.add_node_behind(n, Config::default());
        self.sim.set_external(node);
        let identity = self.sim.nodes[node].identity.clone();
        let (now, seed) = (self.sim.now, [node as u8 + 1; 32]);
        let mut ep = Endpoint::new(&identity, Config::default(), seed);
        let (local, stun) = (vec![Hop::Net(self.sim.addr(node))], self.stun.iter().map(|s| Hop::Net(*s)).collect());
        let mut trav = Traversal::new(&identity, seed, BASE_MS, local, stun);
        trav.set_policy(now, &mut ep, Some(policy));
        if turn {
            let server = TurnServer::new(self.turn.to_string(), "app", "secret");
            trav.set_turn_servers(now, &mut ep, vec![(Hop::Net(self.turn), server)]);
        }
        trav.join_room(now, "pw-sim", "turn", Role::Peer, None);
        self.peers.push(Peer { node, ep, trav, topics: BTreeSet::new(), events: Vec::new() });
        self.peers.len() - 1
    }

    fn id(&self, p: usize) -> PeerId {
        self.peers[p].ep.peer_id()
    }

    /// One host-loop iteration per peer, as `Node::poll` does, then `TICK` of network.
    fn tick(&mut self) {
        let now = self.sim.now;
        while self.inflight.front().is_some_and(|m| m.0 <= now) {
            let (_, to, topic, payload) = self.inflight.pop_front().unwrap();
            let p = &mut self.peers[to];
            p.trav.handle_signal(now, &mut p.ep, topic, &payload);
        }
        for i in 0..self.peers.len() {
            let node = self.peers[i].node;
            let inbox = std::mem::take(&mut self.sim.nodes[node].stun_inbox);
            let p = &mut self.peers[i];
            for (from, data) in inbox {
                let from = Hop::Net(from);
                match p.trav.handle_turn(now, &from, &data) {
                    Some(Some((hop, payload))) => p.ep.handle_datagram(now, hop, &payload),
                    Some(None) => {}
                    None if p.trav.handle_stun(&from, &data) => {}
                    None => {
                        let hop = p.trav.hop(&p.ep, from, &data);
                        p.ep.handle_datagram(now, hop, &data);
                    }
                }
            }
            if p.ep.poll_timeout().is_some_and(|t| t <= now) {
                p.ep.handle_timeout(now);
            }
            if p.trav.poll_timeout().is_some_and(|t| t <= now) {
                p.trav.handle_timeout(now, &mut p.ep);
            }
            while let Some(e) = p.ep.poll_event() {
                p.trav.handle_event(now, &mut p.ep, &e);
                p.events.push(e);
            }
            let mut out = Vec::new();
            while let Some(t) = p.ep.poll_transmit(now) {
                match t.dst {
                    Hop::Turn { .. } => {
                        p.trav.send_turn(now, &t.dst, &t.payload);
                    }
                    dst => out.push((*dst.addr(), t.payload)),
                }
            }
            out.extend(std::iter::from_fn(|| p.trav.poll_transmit()).map(|t| (*t.dst.addr(), t.payload)));
            while p.trav.poll_birthday().is_some() {} // neither pair here takes a birthday punch
            let signals: Vec<Signal> = std::iter::from_fn(|| p.trav.poll_signal()).collect();
            for (dst, payload) in out {
                self.sim.send_raw(node, dst, payload);
            }
            for s in signals {
                self.signal(i, s);
            }
        }
        self.sim.run_for(TICK);
    }

    fn signal(&mut self, from: usize, s: Signal) {
        match s {
            Signal::Subscribe(t) => {
                self.peers[from].topics.insert(t);
            }
            Signal::Unsubscribe(t) => {
                self.peers[from].topics.remove(&t);
            }
            Signal::Publish(t, payload) => {
                let at = self.sim.now + Duration::from_millis(150);
                for (to, p) in self.peers.iter().enumerate() {
                    if to != from && p.topics.contains(&t) {
                        self.inflight.push_back((at, to, t, payload.clone()));
                    }
                }
            }
        }
    }

    fn run_until(&mut self, limit: Duration, mut done: impl FnMut(&World) -> bool) -> bool {
        let end = self.sim.now + limit;
        while self.sim.now < end {
            if done(self) {
                return true;
            }
            self.tick();
        }
        done(self)
    }

    fn path(&self, a: usize, b: usize) -> Option<Path<Addr>> {
        self.peers[a].ep.path(&self.id(b)).cloned()
    }

    fn linked(&self, a: usize, b: usize) -> bool {
        self.peers[a].ep.is_connected(&self.id(b)) && self.peers[b].ep.is_connected(&self.id(a))
    }

    /// Whether both ends are on a relayed address (TURN), or both on a network one.
    fn relayed(&self, a: usize, b: usize, relayed: bool) -> bool {
        let on = |p: Option<Path<Addr>>| matches!(p, Some(Path::Direct(h)) if h.relayed() == relayed);
        on(self.path(a, b)) && on(self.path(b, a))
    }
}

fn policy() -> Policy {
    Policy::new([PathKind::Direct, PathKind::Birthday, PathKind::Turn])
}

/// Two symmetric NATs with random ports: no punch links them, nor a birthday punch (neither side is a cone).
/// Without TURN the dial fails; with a TURN server at one side, the peers link through it: that side through its
/// allocation, the other at its relayed address. Messages cross.
#[test]
fn two_symmetric_nats_link_only_through_turn() {
    let mut w = World::new();
    let a = w.add(NatConfig::symmetric_random(), false, policy());
    let b = w.add(NatConfig::symmetric_random(), false, policy());
    assert!(!w.run_until(Duration::from_secs(20), |w| w.linked(a, b)), "no TURN: no link");

    let mut w = World::new();
    let a = w.add(NatConfig::symmetric_random(), true, policy());
    let b = w.add(NatConfig::symmetric_random(), false, policy());
    assert!(
        w.run_until(Duration::from_secs(20), |w| w.linked(a, b)),
        "{:?} {:?}",
        w.peers[a].events,
        w.peers[b].events
    );
    assert!(w.relayed(a, b, true), "{:?} {:?}", w.path(a, b), w.path(b, a));
    assert!(matches!(w.path(a, b), Some(Path::Direct(Hop::Turn { .. }))));
    assert!(matches!(w.path(b, a), Some(Path::Direct(Hop::Relayed(_)))));
    let bid = w.id(b);
    w.peers[a].ep.send(&bid, Lane::Reliable, b"across two symmetric NATs").unwrap();
    w.run_until(Duration::from_secs(2), |_| false);
    let got = |e: &Event<Addr>| matches!(e, Event::Message { data, .. } if data == b"across two symmetric NATs");
    assert!(w.peers[b].events.iter().any(got));
    assert!(w.relayed(a, b, true), "and stays on TURN: nothing direct works");
    let turn = w.turn;
    let st = w.sim.turn_server(turn).unwrap().stats;
    assert_eq!((st.allocations, st.auth_failures), (1, 0));
}

/// A TURN server that is the peers' only STUN server too: its Binding answers reach STUN, so a learns its NAT's
/// mapping from it (the client address its allocation is for), and a links through it.
#[test]
fn one_server_for_stun_and_turn() {
    let mut w = World::new();
    w.stun = vec![w.turn];
    let a = w.add(NatConfig::symmetric_random(), true, policy());
    let b = w.add(NatConfig::symmetric_random(), false, policy());
    let up = |w: &World| w.linked(a, b) && w.peers[a].trav.public_addr().is_some();
    assert!(w.run_until(Duration::from_secs(20), up), "{:?}", w.peers[a].trav.public_addr());
    assert!(w.relayed(a, b, true), "{:?} {:?}", w.path(a, b), w.path(b, a));
    let (turn, relayed) = (w.turn, w.peers[a].trav.turn_status()[0].1.unwrap());
    let server = w.sim.turn_server(turn).unwrap();
    let seen = SimAddr::from_socket_addr(server.client_of(relayed).unwrap()).unwrap();
    assert!(server.stats.bindings > 0);
    assert_eq!(w.peers[a].trav.public_addr(), Some(&Hop::Net(seen)));
}

/// Two NATs that punch through each other (full cone and port-restricted), linked on TURN first (`[Turn]`): with
/// `Direct` ranked first, live, both ends move to the direct path (`PathChanged`), and stay linked.
#[test]
fn a_turn_link_moves_to_direct_where_the_nats_let_it() {
    let mut w = World::new();
    let only = || Policy::new([PathKind::Turn]);
    let a = w.add(NatConfig::full_cone(), true, only());
    let b = w.add(NatConfig::port_restricted(), false, only());
    assert!(w.run_until(Duration::from_secs(20), |w| w.linked(a, b)));
    w.run_until(Duration::from_secs(5), |_| false);
    assert!(w.relayed(a, b, true), "kept on TURN: {:?} {:?}", w.path(a, b), w.path(b, a));
    for p in [a, b] {
        let (now, peer) = (w.sim.now, &mut w.peers[p]);
        peer.trav.set_policy(now, &mut peer.ep, Some(Policy::new([PathKind::Direct, PathKind::Turn])));
    }
    assert!(w.run_until(Duration::from_secs(20), |w| w.relayed(a, b, false)), "{:?} {:?}", w.path(a, b), w.path(b, a));
    let moved = |e: &Event<Addr>| matches!(e, Event::PathChanged { path: Path::Direct(Hop::Net(_)), .. });
    assert!(w.peers[a].events.iter().any(moved) && w.peers[b].events.iter().any(moved));
    assert!(w.linked(a, b) && !w.peers[a].events.iter().any(|e| matches!(e, Event::Disconnected { .. })));
}
