//! Shared docs (peerwire-sync) converge: seeded, so a failing seed replays. Members write at random (map sets and
//! deletes, set adds and deletes, log appends) on real endpoints over simulated links that lose, duplicate and
//! delay packets (the reliable lane hides that); links are cut and made again (a partition healed) and a member
//! joins late. Without the lane, doc frames delivered in any order and any number of times converge too. A star,
//! every member linked to the host alone as in a Godot room, converges through a hub, and a member whose link
//! stalls is cut before what waits for it fills the host's memory.

use peerwire_core::{Config, Event as Net, Instant, Lane, PeerId, SendError};
use peerwire_sim::{NetConfig, Sim};
use peerwire_sync::{DocId, Docs, Event, Kind, Options, Route, MESSAGE_BYTES, OUTBOX_BYTES};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

const KEYS: [&str; 6] = ["k0", "k1", "k2", "ключ", "😀", ""];

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// What member `d` holds of the doc, as doc.js lists it.
fn held(docs: &mut Docs, id: DocId) -> String {
    let d = docs.doc(id).unwrap();
    [(Kind::Map, "meta"), (Kind::Set, "strokes"), (Kind::Log, "chat")].map(|(k, c)| d.entries_json(k, c)).join("\n")
}

/// A random write by member `d`; `big` writes 50 KB values, on ten keys.
fn write(rng: &mut ChaCha8Rng, docs: &mut Docs, id: DocId, big: bool) {
    let r = rng.next_u32();
    let mut d = docs.doc(id).unwrap();
    if big {
        let v = format!("\"{}{r}\"", "x".repeat(50_000));
        assert!(d.map_set("big", &format!("k{}", r % 10), &v).unwrap());
        return;
    }
    let key = KEYS[r as usize % KEYS.len()];
    let v = format!("{{\"n\":{},\"r\":[{},-0,1e21]}}", r >> 8, r % 7);
    match r % 8 {
        0..=2 => assert!(d.map_set("meta", key, &v).unwrap()),
        3 => {
            d.map_delete("meta", key).unwrap();
        }
        4 | 5 => assert!(d.set_add("strokes", &v).unwrap().is_some()),
        6 => {
            let items = d.entries(Kind::Set, "strokes");
            if !items.is_empty() {
                let id = items[r as usize % items.len()].key.clone();
                assert!(d.set_delete("strokes", &id).unwrap());
            }
        }
        _ => assert!(d.log_append("chat", &v).unwrap().is_some()),
    }
}

/// Members on simulated endpoints, each with the doc "board" open, its host loop driven here.
struct Room {
    sim: Sim,
    docs: Vec<Docs>,
    ids: Vec<DocId>,
    index: BTreeMap<PeerId, usize>,
    rng: ChaCha8Rng,
    overflows: Vec<(usize, PeerId)>,
    /// The most a member had waiting for another, in bytes.
    most_queued: usize,
}

impl Room {
    fn new(n: usize, net: NetConfig, seed: u64, hub: Option<usize>) -> Room {
        let mut r = Room {
            sim: Sim::new(net, seed),
            docs: Vec::new(),
            ids: Vec::new(),
            index: BTreeMap::new(),
            rng: ChaCha8Rng::seed_from_u64(seed),
            overflows: Vec::new(),
            most_queued: 0,
        };
        for i in 0..n {
            r.add(hub == Some(i));
        }
        r
    }

    fn add(&mut self, hub: bool) -> usize {
        let i = self.sim.add_node(Config::default());
        let me = self.sim.peer_id(i);
        let mut seed = [0u8; 32];
        self.rng.fill_bytes(&mut seed);
        let mut docs = Docs::new(me, seed);
        let id = docs.open(self.sim.now, "board", Options { hub, ..Options::default() }).unwrap();
        docs.doc(id).unwrap().log_keep("chat", 40).unwrap();
        self.docs.push(docs);
        self.ids.push(id);
        self.index.insert(me, i);
        i
    }

    fn link(&mut self, i: usize, j: usize) {
        self.sim.connect(i, j).unwrap();
    }

    fn cut(&mut self, i: usize, j: usize) {
        let p = self.sim.peer_id(j);
        self.sim.nodes[i].endpoint.disconnect(&p);
    }

    fn write(&mut self, i: usize, big: bool) {
        write(&mut self.rng, &mut self.docs[i], self.ids[i], big);
    }

