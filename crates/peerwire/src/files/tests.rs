//! The Share protocol between two `Files` over an in-memory wire, and what a hostile peer can send. The golden
//! vectors files.js makes (packages/js/test/rust_files.mjs) are checked in `vectors` below.

use super::frame::{self, Message, CONTROL_FRAME, HEADER, PIECE_FRAME};
use super::*;
use std::cell::RefCell;
use std::io::Cursor;
use std::rc::Rc;

const A: PeerId = PeerId([0xa; 32]);
const B: PeerId = PeerId([0xb; 32]);

/// Frames each side sent, to deliver by hand.
#[derive(Default)]
struct Link {
    sent: Vec<(PeerId, Vec<u8>)>,
    direct: bool,
    /// Refuse sends with WouldBlock.
    full: bool,
}

impl Wire for Link {
    fn send(&mut self, peer: &PeerId, frame: &[u8]) -> Result<(), SendError> {
        if self.full {
            return Err(SendError::WouldBlock);
        }
        self.sent.push((*peer, frame.to_vec()));
        Ok(())
    }

    fn carries_files(&self, _: &PeerId) -> bool {
        self.direct
    }
}

#[derive(Default)]
struct MemState {
    data: Vec<u8>,
    finished: bool,
    aborted: bool,
    fail_write: bool,
}

#[derive(Clone, Default)]
struct Mem(Rc<RefCell<MemState>>);

impl Sink for Mem {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let mut s = self.0.borrow_mut();
        if s.fail_write {
            return Err(io::Error::other("disk full"));
        }
        s.data.extend_from_slice(data);
        Ok(())
    }
    fn finish(&mut self) -> io::Result<()> {
        self.0.borrow_mut().finished = true;
        Ok(())
    }
    fn abort(&mut self) {
        self.0.borrow_mut().aborted = true;
    }
}

/// Two sides, A and B, linked.
struct Pair {
    a: Files,
    b: Files,
    la: Link,
    lb: Link,
    now: Instant,
    /// Most pieces A had in flight to B (sent, not acked as B's acks reached A).
    max_in_flight: u64,
}

impl Pair {
    fn new() -> Pair {
        let mut a = Files::with_seed([1; 32]);
        let mut b = Files::with_seed([2; 32]);
        a.connected(B);
        b.connected(A);
        let la = Link { direct: true, ..Link::default() };
        let lb = Link { direct: true, ..Link::default() };
        Pair { a, b, la, lb, now: Instant::now(), max_in_flight: 0 }
    }

    /// Polls both and delivers what each sent, until neither sends anything.
    fn run(&mut self) {
        for _ in 0..100_000 {
            self.a.poll(self.now, &mut self.la);
            self.b.poll(self.now, &mut self.lb);
            let (fa, fb) = (std::mem::take(&mut self.la.sent), std::mem::take(&mut self.lb.sent));
            if fa.is_empty() && fb.is_empty() {
                return;
            }
            for (to, f) in fa {
                assert_eq!(to, B);
                self.b.receive_frame(A, &f);
            }
            for (to, f) in fb {
                assert_eq!(to, A);
                self.a.receive_frame(B, &f);
            }
            self.max_in_flight = self.max_in_flight.max(self.a.in_flight(&B));
        }
        panic!("never settled");
    }

    fn events(f: &mut Files) -> Vec<Event> {
        std::iter::from_fn(|| f.poll_event()).collect()
    }
}

