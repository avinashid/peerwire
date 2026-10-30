//! Golden vectors: peerwire-sync against doc.js, both ways.
//!
//! - `session_replays_doc_js`: tests/vectors/session.json is a session of doc.js members in fake rooms, made by
//!   `packages/js/test/rust_doc.mjs` (which says how its network and timers go). The members marked `rust` are
//!   replayed here with [`Docs`]: each must send the frames doc.js sent, byte for byte, round by round, give the
//!   same results and events, and hold the same docs at each checkpoint. The doc.js-only members send what they
//!   sent there, so Rust applies frames doc.js made (asks with `z`, keys with lone surrogates, a doc.js copy).
//! - `rust_session_applies_in_doc_js`: a session of Rust members, whose frames to one of them that only listens
//!   are tests/vectors/rust.json, with what it held at the end; rust_doc.mjs applies them to doc.js and checks it
//!   holds the same. `PW_WRITE_VECTORS=1` writes the file; otherwise it must be what this makes now.

use peerwire_core::{Instant, PeerId};
use peerwire_sync::{DocId, Docs, Event, Kind, Options, Route, DOC_TAG};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/");

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// rust_doc.mjs `short`: a text Rust only compares, as the vector keeps it.
fn short(s: &str) -> String {
    if s.len() <= 96 {
        return s.to_string();
    }
    format!("sha256:{}:{}", hex(&Sha256::digest(s.as_bytes())[..16]), s.len())
}

fn same(expected: &str, actual: &str) -> bool {
    expected == actual || expected == short(actual)
}

fn id_of(name: &str) -> PeerId {
    PeerId([u8::from_str_radix(&name.repeat(2), 16).unwrap(); 32])
}

fn route_of(kind: &str) -> Route {
    match kind {
        "peer" => Route::Peer,
        "signaling" => Route::Signaling,
        _ => Route::Direct,
    }
}

fn kind_of(t: &str) -> Kind {
    match t {
        "m" => Kind::Map,
        "s" => Kind::Set,
        _ => Kind::Log,
    }
}

/// rust_doc.mjs `flood`: a frame of many writers or many ranges, from its parameters.
fn flood(what: &str, n: u64, offset: u64) -> String {
    let w = |i: u64| format!("\"{i:064x}0000000f\"");
    let list = |f: &dyn Fn(u64) -> String, n: u64| (0..n).map(f).collect::<Vec<_>>().join(",");
    let one = ["ranges", "unranges", "big", "unbig"].contains(&what);
    let ws = list(&|i| w(offset + i), if one { 1 } else { n });
    let body = match what {
        "writers" => format!("\"e\":[{}]", list(&|i| format!("[{i},1,1,\"s\",\"fw\",null,{}]", offset + i), n)),
        "dead" => format!("\"x\":[{}]", list(&|i| format!("[{i},1,1]"), n)),
        "late" => format!("\"x\":[{}]", list(&|i| format!("[{i},5,5]"), n)),
        "unranges" => format!("\"x\":[[0,1,{}]]", 2 * n),
        "unbig" => format!("\"x\":[[0,1,{n}]]"),
        "big" => {
            let v = "x".repeat(1000);
            format!("\"e\":[{}]", list(&|k| format!("[0,{},1,\"m\",\"fb\",\"k{k}\",\"{v}\"]", k + 1), n))
        }
        _ => format!("\"e\":[{}]", list(&|k| format!("[0,{},1,\"s\",\"fr\",null,{k}]", 2 * k + 1), n)),
    };
    format!("{{\"d\":\"board\",\"t\":3,\"ws\":[{ws}],{body}}}")
}

struct Node {
    rust: bool,
    docs: Docs,
    /// Open docs by name, and every doc's name by id.
    open: BTreeMap<String, DocId>,
    names: BTreeMap<DocId, String>,
    routes: BTreeMap<String, Route>,
    events: Vec<(String, String, String)>,
}

struct Net {
    now: u64,
    nodes: BTreeMap<String, Node>,
    by_id: BTreeMap<PeerId, String>,
    /// Frames the Rust members sent, per link, not yet delivered.
    links: BTreeMap<(String, String), VecDeque<String>>,
    held: BTreeSet<String>,
    step: usize,
    /// Each Rust member's last snapshot.
    snapshots: BTreeMap<String, Vec<u8>>,
}

impl Net {
    fn at(&self) -> Instant {
        Instant::from_millis(self.now)
    }

