//! Shared docs (peerwire-sync) at size: the numbers in docs/PERFORMANCE.md §4.
//!
//! ```text
//! cargo run --release -p peerwire-sim --example docs
//! ```
//!
//! 1. CPU, in one process (wall clock, so it depends on the machine): a member writes 10,000 map keys of 100
//!    characters, another applies them as they come (deltas), and a third joins late and catches up from the first
//!    (answer pieces and MORE acks). Also the bytes each entry takes on the wire.
//! 2. Catch-up time in the simulator (virtual time, the same on any machine): the same doc, over real endpoints on
//!    a 20 ms link with a 10 Mbit/s uplink, to a member that links directly, and to one whose answers come in the
//!    pieces of a relayed route (32K characters, not 128K).

use peerwire_core::{Config, Event as Net, Instant, Lane, PeerId, SendError};
use peerwire_sim::{NetConfig, Sim};
use peerwire_sync::{DocId, Docs, Kind, Options, Route, State};
use std::time::{Duration, Instant as Wall};

const ENTRIES: usize = 10_000;
const VALUE: usize = 100;

fn value(i: usize) -> String {
    format!("\"{}{i:05}\"", "v".repeat(VALUE - 5))
}

/// Delivers every message between `members`, in process, until none is left. Returns the bytes sent.
fn deliver(members: &mut [(PeerId, Docs)], now: Instant) -> usize {
    let mut bytes = 0;
    loop {
        let mut sent = Vec::new();
        for (from, docs) in members.iter_mut() {
            while let Some((to, m)) = docs.poll_transmit() {
                sent.push((*from, to, m));
            }
            while docs.poll_event().is_some() {}
        }
        if sent.is_empty() {
            return bytes;
        }
        for (from, to, m) in sent {
            bytes += m.len();
            let k = members.iter().position(|(p, _)| *p == to).unwrap();
            members[k].1.receive(now, from, &m);
        }
    }
}

fn cpu() {
    let now = Instant::from_millis(0);
    let ids: Vec<PeerId> = (1..=3).map(|i| PeerId([i; 32])).collect();
    let mut members: Vec<(PeerId, Docs)> = ids.iter().map(|&p| (p, Docs::new(p, [p.0[0]; 32]))).collect();
    let doc: Vec<DocId> = members.iter_mut().map(|(_, d)| d.open(now, "board", Options::default()).unwrap()).collect();
    members[0].1.joined(now, ids[1], Route::Direct);
    members[1].1.joined(now, ids[0], Route::Direct);
    deliver(&mut members[..2], now);

    // A writes; B applies each delta as it comes.
    let t = Wall::now();
    for i in 0..ENTRIES {
        assert!(members[0].1.doc(doc[0]).unwrap().map_set("m", &format!("k{i}"), &value(i)).unwrap());
    }
    let write = t.elapsed();
    let deltas: Vec<Vec<u8>> = std::iter::from_fn(|| members[0].1.poll_transmit()).map(|(_, m)| m).collect();
    let delta_bytes: usize = deltas.iter().map(Vec::len).sum();
    let t = Wall::now();
    for m in &deltas {
        members[1].1.receive(now, ids[0], m);
    }
    let apply = t.elapsed();
    while members[1].1.poll_event().is_some() {}
    let snapshot = members[0].1.doc(doc[0]).unwrap().snapshot().len();

    // C joins late and catches up from A.
    members[0].1.joined(now, ids[2], Route::Direct);
    members[2].1.joined(now, ids[0], Route::Direct);
    let t = Wall::now();
    let mut sent = 0;
    let mut a_c = [members.remove(0), members.remove(1)];
    sent += deliver(&mut a_c, now);
    let catchup = t.elapsed();
    let held = a_c[1].1.doc(doc[2]).unwrap().entries(Kind::Map, "m").len();
    assert_eq!(held, ENTRIES);
    assert_eq!(a_c[1].1.doc(doc[2]).unwrap().sync().state, State::Synced);

    let rate = |d: Duration| ENTRIES as f64 / d.as_secs_f64();
    println!("cpu ({ENTRIES} map keys of {VALUE} characters, one thread):");
    println!("  local writes        {:>8.0} /s  ({:.1} ms)", rate(write), write.as_secs_f64() * 1e3);
    println!(
        "  deltas applied      {:>8.0} /s  ({:.1} ms; {} bytes each)",
        rate(apply),
        apply.as_secs_f64() * 1e3,
        delta_bytes / ENTRIES
    );
    println!(
        "  late joiner         {:>8.0} /s  ({:.1} ms, both sides; {} bytes sent, {:.0} per entry)",
        rate(catchup),
        catchup.as_secs_f64() * 1e3,
        sent,
        sent as f64 / ENTRIES as f64
    );
    println!("  snapshot            {snapshot} bytes ({:.0} per entry)", snapshot as f64 / ENTRIES as f64);
}

