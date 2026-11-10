//! The C ABI's Bluetooth central (`pw_ble_*`), driven as a browser drives it through the wasm
//! (`packages/js/src/bluetooth.js`), against a node, as `ble_led`'s board runs one, over the in-process radios: a
//! central that never advertises proves the room, sends a room hello and dials the peer that proved who it is on the
//! connection, and the two swap messages. Built with `--features ble`.
#![cfg(feature = "ble")]

use aes_gcm::aead::{Aead, KeyInit};
use hmac::{Hmac, Mac};
use peerwire::ble::{ConnId, DeviceId, FakeAir, FakeRadio, Limits, Radio, RadioEvent};
use peerwire::{Event, Lane, Node, PathKind, PeerId};
use peerwire_ffi::ble::{
    pw_ble_datagram, pw_ble_free, pw_ble_new, pw_ble_poll_event, pw_ble_poll_value, pw_ble_push, pw_ble_signal,
    pw_ble_state, pw_ble_timeout, PwBle, PW_BLE_CLOSED, PW_BLE_PEER, PW_BLE_PROVEN, PW_BLE_SIGNAL,
};
use peerwire_ffi::{
    pw_connect, pw_endpoint_free, pw_endpoint_new, pw_handle_timeout, pw_poll_event, pw_poll_transmit, pw_send,
    PwEndpoint,
};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant, SystemTime};

const APP: &str = "pw-test";
const ROOM: &str = "led";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A room hello as the browser makes it (proof.js's proof, nostr.js's seal), in the shape a node's traversal takes:
/// the JSON with its proof under the room key, sealed under the room's hello key.
fn hello(room: &str, me: &[u8; 32]) -> Vec<u8> {
    let name = format!("{APP}/{room}");
    let key: [u8; 32] = Sha256::digest(format!("peerwire/room-key/{name}")).into();
    let enc: [u8; 32] = Sha256::digest(format!("peerwire/room-enc/{name}")).into();
    let ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis() as u64;
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&key).unwrap();
    for part in [&b"pw-hello"[..], me, &[0], &ts.to_le_bytes()] {
        m.update(part);
    }
    let p = hex(&m.finalize().into_bytes());
    let body = format!(r#"{{"t":"hello","from":"{}","meta":{{"nb":1,"role":"peer","ts":{ts},"p":"{p}"}}}}"#, hex(me));
    let nonce = [7u8; 12];
    let ct = aes_gcm::Aes256Gcm::new(&enc.into()).encrypt(&nonce.into(), body.as_bytes()).unwrap();
    [&nonce[..], &ct].concat()
}

/// The browser: its endpoint, a radio that never advertises, and the C ABI's link on its one connection.
struct Browser {
    ep: *mut PwEndpoint,
    id: [u8; 32],
    radio: FakeRadio,
    room: &'static str,
    /// It sends a room hello once the room is proven.
    hellos: bool,
    conn: Option<ConnId>,
    ble: *mut PwBle,
    start: Instant,
    /// The peer the other end proved it is.
    peer: Option<[u8; 32]>,
    connected: bool,
    got: Vec<Vec<u8>>,
    /// CLOSED's reason, or the radio's disconnect (with the link's state then).
    closed: Option<u8>,
    gone_at: Option<i32>,
    /// The largest value it sent.
    widest: usize,
}