    /// Takes what the Rust members sent and the events they gave.
    fn drain(&mut self) {
        for (name, n) in &mut self.nodes {
            while let Some((p, m)) = n.docs.poll_transmit() {
                assert_eq!(m[0], DOC_TAG);
                let to = self.by_id[&p].clone();
                self.links
                    .entry((name.clone(), to))
                    .or_default()
                    .push_back(String::from_utf8(m[1..].to_vec()).unwrap());
            }
            while let Some(e) = n.docs.poll_event() {
                n.events.push(match e {
                    Event::Change(id, c) => ("change".into(), n.names[&id].clone(), c.json()),
                    Event::Sync(id, s) => ("sync".into(), n.names[&id].clone(), s.json()),
                    Event::Overflow(p) => panic!("{name}: overflow of {p:?}"),
                });
            }
        }
    }

    fn deliver(&mut self, from: &str, to: &str, text: &str) {
        let now = self.at();
        if let Some(n) = self.nodes.get_mut(to).filter(|n| n.rust) {
            let mut m = vec![DOC_TAG];
            m.extend_from_slice(text.as_bytes());
            assert!(n.docs.receive(now, id_of(from), &m));
        }
    }

    /// Delivers rounds as doc.js did, checking that the Rust members sent what doc.js's sent, link by link.
    fn rounds(&mut self, rounds: &[Value]) {
        for (r, round) in rounds.iter().enumerate() {
            self.drain();
            let at = format!("step {} round {r}", self.step);
            // [from, to, frames]: a doc.js-only member's frames whole, a Rust member's as short() of their JSON.
            let expected = round.as_array().unwrap();
            let mut keys: BTreeSet<(String, String)> =
                self.links.iter().filter(|(_, q)| !q.is_empty()).map(|(k, _)| k.clone()).collect();
            for x in expected {
                keys.insert((x[0].as_str().unwrap().into(), x[1].as_str().unwrap().into()));
            }
            let mut deliver = Vec::new();
            for (k, (from, to)) in
                keys.into_iter().filter(|(f, t)| !self.held.contains(&format!("{f}>{t}"))).enumerate()
            {
                let x = expected.get(k).filter(|x| x[0] == from.as_str() && x[1] == to.as_str());
                let texts: Vec<String> = if self.nodes[&from].rust {
                    let sent: Vec<String> =
                        self.links.get_mut(&(from.clone(), to.clone())).unwrap().drain(..).collect();
                    let json = serde_json::to_string(&sent).unwrap();
                    let want = x.map(|x| x[2].as_str().unwrap().to_string());
                    assert!(
                        want.as_ref().is_some_and(|w| same(w, &json)),
                        "{at}: {from}>{to} sent\n  {sent:?}\ndoc.js sent {want:?}"
                    );
                    sent
                } else {
                    let x = x
                        .unwrap_or_else(|| panic!("{at}: {from}>{to}: the Rust members sent on links doc.js's didn't"));
                    x[2].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect()
                };
                deliver.extend(texts.into_iter().map(|t| (from.clone(), to.clone(), t)));
            }
            assert_eq!(
                deliver.iter().map(|f| (&f.0, &f.1)).collect::<BTreeSet<_>>().len(),
                expected.len(),
                "{at}: links {:?}",
                deliver.iter().map(|f| (&f.0, &f.1)).collect::<BTreeSet<_>>()
            );
            for (from, to, text) in deliver {
                self.deliver(&from, &to, &text);
            }
        }
        self.drain();
        let left: Vec<_> = self
            .links
            .iter()
            .filter(|(k, q)| !q.is_empty() && !self.held.contains(&format!("{}>{}", k.0, k.1)))
            .collect();
        assert!(left.is_empty(), "step {}: frames left after doc.js's last round: {left:?}", self.step);
    }

