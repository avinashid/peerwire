//! NB-126: a browser, which can't send UDP, links with a native node over the signaling relays (a pipe), as
//! `packages/js/src/webrtc.js` carries a link: its datagrams batched as `{"kind":"dg","d":[…]}`, sealed to the node on
//! the node's inbox topic, after a room hello in the native shape (as `bluetooth.js` says one). The browser here is a
//! bare endpoint on a `MemoryHub`, doing what the page does.

use peerwire::traversal::Traversal;
use peerwire::{
    Event, Lane, MemoryHub, MemorySignaling, Node, PathKind, PeerId, Policy, Role, Signaling, Status, Topic,
};
use peerwire_core::{sealed, Config, Endpoint, Identity, Instant as CoreInstant};
use rand_core::{OsRng, RngCore};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const APP: &str = "pw-test";
const ROOM: &str = "pipe";
/// The browser's handle for its pipe to the node.
const PIPE: u32 = 1;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

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

/// A batch's datagrams, as `webrtc.js` `pipeReceive` reads them.
fn decode(body: &[u8]) -> Vec<Vec<u8>> {
    let s = std::str::from_utf8(body).unwrap();
    let list = s.strip_prefix(r#"{"kind":"dg","d":["#).unwrap().strip_suffix("]}").unwrap();
    list.split(',').map(|item| unb64(item.trim_matches('"'))).collect()
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

/// What the LED page does over Nostr: a bare endpoint, a room hello in the native shape, one pipe to the node.
struct Browser {
    me: Identity,
    ep: Endpoint<u32>,
    sig: MemorySignaling,
    /// For the room hello alone, and to check the node's.
    trav: Traversal<u32>,
    epoch: Instant,
    node: PeerId,
    /// The node's hellos heard, and its messages.
    hellos: usize,
    got: Vec<Vec<u8>>,
    batches_in: usize,
}

impl Browser {
    fn new(hub: &MemoryHub, node: PeerId) -> Browser {
        let me = Identity::generate(&mut OsRng);
        let mut sig = hub.connect();
        sig.subscribe(Topic::peer(&me.peer_id()));
        sig.subscribe(Topic::room(APP, ROOM));
        let mut trav = Traversal::new(&me, [9; 32], unix_ms(), vec![], vec![]);
        trav.join_room(CoreInstant::ZERO, APP, ROOM, Role::Peer, None);
        while trav.poll_signal().is_some() {}
        let ep = Endpoint::new(&me, Config::default(), [4; 32]);
        Browser { me, ep, sig, trav, epoch: Instant::now(), node, hellos: 0, got: vec![], batches_in: 0 }
    }

    fn now(&self) -> CoreInstant {
        CoreInstant::from_micros(self.epoch.elapsed().as_micros() as u64)
    }

    fn hello(&mut self) {
        let h = self.trav.room_hello(self.now()).unwrap();
        self.sig.publish(Topic::room(APP, ROOM), &h);
    }

    fn dial(&mut self) {
        let now = self.now();
        self.ep.connect(now, PIPE, self.node).unwrap();
    }

    /// Sends `datagrams` to the node as one batch from this browser, sealed to the node on its inbox topic.
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
                let mut scratch = Endpoint::new(&self.me, Config::default(), [5; 32]);
                if self.trav.handle_signal(now, &mut scratch, topic, &payload) == Some(self.node) {
                    self.hellos += 1;
                }
                while self.trav.poll_signal().is_some() {}
                continue;
            }
            let Some((from, body)) = sealed::open(&self.me, &payload) else { continue };
            // The node's offers (binary, a version first) are no browser's business, as in nostr.js.
            if from != self.node || body.first() != Some(&b'{') {
                continue;
            }
            self.batches_in += 1;
            for d in decode(&body) {
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

fn node(hub: &MemoryHub) -> Node {
    let mut n =
        Node::builder().stun_servers(Vec::<String>::new()).signaling(hub.connect()).bind("127.0.0.1:0").unwrap();
    n.join_room(APP, ROOM);
    n
}

/// Runs both until `done` or `secs` pass; the node's events go to `ev`.
fn pump(
    n: &mut Node,
    b: &mut Browser,
    ev: &mut Vec<Event>,
    secs: u64,
    done: impl Fn(&Node, &Browser, &[Event]) -> bool,
) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        n.poll();
        b.step();
        ev.extend(std::iter::from_fn(|| n.try_recv_event()));
        if done(n, b, ev) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// Says `b`'s hello and links it with `n` over a pipe.
fn link(n: &mut Node, b: &mut Browser, ev: &mut Vec<Event>) {
    let (nid, bid) = (n.peer_id(), b.me.peer_id());
    b.hello();
    pump(n, b, ev, 1, |_, _, _| false);
    b.dial();
    assert!(pump(n, b, ev, 10, |n, b, _| n.status(&bid) == Status::Connected && b.ep.is_connected(&nid)));
    assert_eq!(n.path_kind(&bid), Some(PathKind::Signaling));
}

#[test]
fn a_browser_links_over_a_pipe_and_both_ends_talk() {
    let hub = MemoryHub::new();
    let mut n = node(&hub);
    let mut b = Browser::new(&hub, n.peer_id());
    let bid = b.me.peer_id();
    let mut ev = vec![];
    // Let the node's own first hellos go by, then say ours: it answers with its own at once.
    pump(&mut n, &mut b, &mut ev, 3, |_, _, _| false);
    let before = b.hellos;
    b.hello();
    assert!(pump(&mut n, &mut b, &mut ev, 2, |_, b, _| b.hellos > before), "the node answered our hello");
    b.dial();
    assert!(pump(&mut n, &mut b, &mut ev, 10, |n, b, _| (n.status(&bid) == Status::Connected)
        && b.ep.is_connected(&n.peer_id())));
    assert_eq!(n.path_kind(&bid), Some(PathKind::Signaling));
    assert!(ev.iter().any(|e| matches!(e, Event::Connected { peer, .. } if *peer == bid)), "{ev:?}");
    // The page's command, and the node's answer.
    b.ep.send(&n.peer_id(), Lane::Reliable, b"led on").unwrap();
    let node_got = |ev: &[Event]| {
        ev.iter().any(|e| matches!(e, Event::Message { peer, data, .. } if *peer == bid && data == b"led on"))
    };
    assert!(pump(&mut n, &mut b, &mut ev, 10, |_, _, ev| node_got(ev)), "{ev:?}");
    n.send(&bid, Lane::Reliable, b"LED is on").unwrap();
    assert!(pump(&mut n, &mut b, &mut ev, 10, |_, b, _| b.got.iter().any(|d| d == b"LED is on")));
    assert!(b.batches_in > 0);
}

#[test]
fn a_peer_that_said_no_hello_gets_no_pipe() {
    let hub = MemoryHub::new();
    let mut n = node(&hub);
    let mut b = Browser::new(&hub, n.peer_id());
    let bid = b.me.peer_id();
    let mut ev = vec![];
    b.dial();
    assert!(!pump(&mut n, &mut b, &mut ev, 4, |n, _, _| n.status(&bid) == Status::Connected));
    assert_eq!(b.batches_in, 0, "nothing came back");
    assert!(!ev.iter().any(|e| matches!(e, Event::Connected { .. })), "{ev:?}");
}

#[test]
fn a_policy_without_signaling_refuses_pipe_links() {
    let hub = MemoryHub::new();
    let mut n = node(&hub);
    n.set_policy(Policy::new([PathKind::Direct, PathKind::Birthday])).unwrap();
    let mut b = Browser::new(&hub, n.peer_id());
    let bid = b.me.peer_id();
    let mut ev = vec![];
    b.hello();
    pump(&mut n, &mut b, &mut ev, 1, |_, _, _| false);
    b.dial();
    assert!(!pump(&mut n, &mut b, &mut ev, 4, |n, _, _| n.status(&bid) == Status::Connected));
    assert!(!ev.iter().any(|e| matches!(e, Event::Connected { peer, .. } if *peer == bid)), "{ev:?}");
    // With it listed, the same page links.
    n.set_policy(Policy::new([PathKind::Direct, PathKind::Birthday, PathKind::Signaling])).unwrap();
    let mut b = Browser::new(&hub, n.peer_id());
    let bid = b.me.peer_id();
    b.hello();
    pump(&mut n, &mut b, &mut ev, 1, |_, _, _| false);
    b.dial();
    assert!(pump(&mut n, &mut b, &mut ev, 10, |n, _, _| n.status(&bid) == Status::Connected));
    assert_eq!(n.path_kind(&bid), Some(PathKind::Signaling));
}

/// NB126-T1: a member can't carry another peer's packets onto its own pipe. A links; B, a second member, puts A's
/// next datagrams in its own batch. The node drops them (`rx_wrong_peer`), and A's link stays on A's pipe.
#[test]
fn a_member_cannot_carry_another_peers_datagrams() {
    let hub = MemoryHub::new();
    let mut n = node(&hub);
    let nid = n.peer_id();
    let mut a = Browser::new(&hub, nid);
    let aid = a.me.peer_id();
    let mut ev = vec![];
    link(&mut n, &mut a, &mut ev);
    let path = n.path(&aid);
    // B says its hello, so its batches are taken.
    let mut b = Browser::new(&hub, nid);
    b.hello();
    pump(&mut n, &mut b, &mut ev, 1, |_, _, _| false);
    // A's next datagrams, fresh to the node, go in B's batch instead of A's.
    a.ep.send(&nid, Lane::Unreliable, b"via b").unwrap();
    let now = a.now();
    let carried: Vec<Vec<u8>> = std::iter::from_fn(|| a.ep.poll_transmit(now)).map(|t| t.payload).collect();
    assert!(!carried.is_empty());
    let wrong = n.stats().rx_wrong_peer;
    b.send_batch(&carried);
    let dropped = |n: &Node| n.stats().rx_wrong_peer >= wrong + carried.len() as u64;
    assert!(pump(&mut n, &mut b, &mut ev, 2, |n, _, _| dropped(n)), "B's batch was taken and A's packets dropped");
    let via_b = |ev: &[Event]| ev.iter().any(|e| matches!(e, Event::Message { data, .. } if data == b"via b"));
    assert!(!via_b(&ev), "{ev:?}");
    assert_eq!((n.status(&aid), n.path(&aid)), (Status::Connected, path), "A's link stays on A's pipe");
    // And A's own pipe still carries it, both ways.
    a.ep.send(&nid, Lane::Reliable, b"still here").unwrap();
    let got = |ev: &[Event]| {
        ev.iter().any(|e| matches!(e, Event::Message { peer, data, .. } if *peer == aid && data == b"still here"))
    };
    assert!(pump(&mut n, &mut a, &mut ev, 10, |_, _, ev| got(ev)), "{ev:?}");
    n.send(&aid, Lane::Reliable, b"ok").unwrap();
    assert!(pump(&mut n, &mut a, &mut ev, 10, |_, a, _| a.got.iter().any(|d| d == b"ok")));
    assert!(!via_b(&ev), "{ev:?}");
}

/// NB126-3: while batches wait for the pipes' rate, the node's `next_timeout` is when the next token comes, not 0, so
/// a host loop that sleeps on it doesn't spin.
#[test]
fn a_host_sleeps_while_batches_wait_for_the_rate() {
    let hub = MemoryHub::new();
    let mut n = node(&hub);
    let mut b = Browser::new(&hub, n.peer_id());
    let bid = b.me.peer_id();
    let mut ev = vec![];
    link(&mut n, &mut b, &mut ev);
    // More batches than the burst: most wait for the rate.
    for _ in 0..20 {
        n.send(&bid, Lane::Unreliable, &[7; 1000]).unwrap();
    }
    let (mut polls, mut zero) = (0, 0);
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        n.poll();
        polls += 1;
        let t = n.next_timeout().unwrap_or(Duration::from_millis(5));
        zero += usize::from(t.is_zero());
        std::thread::sleep(t.min(Duration::from_millis(5)));
    }
    eprintln!("{polls} polls, {zero} with a zero timeout");
    assert!(zero <= 5, "{zero} of {polls} polls had a zero timeout");
}
