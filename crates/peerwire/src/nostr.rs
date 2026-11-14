//! Signaling over public Nostr relays (NIP-01), wire-compatible with
//! `packages/js/src/nostr.js`: ephemeral events of kind 25059 (relays forward them to
//! live subscribers and don't store them), one `p` tag holding the [`Topic`] in
//! hex, the payload in base64 as `content`, and a fresh secp256k1 key per carrier
//! so nothing links events to the Peerwire identity or to other sessions.
//!
//! Each relay gets a small thread that does the blocking websocket I/O; the
//! [`Signaling`] side never blocks. `wss://` uses rustls with the webpki roots;
//! `ws://` is plain TCP, for a relay on the LAN or loopback.

use crate::signaling::{b64, hex, unb64, Signaling, Topic};
use k256::schnorr::SigningKey;
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::Message;

/// Ephemeral event kind used by Peerwire signaling.
pub const KIND: u64 = 25059;
/// Relays [`NostrSignaling::default`] connects to: three operators (Primal, nostr.net, 0xtr) on
/// two networks. Each took and passed on our kind-25059 events and kept a kind-30078 one when
/// checked on 14 Nov 2026. nos.lol and nostr.mom, the defaults before, now drop our events of
/// both kinds with no `OK` and pass none on.
pub const DEFAULT_RELAYS: &[&str] = &["wss://relay.primal.net", "wss://relay.nostr.net", "wss://nostr.oxtr.dev"];

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a relay thread blocks reading before it looks at its queue again.
const READ_SLICE: Duration = Duration::from_millis(20);
const SMALL: bool = cfg!(target_os = "espidf");
const LIMITS: Limits = if SMALL { Limits::SMALL } else { Limits::FULL };
/// A relay thread's stack on ESP-IDF, where a thread gets 4 KB unless asked: the TLS
/// handshake overflowed that (NB-095). Elsewhere the platform's default.
const RELAY_STACK: Option<usize> = if SMALL { Some(32 << 10) } else { None };
/// Relay messages with more `[`, `{` and `,` than this are dropped unparsed: a parsed
/// JSON value costs several times its text, most of all as many small values (4.5 KB of
/// empty tags took 192 KB, NB95-1). A Peerwire event has 13.
const MAX_SHAPE: usize = 64;
/// Nor more `{` or `[` than these: an object costs about 630 B parsed even with one key,
/// and nesting costs stack too (738 B of 50 nested objects took +65.6 KB of heap and 33 KB
/// of stack, NB95-1a). A Peerwire event has 1 and 3.
const MAX_OBJECTS: usize = 2;
const MAX_ARRAYS: usize = 8;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A connection that stayed up this long resets the back-off. One that drops sooner, as
/// one a frame over the cap closes does, keeps it, so a sender can't make the carrier
/// redo the TLS handshake every second (NB95-1).
const STABLE: Duration = MAX_BACKOFF;

/// Caps on what a carrier takes and keeps.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// Largest websocket message or frame taken from a relay (tungstenite's default is
    /// 64 MiB); a larger one closes the connection.
    message: usize,
    /// Events received but not yet polled, and their bytes; past either, relays' events
    /// are dropped. The channel holds room for `inbox` events from the start.
    inbox: usize,
    inbox_bytes: usize,
    /// Events kept for a relay that is down or not taking them; older ones are dropped.
    queued: usize,
    /// Event ids remembered to drop the copy each relay delivers.
    seen: usize,
    /// Events a relay connection awaits an `OK` for ([`RelayStats::unanswered`]); past it, the oldest counts as
    /// unanswered.
    waiting: usize,
}

impl Limits {
    const FULL: Limits =
        Limits { message: 128 << 10, inbox: 1024, inbox_bytes: 8 << 20, queued: 64, seen: 4096, waiting: 256 };
    /// ESP-IDF's: an ESP32 has about 300 KB of heap, and 80 KB free with two links. A
    /// full inbox took 53 KB of it from the start and the ids remembered about 170 B each
    /// (NB-095); a signal event is at most about 2.3 KB (NB95-1).
    const SMALL: Limits =
        Limits { message: 4 << 10, inbox: 32, inbox_bytes: 16 << 10, queued: 8, seen: 128, waiting: 32 };
}

/// Counters for a [`NostrSignaling`] carrier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NostrStats {
    /// Relays with an open connection now.
    pub relays_open: u64,
    pub published: u64,
    pub received: u64,
    /// Events a relay refused (`OK false`).
    pub rejected: u64,
}

/// One relay's counters since the carrier started ([`NostrSignaling::relay_stats`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelayStats {
    /// The relay's URL, as given.
    pub url: String,
    /// Whether a connection to it is open now.
    pub open: bool,
    /// Events written to it.
    pub sent: u64,
    /// Events it took (`OK true`).
    pub accepted: u64,
    /// Events it refused (`OK false`).
    pub rejected: u64,
    /// Events it never answered: no `OK` within 10 s on the connection they went out on. A relay that drops events
    /// without a word, as two default relays did (NB-130), counts them here, and so does one past its wait queue's
    /// cap of answers awaited.
    pub unanswered: u64,
}

/// The relays' counters of a [`NostrSignaling`], to read once the carrier has gone into a node
/// ([`NostrSignaling::relay_watch`]).
#[derive(Clone)]
pub struct RelayWatch(Arc<[Arc<RelayCounters>]>);

impl RelayWatch {
    /// Each relay's counters, in the order the carrier was given them.
    pub fn stats(&self) -> Vec<RelayStats> {
        self.0.iter().map(|c| c.stats()).collect()
    }
}

impl std::fmt::Debug for RelayWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.stats()).finish()
    }
}

/// How long a relay has to answer an event with an `OK` ([`RelayStats::unanswered`]).
const ANSWER_WAIT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(10) };

/// A relay's counters, shared with its thread.
#[derive(Default)]
struct RelayCounters {
    url: String,
    open: std::sync::atomic::AtomicBool,
    sent: Counter,
    accepted: Counter,
    rejected: Counter,
    unanswered: Counter,
}

impl RelayCounters {
    #[allow(clippy::useless_conversion)] // from u32 where there are no 64-bit atomics
    fn stats(&self) -> RelayStats {
        let n = |c: &Counter| u64::from(c.load(Ordering::Relaxed));
        RelayStats {
            url: self.url.clone(),
            open: self.open.load(Ordering::Relaxed),
            sent: n(&self.sent),
            accepted: n(&self.accepted),
            rejected: n(&self.rejected),
            unanswered: n(&self.unanswered),
        }
    }
}

