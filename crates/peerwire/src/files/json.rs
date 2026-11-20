//! A control message's JSON, read as share.js reads it (`TextDecoder`, then `JSON.parse`) and written as
//! `JSON.stringify` writes it.
//!
//! A peer that read a control message any other way would take or drop messages the browser doesn't, so this reader
//! keeps what V8 keeps:
//! - bytes are decoded as `TextDecoder` decodes them: a leading BOM dropped, each invalid sequence as U+FFFD;
//! - strings are UTF-16 code units, lone surrogates (`"\ud800"`) and all, as JS holds them;
//! - numbers are f64, rounded to nearest; past f64 they are infinite (`1e400`), as `JSON.parse` gives them;
//! - a key given twice keeps its last value;
//! - any depth: nested values are checked with a stack of their own, never recursion, so no input (at most
//!   `MAX_CONTROL` bytes anyway) can overflow the thread's stack.
//!
//! Only the top-level object's fields are kept, and of those only the ones the protocol reads ([`Field`]); the rest
//! are checked and skipped, as share.js ignores them.

/// A string as JS holds it: UTF-16 code units.
pub(crate) type Utf16 = Vec<u16>;

/// A top-level field's value, as far as the protocol tells values apart.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) enum Value {
    /// Not given (`undefined`).
    #[default]
    Absent,
    Null,
    Bool(bool),
    Num(f64),
    Str(Utf16),
    /// An object or an array.
    Other,
}

impl Value {
    /// The string, if this is one.
    pub(crate) fn str(&self) -> Option<&[u16]> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The number, if this is one.
    pub(crate) fn num(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// Whether this is the string `s` (ASCII).
    pub(crate) fn is(&self, s: &str) -> bool {
        self.str().is_some_and(|v| v.iter().copied().eq(s.bytes().map(u16::from)))
    }
}

/// The fields of a control message the protocol reads (files.js and share.js), by name.
pub(crate) const FIELDS: [&str; 10] = ["t", "id", "have", "ok", "sha256", "name", "size", "type", "piece", "count"];

/// A control message's top-level object: the value of each of [`FIELDS`], in that order.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Object(pub(crate) [Value; FIELDS.len()]);

impl Object {
    pub(crate) fn get(&self, name: &str) -> &Value {
        let i = FIELDS.iter().position(|f| *f == name).expect("a known field");
        &self.0[i]
    }
}

/// What `JSON.parse(new TextDecoder().decode(bytes))` gives, if it is an object (not an array): its fields the
/// protocol reads. `None` where `JSON.parse` throws, or for any other value.
pub(crate) fn parse_object(bytes: &[u8]) -> Option<Object> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut r = Reader { text, s: text.as_bytes(), i: 0 };
    r.ws();
    if !r.eat(b'{') {
        // Some other value, or not JSON: null either way.
        return None;
    }
    let mut obj = Object::default();
    r.ws();
    if !r.eat(b'}') {
        loop {
            r.ws();
            let key = r.string()?;
            r.ws();
            if !r.eat(b':') {
                return None;
            }
            r.ws();
            let v = r.value()?;
            if let Some(i) = FIELDS.iter().position(|f| f.bytes().map(u16::from).eq(key.iter().copied())) {
                obj.0[i] = v;
            }
            r.ws();
            if r.eat(b',') {
                continue;
            }
            if r.eat(b'}') {
                break;
            }
            return None;
        }
    }
    r.ws();
    (r.i == r.s.len()).then_some(obj)
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
                    // The text is valid UTF-8 (decoded lossily), so this starts a character.
                    self.i -= 1;
                    let c = self.text[self.i..].chars().next()?;
                    self.i += c.len_utf8();
                    let mut buf = [0u16; 2];
                    out.extend_from_slice(c.encode_utf16(&mut buf));
                }
            }
        }
    }

    // A literal, number or string as itself; an object or array checked to its end and skipped.
    fn value(&mut self) -> Option<Value> {
        match self.peek()? {
            b'"' => self.string().map(Value::Str),
            b'-' | b'0'..=b'9' => self.number().map(Value::Num),
            b't' => self.word(b"true").map(|_| Value::Bool(true)),
            b'f' => self.word(b"false").map(|_| Value::Bool(false)),
            b'n' => self.word(b"null").map(|_| Value::Null),
            b'{' | b'[' => self.skip_nested().map(|_| Value::Other),
            _ => None,
        }
    }

    // An object or array, any depth, with a stack of its own: true for each open object, false for each array.
    fn skip_nested(&mut self) -> Option<()> {
        let mut open: Vec<bool> = Vec::new();
        loop {
            // At a value's start.
            self.ws();
            match self.peek()? {
                b'{' | b'[' => {
                    let obj = self.next()? == b'{';
                    open.push(obj);
                    self.ws();
                    if self.eat(if obj { b'}' } else { b']' }) {
                        open.pop();
                    } else {
                        if obj {
                            self.string()?;
                            self.ws();
                            if !self.eat(b':') {
                                return None;
                            }
                        }
                        continue;
                    }
                }
                _ => {
                    self.value()?;
                }
            }
            // After a value: a comma and the next one, or the ends of containers.
            loop {
                let Some(&obj) = open.last() else { return Some(()) };
                self.ws();
                if self.eat(b',') {
                    if obj {
                        self.ws();
                        self.string()?;
                        self.ws();
                        if !self.eat(b':') {
                            return None;
                        }
                    }
                    break;
                }
                if !self.eat(if obj { b'}' } else { b']' }) {
                    return None;
                }
                open.pop();
            }
        }
    }
}

/// `s` as `JSON.stringify` writes a string: quoted, `"` and `\` escaped, the C0 controls as `\b` `\f` `\n` `\r`
/// `\t` or `\u00xx`, everything else as itself.
pub(crate) fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