    fn held(&mut self, i: usize) -> String {
        held(&mut self.docs[i], self.ids[i])
    }

    /// The host loop of every member, once: link events in, timers, events out, messages out.
    fn serve(&mut self) {
        let now = self.sim.now;
        for i in 0..self.docs.len() {
            let docs = &mut self.docs[i];
            for ev in self.sim.take_events(i) {
                match ev {
                    Net::Connected { peer, .. } => docs.joined(now, peer, Route::Direct),
                    Net::Disconnected { peer, .. } => docs.left(now, peer),
                    Net::Message { peer, lane: Lane::Reliable, data } => assert!(docs.receive(now, peer, &data)),
                    _ => {}
                }
            }
            if docs.poll_timeout().is_some_and(|t| t <= now) {
                docs.handle_timeout(now);
            }
            while let Some(e) = docs.poll_event() {
                if let Event::Overflow(p) = e {
                    self.overflows.push((i, p));
                    self.sim.nodes[i].endpoint.disconnect(&p);
                }
            }
            while let Some((p, m)) = docs.poll_transmit() {
                match self.sim.send(i, self.index[&p], Lane::Reliable, &m) {
                    Ok(()) | Err(SendError::NotConnected) => {}
                    Err(SendError::WouldBlock) => docs.retry(p, m),
                    Err(e) => panic!("{e:?}"),
                }
            }
            for p in self.index.keys() {
                self.most_queued = self.most_queued.max(docs.queued(p));
            }
        }
    }

    /// Runs `d` of virtual time.
    fn pump(&mut self, d: Duration) {
        let end = self.sim.now + d;
        loop {
            self.serve();
            let now = self.sim.now;
            if now >= end {
                return;
            }
            let mut until = end;
            for docs in &self.docs {
                if let Some(t) = docs.poll_timeout() {
                    until = until.min(t.max(now));
                }
                // Messages a link refused go again once the sim has moved a little.
                if self.index.keys().any(|p| docs.queued(p) > 0) {
                    until = until.min(now + ms(2));
                }
            }
            let n = self.docs.len();
            self.sim.run_until(until - now, |s| (0..n).any(|i| !s.nodes[i].events.is_empty()));
        }
    }

    fn converged(&mut self, members: &[usize]) -> bool {
        let first = self.held(members[0]);
        members.iter().all(|&i| self.held(i) == first)
    }
}

fn lossy() -> NetConfig {
    NetConfig { latency: ms(30), jitter: ms(40), loss: 0.05, duplicate: 0.05, in_order: false }
}

/// Five members, all linked, write at random while packets are lost, duplicated and reordered.
#[test]
fn a_mesh_converges_through_loss_duplicates_and_jitter() {
    for seed in 1..=3 {
        let mut r = Room::new(5, lossy(), seed, None);
        for i in 0..5 {
            for j in i + 1..5 {
                r.link(i, j);
            }
        }
        r.pump(ms(2000));
        for _ in 0..150 {
            let i = r.rng.next_u32() as usize % 5;
            r.write(i, false);
            let wait = r.rng.next_u32() as u64 % 150;
            r.pump(ms(wait));
        }
        r.pump(ms(20_000));
        assert!(r.converged(&[0, 1, 2, 3, 4]), "seed {seed}:\n{}\n{}", r.held(0), r.held(1));
        for i in 0..5 {
            let d = r.docs[i].doc(r.ids[i]).unwrap();
            assert!(
                !d.entries(Kind::Map, "meta").is_empty() && d.sync().json().contains("synced"),
                "seed {seed}: {}",
                d.sync().json()
            );
        }
    }
}

