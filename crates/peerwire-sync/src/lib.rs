//! `peerwire-sync`: shared documents, state that every member of a room holds and that converges. It is
//! `room.doc(name)` of `@peerwire/js` (`packages/js/src/doc.js`) for Rust, Godot and C hosts: the same maps, sets
//! and logs, the same frames, byte for byte, and the same caps and budgets.
//!
//! Like `peerwire-core`, it is sans-IO and deterministic: no sockets, threads, clocks or OS randomness. The host
//! passes time and messages in, and drains messages, events and the next timer:
//!
//! ```
//! use peerwire_core::{Instant, PeerId};
//! use peerwire_sync::{Docs, Event, Kind, Options, Route};
//!
//! let (me, friend) = (PeerId([1; 32]), PeerId([2; 32]));
//! let mut docs = Docs::new(me, [7; 32]); // seed it from the OS's randomness
//! let now = Instant::from_millis(0);
//! let board = docs.open(now, "board", Options::default()).unwrap();
//! docs.joined(now, friend, Route::Direct); // a member joined the room
//! docs.doc(board).unwrap().map_set("meta", "title", r#""Plans""#).unwrap(); // values are JSON text
//! // Each message goes to its member on the reliable lane; one the link refuses goes back with retry().
//! while let Some((peer, message)) = docs.poll_transmit() {
//!     assert_eq!((peer, message[0]), (friend, peerwire_sync::DOC_TAG));
//! }
//! // A reliable message from a member: `receive` takes it if it is a doc's (DOC_TAG), else it is the app's.
//! assert!(!docs.receive(now, friend, b"\x01app bytes"));
//! while let Some(event) = docs.poll_event() {
//!     if let Event::Change(_, c) = event {
//!         assert_eq!((c.kind(), c.key().as_str(), c.value()), (Kind::Map, "title", Some(r#""Plans""#)));
//!     }
//! }
//! // Then call handle_timeout(now) once poll_timeout() has passed.
//! ```
//!
//! A doc message is `DOC_TAG` (7, the tag room.js gives doc frames) and the frame's JSON. `Node::send` carries raw
//! app bytes, so an app that opens docs tags its own messages too and hands `receive` every reliable message. A
//! Godot room does this itself. room.js's deflated frames (tag 8) are never asked for and are dropped.
//!
//! The semantics are doc.js's, which says why each is as it is. Every write is a dot (writer, n), the writer
//! being our PeerId and a random incarnation per opening. A map key shows the value of its latest write (a set made
//! elsewhere at the same time wins over a delete: add-wins), a set holds items until deleted, and a log keeps its
//! newest `keep` entries by Lamport stamp. Members that join later, or come back after a partition, catch up in
//! pieces from one member at a time, the best routed first. Members are trusted alike to write, as in the room
//! itself; what the doc defends is each member's memory and bandwidth: frames are checked field by field and
//! dropped whole if anything is off, and names, keys, values, entries, writers, ranges and what a member can have
//! us send are capped.
//!
//! Not in this version: the copy kept on the device is the host's to keep ([`DocMut::snapshot`] and
//! [`Options::copy`]; sealing and pacing it is NB-006g), and deflated frames.

mod context;
mod doc;
mod docs;
mod frame;
#[cfg(test)]
mod fuzz;
mod json;
mod ordmap;

pub use docs::{DocMut, Docs};
pub use frame::Kind;
/// The core's time and identity, as the host passes them in.
pub use peerwire_core::{Instant, PeerId};

use frame::Id;
use json::{lossy, write_str, Utf16};
use std::fmt::Write as _;

