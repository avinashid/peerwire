//! A member floods a hub's readers (NB6B-4). The frame that overdraws a member's relay budget is relayed before
//! the member is cut, so each join lands up to a doc's worth (about 4 MB here) on every reader, and budgets are
//! per key: fresh keys in turn, or one key coming back, pile more than OUTBOX_BYTES up for readers whose links are
//! slow or stalled. The hub drops what piled up and catches each such reader up afresh rather than cutting it:
//! every reader stays linked, and once the flood stops it holds what the hub holds.

use peerwire_sync::{DocId, Docs, Event, Instant, Kind, Options, PeerId, Route, DOC_TAG, OUTBOX_BYTES};

const HUB: PeerId = PeerId([0x11; 32]);
const READERS: u8 = 7;
/// What a reader's link holds unsent at most, as a send buffer does: past it, the hub's messages wait.
const BACKLOG: u64 = 1 << 20;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Member `p`'s first writer, incarnation 1.
fn writer(p: &PeerId) -> String {
    format!("{}{:08x}", hex(&p.0), 1)
}

/// A value of 1e20s as long as MAX_VALUE_LENGTH allows once written as JSON.stringify writes it (21 digits each).
fn value() -> String {
    format!("[{}0]", "1e20,".repeat(2978))
}

/// Set items `from..from + 63` by `w`, a doc's worth, then a kill of its own dots 1..=`kill` if not 0, in one
/// frame.
fn adds(w: &str, from: u64, kill: u64) -> String {
    let v = value();
    let es: Vec<String> = (from..from + 63).map(|n| format!("[0,{n},{n},\"s\",\"c\",null,{v}]")).collect();
    let x = if kill > 0 { format!(",\"x\":[[0,1,{kill}]]") } else { String::new() };
    format!("{{\"d\":\"board\",\"t\":1,\"ws\":[\"{w}\"],\"e\":[{}]{x}}}", es.join(","))
}

/// A kill by `by` of writer `of`'s dots 1..=n.
fn kill(by: &str, of: &str, n: u64) -> String {
    format!("{{\"d\":\"board\",\"t\":1,\"ws\":[\"{by}\",\"{of}\"],\"x\":[[1,1,{n}]]}}")
}

/// A hub whose readers link to it alone. Its link to each takes `rate` bytes a second; theirs to it take all.
struct Star {
    hub: Docs,
    id: DocId,
    readers: Vec<(PeerId, Docs, u64)>,
    rate: u64,
    us: u64,
    /// Members the hub forgot (Overflow).
    cut: Vec<PeerId>,
    /// Times a reader's queue was dropped: the hub's queue for it shrank while nothing was taken.
    resyncs: usize,
}

impl Star {
    fn new(rate: u64) -> Star {
        let now = Instant::from_millis(0);
        let mut hub = Docs::new(HUB, [5; 32]);
        let id = hub.open(now, "board", Options { hub: true, ..Options::default() }).unwrap();
        let mut readers = Vec::new();
        for i in 0..READERS {
            let p = PeerId([0x20 + i; 32]);
            let mut d = Docs::new(p, [0x20 + i; 32]);
            d.open(now, "board", Options::default()).unwrap();
            d.joined(now, HUB, Route::Direct);
            hub.joined(now, p, Route::Direct);
            readers.push((p, d, BACKLOG));
        }
        let mut s = Star { hub, id, readers, rate, us: 0, cut: Vec::new(), resyncs: 0 };
        s.run(1000);
        s
    }

    fn now(&self) -> Instant {
        Instant::from_micros(self.us)
    }

    fn queued(&self) -> Vec<usize> {
        self.readers.iter().map(|(p, ..)| self.hub.queued(p)).collect()
    }

    /// A frame from member `p` to the hub.
    fn send(&mut self, p: PeerId, frame: &str) {
        let before = self.queued();
        let mut m = vec![DOC_TAG];
        m.extend_from_slice(frame.as_bytes());
        assert!(self.hub.receive(self.now(), p, &m));
        self.resyncs += before.iter().zip(self.queued()).filter(|(b, a)| a < b).count();
        self.events();
    }

    fn events(&mut self) {
        while let Some(e) = self.hub.poll_event() {
            if let Event::Overflow(p) = e {
                self.cut.push(p);
            }
        }
    }

