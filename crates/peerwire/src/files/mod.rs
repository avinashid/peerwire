//! Peerwire Share in Rust (feature `files`): files of any size, sent peer to peer in checked pieces, in the protocol
//! of the Share page (`packages/js/demo/share/`), byte for byte. `docs/concepts/share-protocol.md` is the protocol;
//! [`frame`] has its bytes and checks. A browser and a native node can't link yet, so today native apps share with
//! each other; the protocol is the page's all the same (`packages/js/test/share_native.mjs` checks it).
//!
//! A sender offers a file to a peer; the receiver accepts (and picks where it goes) or declines; the file goes in
//! pieces of 512 KiB on the reliable lane, at most [`WINDOW`] in flight to a peer, each with its SHA-256, and the
//! whole file's SHA-256 at the end. The receiver takes pieces only in order and only for a transfer it accepted,
//! writes each before it acknowledges it, and keeps the file only once the whole hash matches. Neither side holds
//! more than a piece of a file at a time ([`Source`], [`Sink`], [`FileSink`]). Data goes only while the link to the
//! peer is direct ([`Wire::carries_files`]); after a link drops, the two go on from the last piece written.
//!
//! [`Files`] is sans-IO like `peerwire_sync::Docs`: the host hands it the node's events and the time, and it sends
//! through a [`Wire`], the one seam between the protocol and the link (today the room's reliable messages, as the
//! page sends them: [`impl Wire for Node`](Wire#impl-Wire-for-Node)).
//!
//! ```no_run
//! use peerwire::files::{End, Event as FileEvent, Files};
//! use peerwire::{Event, Node};
//! use std::time::Instant;
//!
//! let mut node = Node::bind("[::]:0")?;
//! let mut files = Files::new();
//! loop {
//!     node.poll();
//!     let now = Instant::now();
//!     while let Some(e) = node.try_recv_event() {
//!         if files.handle_event(now, &e) {
//!             continue; // a Share message
//!         }
//!         if let Event::Connected { peer, .. } = e {
//!             files.offer_file(peer, "photo.jpg".as_ref())?;
//!         }
//!     }
//!     files.poll(now, &mut node);
//!     while let Some(e) = files.poll_event() {
//!         match e {
//!             FileEvent::Offer { id, .. } => files.accept_into(id, "downloads".as_ref())?,
//!             FileEvent::Ended { id, end: End::Done { sha256 } } => println!("{id:?} verified: {sha256}"),
//!             _ => {}
//!         }
//!     }
//!     # break;
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod frame;
mod json;
mod sink;
#[cfg(test)]
mod tests;
mod unicode;

pub use frame::{
    clean_name, clean_type, Offer, Refused, WireId, MAX_CONTROL, MAX_NAME, MAX_PIECE, MAX_QUEUED, MAX_RAW_NAME,
    MAX_SIZE, MAX_TYPE, MIN_PIECE, PIECE, WINDOW,
};
pub use sink::{FileSink, Sink, Source};

use crate::{DisconnectReason, Lane, Node, PathKind, PeerId, SendError};
use frame::{Control, Message, Piece};
use json::Value;
use rand_chacha::rand_core::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a transfer waits for its peer to come back after its link dropped.
pub const RESUME: Duration = Duration::from_secs(10 * 60);
/// Offers from one peer waiting for an answer; more are declined.
pub const MAX_OFFERS: usize = 32;
/// Pieces refused in one transfer before it fails.
pub const MAX_REJECTS: u32 = 8;
/// Finished transfers kept (for [`Files::info`], and to answer a sender back on a new link); the oldest go first,
/// and only once [`Files::poll_event`] has handed out their [`Event::Ended`].
pub const MAX_KEPT: usize = 200;
/// Control messages held for one peer while its link takes none (its send buffer full); past that the newest are
/// dropped. A peer that stops reading gets nothing more to make this side hold.
pub const MAX_OUTBOX: usize = 256;
/// The room message tag of bytes (room.js `T.Bytes`): a Share frame goes in a room message as this, then the frame.
pub const ROOM_BYTES: u8 = 0;

// ---------- the seam ----------

/// Where Share frames go: the one seam between the protocol and the link. Today [`Node`] carries them as room
/// messages on the reliable lane, as the Share page sends them; a dedicated file lane would change only this.
pub trait Wire {
    /// Sends one Share frame to `peer`, reliably and in order with the others sent to it. `WouldBlock` when the link
    /// can't take it now: it is offered again on a later [`Files::poll`]. Any other error drops it.
    fn send(&mut self, peer: &PeerId, frame: &[u8]) -> Result<(), SendError>;
    /// Whether file data (pieces, and the end that follows them) may go to `peer` now. Control messages go
    /// whatever this says.
    fn carries_files(&self, peer: &PeerId) -> bool;
}

