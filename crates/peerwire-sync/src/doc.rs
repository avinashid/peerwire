//! One shared doc: doc.js's `Doc`, method for method, so the two hold the same state after the same frames and
//! send the same frames, byte for byte. doc.js says why each step is as it is; comments here say where the port
//! differs. What differs:
//! - the host is [`Host`], lent by [`crate::Docs`] for each call: time comes in, frames and events go out;
//! - nothing is loaded or saved here: the copy is [`Doc::snapshot`] and [`Doc::restore`], the host keeps it;
//! - a hub (the host of a Godot star) relays what each member's frame added to its other members.

use crate::context::{seek, Context, Writer};
use crate::frame::{self, Data, Entry, Frame, Id, Kind, Numbered, Range, ANSWER, ASK, DELTA, MAX_SAFE, MORE};
use crate::json::{json_length, utf16_len, write_number, write_str, Tree, Utf16};
use crate::ordmap::OrdMap;
use crate::{
    Change, DocId, Error, Event, Item, Route, Source, State, Status, DEFAULT_KEEP, FRAME_ITEMS, MAX_DOC_LENGTH,
    MAX_ENTRIES, MAX_KEY_LENGTH, MAX_RANGES, MAX_VALUE_DEPTH, MAX_VALUE_LENGTH, MAX_WRITERS, MESSAGE_BYTES,
    STORED_FRAMES,
};
use peerwire_core::PeerId;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};
use std::fmt::Write as _;

const FORGET_RANGES: usize = 1024;
const CATCHUP_BURST: f64 = 8.0;
const CATCHUP_EVERY_MS: f64 = 15000.0;
const CATCHUP_CHARS: f64 = 2.0 * MAX_DOC_LENGTH as f64;
const CATCHUP_CHARS_PER_S: f64 = 64.0 * 1024.0;
/// Each budget: (burst, ms one unit takes to refill).
const ANSWERS: (f64, f64) = (CATCHUP_BURST, CATCHUP_EVERY_MS);
const ASKS: (f64, f64) = (CATCHUP_BURST, CATCHUP_EVERY_MS);
const CHARS: (f64, f64) = (CATCHUP_CHARS, 1000.0 / CATCHUP_CHARS_PER_S);
const WINDOW: i64 = 4;
const ASK_WAIT_MS: f64 = 5000.0;
const TURN_MS: f64 = 60000.0;
const MAX_INCARNATIONS: u32 = 16;
const WRITER_BYTES: usize = 80;
const RANGE_BYTES: usize = 48;
const ENTRY_BYTES: usize = 96;

impl Route {
    fn rank(self) -> u8 {
        match self {
            Route::Direct => 0,
            Route::Peer => 1,
            Route::Signaling => 2,
        }
    }

    fn piece_chars(self) -> usize {
        match self {
            Route::Direct => 128 * 1024,
            Route::Peer | Route::Signaling => 32 * 1024,
        }
    }
}

/// A timer: when it is due, in µs, and when it was set, so timers due together fire in the order set, as
/// `setTimeout`'s do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Timer {
    pub(crate) at: u64,
    pub(crate) seq: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fire {
    /// The turn of the member asked: see watch.
    Watch,
    Answer(PeerId),
    Ask(PeerId),
}

/// The room's side, lent for one call.
pub(crate) struct Host<'a> {
    pub(crate) me: PeerId,
    /// Joined members, in the order they joined.
    pub(crate) peers: &'a [PeerId],
    pub(crate) routes: &'a BTreeMap<PeerId, Route>,
    pub(crate) now_us: u64,
    pub(crate) seq: &'a mut u64,
    pub(crate) sent: &'a mut Vec<(PeerId, String)>,
    pub(crate) events: &'a mut VecDeque<Event>,
    pub(crate) incarnation: &'a mut dyn FnMut() -> u32,
    pub(crate) id: DocId,
    /// Bytes a hub relayed of a member's frame, as queued for its other members: what [`crate::Docs`] charges it.
    pub(crate) relayed: usize,
}

impl Host<'_> {
    fn now(&self) -> f64 {
        self.now_us as f64 / 1000.0
    }

    /// `host.after(Math.ceil(ms))`.
    fn after(&mut self, ms: f64) -> Timer {
        *self.seq += 1;
        let ms = ms.ceil().max(0.0) as u64;
        Timer { at: self.now_us.saturating_add(ms.saturating_mul(1000)), seq: *self.seq }
    }

    fn route(&self, p: &PeerId) -> Route {
        self.routes.get(p).copied().unwrap_or(Route::Direct)
    }

    fn send(&mut self, p: PeerId, frame: String) {
        self.sent.push((p, frame));
    }
}

/// Our latest ask of a member, until its answer ends.
struct Ask {
    q: u64,
    at: f64,
    last: f64,
    got: u64,
    n: u64,
    paused: bool,
}

/// Our answer to a member's ask, under way: the ids of the entries it lacks, newest first, then the dots we know
/// are dead; pieces numbered from 0 (`next`), acknowledged up to `acked`.
struct Answer {
    q: u64,
    list: Vec<Id>,
    i: usize,
    dead: Vec<Range>,
    j: usize,
    next: i64,
    acked: i64,
}

/// A member's catch-up (doc.js `member()`): when each budget is whole again, its latest ask while it waits for
/// them, our answer under way, our ask of it, and the waits.
#[derive(Default)]
struct Member {
    answers: f64,
    asks: f64,
    chars: f64,
    theirs: Option<Context>,
    want: u64,
    out: Option<Answer>,
    answer_wait: Option<Timer>,
    z: bool,
    ask: Option<Ask>,
    ask_wait: Option<Timer>,
    answered: bool,
    stalled: bool,
    ask_again: bool,
    preempted: bool,
}

/// How long until budget `b`, whole again at `whole`, has one unit to spend; at most 0 if it has now.
fn early(whole: f64, b: (f64, f64), now: f64) -> f64 {
    whole.max(now) - now - (b.0 - 1.0) * b.1
}

fn spend(whole: &mut f64, b: (f64, f64), n: f64, now: f64) {
    *whole = whole.max(now) + n * b.1;
}

struct Coll {
    keys: BTreeMap<Utf16, Vec<Id>>,
    items: BTreeSet<Id>,
    /// A log's entries, the oldest on top: trimming is the only way they go, so it holds exactly its items.
    heap: BinaryHeap<Reverse<(u64, Writer, u64)>>,
    keep: u64,
}

/// A key or item of a collection: a map key, or an item's id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Ref {
    Key(Utf16),
    Item(Id),
}

fn peer_of(w: &Writer) -> PeerId {
    let mut p = [0u8; 32];
    p.copy_from_slice(&w[..32]);
    PeerId(p)
}

pub(crate) fn hex(b: &[u8], out: &mut String) {
    for x in b {
        let _ = write!(out, "{x:02x}");
    }
}