// The ESP32-C3 has no 64-bit atomics; its counters wrap at 2^32.
#[cfg(target_has_atomic = "64")]
type Counter = std::sync::atomic::AtomicU64;
#[cfg(not(target_has_atomic = "64"))]
type Counter = std::sync::atomic::AtomicU32;

#[derive(Default)]
struct Shared {
    open: Counter,
    received: Counter,
    rejected: Counter,
    /// The inbox's bytes, against `Limits::inbox_bytes`.
    inbox_bytes: AtomicUsize,
}

enum Cmd {
    /// The subscription to keep open (`None`: none).
    Req(Option<String>),
    /// An event's message, and its id's first 16 bytes, to match the relay's `OK`.
    Event(String, u128),
}

struct Incoming {
    /// The first 16 bytes of the event's id (its SHA-256, checked), for the seen filter.
    id: u128,
    pubkey: String,
    tag: String,
    content: String,
}

impl Incoming {
    fn size(&self) -> usize {
        16 + self.pubkey.len() + self.tag.len() + self.content.len()
    }
}

/// A [`Signaling`] carrier over Nostr relays. See the module docs.
pub struct NostrSignaling {
    key: SigningKey,
    pubkey: String,
    sub: String,
    topics: BTreeSet<Topic>,
    relays: Vec<Sender<Cmd>>,
    inbox: Receiver<Incoming>,
    /// Event ids passed on, as their first 16 bytes (a `u128`, not the 64-character hex twice: 32 bytes an id where
    /// two `String`s took ~190, so ~6 KB for the 128 on an ESP32 rather than ~25, NB-132). An id is checked to be the
    /// event's hash, so a copy that slips through takes ~2^64 tries at events of one's own, and can't target another's.
    seen: HashSet<u128>,
    seen_order: VecDeque<u128>,
    shared: Arc<Shared>,
    counters: RelayWatch,
    published: u64,
    limits: Limits,
}

impl Default for NostrSignaling {
    /// A carrier on the [`DEFAULT_RELAYS`].
    fn default() -> Self {
        NostrSignaling::new(DEFAULT_RELAYS.iter().copied())
    }
}

impl NostrSignaling {
    /// Connects to `relays` (`wss://host[:port][/path]` or `ws://…`) in the background,
    /// reconnecting with backoff when one drops.
    pub fn new<S: AsRef<str>>(relays: impl IntoIterator<Item = S>) -> NostrSignaling {
        NostrSignaling::with_limits(relays, LIMITS)
    }

    fn with_limits<S: AsRef<str>>(relays: impl IntoIterator<Item = S>, limits: Limits) -> NostrSignaling {
        let key = loop {
            let mut b = [0u8; 32];
            OsRng.fill_bytes(&mut b);
            if let Ok(k) = SigningKey::from_bytes(&b) {
                break k;
            }
        };
        let pubkey = hex(&key.verifying_key().to_bytes());
        let mut sub = [0u8; 8];
        OsRng.fill_bytes(&mut sub);
        let (tx, inbox) = sync_channel(limits.inbox);
        let shared = Arc::new(Shared::default());
        let mut counters = Vec::new();
        let relays = relays
            .into_iter()
            .map(|url| {
                let (cmd, rx) = channel();
                let url = url.as_ref().to_string();
                let mine = Arc::new(RelayCounters { url: url.clone(), ..RelayCounters::default() });
                counters.push(mine.clone());
                let (tx, shared) = (tx.clone(), shared.clone());
                let mut thread = std::thread::Builder::new().name("peerwire-nostr".into());
                if let Some(size) = RELAY_STACK {
                    thread = thread.stack_size(size);
                }
                thread.spawn(move || relay_loop(url, rx, tx, shared, mine, limits)).expect("spawn relay thread");
                cmd
            })
            .collect();
        NostrSignaling {
            key,
            pubkey,
            sub: hex(&sub),
            topics: BTreeSet::new(),
            relays,
            inbox,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            shared,
            counters: RelayWatch(counters.into()),
            published: 0,
            limits,
        }
    }

    /// Counters: open relays, events published, received and refused.
    #[allow(clippy::useless_conversion)] // from u32 where there are no 64-bit atomics
    pub fn stats(&self) -> NostrStats {
        NostrStats {
            relays_open: u64::from(self.shared.open.load(Ordering::Relaxed)),
            published: self.published,
            received: u64::from(self.shared.received.load(Ordering::Relaxed)),
            rejected: u64::from(self.shared.rejected.load(Ordering::Relaxed)),
        }
    }

    /// Each relay's counters (open, events sent, taken, refused and never answered), in the order given.
    pub fn relay_stats(&self) -> Vec<RelayStats> {
        self.counters.stats()
    }

    /// A handle on [`relay_stats`](NostrSignaling::relay_stats) that outlives handing the carrier to a node.
    pub fn relay_watch(&self) -> RelayWatch {
        self.counters.clone()
    }

    fn resubscribe(&mut self) {
        let req = (!self.topics.is_empty()).then(|| {
            let tags: Vec<String> = self.topics.iter().map(Topic::to_hex).collect();
            let since = unix_secs().saturating_sub(30);
            json!(["REQ", self.sub, {"kinds": [KIND], "#p": tags, "since": since}]).to_string()
        });
        for r in &self.relays {
            let _ = r.send(Cmd::Req(req.clone()));
        }
    }
}

impl Signaling for NostrSignaling {
    fn subscribe(&mut self, topic: Topic) {
        if self.topics.insert(topic) {
            self.resubscribe();
        }
    }

    fn unsubscribe(&mut self, topic: Topic) {
        if self.topics.remove(&topic) {
            self.resubscribe();
        }
    }

    fn publish(&mut self, topic: Topic, payload: &[u8]) {
        let event = sign_event(&self.key, &self.pubkey, unix_secs(), &topic.to_hex(), &b64(payload));
        let id = event["id"].as_str().and_then(|id| u128::from_str_radix(id.get(..32)?, 16).ok()).unwrap_or(0);
        let msg = json!(["EVENT", event]).to_string();
        for r in &self.relays {
            let _ = r.send(Cmd::Event(msg.clone(), id));
        }
        self.published += 1;
    }