/// The first byte of a doc message: room.js's `T.Doc`.
pub const DOC_TAG: u8 = 7;
/// room.js's `T.DocZ`, a deflated doc frame: never asked for, dropped.
pub const DOCZ_TAG: u8 = 8;
/// A doc's or a collection's name, in UTF-16 code units.
pub const MAX_NAME_LENGTH: usize = 128;
/// A map key, in UTF-16 code units.
pub const MAX_KEY_LENGTH: usize = 256;
/// A value as JSON text, in UTF-16 code units.
pub const MAX_VALUE_LENGTH: usize = 64 * 1024;
/// How deeply a value's arrays and objects nest.
pub const MAX_VALUE_DEPTH: usize = 64;
/// Live entries in a doc, all collections.
pub const MAX_ENTRIES: usize = 10000;
/// What a doc's values, keys and names take as JSON text, in UTF-16 code units.
pub const MAX_DOC_LENGTH: usize = 4 * 1024 * 1024;
/// Writers a doc's context holds.
pub const MAX_WRITERS: usize = 1024;
/// Ranges of dots a doc's context holds.
pub const MAX_RANGES: usize = 8192;
/// Entries a log keeps unless [`DocMut::log_keep`] says otherwise.
pub const DEFAULT_KEEP: u64 = 1000;
/// The most a doc frame takes as UTF-8: half the reliable lane's 1 MiB.
pub const MESSAGE_BYTES: usize = 512 * 1024;
/// Frames a copy may hold (a full doc takes about 30).
pub const STORED_FRAMES: usize = 64;
/// What a member's messages may hold waiting to be sent, retried ones included. Past it, [`Docs`] drops them, and
/// each doc catches the member up afresh, as if it had left and joined again: it asks for what it missed, at the
/// pace catching up goes. What the member's link keeps refusing is then forgotten with it ([`Event::Overflow`]).
/// Four full docs as JSON text.
pub const OUTBOX_BYTES: usize = 16 * 1024 * 1024;
/// What a hub ([`Options::hub`]) may relay of one member's frames at once, as queued for its other members: past
/// it, [`Docs`] forgets the member as if it left ([`Event::Overflow`]), so the member writing pays rather than
/// the members it would fill the queues of. Half of [`OUTBOX_BYTES`], but the frame that overdraws it still goes
/// on, and each key has its own: a member that keeps coming back, or a run of fresh keys, has up to a doc's worth
/// relayed each time, and can pile [`OUTBOX_BYTES`] up for a slow member, which is caught up afresh, not cut.
pub const RELAY_BYTES: usize = OUTBOX_BYTES / 2;
/// How fast [`RELAY_BYTES`] refills, in bytes a second: the rate doc.js lets a member have us send it catch-up.
pub const RELAY_BYTES_PER_S: usize = 64 * 1024;
/// The longest array a frame may hold: entries, dead ranges, writers.
const FRAME_ITEMS: usize = 16384;

/// A doc opened by [`Docs::open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocId(pub u32);

/// How a member is reached, as room.js names it: members are asked to catch us up in this order, and answers
/// go in pieces sized to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Route {
    /// Linked directly (or the host doesn't say).
    #[default]
    Direct,
    /// Relayed by another member.
    Peer,
    /// Over signalling.
    Signaling,
}

/// How [`Docs::open`] opens a doc.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options<'a> {
    /// A copy from [`DocMut::snapshot`], joined before the first ask, so members send only what it lacks.
    pub copy: Option<&'a [u8]>,
    /// This member is the hub of a star (a Godot room's host, which every other member links to alone): what a
    /// member's frame adds goes on to the others, charged to that member ([`RELAY_BYTES`]).
    pub hub: bool,
}

/// What a call can refuse, where doc.js throws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A doc's or collection's name isn't 1 to [`MAX_NAME_LENGTH`] UTF-16 code units.
    Name,
    /// A map key is longer than [`MAX_KEY_LENGTH`].
    Key,
    /// A value isn't JSON text.
    Json,
    /// A value is longer than [`MAX_VALUE_LENGTH`] as JSON text, or nests deeper than [`MAX_VALUE_DEPTH`].
    Value,
    /// A log's `keep` isn't 1 to [`MAX_ENTRIES`].
    Keep,
    /// The copy isn't a doc this version reads, or not this doc's: the doc wasn't opened.
    Copy,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::Name => "a name is 1 to 128 UTF-16 code units",
            Error::Key => "a map key is at most 256 UTF-16 code units",
            Error::Json => "a value must be JSON text",
            Error::Value => "a value is at most 65536 UTF-16 code units as JSON, nested at most 64 deep",
            Error::Keep => "keep is 1 to 10000",
            Error::Copy => "the copy isn't this doc's, as this version reads it",
        })
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// No member to catch up from.
    Alone,
    /// Catching up from [`Status::from`].
    Loading,
    /// Caught up: an answer from a member that had caught up itself ended, or every member had answered.
    Synced,
}

/// Where a doc's contents came from (doc.js `doc.sync.source`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Caught up from members.
    Members,
    /// A save on the signalling relays (doc.js `relays: true`). This crate keeps no copy there, so it never
    /// reports this one; it is here so that `Status` has doc.js's shape.
    Relays,
}

