//! Bluetooth LE framing (`docs/concepts/bluetooth.md`): messages of up to [`MAX_MESSAGE`] bytes in GATT values of as
//! little as 20, and the service's numbers, shared by every host (the Rust node, the ESP32 port, the browser).
//!
//! A message is split into chunks, one per GATT value: a header byte, then as much of the message as the value has
//! room for ([`value_len`]). The header marks the message's first and last chunks and counts the chunks sent on the
//! connection, so the receiver drops a message one of whose chunks a full buffer lost. Sans-IO: the host moves the
//! values and passes the time in.
//!
//! ```
//! use peerwire_core::ble::{Chunker, Reassembler};
//! use peerwire_core::Instant;
//!
//! let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
//! let message = vec![0u8; 300];
//! let mut values = Vec::new();
//! tx.split(&message, 23, |v| {
//!     values.push(v.to_vec()); // each one a write or a notification, 20 bytes at most
//!     true
//! });
//! let mut got = None;
//! for v in &values {
//!     got = rx.push(Instant::ZERO, v).map(<[u8]>::to_vec);
//! }
//! assert_eq!(got, Some(message));
//! ```

use crate::time::Instant;
use crate::wire::MAX_DATAGRAM;
use std::time::Duration;

/// The GATT service a peer serves, as a 128-bit UUID: `1c3eea41-c2be-493c-ab0b-7707ad2dbed6`.
pub const SERVICE_UUID: u128 = 0x1c3eea41_c2be_493c_ab0b_7707ad2dbed6;
/// Its `rx` characteristic: the central writes to it, without response.
pub const RX_UUID: u128 = 0x1c3eea42_c2be_493c_ab0b_7707ad2dbed6;
/// Its `tx` characteristic: the peripheral notifies on it.
pub const TX_UUID: u128 = 0x1c3eea43_c2be_493c_ab0b_7707ad2dbed6;

/// A message's first byte: the rest is one Peerwire datagram.
pub const KIND_DATAGRAM: u8 = 0;
/// A message's first byte: the rest is a signal sealed under the connection's link key.
pub const KIND_SIGNAL: u8 = 1;
/// A message's first byte: the peripheral's challenge, which opens the room proof.
pub const KIND_CHALLENGE: u8 = 3;
/// A message's first byte: the central's answer to the challenge.
pub const KIND_ANSWER: u8 = 4;
/// A message's first byte: the peripheral's proof, which closes the room proof.
pub const KIND_PROOF: u8 = 5;

/// The largest message: a kind byte and a datagram.
pub const MAX_MESSAGE: usize = 1 + MAX_DATAGRAM;
/// The smallest ATT MTU, which every link starts with.
pub const MIN_ATT_MTU: u16 = 23;
/// The longest attribute value GATT allows.
const MAX_VALUE: usize = 512;
/// A message whose last chunk came longer ago than this is dropped.
pub const PARTIAL_TIMEOUT: Duration = Duration::from_secs(2);

const FIRST: u8 = 0x80;
const LAST: u8 = 0x40;
const COUNTER: u8 = 0x3f;

/// The sender index and ephemeral key of an HS1 (one with a cookie too), which tell its resends from a new handshake:
/// a host takes one new HS1 at a time on a connection no peer is bound to (BLE-R9). `None` for any other datagram.
pub fn hs1_id(datagram: &[u8]) -> Option<(u32, [u8; 32])> {
    match crate::wire::parse(datagram)? {
        crate::wire::Packet::Hs1 { sender, noise, .. } => Some((sender, noise.get(..32)?.try_into().ok()?)),
        _ => None,
    }
}

/// The longest GATT value a link with ATT MTU `mtu` carries: `mtu - 3`, at least 20 and at most 512.
pub fn value_len(mtu: u16) -> usize {
    (usize::from(mtu.max(MIN_ATT_MTU)) - 3).min(MAX_VALUE)
}

/// Splits messages into chunks for one connection, in one direction.
#[derive(Debug, Default)]
pub struct Chunker {
    /// The next chunk's counter.
    next: u8,
}

impl Chunker {
    pub fn new() -> Chunker {
        Chunker::default()
    }