/// A value from the app, as JSON text: its text as members will see it (what JSON.stringify gives back) and
/// its length.
fn as_json(text: &str) -> Result<(String, usize), Error> {
    let t = Tree::parse(text.as_bytes()).ok_or(Error::Json)?;
    if !t.depth_within(t.root(), MAX_VALUE_DEPTH) {
        return Err(Error::Value);
    }
    let v = t.text(t.root());
    let len = utf16_len(&v);
    if len > MAX_VALUE_LENGTH {
        return Err(Error::Value);
    }
    Ok((v, len))
}

fn collection(name: &str) -> Result<Utf16, Error> {
    let name: Utf16 = name.encode_utf16().collect();
    if !frame::valid_name(&name) {
        return Err(Error::Name);
    }
    Ok(name)
}

/// Frames of one kind being cut (doc.js `pack`): each under MESSAGE_BYTES as UTF-8, as estimated.
struct Pack<'a> {
    head: String,
    start: usize,
    frames: Vec<String>,
    ws: Vec<Writer>,
    index: BTreeMap<Writer, usize>,
    e: Vec<(usize, &'a Entry)>,
    x: Vec<(usize, u64, u64)>,
    bytes: usize,
}

impl<'a> Pack<'a> {
    fn new(name: &[u16], t: u8) -> Self {
        let mut head = String::from("{\"d\":");
        write_str(name, &mut head);
        let _ = write!(head, ",\"t\":{t},\"ws\":[");
        let start = 3 * json_length(name) + 64;
        Pack {
            head,
            start,
            frames: Vec::new(),
            ws: Vec::new(),
            index: BTreeMap::new(),
            e: Vec::new(),
            x: Vec::new(),
            bytes: start,
        }
    }

    fn writer(&mut self, w: &Writer) -> usize {
        if let Some(&i) = self.index.get(w) {
            return i;
        }
        self.index.insert(*w, self.ws.len());
        self.ws.push(*w);
        self.bytes += WRITER_BYTES;
        self.ws.len() - 1
    }

    fn place(&mut self, w: &Writer, size: usize) {
        let n = self.e.len() + self.x.len();
        let more = if self.index.contains_key(w) { 0 } else { WRITER_BYTES };
        if n > 0 && (n >= FRAME_ITEMS || self.bytes + size + more > MESSAGE_BYTES) {
            self.finish();
        }
        self.bytes += size;
    }

    fn entry(&mut self, e: &'a Entry) {
        self.place(&e.w, ENTRY_BYTES + 3 * e.len);
        let i = self.writer(&e.w);
        self.e.push((i, e));
    }

    fn range(&mut self, (w, a, b): &Range) {
        self.place(w, RANGE_BYTES);
        let i = self.writer(w);
        self.x.push((i, *a, *b));
    }

    /// The frame so far, as JSON text without its closing brace, so that answers can add their numbers.
    fn finish(&mut self) {
        let mut f = self.head.clone();
        for (i, w) in self.ws.iter().enumerate() {
            f.push_str(if i > 0 { ",\"" } else { "\"" });
            hex(w, &mut f);
            f.push('"');
        }
        f.push(']');
        if !self.e.is_empty() {
            f.push_str(",\"e\":[");
            for (j, (i, e)) in self.e.iter().enumerate() {
                let _ = write!(f, "{}[{i},{},{},\"{}\",", if j > 0 { "," } else { "" }, e.n, e.c, e.t.letter());
                write_str(&e.coll, &mut f);
                f.push(',');
                match &e.k {
                    Some(k) => write_str(k, &mut f),
                    None => f.push_str("null"),
                }
                f.push(',');
                f.push_str(&e.v);
                f.push(']');
            }
            f.push(']');
        }
        if !self.x.is_empty() {
            f.push_str(",\"x\":[");
            for (j, (i, a, b)) in self.x.iter().enumerate() {
                let _ = write!(f, "{}[{i},{a},{b}]", if j > 0 { "," } else { "" });
            }
            f.push(']');
        }
        self.frames.push(f);
        self.ws.clear();
        self.index.clear();
        self.e.clear();
        self.x.clear();
        self.bytes = self.start;
    }

    /// The frames, without their closing braces; the last with `end` if asked.
    fn done(mut self, end: bool) -> Vec<String> {
        self.finish();
        if end {
            if let Some(f) = self.frames.last_mut() {
                f.push_str(",\"end\":1");
            }
        }
        self.frames
    }
}

pub(crate) struct Doc {
    pub(crate) name: Utf16,
    hub: bool,
    entries: OrdMap<Id, Entry>,
    /// writer -> its dots held, in the order they came (a JS Map's)
    by_writer: BTreeMap<Writer, OrdMap<u64, ()>>,
    ctx: Context,
    colls: BTreeMap<(Kind, Utf16), Coll>,
    keeps: BTreeMap<Utf16, u64>,
    length: usize,
    clock: u64,
    incarnations: u32,
    writer: Writer,
    n: u64,
    touched: OrdMap<(Kind, Utf16), OrdMap<Ref, Option<Id>>>,
    /// Writers the context holds with no live entry, longest idle first.
    idle: OrdMap<Writer, ()>,
    members: BTreeMap<PeerId, Member>,
    queue: Vec<PeerId>,
    asking: Option<PeerId>,
    /// The watch on the turn of the member asked, and the number of the ask it watches.
    wait: Option<(Timer, u64)>,
    ask_no: u64,
    synced: bool,
    sync: Status,
    claimed: bool,
    claim: Option<(u64, u64)>,
    unheard: bool,
    dropped: Option<Vec<Range>>,
    range_cap: usize,
    /// A hub, joining a member's frame: the entries it added and the ranges that told us anything.
    relay: Option<(Vec<Entry>, Vec<Range>)>,
}

impl Doc {
    pub(crate) fn new(h: &mut Host, name: Utf16, hub: bool) -> Doc {
        let alone = Status { state: State::Alone, from: None, have: 0, total: 0, source: None, at: None };
        let mut d = Doc {
            name,
            hub,
            entries: OrdMap::default(),
            by_writer: BTreeMap::new(),
            ctx: Context::default(),
            colls: BTreeMap::new(),
            keeps: BTreeMap::new(),
            length: 0,
            clock: 0,
            incarnations: 0,
            writer: [0; 36],
            n: 0,
            touched: OrdMap::default(),
            idle: OrdMap::default(),
            members: BTreeMap::new(),
            queue: Vec::new(),
            asking: None,
            wait: None,
            ask_no: 0,
            synced: false,
            sync: alone,
            claimed: false,
            claim: None,
            unheard: false,
            dropped: None,
            range_cap: MAX_RANGES,
            relay: None,
        };
        d.new_incarnation(h, None);
        // Our asks are numbered from the incarnation's bits, so a doc opened again never takes an answer to the
        // asks of the one before it for its own.
        d.ask_no = (u32::from_be_bytes([d.writer[32], d.writer[33], d.writer[34], d.writer[35]]) as u64) << 20;
        d.sync = d.status(h);
        d
    }

    // Collections.

    /// doc.js `doc.log(name, { keep })`.
    pub(crate) fn log_keep(&mut self, h: &mut Host, name: &str, keep: u64) -> Result<(), Error> {
        if !(1..=MAX_ENTRIES as u64).contains(&keep) {
            return Err(Error::Keep);
        }
        let name = collection(name)?;
        self.keeps.insert(name.clone(), keep);
        if let Some(coll) = self.colls.get_mut(&(Kind::Log, name.clone())) {
            coll.keep = keep;
            self.trim(&name);
            self.flush(h, h.me, true);
        }
        Ok(())
    }

