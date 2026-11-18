//! ESP-NOW frames: the wire format of `docs/concepts/espnow.md`. Every frame starts with `PW`, the version and its
//! kind; each kind has one exact length (a data frame's payload is the rest), so a frame parses one way or not at all,
//! and what parses encodes back to the same bytes.

/// A frame's first two bytes.
pub const MAGIC: [u8; 2] = *b"PW";
/// The format's version, the third byte. A frame of another version is dropped.
pub const VERSION: u8 = 1;
/// The longest frame ESP-NOW v1 carries, and the shortest any end must take.
pub const V1_FRAME: usize = 250;
/// The longest frame ESP-NOW v2 carries.
pub const V2_FRAME: usize = 1470;
/// The longest advert: as much service data as a Bluetooth advert has room for.
pub const MAX_ADVERT: usize = 31;
/// A data frame's bytes before its payload: magic, version, kind, the receiver's id, the sequence number, flags and
/// the value's length.
pub const DATA_HEADER: usize = 19;
/// The longest value a data frame is part of: GATT's longest attribute value, which the node's chunks never pass.
pub const MAX_VALUE: usize = 512;

const HEADER: usize = 4;
const ADVERT: u8 = 1;
const OPEN: u8 = 2;
const ACCEPT: u8 = 3;
const PROBE: u8 = 4;
const DATA: u8 = 5;
const KEEPALIVE: u8 = 6;
const CLOSE: u8 = 7;
/// A data frame's flags: it starts a value, it ends one.
const FIRST: u8 = 1;
const LAST: u8 = 2;

/// One ESP-NOW frame. A connection is known at each end by a 64-bit number that end drew at random, its id; every frame
/// on a connection carries the receiver's id, and comes from the MAC address the connection is with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    /// Broadcast: the service data a node advertises (its room advert).
    Advert { data: &'a [u8] },
    /// Opens a connection: the opener's id, and the longest frame it takes.
    Open { from: u64, rx_max: u16 },
    /// Accepts one: the opener's id, the accepter's, and the longest frame the accepter takes.
    Accept { to: u64, from: u64, rx_max: u16 },
    /// Asks the other end for a [`Keepalive`](Frame::Keepalive) at once: whether it still has the connection, after a
    /// CLOSE that may be forged.
    Probe { to: u64 },
    /// Part of a value, or all of it: the sequence number counts the connection's data frames in one direction, and
    /// every part says how long the whole value is (1 to [`MAX_VALUE`]), so a value that lost a part, or the end of
    /// one, doesn't pass for whole.
    Data { to: u64, seq: u32, first: bool, last: bool, total: u16, payload: &'a [u8] },
    /// Nothing to send for a while: the connection is still up.
    Keepalive { to: u64 },
    /// The sender closed the connection.
    Close { to: u64 },
}

/// Why bytes aren't a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// Not one of ours: too short for the header, or another magic. ESP-NOW carries other apps' frames too.
    NotOurs,
    /// Ours, of another version.
    Version,
    /// An unknown kind.
    Kind,
    /// The wrong length for its kind, an empty advert or payload, or an advert over [`MAX_ADVERT`].
    Length,
    /// A field out of range: unknown flags, a frame size outside [`V1_FRAME`]..=[`V2_FRAME`], an id of 0, a value's
    /// length of 0, over [`MAX_VALUE`] or under the payload's.
    Field,
}

