//! One Bluetooth LE connection's protocol (feature `ble`), as `docs/concepts/bluetooth.md` has it (Messages): the room
//! proof, the link key, identity proofs, signals sealed under the link key and datagrams, over the framing, with the
//! limits each connection keeps. The `peerwire` node runs a [`Link`] on each of its connections, beside the rules
//! across them (how many, who connects, the budgets the radio shares), and a browser runs one through the C ABI
//! (`pw_ble_*`). Sans-IO: the host draws the challenge, moves the GATT values and passes the time in.
//!
//! ```
//! use peerwire_core::ble::{ble_key, identity, Input, Link, INNER_ID, INNER_ROOM};
//! use peerwire_core::{Identity, Instant};
//!
//! /// Moves the values `from` has queued to `to` (GATT writes or notifications); what `to` made of them.
//! fn carry(from: &mut Link, to: &mut Link) -> Vec<Input> {
//!     let mut got = Vec::new();
//!     while let Some(v) = from.value().map(<[u8]>::to_vec) {
//!         from.sent();
//!         got.push(to.push(Instant::ZERO, &v));
//!     }
//!     got
//! }
//!
//! let key = ble_key("app", "room");
//! // Each challenge is 16 random bytes.
//! let mut central = Link::central(key, [1; 16], 23, 64, Instant::ZERO);
//! let mut peripheral = Link::peripheral(key, [2; 16], 23, 64, Instant::ZERO);
//! carry(&mut peripheral, &mut central); // the challenge
//! assert!(carry(&mut central, &mut peripheral).contains(&Input::Proven)); // the answer
//! assert!(carry(&mut peripheral, &mut central).contains(&Input::Proven)); // the proof
//! let me = Identity::from_secret_bytes(&[7; 32]);
//! central.send_identity(&me);
//! central.signal(INNER_ROOM, b"hello");
//! let got = carry(&mut central, &mut peripheral);
//! let Some(Input::Signal(INNER_ID, proof)) = got.iter().find(|i| matches!(i, Input::Signal(INNER_ID, _))) else {
//!     panic!()
//! };
//! let (peer, sig) = identity(proof).unwrap();
//! assert!(peer == me.peer_id() && peripheral.verifies(&peer, &sig));
//! assert!(got.contains(&Input::Signal(INNER_ROOM, b"hello".to_vec())));
//! ```

use super::{
    hs1_id, value_len, Chunker, Reassembler, KIND_ANSWER, KIND_CHALLENGE, KIND_DATAGRAM, KIND_PROOF, KIND_SIGNAL,
    MAX_MESSAGE,
};
use crate::{Identity, Instant, PeerId};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::time::Duration;

/// A challenge: 16 random bytes, for the host to draw.
pub const CHALLENGE_LEN: usize = 16;
const MAC_LEN: usize = 32;
/// An identity proof: the sender's `PeerId` and its Ed25519 signature over the connection's challenges.
pub const ID_LEN: usize = 32 + 64;
/// An advert's service data: flags, `n`, tag.
pub const ADVERT_LEN: usize = 9;
/// The advert's flag for a peer that takes no central now.
pub const ADVERT_FULL: u8 = 1;
/// A signal's inner kind: a room hello.
pub const INNER_ROOM: u8 = 1;
/// A signal's inner kind: a message sealed to the receiver, such as an offer.
pub const INNER_PEER: u8 = 2;
/// A signal's inner kind: the sender's identity proof.
pub const INNER_ID: u8 = 3;
/// A connection that hasn't proven the room, and brought the other end's identity proof, this long after it opened
/// closes.
pub const PROVE_WITHIN: Duration = Duration::from_secs(3);
/// Messages dropped or refused on a connection before it closes.
pub const MAX_DROPS: u64 = 32;
/// Signals taken on a connection: a burst, then one per `SIGNAL_REFILL`; the rest dropped before any cryptography.
const SIGNAL_BURST: u32 = 16;
const SIGNAL_REFILL: Duration = Duration::from_millis(500);
/// New handshakes (HS1s) on a connection bound to a peer: a burst, then one per `HS1_REFILL`.
const HS1_BURST: u32 = 4;
const HS1_REFILL: Duration = Duration::from_secs(5);

/// The room's ble key: `sha256("peerwire/room-ble/" + app + "/" + room)`, the room's name hashed under a label of its
/// own, never the topic's.
pub fn ble_key(app_id: &str, room: &str) -> [u8; 32] {
    Sha256::digest(format!("peerwire/room-ble/{app_id}/{room}")).into()
}