    // The room's side.

    /// Asks every member: the doc was just opened (and its copy, if any, restored).
    pub(crate) fn open(&mut self, h: &mut Host) {
        for &p in h.peers {
            self.start_ask(h, p);
        }
        self.emit_sync(h);
    }

    pub(crate) fn joined(&mut self, h: &mut Host, peer: PeerId) {
        self.start_ask(h, peer);
    }

    /// `peer` left: the host has taken it out of `h.peers` already.
    pub(crate) fn left(&mut self, h: &mut Host, peer: PeerId) {
        self.members.remove(&peer);
        self.queue.retain(|p| *p != peer);
        if self.asking == Some(peer) {
            self.next_ask(h, true);
        }
        self.emit_sync(h);
    }

    pub(crate) fn rerouted(&mut self, h: &mut Host) {
        self.better(h);
    }

    /// What we sent `peer` was dropped before its link took it (a hub's relays piled up for it): it is caught up
    /// afresh, as if it left and joined again, though it saw neither. Our answer to its latest ask ends, empty, or
    /// it would wait on it and never ask us again; the dots we know are dead go to it, as an answer would end with
    /// them, since our ask can't show it a delete of an entry it holds; and our ask shows it any entry it missed,
    /// so it asks us back and catches up on the paced path. Its ask's number is kept for the next time, and so are
    /// its budgets: a member that leaves them full by dropping its own messages (acknowledging pieces its link
    /// never took) would have us build answers for it without end, where leaving costs it a rejoin.
    pub(crate) fn resync(&mut self, h: &mut Host, peer: PeerId) {
        let kept = self.members.get(&peer).map(|st| (st.want, st.answers, st.asks, st.chars));
        self.left(h, peer);
        let st = self.members.entry(peer).or_default();
        (st.want, st.answers, st.asks, st.chars) = kept.unwrap_or_default();
        let q = st.want;
        if q > 0 {
            let mut f = String::from("{\"d\":");
            write_str(&self.name, &mut f);
            let _ = write!(f, ",\"t\":{ANSWER},\"ws\":[],\"q\":{q},\"i\":0,\"n\":0,\"end\":1}}");
            h.send(peer, f);
            let dead = self.dead();
            if !dead.is_empty() {
                for f in self.pack(ANSWER, &[], &dead, false) {
                    h.send(peer, format!("{f}}}"));
                }
            }
        }
        self.joined(h, peer);
    }

    /// A doc frame from member `peer`, parsed from JSON. One that is malformed is dropped.
    pub(crate) fn receive(&mut self, h: &mut Host, peer: PeerId, t: &Tree, m: u32) {
        let Some(f) = frame::parse(t, m, Some(&peer)) else { return };
        match f {
            Frame::Ask { ranges, q, z } => self.on_ask(h, peer, ranges, q, z),
            Frame::More { q, i } => self.on_more(h, peer, q, i),
            Frame::Data(f) => {
                let (numbered, got, end) = (f.numbered, f.entries.len() as u64, f.end);
                self.join(h, f, Some(peer));
                if let Some(nb) = numbered {
                    self.landed(h, peer, nb, got, end);
                }
            }
        }
    }

    /// Joins frame `f`, a delta or an answer from member `peer`, or from the copy (`None`).
    fn join(&mut self, h: &mut Host, f: Data, peer: Option<PeerId>) {
        self.claimed = false;
        self.claim = None;
        self.unheard = false;
        self.dropped = (f.answer && peer.is_some()).then(Vec::new);
        if self.hub && peer.is_some() {
            self.relay = Some((Vec::new(), Vec::new()));
        }
        self.apply(f, peer.is_some());
        if let Some((a, b)) = self.claim.take() {
            let w = self.writer;
            self.record(&w, a, b);
            if let Some(d) = &mut self.dropped {
                d.push((w, a, b));
            }
            if let Some((_, r)) = &mut self.relay {
                r.push((w, a, b));
            }
        }
        if self.claimed {
            self.new_incarnation(h, None);
        }
        self.flush(h, peer.unwrap_or(h.me), false);
        let dropped = self.dropped.take();
        if let (Some(peer), Some((entries, ranges))) = (peer, self.relay.take()) {
            // A hub sends what the frame added on to its other members, as an answer with no number (a delta
            // carries only its sender's writes): its members reach each other only through it. It stands in for
            // forward(), which would send them the same dead dots again. Values go on as written here, which can be
            // longer than the frame had them (1e20 is 21 digits), so the member is charged what was queued: each
            // message, its tag and closing brace included, once for each member it goes to.
            if !entries.is_empty() || !ranges.is_empty() {
                let refs: Vec<&Entry> = entries.iter().collect();
                let frames = self.pack(ANSWER, &refs, &ranges, false);
                for &p in h.peers.iter().filter(|&&p| p != peer) {
                    for f in &frames {
                        h.relayed += f.len() + 2;
                        h.send(p, format!("{f}}}"));
                    }
                }
            }
        } else if let (Some(peer), Some(d)) = (peer, dropped) {
            if !d.is_empty() {
                self.forward(h, peer, &d);
            }
        }
        if let (true, Some(peer)) = (self.unheard, peer) {
            self.start_ask(h, peer);
        }
    }

    /// A piece of an answer, applied: if it answers our latest ask of the member, it is acknowledged (unless a
    /// better routed member took the turn), and the last ends the ask.
    fn landed(&mut self, h: &mut Host, peer: PeerId, f: Numbered, got: u64, end: bool) {
        let now = h.now();
        let Some(st) = self.members.get_mut(&peer) else { return };
        let Some(a) = st.ask.as_mut().filter(|a| a.q == f.q) else { return };
        a.got += got;
        a.n = f.n;
        a.last = now;
        if !end {
            if !a.paused {
                let mut m = String::from("{\"d\":");
                write_str(&self.name, &mut m);
                let _ = write!(m, ",\"t\":{MORE},\"q\":{},\"i\":{}}}", a.q, f.i);
                h.send(peer, m);
            }
            return self.emit_sync(h);
        }
        st.ask = None;
        st.answered = true;
        st.preempted = false;
        if f.s {
            self.synced = true;
        }
        if self.asking == Some(peer) {
            self.next_ask(h, true);
        }
        // A member whose turn passed as it stalled is asked again once it has answered, if it was due an ask.
        if let Some(st) = self.members.get_mut(&peer) {
            if st.stalled {
                st.stalled = false;
                if st.ask_again {
                    st.ask_again = false;
                    self.start_ask(h, peer);
                }
            }
        }
        self.emit_sync(h);
    }

    // Local writes.

