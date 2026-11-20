//! Peerwire Share's frames and checks, as `packages/js/demo/share/files.js` makes and reads them: the same bytes,
//! the same limits and the same verdicts on whatever a peer sends. [`Files`](super::Files) drives them;
//! `docs/concepts/share-protocol.md` is the protocol.
//!
//! Every Share message is one frame, whose first byte says what it is:
//!
//! ```text
//! PIECE    kind 1 | transfer id (16) | index (4, big-endian) | SHA-256 of the data (32) | data
//! CONTROL  kind 2 | JSON, at most MAX_CONTROL bytes: { t: 'offer' | 'accept' | 'decline' | 'cancel' | 'ack' |
//!          'end' | 'done' | 'resume', id, ... }
//! ```

use super::json::{self, Object, Value};
use super::unicode;
use sha2::{Digest, Sha256};

/// A piece frame's first byte.
pub const PIECE_FRAME: u8 = 1;
/// A control frame's first byte.
pub const CONTROL_FRAME: u8 = 2;
/// The room's largest reliable message, its own tag byte included (`Config::max_message`, room.js `MAX_FRAME`).
pub const MAX_FRAME: usize = 1 << 20;
/// Bytes of a transfer id.
pub const ID_BYTES: usize = 16;
/// Bytes of a piece frame before its data: kind, transfer id, index and SHA-256.
pub const HEADER: usize = 1 + ID_BYTES + 4 + 32;
/// The piece size this side offers (512 KiB), as the Share page does.
pub const PIECE: u32 = 512 * 1024;
/// The smallest piece size taken from a peer.
pub const MIN_PIECE: u32 = 256 * 1024;
/// The largest piece size taken from a peer: a piece frame and the room's tag byte fill `MAX_FRAME`.
pub const MAX_PIECE: u32 = (MAX_FRAME - 1 - HEADER) as u32;
/// Pieces a sender has in flight to one peer (sent, not acknowledged), over all its transfers to it.
pub const WINDOW: u64 = 8;
/// Pieces a receiver holds taken but not yet written, over one peer's transfers; more means the sender ignores acks.
pub const MAX_QUEUED: usize = 2 * WINDOW as usize;
/// The largest file (1 TiB).
pub const MAX_SIZE: u64 = 1 << 40;
/// Bytes of a control message's JSON; a longer one is never parsed.
pub const MAX_CONTROL: usize = 4096;
/// Characters (code points) of a file name, once cleaned.
pub const MAX_NAME: usize = 120;
/// UTF-16 code units of a name as sent: a longer one is no offer.
pub const MAX_RAW_NAME: usize = 1024;
/// UTF-16 code units of a MIME type; a longer one shows as none.
pub const MAX_TYPE: usize = 100;

/// A transfer id: 16 random bytes, chosen by the sender, written as 32 lowercase hex digits in control messages.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WireId(pub [u8; ID_BYTES]);

impl WireId {
    /// The id in 32 lowercase hex digits, if `s` is exactly that (files.js `isId`).
    pub fn parse(s: &[u16]) -> Option<WireId> {
        if s.len() != 2 * ID_BYTES {
            return None;
        }
        let mut id = [0u8; ID_BYTES];
        for (i, pair) in s.chunks(2).enumerate() {
            id[i] = (lower_hex(pair[0])? << 4) | lower_hex(pair[1])?;
        }
        Some(WireId(id))
    }

    /// The id as control messages write it.
    pub fn hex(&self) -> String {
        hex(&self.0)
    }
}

impl std::fmt::Debug for WireId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

impl std::fmt::Display for WireId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.hex())
    }
}

fn lower_hex(c: u16) -> Option<u8> {
    match c {
        0x30..=0x39 => Some((c - 0x30) as u8),
        0x61..=0x66 => Some((c - 0x61 + 10) as u8),
        _ => None,
    }
}

/// Bytes in lowercase hex.
pub fn hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    b.iter().flat_map(|x| [D[(x >> 4) as usize] as char, D[(x & 15) as usize] as char]).collect()
}

/// Whether `s` is a SHA-256 in 64 lowercase hex digits (files.js `isSha`).
pub fn is_sha(s: &[u16]) -> bool {
    s.len() == 64 && s.iter().all(|&c| lower_hex(c).is_some())
}

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// How many pieces a file of `size` bytes takes, in pieces of `piece` bytes.
pub fn piece_count(size: u64, piece: u32) -> u64 {
    size.div_ceil(piece as u64)
}

/// The bytes `[start, end)` piece `i` of a file covers.
pub fn piece_range(size: u64, piece: u32, i: u64) -> (u64, u64) {
    let start = i.saturating_mul(piece as u64).min(size);
    (start, start.saturating_add(piece as u64).min(size))
}

// ---------- names ----------

