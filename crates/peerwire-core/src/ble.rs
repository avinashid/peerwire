//! Bluetooth LE framing (`docs/concepts/bluetooth.md`): messages of up to [`MAX_MESSAGE`] bytes in GATT values of as
//! little as 20, and the service's numbers, shared by every host (the Rust node, the ESP32 port, the browser).
//!
//! A message is split into chunks, one per GATT value: a header byte, then as much of the message as the value has
//! room for ([`value_len`]). The header marks the message's first and last chunks and counts the chunks sent on the
//! connection, so the receiver drops a message one of whose chunks a full buffer lost. Sans-IO: the host moves the
//! values.
//!
//! ```
//! use peerwire_core::ble::{Chunker, Reassembler};
//!
//! let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
//! let message = vec![0u8; 300];
//! let mut got = None;
//! for value in tx.split(&message, 23) {
//!     got = rx.push(&value); // each one a write or a notification, 20 bytes at most
//! }
//! assert_eq!(got, Some(message));
//! ```

use crate::wire::MAX_DATAGRAM;

/// The GATT service a peer serves, as a 128-bit UUID: `1c3eea41-c2be-493c-ab0b-7707ad2dbed6`.
pub const SERVICE_UUID: u128 = 0x1c3eea41_c2be_493c_ab0b_7707ad2dbed6;
/// Its `rx` characteristic: the central writes to it, without response.
pub const RX_UUID: u128 = 0x1c3eea42_c2be_493c_ab0b_7707ad2dbed6;
/// Its `tx` characteristic: the peripheral notifies on it.
pub const TX_UUID: u128 = 0x1c3eea43_c2be_493c_ab0b_7707ad2dbed6;

/// A message's first byte: the rest is one Peerwire datagram.
pub const KIND_DATAGRAM: u8 = 0;
/// A message's first byte: the rest is a sealed hello for the receiver's room.
pub const KIND_ROOM: u8 = 1;
/// A message's first byte: the rest is sealed to the receiver (an offer).
pub const KIND_PEER: u8 = 2;

/// The largest message: a kind byte and a datagram.
pub const MAX_MESSAGE: usize = 1 + MAX_DATAGRAM;
/// The smallest ATT MTU, which every link starts with.
pub const MIN_ATT_MTU: u16 = 23;
/// The longest attribute value GATT allows.
const MAX_VALUE: usize = 512;

const FIRST: u8 = 0x80;
const LAST: u8 = 0x40;
const COUNTER: u8 = 0x3f;

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

    /// The chunks of `message` for a link with ATT MTU `mtu`, in the order to send them, each a GATT value of at most
    /// [`value_len`]`(mtu)` bytes. None for an empty message or one over [`MAX_MESSAGE`]. Chunks of one message must
    /// go out before the next message's; if one can't be sent, the rest of its message is no use and may be dropped.
    pub fn split(&mut self, message: &[u8], mtu: u16) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if message.is_empty() || message.len() > MAX_MESSAGE {
            return out;
        }
        let room = value_len(mtu) - 1;
        let mut rest = message;
        let mut first = FIRST;
        while !rest.is_empty() {
            let (part, tail) = rest.split_at(rest.len().min(room));
            let last = if tail.is_empty() { LAST } else { 0 };
            let mut value = Vec::with_capacity(1 + part.len());
            value.push(first | last | self.next);
            value.extend_from_slice(part);
            out.push(value);
            self.next = (self.next + 1) & COUNTER;
            (rest, first) = (tail, 0);
        }
        out
    }
}

/// Puts messages back together from one connection's chunks, in one direction. It holds at most one message being
/// assembled, of at most [`MAX_MESSAGE`] bytes.
#[derive(Debug, Default)]
pub struct Reassembler {
    buf: Vec<u8>,
    /// The counter the next chunk of the open message must have; `None` with no message open.
    next: Option<u8>,
    /// Chunks of the open message.
    held: u64,
    dropped: u64,
}

impl Reassembler {
    pub fn new() -> Reassembler {
        Reassembler::default()
    }

    /// Takes one GATT value: the whole message once its last chunk is in. A chunk whose counter shows a lost one, a
    /// first chunk while a message is open (its end was lost), a message growing past [`MAX_MESSAGE`], or a chunk with
    /// no payload drops the open message; the chunk goes with it unless it is a first one, which starts the next. A
    /// chunk that isn't a first one with no message open is dropped.
    ///
    /// Only a run of exactly 64 lost chunks (or a multiple) between two that arrive goes unseen, and joins the parts
    /// of two messages: Peerwire packets are authenticated, so such a message fails its checks.
    pub fn push(&mut self, value: &[u8]) -> Option<Vec<u8>> {
        let Some((&header, data)) = value.split_first().filter(|(_, d)| !d.is_empty()) else {
            self.drop_open();
            self.dropped += 1;
            return None;
        };
        let (first, counter) = (header & FIRST != 0, header & COUNTER);
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
        self.buf.extend_from_slice(data);
        self.next = Some((counter + 1) & COUNTER);
        self.held += 1;
        if header & LAST != 0 {
            (self.next, self.held) = (None, 0);
            return Some(std::mem::take(&mut self.buf));
        }
        None
    }