    pub(crate) fn map_set(&mut self, h: &mut Host, name: &str, key: &str, value: &str) -> Result<bool, Error> {
        let name = collection(name)?;
        let key: Utf16 = key.encode_utf16().collect();
        if key.len() > MAX_KEY_LENGTH {
            return Err(Error::Key);
        }
        let (v, len) = as_json(value)?;
        let old =
            self.colls.get(&(Kind::Map, name.clone())).and_then(|c| c.keys.get(&key)).cloned().unwrap_or_default();
        let cost = len + json_length(&key) + json_length(&name);
        if !self.fits(cost, &old) {
            return Ok(false);
        }
        let e = self.mint(Kind::Map, name, Some(key), v, cost);
        for o in &old {
            self.remove(o);
        }
        self.flush(h, h.me, true);
        let dead: Vec<Range> = old.iter().map(|o| (o.w, o.n, o.n)).collect();
        self.broadcast(h, &[&e], &dead);
        Ok(true)
    }

    pub(crate) fn map_delete(&mut self, h: &mut Host, name: &str, key: &str) -> Result<bool, Error> {
        let name = collection(name)?;
        let key: Utf16 = key.encode_utf16().collect();
        let Some(old) = self.colls.get(&(Kind::Map, name)).and_then(|c| c.keys.get(&key)).cloned() else {
            return Ok(false);
        };
        let dead: Vec<Range> = old.iter().map(|o| (o.w, o.n, o.n)).collect();
        for o in &old {
            self.remove(o);
        }
        self.flush(h, h.me, true);
        self.broadcast(h, &[], &dead);
        Ok(true)
    }

    /// A set item or a log entry: its id, or `None` if the doc refused it.
    pub(crate) fn add(&mut self, h: &mut Host, t: Kind, name: &str, value: &str) -> Result<Option<String>, Error> {
        let name = collection(name)?;
        let (v, len) = as_json(value)?;
        let cost = len + 4 + json_length(&name);
        if !self.fits(cost, &[]) {
            return Ok(None);
        }
        let e = self.mint(t, name.clone(), None, v, cost);
        if t == Kind::Log {
            self.trim(&name);
        }
        self.flush(h, h.me, true);
        self.broadcast(h, &[&e], &[]);
        Ok(Some(e.id().to_string()))
    }

    pub(crate) fn set_delete(&mut self, h: &mut Host, name: &str, id: &str) -> Result<bool, Error> {
        let name = collection(name)?;
        let Some(id) =
            Id::parse(id).filter(|id| self.colls.get(&(Kind::Set, name)).is_some_and(|c| c.items.contains(id)))
        else {
            return Ok(false);
        };
        self.remove(&id);
        self.flush(h, h.me, true);
        self.broadcast(h, &[], &[(id.w, id.n, id.n)]);
        Ok(true)
    }

    /// Whether an entry of `cost` fits in place of `old`. A doc that members filled refuses our write, as every
    /// member refuses one that doesn't fit; so does one with no dot left to write on.
    fn fits(&self, cost: usize, old: &[Id]) -> bool {
        let entries = self.entries.len() - old.len() + 1;
        let length = self.length - old.iter().map(|o| self.entries.get(o).map_or(0, |e| e.len)).sum::<usize>() + cost;
        if self.ctx.next(&self.writer, self.n + 1) > MAX_SAFE {
            return false;
        }
        entries <= MAX_ENTRIES && length <= MAX_DOC_LENGTH
    }

    /// A new entry of ours, on the next dot we haven't seen; a copy of it comes back to send.
    fn mint(&mut self, t: Kind, coll: Utf16, k: Option<Utf16>, v: String, len: usize) -> Entry {
        self.n = self.ctx.next(&self.writer, self.n + 1);
        let c = if self.clock < MAX_SAFE {
            self.clock += 1;
            self.clock
        } else {
            MAX_SAFE
        };
        let w = self.writer;
        self.ctx.add(&w, self.n, self.n);
        let e = Entry { w, n: self.n, c, t, coll, k, v, len };
        self.insert(e.clone());
        e
    }

    /// A writer for our next writes that isn't in `taken` (our context's writers if `None`), drawn again a few
    /// times if it is.
    fn new_incarnation(&mut self, h: &mut Host, taken: Option<&BTreeSet<Writer>>) {
        if self.incarnations >= MAX_INCARNATIONS {
            return;
        }
        self.incarnations += 1;
        let mut draw = || {
            let mut w = [0u8; 36];
            w[..32].copy_from_slice(&h.me.0);
            w[32..].copy_from_slice(&(h.incarnation)().to_be_bytes());
            w
        };
        let is_taken = |w: &Writer| taken.map_or_else(|| self.ctx.writers.contains_key(w), |t| t.contains(w));
        let mut w = draw();
        for _ in 1..4 {
            if !is_taken(&w) {
                break;
            }
            w = draw();
        }
        self.writer = w;
        self.n = 0;
    }

    // State.

    fn apply(&mut self, f: Data, from_member: bool) {
        let sender = !f.answer && from_member;
        for x in f.entries {
            self.put(x, sender);
        }
        let c = Context::of(&f.ranges);
        for (w, l) in c.writers.iter() {
            self.kill(w, l);
        }
    }

    fn put(&mut self, x: Entry, sender: bool) {
        let id = x.id();
        if self.entries.contains_key(&id) {
            return;
        }
        if self.ctx.has(&x.w, x.n) {
            if sender {
                self.unheard = true;
            }
            return;
        }
        // An entry on our own writer that we didn't write (a member claiming our next dot): never held.
        if x.w == self.writer {
            self.claimed = true;
            self.claim = Some(match self.claim {
                None => (x.n, x.n),
                Some((a, b)) => (a.min(x.n), b.max(x.n)),
            });
            return;
        }
        if self.entries.len() >= MAX_ENTRIES || self.length + x.len > MAX_DOC_LENGTH {
            return;
        }
        if !self.record(&x.w, x.n, x.n) {
            return;
        }
        if x.c > self.clock {
            self.clock = x.c;
        }
        let (t, coll) = (x.t, x.coll.clone());
        if let Some((added, _)) = &mut self.relay {
            added.push(x.clone());
        }
        self.insert(x);
        if t == Kind::Log {
            self.trim(&coll);
        }
    }

    /// The dots in ranges `l` of writer w (sorted and apart) are dead: entries of theirs we hold go, except a
    /// log's. Walks the fewer of the dots named and the entries held.
    fn kill(&mut self, w: &Writer, l: &[(u64, u64)]) {
        if *w == self.writer && l.last().is_some_and(|r| r.1 > self.n) {
            self.claimed = true;
        }
        let mut changed = vec![false; l.len()];
        if let Some(held) = self.by_writer.get(w) {
            let not_log = |n: u64| self.entries.get(&Id { w: *w, n }).is_some_and(|e| e.t != Kind::Log);
            let mut drops: Vec<(usize, u64)> = Vec::new();
            let dots = l.iter().fold(0u64, |s, &(a, b)| s.saturating_add(b - a + 1));
            if dots < held.len() as u64 {
                for (i, &(a, b)) in l.iter().enumerate() {
                    for n in a..=b {
                        if held.contains_key(&n) && not_log(n) {
                            drops.push((i, n));
                        }
                    }
                }
            } else {
                for &n in held.keys() {
                    let i = seek(l, n);
                    if i < l.len() && l[i].0 <= n && not_log(n) {
                        drops.push((i, n));
                    }
                }
            }
            // Index in l -> the lowest and highest dot it dropped, in the order first dropped.
            let mut spans: OrdMap<usize, (u64, u64)> = OrdMap::default();
            for (i, n) in drops {
                self.remove(&Id { w: *w, n });
                let s = spans.get_or_insert_with(&i, || (n, n));
                *s = (s.0.min(n), s.1.max(n));
                changed[i] = true;
            }
            if let Some(d) = &mut self.dropped {
                d.extend(spans.values().map(|&(a, b)| (*w, a, b)));
            }
        }
        for (i, &(a, b)) in l.iter().enumerate() {
            let new = self.relay.is_some() && !self.ctx.covers(w, a, b);
            self.record(w, a, b);
            if let Some((_, r)) = &mut self.relay {
                if new || changed[i] {
                    r.push((*w, a, b));
                }
            }
        }
    }

