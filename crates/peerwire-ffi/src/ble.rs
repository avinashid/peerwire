//! Bluetooth LE over the C ABI (feature `ble`): one connection's protocol, [`peerwire_core::ble::Link`], for a host
//! whose own stack moves the GATT values, such as a browser's Web Bluetooth (`packages/js/src/bluetooth.js`). The
//! default wasm build leaves it off; `peerwire_ffi_ble.wasm` has it.
//!
//! The host makes a `PwBle` for each connection, beside its endpoint, with 16 random bytes for its challenge and the
//! endpoint handle the connection's datagrams take. It hands it each value that came (`pw_ble_push`), sends what
//! `pw_ble_poll_value` gives (as the central, a write without response to `rx`), calls `pw_ble_timeout` by the time it
//! gives, and reads its events.
//!
//! - Once the room is proven the link sends the endpoint's identity proof itself.
//! - Once the other end's identity proof checks out (PEER), its datagrams go to the endpoint as from that peer alone
//!   (`Endpoint::handle_datagram_from`), and the host sends the endpoint's datagrams for the handle with
//!   `pw_ble_datagram`. Until then no datagram passes either way.
//! - Signals go both ways: inner kind 1 (a room hello) or 2 (a message sealed to the receiver), with `pw_ble_signal`
//!   and the SIGNAL event. The host checks them as it would from any carrier.
//! - On CLOSED the host closes the connection and frees the `PwBle`; it takes nothing more.
//!
//! Events, from `pw_ble_poll_event` (its length, 0 if none, or `-(needed)` if the buffer is too small):
//! ```text
//! 1 PROVEN                    the room proof completed, and this end's identity proof went
//! 2 PEER     peer(32)         the other end proved, on this connection, that it is `peer`
//! 3 SIGNAL   inner(u8) payload
//! 4 CLOSED   reason(u8)       0 a wrong answer (the central isn't in the room), 1 a wrong proof (the peripheral
//!                             isn't), 2 something else came before the proof, 3 too many messages dropped or
//!                             refused, 4 a wrong identity proof, 5 not proven, with the peer's identity, in 3 s
//! ```

use crate::PW_ERR_WOULD_BLOCK;
use crate::{bytes, now, writable, PwEndpoint, PW_ERR_INVALID, PW_ERR_NOT_CONNECTED, PW_ERR_TOO_LARGE};
use peerwire_core::ble::{
    ble_key, identity, Fault, Input, Link, CHALLENGE_LEN, INNER_ID, INNER_PEER, INNER_ROOM, PROVE_WITHIN,
};
use peerwire_core::{PeerId, MAX_DATAGRAM};
use std::collections::VecDeque;
use std::slice;

pub const PW_BLE_PROVEN: u8 = 1;
pub const PW_BLE_PEER: u8 = 2;
pub const PW_BLE_SIGNAL: u8 = 3;
pub const PW_BLE_CLOSED: u8 = 4;
/// CLOSED's reasons beyond the link's own ([`Fault`]).
const WRONG_ID: u8 = 4;
const LATE: u8 = 5;

/// Values queued to send: the largest datagram takes 65 at the smallest ATT MTU, and a signal or two may wait too.
const QUEUE: usize = 256;
/// Events waiting for the host: each value brings one at most, so this holds a host that reads after every push.
const MAX_EVENTS: usize = 64;
/// The longest signal payload: one message holds a kind byte, `seq`, the inner kind and GCM's tag around it.
const MAX_SIGNAL: usize = peerwire_core::ble::MAX_MESSAGE - 1 - 8 - 1 - 16;

pub struct PwBle {
    link: Link,
    /// The endpoint handle its datagrams take.
    addr: u32,
    /// The peer the other end proved it is.
    peer: Option<PeerId>,
    events: VecDeque<Vec<u8>>,
    closed: bool,
}

impl PwBle {
    fn event(&mut self, e: Vec<u8>) {
        if self.events.len() < MAX_EVENTS {
            self.events.push_back(e);
        }
    }