/// A file name from anyone, safe to show and to save under (files.js `cleanName`): no control or format characters
/// (bidi overrides, zero-width ones), no lone surrogates, no path separators or other characters Windows forbids
/// (each becomes `_`), no leading dots (no hidden file, no `..`) or trailing dots and spaces, no Windows device name
/// (`CON`, `NUL`, `COM1`...), whitespace collapsed, NFC, at most [`MAX_NAME`] characters with a short extension
/// kept. `file` if nothing is left.
pub fn clean_name(raw: &str) -> String {
    clean_name_utf16(&raw.encode_utf16().collect::<Vec<_>>())
}

/// [`clean_name`] of a name as JS holds it (UTF-16, perhaps with lone surrogates).
pub fn clean_name_utf16(raw: &[u16]) -> String {
    // raw.slice(0, MAX_RAW_NAME).replace(/\p{Cs}/gu, '') — a pair cut in two by the slice leaves a lone half.
    let raw = &raw[..raw.len().min(MAX_RAW_NAME)];
    let s: String = char::decode_utf16(raw.iter().copied()).filter_map(Result::ok).collect();
    let s = unicode::nfc(&s);
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_control() || unicode::is_format(c) || c == '\u{2028}' || c == '\u{2029}' {
            continue;
        }
        if is_js_space(c) {
            // .replace(/\s+/g, ' ')
            if !space {
                out.push(' ');
            }
            space = true;
            continue;
        }
        space = false;
        out.push(if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c });
    }
    let mut s = trim_dots_spaces(&out).to_string();
    if is_device(&s) {
        s.insert(0, '_');
    }
    let cps = s.chars().count();
    if cps > MAX_NAME {
        // const dot = s.lastIndexOf('.'); ext if dot > 0 && s.length - dot <= 16 (UTF-16 units), kept whole.
        let units: usize = s.encode_utf16().count();
        let ext = match s.rfind('.') {
            Some(at) if at > 0 && units - s[..at].encode_utf16().count() <= 16 => &s[at..],
            _ => "",
        };
        let keep = MAX_NAME - ext.chars().count();
        let head: String = s.chars().take(keep).collect();
        s = format!("{}{}", trim_end_dots_spaces(&head), ext);
    }
    if s.is_empty() {
        "file".into()
    } else {
        s
    }
}

// JS `\s`: WhiteSpace and LineTerminator. Of these, the controls, U+FEFF (Cf) and U+2028/9 are gone by the time
// this is asked, but the set is the whole one.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{b}'
            | '\u{c}'
            | '\r'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

// .replace(/^[\s.]+/, '').replace(/[\s.]+$/, '')
fn trim_dots_spaces(s: &str) -> &str {
    trim_end_dots_spaces(s.trim_start_matches(|c| c == '.' || is_js_space(c)))
}

fn trim_end_dots_spaces(s: &str) -> &str {
    s.trim_end_matches(|c| c == '.' || is_js_space(c))
}

// /^(con|prn|aux|nul|com[0-9¹²³]|lpt[0-9¹²³])(\.|$)/i: without the `u` flag, `i` folds ASCII letters alone.
fn is_device(s: &str) -> bool {
    let mut cs = s.chars();
    let word: String = cs.by_ref().take(3).map(|c| c.to_ascii_lowercase()).collect();
    let rest = cs.as_str();
    let after = match word.as_str() {
        "con" | "prn" | "aux" | "nul" => rest,
        "com" | "lpt" => {
            let mut r = rest.chars();
            match r.next() {
                Some('0'..='9' | '¹' | '²' | '³') => r.as_str(),
                _ => return false,
            }
        }
        _ => return false,
    };
    after.is_empty() || after.starts_with('.')
}

/// A MIME type to show (files.js `cleanType`): a plain `type/subtype` in lower case, else empty. Never used to open
/// or save anything.
pub fn clean_type(raw: &str) -> String {
    clean_type_utf16(&raw.encode_utf16().collect::<Vec<_>>())
}

/// [`clean_type`] of a type as JS holds it.
pub fn clean_type_utf16(raw: &[u16]) -> String {
    // /^[a-z0-9][\w.+-]*\/[\w.+-]+$/i, without the `u` flag: ASCII alone.
    let word = |c: u16| c < 0x80 && ((c as u8).is_ascii_alphanumeric() || matches!(c as u8, b'_' | b'.' | b'+' | b'-'));
    let ok = raw.len() <= MAX_TYPE
        && raw.first().is_some_and(|&c| c < 0x80 && (c as u8).is_ascii_alphanumeric())
        && match raw.iter().position(|&c| c == u16::from(b'/')) {
            Some(slash) => raw[1..slash].iter().all(|&c| word(c)) && slash + 1 < raw.len() && raw[slash + 1..].iter().all(|&c| word(c)),
            None => false,
        };
    if ok {
        raw.iter().map(|&c| (c as u8).to_ascii_lowercase() as char).collect()
    } else {
        String::new()
    }
}