    /// Counts dots a to b of w as seen, if the context has room or can make it. Our own dots past n aren't ours
    /// yet, so a claim on them joins the claims before it in one span.
    fn record(&mut self, w: &Writer, mut a: u64, mut b: u64) -> bool {
        if self.ctx.covers(w, a, b) {
            return true;
        }
        if *w == self.writer && b > self.n {
            if let Some(l) = self.ctx.writers.get(w) {
                let i = seek(l, self.n + 1);
                if i < l.len() {
                    a = a.min(l[i].0);
                    b = b.max(l[l.len() - 1].1);
                }
            }
        }
        if !self.room(w, a, b) {
            self.make_room(Some(w));
        }
        if !self.room(w, a, b) {
            return false;
        }
        self.ctx.add(w, a, b);
        if !self.by_writer.contains_key(w) {
            self.idle.insert(*w, ());
        }
        true
    }

    fn room(&self, w: &Writer, a: u64, b: u64) -> bool {
        let writers = self.ctx.writers.contains_key(w) || self.ctx.writers.len() < MAX_WRITERS;
        writers && (self.ctx.size < self.range_cap || self.ctx.touches(w, a, b))
    }

    /// A full context forgets what matters least: first the writers with no live entry, longest idle first,
    /// then the highest ranges of the writers with the most (never ours), until FORGET_RANGES are free.
    fn make_room(&mut self, keep: Option<&Writer>) {
        let idle: Vec<Writer> = self.idle.keys().copied().collect();
        for w in idle {
            if self.ctx.writers.len() < MAX_WRITERS && self.ctx.size < MAX_RANGES {
                return;
            }
            if Some(&w) == keep || w == self.writer {
                continue;
            }
            self.ctx.drop_writer(&w);
            self.idle.remove(&w);
        }
        if self.ctx.size < MAX_RANGES {
            return;
        }
        let mut most: Vec<(Writer, usize)> = self
            .ctx
            .writers
            .iter()
            .filter(|(w, l)| **w != self.writer && l.len() > 1)
            .map(|(w, l)| (*w, l.len()))
            .collect();
        most.sort_by_key(|&(_, n)| Reverse(n));
        for (w, _) in most {
            while self.ctx.writers.get(&w).is_some_and(|l| l.len() > 1) && self.ctx.size > MAX_RANGES - FORGET_RANGES {
                self.ctx.drop_last(&w);
            }
            if self.ctx.size <= MAX_RANGES - FORGET_RANGES {
                return;
            }
        }
    }

    fn insert(&mut self, e: Entry) {
        let key = (e.t, e.coll.clone());
        if !self.colls.contains_key(&key) {
            let keep = self.keeps.get(&e.coll).copied().unwrap_or(DEFAULT_KEEP);
            let coll = Coll { keys: BTreeMap::new(), items: BTreeSet::new(), heap: BinaryHeap::new(), keep };
            self.colls.insert(key.clone(), coll);
        }
        let id = e.id();
        self.touch(
            e.t,
            &e.coll,
            match &e.k {
                Some(k) if e.t == Kind::Map => Ref::Key(k.clone()),
                _ => Ref::Item(id),
            },
        );
        let idle = &mut self.idle;
        self.by_writer
            .entry(e.w)
            .or_insert_with(|| {
                idle.remove(&e.w);
                OrdMap::default()
            })
            .insert(e.n, ());
        self.length += e.len;
        let coll = self.colls.get_mut(&key).expect("just made");
        match (e.t, &e.k) {
            (Kind::Map, Some(k)) => coll.keys.entry(k.clone()).or_default().push(id),
            _ => {
                coll.items.insert(id);
                if e.t == Kind::Log {
                    coll.heap.push(Reverse(e.stamp()));
                }
            }
        }
        self.entries.insert(id, e);
    }

    fn remove(&mut self, id: &Id) {
        let Some(e) = self.entries.get(id) else { return };
        let (t, coll, k, len) = (e.t, e.coll.clone(), e.k.clone(), e.len);
        self.touch(
            t,
            &coll,
            match &k {
                Some(k) if t == Kind::Map => Ref::Key(k.clone()),
                _ => Ref::Item(*id),
            },
        );
        self.entries.remove(id);
        if let Some(held) = self.by_writer.get_mut(&id.w) {
            held.remove(&id.n);
            if held.is_empty() {
                self.by_writer.remove(&id.w);
                if self.ctx.writers.contains_key(&id.w) {
                    self.idle.insert(id.w, ());
                }
            }
        }
        self.length -= len;
        let key = (t, coll);
        let Some(c) = self.colls.get_mut(&key) else { return };
        match (t, k) {
            (Kind::Map, Some(k)) => {
                if let Some(list) = c.keys.get_mut(&k) {
                    list.retain(|x| x != id);
                    if list.is_empty() {
                        c.keys.remove(&k);
                    }
                }
            }
            _ => {
                c.items.remove(id);
            }
        }
        if c.keys.is_empty() && c.items.is_empty() {
            self.colls.remove(&key);
        }
    }

    /// Trims a log to its newest `keep` entries.
    fn trim(&mut self, name: &[u16]) {
        let key = (Kind::Log, name.to_vec());
        while let Some(c) = self.colls.get_mut(&key).filter(|c| c.items.len() as u64 > c.keep) {
            let Some(Reverse((_, w, n))) = c.heap.pop() else { return };
            self.remove(&Id { w, n });
        }
    }

    /// What a map key shows: of its values, the one that sorts last.
    fn shown(&self, list: Option<&Vec<Id>>) -> Option<Id> {
        list?.iter().filter_map(|id| self.entries.get(id)).max_by_key(|e| e.stamp()).map(Entry::id)
    }

    fn current(&self, t: Kind, name: &[u16], r: &Ref) -> Option<Id> {
        let c = self.colls.get(&(t, name.to_vec()))?;
        match r {
            Ref::Key(k) => self.shown(c.keys.get(k)),
            Ref::Item(id) => c.items.contains(id).then_some(*id),
        }
    }

    /// Notes a key or item about to change, with what it showed before.
    fn touch(&mut self, t: Kind, name: &[u16], r: Ref) {
        let key = (t, name.to_vec());
        if self.touched.get(&key).is_some_and(|refs| refs.contains_key(&r)) {
            return;
        }
        let before = self.current(t, name, &r);
        self.touched.get_or_insert_with(&key, OrdMap::default).insert(r, before);
    }