/// The service data a peer advertises: flags ([`ADVERT_FULL`]), `n`, and the first 4 bytes of
/// `HMAC-SHA256(ble key, "pw-ble-adv" ‖ n)`, so only a member tells the room's adverts.
pub fn advert(key: &[u8; 32], n: u32, full: bool) -> [u8; ADVERT_LEN] {
    let mut a = [0u8; ADVERT_LEN];
    a[0] = if full { ADVERT_FULL } else { 0 };
    a[1..5].copy_from_slice(&n.to_le_bytes());
    a[5..].copy_from_slice(&mac(key, &[b"pw-ble-adv", &n.to_le_bytes()])[..4]);
    a
}

fn mac(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// What an identity proof signs: the connection's challenges, `c_p ‖ c_c`, under a label of its own.
fn id_message(transcript: &[u8; 2 * CHALLENGE_LEN]) -> Vec<u8> {
    [&b"pw-ble-id"[..], transcript].concat()
}

/// The GCM nonce of signal `seq` sealed by the central (`central`) or the peripheral: 4 bytes of direction (0 from the
/// central, 1 from the peripheral) and the 8 of `seq`.
fn nonce(central: bool, seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&u32::from(!central).to_le_bytes());
    n[4..].copy_from_slice(&seq.to_le_bytes());
    n
}

/// `plain` sealed as signal `seq` under link key `key` by the central (`central`) or the peripheral.
pub fn seal(key: &[u8; 32], central: bool, seq: u64, plain: &[u8]) -> Option<Vec<u8>> {
    Aes256Gcm::new(key.into()).encrypt(Nonce::from_slice(&nonce(central, seq)), plain).ok()
}

/// The plaintext of signal `seq`, sealed under link key `key` by the central (`central`) or the peripheral.
pub fn open(key: &[u8; 32], central: bool, seq: u64, sealed: &[u8]) -> Option<Vec<u8>> {
    Aes256Gcm::new(key.into()).decrypt(Nonce::from_slice(&nonce(central, seq)), sealed).ok()
}

/// The `PeerId` and signature an identity proof (a signal of inner kind [`INNER_ID`]) names, if it is one; it proves
/// nothing until [`Link::verifies`] says so.
pub fn identity(payload: &[u8]) -> Option<(PeerId, [u8; 64])> {
    let payload: &[u8; ID_LEN] = payload.try_into().ok()?;
    Some((PeerId(payload[..32].try_into().ok()?), payload[32..].try_into().ok()?))
}

/// A token bucket.
#[derive(Clone, Copy, Debug, Default)]
pub struct Bucket {
    tokens: u32,
    at: Option<Instant>,
}

impl Bucket {
    /// Takes a token: a full `burst` at first, then one more each `refill`.
    pub fn take(&mut self, now: Instant, burst: u32, refill: Duration) -> bool {
        match self.at {
            None => (self.tokens, self.at) = (burst, Some(now)),
            Some(at) => {
                let fresh = now.saturating_duration_since(at).as_micros() / refill.as_micros().max(1);
                let fresh = u32::try_from(fresh).unwrap_or(u32::MAX);
                if fresh > 0 {
                    self.tokens = self.tokens.saturating_add(fresh).min(burst);
                    self.at = Some(if self.tokens == burst { now } else { at + refill * fresh });
                }
            }
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

/// Why a connection must close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The central's answer to the challenge was wrong: it doesn't hold the room's ble key.
    WrongAnswer,
    /// The peripheral's proof was wrong.
    WrongProof,
    /// Something other than the room proof came before it.
    Early,
    /// [`MAX_DROPS`] messages were dropped or refused on it.
    Drops,
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Fault::WrongAnswer => "a wrong answer to the room challenge",
            Fault::WrongProof => "a wrong room proof",
            Fault::Early => "something else came before the room proof",
            Fault::Drops => "too many values dropped or refused",
        })
    }
}

/// What a value that came on a connection brings its host ([`Link::push`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// Nothing for the host: part of a message, a step of the room proof, or a message dropped and counted.
    Nothing,
    /// The room proof completed: both ends hold the room's ble key, and signals go under the link key. The host sends
    /// its identity proof now ([`Link::send_identity`]).
    Proven,
    /// A signal, opened: its inner kind ([`INNER_ROOM`], [`INNER_PEER`], [`INNER_ID`] or another, for the host to drop)
    /// and the rest.
    Signal(u8, Vec<u8>),
    /// A datagram: one Peerwire packet, which only a binding or the identity proven on the connection may let in.
    Datagram(Vec<u8>),
    /// The connection must close.
    Close(Fault),
}