/// Where catching up stands (doc.js `doc.sync`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// The member whose answer is under way, while loading.
    pub from: Option<PeerId>,
    /// Entries of `total` that the answer under way has brought.
    pub have: u64,
    /// Entries the answer under way carries, as it says; 0 until its first piece lands.
    pub total: u64,
    /// Where the doc came from: [`Source::Members`] once caught up (even after every member left), else none.
    pub source: Option<Source>,
    /// With [`Source::Relays`], the time (ms since the epoch) its saver claims for the save; else none.
    pub at: Option<u64>,
}

impl Status {
    /// As doc.js's 'sync' event gives it:
    /// `{"state":"loading","from":"<hex>","have":0,"total":0,"source":null,"at":null}`.
    pub fn json(&self) -> String {
        let state = match self.state {
            State::Alone => "alone",
            State::Loading => "loading",
            State::Synced => "synced",
        };
        let mut s = format!("{{\"state\":\"{state}\",\"from\":");
        match self.from {
            Some(p) => {
                s.push('"');
                doc::hex(&p.0, &mut s);
                s.push('"');
            }
            None => s.push_str("null"),
        }
        let _ = write!(s, ",\"have\":{},\"total\":{},\"source\":", self.have, self.total);
        s.push_str(match self.source {
            Some(Source::Members) => "\"members\"",
            Some(Source::Relays) => "\"relays\"",
            None => "null",
        });
        let _ = match self.at {
            Some(at) => write!(s, ",\"at\":{at}}}"),
            None => write!(s, ",\"at\":null}}"),
        };
        s
    }
}

/// A key or item that changed (doc.js's 'change' event), after the write or frame that changed it.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    name: Utf16,
    kind: Kind,
    key: Option<Utf16>,
    item: Option<Id>,
    value: Option<String>,
    from: PeerId,
    local: bool,
}

impl Change {
    /// The collection's name. A lone surrogate (only JS writes one) reads as U+FFFD here; [`Change::json`] keeps it.
    pub fn collection(&self) -> String {
        lossy(&self.name)
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The map key, or the set item's or log entry's id.
    pub fn key(&self) -> String {
        match (&self.key, &self.item) {
            (Some(k), _) => lossy(k),
            (_, Some(id)) => id.to_string(),
            _ => String::new(),
        }
    }

    /// What it shows now, as JSON text; `None` once deleted (or trimmed from a log).
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    pub fn deleted(&self) -> bool {
        self.value.is_none()
    }

    /// Who wrote what it shows now, or who deleted it (as far as we know: the member that told us).
    pub fn from(&self) -> PeerId {
        self.from
    }

    /// Whether our own write made it.
    pub fn local(&self) -> bool {
        self.local
    }

    /// As doc.js's 'change' event gives it:
    /// `{"name":…,"type":"map","key":…,"value":…,"from":"<hex>","local":false}`, `"id"` for an item, and
    /// `"deleted":true` in place of a value once deleted.
    pub fn json(&self) -> String {
        let mut s = String::from("{\"name\":");
        write_str(&self.name, &mut s);
        let _ = write!(s, ",\"type\":\"{}\",", self.kind.name());
        match (&self.key, &self.item) {
            (Some(k), _) => {
                s.push_str("\"key\":");
                write_str(k, &mut s);
            }
            (_, Some(id)) => {
                let _ = write!(s, "\"id\":\"{id}\"");
            }
            _ => s.push_str("\"key\":null"),
        }
        if let Some(v) = &self.value {
            let _ = write!(s, ",\"value\":{v}");
        }
        s.push_str(",\"from\":\"");
        doc::hex(&self.from.0, &mut s);
        let _ = write!(s, "\",\"local\":{}", self.local);
        if self.value.is_none() {
            s.push_str(",\"deleted\":true");
        }
        s.push('}');
        s
    }
}

/// An entry of a collection, as [`DocMut::entries`] lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The map key (a lone surrogate as U+FFFD), or the set item's or log entry's id.
    pub key: String,
    /// The value, as JSON text.
    pub value: String,
    /// Who wrote it.
    pub from: PeerId,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Change(DocId, Change),
    Sync(DocId, Status),
    /// This member's link took none of its messages for 30 s, while they passed [`OUTBOX_BYTES`] waiting and were
    /// dropped twice; or, at a hub, it had more than [`RELAY_BYTES`] of its frames relayed beyond what refilled:
    /// [`Docs`] forgot it as if it left, and takes nothing more from it until [`Docs::joined`] again. Close its link
    /// (it catches up when it comes back; its relay budget doesn't refill by coming back).
    Overflow(PeerId),
}