/// Two halves write apart, the same keys too, then link again: they converge, and a set made on one side wins
/// over a delete of the same key made on the other (add-wins).
#[test]
fn a_partition_heals_when_members_link_again() {
    for seed in 1..=3 {
        let mut r = Room::new(4, NetConfig::default(), seed, None);
        for i in 0..4 {
            for j in i + 1..4 {
                r.link(i, j);
            }
        }
        r.pump(ms(1000));
        r.docs[0].doc(r.ids[0]).unwrap().map_set("meta", "x", "\"before\"").unwrap();
        r.pump(ms(1000));
        for (i, j) in [(0, 2), (0, 3), (1, 2), (1, 3)] {
            r.cut(i, j);
        }
        r.pump(ms(1000));
        r.docs[0].doc(r.ids[0]).unwrap().map_delete("meta", "x").unwrap();
        r.docs[3].doc(r.ids[3]).unwrap().map_set("meta", "x", "\"set apart\"").unwrap();
        for _ in 0..80 {
            let i = r.rng.next_u32() as usize % 4;
            r.write(i, false);
            r.pump(ms(50));
        }
        assert!(!r.converged(&[0, 2]), "seed {seed}: the halves were apart");
        for (i, j) in [(0, 2), (0, 3), (1, 2), (1, 3)] {
            r.link(i, j);
        }
        r.pump(ms(15_000));
        assert!(r.converged(&[0, 1, 2, 3]), "seed {seed}");
        assert_eq!(r.docs[0].doc(r.ids[0]).unwrap().get(Kind::Map, "meta", "x"), Some("\"set apart\""), "seed {seed}");
    }
}

/// A member that joins a doc of a thousand entries late is sent it about once, not once per member.
#[test]
fn a_late_joiner_catches_up_once() {
    let mut r = Room::new(3, NetConfig::default(), 7, None);
    for (i, j) in [(0, 1), (0, 2), (1, 2)] {
        r.link(i, j);
    }
    r.pump(ms(1000));
    for k in 0..1000 {
        let i = k % 3;
        let mut d = r.docs[i].doc(r.ids[i]).unwrap();
        d.set_add("strokes", &format!("{{\"k\":{k},\"pts\":[{}]}}", "1,".repeat(40) + "1")).unwrap();
        if k.is_multiple_of(50) {
            r.pump(ms(100));
        }
    }
    r.pump(ms(5000));
    let size = r.docs[0].doc(r.ids[0]).unwrap().snapshot().len() as u64;
    let late = r.add(false);
    for i in 0..3 {
        r.link(late, i);
    }
    r.pump(ms(20_000));
    assert!(r.converged(&[0, 1, 2, late]));
    let got = r.sim.nodes[late].rx_bytes;
    assert!(got > size && got < size * 3 / 2, "the late joiner was sent {got} bytes for a doc of {size}");
}

/// The frames alone, no lane: delivered in any order and any number of times, members still converge.
#[test]
fn frames_in_any_order_and_any_number_of_times_converge() {
    for seed in 1..=4 {
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let peers: Vec<PeerId> = (1..=4).map(|i| PeerId([i; 32])).collect();
        let mut docs: Vec<Docs> = peers.iter().map(|&p| Docs::new(p, [p.0[0]; 32])).collect();
        let mut now = Instant::from_millis(0);
        let ids: Vec<DocId> = docs.iter_mut().map(|d| d.open(now, "board", Options::default()).unwrap()).collect();
        for (i, d) in docs.iter_mut().enumerate() {
            for (j, &p) in peers.iter().enumerate() {
                if i != j {
                    d.joined(now, p, Route::Direct);
                }
            }
        }
        let mut flight: Vec<(PeerId, PeerId, Vec<u8>)> = Vec::new();
        let step = |docs: &mut Vec<Docs>, flight: &mut Vec<_>, rng: &mut ChaCha8Rng, now: Instant| {
            for (i, d) in docs.iter_mut().enumerate() {
                while let Some((to, m)) = d.poll_transmit() {
                    flight.push((peers[i], to, m));
                }
                while d.poll_event().is_some() {}
            }
            for _ in 0..1 + rng.next_u32() % 4 {
                if flight.is_empty() {
                    break;
                }
                let k = rng.next_u32() as usize % flight.len();
                let (from, to, m) = flight.swap_remove(k);
                if rng.next_u32().is_multiple_of(5) {
                    flight.push((from, to, m.clone()));
                }
                let j = peers.iter().position(|&p| p == to).unwrap();
                docs[j].receive(now, from, &m);
            }
        };
        for _ in 0..300 {
            let i = rng.next_u32() as usize % 4;
            write(&mut rng, &mut docs[i], ids[i], false);
            step(&mut docs, &mut flight, &mut rng, now);
        }
        // Everything left lands, in any order, and timers run out.
        while !flight.is_empty() || docs.iter().any(|d| d.poll_timeout().is_some()) {
            step(&mut docs, &mut flight, &mut rng, now);
            if flight.is_empty() {
                now = docs.iter().filter_map(|d| d.poll_timeout()).min().unwrap_or(now).max(now);
                for d in &mut docs {
                    d.handle_timeout(now);
                }
            }
        }
        let first = held(&mut docs[0], ids[0]);
        for i in 1..4 {
            assert_eq!(held(&mut docs[i], ids[i]), first, "seed {seed}: member {i}");
        }
    }
}