    /// Chunks dropped so far: invalid ones, those of messages that lost one, and strays after a lost first chunk.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Bytes of the message being assembled.
    pub fn pending(&self) -> usize {
        self.buf.len()
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

    fn below(rng: &mut ChaCha8Rng, n: usize) -> usize {
        (rng.next_u64() % n as u64) as usize
    }

    fn message(rng: &mut ChaCha8Rng, len: usize) -> Vec<u8> {
        let mut m = vec![0u8; len];
        rng.fill_bytes(&mut m);
        m
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
        let m = vec![7u8; 100];
        let chunks = tx.split(&m, 23);
        assert_eq!(chunks.len(), 6, "19 bytes each: 100 = 5 x 19 + 5");
        assert!(chunks.iter().all(|c| c.len() <= 20));
        assert_eq!(chunks[0][0], FIRST);
        assert_eq!(chunks[5][0], LAST | 5);
        assert!(chunks[1..5].iter().enumerate().all(|(i, c)| c[0] == i as u8 + 1));
        let one = tx.split(&[1, 2, 3], 23);
        assert_eq!(one, vec![vec![FIRST | LAST | 6, 1, 2, 3]], "the counter goes on across messages");
    }

    #[test]
    fn empty_and_oversized_messages_make_no_chunks() {
        let mut tx = Chunker::new();
        assert!(tx.split(&[], 517).is_empty());
        assert!(tx.split(&vec![0; MAX_MESSAGE + 1], 517).is_empty());
        assert_eq!(tx.split(&vec![0; MAX_MESSAGE], 517).len(), 3);
        assert_eq!(tx.next, 3, "nothing was counted for the refused ones");
    }

    #[test]
    fn a_largest_message_reassembles_at_the_smallest_mtu() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let m: Vec<u8> = (0..MAX_MESSAGE).map(|i| i as u8).collect();
        let chunks = tx.split(&m, 23);
        assert_eq!(chunks.len(), 65);
        let got: Vec<_> = chunks.iter().filter_map(|c| rx.push(c)).collect();
        assert_eq!(got, vec![m]);
        assert_eq!((rx.dropped(), rx.pending()), (0, 0));
    }