// ---------- control messages ----------

/// A control message read from a peer: its fields, as `JSON.parse` gave them.
#[derive(Debug, Clone, PartialEq)]
pub struct Control(pub(crate) Object);

impl Control {
    /// Its `t`, as UTF-16 (always a string: [`decode_control`] takes nothing else).
    pub fn kind(&self) -> &[u16] {
        self.0.get("t").str().unwrap_or_default()
    }

    /// Whether its `t` is `t`.
    pub fn is(&self, t: &str) -> bool {
        self.0.get("t").is(t)
    }

    /// Its `id`, if that is a transfer id (files.js `isId`).
    pub fn id(&self) -> Option<WireId> {
        WireId::parse(self.0.get("id").str()?)
    }

    pub(crate) fn field(&self, name: &str) -> &Value {
        self.0.get(name)
    }

    /// A whole number field from `lo` to `hi` (files.js `intIn`).
    pub fn int_in(&self, name: &str, lo: u64, hi: u64) -> Option<u64> {
        int_in(self.field(name), lo, hi)
    }

    /// Its `sha256`, if it is 64 lowercase hex digits.
    pub fn sha256(&self) -> Option<String> {
        let s = self.field("sha256").str()?;
        is_sha(s).then(|| s.iter().map(|&c| c as u8 as char).collect())
    }
}

/// `Number.isSafeInteger(v) && v >= lo && v <= hi`.
pub(crate) fn int_in(v: &Value, lo: u64, hi: u64) -> Option<u64> {
    let n = v.num()?;
    let safe = n.is_finite() && n.trunc() == n && n.abs() <= ((1u64 << 53) - 1) as f64;
    // -0 is a safe integer, and 0.
    (safe && n >= lo as f64 && n <= hi as f64).then_some(n as u64)
}

/// A control frame's message (files.js `decodeControl`), or `None`: not a control frame, over [`MAX_CONTROL`] bytes
/// (never parsed), not JSON, or not an object with a string `t`.
pub fn decode_control(f: &[u8]) -> Option<Control> {
    if f.first() != Some(&CONTROL_FRAME) || f.len() > 1 + MAX_CONTROL {
        return None;
    }
    let obj = json::parse_object(&f[1..])?;
    obj.get("t").str()?;
    Some(Control(obj))
}

/// A control message this side sends, as share.js writes each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// The receiver takes the file (`have` pieces of it already, after a new link).
    Accept { id: WireId, have: u64 },
    Decline { id: WireId },
    Cancel { id: WireId },
    /// The receiver has written `have` pieces.
    Ack { id: WireId, have: u64 },
    /// The sender has sent every piece: the whole file's SHA-256.
    End { id: WireId, sha256: String },
    /// The receiver's verdict: whether the whole file's SHA-256 matched, and the one it got.
    Done { id: WireId, ok: bool, sha256: String },
    /// The sender, back on a new link, asks where a transfer is.
    Resume { id: WireId },
}

