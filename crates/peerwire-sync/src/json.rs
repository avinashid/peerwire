//! JSON as V8's `JSON.parse` reads it and `JSON.stringify` writes it.
//!
//! doc.js checks the object `JSON.parse` gives it and holds a value as the text `JSON.stringify` gives back, so
//! a member that read JSON any other way would keep or drop entries the others don't, and split the room. So this
//! reader keeps what V8 keeps:
//! - any depth: doc.js drops a frame whose *value* nests deeper than 64, but ignores a deep unknown field;
//! - lone surrogates (`"\ud800"`): strings are held as UTF-16 code units, as JS holds them;
//! - numbers past f64: `1e400` is Infinity, written `null`; `-0` is written `0`.
//!
//! Objects keep V8's key order: array indices (canonical integers up to 2^32 - 2) first, ascending, then the other
//! keys as they first came; a key given twice keeps its last value, at its first place. Bytes are read as
//! `TextDecoder` reads them: a leading BOM dropped, and each invalid sequence as U+FFFD. Reading and writing are
//! loops over a stack of their own, never recursion, so no input can overflow the thread's stack.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A string as JS holds it: UTF-16 code units, lone surrogates and all.
pub(crate) type Utf16 = Vec<u16>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Null,
    Bool(bool),
    Num(f64),
    Str(Utf16),
    Arr(Vec<u32>),
    Obj(Vec<(Utf16, u32)>),
}

/// A JSON text, parsed: its nodes, each container after its children and the root last, so that dropping it is a
/// loop too.
#[derive(Debug)]
pub(crate) struct Tree {
    nodes: Vec<Node>,
}

enum Open {
    Arr(Vec<u32>),
    Obj(Vec<(Utf16, u32)>, Utf16),
}

struct Reader<'a> {
    text: &'a str,
    s: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.s.get(self.i) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.i += 1;
        Some(b)
    }

    fn eat(&mut self, b: u8) -> bool {
        let ok = self.peek() == Some(b);
        if ok {
            self.i += 1;
        }
        ok
    }

    fn word(&mut self, w: &[u8]) -> Option<()> {
        let ok = self.s.get(self.i..self.i + w.len()) == Some(w);
        self.i += w.len();
        ok.then_some(())
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.i += 1;
        }
        self.i - start
    }

    fn number(&mut self) -> Option<f64> {
        let start = self.i;
        self.eat(b'-');
        match self.peek()? {
            b'0' => self.i += 1,
            b'1'..=b'9' => {
                self.digits();
            }
            _ => return None,
        }
        if self.eat(b'.') && self.digits() == 0 {
            return None;
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.i += 1;
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if self.digits() == 0 {
                return None;
            }
        }
        // Rounded to nearest as V8 does; past f64 it is infinite.
        self.text[start..self.i].parse().ok()
    }

    fn string(&mut self) -> Option<Utf16> {
        if !self.eat(b'"') {
            return None;
        }
        let mut out = Vec::new();
        loop {
            match self.next()? {
                b'"' => return Some(out),
                b'\\' => out.push(match self.next()? {
                    b'"' => 0x22,
                    b'\\' => 0x5c,
                    b'/' => 0x2f,
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => 0x0a,
                    b'r' => 0x0d,
                    b't' => 0x09,
                    b'u' => {
                        let mut u = 0u16;
                        for _ in 0..4 {
                            u = u << 4 | (self.next()? as char).to_digit(16)? as u16;
                        }
                        u
                    }
                    _ => return None,
                }),
                0..=0x1f => return None,
                b @ 0x20..=0x7f => out.push(b as u16),
                _ => {
                    // The text is valid UTF-8 (read lossily), so this is the start of a character.
                    self.i -= 1;
                    let c = self.text[self.i..].chars().next()?;
                    self.i += c.len_utf8();
                    let mut buf = [0u16; 2];
                    out.extend_from_slice(c.encode_utf16(&mut buf));
                }
            }
        }
    }

    // A key and its colon.
    fn key(&mut self) -> Option<Utf16> {
        let k = self.string()?;
        self.ws();
        self.eat(b':').then_some(k)
    }
}