    /// Hands `send` the chunks of `message` for a link with ATT MTU `mtu`, in order, each a GATT value of at most
    /// [`value_len`]`(mtu)` bytes, and stops at the first it can't send (`false`): the rest of a message is no use
    /// without it. Whether all went; nothing goes for an empty message or one over [`MAX_MESSAGE`]. Chunks of one
    /// message must go out before the next message's.
    pub fn split(&mut self, message: &[u8], mtu: u16, mut send: impl FnMut(&[u8]) -> bool) -> bool {
        if message.is_empty() || message.len() > MAX_MESSAGE {
            return false;
        }
        let room = value_len(mtu) - 1;
        let mut value = [0u8; MAX_VALUE];
        let mut first = FIRST;
        for (i, part) in message.chunks(room).enumerate() {
            let last = if (i + 1) * room >= message.len() { LAST } else { 0 };
            value[0] = first | last | self.next;
            value[1..=part.len()].copy_from_slice(part);
            self.next = (self.next + 1) & COUNTER;
            first = 0;
            if !send(&value[..=part.len()]) {
                return false;
            }
        }
        true
    }
}

/// Puts messages back together from one connection's chunks, in one direction. It holds at most one message being
/// assembled, of at most [`MAX_MESSAGE`] bytes, in one buffer it keeps.
#[derive(Debug, Default)]
pub struct Reassembler {
    buf: Vec<u8>,
    /// The counter the next chunk of the open message must have; `None` with no message open.
    next: Option<u8>,
    /// When the open message's last chunk came.
    at: Instant,
    /// Chunks of the open message.
    held: u64,
    /// The counter of the last chunk taken: one that repeats it is a copy.
    last: Option<u8>,
    /// The buffer holds a message handed out by the last `push`.
    done: bool,
    dropped: u64,
}

impl Reassembler {
    pub fn new() -> Reassembler {
        Reassembler::default()
    }

    /// Takes one GATT value that came at `now`: the whole message once its last chunk is in, valid until the next
    /// call. A chunk whose counter shows a lost one, a first chunk while a message is open (its end was lost), a
    /// message growing past [`MAX_MESSAGE`] or left for [`PARTIAL_TIMEOUT`], or a chunk with no payload drops the open
    /// message; the chunk goes with it unless it is a first one, which starts the next. A chunk that isn't a first one
    /// with no message open, or that repeats the last chunk's counter (a copy), is dropped, with the open message.
    ///
    /// Only a run of exactly 64 lost chunks (or a multiple) between two that arrive within the timeout goes unseen,
    /// and joins the parts of two messages: Peerwire packets are authenticated, so such a message fails its checks.
    pub fn push(&mut self, now: Instant, value: &[u8]) -> Option<&[u8]> {
        if std::mem::take(&mut self.done) {
            self.buf.clear();
        }
        if self.next.is_some() && now.saturating_duration_since(self.at) > PARTIAL_TIMEOUT {
            self.drop_open();
        }
        let Some((&header, data)) = value.split_first().filter(|(_, d)| !d.is_empty()) else {
            self.drop_open();
            self.dropped += 1;
            return None;
        };
        let (first, counter) = (header & FIRST != 0, header & COUNTER);
        if self.last == Some(counter) {
            self.drop_open();
            self.dropped += 1;
            return None;
        }
        if self.next.is_some() && (first || self.next != Some(counter)) {
            self.drop_open();
        }
        if self.next.is_none() && !first {
            self.dropped += 1;
            return None;
        }
        if self.buf.len() + data.len() > MAX_MESSAGE {
            self.drop_open();
            self.dropped += 1;
            return None;
        }
        if self.buf.capacity() < MAX_MESSAGE {
            self.buf.reserve_exact(MAX_MESSAGE - self.buf.len());
        }
        self.buf.extend_from_slice(data);
        (self.next, self.at, self.held, self.last) = (Some((counter + 1) & COUNTER), now, self.held + 1, Some(counter));
        if header & LAST == 0 {
            return None;
        }
        (self.next, self.held, self.done) = (None, 0, true);
        Some(&self.buf)
    }

    /// Chunks dropped so far: invalid ones, those of messages that lost one, and strays after a lost first chunk.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Bytes of the message being assembled.
    pub fn pending(&self) -> usize {
        if self.done {
            0
        } else {
            self.buf.len()
        }
    }