/// Parses one frame, as received. The fuzz target's entry point: any bytes give a frame or a [`Malformed`], never a
/// panic, and a frame encodes back to exactly these bytes.
pub fn parse(b: &[u8]) -> Result<Frame<'_>, Malformed> {
    if b.len() < HEADER || b[..2] != MAGIC {
        return Err(Malformed::NotOurs);
    }
    if b[2] != VERSION {
        return Err(Malformed::Version);
    }
    let body = &b[HEADER..];
    let id = |at: usize| -> Result<u64, Malformed> {
        let id = body.get(at..at + 8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes);
        match id {
            None => Err(Malformed::Length),
            Some(0) => Err(Malformed::Field),
            Some(id) => Ok(id),
        }
    };
    let size = |at: usize| -> Result<u16, Malformed> {
        let n = body.get(at..at + 2).and_then(|b| b.try_into().ok()).map(u16::from_le_bytes);
        match n {
            None => Err(Malformed::Length),
            Some(n) if (V1_FRAME..=V2_FRAME).contains(&usize::from(n)) => Ok(n),
            Some(_) => Err(Malformed::Field),
        }
    };
    let exact = |n: usize| if body.len() == n { Ok(()) } else { Err(Malformed::Length) };
    match b[3] {
        ADVERT => {
            if body.is_empty() || body.len() > MAX_ADVERT {
                return Err(Malformed::Length);
            }
            Ok(Frame::Advert { data: body })
        }
        OPEN => {
            exact(10)?;
            Ok(Frame::Open { from: id(0)?, rx_max: size(8)? })
        }
        ACCEPT => {
            exact(18)?;
            Ok(Frame::Accept { to: id(0)?, from: id(8)?, rx_max: size(16)? })
        }
        PROBE => {
            exact(8)?;
            Ok(Frame::Probe { to: id(0)? })
        }
        KEEPALIVE => {
            exact(8)?;
            Ok(Frame::Keepalive { to: id(0)? })
        }
        CLOSE => {
            exact(8)?;
            Ok(Frame::Close { to: id(0)? })
        }
        DATA => {
            if b.len() <= DATA_HEADER {
                return Err(Malformed::Length);
            }
            let flags = b[16];
            let total = u16::from_le_bytes([b[17], b[18]]);
            let payload = &b[DATA_HEADER..];
            if flags & !(FIRST | LAST) != 0
                || total == 0
                || usize::from(total) > MAX_VALUE
                || payload.len() > total.into()
            {
                return Err(Malformed::Field);
            }
            let seq = u32::from_le_bytes([b[12], b[13], b[14], b[15]]);
            let (first, last) = (flags & FIRST != 0, flags & LAST != 0);
            Ok(Frame::Data { to: id(0)?, seq, first, last, total, payload })
        }
        _ => Err(Malformed::Kind),
    }
}

