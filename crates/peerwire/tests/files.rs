//! `peerwire::files` between real nodes on loopback, in a room over the in-process carrier, through `impl Wire for
//! Node`: a file each way at once into `FileSink`s, byte for byte, and a file that goes on from the last piece written
//! after the receiver's link is replaced (its node restarted with the same identity) mid-file.

use peerwire::files::{End, Event as FileEvent, Files, TransferId};
use peerwire::{DisconnectReason, Identity, MemoryHub, Node, PeerId};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Side {
    node: Node,
    files: Files,
    events: Vec<FileEvent>,
}

impl Side {
    fn new(node: Node) -> Side {
        Side { node, files: Files::new(), events: Vec::new() }
    }

    fn turn(&mut self) {
        let now = Instant::now();
        self.node.poll();
        while let Some(e) = self.node.try_recv_event() {
            self.files.handle_event(now, &e);
        }
        self.files.poll(now, &mut self.node);
        self.node.poll();
        self.events.extend(std::iter::from_fn(|| self.files.poll_event()));
    }

    fn ended(&self, id: TransferId) -> Option<&End> {
        self.events.iter().find_map(|e| match e {
            FileEvent::Ended { id: i, end } if *i == id => Some(end),
            _ => None,
        })
    }

    fn offered(&self) -> Option<TransferId> {
        self.events.iter().find_map(|e| if let FileEvent::Offer { id, .. } = e { Some(*id) } else { None })
    }
}

fn node(hub: &MemoryHub, identity: Option<Identity>) -> Node {
    let mut b = Node::builder().signaling(hub.connect());
    if let Some(id) = identity {
        b = b.identity(id);
    }
    let mut n = b.bind("127.0.0.1:0").unwrap();
    n.join_room("pw-files-test", "room");
    n
}

fn until(sides: &mut [&mut Side], what: &str, mut done: impl FnMut(&[&mut Side]) -> bool) {
    let t = Instant::now();
    while !done(sides) {
        assert!(t.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
        for s in sides.iter_mut() {
            s.turn();
        }
        std::thread::sleep(Duration::from_micros(200));
    }
}

fn data(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pw-files-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn files_go_both_ways_at_once_between_two_nodes() {
    let hub = MemoryHub::new();
    let (mut a, mut b) = (Side::new(node(&hub, None)), Side::new(node(&hub, None)));
    let (pa, pb) = (a.node.peer_id(), b.node.peer_id());
    until(&mut [&mut a, &mut b], "a link", |s| {
        s[0].node.peers().any(|p| *p == pb) && s[1].node.peers().any(|p| *p == pa)
    });
    let (src, into_a, into_b) = (dir("src"), dir("into-a"), dir("into-b"));
    let one = data(3 * 512 * 1024 + 17, 1);
    let two = data(700_000, 2);
    let to_b = a.files.offer_file(pb, &write(&src, "one.bin", &one)).unwrap();
    // A name with a path and a bidi override, as a peer might send it.
    let f = std::fs::File::open(write(&src, "two.bin", &two)).unwrap();
    let to_a = b.files.offer(pa, "../two\u{202e}.bin", "", two.len() as u64, Box::new(f)).unwrap();
    until(&mut [&mut a, &mut b], "the offers", |s| s[0].offered().is_some() && s[1].offered().is_some());
    let (in_a, in_b) = (a.offered().unwrap(), b.offered().unwrap());
    a.files.accept_into(in_a, &into_a).unwrap();
    b.files.accept_into(in_b, &into_b).unwrap();
    until(&mut [&mut a, &mut b], "both files", |s| {
        s[0].ended(to_b).is_some()
            && s[1].ended(to_a).is_some()
            && s[0].ended(in_a).is_some()
            && s[1].ended(in_b).is_some()
    });
    for (side, id) in [(&a, to_b), (&b, to_a), (&a, in_a), (&b, in_b)] {
        assert!(matches!(side.ended(id), Some(End::Done { .. })), "{:?}", side.ended(id));
    }
    assert_eq!(std::fs::read(into_b.join("one.bin")).unwrap(), one);
    // The name as cleanName cleans it: no path, no bidi override.
    assert_eq!(std::fs::read(into_a.join("_two.bin")).unwrap(), two);
    assert_eq!(b.files.info(in_b).unwrap().saved_as.unwrap(), into_b.join("one.bin"));
    for d in [src, into_a, into_b] {
        std::fs::remove_dir_all(d).unwrap();
    }
}

#[test]
fn a_file_goes_on_after_the_receivers_link_is_replaced() {
    let hub = MemoryHub::new();
    let key = [7u8; 32];
    let mut a = Side::new(node(&hub, None));
    let mut b = Side::new(node(&hub, Some(Identity::from_secret_bytes(&key))));
    let (pa, pb): (PeerId, PeerId) = (a.node.peer_id(), b.node.peer_id());
    until(&mut [&mut a, &mut b], "a link", |s| {
        s[0].node.peers().any(|p| *p == pb) && s[1].node.peers().any(|p| *p == pa)
    });
    let (src, into) = (dir("resume-src"), dir("resume-into"));
    let bytes = data(12 * 512 * 1024 + 5, 3);
    let id = a.files.offer_file(pb, &write(&src, "big.bin", &bytes)).unwrap();
    until(&mut [&mut a, &mut b], "the offer", |s| s[1].offered().is_some());
    let inc = b.offered().unwrap();
    b.files.accept_into(inc, &into).unwrap();
    until(&mut [&mut a, &mut b], "a few pieces", |s| s[1].files.info(inc).unwrap().bytes >= 3 * 512 * 1024);
    // B's node restarts with the same identity; its transfers carry on with the new node.
    b.node = node(&hub, Some(Identity::from_secret_bytes(&key)));
    b.files.disconnected(Instant::now(), pa, DisconnectReason::Timeout);
    until(&mut [&mut a, &mut b], "the file", |s| s[0].ended(id).is_some() && s[1].ended(inc).is_some());
    assert!(matches!(a.ended(id), Some(End::Done { .. })), "{:?}", a.ended(id));
    assert!(matches!(b.ended(inc), Some(End::Done { .. })), "{:?}", b.ended(inc));
    assert!(a.events.iter().any(|e| matches!(e, FileEvent::Resumed { from, .. } if *from >= 3)), "{:?}", a.events);
    assert_eq!(std::fs::read(into.join("big.bin")).unwrap(), bytes);
    let left: Vec<_> = std::fs::read_dir(&into).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, ["big.bin"], "no partial file left");
    for d in [src, into] {
        std::fs::remove_dir_all(d).unwrap();
    }
}