/// Where a connection's room proof stands.
enum State {
    /// The peripheral sent this challenge.
    Challenged([u8; CHALLENGE_LEN]),
    /// The central waits for the challenge, with its own to answer it with.
    Awaiting([u8; CHALLENGE_LEN]),
    /// The central answered `c_p` with `c_c`, and waits for the proof.
    Answered([u8; CHALLENGE_LEN], [u8; CHALLENGE_LEN]),
    /// Both ends hold the room's ble key: signals go under `key`. `transcript` is `c_p ‖ c_c`.
    Proven { key: [u8; 32], tx: u64, rx: u64, transcript: [u8; 2 * CHALLENGE_LEN] },
}

/// One connection's protocol, at either end: the room proof, then signals and datagrams, in chunks.
///
/// The peripheral sends a challenge as the connection opens, the central answers it, and the peripheral proves it in
/// turn: from then on ([`Input::Proven`]) each end sends its identity proof, then signals sealed under the link key and
/// datagrams. Until then only those three messages pass; anything else closes the connection. Values to send queue in
/// the link, [`Link::value`] first.
pub struct Link {
    central: bool,
    /// The room's ble key.
    room: [u8; 32],
    state: State,
    mtu: u16,
    chunker: Chunker,
    reassembler: Reassembler,
    queue: VecDeque<Vec<u8>>,
    max_queue: usize,
    opened: Instant,
    signals: Bucket,
    /// The last HS1 taken while bound, and their budget.
    last_hs1: Option<(u32, [u8; 32])>,
    hs1s: Bucket,
    drops: u64,
    /// The reassembler's drops already counted in `drops`.
    counted: u64,
}

impl Link {
    /// The central's end of a connection it made at `now`, in the room whose ble key is `room`, with the challenge
    /// `c_c` (16 random bytes) to answer the peripheral's with, ATT MTU `mtu` and room for `queue` values to send.
    pub fn central(room: [u8; 32], c_c: [u8; CHALLENGE_LEN], mtu: u16, queue: usize, now: Instant) -> Link {
        Link::new(true, room, State::Awaiting(c_c), mtu, queue, now)
    }

    /// The peripheral's end of a connection a central made at `now`: as [`Link::central`], with the challenge `c_p` (16
    /// random bytes), which it sends at once.
    pub fn peripheral(room: [u8; 32], c_p: [u8; CHALLENGE_LEN], mtu: u16, queue: usize, now: Instant) -> Link {
        let mut link = Link::new(false, room, State::Challenged(c_p), mtu, queue, now);
        link.send(&[&[KIND_CHALLENGE], &c_p]);
        link
    }

    fn new(central: bool, room: [u8; 32], state: State, mtu: u16, max_queue: usize, opened: Instant) -> Link {
        Link {
            central,
            room,
            state,
            mtu,
            chunker: Chunker::new(),
            reassembler: Reassembler::new(),
            queue: VecDeque::new(),
            max_queue,
            opened,
            signals: Bucket::default(),
            last_hs1: None,
            hs1s: Bucket::default(),
            drops: 0,
            counted: 0,
        }
    }

    /// Whether this end is the central.
    pub fn is_central(&self) -> bool {
        self.central
    }

    /// When the connection opened.
    pub fn opened(&self) -> Instant {
        self.opened
    }

    /// The ATT MTU.
    pub fn mtu(&self) -> u16 {
        self.mtu
    }

    /// The ATT MTU changed: values sent from now on are cut to it.
    pub fn set_mtu(&mut self, mtu: u16) {
        self.mtu = mtu;
    }

    /// Whether the room proof completed.
    pub fn proven(&self) -> bool {
        matches!(self.state, State::Proven { .. })
    }

    /// Whether a challenge was sent or came: false only for a central still waiting for one.
    pub fn challenged(&self) -> bool {
        !matches!(self.state, State::Awaiting(_))
    }

    /// Takes one GATT value that came at `now`, and runs the room proof as its messages come: an answer or a proof to
    /// send queues. A message that is no use once the room is proven (another kind, an empty datagram, a signal over
    /// the budget, out of sequence or that doesn't open) is dropped and counted, as are the codec's drops.
    pub fn push(&mut self, now: Instant, value: &[u8]) -> Input {
        let message = self.reassembler.push(now, value).map(<[u8]>::to_vec);
        let fresh = self.reassembler.dropped() - self.counted;
        self.counted += fresh;
        if self.count(fresh) {
            return Input::Close(Fault::Drops);
        }
        match message {
            Some(m) => self.on_message(now, &m),
            None => Input::Nothing,
        }
    }

