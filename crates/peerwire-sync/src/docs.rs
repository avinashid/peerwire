//! [`Docs`]: the docs a member has open, and what room.js does for them: routing each frame to its doc by name,
//! answering an ask for a doc we haven't open, keeping each member's messages in order, and the members.

use crate::doc::{Doc, Host};
use crate::frame::{valid_name, Kind, ASK, MAX_SAFE};
use crate::json::{lossy, write_str, Tree, Utf16};
use crate::{DocId, Error, Event, Item, Options, Route, Status, DOCZ_TAG, DOC_TAG, OUTBOX_BYTES};
use peerwire_core::{Instant, PeerId};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;

#[derive(Default)]
struct Outbox {
    messages: VecDeque<Vec<u8>>,
    bytes: usize,
}

/// The docs a member has open in one room. See the crate docs for the host loop.
pub struct Docs {
    me: PeerId,
    rng: ChaCha20Rng,
    fixed: VecDeque<u32>,
    now_us: u64,
    seq: u64,
    docs: BTreeMap<u32, Doc>,
    names: BTreeMap<Utf16, u32>,
    next_id: u32,
    /// Joined members, in the order they joined (room.js `peers`).
    members: Vec<PeerId>,
    routes: BTreeMap<PeerId, Route>,
    out: BTreeMap<PeerId, Outbox>,
    /// Members whose link refused a message since poll_transmit last ran dry.
    held: BTreeSet<PeerId>,
    events: VecDeque<Event>,
}

/// An ask for a doc we don't have open is answered with nothing, so that the asker counts us as answered rather
/// than waiting for us (doc.js `unopened`).
fn unopened(t: &Tree, m: u32) -> Option<String> {
    let kind = t.field(m, "t").and_then(|x| t.num(x));
    let name = t.field(m, "d").and_then(|d| t.str(d)).filter(|d| valid_name(d))?;
    let q =
        t.field(m, "q").and_then(|q| t.num(q)).filter(|q| q.fract() == 0.0 && *q >= 1.0 && *q <= MAX_SAFE as f64)?;
    if kind != Some(ASK as f64) {
        return None;
    }
    let mut s = String::from("{\"d\":");
    write_str(name, &mut s);
    let _ = write!(s, ",\"t\":3,\"ws\":[],\"q\":{},\"i\":0,\"n\":0,\"end\":1}}", q as u64);
    Some(s)
}

impl Docs {
    /// `me` is this member's PeerId; `seed` draws each doc's incarnations (take it from the OS's randomness: two
    /// openings that drew the same one would write on each other's dots).
    pub fn new(me: PeerId, seed: [u8; 32]) -> Docs {
        Docs {
            me,
            rng: ChaCha20Rng::from_seed(seed),
            fixed: VecDeque::new(),
            now_us: 0,
            seq: 0,
            docs: BTreeMap::new(),
            names: BTreeMap::new(),
            next_id: 0,
            members: Vec::new(),
            routes: BTreeMap::new(),
            out: BTreeMap::new(),
            held: BTreeSet::new(),
            events: VecDeque::new(),
        }
    }

    /// The incarnations the next openings draw, before the seed's: for tests that replay doc.js.
    #[doc(hidden)]
    pub fn fix_incarnations(&mut self, list: &[u32]) {
        self.fixed.extend(list);
    }

    fn set_now(&mut self, now: Instant) {
        self.now_us = self.now_us.max(now.as_micros());
    }

    /// Runs `f` with the host lent to the docs, then queues what they sent.
    fn with_host<R>(&mut self, id: u32, f: impl FnOnce(&mut Host, &mut BTreeMap<u32, Doc>) -> R) -> R {
        let mut sent = Vec::new();
        let Docs { me, rng, fixed, now_us, seq, docs, members, routes, events, .. } = self;
        let mut incarnation = || fixed.pop_front().unwrap_or_else(|| rng.next_u32());
        let mut h = Host {
            me: *me,
            peers: members,
            routes,
            now_us: *now_us,
            seq,
            sent: &mut sent,
            events,
            incarnation: &mut incarnation,
            id: DocId(id),
        };
        let r = f(&mut h, docs);
        self.queue(sent);
        r
    }

