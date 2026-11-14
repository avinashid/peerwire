//! NB-132: a board's heap under page reloads. Every load of the LED page is a new PeerId that says a room hello, links
//! over the relay (a pipe, `PathKind::Signaling`) and leaves, closing or timing out. Whatever the node keeps per peer
//! must be capped or freed, or anyone who knows the room runs a board's ~100 KB of heap down by reloading.
//!
//! A counting allocator measures the live heap of this process, the node's and the pages'; each page is dropped
//! before a reading, and the test's own hub frees a carrier when it is dropped, so what stays is the node's.

use peerwire::traversal::Traversal;
use peerwire::{Event, Lane, Node, PathKind, PeerId, Role, Signaling, Status, Topic};
use peerwire_core::{sealed, Config, Endpoint, Identity, Instant as CoreInstant};
use rand_core::{OsRng, RngCore};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Relaxed);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, size) };
        if !q.is_null() {
            LIVE.fetch_add(size, Relaxed);
            LIVE.fetch_sub(l.size(), Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const APP: &str = "pw-test";
const ROOM: &str = "heap-churn";
const PIPE: u32 = 1;
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// A carrier's topics and what waits for it.
type Member = (HashSet<Topic>, VecDeque<(Topic, Vec<u8>)>);

/// A hub like `MemoryHub` that forgets a carrier when it is dropped, so pages that came and went cost it nothing:
/// the last carrier's number, and the carriers by number.
#[derive(Clone, Default)]
struct Hub(Arc<Mutex<(usize, HashMap<usize, Member>)>>);

struct Carrier {
    hub: Hub,
    me: usize,
}

impl Hub {
    fn connect(&self) -> Carrier {
        let mut h = self.0.lock().unwrap();
        h.0 += 1;
        let me = h.0;
        h.1.insert(me, Default::default());
        Carrier { hub: self.clone(), me }
    }
}

impl Signaling for Carrier {
    fn subscribe(&mut self, topic: Topic) {
        self.hub.0.lock().unwrap().1.get_mut(&self.me).unwrap().0.insert(topic);
    }
    fn unsubscribe(&mut self, topic: Topic) {
        self.hub.0.lock().unwrap().1.get_mut(&self.me).unwrap().0.remove(&topic);
    }
    fn publish(&mut self, topic: Topic, payload: &[u8]) {
        let mut h = self.hub.0.lock().unwrap();
        for (i, (topics, inbox)) in h.1.iter_mut() {
            if *i != self.me && topics.contains(&topic) {
                inbox.push_back((topic, payload.to_vec()));
            }
        }
    }
    fn poll(&mut self) -> Option<(Topic, Vec<u8>)> {
        self.hub.0.lock().unwrap().1.get_mut(&self.me).unwrap().1.pop_front()
    }
}

impl Drop for Carrier {
    fn drop(&mut self) {
        if let Ok(mut h) = self.hub.0.lock() {
            h.1.remove(&self.me);
        }
    }
}

fn b64(data: &[u8]) -> String {
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = u32::from(c[0]) << 16 | u32::from(*c.get(1).unwrap_or(&0)) << 8 | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() { B64[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn unb64(s: &str) -> Vec<u8> {
    let val = |c: u8| B64.iter().position(|&b| b == c).unwrap() as u32;
    let mut out = Vec::new();
    for q in s.as_bytes().chunks(4) {
        let pad = q.iter().filter(|&&c| c == b'=').count();
        let n = q[..4 - pad].iter().fold(0, |n, &c| n << 6 | val(c)) << (6 * pad);
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8][..3 - pad]);
    }
    out
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// The LED page over Nostr, as in `tests/pipe.rs`: a bare endpoint, a room hello as a client, one pipe to the node.
struct Page {
    me: Identity,
    ep: Endpoint<u32>,
    sig: Carrier,
    trav: Traversal<u32>,
    epoch: Instant,
    node: PeerId,
    got: Vec<Vec<u8>>,
}

impl Page {
    fn new(hub: &Hub, node: PeerId) -> Page {
        let me = Identity::generate(&mut OsRng);
        let mut sig = hub.connect();
        sig.subscribe(Topic::peer(&me.peer_id()));
        sig.subscribe(Topic::room(APP, ROOM));
        let mut trav = Traversal::new(&me, [9; 32], unix_ms(), vec![], vec![]);
        trav.join_room(CoreInstant::ZERO, APP, ROOM, Role::Client, None);
        while trav.poll_signal().is_some() {}
        let ep = Endpoint::new(&me, Config::default(), [4; 32]);
        Page { me, ep, sig, trav, epoch: Instant::now(), node, got: vec![] }
    }

    fn now(&self) -> CoreInstant {
        CoreInstant::from_micros(self.epoch.elapsed().as_micros() as u64)
    }

    fn hello(&mut self) {
        let h = self.trav.room_hello(self.now()).unwrap();
        self.sig.publish(Topic::room(APP, ROOM), &h);
    }

    fn send_batch(&mut self, datagrams: &[Vec<u8>]) {
        if datagrams.is_empty() {
            return;
        }
        let d: Vec<String> = datagrams.iter().map(|d| format!("\"{}\"", b64(d))).collect();
        let body = format!(r#"{{"kind":"dg","d":[{}]}}"#, d.join(","));
        let mut seed = [0; 32];
        OsRng.fill_bytes(&mut seed);
        let sealed = sealed::seal(&self.me, &self.node, seed, body.as_bytes()).unwrap();
        self.sig.publish(Topic::peer(&self.node), &sealed);
    }

    fn step(&mut self) {
        let now = self.now();
        while let Some((topic, payload)) = self.sig.poll() {
            if topic == Topic::room(APP, ROOM) {
                continue;
            }
            let Some((from, body)) = sealed::open(&self.me, &payload) else { continue };
            if from != self.node || body.first() != Some(&b'{') {
                continue;
            }
            let s = std::str::from_utf8(&body).unwrap();
            let list = s.strip_prefix(r#"{"kind":"dg","d":["#).unwrap().strip_suffix("]}").unwrap();
            for d in list.split(',').map(|item| unb64(item.trim_matches('"'))) {
                self.ep.handle_datagram(now, PIPE, &d);
            }
        }
        if self.ep.poll_timeout().is_some_and(|t| t <= now) {
            self.ep.handle_timeout(now);
        }
        let d: Vec<Vec<u8>> = std::iter::from_fn(|| self.ep.poll_transmit(now)).map(|t| t.payload).collect();
        self.send_batch(&d);
        while let Some(e) = self.ep.poll_event() {
            if let peerwire_core::Event::Message { data, .. } = e {
                self.got.push(data);
            }
        }
    }
}

/// The board's node: `peerwire_esp::config()`'s caps, a host in the room, with a short idle timeout so a page that
/// vanishes is timed out in seconds rather than 30.
fn board(hub: &Hub) -> Node {
    let config = Config {
        max_message: 16 * 1024,
        max_send_buffer: 32 * 1024,
        max_links: 8,
        max_pending_handshakes: 8,
        keepalive: Duration::from_millis(500),
        idle_timeout: Duration::from_secs(2),
        ..Config::default()
    };
    let mut n = Node::builder()
        .config(config)
        .stun_servers(Vec::<String>::new())
        .signaling(hub.connect())
        .bind("127.0.0.1:0")
        .unwrap();
    n.join_room_as(APP, ROOM, Role::Host);
    n
}

/// Runs the node, and the page if there is one, until `done` or `secs` pass; the node's events go to `ev`.
fn pump(
    n: &mut Node,
    mut page: Option<&mut Page>,
    ev: &mut Vec<Event>,
    secs: f64,
    done: impl Fn(&Node, Option<&Page>, &[Event]) -> bool,
) -> bool {
    let until = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < until {
        n.poll();
        if let Some(p) = page.as_deref_mut() {
            p.step();
        }
        ev.extend(std::iter::from_fn(|| n.try_recv_event()));
        if done(n, page.as_deref(), ev) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

/// One page load: hello, a pipe link, `led on` and the answer, then a close (or nothing, `vanish`) and the node's
/// `Disconnected`.
fn visit(n: &mut Node, hub: &Hub, ev: &mut Vec<Event>, vanish: bool) {
    let nid = n.peer_id();
    let mut page = Page::new(hub, nid);
    let pid = page.me.peer_id();
    page.hello();
    pump(n, Some(&mut page), ev, 0.2, |_, _, _| false);
    let now = page.now();
    page.ep.connect(now, PIPE, nid).unwrap();
    assert!(pump(n, Some(&mut page), ev, 10.0, |n, _, _| n.status(&pid) == Status::Connected), "linked");
    assert_eq!(n.path_kind(&pid), Some(PathKind::Signaling));
    page.ep.send(&nid, Lane::Reliable, b"led on").unwrap();
    let got = |ev: &[Event]| ev.iter().any(|e| matches!(e, Event::Message { peer, .. } if *peer == pid));
    assert!(pump(n, Some(&mut page), ev, 10.0, |_, _, ev| got(ev)), "the node got the command");
    n.send(&pid, Lane::Reliable, b"LED is on").unwrap();
    let answered = |p: Option<&Page>| p.is_some_and(|p| p.got.iter().any(|d| d == b"LED is on"));
    assert!(pump(n, Some(&mut page), ev, 10.0, |_, p, _| answered(p)), "the page got the answer");
    if !vanish {
        page.ep.disconnect(&nid);
        page.step();
    }
    drop(page);
    let left = |ev: &[Event]| ev.iter().any(|e| matches!(e, Event::Disconnected { peer, .. } if *peer == pid));
    assert!(pump(n, None, ev, 10.0, |_, _, ev| left(ev)), "the node saw the page leave");
    pump(n, None, ev, 0.3, |_, _, _| false);
    ev.clear();
}

#[test]
#[ignore = "NB-132 measurement, about four minutes: cargo test -p peerwire --test heap_churn -- --ignored --nocapture"]
fn page_reloads_leave_the_board_heap_flat() {
    let hub = Hub::default();
    let mut n = board(&hub);
    let mut ev = vec![];
    pump(&mut n, None, &mut ev, 0.5, |_, _, _| false);
    let base = LIVE.load(Relaxed);
    eprintln!("joined: {base} B live");
    let mut after = vec![];
    for i in 0..80 {
        // One page load each 2.1 s at most: the node takes a new peer's hello (and so its pipe) only within its hello
        // budget, a burst of 30 then one each 2 s, so faster reloads than that are refused, not measured.
        let start = Instant::now();
        visit(&mut n, &hub, &mut ev, i % 4 == 3);
        let left = Duration::from_millis(2100).saturating_sub(start.elapsed());
        pump(&mut n, None, &mut ev, left.as_secs_f64(), |_, _, _| false);
        ev.clear();
        after.push(LIVE.load(Relaxed) as i64 - base as i64);
    }
    eprintln!("after each visit, over the joined node (B): {after:?}");
    // Then hellos alone from new ids, as fast as a reloading page could say them, for 40 s.
    let before = LIVE.load(Relaxed) as i64 - base as i64;
    let mut flood = vec![];
    let start = Instant::now();
    let mut said = 0;
    while start.elapsed() < Duration::from_secs(40) {
        let mut page = Page::new(&hub, n.peer_id());
        page.hello();
        drop(page);
        said += 1;
        pump(&mut n, None, &mut ev, 0.05, |_, _, _| false);
        ev.clear();
        if said % 50 == 0 {
            flood.push(LIVE.load(Relaxed) as i64 - base as i64);
        }
    }
    eprintln!("{said} hellos from new ids; before {before} B, then every 50 hellos (B): {flood:?}");
    // Past the cap on peers remembered (32 for 8 links) and the pipes' IDLE (60 s), the last 30 visits and the second
    // half of the hellos keep nothing more.
    let last = *after.last().unwrap();
    let late = after[after.len() - 1] - after[after.len() - 31];
    let grew = flood[flood.len() - 1] - flood[flood.len() / 2];
    assert!(late < 4096, "the last 30 visits grew the heap {late} B");
    assert!(grew < 4096, "the second half of the hellos grew the heap {grew} B");
    assert!(last < 64 << 10, "{last} B kept after 80 visits");
}