    fn on_message(&mut self, now: Instant, m: &[u8]) -> Input {
        let (kind, body) = (m[0], &m[1..]);
        match (&self.state, kind) {
            (State::Challenged(c_p), KIND_ANSWER) if body.len() == CHALLENGE_LEN + MAC_LEN => {
                let (c_p, c_c) = (*c_p, <[u8; CHALLENGE_LEN]>::try_from(&body[..CHALLENGE_LEN]).expect("16 bytes"));
                if !same(&mac(&self.room, &[b"pw-ble-c", &c_p, &c_c]), &body[CHALLENGE_LEN..]) {
                    return Input::Close(Fault::WrongAnswer);
                }
                let proof = mac(&self.room, &[b"pw-ble-p", &c_c, &c_p]);
                self.send(&[&[KIND_PROOF], &proof]);
                self.prove(c_p, c_c)
            }
            (State::Awaiting(c_c), KIND_CHALLENGE) if body.len() == CHALLENGE_LEN => {
                let (c_p, c_c) = (<[u8; CHALLENGE_LEN]>::try_from(body).expect("16 bytes"), *c_c);
                let answer = mac(&self.room, &[b"pw-ble-c", &c_p, &c_c]);
                self.state = State::Answered(c_p, c_c);
                self.send(&[&[KIND_ANSWER], &c_c, &answer]);
                Input::Nothing
            }
            (State::Answered(c_p, c_c), KIND_PROOF) if body.len() == MAC_LEN => {
                let (c_p, c_c) = (*c_p, *c_c);
                if !same(&mac(&self.room, &[b"pw-ble-p", &c_c, &c_p]), body) {
                    return Input::Close(Fault::WrongProof);
                }
                self.prove(c_p, c_c)
            }
            (State::Proven { .. }, KIND_SIGNAL) => self.on_signal(now, body),
            (State::Proven { .. }, KIND_DATAGRAM) if !body.is_empty() => Input::Datagram(body.to_vec()),
            (State::Proven { .. }, _) => self.dropped(),
            // Before the proof, nothing else: a connection that tries closes.
            _ => Input::Close(Fault::Early),
        }
    }

    /// The room is proven with challenges `c_p` and `c_c`: the link key is `HMAC-SHA256(ble key, "pw-ble-k" ‖ c_p ‖
    /// c_c)`.
    fn prove(&mut self, c_p: [u8; CHALLENGE_LEN], c_c: [u8; CHALLENGE_LEN]) -> Input {
        let mut transcript = [0u8; 2 * CHALLENGE_LEN];
        transcript[..CHALLENGE_LEN].copy_from_slice(&c_p);
        transcript[CHALLENGE_LEN..].copy_from_slice(&c_c);
        let key = mac(&self.room, &[b"pw-ble-k", &transcript]);
        self.state = State::Proven { key, tx: 0, rx: 0, transcript };
        Input::Proven
    }

    /// A signal from the other end: `seq`, then the sealed `inner ‖ payload`. Taken within the budget, before any
    /// cryptography, and only with a `seq` above the last taken.
    fn on_signal(&mut self, now: Instant, body: &[u8]) -> Input {
        if body.len() < 8 || !self.signals.take(now, SIGNAL_BURST, SIGNAL_REFILL) {
            return self.dropped();
        }
        let from_central = !self.central;
        let State::Proven { key, rx, .. } = &mut self.state else { return Input::Nothing };
        let seq = u64::from_le_bytes(body[..8].try_into().expect("8 bytes"));
        let opened = (seq > *rx).then(|| open(key, from_central, seq, &body[8..])).flatten();
        let Some(mut plain) = opened.filter(|p| !p.is_empty()) else { return self.dropped() };
        *rx = seq;
        let inner = plain.remove(0);
        Input::Signal(inner, plain)
    }

    fn dropped(&mut self) -> Input {
        match self.count(1) {
            true => Input::Close(Fault::Drops),
            false => Input::Nothing,
        }
    }

    /// Counts `n` more messages dropped or refused on the connection (by the host's own rules too); whether it must
    /// close now ([`MAX_DROPS`]).
    pub fn count(&mut self, n: u64) -> bool {
        self.drops += n;
        self.drops >= MAX_DROPS
    }

    /// Sends this end's identity proof, `me` signed over the connection's challenges, as its next signal. False before
    /// the room is proven, or with no room in the queue.
    pub fn send_identity(&mut self, me: &Identity) -> bool {
        let State::Proven { transcript, .. } = &self.state else { return false };
        let sig = me.sign(&id_message(transcript));
        self.signal(INNER_ID, &[&me.peer_id().as_bytes()[..], &sig].concat())
    }

    /// Whether `sig` proves that `peer` is at the other end: its signature over this connection's challenges. The host
    /// also refuses a proof in its own name.
    pub fn verifies(&self, peer: &PeerId, sig: &[u8; 64]) -> bool {
        let State::Proven { transcript, .. } = &self.state else { return false };
        peer.verify(&id_message(transcript), sig)
    }