    fn run<R>(&mut self, id: u32, f: impl FnOnce(&mut Doc, &mut Host) -> R) -> Option<R> {
        self.with_host(id, |h, docs| docs.get_mut(&id).map(|d| f(d, h)))
    }

    fn each(&mut self, f: impl Fn(&mut Doc, &mut Host)) {
        let ids: Vec<u32> = self.docs.keys().copied().collect();
        for id in ids {
            self.run(id, &f);
        }
    }

    /// Queues frames for their members, as doc messages. A member whose queue passes OUTBOX_BYTES is forgotten.
    fn queue(&mut self, sent: Vec<(PeerId, String)>) {
        for (p, text) in sent {
            if !self.members.contains(&p) {
                continue;
            }
            let mut m = Vec::with_capacity(1 + text.len());
            m.push(DOC_TAG);
            m.extend_from_slice(text.as_bytes());
            let ob = self.out.entry(p).or_default();
            ob.bytes += m.len();
            ob.messages.push_back(m);
            if ob.bytes > OUTBOX_BYTES {
                self.forget(p);
                self.events.push_back(Event::Overflow(p));
            }
        }
    }

    /// Opens doc `name` (the same name on every member is the same doc), or gives the one open already, and asks
    /// every member for it. A copy that isn't one this version reads is refused, and the doc isn't opened.
    pub fn open(&mut self, now: Instant, name: &str, options: Options<'_>) -> Result<DocId, Error> {
        let name: Utf16 = name.encode_utf16().collect();
        if !valid_name(&name) {
            return Err(Error::Name);
        }
        if let Some(&id) = self.names.get(&name) {
            return Ok(DocId(id));
        }
        self.set_now(now);
        let id = self.next_id;
        let opened = self.with_host(id, |h, docs| {
            let mut d = Doc::new(h, name.clone(), options.hub);
            if let Some(copy) = options.copy {
                // A copy is checked whole before any of it joins, so one refused leaves no event behind.
                if !d.restore(h, copy) {
                    return false;
                }
            }
            d.open(h);
            docs.insert(id, d);
            true
        });
        if !opened {
            return Err(Error::Copy);
        }
        self.next_id += 1;
        self.names.insert(name, id);
        Ok(DocId(id))
    }

    /// Stops syncing doc `doc` and forgets it: opening it again starts afresh and catches up again.
    pub fn close(&mut self, doc: DocId) {
        if let Some(d) = self.docs.remove(&doc.0) {
            self.names.remove(&d.name);
        }
    }