/// Share frames as room messages ([`room_message`]) on [`Lane::Reliable`]. Pieces go only while the link is direct:
/// [`PathKind::Direct`] or [`PathKind::Birthday`], never through another peer, a relay peer, TURN or the signalling
/// relays, as the page sends them only over a peer's own WebRTC link (files.js `directForFiles`).
impl Wire for Node {
    fn send(&mut self, peer: &PeerId, frame: &[u8]) -> Result<(), SendError> {
        Node::send(self, peer, Lane::Reliable, &room_message(frame))
    }

    fn carries_files(&self, peer: &PeerId) -> bool {
        matches!(self.path_kind(peer), Some(PathKind::Direct | PathKind::Birthday))
    }
}

/// A Share frame as a room message, as room.js sends bytes: [`ROOM_BYTES`], then the frame.
pub fn room_message(frame: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(1 + frame.len());
    m.push(ROOM_BYTES);
    m.extend_from_slice(frame);
    m
}

/// The Share frame in a room message, if it is one: [`ROOM_BYTES`], then a piece or a control frame.
pub fn share_frame(message: &[u8]) -> Option<&[u8]> {
    match message {
        [ROOM_BYTES, kind, ..] if *kind == frame::PIECE_FRAME || *kind == frame::CONTROL_FRAME => Some(&message[1..]),
        _ => None,
    }
}

// ---------- the API ----------

/// A transfer, as this side numbers them (sent or received).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransferId(pub u64);

/// Which way a transfer goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Send,
    Receive,
}

/// Where a transfer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Offered, not answered yet.
    Offered,
    /// Accepted: going out.
    Sending,
    /// Accepted: coming in.
    Receiving,
    /// Delivered: the receiver's SHA-256 of the whole file matched the sender's.
    Done,
    Declined,
    Cancelled,
    Failed,
}

impl State {
    /// Whether it may still change.
    pub fn is_live(self) -> bool {
        matches!(self, State::Offered | State::Sending | State::Receiving)
    }
}

/// How a transfer ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// Delivered and verified: the whole file's SHA-256, in hex.
    Done {
        sha256: String,
    },
    Declined,
    /// By either side; why, in words.
    Cancelled(String),
    /// A check failed, the file couldn't be read or written, or the peer didn't come back; why, in words.
    Failed(String),
}

/// What happened, from [`Files::poll_event`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A peer offers a file: [`accept`](Files::accept) or [`decline`](Files::decline) it. The name is cleaned (safe
    /// to show and save under), the MIME type checked (empty if none).
    Offer { id: TransferId, peer: PeerId, name: String, size: u64, mime: String },
    /// The peer took a file we offered: it starts going.
    Accepted { id: TransferId },
    /// Bytes the receiver has written: acknowledged (sending) or checked and written here (receiving).
    Progress { id: TransferId, bytes: u64, size: u64 },
    /// After its link dropped, the transfer goes on from piece `from` (counted from 0).
    Resumed { id: TransferId, from: u64 },
    /// It ended (every transfer ends with one, also those this side ended).
    Ended { id: TransferId, end: End },
}

/// A transfer, as [`Files::info`] shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub peer: PeerId,
    pub direction: Direction,
    pub state: State,
    /// Cleaned.
    pub name: String,
    pub size: u64,
    pub mime: String,
    /// Bytes the receiver has written.
    pub bytes: u64,
    /// The peer's link is down: the transfer waits for it (up to [`RESUME`]).
    pub peer_gone: bool,
    /// The whole file's SHA-256, once known.
    pub sha256: Option<String>,
    /// Where a received file was kept.
    pub saved_as: Option<PathBuf>,
    /// The id both sides know it by.
    pub wire_id: WireId,
}