    /// Emits a change for each key or item a frame or write changed, now that all of it is in.
    fn flush(&mut self, h: &mut Host, from: PeerId, local: bool) {
        let touched = std::mem::take(&mut self.touched);
        for ((t, name), refs) in touched.into_vec() {
            for (r, before) in refs.into_vec() {
                let now = self.current(t, &name, &r);
                if now == before {
                    continue;
                }
                let e = now.and_then(|id| self.entries.get(&id));
                let (key, item) = match r {
                    Ref::Key(k) => (Some(k), None),
                    Ref::Item(id) => (None, Some(id)),
                };
                let change = Change {
                    name: name.clone(),
                    kind: t,
                    key,
                    item,
                    value: e.map(|e| e.v.clone()),
                    from: e.map_or(from, |e| peer_of(&e.w)),
                    local,
                };
                h.events.push_back(Event::Change(h.id, change));
            }
        }
    }

    // Reads.

    pub(crate) fn get(&self, t: Kind, name: &str, r: &str) -> Option<&str> {
        let name: Utf16 = name.encode_utf16().collect();
        let c = self.colls.get(&(t, name))?;
        let id = match t {
            Kind::Map => self.shown(c.keys.get(&r.encode_utf16().collect::<Utf16>()))?,
            _ => Id::parse(r).filter(|id| c.items.contains(id))?,
        };
        self.entries.get(&id).map(|e| e.v.as_str())
    }

    /// A collection's entries as doc.js lists them: a map's keys in order, with the value each shows; a set's
    /// items or a log's entries by stamp, oldest first.
    pub(crate) fn list(&self, t: Kind, name: &str) -> Vec<(Ref, &Entry)> {
        let name: Utf16 = name.encode_utf16().collect();
        let Some(c) = self.colls.get(&(t, name)) else { return Vec::new() };
        if t == Kind::Map {
            c.keys
                .iter()
                .filter_map(|(k, list)| Some((Ref::Key(k.clone()), self.entries.get(&self.shown(Some(list))?)?)))
                .collect()
        } else {
            let mut items: Vec<&Entry> = c.items.iter().filter_map(|id| self.entries.get(id)).collect();
            items.sort_by_key(|e| e.stamp());
            items.into_iter().map(|e| (Ref::Item(e.id()), e)).collect()
        }
    }

    pub(crate) fn items(&self, t: Kind, name: &str) -> Vec<Item> {
        self.list(t, name)
            .into_iter()
            .map(|(r, e)| Item {
                key: match r {
                    Ref::Key(k) => crate::json::lossy(&k),
                    Ref::Item(id) => id.to_string(),
                },
                value: e.v.clone(),
                from: peer_of(&e.w),
            })
            .collect()
    }

    /// doc.js `entries()`, as `JSON.stringify` writes it.
    pub(crate) fn items_json(&self, t: Kind, name: &str) -> String {
        let mut s = String::from("[");
        for (i, (r, e)) in self.list(t, name).into_iter().enumerate() {
            s.push_str(if i > 0 { ",{" } else { "{" });
            match r {
                Ref::Key(k) => {
                    s.push_str("\"key\":");
                    write_str(&k, &mut s);
                }
                Ref::Item(id) => {
                    let _ = write!(s, "\"id\":\"{id}\"");
                }
            }
            let _ = write!(s, ",\"value\":{},\"from\":\"", e.v);
            hex(&e.w[..32], &mut s);
            s.push_str("\"}");
        }
        s.push(']');
        s
    }

    pub(crate) fn sync(&self) -> Status {
        self.sync
    }

    // Sending.

    /// Sends entries and dead ranges to every member, as one delta or a few.
    fn broadcast(&mut self, h: &mut Host, entries: &[&Entry], ranges: &[Range]) {
        let frames = self.pack(DELTA, entries, ranges, false);
        for &p in h.peers {
            for f in &frames {
                h.send(p, format!("{f}}}"));
            }
        }
    }

    /// Sends dead ranges member `peer`'s answer named on to our other members, unless its chars budget is empty,
    /// and charges it their JSON text once for each member.
    fn forward(&mut self, h: &mut Host, peer: PeerId, ranges: &[Range]) {
        let now = h.now();
        let st = self.members.entry(peer).or_default();
        if early(st.chars, CHARS, now) > 0.0 {
            return;
        }
        let to: Vec<PeerId> = h.peers.iter().copied().filter(|&p| p != peer).collect();
        let frames: Vec<String> = self.pack(DELTA, &[], ranges, false).into_iter().map(|f| f + "}").collect();
        let chars: usize = frames.iter().map(|f| utf16_len(f)).sum();
        let st = self.members.entry(peer).or_default();
        spend(&mut st.chars, CHARS, (chars * to.len()) as f64, now);
        for p in to {
            for f in &frames {
                h.send(p, f.clone());
            }
        }
    }

    /// Frames of kind `t` holding `entries` and dead `ranges`, each under MESSAGE_BYTES as UTF-8, with `end` on
    /// the last if asked: as JSON text without the closing brace.
    fn pack(&self, t: u8, entries: &[&Entry], ranges: &[Range], end: bool) -> Vec<String> {
        let mut p = Pack::new(&self.name, t);
        for e in entries {
            p.entry(e);
        }
        for r in ranges {
            p.range(r);
        }
        p.done(end)
    }

    /// Runs `fire` in `ms` if the member stays; one of each kind at a time.
    fn later(h: &mut Host, slot: &mut Option<Timer>, ms: f64) {
        if slot.is_none() {
            *slot = Some(h.after(ms));
        }
    }

    // Catching up.

    /// Queues an ask of `peer`. One whose turn passed as it stalled isn't asked again until it answers.
    fn start_ask(&mut self, h: &mut Host, peer: PeerId) {
        let st = self.members.entry(peer).or_default();
        if st.stalled {
            st.ask_again = true;
            return;
        }
        if self.asking == Some(peer) || self.queue.contains(&peer) {
            return;
        }
        let now = h.now();
        let wait = early(st.asks, ASKS, now);
        if wait > 0.0 {
            return Self::later(h, &mut st.ask_wait, wait);
        }
        spend(&mut st.asks, ASKS, 1.0, now);
        self.queue.push(peer);
        if self.asking.is_none() {
            self.next_ask(h, false);
        } else {
            self.better(h);
        }
    }

    fn rank(&self, h: &Host, p: &PeerId) -> u8 {
        h.route(p).rank()
    }

    /// Gives the turn to the next member in the queue, the best routed (of those, the first queued), and asks it;
    /// `done` ends the turn under way.
    fn next_ask(&mut self, h: &mut Host, done: bool) {
        if done {
            self.wait = None;
            self.asking = None;
        }
        if self.asking.is_some() {
            return;
        }
        self.queue.retain(|p| h.peers.contains(p));
        if !self.queue.is_empty() {
            let mut k = 0;
            for j in 1..self.queue.len() {
                if self.rank(h, &self.queue[j]) < self.rank(h, &self.queue[k]) {
                    k = j;
                }
            }
            let p = self.queue.remove(k);
            let now = h.now();
            self.asking = Some(p);
            self.ask_no += 1;
            let q = self.ask_no;
            self.members.entry(p).or_default().ask = Some(Ask { q, at: now, last: now, got: 0, n: 0, paused: false });
            let f = self.ask_frame(q);
            h.send(p, f);
            self.watch(h, p);
        }
        self.emit_sync(h);
    }