    #[test]
    fn a_lost_middle_chunk_drops_its_message_and_the_next_one_arrives() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let (a, b) = (vec![1u8; 50], vec![2u8; 50]);
        let mut chunks = tx.split(&a, 23);
        chunks.remove(1);
        chunks.extend(tx.split(&b, 23));
        let got: Vec<_> = chunks.iter().filter_map(|c| rx.push(c)).collect();
        assert_eq!(got, vec![b]);
        assert!(rx.dropped() >= 1);
    }

    #[test]
    fn a_lost_last_chunk_is_seen_at_the_next_first_one() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let (a, b) = (vec![1u8; 30], vec![2u8; 30]);
        let mut chunks = tx.split(&a, 23);
        chunks.pop();
        chunks.extend(tx.split(&b, 23));
        let got: Vec<_> = chunks.iter().filter_map(|c| rx.push(c)).collect();
        assert_eq!(got, vec![b], "a's start isn't glued to b");
    }

    #[test]
    fn a_lost_first_chunk_drops_the_strays_after_it() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let mut chunks = tx.split(&[3u8; 60], 23);
        chunks.remove(0);
        assert!(chunks.iter().all(|c| rx.push(c).is_none()));
        assert_eq!((rx.dropped(), rx.pending()), (3, 0));
    }

    #[test]
    fn empty_and_header_only_values_drop_the_open_message() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let chunks = tx.split(&[4u8; 40], 23);
        assert_eq!(rx.push(&chunks[0]), None);
        assert_eq!(rx.push(&[]), None);
        assert_eq!(rx.pending(), 0);
        assert_eq!(rx.push(&chunks[1]), None);
        assert_eq!(rx.push(&chunks[2]), None, "the rest of it is strays now");
        let chunks = tx.split(&[5u8; 40], 23);
        assert_eq!(rx.push(&chunks[0]), None);
        assert_eq!(rx.push(&[chunks[1][0]]), None, "a header with no payload");
        assert_eq!(rx.pending(), 0);
    }

    #[test]
    fn a_message_growing_past_the_largest_is_dropped() {
        let mut rx = Reassembler::new();
        // A forged run of continuation chunks with the right counters, never a last one.
        let mut value = vec![FIRST];
        value.extend([0u8; 511]);
        assert_eq!(rx.push(&value), None);
        for i in 1..=2u8 {
            value[0] = i;
            assert_eq!(rx.push(&value), None);
        }
        assert!(rx.pending() <= MAX_MESSAGE);
        assert_eq!(rx.pending(), 0, "1,533 bytes would be over");
        // A largest message fills it exactly.
        let mut tx = Chunker::new();
        let chunks = tx.split(&vec![9u8; MAX_MESSAGE], 517);
        let got: Vec<_> = chunks.iter().filter_map(|c| rx.push(c)).collect();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn a_first_chunk_with_the_expected_counter_still_starts_a_new_message() {
        // A sender that counts only the chunks it sends: the rest of a message it gave up on never got numbers.
        let mut rx = Reassembler::new();
        assert_eq!(rx.push(&[FIRST, 1, 1]), None);
        assert_eq!(rx.push(&[FIRST | LAST | 1, 2, 2]), Some(vec![2, 2]));
        assert_eq!(rx.dropped(), 1);
    }

    #[test]
    fn the_bound_is_exactly_the_largest_message() {
        let mut rx = Reassembler::new();
        let first = |n: usize| [&[FIRST][..], &vec![0u8; n]].concat();
        let last = |n: usize| [&[LAST | 1][..], &vec![0u8; n]].concat();
        assert_eq!(rx.push(&first(1000)), None);
        assert_eq!(rx.push(&last(MAX_MESSAGE - 1000)).map(|m| m.len()), Some(MAX_MESSAGE));
        assert_eq!(rx.push(&first(1000)), None);
        assert_eq!(rx.push(&last(MAX_MESSAGE - 999)), None, "one byte over");
        assert_eq!((rx.pending(), rx.dropped()), (0, 2));
    }

    /// Property: with no loss, every message comes out whole and in order, at any MTU, across counter wraps.
    #[test]
    fn prop_lossless_round_trip() {
        let mut rng = ChaCha8Rng::seed_from_u64(110);
        for _ in 0..300 {
            let mtu = 23 + below(&mut rng, 600) as u16;
            let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
            for _ in 0..20 {
                let len = 1 + below(&mut rng, MAX_MESSAGE);
                let m = message(&mut rng, len);
                let chunks = tx.split(&m, mtu);
                assert!(chunks.iter().all(|c| c.len() <= value_len(mtu) && c.len() >= 2));
                let got: Vec<_> = chunks.iter().filter_map(|c| rx.push(c)).collect();
                assert_eq!(got, vec![m], "mtu {mtu}");
            }
            assert_eq!(rx.dropped(), 0);
        }
    }

    /// Property: with chunks lost at random (no run of 64 or more), exactly the messages none of whose chunks was lost
    /// come out, unaltered and in order; nothing else does, and nothing over the bound is held.
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
                for c in tx.split(&m, mtu) {
                    if below(&mut rng, 100) < loss && run < 63 {
                        (whole, run) = (false, run + 1);
                        continue;
                    }
                    run = 0;
                    pushed += 1;
                    got.extend(rx.push(&c));
                    assert!(rx.pending() <= MAX_MESSAGE);
                }
                if whole {
                    expect.push(m);
                }
            }
            assert_eq!(got, expect, "round {round}, mtu {mtu}, loss {loss}%");
            // Every chunk that arrived went into a message, was dropped, or is held.
            let delivered: u64 = got.iter().map(|m| m.len().div_ceil(value_len(mtu) - 1) as u64).sum();
            assert_eq!(delivered + rx.dropped() + rx.held, pushed, "round {round}");
        }
    }

    /// Property: arbitrary values never panic, never yield an empty message or one over the bound, and never hold more.
    #[test]
    fn prop_arbitrary_values_are_bounded() {
        let mut rng = ChaCha8Rng::seed_from_u64(11);
        let mut rx = Reassembler::new();
        for _ in 0..200_000 {
            let len = below(&mut rng, 40);
            let mut value = message(&mut rng, len);
            // Mostly well-formed headers in sequence, to reach the deep states.
            if len > 0 && below(&mut rng, 4) > 0 {
                value[0] = (value[0] & (FIRST | LAST)) | rx.next.unwrap_or(0);
            }
            if let Some(m) = rx.push(&value) {
                assert!(!m.is_empty() && m.len() <= MAX_MESSAGE);
            }
            assert!(rx.pending() <= MAX_MESSAGE);
        }
    }

    /// The limit the docs name: 64 lost chunks in a row, exactly, joins two messages' parts unseen.
    #[test]
    fn a_run_of_64_lost_chunks_goes_unseen() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let mut chunks = tx.split(&[1u8; 19 * 10], 23); // counters 0 to 9
        chunks.extend(tx.split(&[2u8; 19 * 60], 23)); // 10 to 69: 65 is its 56th chunk, with counter 1
        assert_eq!(rx.push(&chunks[0]), None);
        let got: Vec<_> = chunks[65..].iter().filter_map(|c| rx.push(c)).collect();
        let mut joined = vec![1u8; 19];
        joined.extend([2u8; 19 * 5]);
        assert_eq!(got, vec![joined]);
    }

    #[test]
    fn dropped_counts_every_chunk_of_a_message_cut_short() {
        let (mut tx, mut rx) = (Chunker::new(), Reassembler::new());
        let chunks = tx.split(&[6u8; 19 * 4], 23);
        for c in &chunks[..3] {
            assert_eq!(rx.push(c), None);
        }
        assert_eq!(rx.push(&tx.split(&[7], 23)[0]), Some(vec![7]), "a first chunk cuts the open one short");
        assert_eq!(rx.dropped(), 3);
    }
}