    fn close(&mut self, reason: u8) {
        if !self.closed {
            self.closed = true;
            // Past MAX_EVENTS too: the host has to see that the connection closed (NB110d-I2).
            self.events.push_back(vec![PW_BLE_CLOSED, reason]);
        }
    }

    /// Counts `n` messages dropped or refused, and closes at the link's limit.
    fn count(&mut self, n: u64) {
        if self.link.count(n) {
            self.close(reason(Fault::Drops));
        }
    }

    /// The other end's identity proof: the first one names its peer if it checks out, and closes the connection if
    /// it doesn't (or is in this end's own name); another is dropped.
    fn on_id(&mut self, me: &PeerId, proof: &[u8]) {
        match identity(proof).filter(|_| self.peer.is_none()) {
            Some((peer, sig)) if peer != *me && self.link.verifies(&peer, &sig) => {
                self.peer = Some(peer);
                self.event([&[PW_BLE_PEER][..], peer.as_bytes()].concat());
            }
            Some(_) => self.close(WRONG_ID),
            None => self.count(1),
        }
    }
}

fn reason(f: Fault) -> u8 {
    match f {
        Fault::WrongAnswer => 0,
        Fault::WrongProof => 1,
        Fault::Early => 2,
        Fault::Drops => 3,
    }
}

unsafe fn text<'a>(p: *const u8, len: usize) -> Option<&'a str> {
    std::str::from_utf8(bytes(p, len)?).ok()
}

/// A connection's protocol, at the central's end (`central` 1) or the peripheral's (0), made at `now_us`, in room
/// `room` of app `app` (UTF-8), with `challenge` (16 random bytes, from a CSPRNG), ATT MTU `mtu` (23 if the host can't
/// tell; values are cut to it) and endpoint handle `addr`. The peripheral's challenge is queued at once. Null for a
/// null or short input, or a name that isn't UTF-8.
#[no_mangle]
pub unsafe extern "C" fn pw_ble_new(
    app: *const u8,
    app_len: usize,
    room: *const u8,
    room_len: usize,
    central: u32,
    challenge: *const u8,
    mtu: u32,
    now_us: u64,
    addr: u32,
) -> *mut PwBle {
    let (Some(app), Some(room), Some(c)) = (text(app, app_len), text(room, room_len), bytes(challenge, CHALLENGE_LEN))
    else {
        return std::ptr::null_mut();
    };
    let (key, c) = (ble_key(app, room), c.try_into().expect("16 bytes"));
    let mtu = u16::try_from(mtu).unwrap_or(u16::MAX);
    let link = match central != 0 {
        true => Link::central(key, c, mtu, QUEUE, now(now_us)),
        false => Link::peripheral(key, c, mtu, QUEUE, now(now_us)),
    };
    Box::into_raw(Box::new(PwBle { link, addr, peer: None, events: VecDeque::new(), closed: false }))
}

#[no_mangle]
pub unsafe extern "C" fn pw_ble_free(b: *mut PwBle) {
    if !b.is_null() {
        drop(Box::from_raw(b));
    }
}

/// The ATT MTU changed.
#[no_mangle]
pub unsafe extern "C" fn pw_ble_set_mtu(b: *mut PwBle, mtu: u32) {
    if let Some(b) = b.as_mut() {
        b.link.set_mtu(u16::try_from(mtu).unwrap_or(u16::MAX));
    }
}