impl Frame<'_> {
    /// The frame's bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(self.len());
        b.extend_from_slice(&MAGIC);
        b.push(VERSION);
        match *self {
            Frame::Advert { data } => {
                b.push(ADVERT);
                b.extend_from_slice(data);
            }
            Frame::Open { from, rx_max } => {
                b.push(OPEN);
                b.extend_from_slice(&from.to_le_bytes());
                b.extend_from_slice(&rx_max.to_le_bytes());
            }
            Frame::Accept { to, from, rx_max } => {
                b.push(ACCEPT);
                b.extend_from_slice(&to.to_le_bytes());
                b.extend_from_slice(&from.to_le_bytes());
                b.extend_from_slice(&rx_max.to_le_bytes());
            }
            Frame::Probe { to } => {
                b.push(PROBE);
                b.extend_from_slice(&to.to_le_bytes());
            }
            Frame::Data { to, seq, first, last, total, payload } => {
                b.push(DATA);
                b.extend_from_slice(&to.to_le_bytes());
                b.extend_from_slice(&seq.to_le_bytes());
                b.push(if first { FIRST } else { 0 } | if last { LAST } else { 0 });
                b.extend_from_slice(&total.to_le_bytes());
                b.extend_from_slice(payload);
            }
            Frame::Keepalive { to } => {
                b.push(KEEPALIVE);
                b.extend_from_slice(&to.to_le_bytes());
            }
            Frame::Close { to } => {
                b.push(CLOSE);
                b.extend_from_slice(&to.to_le_bytes());
            }
        }
        b
    }

    /// The length of [`encode`](Frame::encode)'s bytes.
    pub fn len(&self) -> usize {
        HEADER
            + match self {
                Frame::Advert { data } => data.len(),
                Frame::Open { .. } => 10,
                Frame::Accept { .. } => 18,
                Frame::Data { payload, .. } => DATA_HEADER - HEADER + payload.len(),
                Frame::Probe { .. } | Frame::Keepalive { .. } | Frame::Close { .. } => 8,
            }
    }

    /// Never: every frame has its header. For clippy's `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Frame<'static>> {
        vec![
            Frame::Advert { data: &[7; 9] },
            Frame::Advert { data: &[1; MAX_ADVERT] },
            Frame::Open { from: 0x0102_0304_0506_0708, rx_max: 250 },
            Frame::Accept { to: 9, from: u64::MAX, rx_max: 1470 },
            Frame::Probe { to: 3 },
            Frame::Data { to: 5, seq: u32::MAX, first: true, last: false, total: 300, payload: &[0xaa; 231] },
            Frame::Data { to: 5, seq: 0, first: false, last: true, total: 300, payload: &[1; 69] },
            Frame::Data { to: 5, seq: 1, first: true, last: true, total: 512, payload: &[2; 512] },
            Frame::Keepalive { to: 1 },
            Frame::Close { to: 2 },
        ]
    }

    #[test]
    fn every_frame_round_trips() {
        for f in all() {
            let b = f.encode();
            assert_eq!(b.len(), f.len(), "{f:?}");
            assert_eq!(parse(&b), Ok(f), "{f:?}");
        }
        let data =
            Frame::Data { to: 5, seq: 0, first: true, last: true, total: 300, payload: &[0; V1_FRAME - DATA_HEADER] };
        assert_eq!(data.len(), V1_FRAME, "a full v1 data frame");
    }

    #[test]
    fn what_isnt_a_frame_is_refused() {
        let open = Frame::Open { from: 1, rx_max: 250 }.encode();
        assert_eq!(parse(&[]), Err(Malformed::NotOurs));
        assert_eq!(parse(b"PW\x01"), Err(Malformed::NotOurs));
        assert_eq!(parse(b"XW\x01\x02"), Err(Malformed::NotOurs), "another app's frame");
        assert_eq!(parse(&[&open[..2], &[2], &open[3..]].concat()), Err(Malformed::Version));
        assert_eq!(parse(b"PW\x01\x00"), Err(Malformed::Kind));
        assert_eq!(parse(b"PW\x01\x08\0\0\0\0\0\0\0\x01"), Err(Malformed::Kind));
        // Truncated, or one byte over.
        for f in all() {
            let b = f.encode();
            for n in 0..b.len() {
                let short = parse(&b[..n]);
                let ok_shorter = matches!(f, Frame::Advert { .. } | Frame::Data { .. });
                assert!(short.is_err() || ok_shorter && n > 4, "{f:?} cut to {n}: {short:?}");
            }
            let mut long = b.clone();
            long.push(0);
            let over = parse(&long);
            match f {
                Frame::Advert { data } if data.len() < MAX_ADVERT => assert!(over.is_ok()),
                Frame::Data { total, payload, .. } => assert_eq!(over.is_ok(), payload.len() < total.into()),
                _ => assert_eq!(over, Err(Malformed::Length), "{f:?} with a byte more"),
            }
        }
        assert_eq!(parse(b"PW\x01\x01"), Err(Malformed::Length), "an empty advert");
        assert_eq!(parse(&[&b"PW\x01\x01"[..], &[0; MAX_ADVERT + 1]].concat()), Err(Malformed::Length));
        let data = |total, payload| Frame::Data { to: 1, seq: 0, first: true, last: true, total, payload }.encode();
        assert_eq!(parse(&data(1, &[])), Err(Malformed::Length));
        assert_eq!(parse(&data(0, &[1])), Err(Malformed::Field), "a value of 0 bytes");
        assert_eq!(parse(&data(513, &[1])), Err(Malformed::Field), "a value over 512 bytes");
        assert_eq!(parse(&data(2, &[1, 2, 3])), Err(Malformed::Field), "a part longer than its value");
        // Fields out of range.
        assert_eq!(parse(&Frame::Close { to: 0 }.encode()), Err(Malformed::Field), "id 0");
        assert_eq!(parse(&Frame::Open { from: 1, rx_max: 249 }.encode()), Err(Malformed::Field));
        assert_eq!(parse(&Frame::Accept { to: 1, from: 2, rx_max: 1471 }.encode()), Err(Malformed::Field));
        let mut bad_flags = Frame::Data { to: 1, seq: 0, first: true, last: true, total: 1, payload: &[1] }.encode();
        bad_flags[16] |= 4;
        assert_eq!(parse(&bad_flags), Err(Malformed::Field));
    }

    /// Any bytes: no panic, and a frame that parses encodes back to the same bytes.
    #[test]
    fn fuzz_frames() {
        use peerwire_sim::fuzz::{cases, mutate, run};
        let seeds: Vec<Vec<u8>> = all().iter().map(Frame::encode).collect();
        let mut parsed = 0;
        run(
            "espnow-frame",
            cases(20_000),
            |rng| {
                let mut b = rng.pick(&seeds).clone();
                for _ in 0..1 + rng.below(4) {
                    mutate(rng, &mut b, V2_FRAME + 8);
                }
                b
            },
            |b| {
                if let Ok(f) = parse(b) {
                    parsed += 1;
                    assert_eq!(f.encode(), b, "{f:?} isn't canonical");
                }
            },
        );
        assert!(parsed > 100, "the mutations keep some frames whole: {parsed}");
    }
}