/// Why a call was refused.
#[derive(Debug)]
pub enum Error {
    /// The peer isn't connected.
    NotConnected,
    /// No such transfer, or not one this call takes (an offer to accept, a live one to cancel).
    NoTransfer,
    /// A file over [`MAX_SIZE`].
    TooLarge,
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotConnected => f.write_str("the peer isn't connected"),
            Error::NoTransfer => f.write_str("no such transfer in a state that takes this"),
            Error::TooLarge => write!(f, "a file over {MAX_SIZE} bytes"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

/// Counters of what peers sent that was refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub pieces_sent: u64,
    pub pieces_taken: u64,
    /// Pieces refused: for a transfer not accepted, out of order, the wrong length or failing their hash.
    pub rejected: u64,
    /// Offers that weren't valid, or past [`MAX_OFFERS`].
    pub offers_refused: u64,
    /// Control messages dropped past [`MAX_OUTBOX`].
    pub outbox_dropped: u64,
}

struct Transfer {
    id: TransferId,
    wire: WireId,
    peer: PeerId,
    dir: Direction,
    state: State,
    name: String,
    size: u64,
    mime: String,
    piece: u32,
    count: u64,
    /// The peer's link dropped then; `None` while it is up.
    gone: Option<Instant>,
    /// When it ended, as a count of ends (its `Event::Ended` is that one), to forget the oldest first.
    ended: u64,
    sha: Option<String>,
    // Sending: the next piece to send, how many the receiver acknowledged, how many the whole-file hash has taken.
    source: Option<Box<dyn Source>>,
    offer: Vec<u8>,
    next: u64,
    acked: u64,
    read: u64,
    hasher: Option<Sha256>,
    end_sent: bool,
    /// The piece frame `next` the wire couldn't take yet.
    pending: Option<Vec<u8>>,
    // Receiving: pieces written (`next` is the same here: pieces are written as they come), pieces refused, the
    // sender's whole-file hash.
    sink: Option<Box<dyn Sink>>,
    have: u64,
    rejected: u32,
    want: Option<String>,
    saved_as: Option<PathBuf>,
}

impl Transfer {
    fn new(id: TransferId, peer: PeerId, dir: Direction, o: &Offer) -> Transfer {
        Transfer {
            id,
            wire: o.id,
            peer,
            dir,
            state: State::Offered,
            name: o.name.clone(),
            size: o.size,
            mime: o.mime.clone(),
            piece: o.piece,
            count: o.count,
            gone: None,
            ended: 0,
            sha: None,
            source: None,
            offer: Vec::new(),
            next: 0,
            acked: 0,
            read: 0,
            hasher: None,
            end_sent: false,
            pending: None,
            sink: None,
            have: 0,
            rejected: 0,
            want: None,
            saved_as: None,
        }
    }

    fn live(&self) -> bool {
        self.state.is_live()
    }

    fn bytes(&self, pieces: u64) -> u64 {
        pieces.saturating_mul(self.piece as u64).min(self.size)
    }
}

/// Share transfers with the peers of a node: see the [module](self) for the protocol and a host loop.
pub struct Files {
    transfers: Vec<Transfer>,
    present: HashSet<PeerId>,
    outbox: HashMap<PeerId, VecDeque<Vec<u8>>>,
    events: VecDeque<Event>,
    rng: ChaCha20Rng,
    next_id: u64,
    /// Transfers ended, each with its `Event::Ended`, in order.
    ends: u64,
    /// Of those `Event::Ended`, how many `poll_event` handed out: a transfer whose end is past this isn't forgotten.
    told: u64,
    buf: Vec<u8>,
    stats: Stats,
}

impl Default for Files {
    fn default() -> Files {
        Files::new()
    }
}

impl Files {
    /// No transfers yet; transfer ids from the OS's randomness.
    pub fn new() -> Files {
        let mut seed = [0u8; 32];
        rand_core::OsRng.fill_bytes(&mut seed);
        Files::with_seed(seed)
    }

    /// Transfer ids from `seed` (32 random bytes): for tests and hosts that bring their own randomness.
    pub fn with_seed(seed: [u8; 32]) -> Files {
        Files {
            transfers: Vec::new(),
            present: HashSet::new(),
            outbox: HashMap::new(),
            events: VecDeque::new(),
            rng: ChaCha20Rng::from_seed(seed),
            next_id: 1,
            ends: 0,
            told: 0,
            buf: Vec::new(),
            stats: Stats::default(),
        }
    }

    /// The counters.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// A transfer, if it is still kept: live ones always are, and finished ones at least until
    /// [`poll_event`](Self::poll_event) has handed out their [`Event::Ended`] (then the [`MAX_KEPT`] latest).
    pub fn info(&self, id: TransferId) -> Option<Info> {
        let t = self.get(id)?;
        let pieces = if t.dir == Direction::Send { t.acked } else { t.have };
        Some(Info {
            peer: t.peer,
            direction: t.dir,
            state: t.state,
            name: t.name.clone(),
            size: t.size,
            mime: t.mime.clone(),
            bytes: if t.state == State::Done { t.size } else { t.bytes(pieces) },
            peer_gone: t.gone.is_some(),
            sha256: t.sha.clone(),
            saved_as: t.saved_as.clone(),
            wire_id: t.wire,
        })
    }

    /// The transfers kept, oldest first.
    pub fn transfers(&self) -> impl Iterator<Item = TransferId> + '_ {
        self.transfers.iter().map(|t| t.id)
    }

    /// The next event, if any.
    pub fn poll_event(&mut self) -> Option<Event> {
        let e = self.events.pop_front();
        if let Some(Event::Ended { .. }) = e {
            self.told += 1;
        }
        e
    }

    // ----- the node's side -----