    /// Watches the turn of member `p`: it passes on once the answer has gone ASK_WAIT_MS without a piece or taken
    /// TURN_MS, if another member is waiting to be asked.
    fn watch(&mut self, h: &mut Host, p: PeerId) {
        let Some(a) = self.members.get(&p).and_then(|st| st.ask.as_ref()) else { return };
        let turn = if self.queue.is_empty() { f64::INFINITY } else { a.at + TURN_MS };
        let due = (a.last + ASK_WAIT_MS).min(turn);
        let q = a.q;
        self.wait = Some((h.after(due - h.now()), q));
    }

    fn on_watch(&mut self, h: &mut Host, q: u64) {
        let Some(p) = self.asking else { return };
        let now = h.now();
        let Some(st) = self.members.get_mut(&p) else { return };
        let Some(a) = st.ask.as_ref().filter(|a| a.q == q) else { return };
        if now - a.last >= ASK_WAIT_MS {
            st.stalled = true;
        } else if self.queue.is_empty() || now - a.at < TURN_MS {
            return self.watch(h, p);
        }
        if !self.queue.is_empty() {
            self.next_ask(h, true);
        }
    }

    /// With a member waiting to be asked, the turn passes on if the member whose turn it is stalled or has had
    /// TURN_MS, or the one waiting is better routed: the answer under way is then paused, its member asked again.
    fn better(&mut self, h: &mut Host) {
        let Some(p) = self.asking else { return };
        if self.queue.is_empty() {
            return;
        }
        let now = h.now();
        let Some(st) = self.members.get(&p) else { return };
        if st.stalled || st.ask.as_ref().is_some_and(|a| now - a.at >= TURN_MS) {
            return self.next_ask(h, true);
        }
        let r = self.rank(h, &p);
        if st.preempted || !self.queue.iter().any(|x| self.rank(h, x) < r) {
            return;
        }
        let st = self.members.get_mut(&p).expect("asked");
        if let Some(a) = &mut st.ask {
            a.paused = true;
        }
        st.preempted = true;
        self.queue.push(p);
        self.next_ask(h, true);
    }

    /// Where catching up stands, and where the doc came from: members once caught up, else nowhere yet (with no
    /// copy on the relays, `at` is always none).
    fn status(&self, h: &Host) -> Status {
        let peers = h.peers.len();
        let source = self.synced.then_some(Source::Members);
        if peers == 0 || self.synced {
            let state = if peers > 0 { State::Synced } else { State::Alone };
            return Status { state, from: None, have: 0, total: 0, source, at: None };
        }
        let a = self.asking.and_then(|p| self.members.get(&p)?.ask.as_ref());
        Status {
            state: State::Loading,
            from: self.asking,
            have: a.map_or(0, |a| a.got.min(a.n)),
            total: a.map_or(0, |a| a.n),
            source,
            at: None,
        }
    }

    /// Emits a sync event if where catching up stands, or where the doc came from, changed. Every member having
    /// answered counts as caught up.
    fn emit_sync(&mut self, h: &mut Host) {
        if !self.synced
            && !h.peers.is_empty()
            && h.peers.iter().all(|p| self.members.get(p).is_some_and(|st| st.answered))
        {
            self.synced = true;
        }
        let s = self.status(h);
        if s == self.sync {
            return;
        }
        self.sync = s;
        h.events.push_back(Event::Sync(h.id, s));
    }

    /// Our context, so the member answers with what we lack, and `q` for the answer to name.
    fn ask_frame(&self, q: u64) -> String {
        let mut ws = String::new();
        let mut r = String::new();
        let (mut nw, mut nr) = (0usize, 0usize);
        let mut bytes = 3 * json_length(&self.name) + 64 + WRITER_BYTES;
        for (w, l) in self.ctx.writers.iter() {
            if bytes + WRITER_BYTES + RANGE_BYTES > MESSAGE_BYTES {
                break;
            }
            ws.push_str(if nw > 0 { ",\"" } else { "\"" });
            hex(w, &mut ws);
            ws.push('"');
            let i = nw;
            nw += 1;
            bytes += WRITER_BYTES;
            for (a, b) in l {
                if bytes + RANGE_BYTES > MESSAGE_BYTES || nr >= MAX_RANGES {
                    break;
                }
                let _ = write!(r, "{}[{i},{a},{b}]", if nr > 0 { "," } else { "" });
                nr += 1;
                bytes += RANGE_BYTES;
            }
        }
        let mut f = String::from("{\"d\":");
        write_str(&self.name, &mut f);
        let _ = write!(f, ",\"t\":{ASK},\"ws\":[{ws}],\"r\":[{r}],\"q\":{q}}}");
        f
    }

    fn on_ask(&mut self, h: &mut Host, peer: PeerId, ranges: Vec<Range>, q: u64, z: bool) {
        if ranges.iter().any(|(w, _, b)| *w == self.writer && *b > self.n) {
            self.new_incarnation(h, None);
        }
        let st = self.members.entry(peer).or_default();
        st.z = z;
        st.theirs = Some(Context::of(&ranges));
        st.want = q;
        st.out = None;
        self.serve(h, peer);
        // It has seen dots we haven't: ask it back.
        if ranges.iter().any(|(w, a, b)| !self.ctx.covers(w, *a, *b)) {
            self.start_ask(h, peer);
        }
    }

    /// Starts the answer to the member's latest ask once its answers budget allows, then sends pieces while the
    /// window and the chars budget allow; past a budget, it waits for it.
    fn serve(&mut self, h: &mut Host, peer: PeerId) {
        let now = h.now();
        let Some(st) = self.members.get_mut(&peer) else { return };
        if st.theirs.is_some() {
            let wait = early(st.answers, ANSWERS, now);
            if wait > 0.0 {
                return Self::later(h, &mut st.answer_wait, wait);
            }
            spend(&mut st.answers, ANSWERS, 1.0, now);
            let (theirs, q) = (st.theirs.take().expect("is some"), st.want);
            let o = self.answer_of(&theirs, q);
            self.members.get_mut(&peer).expect("member").out = Some(o);
        }
        loop {
            let now = h.now();
            let Some(st) = self.members.get_mut(&peer) else { return };
            match &st.out {
                Some(o) if o.next - o.acked - 1 < WINDOW => {}
                _ => return,
            }
            let wait = early(st.chars, CHARS, now);
            if wait > 0.0 {
                return Self::later(h, &mut st.answer_wait, wait);
            }
            self.piece(h, peer);
        }
    }

    fn answer_of(&self, theirs: &Context, q: u64) -> Answer {
        let mut list: Vec<&Entry> = self.entries.values().filter(|e| !theirs.has(&e.w, e.n)).collect();
        list.sort_by_key(|e| Reverse(e.stamp()));
        Answer { q, list: list.iter().map(|e| e.id()).collect(), i: 0, dead: self.dead(), j: 0, next: 0, acked: -1 }
    }