    /// Doc `doc`, to read and write; `None` once closed.
    pub fn doc(&mut self, doc: DocId) -> Option<DocMut<'_>> {
        self.docs.contains_key(&doc.0).then_some(DocMut { docs: self, id: doc.0 })
    }

    /// Member `peer` joined the room (room.js: its presence came), reached by `route`: each doc asks it. Called
    /// for a member that has joined already, it says the route changed.
    pub fn joined(&mut self, now: Instant, peer: PeerId, route: Route) {
        self.set_now(now);
        self.routes.insert(peer, route);
        if self.members.contains(&peer) {
            return self.each(|d, h| d.rerouted(h));
        }
        self.members.push(peer);
        self.each(|d, h| d.joined(h, peer));
    }

    /// The route to member `peer` changed: one better routed than the member answering may take its turn.
    pub fn rerouted(&mut self, now: Instant, peer: PeerId, route: Route) {
        if self.members.contains(&peer) {
            self.joined(now, peer, route);
        }
    }

    /// Member `peer` left: what was queued for it goes.
    pub fn left(&mut self, now: Instant, peer: PeerId) {
        self.set_now(now);
        if self.members.contains(&peer) {
            self.forget(peer);
        }
    }

    fn forget(&mut self, peer: PeerId) {
        self.members.retain(|p| *p != peer);
        self.routes.remove(&peer);
        self.out.remove(&peer);
        self.held.remove(&peer);
        self.each(|d, h| d.left(h, peer));
    }

    /// A reliable message from member `peer`. False if it isn't a doc message (its first byte isn't
    /// [`DOC_TAG`] or [`DOCZ_TAG`]): it is the app's. A doc message from a peer that hasn't joined, or one that is
    /// malformed, is dropped.
    pub fn receive(&mut self, now: Instant, peer: PeerId, message: &[u8]) -> bool {
        match message.first() {
            Some(&DOC_TAG) => {}
            Some(&DOCZ_TAG) => return true,
            _ => return false,
        }
        self.set_now(now);
        if !self.members.contains(&peer) {
            return true;
        }
        let Some(t) = Tree::parse(&message[1..]) else { return true };
        let root = t.root();
        let open = t.field(root, "d").and_then(|d| t.str(d)).and_then(|d| self.names.get(d)).copied();
        match open {
            Some(id) => {
                self.run(id, |d, h| d.receive(h, peer, &t, root));
            }
            None => {
                if let Some(reply) = unopened(&t, root) {
                    self.queue(vec![(peer, reply)]);
                }
            }
        }
        true
    }

    /// The next message to send: to `peer`, on the reliable lane, as it is. One its link refuses (would block)
    /// goes back with [`Docs::retry`], and that member's messages wait until this has returned `None`.
    pub fn poll_transmit(&mut self) -> Option<(PeerId, Vec<u8>)> {
        let held = &self.held;
        let Some(p) = self.out.iter().find(|(p, ob)| !ob.messages.is_empty() && !held.contains(p)).map(|(p, _)| *p)
        else {
            self.held.clear();
            return None;
        };
        let ob = self.out.get_mut(&p)?;
        let m = ob.messages.pop_front()?;
        ob.bytes -= m.len();
        if ob.messages.is_empty() {
            self.out.remove(&p);
        }
        Some((p, m))
    }

    /// A message from [`Docs::poll_transmit`] that `peer`'s link refused: it goes first, when the next call to
    /// poll_transmit after one that returned `None` tries that member again.
    pub fn retry(&mut self, peer: PeerId, message: Vec<u8>) {
        if !self.members.contains(&peer) {
            return;
        }
        self.held.insert(peer);
        let ob = self.out.entry(peer).or_default();
        ob.bytes += message.len();
        ob.messages.push_front(message);
        if ob.bytes > OUTBOX_BYTES {
            self.forget(peer);
            self.events.push_back(Event::Overflow(peer));
        }
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// When [`Docs::handle_timeout`] is next due.
    pub fn poll_timeout(&self) -> Option<Instant> {
        self.docs.values().filter_map(|d| d.next_timer()).map(|(t, _)| t).min().map(|t| Instant::from_micros(t.at))
    }

    /// Runs the timers due by `now`, in the order due.
    pub fn handle_timeout(&mut self, now: Instant) {
        self.set_now(now);
        loop {
            let next =
                self.docs.iter().filter_map(|(&id, d)| d.next_timer().map(|(t, f)| (t, id, f))).min_by_key(|x| x.0);
            match next {
                Some((t, id, f)) if t.at <= self.now_us => {
                    self.run(id, |d, h| d.fire(h, f));
                }
                _ => return,
            }
        }
    }
}

/// An open doc, to read and write. Writes go to every member at once; a write members' data stops (the doc is
/// full, or members claimed every dot it could take) is refused, as doc.js refuses it, rather than an error.
pub struct DocMut<'a> {
    docs: &'a mut Docs,
    id: u32,
}