impl Browser {
    fn new(air: &FakeAir, room: &'static str, hellos: bool) -> Browser {
        let mut id = [0u8; 32];
        let ep = unsafe { pw_endpoint_new(std::ptr::null(), [5u8; 32].as_ptr(), 0, id.as_mut_ptr()) };
        assert!(!ep.is_null());
        let radio = air.radio(Limits::DESKTOP);
        Browser {
            ep,
            id,
            radio,
            room,
            hellos,
            conn: None,
            ble: std::ptr::null_mut(),
            start: Instant::now(),
            peer: None,
            connected: false,
            got: Vec::new(),
            closed: None,
            gone_at: None,
            widest: 0,
        }
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    /// What the page does each time something comes: the radio's events into the link, the link's events, the
    /// endpoint's datagrams into the link and its values onto the radio.
    fn step(&mut self) {
        let t = self.now();
        while let Some(e) = self.radio.poll() {
            match e {
                RadioEvent::Connected { conn, .. } => {
                    self.conn = Some(conn);
                    let (app, room, ch) = (APP.as_bytes(), self.room.as_bytes(), [9u8; 16]);
                    self.ble = unsafe {
                        pw_ble_new(app.as_ptr(), app.len(), room.as_ptr(), room.len(), 1, ch.as_ptr(), 23, t, 1)
                    };
                }
                RadioEvent::Value { value, .. } => {
                    assert_eq!(unsafe { pw_ble_push(self.ble, self.ep, t, value.as_ptr(), value.len()) }, 0);
                }
                RadioEvent::Disconnected { .. } => {
                    self.gone_at = Some(unsafe { pw_ble_state(self.ble) });
                    self.conn = None;
                }
                _ => {}
            }
        }
        if self.ble.is_null() {
            return;
        }
        let mut buf = vec![0u8; 2048];
        loop {
            let n = unsafe { pw_ble_poll_event(self.ble, buf.as_mut_ptr(), buf.len()) };
            if n <= 0 {
                break;
            }
            match buf[0] {
                PW_BLE_PROVEN if self.hellos => {
                    let h = hello(self.room, &self.id);
                    assert_eq!(unsafe { pw_ble_signal(self.ble, 1, h.as_ptr(), h.len()) }, 0);
                }
                PW_BLE_PEER => {
                    let peer: [u8; 32] = buf[1..33].try_into().unwrap();
                    self.peer = Some(peer);
                    assert_eq!(unsafe { pw_connect(self.ep, t, 1, peer.as_ptr()) }, 0);
                }
                PW_BLE_CLOSED => self.closed = Some(buf[1]),
                // The node's room hello and offers: the browser needs neither, as the peer proved who it is.
                PW_BLE_SIGNAL | PW_BLE_PROVEN => {}
                other => panic!("event {other}"),
            }
        }
        unsafe {
            pw_ble_timeout(self.ble, t);
            pw_handle_timeout(self.ep, t);
            let (mut addr, mut d) = (0u32, vec![0u8; 1232]);
            loop {
                let n = pw_poll_transmit(self.ep, t, &mut addr, d.as_mut_ptr(), d.len());
                if n <= 0 {
                    break;
                }
                assert_eq!(addr, 1);
                pw_ble_datagram(self.ble, d.as_ptr(), n as usize);
            }
            loop {
                let n = pw_poll_event(self.ep, buf.as_mut_ptr(), buf.len());
                if n <= 0 {
                    break;
                }
                match buf[0] {
                    1 => self.connected = true,
                    2 => self.got.push(buf[34..n as usize].to_vec()),
                    _ => {}
                }
            }
            loop {
                let n = pw_ble_poll_value(self.ble, buf.as_mut_ptr(), buf.len());
                if n <= 0 {
                    break;
                }
                self.widest = self.widest.max(n as usize);
                if let Some(conn) = self.conn {
                    self.radio.send(conn, &buf[..n as usize]);
                }
            }
        }
    }

    fn send(&mut self, m: &[u8]) -> i32 {
        unsafe { pw_send(self.ep, self.peer.unwrap().as_ptr(), 0, m.as_ptr(), m.len()) }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        unsafe {
            pw_ble_free(self.ble);
            pw_endpoint_free(self.ep);
        }
    }
}

/// A node with the radio alone, as `ble_led` has it, in `APP`/`ROOM`: radio 0 on `air`.
fn board(air: &FakeAir) -> Node {
    let builder = Node::builder().bluetooth(air.radio(Limits::SMALL)).stun_servers(Vec::<String>::new());
    let mut n = builder.bind("127.0.0.1:0").unwrap();
    n.join_room(APP, ROOM);
    n
}

/// Runs the board, answering `led on` as `ble_led` does, and the browser, until `done` or `secs` run out.
fn run(board: &mut Node, b: &mut Browser, secs: u64, mut done: impl FnMut(&Node, &mut Browser) -> bool) -> Vec<Event> {
    let (end, mut seen) = (Instant::now() + Duration::from_secs(secs), Vec::new());
    while Instant::now() < end && !done(board, b) {
        board.poll();
        while let Some(e) = board.try_recv_event() {
            if let Event::Message { peer, data, .. } = &e {
                if data == b"led on" {
                    board.send(peer, Lane::Reliable, b"led: on").unwrap();
                }
            }
            seen.push(e);
        }
        b.step();
        std::thread::sleep(Duration::from_millis(1));
    }
    seen
}

#[test]
fn a_browsers_central_links_with_a_node_and_they_swap_messages() {
    let air = FakeAir::new(23);
    let mut board = board(&air);
    let mut b = Browser::new(&air, ROOM, true);
    run(&mut board, &mut b, 2, |_, _| air.advert(0).is_some());
    // The device the user picked: the browser connects to it, never advertising itself.
    b.radio.connect(DeviceId(0));
    let mut asked = false;
    let started = Instant::now();
    let seen = run(&mut board, &mut b, 10, |_, b| {
        if b.connected && !asked {
            asked = b.send(b"led on") == 0;
        }
        b.got.iter().any(|m| m == b"led: on")
    });
    assert!(b.got.iter().any(|m| m == b"led: on"), "the board's reply came: {:?}", b.got);
    eprintln!("linked and answered in {:?}", started.elapsed());
    let me = PeerId(b.id);
    assert_eq!(b.peer, Some(board.peer_id().0), "the peer that proved who it is is the board");
    assert!(seen.iter().any(|e| matches!(e, Event::Connected { peer, .. } if *peer == me)));
    assert_eq!(board.path_kind(&me), Some(PathKind::Bluetooth));
    assert_eq!(board.ble_counts(), (1, 1, 1), "one connection, proven and bound to the browser");
    assert!(b.widest <= 20, "every value the browser wrote fits ATT MTU 23: {}", b.widest);
    assert_eq!((b.closed, b.gone_at), (None, None));
}

/// The room hello is what lets the browser in: the node takes links only with members, and a hello is how a member
/// says so. Without one the handshake may complete, but the node lets nothing of the browser's through.
#[test]
fn a_central_that_sends_no_room_hello_isnt_taken() {
    let air = FakeAir::new(23);
    let mut board = board(&air);
    let mut b = Browser::new(&air, ROOM, false);
    run(&mut board, &mut b, 2, |_, _| air.advert(0).is_some());
    b.radio.connect(DeviceId(0));
    let mut asked = false;
    let seen = run(&mut board, &mut b, 3, |_, b| {
        if b.connected && !asked {
            asked = b.send(b"led on") == 0;
        }
        false
    });
    assert!(b.peer.is_some(), "the connection was proven both ways");
    let me = PeerId(b.id);
    let about = |e: &Event| matches!(e, Event::Connected { peer, .. } | Event::Message { peer, .. } if *peer == me);
    assert!(!seen.iter().any(about), "the node shows nothing of a peer that never said it is a member");
    assert!(b.got.is_empty());
}

#[test]
fn another_rooms_central_is_closed_after_its_answer() {
    let air = FakeAir::new(23);
    let mut board = board(&air);
    let mut b = Browser::new(&air, "another room", true);
    run(&mut board, &mut b, 2, |_, _| air.advert(0).is_some());
    b.radio.connect(DeviceId(0));
    run(&mut board, &mut b, 3, |_, b| b.gone_at.is_some());
    assert_eq!(b.gone_at, Some(1), "the node closed it once it had answered: another room's name");
    assert!(b.peer.is_none() && !b.connected);
    assert_eq!(board.ble_counts(), (0, 0, 0));
}