    /// Seals `inner ‖ payload` under the link key, as this end's next signal, and queues it. False before the room is
    /// proven, or with no room in the queue.
    pub fn signal(&mut self, inner: u8, payload: &[u8]) -> bool {
        let central = self.central;
        let State::Proven { key, tx, .. } = &mut self.state else { return false };
        *tx += 1;
        let Some(sealed) = seal(key, central, *tx, &[&[inner][..], payload].concat()) else { return false };
        let seq = tx.to_le_bytes();
        self.send(&[&[KIND_SIGNAL], &seq, &sealed])
    }

    /// Queues a datagram (one Peerwire packet). False before the room is proven, for an empty one or one over
    /// [`MAX_MESSAGE`], or with no room in the queue.
    pub fn datagram(&mut self, d: &[u8]) -> bool {
        self.proven() && !d.is_empty() && self.send(&[&[KIND_DATAGRAM], d])
    }

    /// Whether to take `datagram` on a connection bound to a peer: an HS1 other than a resend of the last one draws on
    /// the connection's budget, a burst of 4 and then one per 5 s; anything else passes.
    pub fn take_bound(&mut self, now: Instant, datagram: &[u8]) -> bool {
        let Some(id) = hs1_id(datagram).filter(|id| self.last_hs1 != Some(*id)) else { return true };
        if !self.hs1s.take(now, HS1_BURST, HS1_REFILL) {
            return false;
        }
        self.last_hs1 = Some(id);
        true
    }

    /// Queues a message, in its chunks: the whole of it, or none if the queue lacks room.
    fn send(&mut self, parts: &[&[u8]]) -> bool {
        let m = parts.concat();
        let room = value_len(self.mtu) - 1;
        if m.len() > MAX_MESSAGE || self.queue.len() + m.len().div_ceil(room) > self.max_queue {
            return false;
        }
        let queue = &mut self.queue;
        self.chunker.split(&m, self.mtu, |v| {
            queue.push_back(v.to_vec());
            true
        })
    }

    /// The next GATT value to send, if any: once the radio took it, [`Link::sent`].
    pub fn value(&self) -> Option<&[u8]> {
        self.queue.front().map(Vec::as_slice)
    }

    /// The radio took the value [`Link::value`] gave.
    pub fn sent(&mut self) {
        self.queue.pop_front();
    }

    /// Values queued to send.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Endpoint, Lane};

    const T0: Instant = Instant::ZERO;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn key() -> [u8; 32] {
        ble_key("app", "room")
    }

    /// A whole message in one value, with the chunk counter `link` takes next (any but the last it took).
    fn whole(link: &Link, m: &[u8]) -> Vec<u8> {
        [&[0xc0 | link.reassembler.last.map_or(0, |n| (n + 1) & 0x3f)][..], m].concat()
    }

    fn pair(mtu: u16) -> (Link, Link) {
        (Link::central(key(), [9; 16], mtu, 64, T0), Link::peripheral(key(), [3; 16], mtu, 64, T0))
    }

    /// Moves every value `from` has queued to `to`, and gives what `to` made of them.
    fn carry(from: &mut Link, to: &mut Link, now: Instant) -> Vec<Input> {
        let mut got = Vec::new();
        while let Some(v) = from.value().map(<[u8]>::to_vec) {
            from.sent();
            got.push(to.push(now, &v));
        }
        got.retain(|i| *i != Input::Nothing);
        got
    }

    /// Runs the room proof both ways; each end then sends its identity proof.
    fn prove(c: &mut Link, p: &mut Link) {
        assert_eq!(carry(p, c, T0), vec![], "the challenge");
        assert_eq!(carry(c, p, T0), vec![Input::Proven], "the answer");
        assert_eq!(carry(p, c, T0), vec![Input::Proven], "the proof");
    }