    /// Hands it one of the node's events: a peer connected or dropped, or a message. True if it was a Share
    /// message (taken, or dropped as not valid): not the app's. A peer's connection is the app's too, so it is false.
    pub fn handle_event(&mut self, now: Instant, e: &crate::Event) -> bool {
        match e {
            crate::Event::Connected { peer, .. } => {
                self.connected(*peer);
                false
            }
            crate::Event::Disconnected { peer, reason } => {
                self.disconnected(now, *peer, *reason);
                false
            }
            crate::Event::Message { peer, lane, data } => self.receive(*peer, *lane, data),
            _ => false,
        }
    }

    /// A peer connected, perhaps again: what it was offered goes again, and transfers under way go on from the last
    /// piece the receiver wrote.
    pub fn connected(&mut self, peer: PeerId) {
        self.present.insert(peer);
        let mut say = Vec::new();
        for t in self.transfers.iter_mut().filter(|t| t.peer == peer && t.live()) {
            match (t.dir, t.state) {
                // An offer not answered yet goes again (the receiver keeps one per id); a transfer under way waits
                // for the receiver's accept, which says where to go on from.
                (Direction::Send, State::Offered) => {
                    t.gone = None;
                    say.push(t.offer.clone());
                }
                (Direction::Send, _) => say.push(frame::encode_control(&Message::Resume { id: t.wire })),
                (Direction::Receive, state) => {
                    t.gone = None;
                    if state == State::Receiving {
                        // Pieces are written as they come, so `have` is where the sender goes on from.
                        t.next = t.have;
                        say.push(frame::encode_control(&Message::Accept { id: t.wire, have: t.have }));
                    }
                }
            }
        }
        for f in say {
            self.say(peer, f);
        }
    }

    /// A peer's link dropped. Its transfers wait for it (up to [`RESUME`]); if it closed the link itself
    /// (`ClosedByPeer`), they end, as the page's do when a peer leaves the room.
    pub fn disconnected(&mut self, now: Instant, peer: PeerId, reason: DisconnectReason) {
        self.present.remove(&peer);
        self.outbox.remove(&peer);
        let ids: Vec<_> = self.transfers.iter().filter(|t| t.peer == peer && t.live()).map(|t| t.id).collect();
        for id in ids {
            if reason == DisconnectReason::ClosedByPeer {
                self.end(id, End::Cancelled("the peer closed the link".into()));
            } else if let Some(t) = self.get_mut(id) {
                t.gone.get_or_insert(now);
                t.pending = None;
            }
        }
    }

    /// Hands it a message from `peer`. True if it was a Share message ([`share_frame`]), taken or dropped: not the
    /// app's. Only reliable ones are taken, as the page takes them.
    pub fn receive(&mut self, peer: PeerId, lane: Lane, message: &[u8]) -> bool {
        let Some(f) = share_frame(message) else { return false };
        if lane == Lane::Reliable {
            self.receive_frame(peer, f);
        }
        true
    }

    /// Hands it a Share frame from `peer`, as it came off the wire (a room message's, less its tag).
    pub fn receive_frame(&mut self, peer: PeerId, f: &[u8]) {
        match f.first() {
            Some(&frame::PIECE_FRAME) => self.on_piece(peer, f),
            Some(&frame::CONTROL_FRAME) => {
                if let Some(m) = frame::decode_control(f) {
                    self.on_control(peer, &m);
                }
            }
            _ => {}
        }
    }

    /// Sends what may go now through `wire`: control messages held for a peer, then pieces while the link carries
    /// files and the peer's window has room; and fails transfers whose peer didn't come back within [`RESUME`]. Call
    /// it from the host loop, after handing it the node's events.
    pub fn poll(&mut self, now: Instant, wire: &mut dyn Wire) {
        let late: Vec<_> = self
            .transfers
            .iter()
            .filter(|t| t.live() && t.gone.is_some_and(|g| now.saturating_duration_since(g) >= RESUME))
            .map(|t| t.id)
            .collect();
        for id in late {
            self.end(id, End::Failed("the peer didn't come back".into()));
        }
        let peers: Vec<PeerId> = self.outbox.keys().copied().collect();
        for peer in peers {
            self.flush(&peer, wire);
        }
        let mut sending: Vec<PeerId> = Vec::new();
        for t in &self.transfers {
            if t.state == State::Sending && !sending.contains(&t.peer) {
                sending.push(t.peer);
            }
        }
        for peer in sending {
            if self.outbox.get(&peer).is_none_or(|q| q.is_empty()) {
                self.pump(&peer, wire);
            }
        }
    }

    // ----- what the app does -----