/// Virtual time until a member that links to a holder of the doc has caught up.
fn sim(route: Route) -> (Duration, u64) {
    let net = NetConfig { latency: Duration::from_millis(20), ..NetConfig::default() };
    let mut sim = Sim::new(net, 1);
    let (a, b) = (sim.add_node(Config::default()), sim.add_node(Config::default()));
    for n in [a, b] {
        sim.nodes[n].uplink_bps = Some(10_000_000);
    }
    let ids = [sim.peer_id(a), sim.peer_id(b)];
    let mut docs: Vec<Docs> = ids.iter().map(|&p| Docs::new(p, [p.0[0]; 32])).collect();
    let doc: Vec<DocId> = docs.iter_mut().map(|d| d.open(sim.now, "board", Options::default()).unwrap()).collect();
    for i in 0..ENTRIES {
        docs[0].doc(doc[0]).unwrap().map_set("m", &format!("k{i}"), &value(i)).unwrap();
    }
    sim.connect(b, a).unwrap();
    let start = sim.now;
    loop {
        let now = sim.now;
        for (i, d) in docs.iter_mut().enumerate() {
            for e in sim.take_events(i) {
                match e {
                    Net::Connected { peer, .. } => d.joined(now, peer, route),
                    Net::Message { peer, lane: Lane::Reliable, data } => {
                        d.receive(now, peer, &data);
                    }
                    _ => {}
                }
            }
            if d.poll_timeout().is_some_and(|t| t <= now) {
                d.handle_timeout(now);
            }
            while d.poll_event().is_some() {}
            while let Some((p, m)) = d.poll_transmit() {
                let j = if p == ids[0] { 0 } else { 1 };
                if let Err(SendError::WouldBlock) = sim.send(i, j, Lane::Reliable, &m) {
                    d.retry(p, m);
                }
            }
        }
        let held = docs[1].doc(doc[1]).unwrap().entries(Kind::Map, "m").len();
        if held == ENTRIES {
            return (sim.now - start, sim.nodes[b].rx_bytes);
        }
        let limit = docs.iter().filter_map(|d| d.poll_timeout()).min().map_or(Duration::from_millis(5), |t| t - now);
        sim.run_until(limit.min(Duration::from_millis(5)), |s| {
            !s.nodes[0].events.is_empty() || !s.nodes[1].events.is_empty()
        });
        assert!(sim.now - start < Duration::from_secs(120), "no catch-up");
    }
}

fn main() {
    cpu();
    println!("simulator (20 ms one way, 10 Mbit/s uplinks, virtual time from the link's first packet):");
    for (name, route) in [("direct", Route::Direct), ("relayed pieces", Route::Peer)] {
        let (t, rx) = sim(route);
        println!(
            "  late joiner, {name:<14} {:>6.0} ms  ({} bytes received, link setup included)",
            t.as_secs_f64() * 1e3,
            rx
        );
    }
}