/// The value of `k` as an array index (a canonical integer up to 2^32 - 2), which V8 orders first.
fn array_index(k: &[u16]) -> Option<u32> {
    if k.is_empty() || k.len() > 10 || (k[0] == u16::from(b'0') && k.len() > 1) {
        return None;
    }
    let mut n = 0u64;
    for &c in k {
        let d = char::from_u32(c as u32)?.to_digit(10)?;
        n = n * 10 + d as u64;
    }
    (n < u32::MAX as u64).then_some(n as u32)
}

/// An object's fields in V8's order: a key given again keeps its first place and takes the last value, then the
/// array indices go first, ascending (a stable sort, so the other keys stay in order).
fn ordered(fields: Vec<(Utf16, u32)>) -> Vec<(Utf16, u32)> {
    let mut values: Vec<u32> = Vec::with_capacity(fields.len());
    let mut again = vec![false; fields.len()];
    {
        let mut first: BTreeMap<&[u16], usize> = BTreeMap::new();
        for (i, (k, v)) in fields.iter().enumerate() {
            match first.get(k.as_slice()) {
                Some(&at) => {
                    values[at] = *v;
                    again[i] = true;
                }
                None => {
                    first.insert(k, values.len());
                    values.push(*v);
                }
            }
        }
    }
    let mut out: Vec<(Utf16, u32)> =
        fields.into_iter().zip(again).filter(|(_, again)| !again).zip(values).map(|(((k, _), _), v)| (k, v)).collect();
    if out.iter().any(|(k, _)| array_index(k).is_some()) {
        out.sort_by_key(|(k, _)| array_index(k).map_or((1, 0), |n| (0, n)));
    }
    out
}