    /// Offers `size` bytes from `source`, named `name` (cleaned before it goes) with MIME type `mime` (or ""), to
    /// `peer`. It goes once the peer accepts.
    pub fn offer(
        &mut self,
        peer: PeerId,
        name: &str,
        mime: &str,
        size: u64,
        source: Box<dyn Source>,
    ) -> Result<TransferId, Error> {
        if !self.present.contains(&peer) {
            return Err(Error::NotConnected);
        }
        if size > MAX_SIZE {
            return Err(Error::TooLarge);
        }
        let mut wire = [0u8; frame::ID_BYTES];
        self.rng.fill_bytes(&mut wire);
        let o = Offer::new(WireId(wire), name, size, mime);
        let id = self.new_id();
        let mut t = Transfer::new(id, peer, Direction::Send, &o);
        t.source = Some(source);
        t.offer = o.encode();
        let f = t.offer.clone();
        self.transfers.push(t);
        self.say(peer, f);
        self.prune();
        Ok(id)
    }

    /// Offers the file at `path` to `peer`, under its file name, read from disk a piece at a time.
    pub fn offer_file(&mut self, peer: PeerId, path: &Path) -> Result<TransferId, Error> {
        let file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(Error::Io(io::Error::new(io::ErrorKind::InvalidInput, "not a file")));
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.offer(peer, &name, "", meta.len(), Box::new(file))
    }

    /// Accepts an offer: the file goes into `sink`. Nothing here caps the transfers a peer has under way ([`MAX_OFFERS`]
    /// caps only offers waiting), and each holds its sink (a [`FileSink`], an open file): an app that accepts without
    /// asking must cap the live transfers per peer itself, as the `share` example does with `--yes`.
    pub fn accept(&mut self, id: TransferId, sink: Box<dyn Sink>) -> Result<(), Error> {
        let present = &self.present;
        let t = self
            .transfers
            .iter_mut()
            .find(|t| t.id == id && t.dir == Direction::Receive && t.state == State::Offered)
            .ok_or(Error::NoTransfer)?;
        if !present.contains(&t.peer) {
            return Err(Error::NotConnected);
        }
        t.sink = Some(sink);
        t.hasher = Some(Sha256::new());
        t.state = State::Receiving;
        let (peer, f) = (t.peer, frame::encode_control(&Message::Accept { id: t.wire, have: 0 }));
        // An empty file has no pieces: its end may have come already.
        let empty = t.count == 0 && t.want.is_some();
        self.say(peer, f);
        if empty {
            self.complete(id);
        }
        Ok(())
    }

    /// Accepts an offer into a [`FileSink`] in `dir`: the file appears there, under its cleaned name, once verified.
    /// As with [`accept`](Self::accept), an app that accepts without asking must cap the live transfers per peer.
    pub fn accept_into(&mut self, id: TransferId, dir: &Path) -> Result<(), Error> {
        let name = self
            .get(id)
            .filter(|t| t.dir == Direction::Receive && t.state == State::Offered)
            .ok_or(Error::NoTransfer)?
            .name
            .clone();
        let sink = FileSink::create(dir, &name)?;
        self.accept(id, Box::new(sink))
    }

    /// Declines an offer.
    pub fn decline(&mut self, id: TransferId) -> Result<(), Error> {
        let t = self.get(id).filter(|t| t.dir == Direction::Receive && t.state == State::Offered);
        let (peer, wire) = t.map(|t| (t.peer, t.wire)).ok_or(Error::NoTransfer)?;
        self.say(peer, frame::encode_control(&Message::Decline { id: wire }));
        self.end(id, End::Declined);
        Ok(())
    }

    /// Cancels a transfer either way, and tells the peer.
    pub fn cancel(&mut self, id: TransferId) -> Result<(), Error> {
        if !self.get(id).is_some_and(|t| t.live()) {
            return Err(Error::NoTransfer);
        }
        self.end_and_tell(id, End::Cancelled("cancelled here".into()));
        Ok(())
    }

    // ----- inside -----

    fn new_id(&mut self) -> TransferId {
        let id = TransferId(self.next_id);
        self.next_id += 1;
        id
    }

    fn get(&self, id: TransferId) -> Option<&Transfer> {
        self.transfers.iter().find(|t| t.id == id)
    }

    fn get_mut(&mut self, id: TransferId) -> Option<&mut Transfer> {
        self.transfers.iter_mut().find(|t| t.id == id)
    }

    fn outgoing(&self, peer: &PeerId, wire: WireId) -> Option<TransferId> {
        self.transfers.iter().find(|t| t.dir == Direction::Send && t.wire == wire && t.peer == *peer).map(|t| t.id)
    }

    fn incoming(&self, peer: &PeerId, wire: WireId) -> Option<TransferId> {
        self.transfers.iter().find(|t| t.dir == Direction::Receive && t.wire == wire && t.peer == *peer).map(|t| t.id)
    }

    // Queues a control frame for `peer`, unless its link isn't up (the page's send() to a peer not in the room goes
    // nowhere) or too many wait already.
    fn say(&mut self, peer: PeerId, f: Vec<u8>) {
        if !self.present.contains(&peer) {
            return;
        }
        let q = self.outbox.entry(peer).or_default();
        if q.len() >= MAX_OUTBOX {
            self.stats.outbox_dropped += 1;
            return;
        }
        q.push_back(f);
    }

