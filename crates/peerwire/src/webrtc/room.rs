//! room.js's room layer on a browser's links (feature `webrtc`), so a native node is a member of a `joinRoom` room as a
//! page is. Every room message starts with a tag byte (room.js `T`):
//!
//! ```text
//! 0 bytes   1 text   2 JSON        the app's messages, passed on to the app as they are, the tag first
//! 3 presence (JSON)  4 goodbye     5 link proof (32 bytes)   6 nudge (nothing)
//! 7 doc frame (JSON) 8 doc frame, deflated   9 call frame   10 birthday punch frame
//! ```
//!
//! On a new link each side sends its link proof first, on the reliable lane: HMAC-SHA256 under the room key over
//! `pw-link`, its id and the other's (proof.js `pair`). A link that hasn't proved the room key within
//! [`PROOF_WITHIN`] closes, and until it has, only its proof and a goodbye are read. Once a peer's proof checks
//! out, our presence goes to it; once its presence comes, it has joined, and only then does the app see it
//! (`Event::Connected`) or its messages. A goodbye (on any lane) closes its link as one it closed. A doc frame asking
//! for a doc is answered with nothing, as room.js answers for a doc it hasn't open (doc.js `unopened`), so a page's
//! catch-up doesn't wait on us; anything else of the room layer's (nudges, deflated doc frames, calls, punches) and
//! any tag past these is dropped, as an older room.js drops a tag it doesn't know.

use peerwire_core::{Instant, Lane, PeerId};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

pub(crate) const BYTES: u8 = 0;
pub(crate) const TEXT: u8 = 1;
pub(crate) const JSON: u8 = 2;
pub(crate) const PRESENCE: u8 = 3;
pub(crate) const BYE: u8 = 4;
pub(crate) const PROOF: u8 = 5;
pub(crate) const DOC: u8 = 7;
/// The label of the link proof.
pub(crate) const LINK: &[u8] = b"pw-link";
/// A link must prove the room key this soon after it connects (room.js `PROOF_MS`).
pub(crate) const PROOF_WITHIN: Duration = Duration::from_secs(10);
/// The longest doc name a doc frame asks for, in UTF-16 code units (doc.js `MAX_NAME_LENGTH`).
const MAX_DOC_NAME: usize = 128;
/// doc.js's frame types: an ask, and its answer.
const DOC_ASK: u64 = 2;
const DOC_ANSWER: u64 = 3;

/// A browser on a link.
struct Member {
    /// When its link came up, and whether this node dialled it (its `Connected` as the app sees it, later).
    initiator: bool,
    /// Its link proof checked out (and our presence went).
    proven: bool,
    /// Its presence, JSON: it has joined.
    presence: Option<String>,
    /// Unproven by then: its link closes.
    deadline: Instant,
}

/// What a browser's message comes to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Nothing for the app: the room layer's own, or dropped.
    Quiet,
    /// It has joined: the app sees it connected now ([`Members::initiator`]), its presence in [`Members::presence`].
    Joined,
    /// A message for the app, as it came.
    App,
    /// Its link closes: it said goodbye (`left`), or its proof failed.
    Close { left: bool },
}

/// The browsers on this node's links, and our presence. Sans-IO: the frames to send come back to the host.
pub(crate) struct Members {
    /// Our presence, a whole frame (tag and JSON).
    me: Vec<u8>,
    peers: HashMap<PeerId, Member>,
}

impl Members {
    /// No member yet, our presence `{}`.
    pub(crate) fn new() -> Members {
        Members { me: frame(PRESENCE, b"{}"), peers: HashMap::new() }
    }

    /// Sets our presence, JSON text that parses (checked by the caller), and gives the frame to send to every peer
    /// whose proof checked out, with those peers.
    pub(crate) fn set_presence(&mut self, json: &str) -> (Vec<u8>, Vec<PeerId>) {
        self.me = frame(PRESENCE, json.as_bytes());
        (self.me.clone(), self.peers.iter().filter(|(_, m)| m.proven).map(|(p, _)| *p).collect())
    }

    /// A browser's link came up (`initiator`: this node dialled its core handshake): its link proof to send it now,
    /// `proof`, as a frame; it must send its own within [`PROOF_WITHIN`]. A link that comes up again starts over.
    pub(crate) fn linked(&mut self, now: Instant, peer: PeerId, initiator: bool, proof: &[u8; 32]) -> Vec<u8> {
        self.peers.insert(peer, Member { initiator, proven: false, presence: None, deadline: now + PROOF_WITHIN });
        frame(PROOF, proof)
    }

    /// Whether `peer` has a link the room layer runs on.
    pub(crate) fn has(&self, peer: &PeerId) -> bool {
        self.peers.contains_key(peer)
    }

