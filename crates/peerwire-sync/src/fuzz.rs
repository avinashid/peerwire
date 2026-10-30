//! Fuzz tests (`peerwire_sim::fuzz`; `PW_FUZZ_SCALE` and `PW_FUZZ_SEED` for a longer soak). The seeds are the
//! frames of doc.js's corpus (tests/vectors/frames.json), mutated. The gate also runs them in a debug build, where
//! integer overflow panics.

use crate::json::Tree;
use crate::{Docs, Event, Kind, Options, Route, DOC_TAG, MAX_DOC_LENGTH, MAX_ENTRIES};
use peerwire_core::{Instant, PeerId};
use peerwire_sim::fuzz::{cases, mutate, run, Rng};
use serde_json::Value;

/// The corpus's frames that doc.js keeps, as bytes.
fn seeds() -> Vec<Vec<u8>> {
    let v: Vec<Value> = serde_json::from_str(include_str!("../tests/vectors/frames.json")).unwrap();
    v.iter()
        .filter(|c| !c["verdict"].is_null())
        .filter_map(|c| c["parts"].as_array())
        .filter(|p| p.iter().all(Value::is_string))
        .map(|p| p.iter().map(|s| s.as_str().unwrap()).collect::<String>().into_bytes())
        .collect()
}

/// Mutates a seed, now and then splicing in another's bytes or a JSON fragment.
fn gen(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    let mut b = rng.pick(seeds).clone();
    if rng.one_in(4) {
        let at = rng.below(b.len() + 1);
        let bit: &[u8] =
            rng.pick(&[&b"[["[..], b"]]", b"\"\\ud800\"", b"1e400", b"-0", b",\"x\":[0,1,2]", b"null", b"{}", b"\xff"]);
        b.splice(at..at, bit.iter().copied());
    }
    mutate(rng, &mut b, 8192);
    b
}

/// What the reader reads it writes back as text that reads back the same; it never panics.
#[test]
fn fuzz_json_reader() {
    let seeds = seeds();
    run(
        "sync json",
        cases(20_000),
        |rng| gen(rng, &seeds),
        |b| {
            if let Some(t) = Tree::parse(b) {
                let text = t.text(t.root());
                let again = Tree::parse(text.as_bytes()).expect("its own text reads");
                assert_eq!(again.text(again.root()), text);
            }
        },
    );
}

/// One member's docs take mutated frames from a member, an ask now and then, and stay within their caps. Half
/// the frames are for "b", which this member runs as a hub, so what it relays (and charges) is fuzzed too: half
/// of those are seeds as they are, which a hub opened afresh every 100 frames takes and relays, so that the mutated
/// ones land on what they brought.
#[test]
fn fuzz_doc_frames() {
    let seeds = seeds();
    let to_hub: Vec<Vec<u8>> = seeds
        .iter()
        .map(|s| String::from_utf8(s.clone()).unwrap().replacen("\"d\":\"board\"", "\"d\":\"b\"", 1).into_bytes())
        .collect();
    let (me, peer, other) = (PeerId([0xcc; 32]), PeerId([0xaa; 32]), PeerId([0xbb; 32]));
    let mut docs = Docs::new(me, [9; 32]);
    let mut t = 0;
    let hub = Options { hub: true, ..Options::default() };
    let board = docs.open(Instant::from_millis(t), "board", Options::default()).unwrap();
    let mut b_id = docs.open(Instant::from_millis(t), "b", hub).unwrap();
    docs.joined(Instant::from_millis(t), peer, Route::Peer);
    docs.joined(Instant::from_millis(t), other, Route::Direct);
    let mut relays = 0;
    run(
        "sync frames",
        cases(20_000),
        |rng| match rng.below(4) {
            0 => rng.pick(&to_hub).clone(),
            1 => gen(rng, &to_hub),
            _ => gen(rng, &seeds),
        },
        |b| {
            t += 7;
            let now = Instant::from_millis(t);
            if t % 700 == 0 {
                docs.close(b_id);
                b_id = docs.open(now, "b", hub).unwrap();
            }
            let mut m = vec![DOC_TAG];
            m.extend_from_slice(b);
            assert!(docs.receive(now, peer, &m));
            if docs.poll_timeout().is_some_and(|d| d <= now) {
                docs.handle_timeout(now);
            }
            while let Some((to, out)) = docs.poll_transmit() {
                let f = Tree::parse(&out[1..]).expect("our frames are JSON");
                assert!(f.field(f.root(), "d").is_some());
                relays += (to == other && f.field(f.root(), "q").is_none()) as usize;
            }
            while let Some(e) = docs.poll_event() {
                // Cut for what it had relayed, it joins again, so the frames after are read too.
                if e == Event::Overflow(peer) {
                    docs.joined(now, peer, Route::Peer);
                }
            }
        },
    );
    assert!(relays > cases(20_000) / 100, "{relays} frames relayed");
    let d = docs.doc(board).unwrap();
    let held: usize =
        [Kind::Map, Kind::Set, Kind::Log].iter().map(|&k| d.entries(k, "c").len() + d.entries(k, "meta").len()).sum();
    assert!(held <= MAX_ENTRIES);
    assert!(d.snapshot().len() < 3 * MAX_DOC_LENGTH);
}

/// A copy, mutated, opens or is refused, never panics; the copy itself opens to what it was taken from.
#[test]
fn fuzz_copies() {
    let mut docs = Docs::new(PeerId([0xaa; 32]), [3; 32]);
    let id = docs.open(Instant::from_millis(0), "b", Options::default()).unwrap();
    let mut d = docs.doc(id).unwrap();
    for i in 0..20 {
        d.map_set("c", &format!("k{i}"), &format!("[{i},\"\\ud800\",-0]")).unwrap();
        d.set_add("s", "{\"a\":1}").unwrap();
    }
    d.map_delete("c", "k3").unwrap();
    let copy = d.snapshot();
    let full = docs.doc(id).unwrap().entries_json(Kind::Map, "c");
    run(
        "sync copies",
        cases(5_000),
        |rng| {
            let mut b = copy.clone();
            mutate(rng, &mut b, 16384);
            b
        },
        |b| {
            let mut other = Docs::new(PeerId([0xbb; 32]), [4; 32]);
            if let Ok(id) = other.open(Instant::from_millis(0), "b", Options { copy: Some(b), hub: false }) {
                other.doc(id).unwrap().snapshot();
            }
        },
    );
    let mut other = Docs::new(PeerId([0xbb; 32]), [4; 32]);
    let id = other.open(Instant::from_millis(0), "b", Options { copy: Some(&copy), hub: false }).unwrap();
    assert_eq!(other.doc(id).unwrap().entries_json(Kind::Map, "c"), full);
}