impl DocMut<'_> {
    fn doc(&self) -> &Doc {
        &self.docs.docs[&self.id]
    }

    fn write<R>(&mut self, f: impl FnOnce(&mut Doc, &mut Host) -> R) -> R {
        self.docs.run(self.id, f).expect("open")
    }

    pub fn id(&self) -> DocId {
        DocId(self.id)
    }

    pub fn name(&self) -> String {
        lossy(&self.doc().name)
    }

    /// Sets map `coll`'s `key` to `value`, JSON text: true, or false if the doc refused it.
    pub fn map_set(&mut self, coll: &str, key: &str, value: &str) -> Result<bool, Error> {
        self.write(|d, h| d.map_set(h, coll, key, value))
    }

    /// Deletes map `coll`'s `key`: whether it held a value.
    pub fn map_delete(&mut self, coll: &str, key: &str) -> Result<bool, Error> {
        self.write(|d, h| d.map_delete(h, coll, key))
    }

    /// Adds `value` to set `coll`: its id, or `None` if the doc refused it.
    pub fn set_add(&mut self, coll: &str, value: &str) -> Result<Option<String>, Error> {
        self.write(|d, h| d.add(h, Kind::Set, coll, value))
    }

    /// Deletes item `id` of set `coll`: whether it held it.
    pub fn set_delete(&mut self, coll: &str, id: &str) -> Result<bool, Error> {
        self.write(|d, h| d.set_delete(h, coll, id))
    }

    /// Appends `value` to log `coll`: its id, or `None` if the doc refused it.
    pub fn log_append(&mut self, coll: &str, value: &str) -> Result<Option<String>, Error> {
        self.write(|d, h| d.add(h, Kind::Log, coll, value))
    }

    /// How many entries log `coll` keeps, the newest ([`crate::DEFAULT_KEEP`] until said): every member should
    /// say the same, right after opening the doc.
    pub fn log_keep(&mut self, coll: &str, keep: u64) -> Result<(), Error> {
        self.write(|d, h| d.log_keep(h, coll, keep))
    }

    /// What map `coll`'s `key`, or set or log `coll`'s item `id`, shows, as JSON text.
    pub fn get(&self, kind: Kind, coll: &str, key: &str) -> Option<&str> {
        self.doc().get(kind, coll, key)
    }

    /// A collection's entries: a map's keys in order, or a set's items or a log's entries, oldest first.
    pub fn entries(&self, kind: Kind, coll: &str) -> Vec<Item> {
        self.doc().items(kind, coll)
    }

    /// [`DocMut::entries`] as doc.js's `entries()` gives them, as JSON text: `[{"key":…,"value":…,"from":…}]`, or
    /// `"id"` for items.
    pub fn entries_json(&self, kind: Kind, coll: &str) -> String {
        self.doc().items_json(kind, coll)
    }

    /// Where catching up stands.
    pub fn sync(&self) -> Status {
        self.doc().sync()
    }

    /// The doc as a copy to keep on this device, for [`crate::Options::copy`] when it opens again: its entries,
    /// the dots it knows are dead and its clock, as JSON (doc.js `snapshot()`). It holds what members wrote, so
    /// keep it as private as the room.
    pub fn snapshot(&self) -> Vec<u8> {
        self.doc().snapshot().into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unopened_answers_an_ask_with_nothing() {
        let ask = |text: &str| {
            let t = Tree::parse(text.as_bytes()).unwrap();
            unopened(&t, t.root())
        };
        assert_eq!(
            ask(r#"{"d":"b\ud800","t":2,"q":5}"#).unwrap(),
            r#"{"d":"b\ud800","t":3,"ws":[],"q":5,"i":0,"n":0,"end":1}"#
        );
        assert_eq!(
            ask(r#"{"d":"b","t":2,"q":5e0,"ws":"junk"}"#).unwrap(),
            r#"{"d":"b","t":3,"ws":[],"q":5,"i":0,"n":0,"end":1}"#
        );
        for bad in [
            r#"{"d":"b","t":1,"q":5}"#,
            r#"{"d":"","t":2,"q":5}"#,
            r#"{"d":"b","t":2,"q":0}"#,
            r#"{"d":"b","t":2,"q":1.5}"#,
            "[2]",
            "null",
        ] {
            assert_eq!(ask(bad), None, "{bad}");
        }
        assert_eq!(ask(&format!(r#"{{"d":"{}","t":2,"q":1}}"#, "x".repeat(129))), None);
    }
}