    /// The bytes on the wire are those `docs/concepts/bluetooth.md` gives, as the node made them before the protocol
    /// moved here: change them and every peer in the field stops linking.
    #[test]
    fn the_bytes_match_the_wire_format() {
        let key = key();
        assert_eq!(key[..], hex("213763244cd1d85414b1132d397f04e95628e554651fbdf0f19e88c15efa1d13"));
        assert_eq!(advert(&key, 7, true)[..], hex("0107000000d5221683"));
        let (mut c, mut p) = pair(517);
        // Each message in one chunk: the header says first, last and the chunk's counter.
        assert_eq!(p.value().unwrap(), [&[0xc0, KIND_CHALLENGE][..], &[3; 16]].concat(), "kind 3 and c_p");
        carry(&mut p, &mut c, T0);
        let answer = c.value().unwrap().to_vec();
        let mac = hex("b7d4c3c5cb5e16888ae686c16d95263d045b84f6eaa079b681f0ef72f651ecbe");
        assert_eq!(answer, [&[0xc0, KIND_ANSWER][..], &[9; 16], &mac].concat(), "kind 4, c_c and the HMAC");
        carry(&mut c, &mut p, T0);
        let proof = hex("ce9873ba04d26cfb3d2cfdd3577d077aeaedee884a76327a5f45ad3874ce2634");
        assert_eq!(p.value().unwrap(), [&[0xc1, KIND_PROOF][..], &proof].concat());
        carry(&mut p, &mut c, T0);
        let link = hex("2908364657d82870cd5a6096f19fbdab9e15eb7736e94667e2e43376b6134f32");
        assert!(c.signal(INNER_ROOM, b"hi"));
        let sealed = hex("12cc1e6c867aa32e312180e9b88a026e2ef0a1");
        assert_eq!(c.value().unwrap(), [&[0xc1, KIND_SIGNAL][..], &1u64.to_le_bytes(), &sealed].concat());
        assert_eq!(open(&link.clone().try_into().unwrap(), true, 1, &sealed), Some(vec![INNER_ROOM, b'h', b'i']));
        let back = hex("b5a6b098c3749f6dead50df74f0b8f73eb16");
        assert_eq!(seal(&link.try_into().unwrap(), false, 2, &[INNER_PEER, 7]), Some(back));
        let me = Identity::from_secret_bytes(&[1; 32]);
        let sig = hex(concat!(
            "4b93d92f2be309195ce1c7ac89dd9ce1940acb6ee1b8775d2e27ba1eb634805a",
            "67d134e467abc677c6437b3ead796971b3e61df9e46ef1720ad8ecb8704c1e0a"
        ));
        assert!(p.verifies(&me.peer_id(), &sig.try_into().unwrap()), "an identity proof made before the move");
    }

    #[test]
    fn both_ends_prove_the_room_and_who_they_are_then_carry_signals_and_datagrams() {
        let (mut c, mut p) = pair(23);
        assert!(!c.challenged() && p.challenged());
        assert!(
            !c.signal(INNER_ROOM, b"x")
                && !c.datagram(b"x")
                && !c.send_identity(&Identity::from_secret_bytes(&[1; 32]))
        );
        prove(&mut c, &mut p);
        assert!(c.challenged() && c.proven() && p.proven());
        let (a, b) = (Identity::from_secret_bytes(&[1; 32]), Identity::from_secret_bytes(&[2; 32]));
        assert!(c.send_identity(&a) && p.send_identity(&b));
        let Input::Signal(INNER_ID, from_c) = carry(&mut c, &mut p, T0).remove(0) else { panic!() };
        let Input::Signal(INNER_ID, from_p) = carry(&mut p, &mut c, T0).remove(0) else { panic!() };
        let ((pa, sa), (pb, sb)) = (identity(&from_c).unwrap(), identity(&from_p).unwrap());
        assert_eq!((pa, pb), (a.peer_id(), b.peer_id()));
        assert!(p.verifies(&pa, &sa) && c.verifies(&pb, &sb));
        assert!(!p.verifies(&pb, &sa), "another's name on it");
        assert_eq!(identity(&from_c[1..]), None);
        assert!(p.signal(INNER_PEER, &[7; 300]) && c.datagram(&[5; 1200]));
        assert_eq!(carry(&mut p, &mut c, T0), vec![Input::Signal(INNER_PEER, vec![7; 300])]);
        assert_eq!(carry(&mut c, &mut p, T0), vec![Input::Datagram(vec![5; 1200])]);
        assert!(!c.datagram(&[]) && !c.datagram(&[0; MAX_MESSAGE]), "empty, or over the largest message");
    }

    /// An identity proof made for one connection proves nothing on another: it signs that one's challenges.
    #[test]
    fn an_identity_proof_is_for_its_connection_alone() {
        let me = Identity::from_secret_bytes(&[1; 32]);
        let (mut c1, mut p1) = pair(23);
        prove(&mut c1, &mut p1);
        let (mut c2, mut p2) =
            (Link::central(key(), [8; 16], 23, 64, T0), Link::peripheral(key(), [3; 16], 23, 64, T0));
        prove(&mut c2, &mut p2);
        c1.send_identity(&me);
        let Input::Signal(_, proof) = carry(&mut c1, &mut p1, T0).remove(0) else { panic!() };
        let (peer, sig) = identity(&proof).unwrap();
        assert!(p1.verifies(&peer, &sig) && !p2.verifies(&peer, &sig));
    }