/// Takes one GATT value that came at `now_us`, for the connection beside endpoint `ep`: the room proof runs as its
/// messages come, and a datagram from the proven peer goes to `ep` (pump it after). Returns 0, or PW_ERR_INVALID for
/// a null input. Nothing is taken once closed.
#[no_mangle]
pub unsafe extern "C" fn pw_ble_push(
    b: *mut PwBle,
    ep: *mut PwEndpoint,
    now_us: u64,
    value: *const u8,
    len: usize,
) -> i32 {
    let (Some(b), Some(h), Some(value)) = (b.as_mut(), ep.as_mut(), bytes(value, len)) else { return PW_ERR_INVALID };
    if b.closed {
        return 0;
    }
    let t = now(now_us);
    match b.link.push(t, value) {
        Input::Nothing => {}
        Input::Proven => {
            b.link.send_identity(&h.identity);
            b.event(vec![PW_BLE_PROVEN]);
        }
        Input::Signal(INNER_ID, proof) => b.on_id(&h.identity.peer_id(), &proof),
        Input::Signal(inner @ (INNER_ROOM | INNER_PEER), payload) => {
            b.event([&[PW_BLE_SIGNAL, inner][..], &payload].concat())
        }
        Input::Signal(..) => b.count(1),
        Input::Datagram(d) => match b.peer {
            Some(p) if b.link.take_bound(t, &d) => {
                let before = h.ep.stats().rx_wrong_peer;
                h.ep.handle_datagram_from(t, b.addr, &p, &d);
                let wrong = h.ep.stats().rx_wrong_peer - before;
                b.count(wrong);
            }
            _ => b.count(1),
        },
        Input::Close(f) => b.close(reason(f)),
    }
    0
}

/// Closes the connection (CLOSED, reason 5) if, at `now_us`, it hasn't proven the room and the peer's identity within
/// 3 s of opening. Returns when to call again, in µs on the same clock, or -1 if there is no need (proven, or closed).
#[no_mangle]
pub unsafe extern "C" fn pw_ble_timeout(b: *mut PwBle, now_us: u64) -> i64 {
    let Some(b) = b.as_mut() else { return -1 };
    if b.closed || b.peer.is_some() {
        return -1;
    }
    let deadline = b.link.opened() + PROVE_WITHIN;
    if now(now_us) > deadline {
        b.close(LATE);
        return -1;
    }
    // Just past the deadline, so that the call then closes it; never negative, on any host clock (NB110d-I2).
    deadline.as_micros().saturating_add(1).min(i64::MAX as u64) as i64
}

/// Seals `payload` under the link key as this end's next signal of inner kind `inner` (1 a room hello, 2 a message
/// sealed to the receiver), and queues it. Returns 0, PW_ERR_NOT_CONNECTED before the room is proven (or once closed),
/// PW_ERR_TOO_LARGE over 1,207 bytes, PW_ERR_WOULD_BLOCK with the queue full, or PW_ERR_INVALID for another kind.
#[no_mangle]
pub unsafe extern "C" fn pw_ble_signal(b: *mut PwBle, inner: u32, payload: *const u8, len: usize) -> i32 {
    let (Some(b), Some(payload)) = (b.as_mut(), bytes(payload, len)) else { return PW_ERR_INVALID };
    let inner = match u8::try_from(inner) {
        Ok(i @ (INNER_ROOM | INNER_PEER)) => i,
        _ => return PW_ERR_INVALID,
    };
    if b.closed || !b.link.proven() {
        return PW_ERR_NOT_CONNECTED;
    }
    if payload.len() > MAX_SIGNAL {
        return PW_ERR_TOO_LARGE;
    }
    match b.link.signal(inner, payload) {
        true => 0,
        false => PW_ERR_WOULD_BLOCK,
    }
}

/// Queues a datagram the endpoint sent to this connection's handle. Returns 0, PW_ERR_NOT_CONNECTED until the other
/// end proved who it is (or once closed), PW_ERR_TOO_LARGE for an empty one or one over 1,232 bytes, or
/// PW_ERR_WOULD_BLOCK with the queue full (it is dropped, as a datagram may be: the endpoint resends).
#[no_mangle]
pub unsafe extern "C" fn pw_ble_datagram(b: *mut PwBle, data: *const u8, len: usize) -> i32 {
    let (Some(b), Some(data)) = (b.as_mut(), bytes(data, len)) else { return PW_ERR_INVALID };
    if b.closed || b.peer.is_none() {
        return PW_ERR_NOT_CONNECTED;
    }
    if data.is_empty() || data.len() > MAX_DATAGRAM {
        return PW_ERR_TOO_LARGE;
    }
    match b.link.datagram(data) {
        true => 0,
        false => PW_ERR_WOULD_BLOCK,
    }
}

