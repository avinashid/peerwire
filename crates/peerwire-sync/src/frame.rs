//! Doc frames from members, checked as doc.js `parseFrame` checks them: a frame with anything off is dropped
//! whole. A frame is JSON `{d, t, …}`: `t` 1 a delta, 2 an ask, 3 an answer, 4 a MORE.

use crate::context::Writer;
use crate::json::{json_length, utf16_len, Node, Tree, Utf16};
use crate::{FRAME_ITEMS, MAX_KEY_LENGTH, MAX_NAME_LENGTH, MAX_RANGES, MAX_VALUE_DEPTH, MAX_VALUE_LENGTH};
use peerwire_core::PeerId;
use std::fmt;

pub(crate) const DELTA: u8 = 1;
pub(crate) const ASK: u8 = 2;
pub(crate) const ANSWER: u8 = 3;
pub(crate) const MORE: u8 = 4;
/// `Number.MAX_SAFE_INTEGER`: the highest dot, stamp or ask number.
pub(crate) const MAX_SAFE: u64 = (1 << 53) - 1;

/// What a collection is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Map,
    Set,
    Log,
}

impl Kind {
    pub(crate) fn letter(self) -> &'static str {
        match self {
            Kind::Map => "m",
            Kind::Set => "s",
            Kind::Log => "l",
        }
    }

    /// Its name in doc.js's events: `map`, `set` or `log`.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Map => "map",
            Kind::Set => "set",
            Kind::Log => "log",
        }
    }

    fn of(s: &[u16]) -> Option<Kind> {
        match s {
            [0x6d] => Some(Kind::Map),
            [0x73] => Some(Kind::Set),
            [0x6c] => Some(Kind::Log),
            _ => None,
        }
    }
}

/// An entry's dot: (writer, n). As text, `<writer>.<n>`, the id a set item or log entry goes by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Id {
    pub(crate) w: Writer,
    pub(crate) n: u64,
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.w {
            write!(f, "{b:02x}")?;
        }
        write!(f, ".{}", self.n)
    }
}

impl Id {
    /// An item's id as doc.js writes it: 72 lowercase hex digits, a dot and n in decimal, nothing else.
    pub(crate) fn parse(s: &str) -> Option<Id> {
        let (w, n) = s.split_once('.')?;
        let w = writer_of(&w.encode_utf16().collect::<Vec<_>>())?;
        let ok = !n.is_empty() && !n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit());
        let n: u64 = if ok { n.parse().ok()? } else { return None };
        (n <= MAX_SAFE).then_some(Id { w, n })
    }
}

/// An entry: a value written to a collection, on dot (w, n) with Lamport stamp c. `k` is a map's key, `v` the
/// value as JSON text (what `JSON.stringify` gives back), and `len` what the entry takes as JSON text (value, key
/// and collection name), the length doc.js counts against MAX_DOC_LENGTH.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub(crate) w: Writer,
    pub(crate) n: u64,
    pub(crate) c: u64,
    pub(crate) t: Kind,
    pub(crate) coll: Utf16,
    pub(crate) k: Option<Utf16>,
    pub(crate) v: String,
    pub(crate) len: usize,
}

impl Entry {
    pub(crate) fn id(&self) -> Id {
        Id { w: self.w, n: self.n }
    }

    /// Entries sort by this: the later stamp, then the higher writer, then the later dot.
    pub(crate) fn stamp(&self) -> (u64, Writer, u64) {
        (self.c, self.w, self.n)
    }
}

pub(crate) type Range = (Writer, u64, u64);

/// A numbered answer's piece: the ask `q` it answers, its own number `i`, the entries `n` the whole answer
/// carries, and `s` on the last if the member had caught up itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Numbered {
    pub(crate) q: u64,
    pub(crate) i: u64,
    pub(crate) n: u64,
    pub(crate) s: bool,
}