    /// Sends the answer's next piece: its next entries still held, then its dead ranges, about the route's piece
    /// size, charged to the member's chars budget. The last has `end`, and `s` if we have caught up.
    fn piece(&mut self, h: &mut Host, peer: PeerId) {
        let size = h.route(&peer).piece_chars();
        let Some(mut o) = self.members.get_mut(&peer).and_then(|st| st.out.take()) else { return };
        let mut entries: Vec<&Entry> = Vec::new();
        let mut ranges: Vec<Range> = Vec::new();
        let mut chars = 0usize;
        while o.i < o.list.len() {
            // Gone since the ask: the member hears of that as the others did.
            if let Some(e) = self.entries.get(&o.list[o.i]) {
                if chars > 0 && chars + ENTRY_BYTES + e.len > size {
                    break;
                }
                entries.push(e);
                chars += ENTRY_BYTES + e.len;
            }
            o.i += 1;
        }
        while o.i == o.list.len() && o.j < o.dead.len() && (chars == 0 || chars + RANGE_BYTES <= size) {
            ranges.push(o.dead[o.j]);
            chars += RANGE_BYTES;
            o.j += 1;
        }
        let end = o.i == o.list.len() && o.j == o.dead.len();
        let frames = self.pack(ANSWER, &entries, &ranges, end);
        let st = self.members.get_mut(&peer).expect("member");
        spend(&mut st.chars, CHARS, chars as f64, h.now());
        let last = frames.len() - 1;
        for (k, mut f) in frames.into_iter().enumerate() {
            let _ = write!(f, ",\"q\":{},\"i\":{},\"n\":{}", o.q, o.next, o.list.len());
            o.next += 1;
            if end && k == last && self.synced {
                f.push_str(",\"s\":1");
            }
            f.push('}');
            h.send(peer, f);
        }
        if !end {
            self.members.get_mut(&peer).expect("member").out = Some(o);
        }
    }

    /// Piece `i` of our answer to the member's ask `q` was applied: the window moves on.
    fn on_more(&mut self, h: &mut Host, peer: PeerId, q: u64, i: u64) {
        let Some(o) = self.members.get_mut(&peer).and_then(|st| st.out.as_mut()) else { return };
        let i = i as i64;
        if q != o.q || i <= o.acked || i >= o.next {
            return;
        }
        o.acked = i;
        self.serve(h, peer);
    }

    /// The dots we have seen and don't hold, as ranges.
    fn dead(&self) -> Vec<Range> {
        let mut out = Vec::new();
        for (w, l) in self.ctx.writers.iter() {
            let mut live: Vec<u64> = self.by_writer.get(w).map(|h| h.keys().copied().collect()).unwrap_or_default();
            live.sort_unstable();
            let mut i = 0;
            for &(a, b) in l {
                let mut s = a;
                while i < live.len() && live[i] <= b {
                    if live[i] >= s {
                        if live[i] > s {
                            out.push((*w, s, live[i] - 1));
                        }
                        s = live[i] + 1;
                    }
                    i += 1;
                }
                if s <= b {
                    out.push((*w, s, b));
                }
            }
        }
        out
    }

    // Timers.

    /// The doc's next timer, and what it does.
    pub(crate) fn next_timer(&self) -> Option<(Timer, Fire)> {
        let mut best = self.wait.map(|(t, _)| (t, Fire::Watch));
        for (&p, st) in &self.members {
            for (t, f) in [(st.answer_wait, Fire::Answer(p)), (st.ask_wait, Fire::Ask(p))] {
                if let Some(t) = t {
                    if best.is_none_or(|(b, _)| t < b) {
                        best = Some((t, f));
                    }
                }
            }
        }
        best
    }

    pub(crate) fn fire(&mut self, h: &mut Host, f: Fire) {
        match f {
            Fire::Watch => {
                if let Some((_, q)) = self.wait.take() {
                    self.on_watch(h, q);
                }
            }
            Fire::Answer(p) => {
                if let Some(st) = self.members.get_mut(&p) {
                    st.answer_wait = None;
                    self.serve(h, p);
                }
            }
            Fire::Ask(p) => {
                if let Some(st) = self.members.get_mut(&p) {
                    st.ask_wait = None;
                    self.start_ask(h, p);
                }
            }
        }
    }

    // The copy.

    /// The doc as a device keeps it: its entries and the dots it knows are dead, as the frames of an answer, and
    /// its Lamport clock: `{"v":1,"c":…,"f":[…]}`.
    pub(crate) fn snapshot(&self) -> String {
        let entries: Vec<&Entry> = self.entries.values().collect();
        let frames = self.pack(ANSWER, &entries, &self.dead(), false);
        let mut s = String::from("{\"v\":1,\"c\":");
        write_number(self.clock as f64, &mut s);
        s.push_str(",\"f\":[");
        for (i, f) in frames.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(f);
            s.push('}');
        }
        s.push_str("]}");
        s
    }

    /// Joins `copy` (see snapshot) into the doc as an answer from ourselves, checked frame by frame as a
    /// member's: false, with nothing joined, if any of it is off.
    pub(crate) fn restore(&mut self, h: &mut Host, copy: &[u8]) -> bool {
        let Some(t) = Tree::parse(copy) else { return false };
        let root = t.root();
        let (v, c, f) = (t.field(root, "v"), t.field(root, "c"), t.field(root, "f"));
        let c = match c.and_then(|c| t.num(c)) {
            Some(c) if c.fract() == 0.0 && (0.0..=MAX_SAFE as f64).contains(&c) => c as u64,
            _ => return false,
        };
        if !v.is_some_and(|v| t.num(v) == Some(1.0)) {
            return false;
        }
        let Some(list) = f.and_then(|f| t.arr(f)) else { return false };
        if list.len() > STORED_FRAMES {
            return false;
        }
        let mut frames = Vec::new();
        let mut writers = BTreeSet::new();
        for &m in list {
            let named = t.field(m, "d").and_then(|d| t.str(d)) == Some(&self.name[..]);
            let answer = t.field(m, "t").and_then(|x| t.num(x)) == Some(ANSWER as f64);
            if !named || !answer || t.field(m, "q").is_some() {
                return false;
            }
            let Some(Frame::Data(f)) = frame::parse(&t, m, None) else { return false };
            for &w in t.arr(t.field(m, "ws").expect("parsed")).expect("parsed") {
                writers.insert(frame::writer_of(t.str(w).expect("parsed")).expect("parsed"));
            }
            frames.push(f);
        }
        if self.n == 0 && writers.contains(&self.writer) {
            self.new_incarnation(h, Some(&writers));
        }
        self.range_cap = MAX_RANGES + MAX_ENTRIES;
        for f in frames {
            self.join(h, f, None);
        }
        self.range_cap = MAX_RANGES;
        if self.ctx.size >= MAX_RANGES {
            self.make_room(None);
        }
        if c > self.clock {
            self.clock = c;
        }
        true
    }
}