    fn flush(&mut self, peer: &PeerId, wire: &mut dyn Wire) {
        let Some(q) = self.outbox.get_mut(peer) else { return };
        while let Some(f) = q.front() {
            match wire.send(peer, f) {
                Err(SendError::WouldBlock) => return,
                // Sent, or never will be (not connected, too large): either way, the next.
                _ => {
                    q.pop_front();
                }
            }
        }
        self.outbox.remove(peer);
    }

    // Ends a live transfer: its source or sink closed (a sink's partial file deleted), and an event.
    fn end(&mut self, id: TransferId, end: End) {
        let n = self.ends + 1;
        let Some(t) = self.get_mut(id).filter(|t| t.live()) else { return };
        t.state = match &end {
            End::Done { .. } => State::Done,
            End::Declined => State::Declined,
            End::Cancelled(_) => State::Cancelled,
            End::Failed(_) => State::Failed,
        };
        t.ended = n;
        t.source = None;
        t.hasher = None;
        t.pending = None;
        if let Some(mut s) = t.sink.take() {
            if t.state != State::Done {
                s.abort();
            }
        }
        self.ends = n;
        self.events.push_back(Event::Ended { id, end });
    }

    // Ends a live transfer and tells the peer (a cancel).
    fn end_and_tell(&mut self, id: TransferId, end: End) {
        let Some(t) = self.get(id).filter(|t| t.live()) else { return };
        let (peer, wire) = (t.peer, t.wire);
        self.say(peer, frame::encode_control(&Message::Cancel { id: wire }));
        self.end(id, end);
    }

    // Forgets the oldest finished transfers past MAX_KEPT, of those whose Event::Ended the app has had: info() works
    // for every Ended it gets.
    fn prune(&mut self) {
        let over = self.transfers.len().saturating_sub(MAX_KEPT);
        if over == 0 {
            return;
        }
        let told = self.told;
        let mut done: Vec<(u64, TransferId)> =
            self.transfers.iter().filter(|t| !t.live() && t.ended <= told).map(|t| (t.ended, t.id)).collect();
        done.sort_unstable();
        let gone: HashSet<TransferId> = done.into_iter().take(over).map(|(_, id)| id).collect();
        self.transfers.retain(|t| !gone.contains(&t.id));
    }

    // ----- control messages -----

    fn on_control(&mut self, peer: PeerId, m: &Control) {
        if m.is("offer") {
            return self.on_offer(peer, m);
        }
        let Some(wire) = m.id() else { return };
        let out = self.outgoing(&peer, wire);
        let inc = self.incoming(&peer, wire);
        let state = |me: &Files, id: Option<TransferId>| id.and_then(|id| me.get(id)).map(|t| t.state);
        if m.is("accept") {
            if let Some(id) = out {
                self.on_accept(id, m.field("have"));
            }
        } else if m.is("decline") {
            if let Some(id) = out.filter(|_| state(self, out) == Some(State::Offered)) {
                self.end(id, End::Declined);
            }
        } else if m.is("ack") {
            if let Some(id) = out {
                self.on_ack(id, m.field("have"));
            }
        } else if m.is("done") {
            if let Some(id) = out.filter(|_| state(self, out) == Some(State::Sending)) {
                self.on_done(id, m);
            }
        } else if m.is("end") {
            if let Some(id) = inc {
                self.on_end(id, m.field("sha256"));
            }
        } else if m.is("resume") {
            self.on_resume(peer, wire, inc);
        } else if m.is("cancel") {
            if let Some(id) = out {
                self.end(id, End::Cancelled("the peer cancelled".into()));
            }
            if let Some(id) = inc {
                self.end(id, End::Cancelled("the peer cancelled".into()));
            }
        }
    }

    fn on_offer(&mut self, peer: PeerId, m: &Control) {
        let Some(o) = frame::parse_offer(m) else {
            self.stats.offers_refused += 1;
            return;
        };
        // Sent again on a new link.
        if self.incoming(&peer, o.id).is_some() {
            return;
        }
        let waiting = self
            .transfers
            .iter()
            .filter(|t| t.dir == Direction::Receive && t.peer == peer && t.state == State::Offered);
        if waiting.count() >= MAX_OFFERS {
            self.stats.offers_refused += 1;
            self.say(peer, frame::encode_control(&Message::Decline { id: o.id }));
            return;
        }
        let id = self.new_id();
        self.transfers.push(Transfer::new(id, peer, Direction::Receive, &o));
        self.events.push_back(Event::Offer { id, peer, name: o.name, size: o.size, mime: o.mime });
        self.prune();
    }

    // ----- sending -----