/// A Godot room: every member linked to the host alone, which runs the doc as a hub. Members' writes reach each
/// other through it. Then one member's link stalls (none of its packets get through) while a member writes a
/// lot: the host never holds more than OUTBOX_BYTES for it, cuts it, and once it links again it catches up.
#[test]
fn a_star_converges_through_its_hub_and_a_stalled_member_is_cut() {
    let mut r = Room::new(5, NetConfig::default(), 11, Some(0));
    for i in 1..5 {
        r.link(i, 0);
    }
    r.pump(ms(1000));
    for _ in 0..120 {
        let i = 1 + r.rng.next_u32() as usize % 4;
        r.write(i, false);
        r.pump(ms(30));
    }
    r.pump(ms(5000));
    assert!(r.converged(&[0, 1, 2, 3, 4]));
    assert!(!r.held(0).contains("[]\n[]"), "they wrote");
    // Member 4 stalls: its packets are dropped, so the host's link to it takes nothing more.
    let stalled = Rc::new(Cell::new(true));
    let (addr, s) = (r.sim.addr(4), stalled.clone());
    r.sim.hook = Some(Box::new(move |from, _, _| !(s.get() && from == addr)));
    // 30 MB, more than the host's link (4 MiB) and its outbox (16 MiB) can hold for it, well within the 30 s a
    // silent link takes to time out.
    for _ in 0..60 {
        for _ in 0..10 {
            r.write(1, true);
        }
        r.pump(ms(250));
    }
    assert_eq!(r.overflows.iter().filter(|(i, _)| *i == 0).map(|(_, p)| *p).collect::<Vec<_>>(), [r.sim.peer_id(4)]);
    assert!(r.most_queued <= OUTBOX_BYTES + MESSAGE_BYTES, "{} bytes waited", r.most_queued);
    assert_eq!(r.docs[0].queued(&r.sim.peer_id(4)), 0);
    r.pump(ms(2000));
    assert!(r.converged(&[0, 1, 2, 3]));
    // It comes back.
    stalled.set(false);
    r.pump(ms(1000));
    r.link(4, 0);
    r.pump(ms(20_000));
    assert!(r.converged(&[0, 1, 2, 3, 4]));
}

/// A hub sends what a member's frame added to its other members, never back to that member.
#[test]
fn a_hub_never_sends_a_frame_back_to_its_source() {
    let (h, a, b) = (PeerId([1; 32]), PeerId([2; 32]), PeerId([3; 32]));
    let now = Instant::from_millis(0);
    let mut hub = Docs::new(h, [1; 32]);
    let hid = hub.open(now, "board", Options { hub: true, ..Options::default() }).unwrap();
    let mut leaf = Docs::new(a, [2; 32]);
    let lid = leaf.open(now, "board", Options::default()).unwrap();
    hub.joined(now, a, Route::Direct);
    hub.joined(now, b, Route::Direct);
    leaf.joined(now, h, Route::Direct);
    while hub.poll_transmit().is_some() {}
    while leaf.poll_transmit().is_some() {}
    leaf.doc(lid).unwrap().map_set("meta", "k", "1").unwrap();
    let (to, m) = leaf.poll_transmit().unwrap();
    assert_eq!(to, h);
    assert!(hub.receive(now, a, &m));
    let sent: Vec<(PeerId, Vec<u8>)> = std::iter::from_fn(|| hub.poll_transmit()).collect();
    assert_eq!(sent.iter().map(|(p, _)| *p).collect::<Vec<_>>(), [b], "only to the other member");
    let text = String::from_utf8_lossy(&sent[0].1[1..]).to_string();
    assert!(text.contains("\"t\":3") && !text.contains("\"q\"") && text.contains("\"k\""), "{text}");
    assert_eq!(hub.doc(hid).unwrap().get(Kind::Map, "meta", "k"), Some("1"));
    // The same frame again adds nothing: nothing goes on.
    assert!(hub.receive(now, a, &m));
    assert!(hub.poll_transmit().is_none());
}