    fn result(&mut self, n: &str, s: &Value) -> Option<Value> {
        let args: Vec<Value> = serde_json::from_str(s["args"].as_str()?).unwrap();
        let a = |i: usize| args[i].as_str().unwrap().to_string();
        let node = self.nodes.get_mut(n).unwrap();
        let id = node.open[s["doc"].as_str().unwrap()];
        let mut d = node.docs.doc(id).unwrap();
        let out = |r: Result<Value, peerwire_sync::Error>| r.unwrap_or_else(|_| Value::from("throw"));
        Some(match s["op"].as_str().unwrap() {
            "set" => out(d.map_set(&a(0), &a(1), &a(2)).map(Value::from)),
            "del" => out(d.map_delete(&a(0), &a(1)).map(Value::from)),
            "add" => out(d.set_add(&a(0), &a(1)).map(|r| r.map_or(Value::Null, Value::from))),
            "rem" => out(d.set_delete(&a(0), &a(1)).map(Value::from)),
            "append" => out(d.log_append(&a(0), &a(1)).map(|r| r.map_or(Value::Null, Value::from))),
            "keep" => out(d.log_keep(&a(0), args[1].as_u64().unwrap_or(0)).map(|_| Value::from(true))),
            "fill" => {
                let (n, len) = (args[1].as_u64().unwrap(), args[2].as_u64().unwrap() as usize);
                let v = |i: u64| Value::from(format!("{}{i}", "v".repeat(len))).to_string();
                Value::from((0..n).filter(|&i| d.map_set(&a(0), &format!("k{i}"), &v(i)).unwrap()).count())
            }
            "unfill" => Value::from(
                (0..args[1].as_u64().unwrap()).filter(|i| d.map_delete(&a(0), &format!("k{i}")).unwrap()).count(),
            ),
            op => panic!("no op {op}"),
        })
    }

    fn check(&mut self, state: &Value) {
        self.drain();
        for (name, n) in &mut self.nodes {
            if !n.rust {
                continue;
            }
            let at = format!("step {} member {name}", self.step);
            let want = &state[name];
            let events: Vec<(String, String, String)> = std::mem::take(&mut n.events);
            let json = serde_json::to_string(&events).unwrap();
            assert!(same(want["events"].as_str().unwrap(), &json), "{at}: events {events:#?}");
            let docs = want["docs"].as_object().unwrap();
            assert_eq!(docs.keys().collect::<BTreeSet<_>>(), n.open.keys().collect::<BTreeSet<_>>(), "{at}: open docs");
            for (doc, w) in docs {
                let d = n.docs.doc(n.open[doc]).unwrap();
                assert_eq!(d.sync().json(), w["sync"].as_str().unwrap(), "{at} {doc}: sync");
                let snapshot = String::from_utf8(d.snapshot()).unwrap();
                assert!(same(w["snapshot"].as_str().unwrap(), &snapshot), "{at} {doc}: snapshot\n  {snapshot}");
                for e in w["entries"].as_array().unwrap() {
                    let (t, c) = (e[0].as_str().unwrap(), e[1].as_str().unwrap());
                    let json = d.entries_json(kind_of(t), c);
                    assert!(same(e[2].as_str().unwrap(), &json), "{at} {doc}: entries of {t}{c}\n  {json}");
                }
            }
        }
    }