    fn drop_open(&mut self) {
        self.next = None;
        self.dropped += std::mem::take(&mut self.held);
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha8Rng;
    use rand_core::{RngCore, SeedableRng};

    const T0: Instant = Instant::ZERO;

    fn below(rng: &mut ChaCha8Rng, n: usize) -> usize {
        (rng.next_u64() % n as u64) as usize
    }

    fn message(rng: &mut ChaCha8Rng, len: usize) -> Vec<u8> {
        let mut m = vec![0u8; len];
        rng.fill_bytes(&mut m);
        m
    }

    fn split(tx: &mut Chunker, m: &[u8], mtu: u16) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        tx.split(m, mtu, |v| {
            out.push(v.to_vec());
            true
        });
        out
    }

    /// The messages `rx` gives for `values`, all at `now`.
    fn feed(rx: &mut Reassembler, values: &[Vec<u8>]) -> Vec<Vec<u8>> {
        values.iter().filter_map(|v| rx.push(T0, v).map(<[u8]>::to_vec)).collect()
    }

    #[test]
    fn hs1_id_names_a_handshake_and_its_resends_only() {
        use crate::{Config, Endpoint, Identity, Lane};
        let ids: Vec<Identity> = (1..=2u8).map(|i| Identity::from_secret_bytes(&[i; 32])).collect();
        let mut ex = Endpoint::<u32>::new(&ids[0], Config::default(), [1; 32]);
        let mut ey = Endpoint::<u32>::new(&ids[1], Config::default(), [2; 32]);
        ex.connect(T0, 2, ids[1].peer_id()).unwrap();
        let hs1 = ex.poll_transmit(T0).unwrap().payload;
        let id = hs1_id(&hs1).expect("an HS1");
        ex.handle_timeout(Instant::from_millis(600));
        let again = ex.poll_transmit(Instant::from_millis(600)).unwrap().payload;
        assert_eq!(hs1_id(&again), Some(id), "a resend has the same id");
        ey.handle_datagram(T0, 1, &hs1);
        let hs2 = ey.poll_transmit(T0).unwrap().payload;
        assert_eq!(hs1_id(&hs2), None);
        ex.handle_datagram(T0, 2, &hs2);
        ex.send(&ids[1].peer_id(), Lane::Reliable, b"x").unwrap();
        assert_eq!(hs1_id(&ex.poll_transmit(T0).unwrap().payload), None);
        assert_eq!(hs1_id(&hs1[..20]), None);
        let mut other = Endpoint::<u32>::new(&ids[0], Config::default(), [3; 32]);
        other.connect(T0, 2, ids[1].peer_id()).unwrap();
        assert_ne!(hs1_id(&other.poll_transmit(T0).unwrap().payload), Some(id), "another dial");
    }

    #[test]
    fn value_len_follows_the_mtu_within_gatt_limits() {
        assert_eq!(value_len(0), 20);
        assert_eq!(value_len(23), 20);
        assert_eq!(value_len(185), 182);
        assert_eq!(value_len(515), 512);
        assert_eq!(value_len(517), 512);
        assert_eq!(value_len(u16::MAX), 512);
    }

    #[test]
    fn a_message_splits_into_values_that_fit_with_first_and_last_marked() {
        let mut tx = Chunker::new();
        let chunks = split(&mut tx, &[7u8; 100], 23);
        assert_eq!(chunks.len(), 6, "19 bytes each: 100 = 5 x 19 + 5");
        assert!(chunks.iter().all(|c| c.len() <= 20));
        assert_eq!(chunks[0][0], FIRST);
        assert_eq!(chunks[5][0], LAST | 5);
        assert_eq!(chunks[5].len(), 6);
        assert!(chunks[1..5].iter().enumerate().all(|(i, c)| c[0] == i as u8 + 1));
        let one = split(&mut tx, &[1, 2, 3], 23);
        assert_eq!(one, vec![vec![FIRST | LAST | 6, 1, 2, 3]], "the counter goes on across messages");
        let exact = split(&mut tx, &[9u8; 38], 23);
        assert_eq!((exact.len(), exact[1][0] & LAST), (2, LAST), "a message that fills its chunks exactly");
    }

