//! [`Docs`]: the docs a member has open, and what room.js does for them: routing each frame to its doc by name,
//! answering an ask for a doc we haven't open, keeping each member's messages in order, and the members.

use crate::doc::{Doc, Host};
use crate::frame::{valid_name, Kind, ASK, MAX_SAFE};
use crate::json::{lossy, write_str, Tree, Utf16};
use crate::{
    DocId, Error, Event, Item, Options, Route, Status, DOCZ_TAG, DOC_TAG, OUTBOX_BYTES, RELAY_BYTES, RELAY_BYTES_PER_S,
};
use peerwire_core::{Instant, PeerId};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;

/// Times a member's messages are dropped (it is resynced) with its link taking none of them before it is forgotten.
const RESYNCS: u32 = 2;
/// How long, in µs, a link must also have taken nothing for: a burst of frames handled before the host sends
/// again can drop a slow member's messages more than once, but not for this long. A silent link times out in it.
const STALL_US: u64 = 30_000_000;
/// Docs we haven't open whose replies to a member's asks a resync sends again, the latest of each: a member asks
/// only for the docs it has open.
const UNOPENED: usize = 16;

/// A member's messages waiting for its link, and when its link last took one.
struct Outbox {
    messages: VecDeque<Vec<u8>>,
    bytes: usize,
    /// Our latest reply to its ask for each doc we hadn't open, for the last UNOPENED of them: no doc would end
    /// that ask again if a resync dropped it, and the member would wait on it for good.
    unopened: VecDeque<(Utf16, String)>,
    /// When its link last took a message (or it joined), in µs. poll_transmit isn't told the time, so a message
    /// taken is `taken` until the next call that is, which gives `took` its time: a clock not moved on since can
    /// only make the link seem to have taken it later.
    took: u64,
    taken: bool,
    /// The times its messages were dropped since its link last took one.
    resyncs: u32,
    /// `taken` and `resyncs` as they were before the message last taken, for when it comes back with retry.
    undo: (bool, u32),
}

impl Outbox {
    fn new(now_us: u64) -> Outbox {
        Outbox {
            messages: VecDeque::new(),
            bytes: 0,
            unopened: VecDeque::new(),
            took: now_us,
            taken: false,
            resyncs: 0,
            undo: (false, 0),
        }
    }
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
    /// Each member's, from when it joins until it leaves.
    out: BTreeMap<PeerId, Outbox>,
    /// Members whose link refused a message since poll_transmit last ran dry.
    held: BTreeSet<PeerId>,
    /// When each member's relay budget is whole again, in µs. Kept when it leaves (so coming back doesn't refill
    /// it) until it is whole.
    relays: BTreeMap<PeerId, u64>,
    events: VecDeque<Event>,
}