    fn on_accept(&mut self, id: TransferId, have: &Value) {
        let Some(t) = self.get_mut(id) else { return };
        match t.state {
            State::Offered => {
                if !frame::is_zero(have) {
                    return;
                }
                t.state = State::Sending;
                t.hasher = Some(Sha256::new());
                t.gone = None;
                self.events.push_back(Event::Accepted { id });
            }
            State::Sending => {
                // A resume, on a new link: the receiver says how many pieces it has written.
                let Some(at) = frame::resume_at(t.count, t.read, have) else {
                    return self
                        .end_and_tell(id, End::Failed("the receiver asked to resume from a piece never sent".into()));
                };
                t.next = at;
                t.acked = at;
                t.end_sent = false; // it may not have arrived
                t.pending = None;
                t.gone = None;
                self.events.push_back(Event::Resumed { id, from: at });
            }
            _ => {}
        }
    }

    fn on_ack(&mut self, id: TransferId, have: &Value) {
        let Some(t) = self.get_mut(id).filter(|t| t.state == State::Sending) else { return };
        let Some(have) = frame::int_in(have, t.acked, t.next) else { return };
        t.acked = have;
        let (bytes, size) = (t.bytes(have), t.size);
        self.events.push_back(Event::Progress { id, bytes, size });
    }

    // The receiver's verdict: its whole-file SHA-256 is ours (it may come again after a resume, its acks lost), or
    // it isn't, once every piece was acked.
    fn on_done(&mut self, id: TransferId, m: &Control) {
        let Some(t) = self.get_mut(id) else { return };
        let theirs = m.field("sha256").str();
        let ok = m.field("ok") == &Value::Bool(true)
            && t.sha.as_ref().is_some_and(|s| theirs.is_some_and(|h| h.iter().copied().eq(s.bytes().map(u16::from))));
        if ok {
            t.acked = t.count;
            let sha256 = t.sha.clone().unwrap_or_default();
            self.end(id, End::Done { sha256 });
        } else if t.acked == t.count {
            self.end(id, End::Failed("the receiver's SHA-256 doesn't match".into()));
        }
    }

    // Pieces in flight to `peer` over all our transfers to it: at most WINDOW, so the receiver never holds more.
    fn in_flight(&self, peer: &PeerId) -> u64 {
        self.transfers.iter().filter(|t| t.peer == *peer && t.state == State::Sending).map(|t| t.next - t.acked).sum()
    }

    // The transfer to `peer` whose pieces go next: the oldest accepted one with pieces (or its end) still to send.
    fn head(&self, peer: &PeerId) -> Option<usize> {
        self.transfers.iter().position(|t| {
            t.peer == *peer && t.state == State::Sending && t.gone.is_none() && !(t.end_sent && t.next == t.count)
        })
    }

    // Sends what may go to `peer` now, a piece at a time: while the link carries files and the window has room.
    fn pump(&mut self, peer: &PeerId, wire: &mut dyn Wire) {
        while wire.carries_files(peer) {
            let Some(i) = self.head(peer) else { return };
            let in_flight = self.in_flight(peer);
            let t = &mut self.transfers[i];
            let id = t.id;
            if t.next < t.count {
                let f = match t.pending.take() {
                    Some(f) => f,
                    None => {
                        if in_flight >= WINDOW {
                            return;
                        }
                        match read_piece(t, &mut self.buf) {
                            Ok(f) => f,
                            Err(e) => {
                                let why = format!("couldn't read the file ({e}): was it moved or changed?");
                                return self.end_and_tell(id, End::Failed(why));
                            }
                        }
                    }
                };
                match wire.send(peer, &f) {
                    Ok(()) => {
                        t.next += 1;
                        self.stats.pieces_sent += 1;
                    }
                    Err(SendError::WouldBlock) => {
                        t.pending = Some(f);
                        return;
                    }
                    Err(_) => return,
                }
            } else {
                let sha = match &t.sha {
                    Some(s) => s.clone(),
                    None => {
                        let h = t.hasher.take().unwrap_or_default();
                        let s = frame::hex(&h.finalize());
                        t.sha = Some(s.clone());
                        s
                    }
                };
                let f = frame::encode_control(&Message::End { id: t.wire, sha256: sha });
                match wire.send(peer, &f) {
                    Ok(()) => t.end_sent = true,
                    _ => return,
                }
            }
        }
    }

    // ----- receiving -----

    fn on_piece(&mut self, peer: PeerId, f: &[u8]) {
        let Some(p) = frame::decode_piece(f) else { return };
        // Nothing is kept for a transfer we haven't accepted.
        let Some(id) =
            self.incoming(&peer, p.id).filter(|&id| self.get(id).is_some_and(|t| t.state == State::Receiving))
        else {
            self.stats.rejected += 1;
            return;
        };
        let t = self.get_mut(id).expect("found");
        if let Err(why) = frame::check_piece(t.size, t.piece, t.count, t.next, p.index, p.data.len()) {
            t.rejected += 1;
            let over = t.rejected > MAX_REJECTS;
            self.stats.rejected += 1;
            if over {
                let why = format!("the sender sent pieces it shouldn't ({})", why.as_str());
                self.end_and_tell(id, End::Cancelled(why));
            }
            return;
        }
        // Written at once, so nothing is held unwritten: MAX_QUEUED (the page's bound on pieces it holds while its
        // writes catch up) is never near.
        self.stats.pieces_taken += 1;
        self.write_piece(id, &p);
    }

