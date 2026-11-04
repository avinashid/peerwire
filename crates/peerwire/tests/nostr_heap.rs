//! The heap a Nostr carrier takes for what a relay sends it (NB95-1), and holds for a
//! relay that stops reading (NB95-2), against relays of our own on loopback. One test, as
//! the counting allocator counts the whole process.
use peerwire::{NostrSignaling, Signaling, Topic};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use socket2::{Domain, Socket, Type};
use std::alloc::{GlobalAlloc, Layout, System};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tungstenite::Message;

struct Counting;
static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(n: usize) {
    PEAK.fetch_max(NOW.fetch_add(n, Relaxed) + n, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        grew(l.size());
        unsafe { System.alloc(l) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        NOW.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }

    // Counted as if in place: the peak is a lower bound.
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        if new > l.size() {
            grew(new - l.size());
        } else {
            NOW.fetch_sub(l.size() - new, Relaxed);
        }
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn wait(secs: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// `["EVENT", "s", event]` on `topic` with a valid id and `extra` beside the `p` tag. The
/// carrier checks no signature, so none is made.
fn event(topic: Topic, content: &str, extra: impl IntoIterator<Item = Value>) -> String {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let pubkey = "11".repeat(32);
    let mut tags = vec![json!(["p", topic.to_hex()])];
    tags.extend(extra);
    let id = hex(&Sha256::digest(json!([0, pubkey, 1_700_000_000, 25059, tags, content]).to_string()));
    let ev = json!({"id": id, "pubkey": pubkey, "created_at": 1_700_000_000, "kind": 25059, "tags": tags,
                    "content": content, "sig": "22".repeat(64)});
    json!(["EVENT", "s", ev]).to_string()
}

/// A relay of our own: it takes the handshake and the REQ, then sends what it is given.
/// With `reads` false it never reads again, with a small receive buffer, so writes to it
/// soon block.
fn relay(reads: bool) -> (String, mpsc::Sender<String>) {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    if !reads {
        socket.set_recv_buffer_size(4096).unwrap();
    }
    socket.bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into()).unwrap();
    socket.listen(1).unwrap();
    let listener: TcpListener = socket.into();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        let mut ws = tungstenite::accept(s).unwrap();
        let _req = ws.read();
        if !reads {
            std::thread::sleep(Duration::from_secs(3600));
        }
        ws.get_mut().set_read_timeout(Some(Duration::from_millis(5))).unwrap();
        loop {
            while let Ok(m) = rx.try_recv() {
                ws.send(Message::Text(m)).unwrap();
            }
            match ws.read() {
                Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return,
                Ok(_) => {}
            }
        }
    });
    (url, tx)
}

fn subscribed(url: &str, topic: Topic) -> NostrSignaling {
    let mut carrier = NostrSignaling::new([url]);
    carrier.subscribe(topic);
    assert!(wait(5, || carrier.stats().relays_open == 1));
    std::thread::sleep(Duration::from_millis(100)); // the REQ goes out
    carrier
}

#[test]
fn relays_cant_make_a_carrier_hold_much_heap() {
    let topic = Topic::room("pw-test", "nostr-heap");

    // NB95-1: a message parsed whole took several times its size, 192 KB for 4.5 KB of
    // empty tags; an ESP32 has about 80 KB free.
    let (url, to_carrier) = relay(true);
    let mut carrier = subscribed(&url, topic);
    // NB95-1a: the nested shapes, under 64 values, took up to +65.6 KB and 33 KB of stack.
    let nested = |n, inner: Value, wrap: fn(Value) -> Value| (0..n).fold(inner, |v, _| wrap(v));
    let signal = "A".repeat(1808); // the base64 of 1,356 B, the largest LAN signal
    for (what, msg, delivered, max_peak) in [
        ("an event the size of a signal", event(topic, &signal, []), true, 64 << 10),
        ("4.5 KB of empty tags", event(topic, "QQ==", vec![json!([""]); 900]), false, 32 << 10),
        ("50 nested objects", event(topic, "QQ==", [nested(50, json!(0), |v| json!({ "a": v }))]), false, 32 << 10),
        ("25 objects side by side", event(topic, "QQ==", vec![json!({"a": 0}); 25]), false, 32 << 10),
        ("50 nested arrays", event(topic, "QQ==", [nested(50, json!(0), |v| json!([v]))]), false, 32 << 10),
    ] {
        std::thread::sleep(Duration::from_millis(50));
        let base = NOW.load(Relaxed);
        PEAK.store(base, Relaxed);
        to_carrier.send(msg).unwrap();
        let got = wait(2, || carrier.poll().is_some());
        let peak = PEAK.load(Relaxed) - base;
        println!("{what}: delivered {got}, peak heap +{peak} B");
        assert_eq!(got, delivered, "{what}");
        assert!(peak < max_peak, "{what}: peak heap +{peak} B");
    }
    drop(carrier);

    // NB95-2: with no write timeout the relay thread blocked in its write for good, the
    // relay still counted as open, and every later publish stayed in memory.
    let (url, _to_carrier) = relay(false);
    let mut carrier = subscribed(&url, topic);
    let base = NOW.load(Relaxed);
    let payload = [7u8; 1356];
    for _ in 0..5_000 {
        carrier.publish(topic, &payload); // about 11 MB, past what the sockets buffer
    }
    let start = Instant::now();
    // Each write may wait 5 s, and one that takes a few bytes waits again: 5 to 10 s.
    assert!(wait(20, || carrier.stats().relays_open == 0), "a relay that stopped reading stayed open");
    println!("the stalled relay dropped after {} ms", start.elapsed().as_millis());
    std::thread::sleep(Duration::from_secs(1)); // the relay thread drains its commands
    let held = NOW.load(Relaxed).saturating_sub(base);
    println!("then held +{held} B");
    assert!(held < 1 << 20, "+{held} B held after the stalled relay dropped");
}