impl Tree {
    /// `bytes` as `JSON.parse(new TextDecoder().decode(bytes))` reads them; `None` where that throws.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Tree> {
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
        let text = String::from_utf8_lossy(bytes);
        let mut r = Reader { text: &text, s: text.as_bytes(), i: 0 };
        let mut nodes: Vec<Node> = Vec::new();
        let mut stack: Vec<Open> = Vec::new();
        'value: loop {
            r.ws();
            let mut node = match r.peek()? {
                b'{' => {
                    r.i += 1;
                    r.ws();
                    if r.eat(b'}') {
                        Node::Obj(Vec::new())
                    } else {
                        stack.push(Open::Obj(Vec::new(), r.key()?));
                        continue 'value;
                    }
                }
                b'[' => {
                    r.i += 1;
                    r.ws();
                    if r.eat(b']') {
                        Node::Arr(Vec::new())
                    } else {
                        stack.push(Open::Arr(Vec::new()));
                        continue 'value;
                    }
                }
                b'"' => Node::Str(r.string()?),
                b't' => r.word(b"true").map(|_| Node::Bool(true))?,
                b'f' => r.word(b"false").map(|_| Node::Bool(false))?,
                b'n' => r.word(b"null").map(|_| Node::Null)?,
                _ => Node::Num(r.number()?),
            };
            // A value is complete: it goes into the container open around it, which it may close.
            loop {
                let at = u32::try_from(nodes.len()).ok()?;
                nodes.push(node);
                r.ws();
                match stack.last_mut() {
                    None => return (r.i == r.s.len()).then_some(Tree { nodes }),
                    Some(Open::Arr(items)) => {
                        items.push(at);
                        match r.next()? {
                            b',' => continue 'value,
                            b']' => {}
                            _ => return None,
                        }
                    }
                    Some(Open::Obj(fields, key)) => {
                        fields.push((std::mem::take(key), at));
                        match r.next()? {
                            b',' => {
                                r.ws();
                                *key = r.key()?;
                                continue 'value;
                            }
                            b'}' => {}
                            _ => return None,
                        }
                    }
                }
                node = match stack.pop()? {
                    Open::Arr(items) => Node::Arr(items),
                    Open::Obj(fields, _) => Node::Obj(ordered(fields)),
                };
            }
        }
    }

    pub(crate) fn root(&self) -> u32 {
        self.nodes.len() as u32 - 1
    }

    pub(crate) fn node(&self, at: u32) -> &Node {
        &self.nodes[at as usize]
    }

    /// The value of field `key` of object `at`, if it is an object that has one.
    pub(crate) fn field(&self, at: u32, key: &str) -> Option<u32> {
        let Node::Obj(fields) = self.node(at) else { return None };
        fields.iter().find(|(k, _)| k.iter().copied().eq(key.encode_utf16())).map(|&(_, v)| v)
    }

    pub(crate) fn num(&self, at: u32) -> Option<f64> {
        match self.node(at) {
            Node::Num(x) => Some(*x),
            _ => None,
        }
    }

    pub(crate) fn str(&self, at: u32) -> Option<&[u16]> {
        match self.node(at) {
            Node::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn arr(&self, at: u32) -> Option<&[u32]> {
        match self.node(at) {
            Node::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// Whether `at` nests no deeper than `max` arrays and objects.
    pub(crate) fn depth_within(&self, at: u32, max: usize) -> bool {
        let mut stack = vec![(at, 1usize)];
        while let Some((at, depth)) = stack.pop() {
            match self.node(at) {
                Node::Arr(_) | Node::Obj(_) if depth > max => return false,
                Node::Arr(items) => stack.extend(items.iter().map(|&c| (c, depth + 1))),
                Node::Obj(fields) => stack.extend(fields.iter().map(|&(_, c)| (c, depth + 1))),
                _ => {}
            }
        }
        true
    }

    /// `JSON.stringify` of node `at`.
    pub(crate) fn write(&self, at: u32, out: &mut String) {
        let mut stack: Vec<(u32, usize)> = Vec::new();
        let mut next = Some(at);
        loop {
            if let Some(at) = next.take() {
                match self.node(at) {
                    Node::Null => out.push_str("null"),
                    Node::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                    Node::Num(x) => write_number(*x, out),
                    Node::Str(s) => write_str(s, out),
                    Node::Arr(_) => {
                        out.push('[');
                        stack.push((at, 0));
                    }
                    Node::Obj(_) => {
                        out.push('{');
                        stack.push((at, 0));
                    }
                }
            }
            let Some((at, k)) = stack.last_mut() else { return };
            match &self.nodes[*at as usize] {
                Node::Arr(items) if *k < items.len() => {
                    if *k > 0 {
                        out.push(',');
                    }
                    next = Some(items[*k]);
                    *k += 1;
                }
                Node::Obj(fields) if *k < fields.len() => {
                    if *k > 0 {
                        out.push(',');
                    }
                    write_str(&fields[*k].0, out);
                    out.push(':');
                    next = Some(fields[*k].1);
                    *k += 1;
                }
                node => {
                    out.push(if matches!(node, Node::Arr(_)) { ']' } else { '}' });
                    stack.pop();
                }
            }
        }
    }

    pub(crate) fn text(&self, at: u32) -> String {
        let mut s = String::new();
        self.write(at, &mut s);
        s
    }
}

/// A number as ECMAScript's `Number::toString` writes it, and `null` for one that isn't finite (JSON.stringify).
pub(crate) fn write_number(x: f64, out: &mut String) {
    if !x.is_finite() {
        return out.push_str("null");
    }
    if x == 0.0 {
        return out.push('0');
    }
    if x < 0.0 {
        out.push('-');
    }
    // As few digits as read back as x and, of those, the ones nearest x, the even ones on a tie: Rust's shortest
    // form gives how many (its digits can be the other of a tie), its exact form at that many gives which. Below a
    // power of two, where the doubles are closer, the nearest may not read back: then the shortest form's.
    let x = x.abs();
    let shortest = format!("{x:e}");
    let k = shortest.split_once('e').map_or(1, |(m, _)| m.chars().filter(char::is_ascii_digit).count());
    let nearest = format!("{x:.*e}", k - 1);
    let e = if nearest.parse::<f64>() == Ok(x) { nearest } else { shortest };
    // x = 0.digits * 10^n.
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let digits: String = mantissa.chars().filter(|&c| c != '.').collect();
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().unwrap_or(0) + 1;
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', -n as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{}", if n > 0 { '+' } else { '-' }, (n - 1).abs());
    }
}

/// A string as `JSON.stringify` writes it: `"`, `\` and control characters escaped, a lone surrogate as
/// `\udxxx`, everything else (U+2028 and U+2029 too) as itself.
pub(crate) fn write_str(s: &[u16], out: &mut String) {
    out.push('"');
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        match c {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0xd800..=0xdbff if s.get(i + 1).is_some_and(|d| (0xdc00..=0xdfff).contains(d)) => {
                let cp = 0x10000 + ((c as u32 - 0xd800) << 10) + (s[i + 1] as u32 - 0xdc00);
                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                i += 1;
            }
            0..=0x1f | 0xd800..=0xdfff => {
                let _ = write!(out, "\\u{c:04x}");
            }
            _ => out.push(char::from_u32(c as u32).unwrap_or('\u{fffd}')),
        }
        i += 1;
    }
    out.push('"');
}

/// `JSON.stringify(s).length`: what string `s` takes as JSON text, in UTF-16 code units.
pub(crate) fn json_length(s: &[u16]) -> usize {
    let mut n = 2;
    let mut i = 0;
    while i < s.len() {
        n += match s[i] {
            0x22 | 0x5c | 0x08 | 0x0c | 0x0a | 0x0d | 0x09 => 2,
            0xd800..=0xdbff if s.get(i + 1).is_some_and(|d| (0xdc00..=0xdfff).contains(d)) => {
                i += 1;
                2
            }
            0..=0x1f | 0xd800..=0xdfff => 6,
            _ => 1,
        };
        i += 1;
    }
    n
}

/// A string's length in UTF-16 code units, as JS counts it.
pub(crate) fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s` as UTF-16, a lone surrogate as U+FFFD.
pub(crate) fn lossy(s: &[u16]) -> String {
    char::decode_utf16(s.iter().copied()).map(|c| c.unwrap_or('\u{fffd}')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(text: &str) -> Option<String> {
        let t = Tree::parse(text.as_bytes())?;
        Some(t.text(t.root()))
    }

    #[test]
    fn numbers_read_and_write_as_v8_does() {
        // Checked against Node 26: JSON.stringify(JSON.parse(text)).
        for (text, js) in [
            (
                "[-0, 1e21, 1e-7, 5e-324, 0.000001, 1e400, -1e400, 123e-20, 1.5e300, 0.1, 100, 1e20]",
                "[0,1e+21,1e-7,5e-324,0.000001,null,null,1.23e-18,1.5e+300,0.1,100,100000000000000000000]",
            ),
            ("12345678901234567890", "12345678901234567000"),
            ("123456789012345680000", "123456789012345680000"),
            ("[1E5, 1e+5, -0.0, 0e0, 2.5, -1.25e-3]", "[100000,100000,0,0,2.5,-0.00125]"),
            ("1e99999999999999999999", "null"),
            ("[9007199254740993, 0.1e1, 1.7976931348623157e308]", "[9007199254740992,1,1.7976931348623157e+308]"),
            // A tie between two shortest forms: the even one (Rust's shortest form gives ...063).
            ("-83626311179250.627", "-83626311179250.62"),
        ] {
            assert_eq!(canon(text).as_deref(), Some(js), "{text}");
        }
    }

    #[test]
    fn objects_keep_v8_key_order() {
        assert_eq!(
            canon(r#"{"b":1,"4294967294":2,"4294967295":3,"1":4,"01":5,"0":6,"-1":7,"1.5":8,"a":9,"b":10}"#).unwrap(),
            r#"{"0":6,"1":4,"4294967294":2,"b":10,"4294967295":3,"01":5,"-1":7,"1.5":8,"a":9}"#
        );
        assert_eq!(canon(r#"{"__proto__":1,"x":2}"#).unwrap(), r#"{"__proto__":1,"x":2}"#);
        let many: String = (0..100).map(|i| format!("\"k{}\":{i},", i % 7)).collect::<String>();
        assert_eq!(
            canon(&format!("{{{many}\"z\":0}}")).unwrap(),
            r#"{"k0":98,"k1":99,"k2":93,"k3":94,"k4":95,"k5":96,"k6":97,"z":0}"#
        );
    }

    #[test]
    fn strings_keep_lone_surrogates_and_escape_as_v8_does() {
        assert_eq!(
            canon(r#""\ud83d\ude00\uD83D \udc00\u2028\u2029\u007f\u001f\/\b""#).unwrap(),
            "\"😀\\ud83d \\udc00\u{2028}\u{2029}\u{7f}\\u001f/\\b\""
        );
        let t = Tree::parse(br#""\ud800x""#).unwrap();
        assert_eq!(t.str(t.root()).unwrap(), &[0xd800, 0x78]);
        assert_eq!(json_length(&[0xd800, 0x78]), 9);
        assert_eq!(json_length(&[0xd83d, 0xde00, 0x22, 0x1f]), 2 + 2 + 2 + 6);
    }

    #[test]
    fn bytes_are_read_as_text_decoder_reads_them() {
        // A BOM, a truncated 4-byte sequence, a surrogate's bytes, an overlong form, past U+10FFFF, a truncated
        // 3-byte one: Node's TextDecoder gives "\"\ufffd\"" then ten U+FFFD and "A".
        let bytes = [
            0xef, 0xbb, 0xbf, 0x22, 0xf0, 0x9f, 0x98, 0x22, 0xed, 0xa0, 0x80, 0xc0, 0xaf, 0xf4, 0x90, 0x80, 0x80, 0xe2,
            0x82, 0x41,
        ];
        let text = String::from_utf8_lossy(&bytes[3..]);
        assert_eq!(
            text,
            "\"\u{fffd}\"\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}\u{fffd}A"
        );
        let t = Tree::parse(&bytes[..8]).unwrap();
        assert_eq!(t.str(t.root()).unwrap(), &[0xfffd]);
    }

    #[test]
    fn malformed_text_is_refused() {
        for text in [
            "",
            " ",
            "{",
            "[1,]",
            "[,1]",
            "{\"a\"}",
            "{\"a\":1,}",
            "{a:1}",
            "01",
            "1.",
            ".5",
            "+1",
            "-",
            "1e",
            "1e+",
            "\"\\x\"",
            "\"\\u12g4\"",
            "\"\\u+123\"",
            "\"a\nb\"",
            "tru",
            "nulll",
            "[1] 2",
            "'a'",
            "NaN",
            "Infinity",
            "[1 2]",
            "{\"a\" 1}",
            "\u{a0}1",
        ] {
            assert_eq!(canon(text), None, "{text:?}");
        }
        assert_eq!(canon(" \t\r\n[ 1 , {\"a\" : [ ] } ]\n").unwrap(), "[1,{\"a\":[]}]");
    }

    #[test]
    fn depth_is_any_and_counted_on_values() {
        let deep = format!("{{\"x\":{}0{},\"d\":1}}", "[".repeat(100_000), "]".repeat(100_000));
        let t = Tree::parse(deep.as_bytes()).unwrap();
        let x = t.field(t.root(), "x").unwrap();
        assert!(!t.depth_within(x, 64));
        assert_eq!(t.num(t.field(t.root(), "d").unwrap()), Some(1.0));
        assert_eq!(t.text(t.root()).len(), deep.len());
        let ok = format!("{}{}", "[".repeat(64), "]".repeat(64));
        let t = Tree::parse(ok.as_bytes()).unwrap();
        assert!(t.depth_within(t.root(), 64));
        let t = Tree::parse(format!("[{ok}]").as_bytes()).unwrap();
        assert!(!t.depth_within(t.root(), 64));
        let t = Tree::parse(b"7").unwrap();
        assert!(t.depth_within(t.root(), 0));
    }
}

#[cfg(test)]
mod corpus {
    use super::Tree;
    use serde_json::Value;

    /// JSON texts and bytes as V8 reads and writes them (rust_doc.mjs): key order, duplicate keys, `__proto__`,
    /// numbers in every form (random doubles among them, written by Number::toString), strings with every escape,
    /// lone surrogates, U+2028/2029, invalid UTF-8 and a BOM, nesting, and texts JSON.parse refuses.
    #[test]
    fn texts_read_and_write_as_v8_does() {
        let v: Vec<Value> = serde_json::from_str(include_str!("../tests/vectors/json.json")).unwrap();
        for (k, c) in v.iter().enumerate() {
            let bytes: Vec<u8> = match (c["text"].as_str(), c["hex"].as_str()) {
                (Some(t), _) => t.as_bytes().to_vec(),
                (_, Some(h)) => {
                    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect()
                }
                _ => panic!("case {k}"),
            };
            let got = Tree::parse(&bytes).map(|t| t.text(t.root()));
            assert_eq!(got.as_deref(), c["out"].as_str(), "case {k}: {}", String::from_utf8_lossy(&bytes));
        }
        assert!(v.len() > 1500, "{} texts", v.len());
    }
}