    fn poll(&mut self) -> Option<(Topic, Vec<u8>)> {
        loop {
            let m = self.inbox.try_recv().ok()?;
            self.shared.inbox_bytes.fetch_sub(m.size(), Ordering::Relaxed);
            if m.pubkey == self.pubkey || self.seen.contains(&m.id) {
                continue; // our own, or a copy from another relay
            }
            // The oldest goes first, so the queue never holds more than `seen` (its buffer would double past it).
            if self.seen_order.len() >= self.limits.seen {
                if let Some(old) = self.seen_order.pop_front() {
                    self.seen.remove(&old);
                }
            }
            self.seen.insert(m.id);
            self.seen_order.push_back(m.id);
            let Some(topic) = Topic::from_hex(&m.tag).filter(|t| self.topics.contains(t)) else { continue };
            if let Some(payload) = unb64(&m.content) {
                return Some((topic, payload));
            }
        }
    }
}

impl Drop for NostrSignaling {
    fn drop(&mut self) {
        // Relay threads see their command channel close and exit.
        self.relays.clear();
    }
}

/// A NIP-01 event: id = sha256 of `[0, pubkey, created_at, kind, tags, content]`,
/// signed with BIP-340 Schnorr.
fn sign_event(key: &SigningKey, pubkey: &str, created_at: u64, tag: &str, content: &str) -> Value {
    let tags = json!([["p", tag]]);
    let id: [u8; 32] = Sha256::digest(json!([0, pubkey, created_at, KIND, tags, content]).to_string()).into();
    let mut aux = [0u8; 32];
    OsRng.fill_bytes(&mut aux);
    let sig = key.sign_raw(&id, &aux).expect("schnorr signs a 32-byte digest");
    json!({
        "id": hex(&id), "pubkey": pubkey, "created_at": created_at, "kind": KIND,
        "tags": tags, "content": content, "sig": hex(&sig.to_bytes()),
    })
}

fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A relay connection's bytes: TCP, or TLS over it. It owns the one socket, as ESP-IDF
/// can't duplicate one (`TcpStream::try_clone` fails there with ENOSYS).
trait Stream: Read + Write + Send {
    fn tcp(&self) -> &TcpStream;
}

impl Stream for TcpStream {
    fn tcp(&self) -> &TcpStream {
        self
    }
}

impl Stream for rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    fn tcp(&self) -> &TcpStream {
        &self.sock
    }
}

/// A relay connection that drops each data frame whose payload is over `max` bytes as it streams in, never holding
/// it, and the continuation frames of a message it dropped; control frames always pass. One oversize event that
/// someone sent to our tag then costs a read, not the connection, which before closed and kept a board off the relay
/// it carries its pipes over (NB-128, NB126-2). Until the handshake's HTTP response ends (its blank line), bytes pass
/// untouched, and a read stops there, so tungstenite takes no frame bytes with the response before they're judged:
/// those wait here. tungstenite still checks what passes against the same `max`, so a message fragmented into frames
/// under it, which only the relay itself can send, still ends the connection.
struct Oversize {
    inner: Box<dyn Stream>,
    max: u64,
    /// Frames from here on; until then, the last 4 bytes of the HTTP response.
    on: bool,
    tail: u32,
    /// Bytes read with the end of the HTTP response, which are frames, to judge first.
    early: Vec<u8>,
    part: Part,
    /// The frame header being read, and how much of it has come.
    head: [u8; 14],
    have: usize,
    /// A message is being dropped: its continuation frames go too.
    dropping: bool,
}

/// Where [`Oversize`] is in a frame.
#[derive(Clone, Copy)]
enum Part {
    /// Reading a header.
    Head,
    /// Passing a header on, this much of it so far, then a payload this long.
    Give(usize, u64),
    /// Passing a payload on, this much left.
    Pass(u64),
    /// Dropping a payload, this much left.
    Skip(u64),
}

impl Oversize {
    fn new(inner: Box<dyn Stream>, max: usize) -> Oversize {
        let part = Part::Head;
        let (head, early) = ([0; 14], Vec::new());
        Oversize { inner, max: max as u64, on: false, tail: 0, early, part, head, have: 0, dropping: false }
    }

    /// Frame bytes: what came with the HTTP response first, then the connection's.
    fn src(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.early.is_empty() {
            return self.inner.read(buf);
        }
        let n = buf.len().min(self.early.len());
        buf[..n].copy_from_slice(&self.early[..n]);
        self.early.drain(..n);
        Ok(n)
    }

    /// The length of the header in `head[..have]`: 2 bytes until those say more (RFC 6455 5.2).
    fn head_len(&self) -> usize {
        if self.have < 2 {
            return 2;
        }
        let ext = match self.head[1] & 0x7f {
            126 => 2,
            127 => 8,
            _ => 0,
        };
        2 + ext + if self.head[1] & 0x80 != 0 { 4 } else { 0 }
    }

    /// The whole header read: its payload's length, and whether the frame is dropped.
    fn judge(&mut self) -> (u64, bool) {
        let len = match self.head[1] & 0x7f {
            126 => u64::from(u16::from_be_bytes([self.head[2], self.head[3]])),
            127 => u64::from_be_bytes(self.head[2..10].try_into().expect("8 bytes")),
            n => u64::from(n),
        };
        let (fin, opcode) = (self.head[0] & 0x80 != 0, self.head[0] & 0x0f);
        let drop = match opcode {
            0 => self.dropping,
            1..=7 => len > self.max,
            _ => false, // control frames: ping, pong, close
        };
        match opcode {
            0 if self.dropping => self.dropping = !fin,
            1..=7 => self.dropping = drop && !fin,
            _ => {}
        }
        (len, drop)
    }
}