    #[test]
    fn another_rooms_central_is_closed_and_so_is_a_central_given_a_wrong_proof() {
        let mut p = Link::peripheral(key(), [3; 16], 517, 64, T0);
        let mut c = Link::central(ble_key("app", "other"), [9; 16], 517, 64, T0);
        carry(&mut p, &mut c, T0);
        assert_eq!(carry(&mut c, &mut p, T0), vec![Input::Close(Fault::WrongAnswer)]);
        assert!(!p.proven());
        // A peripheral of another room proves under its own key: the central's check fails.
        let mut c = Link::central(key(), [9; 16], 517, 64, T0);
        carry(&mut Link::peripheral(key(), [3; 16], 517, 64, T0), &mut c, T0);
        let wrong = mac(&ble_key("app", "other"), &[b"pw-ble-p", &[9; 16], &[3; 16]]);
        assert_eq!(c.push(T0, &whole(&c, &[&[KIND_PROOF][..], &wrong].concat())), Input::Close(Fault::WrongProof));
        assert!(!c.proven());
    }

    #[test]
    fn nothing_but_the_proof_passes_before_it() {
        for early in [
            &[KIND_SIGNAL, 1, 0, 0, 0, 0, 0, 0, 0, 9][..],
            &[KIND_DATAGRAM, 1],
            &[KIND_PROOF; 33],
            &[KIND_CHALLENGE; 17],
            &[KIND_ANSWER; 10],
            &[200],
        ] {
            let mut p = Link::peripheral(key(), [3; 16], 23, 64, T0);
            let mut tx = Chunker::new();
            let mut got = Vec::new();
            tx.split(early, 23, |v| {
                got.push(p.push(T0, v));
                true
            });
            assert_eq!(got.pop(), Some(Input::Close(Fault::Early)), "{early:?}");
        }
        let mut c = Link::central(key(), [9; 16], 23, 64, T0);
        let mut tx = Chunker::new();
        let mut got = None;
        tx.split(&[KIND_CHALLENGE; 16], 23, |v| {
            got = Some(c.push(T0, v));
            true
        });
        assert_eq!(got, Some(Input::Close(Fault::Early)), "a challenge one byte short");
    }

    #[test]
    fn signals_go_up_in_seq_and_seal_per_direction() {
        let (mut c, mut p) = pair(517);
        prove(&mut c, &mut p);
        c.signal(INNER_ROOM, b"a");
        c.signal(INNER_ROOM, b"b");
        let sent: Vec<Vec<u8>> = std::iter::from_fn(|| c.value().map(<[u8]>::to_vec).inspect(|_| c.sent())).collect();
        assert_eq!(p.push(T0, &sent[1]), Input::Signal(INNER_ROOM, b"b".to_vec()), "a gap in seq is fine");
        // An earlier one, or the same again (with a fresh chunk counter, so the codec takes it): its seq is spent.
        assert_eq!(p.push(T0, &whole(&p, &sent[0][1..])), Input::Nothing);
        assert_eq!(p.push(T0, &whole(&p, &sent[1][1..])), Input::Nothing);
        assert_eq!(p.drops, 2);
        let State::Proven { key, .. } = p.state else { panic!() };
        let s = seal(&key, true, 1, b"hello").unwrap();
        assert_eq!(open(&key, true, 1, &s).as_deref(), Some(&b"hello"[..]));
        assert_eq!(open(&key, false, 1, &s), None, "the other direction's nonce");
        assert_eq!(open(&key, true, 2, &s), None, "another seq");
        assert_eq!(open(&[8u8; 32], true, 1, &s), None, "another connection's key");
        let mine = seal(&key, false, 5, &[INNER_ROOM, 1]).unwrap();
        let back = whole(&p, &[&[KIND_SIGNAL][..], &5u64.to_le_bytes(), &mine].concat());
        assert_eq!(p.push(T0, &back), Input::Nothing, "its own direction's seal, sent back to it");
        assert_eq!(p.drops, 3);
        let empty = seal(&key, true, 9, &[]).unwrap();
        assert_eq!(p.push(T0, &whole(&p, &[&[KIND_SIGNAL][..], &9u64.to_le_bytes(), &empty].concat())), Input::Nothing);
        assert_eq!(p.drops, 4, "a signal with no inner kind");
    }

    #[test]
    fn signals_over_the_budget_are_dropped_before_any_cryptography() {
        let (mut c, mut p) = pair(517);
        prove(&mut c, &mut p);
        for _ in 0..20 {
            c.signal(INNER_ROOM, b"x");
        }
        let got = carry(&mut c, &mut p, T0);
        assert_eq!(got.len(), 16, "a burst of 16");
        assert_eq!(p.drops, 4);
        c.signal(INNER_ROOM, b"y");
        assert_eq!(carry(&mut c, &mut p, Instant::from_millis(500)), vec![Input::Signal(INNER_ROOM, b"y".to_vec())]);
    }