    fn run(&mut self, s: &Value) {
        let op = s["op"].as_str().unwrap();
        let node = s["node"].as_str();
        let rust = node.is_some_and(|n| self.nodes[n].rust);
        let now = self.at();
        match op {
            "open" if rust => {
                let name = s["doc"].as_str().unwrap();
                // A Rust member's copy is its own last snapshot, which the snapshot step checked.
                let own = s["copyOf"].as_str().map(|m| self.snapshots[m].clone());
                let copy = own.as_deref().or(s["copy"].as_str().map(str::as_bytes));
                let n = self.nodes.get_mut(node.unwrap()).unwrap();
                let r = n.docs.open(now, name, Options { copy, hub: false });
                assert_eq!(r.is_ok(), s["result"].as_bool().unwrap(), "step {}: open {name}", self.step);
                if let Ok(id) = r {
                    n.open.insert(name.into(), id);
                    n.names.insert(id, name.into());
                }
            }
            "close" if rust => {
                let n = self.nodes.get_mut(node.unwrap()).unwrap();
                let id = n.open.remove(s["doc"].as_str().unwrap()).unwrap();
                n.docs.close(id);
            }
            "link" | "unlink" | "route" => {
                let (a, b) = (s["a"].as_str().unwrap().to_string(), s["b"].as_str().unwrap().to_string());
                if op == "unlink" {
                    self.links.remove(&(a.clone(), b.clone()));
                    self.links.remove(&(b.clone(), a.clone()));
                }
                for (x, y) in [(&a, &b), (&b, &a)] {
                    let n = self.nodes.get_mut(x).unwrap();
                    if op == "route" {
                        n.routes.insert(y.clone(), route_of(s["kind"].as_str().unwrap()));
                    }
                    let route = n.routes.get(y).copied().unwrap_or_default();
                    if n.rust {
                        match op {
                            "link" => n.docs.joined(now, id_of(y), route),
                            "unlink" => n.docs.left(now, id_of(y)),
                            _ => n.docs.rerouted(now, id_of(y), route),
                        }
                    }
                }
            }
            "hold" => {
                self.held.insert(s["link"].as_str().unwrap().into());
            }
            "release" => {
                self.held.remove(s["link"].as_str().unwrap());
            }
            "set" | "del" | "add" | "rem" | "append" | "keep" | "fill" | "unfill" if rust => {
                let got = self.result(node.unwrap(), s).unwrap();
                assert_eq!(got, s["result"], "step {}: {s}", self.step);
            }
            "flood" => {
                let (what, n, offset) =
                    (s["what"].as_str().unwrap(), s["n"].as_u64().unwrap(), s["offset"].as_u64().unwrap());
                let mut m = vec![DOC_TAG];
                m.extend(flood(what, n, offset).into_bytes());
                if let Some(node) = self.nodes.get_mut(s["to"].as_str().unwrap()).filter(|n| n.rust) {
                    node.docs.receive(now, id_of(s["from"].as_str().unwrap()), &m);
                }
            }
            "raw" => {
                let mut m = vec![DOC_TAG];
                m.extend(unhex(s["hex"].as_str().unwrap()));
                if let Some(n) = self.nodes.get_mut(s["to"].as_str().unwrap()).filter(|n| n.rust) {
                    n.docs.receive(now, id_of(s["from"].as_str().unwrap()), &m);
                }
            }
            "snapshot" if rust => {
                let n = &mut self.nodes.get_mut(node.unwrap()).unwrap();
                let id = n.open[s["doc"].as_str().unwrap()];
                let copy = n.docs.doc(id).unwrap().snapshot();
                let text = String::from_utf8(copy.clone()).unwrap();
                assert!(same(s["result"].as_str().unwrap(), &text), "step {}: snapshot {text}", self.step);
                self.snapshots.insert(node.unwrap().to_string(), copy);
            }
            "settle" => self.rounds(s["rounds"].as_array().unwrap()),
            "advance" => {
                let target = self.now + s["ms"].as_u64().unwrap();
                for tick in s["ticks"].as_array().unwrap() {
                    let at = tick["at"].as_u64().unwrap();
                    for (name, n) in &self.nodes {
                        let due = n.docs.poll_timeout().map(Instant::as_micros);
                        assert!(
                            due.is_none_or(|t| t >= at * 1000),
                            "step {}: {name} has a timer at {due:?} µs, doc.js's next is at {at} ms",
                            self.step
                        );
                    }
                    self.now = at;
                    for n in self.nodes.values_mut().filter(|n| n.rust) {
                        n.docs.handle_timeout(Instant::from_millis(at));
                    }
                    self.rounds(tick["rounds"].as_array().unwrap());
                }
                for (name, n) in &self.nodes {
                    let due = n.docs.poll_timeout().map(Instant::as_micros);
                    assert!(
                        due.is_none_or(|t| t > target * 1000),
                        "step {}: {name} has a timer at {due:?} µs, doc.js none by {target} ms",
                        self.step
                    );
                }
                self.now = target;
            }
            "check" => self.check(&s["state"]),
            _ => {}
        }
    }
}

fn read(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(format!("{VECTORS}{name}")).unwrap()).unwrap()
}

#[test]
fn session_replays_doc_js() {
    let v = read("session.json");
    let mut net = Net {
        now: 0,
        nodes: BTreeMap::new(),
        by_id: BTreeMap::new(),
        links: BTreeMap::new(),
        held: BTreeSet::new(),
        step: 0,
        snapshots: BTreeMap::new(),
    };
    for (name, n) in v["nodes"].as_object().unwrap() {
        let mut docs = Docs::new(id_of(name), [0; 32]);
        let incs: Vec<u32> = n["incarnations"].as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as u32).collect();
        docs.fix_incarnations(&incs);
        let rust = n["rust"].as_bool().unwrap();
        net.by_id.insert(id_of(name), name.clone());
        let node = Node {
            rust,
            docs,
            open: BTreeMap::new(),
            names: BTreeMap::new(),
            routes: BTreeMap::new(),
            events: Vec::new(),
        };
        net.nodes.insert(name.clone(), node);
    }
    let steps = v["steps"].as_array().unwrap();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (i, s) in steps.iter().enumerate() {
        net.step = i;
        net.run(s);
        net.drain();
        *counts.entry(s["op"].as_str().unwrap().into()).or_default() += 1;
    }
    assert!(counts["check"] >= 8 && counts["settle"] >= 50, "{counts:?}");
    println!("replayed {} steps of doc.js: {counts:?}", steps.len());
}