impl Read for Oversize {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if !self.on {
            let n = self.inner.read(buf)?;
            for i in 0..n {
                self.tail = self.tail << 8 | u32::from(buf[i]);
                if self.tail == u32::from_be_bytes(*b"\r\n\r\n") {
                    self.on = true;
                    self.early.extend_from_slice(&buf[i + 1..n]);
                    return Ok(i + 1);
                }
            }
            return Ok(n);
        }
        loop {
            match self.part {
                Part::Head => {
                    let need = self.head_len();
                    if self.have < need {
                        let mut head = self.head;
                        let n = self.src(&mut head[self.have..need])?;
                        self.head = head;
                        if n == 0 {
                            return Ok(0);
                        }
                        self.have += n;
                        continue;
                    }
                    let (len, drop) = self.judge();
                    self.part = if drop {
                        self.have = 0;
                        Part::Skip(len)
                    } else {
                        Part::Give(0, len)
                    };
                }
                Part::Give(given, len) => {
                    let n = (self.have - given).min(buf.len());
                    buf[..n].copy_from_slice(&self.head[given..given + n]);
                    self.part = if given + n < self.have {
                        Part::Give(given + n, len)
                    } else {
                        self.have = 0;
                        if len == 0 {
                            Part::Head
                        } else {
                            Part::Pass(len)
                        }
                    };
                    return Ok(n);
                }
                Part::Pass(left) => {
                    let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
                    let n = self.src(&mut buf[..want])?;
                    if n == 0 {
                        return Ok(0);
                    }
                    let left = left - n as u64;
                    self.part = if left == 0 { Part::Head } else { Part::Pass(left) };
                    return Ok(n);
                }
                Part::Skip(0) => self.part = Part::Head,
                Part::Skip(left) => {
                    let mut scratch = [0u8; 256];
                    let want = scratch.len().min(usize::try_from(left).unwrap_or(usize::MAX));
                    let n = self.src(&mut scratch[..want])?;
                    if n == 0 {
                        return Ok(0);
                    }
                    self.part = Part::Skip(left - n as u64);
                }
            }
        }
    }
}