    // Checks and writes one piece, then acks it.
    fn write_piece(&mut self, id: TransferId, p: &Piece<'_>) {
        if frame::sha256(p.data) != *p.hash {
            let why = format!("piece {} failed its SHA-256 check", p.index as u64 + 1);
            return self.end_and_tell(id, End::Failed(why));
        }
        let t = self.get_mut(id).expect("found");
        t.next += 1;
        if let Some(h) = t.hasher.as_mut() {
            h.update(p.data);
        }
        let wrote = t.sink.as_mut().map_or(Ok(()), |s| s.write(p.data));
        if let Err(e) = wrote {
            return self.end_and_tell(id, End::Cancelled(format!("couldn't write: {e}")));
        }
        t.have += 1;
        let (peer, f) = (t.peer, frame::encode_control(&Message::Ack { id: t.wire, have: t.have }));
        let (bytes, size) = (t.bytes(t.have), t.size);
        let whole = t.have == t.count && t.want.is_some();
        self.say(peer, f);
        self.events.push_back(Event::Progress { id, bytes, size });
        if whole {
            self.complete(id);
        }
    }

    // The sender, back on a new link, asks where a transfer is: one we are receiving gets our accept anyway
    // (connected); one we finished gets our verdict again; one we no longer have, a cancel.
    fn on_resume(&mut self, peer: PeerId, wire: WireId, inc: Option<TransferId>) {
        let t = inc.and_then(|id| self.get(id));
        match t {
            Some(t) if t.state == State::Done => {
                let sha256 = t.sha.clone().unwrap_or_default();
                self.say(peer, frame::encode_control(&Message::Done { id: wire, ok: true, sha256 }));
            }
            Some(t) if t.live() => {}
            _ => self.say(peer, frame::encode_control(&Message::Cancel { id: wire })),
        }
    }

    fn on_end(&mut self, id: TransferId, sha: &Value) {
        let Some(t) = self.get_mut(id).filter(|t| matches!(t.state, State::Receiving | State::Offered)) else {
            return;
        };
        let Some(sha) = sha.str().filter(|s| frame::is_sha(s)) else {
            return self.end_and_tell(id, End::Cancelled("the sender's end had no valid SHA-256".into()));
        };
        t.want = Some(sha.iter().map(|&c| c as u8 as char).collect());
        if t.state == State::Receiving && t.have == t.count {
            self.complete(id);
        }
    }

    fn complete(&mut self, id: TransferId) {
        let Some(t) = self.get_mut(id).filter(|t| t.state == State::Receiving) else { return };
        let sha = frame::hex(&t.hasher.take().unwrap_or_default().finalize());
        t.sha = Some(sha.clone());
        let peer = t.peer;
        let wire = t.wire;
        if t.want.as_deref() != Some(sha.as_str()) {
            self.say(peer, frame::encode_control(&Message::Done { id: wire, ok: false, sha256: sha }));
            return self.end(id, End::Failed("the whole file's SHA-256 doesn't match: not kept".into()));
        }
        let kept = t.sink.as_mut().map_or(Ok(()), |s| s.finish());
        if let Err(e) = kept {
            self.say(peer, frame::encode_control(&Message::Done { id: wire, ok: false, sha256: sha }));
            return self.end(id, End::Failed(format!("couldn't keep the file: {e}")));
        }
        t.saved_as = t.sink.as_ref().and_then(|s| s.saved_as());
        self.say(peer, frame::encode_control(&Message::Done { id: wire, ok: true, sha256: sha.clone() }));
        self.end(id, End::Done { sha256: sha });
    }
}

// Reads piece `next` of a transfer we send and makes its frame; each piece goes into the whole-file hash once, in
// order, however often a resume sends it again.
fn read_piece(t: &mut Transfer, buf: &mut Vec<u8>) -> io::Result<Vec<u8>> {
    let (start, end) = frame::piece_range(t.size, t.piece, t.next);
    buf.resize((end - start) as usize, 0);
    t.source.as_mut().ok_or_else(|| io::Error::other("no source"))?.read_at(start, buf)?;
    let hash = frame::sha256(buf);
    if t.next == t.read {
        if let Some(h) = t.hasher.as_mut() {
            h.update(&buf[..]);
        }
        t.read += 1;
    }
    Ok(frame::encode_piece(t.wire, t.next as u32, &hash, buf))
}