/// A control frame (files.js `encodeControl`). Its JSON is short (an id, a number, a hash), never near
/// [`MAX_CONTROL`].
pub fn encode_control(m: &Message) -> Vec<u8> {
    let (t, id) = match m {
        Message::Accept { id, .. } => ("accept", id),
        Message::Decline { id } => ("decline", id),
        Message::Cancel { id } => ("cancel", id),
        Message::Ack { id, .. } => ("ack", id),
        Message::End { id, .. } => ("end", id),
        Message::Done { id, .. } => ("done", id),
        Message::Resume { id } => ("resume", id),
    };
    let mut s = format!(r#"{{"t":"{t}","id":"{}""#, id.hex());
    match m {
        Message::Accept { have, .. } | Message::Ack { have, .. } => s += &format!(r#","have":{have}"#),
        Message::End { sha256, .. } => s += &format!(r#","sha256":"{sha256}""#),
        Message::Done { ok, sha256, .. } => s += &format!(r#","ok":{ok},"sha256":"{sha256}""#),
        _ => {}
    }
    s.push('}');
    control_frame(&s)
}

fn control_frame(json: &str) -> Vec<u8> {
    let mut f = Vec::with_capacity(1 + json.len());
    f.push(CONTROL_FRAME);
    f.extend_from_slice(json.as_bytes());
    f
}

// ---------- offers ----------

/// A file on offer: what an offer says, checked and cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub id: WireId,
    /// Cleaned ([`clean_name`]).
    pub name: String,
    pub size: u64,
    /// Cleaned ([`clean_type`]); empty if none.
    pub mime: String,
    pub piece: u32,
    pub count: u64,
}

impl Offer {
    /// The offer of a file this side sends (files.js `makeOffer`), in pieces of [`PIECE`] bytes.
    pub fn new(id: WireId, name: &str, size: u64, mime: &str) -> Offer {
        Offer { id, name: clean_name(name), size, mime: clean_type(mime), piece: PIECE, count: piece_count(size, PIECE) }
    }

    /// Its control frame, with the fields in makeOffer's order.
    pub fn encode(&self) -> Vec<u8> {
        let mut s = format!(r#"{{"t":"offer","v":1,"id":"{}","name":"#, self.id.hex());
        json::write_str(&mut s, &self.name);
        s += &format!(r#","size":{},"type":"#, self.size);
        json::write_str(&mut s, &self.mime);
        s += &format!(r#","piece":{},"count":{}}}"#, self.piece, self.count);
        control_frame(&s)
    }
}

/// A peer's offer (files.js `parseOffer`), or `None` if it isn't a valid one. The name comes back cleaned, the type
/// checked.
pub fn parse_offer(m: &Control) -> Option<Offer> {
    if !m.is("offer") {
        return None;
    }
    let id = m.id()?;
    let size = m.int_in("size", 0, MAX_SIZE)?;
    let piece = m.int_in("piece", MIN_PIECE as u64, MAX_PIECE as u64)? as u32;
    let count = piece_count(size, piece);
    match m.field("count") {
        Value::Absent => {}
        Value::Num(n) if *n == count as f64 => {}
        _ => return None,
    }
    let name = m.field("name").str()?;
    if name.len() > MAX_RAW_NAME {
        return None;
    }
    let mime = match m.field("type") {
        Value::Absent => String::new(),
        Value::Str(t) => clean_type_utf16(t),
        _ => return None,
    };
    Some(Offer { id, name: clean_name_utf16(name), size, mime, piece, count })
}

// ---------- pieces ----------

/// A piece frame, read: views into the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece<'a> {
    pub id: WireId,
    pub index: u32,
    pub hash: &'a [u8; 32],
    pub data: &'a [u8],
}

/// A piece as one frame (files.js `encodePiece`); `hash` is the SHA-256 of `data`.
pub fn encode_piece(id: WireId, index: u32, hash: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(HEADER + data.len());
    f.push(PIECE_FRAME);
    f.extend_from_slice(&id.0);
    f.extend_from_slice(&index.to_be_bytes());
    f.extend_from_slice(hash);
    f.extend_from_slice(data);
    f
}

/// A piece frame, read (files.js `decodePiece`), or `None` if it isn't one.
pub fn decode_piece(f: &[u8]) -> Option<Piece<'_>> {
    if f.len() < HEADER || f[0] != PIECE_FRAME {
        return None;
    }
    Some(Piece {
        id: WireId(f[1..1 + ID_BYTES].try_into().ok()?),
        index: u32::from_be_bytes(f[1 + ID_BYTES..1 + ID_BYTES + 4].try_into().ok()?),
        hash: f[1 + ID_BYTES + 4..HEADER].try_into().ok()?,
        data: &f[HEADER..],
    })
}

/// Why a piece was refused (files.js `checkPiece`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Past the file's last piece.
    OutOfRange,
    /// One the receiver has.
    Duplicate,
    /// One past the next.
    Gap,
    /// Not the piece's size.
    BadLength,
}

impl Refused {
    /// As files.js names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Refused::OutOfRange => "out-of-range",
            Refused::Duplicate => "duplicate",
            Refused::Gap => "gap",
            Refused::BadLength => "bad-length",
        }
    }
}

/// Whether piece `index` of `len` bytes is the one a transfer of `size` bytes in pieces of `piece` (`count` of
/// them) takes next, `next` being the index it expects. Pieces come in order on the reliable lane, so anything else
/// is refused.
pub fn check_piece(size: u64, piece: u32, count: u64, next: u64, index: u32, len: usize) -> Result<(), Refused> {
    let index = index as u64;
    if index >= count {
        return Err(Refused::OutOfRange);
    }
    if index < next {
        return Err(Refused::Duplicate);
    }
    if index > next {
        return Err(Refused::Gap);
    }
    let (start, end) = piece_range(size, piece, index);
    if len as u64 == end - start {
        Ok(())
    } else {
        Err(Refused::BadLength)
    }
}

/// Where a sender goes on from when the receiver says it has `have` pieces of a transfer of `count` pieces, of which
/// the sender has read (and hashed) `read` (files.js `resumeAt`): the next piece's index, or `None` if `have` can't
/// be (not a whole number, more than the file has, or more than was ever sent).
pub(crate) fn resume_at(count: u64, read: u64, have: &Value) -> Option<u64> {
    int_in(have, 0, count.min(read))
}

/// `have` in an accept, as share.js takes a first one: exactly 0.
pub(crate) fn is_zero(have: &Value) -> bool {
    have.num() == Some(0.0)
}