/// Where the connection stands: 0 the room proof is under way (the central waits for the challenge, or the peripheral
/// for the answer), 1 the central answered and waits for the proof, 2 the room is proven, 3 the other end proved who
/// it is too, 4 closed. A connection the peripheral closes at 1 most likely has another room's name.
#[no_mangle]
pub unsafe extern "C" fn pw_ble_state(b: *const PwBle) -> i32 {
    let Some(b) = b.as_ref() else { return PW_ERR_INVALID };
    match (b.closed, b.peer.is_some(), b.link.proven(), b.link.is_central() && b.link.challenged()) {
        (true, ..) => 4,
        (_, true, ..) => 3,
        (_, _, true, _) => 2,
        (_, _, _, true) => 1,
        _ => 0,
    }
}

/// Writes the next GATT value to send into `buf` and takes it off the queue. Returns its length (20 bytes at most at
/// ATT MTU 23), 0 if none, or `-(needed)` if `buf` is too small (it stays queued).
#[no_mangle]
pub unsafe extern "C" fn pw_ble_poll_value(b: *mut PwBle, buf: *mut u8, cap: usize) -> i32 {
    let (Some(b), Some(buf)) = (b.as_mut(), writable(buf, cap)) else { return PW_ERR_INVALID };
    let Some(v) = b.link.value() else { return 0 };
    if v.len() > cap {
        return -(v.len() as i32);
    }
    let n = v.len();
    slice::from_raw_parts_mut(buf, n).copy_from_slice(v);
    b.link.sent();
    n as i32
}