/// A delta or an answer.
#[derive(Debug, PartialEq)]
pub(crate) struct Data {
    pub(crate) answer: bool,
    pub(crate) entries: Vec<Entry>,
    pub(crate) ranges: Vec<Range>,
    pub(crate) end: bool,
    pub(crate) numbered: Option<Numbered>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Frame {
    Ask { ranges: Vec<Range>, q: u64, z: bool },
    More { q: u64, i: u64 },
    Data(Data),
}

/// A writer as the frame names it: 72 lowercase hex digits (`/^[0-9a-f]{72}$/`).
pub(crate) fn writer_of(s: &[u16]) -> Option<Writer> {
    if s.len() != 72 {
        return None;
    }
    let digit = |c: u16| match c {
        0x30..=0x39 => Some(c as u8 - b'0'),
        0x61..=0x66 => Some(c as u8 - b'a' + 10),
        _ => None,
    };
    let mut w = [0u8; 36];
    for (i, b) in w.iter_mut().enumerate() {
        *b = digit(s[2 * i])? << 4 | digit(s[2 * i + 1])?;
    }
    Some(w)
}

/// `Number.isSafeInteger(x) && x >= min`.
fn safe(x: f64, min: u64) -> Option<u64> {
    (x.fract() == 0.0 && x >= min as f64 && x <= MAX_SAFE as f64).then_some(x as u64)
}

/// doc.js `count`: a safe integer of 1 or more.
fn count(t: &Tree, at: Option<u32>) -> Option<u64> {
    safe(t.num(at?)?, 1)
}

/// doc.js `index`: a safe integer of 0 or more (`-0` too).
fn index(t: &Tree, at: Option<u32>) -> Option<u64> {
    safe(t.num(at?)?, 0)
}

fn is_one(t: &Tree, at: u32) -> bool {
    t.num(at) == Some(1.0)
}

pub(crate) fn valid_name(s: &[u16]) -> bool {
    (1..=MAX_NAME_LENGTH).contains(&s.len())
}

/// The frame `m` of `tree` from member `peer` (or from the copy on this device, `None`), checked as doc.js
/// `parseFrame` checks it: `None` if any field is off. Entries come back with their value as JSON text.
pub(crate) fn parse(t: &Tree, m: u32, peer: Option<&PeerId>) -> Option<Frame> {
    let Node::Obj(_) = t.node(m) else { return None };
    let kind = t.field(m, "t").and_then(|x| t.num(x));
    if kind == Some(MORE as f64) {
        return Some(Frame::More { q: count(t, t.field(m, "q"))?, i: index(t, t.field(m, "i"))? });
    }
    let kind = match kind {
        Some(x) if x == DELTA as f64 => DELTA,
        Some(x) if x == ASK as f64 => ASK,
        Some(x) if x == ANSWER as f64 => ANSWER,
        _ => return None,
    };
    let ws = t.arr(t.field(m, "ws")?)?;
    if ws.len() > FRAME_ITEMS {
        return None;
    }
    let ws: Vec<Writer> = ws.iter().map(|&w| t.str(w).and_then(writer_of)).collect::<Option<_>>()?;
    let z = t.field(m, "z");
    if kind == ASK && (count(t, t.field(m, "q")).is_none() || z.is_some_and(|z| !is_one(t, z))) {
        return None;
    }
    let writer = |at: u32| -> Option<Writer> {
        let i = t.num(at)?;
        (i.fract() == 0.0 && i >= 0.0 && i < ws.len() as f64).then(|| ws[i as usize])
    };
    let mut ranges = Vec::new();
    if let Some(list) = t.field(m, if kind == ASK { "r" } else { "x" }) {
        let list = t.arr(list)?;
        if list.len() > if kind == ASK { MAX_RANGES } else { FRAME_ITEMS } {
            return None;
        }
        for &r in list {
            let [w, a, b] = *t.arr(r)? else { return None };
            let (w, a, b) = (writer(w)?, count(t, Some(a))?, count(t, Some(b))?);
            if a > b {
                return None;
            }
            ranges.push((w, a, b));
        }
    }
    let q = t.field(m, "q");
    if kind == ASK {
        return Some(Frame::Ask { ranges, q: count(t, q)?, z: z.is_some() });
    }
    let mut numbered = None;
    if kind == ANSWER && q.is_some() {
        let (i, n) = (index(t, t.field(m, "i"))?, index(t, t.field(m, "n"))?);
        numbered = Some(Numbered { q: count(t, q)?, i, n, s: false });
    }
    let (s, end) = (t.field(m, "s"), t.field(m, "end"));
    if kind == ANSWER && s.is_some_and(|s| !is_one(t, s) || !end.is_some_and(|e| is_one(t, e))) {
        return None;
    }
    let mut entries = Vec::new();
    if let Some(e) = t.field(m, "e") {
        let e = t.arr(e)?;
        if e.len() > FRAME_ITEMS {
            return None;
        }
        entries.reserve(e.len());
        for &x in e {
            let [i, n, c, ty, coll, k, v] = *t.arr(x)? else { return None };
            let (w, n, c) = (writer(i)?, count(t, Some(n))?, count(t, Some(c))?);
            // A type that isn't the string "m", "s" or "l" is dropped (doc.js since NB-051).
            let ty = Kind::of(t.str(ty)?)?;
            let coll = t.str(coll).filter(|c| valid_name(c))?;
            let k = match (ty, t.node(k)) {
                (Kind::Map, Node::Str(k)) if k.len() <= MAX_KEY_LENGTH => Some(k),
                (Kind::Set | Kind::Log, Node::Null) => None,
                _ => return None,
            };
            // A delta carries only its sender's writes.
            if kind == DELTA && peer.is_none_or(|p| w[..32] != p.0) {
                return None;
            }
            if !t.depth_within(v, MAX_VALUE_DEPTH) {
                return None;
            }
            let v = t.text(v);
            let vlen = utf16_len(&v);
            if vlen > MAX_VALUE_LENGTH {
                return None;
            }
            let len = vlen + k.map_or(4, |k| json_length(k)) + json_length(coll);
            entries.push(Entry { w, n, c, t: ty, coll: coll.to_vec(), k: k.cloned(), v, len });
        }
    }
    if end.is_some_and(|e| !is_one(t, e)) {
        return None;
    }
    if let Some(nb) = &mut numbered {
        nb.s = s.is_some();
    }
    Some(Frame::Data(Data { answer: kind == ANSWER, entries, ranges, end: end.is_some(), numbered }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::write_str;
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    /// rust_doc.mjs `short`.
    fn short(s: &str) -> String {
        if s.len() <= 96 {
            return s.to_string();
        }
        let d: String = Sha256::digest(s.as_bytes())[..16].iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256:{d}:{}", s.len())
    }

    fn hex(w: &[u8]) -> String {
        w.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// rust_doc.mjs `norm`: [t, q, i, n, s, z, end, ranges, entries], null for what the frame lacks.
    fn norm(f: &Frame) -> String {
        let ranges = |s: &mut String, rs: &[Range]| {
            s.push('[');
            for (k, (w, a, b)) in rs.iter().enumerate() {
                let _ = write!(s, "{}[\"{}\",{a},{b}]", if k > 0 { "," } else { "" }, hex(w));
            }
            s.push(']');
        };
        let mut s = String::from("[");
        match f {
            Frame::More { q, i } => {
                let _ = write!(s, "4,{q},{i},null,null,null,null,[],[]");
            }
            Frame::Ask { ranges: r, q, z } => {
                let _ = write!(s, "2,{q},null,null,null,{z},null,");
                ranges(&mut s, r);
                s.push_str(",[]");
            }
            Frame::Data(d) => {
                let t = if d.answer { 3 } else { 1 };
                match d.numbered {
                    Some(nb) => write!(s, "{t},{},{},{},{},null,{},", nb.q, nb.i, nb.n, nb.s, d.end),
                    None => write!(s, "{t},null,null,null,null,null,{},", d.end),
                }
                .unwrap();
                ranges(&mut s, &d.ranges);
                s.push_str(",[");
                for (k, e) in d.entries.iter().enumerate() {
                    let _ = write!(
                        s,
                        "{}[\"{}\",{},{},\"{}\",",
                        if k > 0 { "," } else { "" },
                        hex(&e.w),
                        e.n,
                        e.c,
                        e.t.letter()
                    );
                    write_str(&e.coll, &mut s);
                    s.push(',');
                    match &e.k {
                        Some(k) => write_str(k, &mut s),
                        None => s.push_str("null"),
                    }
                    s.push(',');
                    write_str(&e.v.encode_utf16().collect::<Vec<_>>(), &mut s);
                    let _ = write!(s, ",{}]", e.len);
                }
                s.push(']');
            }
        }
        s.push(']');
        s
    }

    /// Every frame of doc.js's corpus (rust_doc.mjs) is kept or dropped as doc.js's parseFrame keeps or drops it,
    /// and what is kept comes out the same: valid frames, frames with each field missing or odd, mutated entries,
    /// writers and ranges, duplicate keys, `__proto__`, a BOM, invalid UTF-8, values, keys and names at their caps,
    /// asks with too many ranges, and a 10^5-deep unknown field (kept) beside a value as deep (dropped).
    #[test]
    fn frames_are_judged_as_doc_js_judges_them() {
        let v: Vec<Value> = serde_json::from_str(include_str!("../tests/vectors/frames.json")).unwrap();
        let (mut kept, mut dropped) = (0, 0);
        for (k, c) in v.iter().enumerate() {
            let bytes: Vec<u8> = match (&c["parts"], c["hex"].as_str()) {
                (Value::Array(parts), _) => parts
                    .iter()
                    .map(|p| match p {
                        Value::String(s) => s.clone(),
                        p => p[0].as_str().unwrap().repeat(p[1].as_u64().unwrap() as usize),
                    })
                    .collect::<String>()
                    .into_bytes(),
                (_, Some(h)) => {
                    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect()
                }
                _ => panic!("case {k}"),
            };
            let p = c["peer"].as_str().unwrap();
            let peer = PeerId(std::array::from_fn(|i| u8::from_str_radix(&p[2 * i..2 * i + 2], 16).unwrap()));
            let got = Tree::parse(&bytes).and_then(|t| parse(&t, t.root(), Some(&peer))).map(|f| short(&norm(&f)));
            assert_eq!(
                got.as_deref(),
                c["verdict"].as_str(),
                "case {k}: {}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(300)])
            );
            if got.is_some() {
                kept += 1;
            } else {
                dropped += 1;
            }
        }
        assert!(kept > 300 && dropped > 250, "{kept} kept, {dropped} dropped");
    }
}