    #[test]
    fn a_connection_closes_at_its_drops() {
        let (mut c, mut p) = pair(23);
        prove(&mut c, &mut p);
        assert!(!p.count(MAX_DROPS - 2));
        c.datagram(&[1]);
        let mut v = c.value().unwrap().to_vec();
        c.sent();
        v[1] = 9; // a kind nobody knows
        assert_eq!(p.push(T0, &v), Input::Nothing);
        v[0] = (v[0] & 0xc0) | 2;
        assert_eq!(p.push(T0, &v), Input::Close(Fault::Drops), "the 32nd");
        let (mut c, mut p) = pair(23);
        prove(&mut c, &mut p);
        p.count(MAX_DROPS - 1);
        assert_eq!(p.push(T0, &[0x05, 1]), Input::Close(Fault::Drops), "a codec drop counts as well");
    }

    #[test]
    fn a_full_queue_takes_no_part_of_a_message() {
        let mut c = Link::central(key(), [9; 16], 23, 4, T0);
        let mut p = Link::peripheral(key(), [3; 16], 23, 64, T0);
        prove(&mut c, &mut p);
        assert!(c.datagram(&[1; 19 * 3]));
        assert_eq!(c.queued(), 4, "4 chunks: the kind byte takes a place");
        assert!(!c.datagram(&[1]));
        c.sent();
        assert!(c.datagram(&[1]) && c.queued() == 4);
        c.set_mtu(185);
        assert_eq!(c.mtu(), 185);
    }

    #[test]
    fn a_bound_connection_takes_new_handshakes_within_its_budget() {
        let ids: Vec<Identity> = (1..=2u8).map(|i| Identity::from_secret_bytes(&[i; 32])).collect();
        let (mut c, _) = pair(23);
        let hs1 = |seed: u8| {
            let mut ex = Endpoint::<u32>::new(&ids[0], Config::default(), [seed; 32]);
            ex.connect(T0, 2, ids[1].peer_id()).unwrap();
            ex.poll_transmit(T0).unwrap().payload
        };
        assert!(c.take_bound(T0, b"not a handshake"));
        let first = hs1(1);
        assert!((0..10).all(|_| c.take_bound(T0, &first)), "its resends pass");
        assert!((2..=4).all(|s| c.take_bound(T0, &hs1(s))));
        assert!(!c.take_bound(T0, &hs1(5)), "a fifth new one at once");
        assert!(c.take_bound(Instant::from_millis(5000), &hs1(6)));
        let mut ey = Endpoint::<u32>::new(&ids[1], Config::default(), [7; 32]);
        ey.handle_datagram(T0, 1, &first);
        let _ = ey.send(&ids[0].peer_id(), Lane::Reliable, b"x");
        assert!(c.take_bound(T0, &ey.poll_transmit(T0).unwrap().payload), "an HS2 isn't counted");
    }

    #[test]
    fn a_bucket_gives_its_burst_then_refills() {
        let mut b = Bucket::default();
        let ms = Duration::from_millis;
        assert!((0..3).all(|_| b.take(T0, 3, ms(100))));
        assert!(!b.take(T0, 3, ms(100)));
        assert!(!b.take(Instant::from_millis(99), 3, ms(100)));
        assert!(b.take(Instant::from_millis(100), 3, ms(100)));
        assert!(!b.take(Instant::from_millis(150), 3, ms(100)));
        assert!(b.take(Instant::from_millis(200), 3, ms(100)), "the refill keeps its phase");
        assert!((0..3).all(|_| b.take(Instant::from_millis(10_000), 3, ms(100))), "a long wait refills to the burst");
        assert!(!b.take(Instant::from_millis(10_000), 3, ms(100)));
    }

    /// Property: arbitrary values at arbitrary times, at either end, before and after the proof, never panic, and
    /// every `Close` is for a reason the docs give.
    #[test]
    fn prop_arbitrary_values_never_panic() {
        use rand_chacha::ChaCha8Rng;
        use rand_core::{RngCore, SeedableRng};
        let mut rng = ChaCha8Rng::seed_from_u64(1100);
        for round in 0..400 {
            let (mut c, mut p) = pair(23 + (round % 50) as u16);
            if round % 2 == 0 {
                prove(&mut c, &mut p);
            }
            let mut now = 0;
            for _ in 0..200 {
                let mut v = vec![0u8; (rng.next_u32() % 40) as usize];
                rng.fill_bytes(&mut v);
                if !v.is_empty() && rng.next_u32() % 2 == 0 {
                    v[0] |= 0xc0; // a whole message in one chunk, to reach the message handling
                }
                now += u64::from(rng.next_u32() % 100);
                let end = if rng.next_u32() % 2 == 0 { &mut c } else { &mut p };
                if let Input::Close(_) = end.push(Instant::from_millis(now), &v) {
                    break;
                }
            }
        }
    }
}