/// Writes the next event into `buf` (see the module's docs). Returns its length, 0 if none, or `-(needed)` if `buf`
/// is too small (it stays queued).
#[no_mangle]
pub unsafe extern "C" fn pw_ble_poll_event(b: *mut PwBle, buf: *mut u8, cap: usize) -> i32 {
    let (Some(b), Some(buf)) = (b.as_mut(), writable(buf, cap)) else { return PW_ERR_INVALID };
    let Some(e) = b.events.front() else { return 0 };
    if e.len() > cap {
        return -(e.len() as i32);
    }
    let n = e.len();
    slice::from_raw_parts_mut(buf, n).copy_from_slice(e);
    b.events.pop_front();
    n as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{pw_connect, pw_endpoint_free, pw_endpoint_new, pw_poll_event, pw_poll_transmit, pw_send};

    struct Ep(*mut PwEndpoint, [u8; 32]);

    impl Ep {
        fn new(seed: u8) -> Ep {
            let mut id = [0u8; 32];
            let h = unsafe { pw_endpoint_new(std::ptr::null(), [seed; 32].as_ptr(), 0, id.as_mut_ptr()) };
            assert!(!h.is_null());
            Ep(h, id)
        }

        /// The datagrams it has to send, by handle.
        fn transmit(&self, t: u64) -> Vec<(u32, Vec<u8>)> {
            let mut out = Vec::new();
            let (mut addr, mut buf) = (0u32, vec![0u8; MAX_DATAGRAM]);
            loop {
                let n = unsafe { pw_poll_transmit(self.0, t, &mut addr, buf.as_mut_ptr(), buf.len()) };
                if n <= 0 {
                    return out;
                }
                out.push((addr, buf[..n as usize].to_vec()));
            }
        }

        fn events(&self) -> Vec<Vec<u8>> {
            let mut buf = vec![0u8; 4096];
            std::iter::from_fn(|| {
                let n = unsafe { pw_poll_event(self.0, buf.as_mut_ptr(), buf.len()) };
                (n > 0).then(|| buf[..n as usize].to_vec())
            })
            .collect()
        }
    }

    impl Drop for Ep {
        fn drop(&mut self) {
            unsafe { pw_endpoint_free(self.0) }
        }
    }

    struct Ble(*mut PwBle);

    impl Ble {
        fn new(room: &str, central: bool, challenge: u8, addr: u32) -> Ble {
            let (app, ch) = ("app", [challenge; CHALLENGE_LEN]);
            let b = unsafe {
                pw_ble_new(app.as_ptr(), app.len(), room.as_ptr(), room.len(), central as u32, ch.as_ptr(), 23, 0, addr)
            };
            assert!(!b.is_null());
            Ble(b)
        }

        fn values(&self) -> Vec<Vec<u8>> {
            let mut buf = [0u8; 20];
            std::iter::from_fn(|| {
                let n = unsafe { pw_ble_poll_value(self.0, buf.as_mut_ptr(), buf.len()) };
                assert!(n >= 0, "a value over 20 bytes at ATT MTU 23");
                (n > 0).then(|| buf[..n as usize].to_vec())
            })
            .collect()
        }

        fn events(&self) -> Vec<Vec<u8>> {
            let mut buf = vec![0u8; 2048];
            std::iter::from_fn(|| {
                let n = unsafe { pw_ble_poll_event(self.0, buf.as_mut_ptr(), buf.len()) };
                (n > 0).then(|| buf[..n as usize].to_vec())
            })
            .collect()
        }

        fn push(&self, ep: &Ep, t: u64, values: &[Vec<u8>]) {
            for v in values {
                assert_eq!(unsafe { pw_ble_push(self.0, ep.0, t, v.as_ptr(), v.len()) }, 0);
            }
        }
    }

    impl Drop for Ble {
        fn drop(&mut self) {
            unsafe { pw_ble_free(self.0) }
        }
    }

    /// Moves values both ways, and each endpoint's datagrams for handle 1 into its link, until nothing moves.
    fn run(c: (&Ble, &Ep), p: (&Ble, &Ep), t: u64) {
        loop {
            let mut moved = false;
            for (from, to) in [(c, p), (p, c)] {
                for (addr, d) in from.1.transmit(t) {
                    assert_eq!(addr, 1);
                    unsafe { pw_ble_datagram(from.0 .0, d.as_ptr(), d.len()) };
                }
                let values = from.0.values();
                moved |= !values.is_empty();
                to.0.push(to.1, t, &values);
            }
            if !moved {
                return;
            }
        }
    }

    #[test]
    fn a_central_and_a_peripheral_prove_the_room_and_who_they_are_then_link_and_carry_signals() {
        let (ec, ep) = (Ep::new(1), Ep::new(2));
        let (c, p) = (Ble::new("room", true, 9, 1), Ble::new("room", false, 3, 1));
        let d = [7u8; 40];
        assert_eq!(unsafe { pw_ble_datagram(c.0, d.as_ptr(), d.len()) }, PW_ERR_NOT_CONNECTED, "before the proof");
        assert_eq!(unsafe { pw_ble_signal(c.0, 1, d.as_ptr(), d.len()) }, PW_ERR_NOT_CONNECTED);
        assert_eq!(unsafe { (pw_ble_state(c.0), pw_ble_state(p.0)) }, (0, 0));
        c.push(&ec, 0, &p.values());
        assert_eq!(unsafe { pw_ble_state(c.0) }, 1, "answered");
        run((&c, &ec), (&p, &ep), 0);
        assert_eq!(unsafe { (pw_ble_state(c.0), pw_ble_state(p.0)) }, (3, 3));
        assert_eq!(c.events(), vec![vec![PW_BLE_PROVEN], [&[PW_BLE_PEER][..], &ep.1].concat()]);
        assert_eq!(p.events(), vec![vec![PW_BLE_PROVEN], [&[PW_BLE_PEER][..], &ec.1].concat()]);
        assert_eq!(unsafe { pw_ble_timeout(c.0, 10_000_000) }, -1, "proven: no deadline");
        // Signals both ways; an identity proof can't be sent as one.
        assert_eq!(unsafe { pw_ble_signal(c.0, 1, b"hello".as_ptr(), 5) }, 0);
        assert_eq!(unsafe { pw_ble_signal(p.0, 2, b"offer".as_ptr(), 5) }, 0);
        assert_eq!(unsafe { pw_ble_signal(c.0, 3, b"x".as_ptr(), 1) }, PW_ERR_INVALID);
        let big = vec![0u8; MAX_SIGNAL + 1];
        assert_eq!(unsafe { pw_ble_signal(c.0, 1, big.as_ptr(), big.len()) }, PW_ERR_TOO_LARGE);
        run((&c, &ec), (&p, &ep), 0);
        assert_eq!(p.events(), vec![[&[PW_BLE_SIGNAL, 1][..], b"hello"].concat()]);
        assert_eq!(c.events(), vec![[&[PW_BLE_SIGNAL, 2][..], b"offer"].concat()]);
        // The central dials the peer it proved, over handle 1, and they link.
        assert_eq!(unsafe { pw_connect(ec.0, 0, 1, ep.1.as_ptr()) }, 0);
        run((&c, &ec), (&p, &ep), 0);
        assert!(ec.events().first().is_some_and(|e| e[0] == 1 && e[1..33] == ep.1), "connected");
        assert!(ep.events().first().is_some_and(|e| e[0] == 1 && e[1..33] == ec.1), "connected");
        let m = b"led on";
        assert_eq!(unsafe { pw_send(ec.0, ep.1.as_ptr(), 0, m.as_ptr(), m.len()) }, 0);
        run((&c, &ec), (&p, &ep), 0);
        assert_eq!(ep.events(), vec![[&[2][..], &ec.1, &[0], m].concat()], "the message, on the reliable lane");
    }

    #[test]
    fn another_rooms_peer_is_closed_at_either_end() {
        let (ec, ep) = (Ep::new(1), Ep::new(2));
        let (c, p) = (Ble::new("other", true, 9, 1), Ble::new("room", false, 3, 1));
        run((&c, &ec), (&p, &ep), 0);
        assert_eq!(p.events(), vec![vec![PW_BLE_CLOSED, 0]], "a wrong answer: the central isn't in the room");
        assert!(c.events().is_empty(), "no proof came");
        // A proof that doesn't check out at the central: the room's own, altered.
        let (c, p) = (Ble::new("room", true, 9, 1), Ble::new("room", false, 3, 1));
        c.push(&ec, 0, &p.values());
        p.push(&ep, 0, &c.values());
        let mut proof = p.values();
        proof[1][5] ^= 1; // in the proof's HMAC, in its second value
        c.push(&ec, 0, &proof);
        assert_eq!(c.events(), vec![vec![PW_BLE_CLOSED, 1]], "a wrong proof");
    }

    #[test]
    fn a_connection_not_proven_in_time_closes_and_takes_nothing_after() {
        let (ec, ep) = (Ep::new(1), Ep::new(2));
        let (c, p) = (Ble::new("room", true, 9, 1), Ble::new("room", false, 3, 1));
        let at = unsafe { pw_ble_timeout(c.0, 0) };
        assert_eq!(at, 3_000_001);
        assert_eq!(unsafe { pw_ble_timeout(c.0, 3_000_000) }, at, "3 s exactly is in time");
        assert_eq!(unsafe { pw_ble_timeout(c.0, 3_000_001) }, -1);
        assert_eq!(c.events(), vec![vec![PW_BLE_CLOSED, 5]]);
        assert_eq!(unsafe { pw_ble_state(c.0) }, 4);
        run((&c, &ec), (&p, &ep), 3_000_001);
        assert!(c.events().is_empty() && c.values().is_empty(), "nothing taken once closed");
    }

    /// NB110d-I2: a host clock near the top of its range gets a deadline, not a wrap to 0 or a negative.
    #[test]
    fn the_deadline_holds_at_the_top_of_the_clock() {
        let (app, room, ch) = ("app", "room", [9u8; CHALLENGE_LEN]);
        for at in [u64::MAX - 1_000_000, i64::MAX as u64 - 1_000_000] {
            let b = Ble(unsafe { pw_ble_new(app.as_ptr(), 3, room.as_ptr(), 4, 1, ch.as_ptr(), 23, at, 1) });
            assert!(!b.0.is_null());
            assert_eq!(unsafe { pw_ble_timeout(b.0, at) }, i64::MAX, "opened at {at}");
            assert!(b.events().is_empty(), "still open");
        }
    }

    /// NB110d-I2: CLOSED reaches a host that let the events pile up past MAX_EVENTS.
    #[test]
    fn closed_is_queued_past_the_event_limit() {
        let c = Ble::new("room", true, 9, 1);
        for _ in 0..MAX_EVENTS {
            unsafe { &mut *c.0 }.event(vec![PW_BLE_PROVEN]);
        }
        unsafe { &mut *c.0 }.event(vec![PW_BLE_PROVEN]);
        assert_eq!(unsafe { pw_ble_timeout(c.0, 3_000_001) }, -1);
        let events = c.events();
        assert_eq!(events.len(), MAX_EVENTS + 1, "the one past the limit dropped, CLOSED kept");
        assert_eq!(events.last(), Some(&vec![PW_BLE_CLOSED, 5]));
    }

    #[test]
    fn an_identity_proof_in_this_ends_own_name_closes_it() {
        // Both ends with the same identity: each sees a proof in its own name.
        let (ec, ep) = (Ep::new(1), Ep::new(1));
        let (c, p) = (Ble::new("room", true, 9, 1), Ble::new("room", false, 3, 1));
        run((&c, &ec), (&p, &ep), 0);
        assert_eq!(c.events(), vec![vec![PW_BLE_PROVEN], vec![PW_BLE_CLOSED, WRONG_ID]]);
    }

    #[test]
    fn null_and_short_inputs_are_refused() {
        unsafe {
            let (app, room, ch) = ("app", "room", [0u8; 16]);
            assert!(pw_ble_new(app.as_ptr(), 3, room.as_ptr(), 4, 1, std::ptr::null(), 23, 0, 1).is_null());
            let bad = [0xffu8];
            assert!(pw_ble_new(bad.as_ptr(), 1, room.as_ptr(), 4, 1, ch.as_ptr(), 23, 0, 1).is_null(), "not UTF-8");
            let b = std::ptr::null_mut();
            assert_eq!(pw_ble_push(b, std::ptr::null_mut(), 0, ch.as_ptr(), 1), PW_ERR_INVALID);
            assert_eq!(pw_ble_signal(b, 1, ch.as_ptr(), 1), PW_ERR_INVALID);
            assert_eq!(pw_ble_datagram(b, ch.as_ptr(), 1), PW_ERR_INVALID);
            assert_eq!(pw_ble_poll_value(b, std::ptr::null_mut(), 0), PW_ERR_INVALID);
            assert_eq!(pw_ble_poll_event(b, std::ptr::null_mut(), 0), PW_ERR_INVALID);
            assert_eq!(pw_ble_timeout(b, 0), -1);
            pw_ble_set_mtu(b, 23);
            pw_ble_free(b);
            let p = Ble::new("room", false, 3, 1);
            let mut small = [0u8; 4];
            assert_eq!(pw_ble_poll_value(p.0, small.as_mut_ptr(), 4), -18, "the challenge stays queued");
            assert_eq!(p.values().len(), 1);
            let ep = Ep::new(1);
            assert_eq!(pw_ble_push(p.0, ep.0, 0, std::ptr::null(), 1), PW_ERR_INVALID);
        }
    }
}