    /// `ms` of time in steps of 10 ms: timers, the hub's messages as each link takes them, the readers' at once.
    fn run(&mut self, ms: u64) {
        for _ in 0..ms / 10 {
            self.us += 10_000;
            let now = self.now();
            for (_, _, take) in &mut self.readers {
                *take = (*take + self.rate / 100).min(BACKLOG);
            }
            if self.hub.poll_timeout().is_some_and(|t| t <= now) {
                self.hub.handle_timeout(now);
            }
            while let Some((p, m)) = self.hub.poll_transmit() {
                // Anyone else is the member flooding: its link takes everything, and it reads none of it.
                let Some((_, d, take)) = self.readers.iter_mut().find(|(r, ..)| *r == p) else { continue };
                if *take < m.len() as u64 {
                    self.hub.retry(p, m);
                    continue;
                }
                *take -= m.len() as u64;
                assert!(d.receive(now, HUB, &m));
            }
            for (p, d, _) in &mut self.readers {
                if d.poll_timeout().is_some_and(|t| t <= now) {
                    d.handle_timeout(now);
                }
                while let Some((to, m)) = d.poll_transmit() {
                    assert_eq!(to, HUB);
                    assert!(self.hub.receive(now, *p, &m));
                }
                while let Some(e) = d.poll_event() {
                    assert!(!matches!(e, Event::Overflow(_)), "{e:?}");
                }
            }
            self.events();
        }
    }

    /// Whether every reader holds what the hub holds; the hub's set, as JSON text, is `len` long.
    fn converged(&mut self) -> (bool, usize) {
        let held = |d: &mut Docs, id| {
            let d = d.doc(id).unwrap();
            [Kind::Set, Kind::Map].map(|k| d.entries_json(k, "c")).join("\n")
        };
        let want = held(&mut self.hub, self.id);
        let id = self.id;
        (self.readers.iter_mut().all(|(_, d, _)| held(d, id) == want), want.len())
    }

    /// The readers' links take `rate` from now on; after a minute every reader is linked and holds what the hub
    /// holds, a doc's worth.
    fn settle(&mut self, rate: u64) {
        self.rate = rate;
        self.run(60_000);
        let readers: Vec<PeerId> = self.readers.iter().map(|(p, ..)| *p).collect();
        assert!(self.cut.iter().all(|p| !readers.contains(p)), "readers cut: {:?}", self.cut);
        let (converged, len) = self.converged();
        assert!(converged && len > 4_000_000, "converged: {converged}, the hub holds {len} B");
        assert!(self.hub.queued(&readers[0]) == 0);
    }
}

/// Fresh keys in turn, 200 ms apart: each clears the last one's items (`x` may name any writer) and adds a doc's
/// worth. Each is cut by its first frame; the readers aren't.
fn fresh_keys(rate: u64, keys: u8) -> Star {
    let mut s = Star::new(rate);
    let mut prev: Option<String> = None;
    for i in 0..keys {
        let a = PeerId([0xa0 + i; 32]);
        let w = writer(&a);
        s.hub.joined(s.now(), a, Route::Direct);
        if let Some(p) = &prev {
            s.send(a, &kill(&w, p, 63));
        }
        s.send(a, &adds(&w, 1, 0));
        assert_eq!(s.cut.last(), Some(&a), "key {i}");
        s.hub.left(s.now(), a);
        prev = Some(w);
        s.run(200);
    }
    s
}

/// One key, cut and back at once, 100 ms apart: each time a doc's worth, with a kill of its own last items in the
/// same frame. The adds come first, so they land only when the doc has room: every other time.
fn same_key(rate: u64, cycles: u64) -> Star {
    let mut s = Star::new(rate);
    let a = PeerId([0xaa; 32]);
    let w = writer(&a);
    for cycle in 0..cycles {
        s.hub.joined(s.now(), a, Route::Direct);
        let base = cycle * 63;
        s.send(a, &adds(&w, base + 1, base));
        s.run(100);
    }
    assert!(s.cut.iter().filter(|p| **p == a).count() as u64 >= cycles / 2, "{:?}", s.cut);
    s
}

/// Security's probe cut all seven readers after five fresh keys (4.70 MB in) when their links took nothing.
#[test]
fn fresh_keys_flooding_a_hub_leave_its_readers_linked() {
    for rate in [0, 2_000_000] {
        let mut s = fresh_keys(rate, 8);
        assert!(s.resyncs >= READERS as usize, "{rate} B/s: {} resyncs", s.resyncs);
        assert!(s.queued().iter().all(|&q| q <= OUTBOX_BYTES + 600_000), "{:?}", s.queued());
        s.settle(2_000_000);
    }
}

/// Security's probe cut all seven readers after one key came back seven times (7.52 MB in). Here it comes back 19
/// times: a reader whose link takes nothing is dropped twice, within the 30 s a member is never cut in.
#[test]
fn one_key_coming_back_to_a_hub_leaves_its_readers_linked() {
    for rate in [0, 2_000_000] {
        let mut s = same_key(rate, 19);
        assert!(s.resyncs >= 2 * READERS as usize, "{rate} B/s: {} resyncs", s.resyncs);
        s.settle(2_000_000);
    }
}