    #[test]
    fn empty_and_oversized_messages_make_no_chunks() {
        let mut tx = Chunker::new();
        assert!(split(&mut tx, &[], 517).is_empty());
        assert!(split(&mut tx, &vec![0; MAX_MESSAGE + 1], 517).is_empty());
        assert!(!tx.split(&[], 517, |_| true));
        assert_eq!(split(&mut tx, &vec![0; MAX_MESSAGE], 517).len(), 3);
        assert_eq!(tx.next, 3, "nothing was counted for the refused ones");
    }

    #[test]
    fn a_send_that_fails_stops_the_message() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let mut sent = Vec::new();
        let all = tx.split(&[1u8; 60], 23, |v| {
            sent.push(v.to_vec());
            sent.len() < 2
        });
        assert!(!all);
        assert_eq!(sent.len(), 2, "the second was refused, and nothing after it was tried");
        assert_eq!(feed(&mut rx, &sent[..1]), Vec::<Vec<u8>>::new());
        // The next message's first chunk has the counter the receiver expects next, and still starts anew.
        assert_eq!(feed(&mut rx, &split(&mut tx, &[2u8; 5], 23)), vec![vec![2u8; 5]]);
    }

    #[test]
    fn a_largest_message_reassembles_at_the_smallest_mtu() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let m: Vec<u8> = (0..MAX_MESSAGE).map(|i| i as u8).collect();
        let chunks = split(&mut tx, &m, 23);
        assert_eq!(chunks.len(), 65);
        assert_eq!(feed(&mut rx, &chunks), vec![m]);
        assert_eq!((rx.dropped(), rx.pending()), (0, 0));
    }

    #[test]
    fn a_lost_middle_chunk_drops_its_message_and_the_next_one_arrives() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let (a, b) = (vec![1u8; 50], vec![2u8; 50]);
        let mut chunks = split(&mut tx, &a, 23);
        chunks.remove(1);
        chunks.extend(split(&mut tx, &b, 23));
        assert_eq!(feed(&mut rx, &chunks), vec![b]);
        assert!(rx.dropped() >= 1);
    }

    #[test]
    fn a_repeated_chunk_drops_its_message() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let mut chunks = split(&mut tx, &[1u8; 50], 23);
        chunks.insert(2, chunks[1].clone());
        assert!(feed(&mut rx, &chunks).is_empty());
    }

    #[test]
    fn a_copy_of_a_whole_message_comes_out_once() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let one = split(&mut tx, &[1, 2], 23);
        assert_eq!(feed(&mut rx, &[one[0].clone(), one[0].clone()]), vec![vec![1, 2]]);
        let two = split(&mut tx, &[3u8; 30], 23);
        assert_eq!(feed(&mut rx, &[two[0].clone(), two[1].clone(), two[1].clone()]), vec![vec![3u8; 30]]);
        assert_eq!(rx.dropped(), 2);
    }

    #[test]
    fn a_lost_last_chunk_is_seen_at_the_next_first_one() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let (a, b) = (vec![1u8; 30], vec![2u8; 30]);
        let mut chunks = split(&mut tx, &a, 23);
        chunks.pop();
        chunks.extend(split(&mut tx, &b, 23));
        assert_eq!(feed(&mut rx, &chunks), vec![b], "a's start isn't glued to b");
    }

    #[test]
    fn a_lost_first_chunk_drops_the_strays_after_it() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let mut chunks = split(&mut tx, &[3u8; 60], 23);
        chunks.remove(0);
        assert!(feed(&mut rx, &chunks).is_empty());
        assert_eq!((rx.dropped(), rx.pending()), (3, 0));
    }

    #[test]
    fn a_first_chunk_with_the_expected_counter_still_starts_a_new_message() {
        // A sender that counts only the chunks it sends: the rest of a message it gave up on never got numbers.
        let mut rx = Reassembler::new();
        assert_eq!(rx.push(T0, &[FIRST, 1, 1]), None);
        assert_eq!(rx.push(T0, &[FIRST | LAST | 1, 2, 2]), Some(&[2u8, 2][..]));
        assert_eq!(rx.dropped(), 1);
    }

    #[test]
    fn empty_and_header_only_values_drop_the_open_message() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let chunks = split(&mut tx, &[4u8; 40], 23);
        assert_eq!(rx.push(T0, &chunks[0]), None);
        assert_eq!(rx.push(T0, &[]), None);
        assert_eq!(rx.pending(), 0);
        assert!(feed(&mut rx, &chunks[1..]).is_empty(), "the rest of it is strays now");
        let chunks = split(&mut tx, &[5u8; 40], 23);
        assert_eq!(rx.push(T0, &chunks[0]), None);
        assert_eq!(rx.push(T0, &[chunks[1][0]]), None, "a header with no payload");
        assert_eq!(rx.pending(), 0);
        assert_eq!(rx.dropped(), 1 + 1 + 2 + 1 + 1);
    }

    #[test]
    fn a_message_growing_past_the_largest_is_dropped() {
        let mut rx = Reassembler::new();
        // A forged run of continuation chunks with the right counters, never a last one.
        let mut value = vec![FIRST];
        value.extend([0u8; 511]);
        assert_eq!(rx.push(T0, &value), None);
        for i in 1..=2u8 {
            value[0] = i;
            assert_eq!(rx.push(T0, &value), None);
        }
        assert_eq!(rx.pending(), 0, "1,533 bytes would be over");
        let mut tx = Chunker::new();
        assert_eq!(feed(&mut rx, &split(&mut tx, &vec![9u8; MAX_MESSAGE], 517)).len(), 1);
    }

    #[test]
    fn the_bound_is_exactly_the_largest_message() {
        let mut rx = Reassembler::new();
        let first = |n: usize| [&[FIRST][..], &vec![0u8; n]].concat();
        let last = |n: usize| [&[LAST | 1][..], &vec![0u8; n]].concat();
        assert_eq!(rx.push(T0, &first(1000)), None);
        assert_eq!(rx.push(T0, &last(MAX_MESSAGE - 1000)).map(<[u8]>::len), Some(MAX_MESSAGE));
        assert_eq!(rx.push(T0, &first(1000)), None);
        assert_eq!(rx.push(T0, &last(MAX_MESSAGE - 999)), None, "one byte over");
        assert_eq!((rx.pending(), rx.dropped()), (0, 2));
    }

    #[test]
    fn a_message_left_too_long_is_dropped() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let chunks = split(&mut tx, &[8u8; 50], 23);
        let at = |ms| Instant::from_millis(ms);
        assert_eq!(rx.push(at(0), &chunks[0]), None);
        assert_eq!(rx.push(at(2000), &chunks[1]), None, "2 s after the last chunk: still in time");
        assert_eq!(rx.push(at(4001), &chunks[2]).map(<[u8]>::to_vec), None, "over 2 s after it");
        assert_eq!((rx.pending(), rx.dropped()), (0, 3));
    }

    #[test]
    fn the_buffer_is_allocated_once() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        for len in [1, MAX_MESSAGE, 600, MAX_MESSAGE, 2] {
            assert_eq!(feed(&mut rx, &split(&mut tx, &vec![1u8; len], 23)).len(), 1);
            assert_eq!(rx.buf.capacity(), MAX_MESSAGE);
        }
    }

    /// Property: with no loss, every message comes out whole and in order, at every MTU from 23 to 517, across counter
    /// wraps.
    #[test]
    fn prop_lossless_round_trip() {
        let mut rng = ChaCha8Rng::seed_from_u64(110);
        for mtu in 23..=517u16 {
            let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
            for _ in 0..6 {
                let len = 1 + below(&mut rng, MAX_MESSAGE);
                let m = message(&mut rng, len);
                let chunks = split(&mut tx, &m, mtu);
                assert!(chunks.iter().all(|c| c.len() <= value_len(mtu) && c.len() >= 2));
                assert_eq!(feed(&mut rx, &chunks), vec![m], "mtu {mtu}");
            }
            assert_eq!(rx.dropped(), 0);
        }
    }

    /// Property: with chunks lost at random (no run of 64 or more), exactly the messages none of whose chunks was lost
    /// come out, unaltered and in order; nothing else does, and every chunk that arrived is accounted for.
    #[test]
    fn prop_loss_drops_whole_messages_only() {
        let mut rng = ChaCha8Rng::seed_from_u64(1100);
        for round in 0..300 {
            let mtu = 23 + below(&mut rng, 200) as u16;
            let loss = 1 + below(&mut rng, 50); // percent
            let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
            let (mut expect, mut got, mut run, mut pushed) = (Vec::new(), Vec::new(), 0, 0);
            for _ in 0..40 {
                let len = 1 + below(&mut rng, 400);
                let m = message(&mut rng, len);
                let mut whole = true;
                for c in split(&mut tx, &m, mtu) {
                    if below(&mut rng, 100) < loss && run < 63 {
                        (whole, run) = (false, run + 1);
                        continue;
                    }
                    run = 0;
                    pushed += 1;
                    got.extend(rx.push(T0, &c).map(<[u8]>::to_vec));
                    assert!(rx.pending() <= MAX_MESSAGE);
                }
                if whole {
                    expect.push(m);
                }
            }
            assert_eq!(got, expect, "round {round}, mtu {mtu}, loss {loss}%");
            let delivered: u64 = got.iter().map(|m| m.len().div_ceil(value_len(mtu) - 1) as u64).sum();
            assert_eq!(delivered + rx.dropped() + rx.held, pushed, "round {round}");
        }
    }

    /// Property: with chunks lost, repeated and swapped at random, every message that comes out is one that was sent,
    /// unaltered. (GATT keeps the order, so only this fault injection can swap two whole messages.)
    #[test]
    fn prop_faults_never_alter_or_reorder_a_message() {
        let mut rng = ChaCha8Rng::seed_from_u64(1101);
        let mut delivered = 0;
        for round in 0..300 {
            let mtu = 23 + below(&mut rng, 100) as u16;
            let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
            let mut sent = Vec::new();
            let mut values = Vec::new();
            for _ in 0..30 {
                let len = 1 + below(&mut rng, 300);
                let m = message(&mut rng, len);
                values.extend(split(&mut tx, &m, mtu));
                sent.push(m);
            }
            for _ in 0..values.len() / 10 {
                let i = below(&mut rng, values.len());
                match below(&mut rng, 3) {
                    0 => drop(values.remove(i)),
                    1 => values.insert(i, values[i].clone()),
                    _ if i + 1 < values.len() => values.swap(i, i + 1),
                    _ => {}
                }
            }
            for m in feed(&mut rx, &values) {
                assert!(sent.contains(&m), "round {round}");
                delivered += 1;
            }
        }
        assert!(delivered > 3000, "most still come out: {delivered}");
    }

    /// Property: arbitrary values at arbitrary times never panic, never yield an empty message or one over the bound,
    /// and never hold more.
    #[test]
    fn prop_arbitrary_values_are_bounded() {
        let mut rng = ChaCha8Rng::seed_from_u64(11);
        let mut rx = Reassembler::new();
        let mut now = 0;
        for _ in 0..200_000 {
            let len = below(&mut rng, 40);
            let mut value = message(&mut rng, len);
            // Mostly well-formed headers in sequence, to reach the deep states.
            if len > 0 && below(&mut rng, 4) > 0 {
                value[0] = (value[0] & (FIRST | LAST)) | rx.next.unwrap_or(0);
            }
            now += below(&mut rng, 300) as u64;
            if let Some(m) = rx.push(Instant::from_millis(now), &value) {
                assert!(!m.is_empty() && m.len() <= MAX_MESSAGE);
            }
            assert!(rx.pending() <= MAX_MESSAGE);
            assert!(rx.buf.capacity() == 0 || rx.buf.capacity() == MAX_MESSAGE);
        }
    }

    /// The limit the docs name: 64 lost chunks in a row, exactly, joins two messages' parts unseen.
    #[test]
    fn a_run_of_64_lost_chunks_goes_unseen() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let mut chunks = split(&mut tx, &[1u8; 19 * 10], 23); // counters 0 to 9
        chunks.extend(split(&mut tx, &[2u8; 19 * 60], 23)); // 10 to 69: 65 is its 56th chunk, with counter 1
        assert_eq!(rx.push(T0, &chunks[0]), None);
        let mut joined = vec![1u8; 19];
        joined.extend([2u8; 19 * 5]);
        assert_eq!(feed(&mut rx, &chunks[65..]), vec![joined]);
    }

    #[test]
    fn dropped_counts_every_chunk_of_a_message_cut_short() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let chunks = split(&mut tx, &[6u8; 19 * 4], 23);
        assert!(feed(&mut rx, &chunks[..3]).is_empty());
        assert_eq!(feed(&mut rx, &split(&mut tx, &[7], 23)), vec![vec![7]], "a first chunk cuts the open one short");
        assert_eq!(rx.dropped(), 3);
    }
}