    /// Whether `peer` has joined.
    pub(crate) fn joined(&self, peer: &PeerId) -> bool {
        self.peers.get(peer).is_some_and(|m| m.presence.is_some())
    }

    /// `peer`'s presence (JSON), once it has joined.
    pub(crate) fn presence(&self, peer: &PeerId) -> Option<&str> {
        self.peers.get(peer)?.presence.as_deref()
    }

    /// Whether this node dialled `peer`'s link.
    pub(crate) fn initiator(&self, peer: &PeerId) -> bool {
        self.peers.get(peer).is_some_and(|m| m.initiator)
    }

    /// A message from `peer` on `lane`; `proved` checks a link proof from it. What it comes to, and the frames to
    /// send it (on the reliable lane): our presence once its proof checked out, a doc answer.
    pub(crate) fn message(
        &mut self,
        peer: &PeerId,
        lane: Lane,
        data: &[u8],
        proved: impl FnOnce(&[u8]) -> bool,
    ) -> (Step, Vec<Vec<u8>>) {
        let Some(m) = self.peers.get_mut(peer) else { return (Step::Quiet, Vec::new()) };
        let Some((&tag, body)) = data.split_first() else { return (Step::Quiet, Vec::new()) };
        if tag == BYE {
            return (Step::Close { left: true }, Vec::new());
        }
        if !m.proven {
            return match tag {
                PROOF if body.len() == 32 && proved(body) => {
                    m.proven = true;
                    (Step::Quiet, vec![self.me.clone()])
                }
                PROOF => (Step::Close { left: false }, Vec::new()),
                _ => (Step::Quiet, Vec::new()),
            };
        }
        if tag == PRESENCE {
            let Some(p) = json_text(body) else { return (Step::Quiet, Vec::new()) };
            let joined = m.presence.is_some();
            m.presence = Some(p);
            return (if joined { Step::Quiet } else { Step::Joined }, Vec::new());
        }
        // Before its presence a peer hasn't joined: room.js drops what races it.
        if m.presence.is_none() {
            return (Step::Quiet, Vec::new());
        }
        match tag {
            BYTES | TEXT => (Step::App, Vec::new()),
            JSON if json_text(body).is_some() => (Step::App, Vec::new()),
            DOC if lane == Lane::Reliable => (Step::Quiet, doc_answer(body).into_iter().collect()),
            _ => (Step::Quiet, Vec::new()),
        }
    }

    /// `peer`'s link ended: forgotten. Whether it had joined (the app saw it).
    pub(crate) fn gone(&mut self, peer: &PeerId) -> bool {
        self.peers.remove(peer).is_some_and(|m| m.presence.is_some())
    }

    /// The peers whose link proof is overdue at `now`: their links close.
    pub(crate) fn overdue(&self, now: Instant) -> Vec<PeerId> {
        self.peers.iter().filter(|(_, m)| !m.proven && m.deadline <= now).map(|(p, _)| *p).collect()
    }

    /// When the next proof falls due.
    pub(crate) fn poll_timeout(&self) -> Option<Instant> {
        self.peers.values().filter(|m| !m.proven).map(|m| m.deadline).min()
    }

    /// The peers that have joined.
    pub(crate) fn members(&self) -> impl Iterator<Item = &PeerId> {
        self.peers.iter().filter(|(_, m)| m.presence.is_some()).map(|(p, _)| p)
    }
}

/// A room message: `tag`, then `body`.
pub(crate) fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(1 + body.len());
    f.push(tag);
    f.extend_from_slice(body);
    f
}

/// `body` as room.js reads JSON (`JSON.parse(new TextDecoder().decode(body))`: a leading BOM dropped, invalid UTF-8
/// as U+FFFD), if it parses: its text.
pub(crate) fn json_text(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str::<Value>(text).ok()?;
    Some(text.to_string())
}

/// The answer to a doc frame that asks for a doc (doc.js `unopened`): none open here, so nothing, and the catch-up
/// it asked for is over. `None` for any other doc frame.
fn doc_answer(body: &[u8]) -> Option<Vec<u8>> {
    let m: Value = serde_json::from_str(&json_text(body)?).ok()?;
    let name = m.get("d")?.as_str()?;
    let q = m.get("q")?.as_f64()?;
    let ask = m.get("t")?.as_f64() == Some(DOC_ASK as f64);
    let named = (1..=MAX_DOC_NAME).contains(&name.encode_utf16().count());
    if !ask || !named || q < 1.0 || q.fract() != 0.0 || q > 9_007_199_254_740_991.0 {
        return None;
    }
    let a = json!({"d": name, "t": DOC_ANSWER, "ws": [], "q": q as u64, "i": 0, "n": 0, "end": 1});
    Some(frame(DOC, a.to_string().as_bytes()))
}