/// What a listening member was sent in a Rust session, and what it held.
#[test]
fn rust_session_applies_in_doc_js() {
    let names = ["a", "b", "c"];
    let x = id_of("e");
    let incs: Vec<u32> = (1..=64).map(|i| 0x1000_0000 + i).collect();
    let mut rng = ChaCha8Rng::seed_from_u64(0x6e62_006b);
    let mut members: Vec<(PeerId, Docs, DocId)> = Vec::new();
    let now = Instant::from_millis(0);
    for (k, n) in names.iter().chain(["e"].iter()).enumerate() {
        let me = id_of(n);
        let mut docs = Docs::new(me, [k as u8; 32]);
        if me == x {
            docs.fix_incarnations(&incs);
        }
        let id = docs.open(now, "board", Options::default()).unwrap();
        members.push((me, docs, id));
    }
    for i in 0..members.len() {
        for j in 0..members.len() {
            if i != j {
                let p = members[j].0;
                members[i].1.joined(now, p, Route::Direct);
            }
        }
    }
    let values = [r#""\ud800 lone""#, "-0", "1E21", r#"{"b":1,"a":[1,{"2":0,"1":0}]}"#, "1e400", r#"" ""#, "[]", "0.1"];
    let keys = ["k1", "k2", "ключ", "😀", ""];
    let mut heard: Vec<(String, String)> = Vec::new();
    let mut items: Vec<(usize, String)> = Vec::new();
    for round in 0..300 {
        let w = (rng.next_u32() % 3) as usize;
        let (_, docs, id) = &mut members[w];
        let mut d = docs.doc(*id).unwrap();
        let v = values[rng.next_u32() as usize % values.len()];
        match rng.next_u32() % 6 {
            0 | 1 => {
                d.map_set("meta", keys[rng.next_u32() as usize % keys.len()], v).unwrap();
            }
            2 => {
                d.map_delete("meta", keys[rng.next_u32() as usize % keys.len()]).unwrap();
            }
            3 => {
                if let Some(id) = d.set_add("strokes", v).unwrap() {
                    items.push((w, id));
                }
            }
            4 => {
                if let Some(k) = items.iter().position(|(o, _)| *o == w) {
                    let (_, id) = items.remove(k);
                    assert!(d.set_delete("strokes", &id).unwrap());
                }
            }
            _ => {
                d.log_append("chat", &format!("{round}")).unwrap();
            }
        }
        // Delivered link by link, in order, until none is left.
        loop {
            let mut sent = Vec::new();
            for (from, docs, _) in members.iter_mut() {
                while let Some((to, m)) = docs.poll_transmit() {
                    sent.push((*from, to, m));
                }
                while docs.poll_event().is_some() {}
            }
            if sent.is_empty() {
                break;
            }
            for (from, to, m) in sent {
                if to == x {
                    heard.push((hex(&from.0), String::from_utf8(m[1..].to_vec()).unwrap()));
                }
                let k = members.iter().position(|(p, ..)| *p == to).unwrap();
                assert!(members[k].1.receive(now, from, &m));
            }
        }
    }
    let (_, docs, id) = &mut members[3];
    let d = docs.doc(*id).unwrap();
    let entries: Vec<Value> = [("m", "meta"), ("s", "strokes"), ("l", "chat")]
        .iter()
        .map(|(t, c)| Value::from(vec![Value::from(*t), Value::from(*c), Value::from(d.entries_json(kind_of(t), c))]))
        .collect();
    let v = serde_json::json!({
        "observer": { "id": hex(&x.0), "incarnations": incs },
        "frames": heard.iter().map(|(p, t)| vec![p.clone(), t.clone()]).collect::<Vec<_>>(),
        "snapshot": String::from_utf8(d.snapshot()).unwrap(),
        "entries": entries,
    });
    let text = format!("{v}\n");
    let path = format!("{VECTORS}rust.json");
    if std::env::var("PW_WRITE_VECTORS").as_deref() == Ok("1") {
        std::fs::write(&path, &text).unwrap();
    } else {
        assert!(
            std::fs::read_to_string(&path).is_ok_and(|t| t == text),
            "{path} isn't what this session makes now: PW_WRITE_VECTORS=1 writes it"
        );
    }
    assert!(heard.len() > 250, "{} frames", heard.len());
}