impl Write for Oversize {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Stream for Oversize {
    fn tcp(&self) -> &TcpStream {
        self.inner.tcp()
    }
}

type Ws = tungstenite::WebSocket<Box<dyn Stream>>;

/// One relay: connect, keep the subscription, forward events both ways, reconnect.
fn relay_loop(
    url: String,
    cmds: Receiver<Cmd>,
    out: SyncSender<Incoming>,
    shared: Arc<Shared>,
    counters: Arc<RelayCounters>,
    limits: Limits,
) {
    let mut req: Option<String> = None;
    let mut queue: VecDeque<(String, u128)> = VecDeque::new();
    let mut backoff = Duration::from_secs(1);
    loop {
        if let Ok(mut ws) = connect(&url, limits.message) {
            shared.open.fetch_add(1, Ordering::Relaxed);
            counters.open.store(true, Ordering::Relaxed);
            let opened = Instant::now();
            let alive = session(&mut ws, &cmds, &out, &shared, &counters, limits, &mut req, &mut queue);
            counters.open.store(false, Ordering::Relaxed);
            shared.open.fetch_sub(1, Ordering::Relaxed);
            if !alive {
                let _ = ws.close(None);
                let _ = ws.flush();
                return;
            }
            if opened.elapsed() >= STABLE {
                backoff = Duration::from_secs(1);
            }
        }
        // Wait out the backoff, still taking commands (and leaving if the carrier is gone).
        let mut waited = Duration::ZERO;
        while waited < backoff {
            match cmds.recv_timeout(Duration::from_millis(100)) {
                Ok(Cmd::Req(r)) => req = r,
                Ok(Cmd::Event(e, id)) => enqueue(&mut queue, (e, id), limits.queued),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => waited += Duration::from_millis(100),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Queues an event for a relay, dropping the oldest past `max`.
fn enqueue(queue: &mut VecDeque<(String, u128)>, event: (String, u128), max: usize) {
    if queue.len() >= max {
        queue.pop_front();
    }
    queue.push_back(event);
}

/// Runs one connection until it drops (`true`) or the carrier is dropped (`false`).
#[allow(clippy::too_many_arguments)]
fn session(
    ws: &mut Ws,
    cmds: &Receiver<Cmd>,
    out: &SyncSender<Incoming>,
    shared: &Shared,
    counters: &RelayCounters,
    limits: Limits,
    req: &mut Option<String>,
    queue: &mut VecDeque<(String, u128)>,
) -> bool {
    let mut sent_req = false;
    // The events sent on this connection that await an `OK`, oldest first.
    let mut waiting: VecDeque<(u128, Instant)> = VecDeque::new();
    loop {
        while waiting.front().is_some_and(|(_, at)| at.elapsed() >= ANSWER_WAIT) {
            waiting.pop_front();
            counters.unanswered.fetch_add(1, Ordering::Relaxed);
        }
        let mut frames: Vec<(String, Option<u128>)> = Vec::new();
        loop {
            match cmds.try_recv() {
                Ok(Cmd::Req(r)) => {
                    if r.is_none() && sent_req {
                        frames.push((json!(["CLOSE", req_sub(req.as_deref())]).to_string(), None));
                    }
                    *req = r;
                    sent_req = false;
                }
                // Capped here too: a relay that stops reading holds the thread in a write
                // for up to CONNECT_TIMEOUT while publishes pile up (NB95-2).
                Ok(Cmd::Event(e, id)) => enqueue(queue, (e, id), limits.queued),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
        if !sent_req {
            if let Some(r) = req.as_ref() {
                frames.push((r.clone(), None));
            }
            sent_req = true;
        }
        frames.extend(queue.drain(..).map(|(e, id)| (e, Some(id))));
        for (f, id) in frames {
            if ws.send(Message::Text(f)).is_err() {
                return true;
            }
            if let Some(id) = id {
                counters.sent.fetch_add(1, Ordering::Relaxed);
                if waiting.len() >= limits.waiting {
                    waiting.pop_front();
                    counters.unanswered.fetch_add(1, Ordering::Relaxed);
                }
                waiting.push_back((id, Instant::now()));
            }
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                if let Some((id, ok)) = ok_reply(&t) {
                    if let Some(i) = waiting.iter().position(|(w, _)| *w == id) {
                        waiting.remove(i);
                    }
                    (if ok { &counters.accepted } else { &counters.rejected }).fetch_add(1, Ordering::Relaxed);
                }
                on_relay_message(&t, out, shared, limits.inbox_bytes);
            }
            Ok(Message::Close(_)) => return true,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if ws.flush().is_err() {
                    return true;
                }
            }
            Err(_) => return true,
        }
    }
}

/// An `OK` reply's event, as its id's first 16 bytes, and whether the relay took it.
fn ok_reply(text: &str) -> Option<(u128, bool)> {
    // An `OK` is a short array; anything else is left to `on_relay_message`.
    if text.len() > 1024 || !text.trim_start().strip_prefix('[')?.trim_start().starts_with("\"OK\"") {
        return None;
    }
    let Ok(Value::Array(m)) = serde_json::from_str::<Value>(text) else { return None };
    let id = u128::from_str_radix(m.get(1)?.as_str()?.get(..32)?, 16).ok()?;
    Some((id, m.get(2)?.as_bool()?))
}

fn req_sub(req: Option<&str>) -> String {
    req.and_then(|r| serde_json::from_str::<Value>(r).ok())
        .and_then(|v| v.get(1).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

fn on_relay_message(text: &str, out: &SyncSender<Incoming>, shared: &Shared, max_bytes: usize) {
    let (mut arrays, mut objects, mut commas) = (0, 0, 0);
    for b in text.bytes() {
        match b {
            b'[' => arrays += 1,
            b'{' => objects += 1,
            b',' => commas += 1,
            _ => {}
        }
    }
    if arrays + objects + commas > MAX_SHAPE || objects > MAX_OBJECTS || arrays > MAX_ARRAYS {
        return;
    }
    let Ok(Value::Array(m)) = serde_json::from_str::<Value>(text) else { return };
    match m.first().and_then(Value::as_str) {
        Some("EVENT") => {
            let Some(ev) = m.get(2) else { return };
            if ev.get("kind").and_then(Value::as_u64) != Some(KIND) {
                return;
            }
            let field = |k: &str| ev.get(k).and_then(Value::as_str).map(str::to_string);
            let tags = ev.get("tags").and_then(Value::as_array);
            let tag = tags.and_then(|tags| {
                tags.iter().find_map(|t| match t.as_array()?.as_slice() {
                    [k, v, ..] if k.as_str() == Some("p") => v.as_str().map(str::to_string),
                    _ => None,
                })
            });
            let created_at = ev.get("created_at").and_then(Value::as_u64);
            let (Some(id), Some(pubkey), Some(tag), Some(content), Some(created_at)) =
                (field("id"), field("pubkey"), tag, field("content"), created_at)
            else {
                return;
            };
            // The id must be the NIP-01 hash of the event, so a relay can't dress one
            // event up as many (copies are dropped by id). Signatures aren't checked:
            // offers are sealed and authenticate themselves.
            let ser = json!([0, pubkey, created_at, KIND, tags, content]).to_string();
            let digest: [u8; 32] = Sha256::digest(ser).into();
            if hex(&digest) != id {
                return;
            }
            let id = u128::from_be_bytes(digest[..16].try_into().expect("16 of 32 bytes"));
            shared.received.fetch_add(1, Ordering::Relaxed);
            // A full inbox drops the event, as a relay may.
            let m = Incoming { id, pubkey, tag, content };
            let size = m.size();
            if shared.inbox_bytes.fetch_add(size, Ordering::Relaxed).saturating_add(size) > max_bytes
                || out.try_send(m).is_err()
            {
                shared.inbox_bytes.fetch_sub(size, Ordering::Relaxed);
            }
        }
        Some("OK") if m.get(2) == Some(&Value::Bool(false)) => {
            shared.rejected.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
}

fn connect(url: &str, max_message: usize) -> io::Result<Ws> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err(io::Error::new(ErrorKind::InvalidInput, "relay URL must start with ws:// or wss://"));
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(']') || h.ends_with(']') => {
            (h, p.parse().map_err(|_| io::Error::new(ErrorKind::InvalidInput, "bad port"))?)
        }
        _ => (authority, if tls { 443 } else { 80 }),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<_> = (host, port).to_socket_addrs()?.collect();
    let mut last = io::Error::new(ErrorKind::NotFound, "relay host has no address");
    let mut tcp = None;
    for a in addrs {
        match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let tcp = tcp.ok_or(last)?;
    tcp.set_nodelay(true)?;
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    // Kept for the connection: a relay that stops reading would otherwise hold the
    // thread, and every later publish, for good (NB95-2). The stream owns the socket, so
    // it covers TLS too.
    tcp.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    let stream: Box<dyn Stream> = if tls {
        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "bad relay host name"))?;
        let conn = rustls::ClientConnection::new(tls_config(), name).map_err(io::Error::other)?;
        Box::new(rustls::StreamOwned::new(conn, tcp))
    } else {
        Box::new(tcp)
    };
    let stream: Box<dyn Stream> = Box::new(Oversize::new(stream, max_message));
    let config = WebSocketConfig {
        max_message_size: Some(max_message),
        max_frame_size: Some(max_message),
        ..Default::default()
    };
    let (ws, _) = tungstenite::client::client_with_config(url, stream, Some(config))
        .map_err(|e| io::Error::other(e.to_string()))?;
    ws.get_ref().tcp().set_read_timeout(Some(READ_SLICE))?;
    Ok(ws)
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a relay thread does with a relay message, with no cap on the inbox's bytes.
    fn on(text: &str, out: &SyncSender<Incoming>, shared: &Shared) {
        on_relay_message(text, out, shared, usize::MAX);
    }

    /// `["EVENT", "s", event]` with a valid id and `extra` tags beside the `p` one; the
    /// signature isn't checked, so none is made.
    fn relay_event(created_at: u64, tag: &str, content: &str, extra: usize) -> String {
        let pubkey = "ab".repeat(32);
        let mut tags = vec![json!(["p", tag])];
        tags.extend((0..extra).map(|_| json!([""])));
        let id = hex(&Sha256::digest(json!([0, pubkey, created_at, KIND, tags, content]).to_string()));
        let ev = json!({"id": id, "pubkey": pubkey, "created_at": created_at, "kind": KIND, "tags": tags,
                        "content": content, "sig": "00"});
        json!(["EVENT", "s", ev]).to_string()
    }

    /// NB-126: the biggest pipe batches a node sends, sealed and signed as an event, fit what an ESP32's carrier
    /// takes from a relay (`Limits::SMALL.message`, 4 KiB, or it closes the socket): 64 small datagrams making
    /// 1,232 B, and one datagram of 1,232 B. Measured as the board reads it, with the relay's subscription id.
    #[test]
    fn the_biggest_pipe_batch_fits_an_esp32s_relay_message() {
        use crate::pipe::{decode, Pipes, BATCH, FLUSH, MAX_ITEMS};
        use peerwire_core::{sealed, Identity, Instant as CoreInstant};
        let (me, to) = (Identity::from_secret_bytes(&[1; 32]), Identity::from_secret_bytes(&[2; 32]).peer_id());
        let key = SigningKey::from_bytes(&[3; 32]).unwrap();
        let pubkey = hex(&key.verifying_key().to_bytes());
        let small = BATCH / MAX_ITEMS;
        let mut many = vec![small; MAX_ITEMS - 1];
        many.push(BATCH - small * (MAX_ITEMS - 1));
        for sizes in [many, vec![BATCH]] {
            let mut pipes = Pipes::default();
            let at = pipes.open(CoreInstant::ZERO, to, |_| false).unwrap();
            for (i, n) in sizes.iter().enumerate() {
                pipes.send(CoreInstant::ZERO, &at, vec![i as u8; *n]);
            }
            let (_, body) = pipes.take(CoreInstant::ZERO + FLUSH).unwrap();
            assert_eq!(decode(body.as_bytes()).map(|d| d.len()), Some(sizes.len()), "all in one batch");
            let sealed = sealed::seal(&me, &to, [4; 32], body.as_bytes()).unwrap();
            let ev = sign_event(&key, &pubkey, 1_800_000_000, &Topic::peer(&to).to_hex(), &b64(&sealed));
            let frame = json!(["EVENT", "0123456789abcdef", ev]).to_string();
            println!("{} datagrams, {} B: batch {} B, event as read {} B", sizes.len(), BATCH, body.len(), frame.len());
            assert!(frame.len() <= Limits::SMALL.message, "{} B", frame.len());
        }
    }

    #[test]
    fn base64_matches_btoa() {
        for (raw, enc) in [(&b""[..], ""), (b"f", "Zg=="), (b"fo", "Zm8="), (b"foo", "Zm9v"), (b"foob", "Zm9vYg==")] {
            assert_eq!(b64(raw), enc);
            assert_eq!(unb64(enc).as_deref(), Some(raw));
        }
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(unb64(&b64(&all)), Some(all));
        for bad in ["Zg=", "Z===", "Zg==Zg==", "Zm9v!A==", "===="] {
            assert_eq!(unb64(bad), None, "{bad}");
        }
    }

    #[test]
    fn events_are_nip01() {
        let key = SigningKey::from_bytes(&[7u8; 32]).unwrap();
        let pubkey = hex(&key.verifying_key().to_bytes());
        let ev = sign_event(&key, &pubkey, 1_700_000_000, &"ab".repeat(32), "aGk=");
        let ser = json!([0, pubkey, 1_700_000_000u64, KIND, [["p", "ab".repeat(32)]], "aGk="]).to_string();
        let id: [u8; 32] = Sha256::digest(ser).into();
        assert_eq!(ev["id"], hex(&id));
        let mut sig = [0u8; 64];
        crate::signaling::unhex(ev["sig"].as_str().unwrap(), &mut sig);
        let vk = k256::schnorr::VerifyingKey::from_bytes(&key.verifying_key().to_bytes()).unwrap();
        let sig = k256::schnorr::Signature::try_from(&sig[..]).unwrap();
        assert!(vk.verify_raw(&id, &sig).is_ok());
        assert_eq!(ev["kind"], KIND);
    }

    #[test]
    fn relay_messages_are_parsed_defensively() {
        let (tx, rx) = sync_channel(4);
        let shared = Shared::default();
        for junk in [
            "",
            "{}",
            "[]",
            r#"["EVENT"]"#,
            r#"["EVENT","s",{"kind":1}]"#,
            r#"["EVENT","s",{"kind":25059,"tags":[["e","x"]]}]"#,
        ] {
            on(junk, &tx, &shared);
        }
        // An id that isn't the event's hash: dropped.
        on(
            r#"["EVENT","s",{"id":"i","pubkey":"p","created_at":1,"kind":25059,"tags":[["p","t"]],"content":"c"}]"#,
            &tx,
            &shared,
        );
        assert!(rx.try_recv().is_err());
        let key = SigningKey::from_bytes(&[7u8; 32]).unwrap();
        let pubkey = hex(&key.verifying_key().to_bytes());
        let ev = sign_event(&key, &pubkey, 1_700_000_000, "t", "c");
        on(&json!(["EVENT", "s", ev]).to_string(), &tx, &shared);
        let m = rx.try_recv().unwrap();
        let prefix = u128::from_str_radix(&ev["id"].as_str().unwrap()[..32], 16).unwrap();
        assert_eq!((m.id, m.tag.as_str(), m.content.as_str()), (prefix, "t", "c"));
        // The same id with other content (a relay replaying it as new): dropped.
        let mut forged = ev.clone();
        forged["content"] = json!("d");
        on(&json!(["EVENT", "s", forged]).to_string(), &tx, &shared);
        assert!(rx.try_recv().is_err());
        // A full inbox drops events rather than growing or blocking the relay thread.
        for i in 0..10 {
            let ev = sign_event(&key, &pubkey, 1_700_000_000 + i, "t", "c");
            on(&json!(["EVENT", "s", ev]).to_string(), &tx, &shared);
        }
        assert_eq!(rx.try_iter().count(), 4);
        on(r#"["OK","i",false,"blocked"]"#, &tx, &shared);
        assert_eq!(shared.rejected.load(Ordering::Relaxed), 1);
    }

    /// NB95-1: a message of many small values is dropped before it is parsed, and the
    /// inbox holds no more than its bytes.
    #[test]
    fn relay_messages_are_bounded_before_they_are_parsed_and_queued() {
        let (tx, rx) = sync_channel(64);
        let shared = Shared::default();
        // 3 + 5 = 8 arrays: passed on. 9, or 13 + 2 × 60 values: dropped, though the id is right.
        on(&relay_event(1, "t", "QQ==", 5), &tx, &shared);
        assert_eq!(rx.try_iter().count(), 1);
        on(&relay_event(2, "t", "QQ==", 6), &tx, &shared);
        on(&relay_event(3, "t", "QQ==", 60), &tx, &shared);
        assert_eq!(rx.try_iter().count(), 0);
        // An ESP32's inbox: 16 KiB, so 5 events of 3,081 B (the id counted as its 16 bytes kept) and not the 6th.
        let shared = Shared::default();
        let content = "A".repeat(3000);
        for i in 0..10 {
            on_relay_message(&relay_event(i, "t", &content, 0), &tx, &shared, Limits::SMALL.inbox_bytes);
        }
        assert_eq!(shared.inbox_bytes.load(Ordering::Relaxed), 5 * 3081);
        assert_eq!(rx.try_iter().count(), 5);
    }

    /// NB95-1 and NB-128, with an ESP32's limits: an event of 3.9 KB is taken; a 16 KiB frame, as an event someone
    /// sent to our tag would be, is dropped as it streams in, and the connection stays, so the next event comes over it.
    /// Before NB-128 the frame closed the connection, and a relay sending one each time kept the board off it.
    #[test]
    fn a_board_drops_large_relay_frames_and_keeps_the_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let topic = Topic([5; 32]);
        let small = relay_event(1, &topic.to_hex(), &"A".repeat(3600), 0);
        let (large, later) = ("x".repeat(16 << 10), relay_event(2, &topic.to_hex(), &"A".repeat(3600), 0));
        assert!(small.len() < Limits::SMALL.message);
        let end = Instant::now() + Duration::from_millis(3500);
        let server = std::thread::spawn(move || {
            let mut handshakes = 0;
            while Instant::now() < end {
                let Ok((s, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                s.set_nonblocking(false).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let mut ws = tungstenite::accept(s).unwrap();
                handshakes += 1;
                let _ = ws.read(); // the REQ
                let _ = ws.send(Message::Text(small.clone()));
                let _ = ws.send(Message::Text(large.clone()));
                let _ = ws.send(Message::Text(later.clone()));
                // Until the client goes, or the test ends: an idle second is no reason to hang up.
                loop {
                    match ws.read() {
                        Ok(_) => {}
                        Err(tungstenite::Error::Io(e))
                            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
                                && Instant::now() < end => {}
                        Err(_) => break,
                    }
                }
            }
            handshakes
        });
        let mut carrier = NostrSignaling::with_limits([&url], Limits::SMALL);
        carrier.subscribe(topic);
        let mut got = Vec::new();
        while Instant::now() < end {
            got.extend(carrier.poll().map(|(_, p)| p.len()));
            std::thread::sleep(Duration::from_millis(5));
        }
        let handshakes = server.join().unwrap();
        assert_eq!(got, [2700, 2700]);
        assert_eq!(handshakes, 1, "one connection, kept");
    }

    /// NB-032(d) and NB-128: right after the handshake, a local relay sends a 200 KiB message, a message whose first
    /// frame is over the cap (and its continuation), a ping, and a small message in two frames. The client drops the
    /// oversize ones as they stream in, holding neither, and reads the small one whole on the same connection.
    #[test]
    fn oversized_relay_messages_are_dropped_and_the_connection_kept() {
        use tungstenite::protocol::frame::coding::{Data, OpCode};
        use tungstenite::protocol::frame::Frame;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(s).unwrap();
            let first = |b: Vec<u8>, fin| Message::Frame(Frame::message(b, OpCode::Data(Data::Text), fin));
            let more = |b: Vec<u8>, fin| Message::Frame(Frame::message(b, OpCode::Data(Data::Continue), fin));
            ws.send(Message::Text("x".repeat(200 << 10))).unwrap();
            ws.send(first(vec![b'y'; 5 << 10], false)).unwrap();
            ws.send(more(vec![b'y'; 10], true)).unwrap();
            ws.send(Message::Ping(vec![1, 2])).unwrap();
            ws.send(first(b"small ".to_vec(), false)).unwrap();
            ws.send(more(b"one".to_vec(), true)).unwrap();
            while ws.read().is_ok() {} // the pong, then until the client goes
        });
        let mut ws = connect(&url, Limits::SMALL.message).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let got = loop {
            match ws.read() {
                Ok(Message::Text(t)) => break t,
                Ok(_) => {} // the ping
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) && Instant::now() < until => {}
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(got, "small one");
        drop(ws);
        server.join().unwrap();
    }

    /// NB-128: random relay byte streams through `Oversize`, read in random pieces with timeouts between. What comes
    /// out is the HTTP response, with no read going past its end, then exactly the frames that should pass, byte for
    /// byte: data frames over the cap go with their continuations, and nothing else does (control frames between
    /// fragments, any length encoding, masked or not, continuations of a message passed whatever their size).
    #[test]
    fn fuzz_oversize_frames() {
        use peerwire_sim::fuzz::{cases, Rng};
        struct Mock {
            data: Vec<u8>,
            at: usize,
            rng: Rng,
            tcp: TcpStream,
        }
        impl Read for Mock {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.rng.one_in(4) {
                    return Err(io::Error::from(ErrorKind::WouldBlock));
                }
                let n = buf.len().min(self.data.len() - self.at).min(1 + self.rng.below(300));
                buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
                self.at += n;
                Ok(n)
            }
        }
        impl Write for Mock {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl Stream for Mock {
            fn tcp(&self) -> &TcpStream {
                &self.tcp
            }
        }
        // A frame: its length in the shortest form or a longer one, masked one time in 4, then the payload.
        fn frame(rng: &mut Rng, fin: bool, opcode: u8, len: usize) -> Vec<u8> {
            let (mask, mut f) = (rng.one_in(4), vec![u8::from(fin) << 7 | opcode]);
            let m = if mask { 0x80 } else { 0 };
            match (len, rng.below(3)) {
                (0..=125, 0) => f.push(m | len as u8),
                (0..=65535, 0 | 1) => {
                    f.push(m | 126);
                    f.extend_from_slice(&(len as u16).to_be_bytes());
                }
                _ => {
                    f.push(m | 127);
                    f.extend_from_slice(&(len as u64).to_be_bytes());
                }
            }
            if mask {
                f.extend_from_slice(&rng.bytes(4));
            }
            f.extend_from_slice(&rng.bytes(len));
            f
        }
        fn control(rng: &mut Rng) -> Vec<u8> {
            let (op, len) = (*rng.pick(&[8u8, 9, 10]), rng.below(126));
            frame(rng, true, op, len)
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let http = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
        let mut rng = Rng::for_target("nostr_oversize");
        for case in 0..cases(3_000) {
            let max = 1 + rng.below(600);
            let (mut input, mut want) = (http.to_vec(), http.to_vec());
            for _ in 0..rng.below(12) {
                if rng.one_in(5) {
                    let c = control(&mut rng);
                    input.extend_from_slice(&c);
                    want.extend_from_slice(&c);
                    continue;
                }
                let pieces = if rng.one_in(3) { 2 + rng.below(3) } else { 1 };
                let op = *rng.pick(&[1u8, 2]);
                let first = if rng.one_in(3) { max + 1 + rng.below(800) } else { rng.below(max + 1) };
                for k in 0..pieces {
                    let len = if k == 0 { first } else { rng.below(2 * max) };
                    let f = frame(&mut rng, k + 1 == pieces, if k == 0 { op } else { 0 }, len);
                    input.extend_from_slice(&f);
                    if first <= max {
                        want.extend_from_slice(&f);
                    }
                    if k + 1 < pieces && rng.one_in(3) {
                        let c = control(&mut rng);
                        input.extend_from_slice(&c);
                        want.extend_from_slice(&c);
                    }
                }
            }
            let mock = Mock {
                data: input,
                at: 0,
                rng: Rng::for_target(&format!("read{case}")),
                tcp: tcp.try_clone().unwrap(),
            };
            let mut o = Oversize::new(Box::new(mock), max);
            let (mut out, mut buf, mut was_on) = (Vec::new(), [0u8; 512], false);
            loop {
                let k = 1 + rng.below(buf.len());
                match o.read(&mut buf[..k]) {
                    Ok(0) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => panic!("case {case}: {e}"),
                }
                if o.on && !was_on {
                    assert_eq!(out.len(), http.len(), "case {case}: a read went past the HTTP response");
                    was_on = true;
                }
            }
            assert!(out == want, "case {case}: max {max}, {} bytes out, {} wanted", out.len(), want.len());
        }
    }

    /// NB-131: each relay's counters. A local relay takes the first event, refuses the second, and never answers the
    /// third, as nos.lol and nostr.mom did every event (NB-130): one each of accepted, rejected and unanswered.
    #[test]
    fn relay_stats_count_each_relays_answers_and_silences() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut ws = tungstenite::accept(s).unwrap();
            let mut n = 0;
            while let Ok(m) = ws.read() {
                let Ok(Value::Array(m)) = serde_json::from_str::<Value>(m.to_text().unwrap_or("")) else { continue };
                if m[0] != "EVENT" {
                    continue;
                }
                n += 1;
                let id = m[1]["id"].clone();
                match n {
                    1 => ws.send(Message::Text(json!(["OK", id, true, ""]).to_string())).unwrap(),
                    2 => ws.send(Message::Text(json!(["OK", id, false, "blocked: no"]).to_string())).unwrap(),
                    _ => {} // the third: never answered
                }
            }
        });
        let mut carrier = NostrSignaling::new([&url]);
        let watch = carrier.relay_watch();
        let until = Instant::now() + Duration::from_secs(5);
        while !carrier.relay_stats()[0].open && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        for i in 0..3u8 {
            carrier.publish(Topic([i; 32]), b"x");
            std::thread::sleep(Duration::from_millis(50));
        }
        let want = RelayStats { url: url.clone(), open: true, sent: 3, accepted: 1, rejected: 1, unanswered: 1 };
        while carrier.relay_stats()[0] != want && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(carrier.relay_stats(), std::slice::from_ref(&want));
        assert_eq!(carrier.stats().rejected, 1);
        drop(carrier);
        // The handle reads on after the carrier is gone, and sees the connection closed.
        while watch.stats()[0].open && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(watch.stats(), [RelayStats { open: false, ..want }]);
        server.join().unwrap();
    }

    /// NB-014: relay messages (real events, mutated, and JSON at the nesting
    /// limit) never panic; whatever is passed on has a topic and payload that
    /// decode and round-trip.
    #[test]
    fn fuzz_relay_messages() {
        use peerwire_sim::fuzz::{cases, mutate, run};
        let pubkey = "ab".repeat(32);
        // Events with a valid id; the signature isn't checked here, so none is made.
        let event = |created_at: u64, tag: &str, content: &str| {
            let tags = json!([["p", tag]]);
            let id = hex(&Sha256::digest(json!([0, pubkey, created_at, KIND, tags, content]).to_string()));
            json!({"id": id, "pubkey": pubkey, "created_at": created_at, "kind": KIND, "tags": tags,
                   "content": content, "sig": "00"})
        };
        let (tx, rx) = sync_channel(8);
        let shared = Shared::default();
        let topic = Topic([3; 32]).to_hex();
        let (mut passed, mut decoded, mut noncanonical) = (0usize, 0usize, 0usize);
        for deep in ["[".repeat(100_000), format!("[\"EVENT\",\"s\",{}", "{\"a\":".repeat(50_000))] {
            on(&deep, &tx, &shared);
        }
        run(
            "relay",
            cases(6_000),
            |rng| {
                let n = rng.below(120);
                let content = match rng.one_in(4) {
                    true => String::from_utf8_lossy(&rng.bytes(n)).into_owned(),
                    false => b64(&rng.bytes(n)),
                };
                let tag = match rng.one_in(4) {
                    true => {
                        let n = rng.below(40);
                        hex(&rng.bytes(n))
                    }
                    false => topic.clone(),
                };
                let ev = event(rng.u64() >> rng.below(64), &tag, &content);
                let mut b = match rng.below(6) {
                    0 => json!(["OK", ev["id"], rng.one_in(2), "x"]).to_string(),
                    _ => json!(["EVENT", "s", ev]).to_string(),
                }
                .into_bytes();
                if rng.below(3) != 0 {
                    mutate(rng, &mut b, 4096);
                }
                b
            },
            |b| {
                on(&String::from_utf8_lossy(b), &tx, &shared);
                for m in rx.try_iter() {
                    passed += 1;
                    if let Some(t) = Topic::from_hex(&m.tag) {
                        assert_eq!(t.to_hex(), m.tag.to_ascii_lowercase());
                    }
                    if let Some(p) = unb64(&m.content) {
                        decoded += 1;
                        noncanonical += (b64(&p) != m.content) as usize;
                        assert_eq!(unb64(&b64(&p)), Some(p));
                    }
                }
            },
        );
        println!("fuzz relay: {passed} events passed on, {decoded} payloads decoded, {noncanonical} not canonical");
        assert!(passed > 0 && decoded > 0);
    }
}