fn data(n: usize) -> Vec<u8> {
    let mut x = 0x9e3779b9u32;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn offer_id(events: &[Event]) -> TransferId {
    events.iter().find_map(|e| if let Event::Offer { id, .. } = e { Some(*id) } else { None }).expect("an offer")
}

fn ended(events: &[Event]) -> Option<End> {
    events.iter().find_map(|e| if let Event::Ended { end, .. } = e { Some(end.clone()) } else { None })
}

/// A sends `bytes` to B, who accepts into memory: (A's events, B's events, what B got).
fn transfer(p: &mut Pair, bytes: &[u8]) -> (Vec<Event>, Vec<Event>, Mem) {
    p.a.offer(B, "report.pdf", "application/pdf", bytes.len() as u64, Box::new(Cursor::new(bytes.to_vec()))).unwrap();
    p.run();
    let offered = Pair::events(&mut p.b);
    let id = offer_id(&offered);
    let mem = Mem::default();
    p.b.accept(id, Box::new(mem.clone())).unwrap();
    p.run();
    (Pair::events(&mut p.a), Pair::events(&mut p.b), mem)
}

#[test]
fn sends_a_file_in_checked_pieces() {
    let mut p = Pair::new();
    let bytes = data(3 * PIECE as usize + 12345);
    let (ea, eb, mem) = transfer(&mut p, &bytes);
    let want = frame::hex(&frame::sha256(&bytes));
    assert_eq!(ended(&ea), Some(End::Done { sha256: want.clone() }));
    assert_eq!(ended(&eb), Some(End::Done { sha256: want }));
    let m = mem.0.borrow();
    assert!(m.finished && !m.aborted && m.data == bytes);
    assert!(p.max_in_flight <= WINDOW);
    assert!(ea.contains(&Event::Progress { id: TransferId(1), bytes: bytes.len() as u64, size: bytes.len() as u64 }));
    assert_eq!(p.a.stats().pieces_sent, 4);
    assert_eq!(p.b.stats().pieces_taken, 4);
}

#[test]
fn the_window_holds_over_many_pieces() {
    let mut p = Pair::new();
    // Deliver A's frames one round at a time, B's acks a round late: A never has more than WINDOW out.
    let bytes = data(20 * MIN_PIECE as usize);
    let (ea, _, mem) = transfer(&mut p, &bytes);
    assert!(matches!(ended(&ea), Some(End::Done { .. })));
    assert_eq!(mem.0.borrow().data, bytes);
    assert_eq!(p.max_in_flight, WINDOW);
}

#[test]
fn an_empty_file() {
    let mut p = Pair::new();
    let (ea, eb, mem) = transfer(&mut p, &[]);
    let want = frame::hex(&frame::sha256(&[]));
    assert_eq!(ended(&ea), Some(End::Done { sha256: want.clone() }));
    assert_eq!(ended(&eb), Some(End::Done { sha256: want }));
    assert!(mem.0.borrow().finished);
}

#[test]
fn decline_and_cancel() {
    let mut p = Pair::new();
    p.a.offer(B, "a", "", 10, Box::new(Cursor::new(vec![0; 10]))).unwrap();
    p.run();
    let id = offer_id(&Pair::events(&mut p.b));
    p.b.decline(id).unwrap();
    p.run();
    assert_eq!(ended(&Pair::events(&mut p.a)), Some(End::Declined));
    assert!(p.b.accept(id, Box::new(Mem::default())).is_err());

    // The receiver cancels halfway: the sender stops, and what was written is dropped.
    let bytes = data(4 * PIECE as usize);
    p.a.offer(B, "big", "", bytes.len() as u64, Box::new(Cursor::new(bytes))).unwrap();
    p.run();
    let id = offer_id(&Pair::events(&mut p.b));
    let mem = Mem::default();
    p.b.accept(id, Box::new(mem.clone())).unwrap();
    p.b.poll(p.now, &mut p.lb);
    for (_, f) in std::mem::take(&mut p.lb.sent) {
        p.a.receive_frame(B, &f);
    }
    p.a.poll(p.now, &mut p.la);
    let first = std::mem::take(&mut p.la.sent);
    p.b.receive_frame(A, &first[0].1);
    p.b.cancel(id).unwrap();
    p.run();
    assert!(mem.0.borrow().aborted && !mem.0.borrow().finished);
    assert!(matches!(ended(&Pair::events(&mut p.a)), Some(End::Cancelled(_))));
    assert_eq!(p.b.info(id).unwrap().state, State::Cancelled);
}

#[test]
fn no_pieces_while_the_link_is_not_direct() {
    let mut p = Pair::new();
    p.la.direct = false;
    let bytes = data(PIECE as usize + 1);
    p.a.offer(B, "f", "", bytes.len() as u64, Box::new(Cursor::new(bytes.clone()))).unwrap();
    p.run();
    let id = offer_id(&Pair::events(&mut p.b));
    let mem = Mem::default();
    p.b.accept(id, Box::new(mem.clone())).unwrap();
    p.run();
    // The accept went (control messages go on any link), no piece did.
    assert!(Pair::events(&mut p.a).contains(&Event::Accepted { id: TransferId(1) }));
    assert_eq!(p.a.stats().pieces_sent, 0);
    p.la.direct = true;
    p.run();
    assert_eq!(mem.0.borrow().data, bytes);
}

#[test]
fn a_full_link_keeps_the_piece_it_refused() {
    let mut p = Pair::new();
    let bytes = data(2 * PIECE as usize);
    p.a.offer(B, "f", "", bytes.len() as u64, Box::new(Cursor::new(bytes.clone()))).unwrap();
    p.run();
    let id = offer_id(&Pair::events(&mut p.b));
    let mem = Mem::default();
    p.b.accept(id, Box::new(mem.clone())).unwrap();
    p.b.poll(p.now, &mut p.lb);
    for (_, f) in std::mem::take(&mut p.lb.sent) {
        p.a.receive_frame(B, &f);
    }
    p.la.full = true;
    for _ in 0..5 {
        p.a.poll(p.now, &mut p.la);
    }
    let t = p.a.get(TransferId(1)).unwrap();
    assert!(t.pending.is_some() && t.read == 1 && t.next == 0);
    p.la.full = false;
    p.run();
    assert_eq!(mem.0.borrow().data, bytes);
}

#[test]
fn resumes_after_the_link_drops() {
    let mut p = Pair::new();
    let bytes = data(6 * PIECE as usize);
    p.a.offer(B, "f", "", bytes.len() as u64, Box::new(Cursor::new(bytes.clone()))).unwrap();
    p.run();
    let id = offer_id(&Pair::events(&mut p.b));
    let mem = Mem::default();
    p.b.accept(id, Box::new(mem.clone())).unwrap();
    p.b.poll(p.now, &mut p.lb);
    for (_, f) in std::mem::take(&mut p.lb.sent) {
        p.a.receive_frame(B, &f);
    }
    // A sends a window; B takes two of the pieces, then the link drops and the rest are lost.
    p.a.poll(p.now, &mut p.la);
    let sent = std::mem::take(&mut p.la.sent);
    assert_eq!(sent.len(), 7, "six pieces and the end");
    for (_, f) in &sent[..2] {
        p.b.receive_frame(A, f);
    }
    p.a.disconnected(p.now, B, DisconnectReason::Timeout);
    p.b.disconnected(p.now, A, DisconnectReason::Timeout);
    p.lb.sent.clear();
    assert!(p.a.info(TransferId(1)).unwrap().peer_gone);
    p.a.connected(B);
    p.b.connected(A);
    p.run();
    assert_eq!(mem.0.borrow().data, bytes);
    let ea = Pair::events(&mut p.a);
    assert!(ea.contains(&Event::Resumed { id: TransferId(1), from: 2 }));
    assert!(matches!(ended(&ea), Some(End::Done { .. })));
    // Every piece went into the whole-file hash once: B's matched.
    assert!(matches!(ended(&Pair::events(&mut p.b)), Some(End::Done { .. })));
}

#[test]
fn a_peer_that_closes_the_link_ends_its_transfers_and_one_that_never_comes_back_fails_them() {
    let mut p = Pair::new();
    p.a.offer(B, "f", "", 1, Box::new(Cursor::new(vec![1]))).unwrap();
    p.a.disconnected(p.now, B, DisconnectReason::ClosedByPeer);
    assert!(matches!(ended(&Pair::events(&mut p.a)), Some(End::Cancelled(_))));
    p.a.connected(B);
    let id = p.a.offer(B, "g", "", 1, Box::new(Cursor::new(vec![1]))).unwrap();
    p.a.disconnected(p.now, B, DisconnectReason::Timeout);
    p.a.poll(p.now + RESUME - Duration::from_secs(1), &mut p.la);
    assert_eq!(p.a.info(id).unwrap().state, State::Offered);
    p.a.poll(p.now + RESUME, &mut p.la);
    assert_eq!(p.a.info(id).unwrap().state, State::Failed);
}

// ---------- what a hostile peer sends ----------

fn control(json: &str) -> Vec<u8> {
    let mut f = vec![CONTROL_FRAME];
    f.extend_from_slice(json.as_bytes());
    f
}

const ID: &str = "0123456789abcdef0123456789abcdef";

fn offer_json(extra: &str) -> String {
    format!(r#"{{"t":"offer","v":1,"id":"{ID}","name":"x.bin","size":600000,"piece":524288{extra}}}"#)
}

/// What B makes of one frame from A: its events.
fn b_takes(f: &[u8]) -> (Files, Vec<Event>) {
    let mut b = Files::with_seed([2; 32]);
    b.connected(A);
    b.receive_frame(A, f);
    let e = Pair::events(&mut b);
    (b, e)
}

#[test]
fn control_messages_over_the_limit_or_malformed_are_dropped() {
    // Exactly MAX_CONTROL bytes of JSON is read; one more never is.
    let base = offer_json("");
    let pad = |n: usize| format!(r#"{},"pad":"{}"}}"#, &base[..base.len() - 1], "p".repeat(n));
    let at = pad(MAX_CONTROL - pad(0).len());
    assert_eq!(at.len(), MAX_CONTROL);
    assert_eq!(b_takes(&control(&at)).1.len(), 1);
    assert!(b_takes(&control(&pad(MAX_CONTROL + 1 - pad(0).len()))).1.is_empty());
    for bad in [
        "",
        "{",
        "[]",
        "null",
        r#""offer""#,
        r#"{"t":1}"#,
        r#"{"t":"offer",}"#,
        r#"{"t":"offer"} x"#,
        r#"{'t':'offer'}"#,
        "{\"t\":\"of\u{1}fer\"}",
        r#"{"t":"\x"}"#,
        r#"{"t":"\u12"}"#,
        r#"{"t":"offer","a":01}"#,
        r#"{"t":"offer","a":1.}"#,
        r#"{"t":"offer","a":-}"#,
        r#"{"t":"offer","a":[1,]}"#,
        r#"{"t":"offer","a":{"b"}}"#,
        r#"{"t":"offer","a":tru}"#,
    ] {
        assert!(frame::decode_control(&control(bad)).is_none(), "{bad:?}");
    }
    // A piece frame, an empty frame, an unknown kind.
    assert!(frame::decode_control(&[PIECE_FRAME, b'{', b'}']).is_none());
    assert!(frame::decode_control(&[]).is_none());
    // As JSON.parse reads them: a BOM is dropped, depth is no limit, the last of a key given twice counts.
    let deep = format!(r#"{{"x":{}1{},"t":"cancel","id":"{ID}"}}"#, "[".repeat(2000), "]".repeat(2000));
    for good in ["\u{feff}{\"t\":\"x\"}", " {\"t\" : \"x\" } ", &deep, r#"{"t":1,"t":"x"}"#, r#"{"t":"x"}"#] {
        assert!(frame::decode_control(&control(good)).is_some(), "{good:?}");
    }
    assert!(frame::decode_control(&control(r#"{"t":"x","t":null}"#)).is_none());
}

#[test]
fn offers_are_checked() {
    let ok = |extra: &str| b_takes(&control(&offer_json(extra))).1.len() == 1;
    assert!(ok(""));
    assert!(ok(r#","count":2"#));
    assert!(ok(r#","count":2.0,"type":"Text/Plain""#));
    assert!(!ok(r#","count":3"#));
    assert!(!ok(r#","count":null"#));
    assert!(!ok(r#","count":"2""#));
    assert!(!ok(r#","type":null"#));
    assert!(!ok(r#","type":5"#));
    let with = |size: &str, piece: &str| {
        let j = format!(r#"{{"t":"offer","id":"{ID}","name":"n","size":{size},"piece":{piece}}}"#);
        b_takes(&control(&j)).1.len() == 1
    };
    assert!(with("0", "262144"));
    assert!(with("-0", "262144"));
    assert!(with("1099511627776", "1048522"));
    assert!(with("1e3", "2.62144e5"));
    for (size, piece) in [
        ("-1", "524288"),
        ("1099511627777", "524288"),
        ("1.5", "524288"),
        ("\"1\"", "524288"),
        ("1e400", "524288"),
        ("null", "524288"),
        ("1", "262143"),
        ("1", "1048523"),
        ("1", "524288.5"),
    ] {
        assert!(!with(size, piece), "{size} {piece}");
    }
    // Ids: 32 lowercase hex digits.
    for bad in ["0123456789ABCDEF0123456789abcdef", "0123", "", "0123456789abcdef0123456789abcdefa"] {
        let j = format!(r#"{{"t":"offer","id":"{bad}","name":"n","size":1,"piece":524288}}"#);
        assert!(b_takes(&control(&j)).1.is_empty(), "{bad}");
    }
    // Names: at most MAX_RAW_NAME UTF-16 units as sent, cleaned.
    let named = |name: &str| {
        let j = format!(r#"{{"t":"offer","id":"{ID}","name":{name},"size":1,"piece":524288}}"#);
        b_takes(&control(&j))
            .1
            .into_iter()
            .find_map(|e| if let Event::Offer { name, .. } = e { Some(name) } else { None })
    };
    assert_eq!(named(&format!("\"{}\"", "a".repeat(1024))).unwrap().chars().count(), MAX_NAME);
    assert_eq!(named(&format!("\"{}\"", "a".repeat(1025))), None);
    assert_eq!(named(&format!("\"{}\"", "😀".repeat(512))).map(|n| n.chars().count()), Some(MAX_NAME));
    assert_eq!(named(&format!("\"{}\"", "😀".repeat(513))), None);
    assert_eq!(named("5"), None);
    assert_eq!(named(r#""../../etc/passwd""#).unwrap(), "_.._etc_passwd");
    assert_eq!(named(r#""a\ud800b""#).unwrap(), "ab");
    assert_eq!(named("\"\u{202e}gpj.exe\"").unwrap(), "gpj.exe");
    assert_eq!(named(r#""CON.txt""#).unwrap(), "_CON.txt");
    assert_eq!(named(r#""...""#).unwrap(), "file");
    // Invalid UTF-8 reads as U+FFFD, as TextDecoder reads it.
    let mut f = control(&format!(r#"{{"t":"offer","id":"{ID}","name":"a"#));
    f.extend_from_slice(&[0xff, 0xc3]);
    f.extend_from_slice(br#"b","size":1,"piece":524288}"#);
    let (_, e) = b_takes(&f);
    assert!(matches!(&e[..], [Event::Offer { name, .. }] if name == "a\u{fffd}\u{fffd}b"));
}

#[test]
fn offers_past_the_limit_are_declined_and_repeats_ignored() {
    let mut b = Files::with_seed([2; 32]);
    b.connected(A);
    let mut lb = Link::default();
    for i in 0..MAX_OFFERS + 2 {
        let j = format!(r#"{{"t":"offer","id":"{i:032x}","name":"n","size":1,"piece":524288}}"#);
        b.receive_frame(A, &control(&j));
        b.receive_frame(A, &control(&j));
    }
    assert_eq!(Pair::events(&mut b).len(), MAX_OFFERS);
    b.poll(Instant::now(), &mut lb);
    let declines: Vec<_> = lb.sent.iter().map(|(_, f)| String::from_utf8_lossy(&f[1..]).into_owned()).collect();
    assert_eq!(declines.len(), 4, "the two past the limit, each twice: {declines:?}");
    assert!(declines[0].starts_with(r#"{"t":"decline","id":"#));
    assert_eq!(b.stats().offers_refused, 4);
}

/// B, having accepted A's offer of `size` bytes in pieces of 262144: B, its sink, and its wire.
fn accepted(size: u64) -> (Files, Mem, Link, TransferId) {
    let mut b = Files::with_seed([2; 32]);
    b.connected(A);
    let j = format!(r#"{{"t":"offer","id":"{ID}","name":"n","size":{size},"piece":262144}}"#);
    b.receive_frame(A, &control(&j));
    let id = offer_id(&Pair::events(&mut b));
    let mem = Mem::default();
    b.accept(id, Box::new(mem.clone())).unwrap();
    let mut lb = Link::default();
    b.poll(Instant::now(), &mut lb);
    lb.sent.clear();
    (b, mem, lb, id)
}

fn wid() -> WireId {
    WireId::parse(&ID.encode_utf16().collect::<Vec<_>>()).unwrap()
}

fn piece(index: u32, data: &[u8]) -> Vec<u8> {
    frame::encode_piece(wid(), index, &frame::sha256(data), data)
}

fn sent_texts(l: &mut Link) -> Vec<String> {
    std::mem::take(&mut l.sent).into_iter().map(|(_, f)| String::from_utf8_lossy(&f[1..]).into_owned()).collect()
}

#[test]
fn pieces_out_of_order_or_the_wrong_length_are_refused() {
    let size = 2 * 262144 + 10;
    let (mut b, mem, mut lb, id) = accepted(size);
    let full = data(262144);
    // Out of order (a gap), past the end, the wrong length, a short frame: refused, nothing written or acked.
    b.receive_frame(A, &piece(1, &full));
    b.receive_frame(A, &piece(3, &full[..10]));
    b.receive_frame(A, &piece(0, &full[..100]));
    b.receive_frame(A, &piece(0, &data(262145)));
    b.receive_frame(A, &piece(0, &full)[..HEADER - 1]);
    assert!(mem.0.borrow().data.is_empty());
    assert_eq!(b.stats().rejected, 4);
    // The next one is taken, and acked once written; again, it is a duplicate.
    b.receive_frame(A, &piece(0, &full));
    b.receive_frame(A, &piece(0, &full));
    b.poll(Instant::now(), &mut lb);
    assert_eq!(sent_texts(&mut lb), [format!(r#"{{"t":"ack","id":"{ID}","have":1}}"#)]);
    assert_eq!(mem.0.borrow().data, full);
    // Past MAX_REJECTS the transfer is cancelled, and the sender told.
    for _ in 0..3 {
        b.receive_frame(A, &piece(5, &full));
    }
    assert_eq!(b.info(id).unwrap().state, State::Receiving);
    b.receive_frame(A, &piece(5, &full));
    assert_eq!(b.info(id).unwrap().state, State::Cancelled);
    b.poll(Instant::now(), &mut lb);
    assert_eq!(sent_texts(&mut lb), [format!(r#"{{"t":"cancel","id":"{ID}"}}"#)]);
    assert!(mem.0.borrow().aborted);
    // A last piece of the right (short) length is fine: the file's last piece.
    let (mut b, mem, _, _) = accepted(10);
    b.receive_frame(A, &piece(0, &full[..10]));
    assert_eq!(mem.0.borrow().data, &full[..10]);
}

#[test]
fn a_piece_that_fails_its_hash_fails_the_transfer() {
    let (mut b, mem, mut lb, id) = accepted(262144);
    let d = data(262144);
    let mut f = piece(0, &d);
    *f.last_mut().unwrap() ^= 1;
    b.receive_frame(A, &f);
    assert_eq!(b.info(id).unwrap().state, State::Failed);
    assert!(mem.0.borrow().data.is_empty() && mem.0.borrow().aborted);
    b.poll(Instant::now(), &mut lb);
    assert_eq!(sent_texts(&mut lb), [format!(r#"{{"t":"cancel","id":"{ID}"}}"#)]);
}

#[test]
fn pieces_of_a_transfer_not_accepted_are_dropped() {
    let mut b = Files::with_seed([2; 32]);
    b.connected(A);
    let d = data(262144);
    b.receive_frame(A, &piece(0, &d));
    let j = format!(r#"{{"t":"offer","id":"{ID}","name":"n","size":262144,"piece":262144}}"#);
    b.receive_frame(A, &control(&j));
    b.receive_frame(A, &piece(0, &d));
    assert_eq!(b.stats().rejected, 2);
    assert_eq!(b.stats().pieces_taken, 0);
    // Another peer's pieces for the same id aren't this transfer's either.
    let (mut b, mem, _, _) = accepted(262144);
    b.connected(B);
    b.receive_frame(B, &piece(0, &d));
    assert!(mem.0.borrow().data.is_empty());
}

#[test]
fn a_whole_file_hash_that_doesnt_match_keeps_nothing() {
    let (mut b, mem, mut lb, id) = accepted(10);
    b.receive_frame(A, &piece(0, &data(10)));
    let bad = "0".repeat(64);
    b.receive_frame(A, &control(&format!(r#"{{"t":"end","id":"{ID}","sha256":"{bad}"}}"#)));
    assert_eq!(b.info(id).unwrap().state, State::Failed);
    assert!(mem.0.borrow().aborted && !mem.0.borrow().finished);
    b.poll(Instant::now(), &mut lb);
    let got = frame::hex(&frame::sha256(&data(10)));
    assert_eq!(sent_texts(&mut lb)[1], format!(r#"{{"t":"done","id":"{ID}","ok":false,"sha256":"{got}"}}"#));
    // An end whose hash isn't 64 lowercase hex digits cancels.
    let (mut b, _, _, id) = accepted(10);
    b.receive_frame(A, &control(&format!(r#"{{"t":"end","id":"{ID}","sha256":"{}"}}"#, "A".repeat(64))));
    assert_eq!(b.info(id).unwrap().state, State::Cancelled);
}

#[test]
fn a_write_that_fails_cancels() {
    let (mut b, mem, _, id) = accepted(10);
    mem.0.borrow_mut().fail_write = true;
    b.receive_frame(A, &piece(0, &data(10)));
    assert_eq!(b.info(id).unwrap().state, State::Cancelled);
}

#[test]
fn the_sender_takes_only_answers_that_can_be() {
    let mut p = Pair::new();
    let bytes = data(3 * PIECE as usize);
    p.a.offer(B, "f", "", bytes.len() as u64, Box::new(Cursor::new(bytes))).unwrap();
    p.run();
    let w = p.a.get(TransferId(1)).unwrap().wire.hex();
    let say = |a: &mut Files, j: String| a.receive_frame(B, &control(&j));
    // An accept with have other than 0 starts nothing; an ack before any piece went is ignored; so is a done.
    say(&mut p.a, format!(r#"{{"t":"accept","id":"{w}","have":1}}"#));
    say(&mut p.a, format!(r#"{{"t":"accept","id":"{w}"}}"#));
    assert_eq!(p.a.info(TransferId(1)).unwrap().state, State::Offered);
    say(&mut p.a, format!(r#"{{"t":"accept","id":"{w}","have":-0}}"#));
    assert_eq!(p.a.info(TransferId(1)).unwrap().state, State::Sending);
    // Every piece, then the end: acks open the window, not the end.
    p.a.poll(p.now, &mut p.la);
    assert_eq!(p.la.sent.len(), 4);
    say(&mut p.a, format!(r#"{{"t":"ack","id":"{w}","have":4}}"#));
    say(&mut p.a, format!(r#"{{"t":"ack","id":"{w}","have":1.5}}"#));
    assert_eq!(p.a.get(TransferId(1)).unwrap().acked, 0);
    say(&mut p.a, format!(r#"{{"t":"ack","id":"{w}","have":2}}"#));
    say(&mut p.a, format!(r#"{{"t":"ack","id":"{w}","have":1}}"#));
    assert_eq!(p.a.get(TransferId(1)).unwrap().acked, 2);
    say(&mut p.a, format!(r#"{{"t":"done","id":"{w}","ok":true,"sha256":"{}"}}"#, "0".repeat(64)));
    assert_eq!(p.a.info(TransferId(1)).unwrap().state, State::Sending);
    // Another peer's answers aren't for this transfer.
    p.a.connected(PeerId([9; 32]));
    p.a.receive_frame(PeerId([9; 32]), &control(&format!(r#"{{"t":"cancel","id":"{w}"}}"#)));
    assert_eq!(p.a.info(TransferId(1)).unwrap().state, State::Sending);
    // A resume from a piece never read fails the transfer, and the receiver is told.
    say(&mut p.a, format!(r#"{{"t":"accept","id":"{w}","have":4}}"#));
    assert_eq!(p.a.info(TransferId(1)).unwrap().state, State::Failed);
}

#[test]
fn a_resume_for_a_transfer_gone_or_done() {
    let mut p = Pair::new();
    let (_, eb, _) = transfer(&mut p, &data(10));
    let Some(End::Done { sha256 }) = ended(&eb) else { panic!() };
    let w = p.b.get(TransferId(1)).unwrap().wire.hex();
    p.b.receive_frame(A, &control(&format!(r#"{{"t":"resume","id":"{w}"}}"#)));
    p.b.receive_frame(A, &control(&format!(r#"{{"t":"resume","id":"{ID}"}}"#)));
    p.b.poll(p.now, &mut p.lb);
    assert_eq!(
        sent_texts(&mut p.lb),
        [
            format!(r#"{{"t":"done","id":"{w}","ok":true,"sha256":"{sha256}"}}"#),
            format!(r#"{{"t":"cancel","id":"{ID}"}}"#)
        ]
    );
}

#[test]
fn the_outbox_is_bounded() {
    let mut b = Files::with_seed([2; 32]);
    b.connected(A);
    for i in 0..MAX_OUTBOX + 10 {
        b.receive_frame(A, &control(&format!(r#"{{"t":"resume","id":"{i:032x}"}}"#)));
    }
    assert_eq!(b.outbox[&A].len(), MAX_OUTBOX);
    assert_eq!(b.stats().outbox_dropped, 10);
}

#[test]
fn what_goes_out_is_what_files_js_writes() {
    let o = Offer::new(wid(), "a \"b\"\\c.txt", 600000, "Text/Plain");
    assert_eq!(
        String::from_utf8(o.encode()[1..].to_vec()).unwrap(),
        format!(
            r#"{{"t":"offer","v":1,"id":"{ID}","name":"a _b__c.txt","size":600000,"type":"text/plain","piece":524288,"count":2}}"#
        )
    );
    let m = frame::encode_control(&Message::Done { id: wid(), ok: true, sha256: "ab".into() });
    assert_eq!(&m[1..], format!(r#"{{"t":"done","id":"{ID}","ok":true,"sha256":"ab"}}"#).as_bytes());
    let mut s = String::new();
    json::write_str(&mut s, "\u{1}\u{8}\t\n\u{c}\r\u{1f}\"\\é😀");
    assert_eq!(s, r#""\u0001\b\t\n\f\r\u001f\"\\é😀""#);
    // Room messages: the bytes tag, then the frame.
    assert_eq!(room_message(&[2, 3]), [0, 2, 3]);
    assert_eq!(share_frame(&[0, 1, 9]), Some(&[1u8, 9][..]));
    assert_eq!(share_frame(&[0, 3]), None);
    assert_eq!(share_frame(&[7, 2]), None);
    assert_eq!(share_frame(&[0]), None);
}

#[test]
fn file_sink_keeps_only_verified_files_and_never_replaces_one() {
    let dir = std::env::temp_dir().join(format!("pw-files-{}-{}", std::process::id(), line!()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), b"mine").unwrap();
    let mut s = FileSink::create(&dir, "a.txt").unwrap();
    s.write(b"theirs").unwrap();
    s.finish().unwrap();
    assert_eq!(s.saved_as().unwrap(), dir.join("a (2).txt"));
    assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"mine");
    assert_eq!(std::fs::read(dir.join("a (2).txt")).unwrap(), b"theirs");
    let mut s = FileSink::create(&dir, "b").unwrap();
    s.write(b"x").unwrap();
    s.abort();
    drop(s);
    {
        let _dropped = FileSink::create(&dir, "c").unwrap();
    }
    let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    assert_eq!(names, ["a (2).txt", "a.txt"]);
    // A long name is cut to fit a file system, its extension kept.
    let long = format!("{}.tar.gz", "é".repeat(150));
    let mut s = FileSink::create(&dir, &long).unwrap();
    s.finish().unwrap();
    let kept = s.saved_as().unwrap().file_name().unwrap().to_string_lossy().into_owned();
    assert!(kept.len() <= 200 && kept.ends_with("é.gz"), "{kept}");
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------- the golden vectors files.js makes (packages/js/test/rust_files.mjs) ----------

mod vectors {
    use super::super::frame::{self, Message, Refused};
    use super::super::json::Value;
    use super::super::{Offer, WireId, PIECE};
    use serde_json::Value as J;

    fn load(name: &str) -> J {
        let path = format!("{}/tests/vectors/files/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str(&text).unwrap()
    }

    fn unhex(s: &J) -> Vec<u8> {
        let s = s.as_str().unwrap();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn units(s: &J) -> Vec<u16> {
        let s = s.as_str().unwrap();
        (0..s.len()).step_by(4).map(|i| u16::from_str_radix(&s[i..i + 4], 16).unwrap()).collect()
    }

    // A JS value as rust_files.mjs writes it.
    fn value(v: &J) -> Value {
        let v = v.as_array().unwrap();
        match v[0].as_str().unwrap() {
            "u" => Value::Absent,
            "n" => Value::Null,
            "b" => Value::Bool(v[1].as_bool().unwrap()),
            "d" => Value::Num(f64::from_bits(u64::from_str_radix(v[1].as_str().unwrap(), 16).unwrap())),
            "s" => Value::Str(units(&v[1])),
            "o" => Value::Other,
            k => panic!("{k}"),
        }
    }

    fn same(a: &Value, b: &Value) -> bool {
        match (a, b) {
            // NaN can't come out of JSON; -0 and 0 are told apart by their bits.
            (Value::Num(x), Value::Num(y)) => x.to_bits() == y.to_bits(),
            _ => a == b,
        }
    }

    #[test]
    fn names_and_types_clean_as_files_js_cleans_them() {
        let v = load("names.json");
        let mut misses = Vec::new();
        for pair in v["names"].as_array().unwrap() {
            let raw = units(&pair[0]);
            let got = frame::clean_name_utf16(&raw);
            if got != pair[1].as_str().unwrap() {
                misses.push(format!("{:?} -> {got:?}, files.js {:?}", String::from_utf16_lossy(&raw), pair[1]));
            }
        }
        for pair in v["types"].as_array().unwrap() {
            let raw = units(&pair[0]);
            let got = frame::clean_type_utf16(&raw);
            if got != pair[1].as_str().unwrap() {
                misses.push(format!("type {:?} -> {got:?}, files.js {:?}", String::from_utf16_lossy(&raw), pair[1]));
            }
        }
        assert!(misses.is_empty(), "{} misses:\n{}", misses.len(), misses.join("\n"));
    }

    #[test]
    fn control_frames_read_as_files_js_reads_them() {
        let v = load("controls.json");
        let mut misses = Vec::new();
        for c in v.as_array().unwrap() {
            let f = unhex(&c["f"]);
            let got = frame::decode_control(&f);
            let ok = match (&got, c["m"].as_array()) {
                (None, None) => true,
                (Some(m), Some(want)) => {
                    want.iter().zip(m.0 .0.iter()).all(|(w, g)| same(&value(w), g))
                        && match (frame::parse_offer(m), c["offer"].as_object()) {
                            (None, None) => true,
                            (Some(o), Some(w)) => {
                                o.id.hex() == w["id"].as_str().unwrap()
                                    && o.name == w["name"].as_str().unwrap()
                                    && o.size == w["size"].as_u64().unwrap()
                                    && o.mime == w["type"].as_str().unwrap()
                                    && o.piece as u64 == w["piece"].as_u64().unwrap()
                                    && o.count == w["count"].as_u64().unwrap()
                            }
                            _ => false,
                        }
                }
                _ => false,
            };
            if !ok {
                misses.push(format!(
                    "{:?}: {got:?}, files.js {} / {}",
                    String::from_utf8_lossy(&f),
                    c["m"],
                    c["offer"]
                ));
            }
        }
        assert!(misses.is_empty(), "{} misses:\n{}", misses.len(), misses.join("\n"));
    }

    #[test]
    fn pieces_check_and_resume_as_files_js_does() {
        let v = load("pieces.json");
        for c in v["checks"].as_array().unwrap() {
            let n = |i: usize| c[i].as_u64().unwrap();
            let got = frame::check_piece(
                n(0),
                n(1) as u32,
                frame::piece_count(n(0), n(1) as u32),
                n(2),
                n(3) as u32,
                n(4) as usize,
            );
            let want = match c[5].as_str().unwrap() {
                "ok" => Ok(()),
                "out-of-range" => Err(Refused::OutOfRange),
                "duplicate" => Err(Refused::Duplicate),
                "gap" => Err(Refused::Gap),
                "bad-length" => Err(Refused::BadLength),
                k => panic!("{k}"),
            };
            assert_eq!(got, want, "{c}");
        }
        for c in v["resumes"].as_array().unwrap() {
            let got = frame::resume_at(c[0].as_u64().unwrap(), c[1].as_u64().unwrap(), &value(&c[2]));
            assert_eq!(got, c[3].as_u64(), "{c}");
        }
        for c in v["frames"].as_array().unwrap() {
            let f = unhex(&c[0]);
            let got =
                frame::decode_piece(&f).map(|p| (p.id.hex(), p.index as u64, frame::hex(p.hash), frame::hex(p.data)));
            let want = c[1].as_array().map(|w| {
                (
                    w[0].as_str().unwrap().to_string(),
                    w[1].as_u64().unwrap(),
                    w[2].as_str().unwrap().into(),
                    w[3].as_str().unwrap().into(),
                )
            });
            assert_eq!(got, want, "{c}");
        }
    }

    #[test]
    fn frames_sent_are_what_files_js_makes() {
        for c in load("out.json").as_array().unwrap() {
            let want = unhex(c.as_array().unwrap().last().unwrap());
            let got = match c[0].as_str().unwrap() {
                "offer" => {
                    let id = WireId(unhex(&c[4]).try_into().unwrap());
                    let size = c[3].as_u64().unwrap();
                    let o = Offer {
                        id,
                        name: frame::clean_name_utf16(&units(&c[1])),
                        size,
                        mime: frame::clean_type_utf16(&units(&c[2])),
                        piece: PIECE,
                        count: frame::piece_count(size, PIECE),
                    };
                    // Offer::new, for a name Rust can hold (no lone surrogates), is the same.
                    if let Ok(name) = String::from_utf16(&units(&c[1])) {
                        let mime = String::from_utf16(&units(&c[2])).unwrap();
                        assert_eq!(Offer::new(id, &name, size, &mime), o);
                    }
                    o.encode()
                }
                "control" => {
                    let m: J = serde_json::from_str(c[1].as_str().unwrap()).unwrap();
                    let id = WireId::parse(&m["id"].as_str().unwrap().encode_utf16().collect::<Vec<_>>()).unwrap();
                    let have = m["have"].as_u64().unwrap_or(0);
                    let sha256 = m["sha256"].as_str().unwrap_or("").to_string();
                    frame::encode_control(&match m["t"].as_str().unwrap() {
                        "accept" => Message::Accept { id, have },
                        "decline" => Message::Decline { id },
                        "cancel" => Message::Cancel { id },
                        "ack" => Message::Ack { id, have },
                        "end" => Message::End { id, sha256 },
                        "done" => Message::Done { id, ok: m["ok"].as_bool().unwrap(), sha256 },
                        "resume" => Message::Resume { id },
                        t => panic!("{t}"),
                    })
                }
                "piece" => {
                    let id = WireId::parse(&c[1].as_str().unwrap().encode_utf16().collect::<Vec<_>>()).unwrap();
                    let hash: [u8; 32] = unhex(&c[3]).try_into().unwrap();
                    frame::encode_piece(id, c[2].as_u64().unwrap() as u32, &hash, &unhex(&c[4]))
                }
                k => panic!("{k}"),
            };
            assert_eq!(got, want, "{c}");
        }
    }

    #[test]
    fn the_whole_file_hash_is_files_js_streaming_sha256() {
        use sha2::{Digest, Sha256};
        for c in load("sha.json").as_array().unwrap() {
            let data = unhex(&c[0]);
            // Fed in pieces of 7 bytes, as a stream would.
            let mut h = Sha256::new();
            for part in data.chunks(7) {
                h.update(part);
            }
            assert_eq!(frame::hex(&h.finalize()), c[1].as_str().unwrap());
        }
    }
}