/// The µs a relay budget takes to refill `bytes`.
fn relay_us(bytes: usize) -> u64 {
    u64::try_from((bytes as u128 * 1_000_000).div_ceil(RELAY_BYTES_PER_S as u128)).unwrap_or(u64::MAX)
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
            relays: BTreeMap::new(),
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
        for ob in self.out.values_mut().filter(|ob| ob.taken) {
            (ob.took, ob.taken) = (self.now_us, false);
        }
    }

    /// Runs `f` with the host lent to the docs, then queues what they sent.
    fn with_host<R>(&mut self, id: u32, f: impl FnOnce(&mut Host, &mut BTreeMap<u32, Doc>) -> R) -> R {
        let (r, sent, _) = self.lend(id, f);
        self.queue(sent);
        r
    }

    /// Runs `f` with the host lent to the docs: what it gave, what the docs sent, and what a hub relayed of it.
    fn lend<R>(
        &mut self,
        id: u32,
        f: impl FnOnce(&mut Host, &mut BTreeMap<u32, Doc>) -> R,
    ) -> (R, Vec<(PeerId, String)>, usize) {
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
            relayed: 0,
        };
        let r = f(&mut h, docs);
        let relayed = h.relayed;
        (r, sent, relayed)
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

    /// Queues frames for their members, as doc messages. A member whose queue passes OUTBOX_BYTES is resynced.
    fn queue(&mut self, sent: Vec<(PeerId, String)>) {
        for (p, text) in sent {
            if self.push(p, text) {
                self.cap(p);
            }
        }
    }

    /// Puts a frame in member `p`'s queue, as a doc message: false if `p` isn't a member.
    fn push(&mut self, p: PeerId, text: String) -> bool {
        let Some(ob) = self.out.get_mut(&p) else { return false };
        let mut m = Vec::with_capacity(1 + text.len());
        m.push(DOC_TAG);
        m.extend_from_slice(text.as_bytes());
        ob.bytes += m.len();
        ob.messages.push_back(m);
        true
    }

    /// A member whose messages passed OUTBOX_BYTES waiting has them dropped, and each doc catches it up afresh
    /// (resync): what piles up for a slow member, as a hub's relays do, costs it a catch-up, not its link. Our
    /// replies to its asks for docs we hadn't open go again first. What that queues isn't capped, so it can't drop
    /// it again. One whose link has taken nothing across RESYNCS of those, and for STALL_US, is forgotten as if it
    /// left.
    fn cap(&mut self, p: PeerId) {
        let now = self.now_us;
        let Some(ob) = self.out.get_mut(&p).filter(|ob| ob.bytes > OUTBOX_BYTES) else { return };
        if ob.resyncs >= RESYNCS && !ob.taken && now.saturating_sub(ob.took) >= STALL_US {
            self.forget(p);
            self.events.push_back(Event::Overflow(p));
            return;
        }
        ob.resyncs += 1;
        ob.messages.clear();
        ob.bytes = 0;
        let again: Vec<String> = ob.unopened.iter().map(|(_, reply)| reply.clone()).collect();
        for reply in again {
            self.push(p, reply);
        }
        let ids: Vec<u32> = self.docs.keys().copied().collect();
        for id in ids {
            let (_, sent, _) = self.lend(id, |h, docs| docs.get_mut(&id).map(|d| d.resync(h, p)));
            for (q, text) in sent {
                self.push(q, text);
            }
        }
    }

    /// Charges member `p` the `bytes` a hub relayed of its frame to the others. One that has had more than
    /// RELAY_BYTES relayed beyond what refilled is forgotten, as if it left: the member writing pays, not those it
    /// would fill the queues of.
    fn charge(&mut self, p: PeerId, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let now = self.now_us;
        let whole = self.relays.entry(p).or_insert(0);
        *whole = (*whole).max(now).saturating_add(relay_us(bytes));
        if *whole - now > relay_us(RELAY_BYTES) {
            self.forget(p);
            self.events.push_back(Event::Overflow(p));
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
        self.out.insert(peer, Outbox::new(self.now_us));
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
        let now = self.now_us;
        self.relays.retain(|_, whole| *whole > now);
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
        let name = t.field(root, "d").and_then(|d| t.str(d));
        let open = name.and_then(|d| self.names.get(d)).copied();
        match open {
            Some(id) => {
                let ((), sent, relayed) = self.lend(id, |h, docs| {
                    if let Some(d) = docs.get_mut(&id) {
                        d.receive(h, peer, &t, root);
                    }
                });
                // Charged before anything is queued: one cut has nothing more queued for it, and what was relayed
                // goes on to the others anyway.
                self.charge(peer, relayed);
                self.queue(sent);
            }
            None => {
                if let Some(reply) = unopened(&t, root) {
                    if let (Some(ob), Some(name)) = (self.out.get_mut(&peer), name) {
                        ob.unopened.retain(|(d, _)| d[..] != *name);
                        if ob.unopened.len() == UNOPENED {
                            ob.unopened.pop_front();
                        }
                        ob.unopened.push_back((name.to_vec(), reply.clone()));
                    }
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
        ob.undo = (ob.taken, ob.resyncs);
        (ob.taken, ob.resyncs) = (true, 0);
        Some((p, m))
    }

    /// A message from [`Docs::poll_transmit`] that `peer`'s link refused: it goes first, when the next call to
    /// poll_transmit after one that returned `None` tries that member again. It doesn't count as taken.
    pub fn retry(&mut self, peer: PeerId, message: Vec<u8>) {
        let Some(ob) = self.out.get_mut(&peer) else { return };
        (ob.taken, ob.resyncs) = ob.undo;
        ob.bytes += message.len();
        ob.messages.push_front(message);
        self.held.insert(peer);
        self.cap(peer);
    }

    /// Bytes of messages waiting for member `peer`, retried ones included: at most [`OUTBOX_BYTES`] and a message,
    /// besides what catching a member up afresh queues past it: for that member, and for each other one an ask per
    /// doc (see [`OUTBOX_BYTES`]).
    pub fn queued(&self, peer: &PeerId) -> usize {
        self.out.get(peer).map_or(0, |ob| ob.bytes)
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

    fn pair() -> (Docs, Docs, DocId, PeerId, PeerId) {
        let (a, b) = (PeerId([0xaa; 32]), PeerId([0xbb; 32]));
        let now = Instant::from_millis(0);
        let mut da = Docs::new(a, [1; 32]);
        let mut db = Docs::new(b, [2; 32]);
        let id = da.open(now, "board", Options::default()).unwrap();
        db.open(now, "board", Options::default()).unwrap();
        (da, db, id, a, b)
    }

    /// Doc messages from a peer that hasn't joined are taken (they are doc messages) and dropped: no reply, no
    /// change. App messages are left to the app, a deflated frame is dropped, and so is one that isn't JSON.
    #[test]
    fn messages_are_taken_from_members_only() {
        let (mut da, mut db, id, a, b) = pair();
        let now = Instant::from_millis(0);
        db.joined(now, a, Route::Direct);
        db.doc(DocId(0)).unwrap().map_set("meta", "k", "1").unwrap();
        // Its ask, then its delta.
        let delta: Vec<Vec<u8>> = std::iter::from_fn(|| db.poll_transmit()).map(|(_, m)| m).collect();
        let ask = br#"{"d":"other","t":2,"ws":[],"r":[],"q":9}"#;
        for m in delta.iter().chain([[&[DOC_TAG][..], ask].concat()].iter()) {
            assert!(da.receive(now, b, m));
        }
        assert!(da.poll_transmit().is_none() && da.poll_event().is_none());
        assert_eq!(da.doc(id).unwrap().get(Kind::Map, "meta", "k"), None);
        da.joined(now, b, Route::Direct);
        while da.poll_transmit().is_some() {}
        for m in &delta {
            assert!(da.receive(now, b, m));
        }
        assert_eq!(da.doc(id).unwrap().get(Kind::Map, "meta", "k"), Some("1"));
        // Its ask came with the delta: answered now that it is a member.
        assert!(std::iter::from_fn(|| da.poll_transmit()).count() > 0);
        assert!(!da.receive(now, b, b"\x01app") && !da.receive(now, b, b""));
        for m in [&b"\x08junk"[..], b"\x07{", b"\x07[]", b"\x07{\"d\":5}"] {
            assert!(da.receive(now, b, m));
        }
        // A deflated frame is dropped unread, even one that reads as JSON: never inflated, never taken as plain.
        assert!(da.receive(now, b, &[&[DOCZ_TAG][..], ask].concat()));
        assert!(da.poll_transmit().is_none());
        assert!(da.receive(now, b, &[&[DOC_TAG][..], ask].concat()));
        assert!(da.poll_transmit().is_some(), "the same frame, plain, is answered (unopened)");
    }

    /// Hands `from`'s messages for `to` to it: whether there were any. Those for other members are dropped.
    fn deliver(from: &mut Docs, to: &mut Docs, now: Instant) -> bool {
        let mut moved = false;
        while let Some((p, m)) = from.poll_transmit() {
            if p == to.me {
                assert!(to.receive(now, from.me, &m));
                moved = true;
            }
        }
        moved
    }

    /// Hands each member's messages to the other until neither sends any, then runs their timers, until neither
    /// has one left: what a link that takes everything does.
    fn settle(x: &mut Docs, y: &mut Docs, mut now: Instant) -> Instant {
        for _ in 0..10_000 {
            while deliver(x, y, now) | deliver(y, x, now) {}
            let Some(t) = [x.poll_timeout(), y.poll_timeout()].into_iter().flatten().min() else { return now };
            now = now.max(t);
            x.handle_timeout(now);
            y.handle_timeout(now);
        }
        panic!("never settled");
    }

    /// What member `d` holds of doc `id`, as doc.js lists it.
    fn held(d: &mut Docs, id: DocId) -> String {
        let d = d.doc(id).unwrap();
        [(Kind::Map, "big"), (Kind::Set, "s")].map(|(k, c)| d.entries_json(k, c)).join("\n")
    }

    /// `peer`'s queue, as text.
    fn waiting(d: &Docs, peer: &PeerId) -> Vec<String> {
        d.out[peer].messages.iter().map(|m| String::from_utf8(m[1..].to_vec()).unwrap()).collect()
    }

    /// A member whose messages pass OUTBOX_BYTES waiting has them dropped and is caught up afresh, not forgotten:
    /// our answer to its latest ask ends, empty; the dots we know are dead go to it; and we ask it, so it asks back
    /// for what it missed.
    #[test]
    fn a_member_past_the_outbox_cap_is_resynced_not_forgotten() {
        let (mut da, mut db, id, a, b) = pair();
        let now = Instant::from_millis(0);
        da.joined(now, b, Route::Direct);
        db.joined(now, a, Route::Direct);
        let (_, ask) = db.poll_transmit().unwrap();
        let text = String::from_utf8(ask.clone()).unwrap();
        let q = text.split("\"q\":").nth(1).unwrap().trim_end_matches('}').to_owned();
        assert!(da.receive(now, b, &ask));
        settle(&mut da, &mut db, now);
        // b's link takes nothing while a writes.
        let value = format!("\"{}\"", "x".repeat(60_000));
        let mut written = 0;
        loop {
            let before = da.queued(&b);
            da.doc(id).unwrap().map_set("big", &format!("k{}", written % 10), &value).unwrap();
            written += 1;
            if da.queued(&b) < before {
                break;
            }
            assert!(da.queued(&b) <= OUTBOX_BYTES, "{written} written");
        }
        assert!(written > 250 && overflows(&mut da).is_empty(), "{written} written");
        let sent = waiting(&da, &b);
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert_eq!(sent[0], format!(r#"{{"d":"board","t":3,"ws":[],"q":{q},"i":0,"n":0,"end":1}}"#));
        assert!(sent[1].starts_with(r#"{"d":"board","t":3,"ws":[""#) && sent[1].ends_with("]]}"), "{}", sent[1]);
        assert!(sent[1].contains(r#""x":[[0,1,"#) && !sent[1].contains("\"q\""), "{}", sent[1]);
        assert!(sent[2].starts_with(r#"{"d":"board","t":2,"ws":[""#), "{}", sent[2]);
        // It asks back, and catches up.
        settle(&mut da, &mut db, now);
        assert_eq!(held(&mut db, id), held(&mut da, id));
        assert!(db.doc(id).unwrap().get(Kind::Map, "big", "k9").is_some());
        assert!(overflows(&mut da).is_empty() && overflows(&mut db).is_empty());
    }

    /// A pair with a doc of 300 set items of 10 KB on `a`, and `b`, which has joined but heard nothing yet.
    fn loaded() -> (Docs, Docs, DocId) {
        let (mut da, mut db, id, a, b) = pair();
        let now = Instant::from_millis(0);
        for i in 0..300 {
            da.doc(id).unwrap().set_add("s", &format!("\"{i}{}\"", "x".repeat(10_000))).unwrap();
        }
        da.joined(now, b, Route::Direct);
        db.joined(now, a, Route::Direct);
        (da, db, id)
    }

    /// Drops what `d` has waiting for `peer`, as a queue past OUTBOX_BYTES has it dropped.
    fn resync(d: &mut Docs, peer: PeerId) {
        assert!(d.push(peer, "x".repeat(OUTBOX_BYTES)));
        d.cap(peer);
    }

    /// One whose messages are dropped as it catches up from us waits on our answer no more: it ends, so the member
    /// asks again. Dropped twice before its link takes any, it still is: we keep the number of its ask.
    #[test]
    fn a_member_dropped_as_it_catches_up_asks_again() {
        for drops in [1, 2] {
            let (mut da, mut db, id) = loaded();
            let (a, b, now) = (da.me, db.me, Instant::from_millis(0));
            // b's ask, then our ask and the first pieces of our answer: it acknowledges them.
            while let Some((_, m)) = db.poll_transmit() {
                assert!(da.receive(now, b, &m));
            }
            for _ in 0..3 {
                let (_, m) = da.poll_transmit().unwrap();
                assert!(db.receive(now, a, &m));
            }
            while let Some((_, m)) = db.poll_transmit() {
                assert!(da.receive(now, b, &m));
            }
            assert!(db.doc(id).unwrap().sync().json().contains("loading"));
            for _ in 0..drops {
                resync(&mut da, b);
            }
            // No dot is dead: our answer's end and our ask alone.
            let sent = waiting(&da, &b);
            assert!(sent.len() == 2 && sent[0].ends_with(r#""i":0,"n":0,"end":1}"#), "{sent:?}");
            assert!(sent[1].starts_with(r#"{"d":"board","t":2,"#), "{sent:?}");
            let now = settle(&mut da, &mut db, now);
            assert_eq!(held(&mut db, id), held(&mut da, id), "{drops} drop(s)");
            assert_eq!(db.doc(id).unwrap().entries(Kind::Set, "s").len(), 300);
            assert!(now < Instant::from_millis(60_000), "{now:?}");
        }
    }

    /// One whose messages are dropped with our deletes in them still holds what we deleted: nothing in our ask shows
    /// it that, as it has seen those dots. The dead dots sent with the resync do.
    #[test]
    fn a_member_dropped_with_deletes_in_its_queue_hears_of_them() {
        let (mut da, mut db, id) = loaded();
        let b = db.me;
        let now = settle(&mut da, &mut db, Instant::from_millis(0));
        assert_eq!(held(&mut db, id), held(&mut da, id));
        let items = da.doc(id).unwrap().entries(Kind::Set, "s");
        for it in items.iter().step_by(3) {
            assert!(da.doc(id).unwrap().set_delete("s", &it.key).unwrap());
        }
        assert_eq!(waiting(&da, &b).len(), 100);
        resync(&mut da, b);
        settle(&mut da, &mut db, now);
        assert_eq!(db.doc(id).unwrap().entries(Kind::Set, "s").len(), 200);
        assert_eq!(held(&mut db, id), held(&mut da, id));
    }

    /// Our reply to its ask for a doc we haven't open, dropped with the rest, goes again: else the member, which
    /// asks us alone, waits on that ask for good, and doesn't ask us back once we open the doc and ask it.
    #[test]
    fn a_reply_for_a_doc_not_open_goes_again_when_dropped() {
        let (mut da, mut db, _, a, b) = pair();
        let now = Instant::from_millis(0);
        // A copy of "other" holding one entry, as a third member wrote it.
        let mut dc = Docs::new(PeerId([0xcc; 32]), [3; 32]);
        let cid = dc.open(now, "other", Options::default()).unwrap();
        dc.doc(cid).unwrap().map_set("m", "k", "\"v\"").unwrap();
        let copy = dc.doc(cid).unwrap().snapshot();
        let bid = db.open(now, "other", Options::default()).unwrap();
        da.joined(now, b, Route::Direct);
        db.joined(now, a, Route::Direct);
        while let Some((_, m)) = db.poll_transmit() {
            assert!(da.receive(now, b, &m));
        }
        let reply = waiting(&da, &b).into_iter().find(|m| m.starts_with(r#"{"d":"other","t":3,"ws":[],"q":"#)).unwrap();
        assert!(reply.ends_with(r#","i":0,"n":0,"end":1}"#), "{reply}");
        resync(&mut da, b);
        assert_eq!(waiting(&da, &b)[0], reply);
        let now = settle(&mut da, &mut db, now);
        da.open(now, "other", Options { copy: Some(&copy), ..Options::default() }).unwrap();
        settle(&mut da, &mut db, now);
        assert_eq!(db.doc(bid).unwrap().get(Kind::Map, "m", "k"), Some("\"v\""));
        assert_eq!(db.doc(bid).unwrap().sync().state, crate::State::Synced);
        // The latest reply for each doc, for the last UNOPENED docs asked for.
        let (mut d, b) = lone();
        let ask = |d: &mut Docs, name: &str, q: u64| {
            let f = format!(r#"{{"d":"{name}","t":2,"ws":[],"r":[],"q":{q}}}"#);
            assert!(d.receive(now, b, &[&[DOC_TAG][..], f.as_bytes()].concat()));
        };
        for n in 0..=UNOPENED {
            ask(&mut d, &format!("n{n}"), 1);
        }
        ask(&mut d, "n5", 2);
        resync(&mut d, b);
        let end = |n: usize, q: u64| format!(r#"{{"d":"n{n}","t":3,"ws":[],"q":{q},"i":0,"n":0,"end":1}}"#);
        let mut want: Vec<String> = (1..=UNOPENED).filter(|&n| n != 5).map(|n| end(n, 1)).collect();
        want.push(end(5, 2));
        assert_eq!(waiting(&d, &b)[..UNOPENED], want[..]);
        assert!(waiting(&d, &b)[UNOPENED..].iter().all(|m| m.starts_with(r#"{"d":"board","#)));
    }

    /// `a` with member `b`, which joined at 0 s and whose link took what `a` sent it then.
    fn lone() -> (Docs, PeerId) {
        let (mut da, _, _, _, b) = pair();
        da.joined(Instant::from_millis(0), b, Route::Direct);
        while da.poll_transmit().is_some() {}
        da.set_now(Instant::from_millis(0));
        (da, b)
    }

    fn at(d: &mut Docs, s: f64) {
        d.set_now(Instant::from_micros((s * 1e6) as u64));
    }

    /// Drops `peer`'s messages at `s` seconds: whether that forgot it (Overflow) rather than resynced it. A member
    /// that never asked us is sent our ask alone, if it is its turn.
    fn drop_at(d: &mut Docs, peer: PeerId, s: f64) -> bool {
        at(d, s);
        resync(d, peer);
        let cut = overflows(d) == [Event::Overflow(peer)];
        if !cut {
            let sent = waiting(d, &peer);
            assert!(sent.len() <= 1 && sent.iter().all(|m| m.starts_with(r#"{"d":"board","t":2,"#)), "{sent:?}");
        }
        cut
    }

    /// A member is forgotten (Overflow) only once its link has taken nothing across RESYNCS resyncs and for
    /// STALL_US. A message its link refused, handed back with retry, isn't taken; one taken is timed by the next
    /// call that brings the time in.
    #[test]
    fn a_member_whose_link_takes_nothing_is_forgotten_after_resyncs_and_a_stall() {
        // A burst of drops in the 30 s after its link last took one: resynced each time.
        let (mut d, b) = lone();
        for s in [1.0, 1.0, 1.0, 1.0, 29.999_999] {
            assert!(!drop_at(&mut d, b, s), "{s} s");
        }
        assert!(drop_at(&mut d, b, 30.0));
        // 30 s on, it is still resynced twice first.
        let (mut d, b) = lone();
        assert_eq!([40.0, 40.0, 40.0].map(|s| drop_at(&mut d, b, s)), [false, false, true]);
        // The 30 s count from when it joined.
        let (mut d, _) = lone();
        let c = PeerId([0xcc; 32]);
        d.joined(Instant::from_millis(40_000), c, Route::Direct);
        assert_eq!([41.0, 41.0, 41.0, 70.0].map(|s| drop_at(&mut d, c, s)), [false, false, false, true]);
        // A message its link took starts both again.
        let (mut d, b) = lone();
        assert_eq!([10.0, 10.0].map(|s| drop_at(&mut d, b, s)), [false, false]);
        at(&mut d, 20.0);
        assert!(d.poll_transmit().is_some_and(|(p, _)| p == b));
        at(&mut d, 20.0);
        assert_eq!([40.0, 40.0, 40.0, 49.9].map(|s| drop_at(&mut d, b, s)), [false; 4]);
        let (mut d, b) = lone();
        assert_eq!([10.0, 10.0].map(|s| drop_at(&mut d, b, s)), [false, false]);
        at(&mut d, 20.0);
        assert!(d.poll_transmit().is_some());
        at(&mut d, 20.0);
        assert_eq!([50.0, 50.0, 50.0].map(|s| drop_at(&mut d, b, s)), [false, false, true]);
        // Taken while the clock stood at 40 s (writes and poll_transmit bring no time in): never cut on that
        // clock's say, however often dropped, but timed by the next call that brings the time in.
        let (mut d, b) = lone();
        assert_eq!([40.0, 40.0].map(|s| drop_at(&mut d, b, s)), [false, false]);
        assert!(d.poll_transmit().is_some());
        for _ in 0..3 {
            resync(&mut d, b);
            assert_eq!(overflows(&mut d), []);
        }
        at(&mut d, 60.0);
        assert_eq!([89.9, 90.0].map(|s| drop_at(&mut d, b, s)), [false, true]);
        // One its link refused doesn't.
        let (mut d, b) = lone();
        assert_eq!([10.0, 10.0].map(|s| drop_at(&mut d, b, s)), [false, false]);
        at(&mut d, 20.0);
        let (p, m) = d.poll_transmit().unwrap();
        d.retry(p, m.clone());
        assert!(d.poll_transmit().is_none(), "held until poll_transmit has run dry");
        assert!(drop_at(&mut d, b, 30.0));
        assert_eq!(d.queued(&b), 0);
        d.retry(b, m);
        assert_eq!(d.queued(&b), 0, "nothing is kept for a member that isn't one");
        // Joined again, it is asked and sent the doc again.
        d.joined(Instant::from_micros(30_000_000), b, Route::Direct);
        assert!(d.poll_transmit().is_some_and(|(p, _)| p == b));
    }

    /// A member resynced keeps its budgets, as it does if it leaves and comes back at once: else one that drops its
    /// own messages, asking us and acknowledging pieces its link never took, would have us build answers for it
    /// without end, and never rejoin. The clock stands still here, so no budget refills.
    #[test]
    fn a_member_resynced_keeps_its_budgets() {
        let doc = |text: String| [&[DOC_TAG][..], text.as_bytes()].concat();
        let now = Instant::from_millis(0);
        // Our answers to its asks and our asks of it, each resynced: 8 of each (the burst), our ask as it joined
        // among them.
        let (mut d, b) = lone();
        let (mut answered, mut asked) = (0, 0);
        for q in 1..=12 {
            assert!(d.receive(now, b, &doc(format!(r#"{{"d":"board","t":2,"ws":[],"r":[],"q":{q}}}"#))));
            answered += waiting(&d, &b).iter().filter(|m| m.contains(&format!(r#""q":{q},"#))).count();
            resync(&mut d, b);
            asked += waiting(&d, &b).iter().filter(|m| m.contains(r#""t":2,"#)).count();
        }
        assert_eq!((answered, asked), (8, 7));
        // Its chars budget: one acknowledging every piece of 40 answers is built two docs' worth, the doc's 69
        // values of 60,000 chars of 3 bytes each as UTF-8, and resynced once.
        let (mut d, _, id, _, b) = pair();
        let value = format!("\"{}\"", "\u{4e2d}".repeat(60_000));
        let mut n = 0;
        while n < 100 && d.doc(id).unwrap().map_set("big", &format!("k{n}"), &value).unwrap() {
            n += 1;
        }
        assert_eq!(n, 69);
        d.joined(now, b, Route::Direct);
        while d.poll_transmit().is_some() {}
        let (mut built, mut resyncs) = (0, 0);
        for q in 1..=40 {
            let ask = format!(r#"{{"d":"board","t":2,"ws":[],"r":[],"q":{q}}}"#);
            let more = (0..64).map(|i| format!(r#"{{"d":"board","t":4,"q":{q},"i":{i}}}"#));
            for f in [ask].into_iter().chain(more) {
                let before = d.queued(&b);
                assert!(d.receive(now, b, &doc(f)));
                match d.queued(&b).checked_sub(before) {
                    Some(grew) => built += grew,
                    None => resyncs += 1,
                }
            }
        }
        assert!(built <= 6 * crate::MAX_DOC_LENGTH + crate::MESSAGE_BYTES, "{built} B built");
        assert!(built > OUTBOX_BYTES && resyncs == 1 && overflows(&mut d).is_empty(), "{resyncs} resyncs");
    }

    /// The cap is OUTBOX_BYTES, retried messages included: a queue of exactly that is kept.
    #[test]
    fn a_queue_of_outbox_bytes_is_kept() {
        for retried in [false, true] {
            let (mut d, b) = lone();
            d.queue(vec![(b, "x".repeat(OUTBOX_BYTES - 1))]);
            assert_eq!(d.queued(&b), OUTBOX_BYTES);
            match retried {
                false => d.queue(vec![(b, String::new())]),
                true => d.retry(b, vec![DOC_TAG]),
            }
            assert!(d.queued(&b) < 1000, "retried: {retried}");
        }
    }

    /// A hub with members `a`, `b` and `c`; `a` writes through `leaf`, its own docs, linked to the hub alone.
    fn star() -> (Docs, Docs, DocId, [PeerId; 3]) {
        let (h, a, b, c) = (PeerId([0x11; 32]), PeerId([0xaa; 32]), PeerId([0xbb; 32]), PeerId([0xcc; 32]));
        let now = Instant::from_millis(0);
        let mut hub = Docs::new(h, [1; 32]);
        hub.open(now, "board", Options { hub: true, ..Options::default() }).unwrap();
        for p in [a, b, c] {
            hub.joined(now, p, Route::Direct);
        }
        let mut leaf = Docs::new(a, [2; 32]);
        let lid = leaf.open(now, "board", Options::default()).unwrap();
        leaf.joined(now, h, Route::Direct);
        while hub.poll_transmit().is_some() {}
        while leaf.poll_transmit().is_some() {}
        (hub, leaf, lid, [a, b, c])
    }

    /// `a`'s next frames: it adds a set item of a thousand 1e20s, 5 bytes each as written here where
    /// JSON.stringify writes 21 digits and a comma, then deletes it.
    fn flood(leaf: &mut Docs, lid: DocId) -> Vec<Vec<u8>> {
        let mut d = leaf.doc(lid).unwrap();
        let id = d.set_add("s", &format!("[{}1]", "1e20,".repeat(1000))).unwrap().unwrap();
        assert!(d.set_delete("s", &id).unwrap());
        std::iter::from_fn(|| leaf.poll_transmit())
            .map(|(_, m)| String::from_utf8(m).unwrap().replace("100000000000000000000", "1e20").into_bytes())
            .collect()
    }

    /// A hub charges the member whose frame it relays what it queued for the others, each message's tag and
    /// closing brace included: over 4 times the frame here, as values go on as JSON.stringify writes them.
    #[test]
    fn a_hub_charges_a_member_what_it_queued_for_the_others() {
        let (mut hub, mut leaf, lid, [a, b, c]) = star();
        let now = Instant::from_millis(0);
        let frames = flood(&mut leaf, lid);
        assert!(hub.receive(now, a, &frames[0]));
        let (qb, qc) = (hub.queued(&b), hub.queued(&c));
        assert!(qb == qc && qb > frames[0].len() * 4, "{qb} queued for a frame of {}", frames[0].len());
        assert_eq!(hub.relays[&a], relay_us(qb + qc));
        assert_eq!(hub.queued(&a), 0, "nothing goes back to it");
        // Its delete is charged too, and nothing else is.
        assert!(hub.receive(now, a, &frames[1]));
        assert_eq!(hub.relays[&a], relay_us(qb + qc) + relay_us(hub.queued(&b) + hub.queued(&c) - qb - qc));
        assert_eq!(overflows(&mut hub), []);
    }

    fn overflows(docs: &mut Docs) -> Vec<Event> {
        std::iter::from_fn(|| docs.poll_event()).filter(|e| matches!(e, Event::Overflow(_))).collect()
    }

    /// A member that has had more than RELAY_BYTES relayed is forgotten, and the members it filled the queues of
    /// are kept. Coming back doesn't refill its budget; time does, and a budget whole again is let go.
    #[test]
    fn a_member_past_its_relay_budget_is_forgotten_not_its_readers() {
        let (mut hub, mut leaf, lid, [a, b, c]) = star();
        let mut now = Instant::from_millis(0);
        let relayed = |hub: &Docs| hub.queued(&b) + hub.queued(&c);
        let mut cut = None;
        for round in 0.. {
            for f in flood(&mut leaf, lid) {
                let before = relayed(&hub);
                assert!(hub.receive(now, a, &f));
                if let Some(e) = overflows(&mut hub).pop() {
                    assert_eq!(e, Event::Overflow(a));
                    assert!(before <= RELAY_BYTES && relayed(&hub) > RELAY_BYTES, "{before} then {}", relayed(&hub));
                    cut = Some(round);
                }
            }
            if cut.is_some() {
                break;
            }
        }
        assert!(overflows(&mut hub).is_empty(), "b and c are kept");
        assert!(cut.unwrap() > 150, "{cut:?} rounds");
        // The frame that overdrew it went on anyway; what it sends now is dropped.
        let held = relayed(&hub);
        for f in flood(&mut leaf, lid) {
            assert!(hub.receive(now, a, &f));
        }
        assert_eq!(relayed(&hub), held);
        while hub.poll_transmit().is_some() {}
        // Back at once: an ask, which has nothing relayed, isn't charged; its first relayed frame overdraws it again.
        hub.joined(now, a, Route::Direct);
        assert!(hub.receive(now, a, &[&[DOC_TAG][..], br#"{"d":"board","t":2,"ws":[],"r":[],"q":9}"#].concat()));
        assert!(hub.queued(&a) > 0 && overflows(&mut hub).is_empty(), "answered, kept");
        let f = flood(&mut leaf, lid);
        assert!(hub.receive(now, a, &f[0]));
        assert_eq!(overflows(&mut hub), [Event::Overflow(a)]);
        // Once its budget is whole again it writes as before, and that budget is let go when a member leaves.
        now = Instant::from_micros(hub.relays[&a]);
        hub.joined(now, a, Route::Direct);
        for f in flood(&mut leaf, lid) {
            assert!(hub.receive(now, a, &f));
        }
        assert_eq!(overflows(&mut hub), []);
        hub.left(now, c);
        assert!(hub.relays.contains_key(&a), "not whole yet");
        hub.left(Instant::from_micros(hub.relays[&a]), b);
        assert!(hub.relays.is_empty());
    }

    /// The budget refills at RELAY_BYTES_PER_S: a member that has 90% of that relayed is never cut, and one that
    /// has 110% is cut once the 10% over it adds up to RELAY_BYTES.
    #[test]
    fn a_relay_budget_refills_at_its_rate() {
        for (pct, cut_s) in [(90, None), (110, Some(RELAY_BYTES as f64 / (RELAY_BYTES_PER_S as f64 / 10.0)))] {
            let (mut hub, mut leaf, lid, [a, ..]) = star();
            let mut us = 0u64;
            let mut cut = None;
            while us < 1_500_000_000 && cut.is_none() {
                for f in flood(&mut leaf, lid) {
                    assert!(hub.receive(Instant::from_micros(us), a, &f));
                }
                let bytes: usize =
                    std::iter::from_fn(|| hub.poll_transmit()).filter(|(p, _)| *p != a).map(|(_, m)| m.len()).sum();
                if !overflows(&mut hub).is_empty() {
                    cut = Some(us as f64 / 1e6);
                }
                us += bytes as u64 * 1_000_000 * 100 / (RELAY_BYTES_PER_S as u64 * pct);
            }
            match (cut, cut_s) {
                (None, None) => {}
                // Each round is charged before its time passes: a round (under a second) early.
                (Some(s), Some(want)) => assert!((s - want).abs() < 10.0, "{pct}%: cut at {s} s, not {want} s"),
                _ => panic!("{pct}%: cut at {cut:?} s"),
            }
        }
    }
}
