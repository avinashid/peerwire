//! Minimal STUN (RFC 5389) Binding client codec, for learning our public
//! address from any STUN server before we have peers to ask, and the STUN
//! messages a TURN client and server exchange (RFC 8489, RFC 8656): [`Message`],
//! [`MessageBuilder`], long-term credentials ([`long_term_key`]) and ChannelData.
//!
//! STUN shares the socket with Peerwire packets. Following RFC 7983, STUN messages
//! start with a byte in 0..=3, TURN ChannelData with 64..=79 and Peerwire packets
//! with 16..=31 (version nibble 1), so [`is_stun`] and [`is_channel_data`] demultiplex.

use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha1::Sha1;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED: u16 = 0x0001;
const ATTR_XOR_MAPPED: u16 = 0x0020;

pub type TxId = [u8; 12];

/// Whether a datagram looks like a STUN message rather than a Peerwire packet.
pub fn is_stun(b: &[u8]) -> bool {
    b.len() >= 20 && b[0] <= 3 && u32::from_be_bytes([b[4], b[5], b[6], b[7]]) == MAGIC
}

pub fn binding_request(tx: &TxId) -> Vec<u8> {
    let mut v = Vec::with_capacity(20);
    v.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    v.extend_from_slice(&0u16.to_be_bytes());
    v.extend_from_slice(&MAGIC.to_be_bytes());
    v.extend_from_slice(tx);
    v
}

/// A Binding success response telling the requester it was seen at `addr`
/// (XOR-MAPPED-ADDRESS): what a STUN server sends, for tests and simulators.
pub fn binding_response(tx: &TxId, addr: SocketAddr) -> Vec<u8> {
    let attr = encode_xor_addr(addr, tx);
    let mut v = Vec::with_capacity(24 + attr.len());
    v.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    v.extend_from_slice(&(4 + attr.len() as u16).to_be_bytes());
    v.extend_from_slice(&MAGIC.to_be_bytes());
    v.extend_from_slice(tx);
    v.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
    v.extend_from_slice(&(attr.len() as u16).to_be_bytes());
    v.extend_from_slice(&attr);
    v
}

/// Whether `b` is a Binding request; returns its transaction id.
pub fn parse_binding_request(b: &[u8]) -> Option<TxId> {
    if !is_stun(b) || u16::from_be_bytes([b[0], b[1]]) != BINDING_REQUEST {
        return None;
    }
    b[8..20].try_into().ok()
}

/// The reflexive address in a Binding success response to transaction `tx`.
pub fn parse_binding_response(b: &[u8], tx: &TxId) -> Option<SocketAddr> {
    if !is_stun(b) || u16::from_be_bytes([b[0], b[1]]) != BINDING_SUCCESS || &b[8..20] != tx {
        return None;
    }
    let len = u16::from_be_bytes([b[2], b[3]]) as usize;
    let mut attrs = b.get(20..20 + len)?;
    let mut mapped = None;
    while attrs.len() >= 4 {
        let t = u16::from_be_bytes([attrs[0], attrs[1]]);
        let l = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let v = attrs.get(4..4 + l)?;
        match t {
            ATTR_XOR_MAPPED => return decode_addr(v, Some(tx)),
            ATTR_MAPPED => mapped = decode_addr(v, None),
            _ => {}
        }
        attrs = attrs.get((4 + l).next_multiple_of(4)..).unwrap_or(&[]);
    }
    mapped
}

fn decode_addr(v: &[u8], xor: Option<&TxId>) -> Option<SocketAddr> {
    if v.len() < 4 {
        return None;
    }
    let mut key = [0u8; 16];
    if let Some(tx) = xor {
        key[..4].copy_from_slice(&MAGIC.to_be_bytes());
        key[4..].copy_from_slice(tx);
    }
    let port = u16::from_be_bytes([v[2] ^ key[0], v[3] ^ key[1]]);
    let ip = match (v[1], v.len()) {
        (1, 8) => IpAddr::V4(Ipv4Addr::new(v[4] ^ key[0], v[5] ^ key[1], v[6] ^ key[2], v[7] ^ key[3])),
        (2, 20) => {
            let mut o = [0u8; 16];
            for i in 0..16 {
                o[i] = v[4 + i] ^ key[i];
            }
            IpAddr::V6(Ipv6Addr::from(o))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// An XOR-…-ADDRESS value: `addr` XORed with the magic cookie and transaction `tx`.
fn encode_xor_addr(addr: SocketAddr, tx: &TxId) -> Vec<u8> {
    let mut key = [0u8; 16];
    key[..4].copy_from_slice(&MAGIC.to_be_bytes());
    key[4..].copy_from_slice(tx);
    let port = addr.port().to_be_bytes();
    let (family, ip) = match addr.ip() {
        IpAddr::V4(ip) => (1, ip.octets().to_vec()),
        IpAddr::V6(ip) => (2, ip.octets().to_vec()),
    };
    let mut v = vec![0, family, port[0] ^ key[0], port[1] ^ key[1]];
    v.extend(ip.iter().zip(key).map(|(b, k)| b ^ k));
    v
}

/// A STUN message's class (RFC 8489 section 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Request,
    Indication,
    Success,
    Error,
}

/// TURN's methods (RFC 8656 section 17), and STUN's Binding.
pub mod method {
    pub const BINDING: u16 = 0x001;
    pub const ALLOCATE: u16 = 0x003;
    pub const REFRESH: u16 = 0x004;
    pub const SEND: u16 = 0x006;
    pub const DATA: u16 = 0x007;
    pub const CREATE_PERMISSION: u16 = 0x008;
    pub const CHANNEL_BIND: u16 = 0x009;
}

/// The attributes a TURN client and server use (RFC 8489 section 18.3, RFC 8656 section 18).
pub mod attr {
    pub const USERNAME: u16 = 0x0006;
    pub const MESSAGE_INTEGRITY: u16 = 0x0008;
    pub const ERROR_CODE: u16 = 0x0009;
    pub const CHANNEL_NUMBER: u16 = 0x000C;
    pub const LIFETIME: u16 = 0x000D;
    pub const XOR_PEER_ADDRESS: u16 = 0x0012;
    pub const DATA: u16 = 0x0013;
    pub const REALM: u16 = 0x0014;
    pub const NONCE: u16 = 0x0015;
    pub const XOR_RELAYED_ADDRESS: u16 = 0x0016;
    pub const REQUESTED_TRANSPORT: u16 = 0x0019;
    pub const XOR_MAPPED_ADDRESS: u16 = 0x0020;
    pub const FINGERPRINT: u16 = 0x8028;
}

/// REQUESTED-TRANSPORT's value for UDP (IANA protocol 17).
pub const TRANSPORT_UDP: [u8; 4] = [17, 0, 0, 0];

/// Most attributes [`Message::parse`] keeps: a TURN message has at most 8.
const MAX_ATTRS: usize = 24;

const FINGERPRINT_XOR: u32 = 0x5354_554e;

/// A STUN message, parsed from a datagram that holds exactly one. Attributes after
/// MESSAGE-INTEGRITY other than FINGERPRINT are left out (RFC 8489 section 14.5), and
/// a FINGERPRINT, if any, must be last and right.
#[derive(Clone, Debug)]
pub struct Message<'a> {
    pub method: u16,
    pub class: Class,
    pub tx: TxId,
    raw: &'a [u8],
    /// Type, value, and where the attribute starts in `raw`.
    attrs: Vec<(u16, &'a [u8], usize)>,
}

impl<'a> Message<'a> {
    pub fn parse(b: &'a [u8]) -> Option<Message<'a>> {
        if !is_stun(b) || b[0] & 0xC0 != 0 {
            return None;
        }
        let len = u16::from_be_bytes([b[2], b[3]]) as usize;
        if !len.is_multiple_of(4) || b.len() != 20 + len {
            return None;
        }
        let (method, class) = split_type(u16::from_be_bytes([b[0], b[1]]));
        let tx: TxId = b[8..20].try_into().ok()?;
        let (mut attrs, mut at, mut integrity) = (Vec::new(), 20, false);
        while at < b.len() {
            let t = u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]);
            let l = u16::from_be_bytes([*b.get(at + 2)?, *b.get(at + 3)?]) as usize;
            let v = b.get(at + 4..at + 4 + l)?;
            let next = at + 4 + l.next_multiple_of(4);
            if t == attr::FINGERPRINT {
                let crc = u32::from_be_bytes(v.try_into().ok()?);
                if next != b.len() || crc != fingerprint(&b[..at], at + 8) {
                    return None;
                }
            } else if !integrity {
                if attrs.len() == MAX_ATTRS {
                    return None;
                }
                attrs.push((t, v, at));
                integrity = t == attr::MESSAGE_INTEGRITY;
            }
            at = next;
        }
        (at == b.len()).then_some(Message { method, class, tx, raw: b, attrs })
    }

    /// The attributes, in order: type and value.
    pub fn attrs(&self) -> impl Iterator<Item = (u16, &'a [u8])> + '_ {
        self.attrs.iter().map(|a| (a.0, a.1))
    }

    /// The first attribute of type `t`.
    pub fn attr(&self, t: u16) -> Option<&'a [u8]> {
        self.attrs.iter().find(|a| a.0 == t).map(|a| a.1)
    }

    /// An XOR-…-ADDRESS attribute's address.
    pub fn xor_address(&self, t: u16) -> Option<SocketAddr> {
        decode_addr(self.attr(t)?, Some(&self.tx))
    }

    /// ERROR-CODE as a number: 401, 438, …
    pub fn error_code(&self) -> Option<u16> {
        let v = self.attr(attr::ERROR_CODE)?;
        let (class, number) = (*v.get(2)? & 7, *v.get(3)?);
        ((3..=6).contains(&class) && number < 100).then_some(u16::from(class) * 100 + u16::from(number))
    }

    /// LIFETIME, in seconds.
    pub fn lifetime(&self) -> Option<u32> {
        Some(u32::from_be_bytes(self.attr(attr::LIFETIME)?.try_into().ok()?))
    }

    /// CHANNEL-NUMBER, if it is one TURN allows (0x4000..=0x4FFF).
    pub fn channel(&self) -> Option<u16> {
        let v = self.attr(attr::CHANNEL_NUMBER)?;
        let ch = u16::from_be_bytes([*v.first()?, *v.get(1)?]);
        (v.len() == 4 && is_channel(ch)).then_some(ch)
    }

    /// A comprehension-required attribute (type below 0x8000) that isn't in [`attr`]: a
    /// response carrying one is a failure (RFC 8489 section 7.3.3).
    pub fn unknown_required(&self) -> bool {
        use attr::*;
        const KNOWN: [u16; 12] = [
            USERNAME,
            MESSAGE_INTEGRITY,
            ERROR_CODE,
            CHANNEL_NUMBER,
            LIFETIME,
            XOR_PEER_ADDRESS,
            DATA,
            REALM,
            NONCE,
            XOR_RELAYED_ADDRESS,
            REQUESTED_TRANSPORT,
            XOR_MAPPED_ADDRESS,
        ];
        self.attrs.iter().any(|a| a.0 < 0x8000 && !KNOWN.contains(&a.0))
    }

    /// Whether MESSAGE-INTEGRITY is there and right under `key` (the password for short-term
    /// credentials, [`long_term_key`] for long-term ones). Checked in constant time.
    pub fn verify(&self, key: &[u8]) -> bool {
        let Some(&(_, mac, at)) = self.attrs.iter().find(|a| a.0 == attr::MESSAGE_INTEGRITY) else { return false };
        let Ok(mut h) = Hmac::<Sha1>::new_from_slice(key) else { return false };
        h.update(&self.raw[..2]);
        h.update(&((at + 24 - 20) as u16).to_be_bytes());
        h.update(&self.raw[4..at]);
        h.verify_slice(mac).is_ok()
    }
}

/// A STUN message under construction: attributes go in in order, then
/// [`integrity`](MessageBuilder::integrity) and [`fingerprint`](MessageBuilder::fingerprint) if wanted.
#[derive(Clone, Debug)]
pub struct MessageBuilder {
    buf: Vec<u8>,
    tx: TxId,
}

impl MessageBuilder {
    pub fn new(method: u16, class: Class, tx: &TxId) -> MessageBuilder {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&join_type(method, class).to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.extend_from_slice(tx);
        MessageBuilder { buf, tx: *tx }
    }

    /// Adds attribute `t`, padded to 4 bytes. A value longer than 65535 bytes is cut.
    pub fn attr(mut self, t: u16, v: &[u8]) -> MessageBuilder {
        let v = &v[..v.len().min(usize::from(u16::MAX))];
        self.buf.extend_from_slice(&t.to_be_bytes());
        self.buf.extend_from_slice(&(v.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(v);
        self.buf.resize(self.buf.len().next_multiple_of(4), 0);
        self.set_len(self.buf.len());
        self
    }

    pub fn xor_address(self, t: u16, addr: SocketAddr) -> MessageBuilder {
        let v = encode_xor_addr(addr, &self.tx);
        self.attr(t, &v)
    }

    pub fn u32(self, t: u16, v: u32) -> MessageBuilder {
        self.attr(t, &v.to_be_bytes())
    }

    /// ERROR-CODE `code` (300..=699) with an empty reason.
    pub fn error(self, code: u16) -> MessageBuilder {
        self.attr(attr::ERROR_CODE, &[0, 0, (code / 100) as u8, (code % 100) as u8])
    }

    /// CHANNEL-NUMBER `ch`, with the 2 bytes RFFU.
    pub fn channel(self, ch: u16) -> MessageBuilder {
        let [hi, lo] = ch.to_be_bytes();
        self.attr(attr::CHANNEL_NUMBER, &[hi, lo, 0, 0])
    }

    /// MESSAGE-INTEGRITY under `key`, over everything so far.
    pub fn integrity(mut self, key: &[u8]) -> MessageBuilder {
        let at = self.buf.len();
        self.set_len(at + 24);
        let mut h = Hmac::<Sha1>::new_from_slice(key).expect("HMAC takes keys of any length");
        h.update(&self.buf);
        let mac = h.finalize().into_bytes();
        self.attr(attr::MESSAGE_INTEGRITY, &mac)
    }

    /// FINGERPRINT, last.
    pub fn fingerprint(self) -> MessageBuilder {
        let at = self.buf.len();
        let crc = fingerprint(&self.buf, at + 8);
        self.u32(attr::FINGERPRINT, crc)
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    fn set_len(&mut self, total: usize) {
        let len = (total - 20) as u16;
        self.buf[2..4].copy_from_slice(&len.to_be_bytes());
    }
}

/// The key of long-term credentials (RFC 8489 section 9.2.2): MD5 of `user:realm:pass`,
/// taken as given (no SASLprep: credentials are the caller's, usually ASCII).
pub fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    let mut h = Md5::new();
    for (i, part) in [username, realm, password].into_iter().enumerate() {
        if i > 0 {
            h.update(b":");
        }
        h.update(part.as_bytes());
    }
    h.finalize().into()
}

/// Whether `ch` is a channel number TURN allows.
pub fn is_channel(ch: u16) -> bool {
    (0x4000..=0x4FFF).contains(&ch)
}

/// Whether a datagram looks like TURN ChannelData (a first byte in 64..=79).
pub fn is_channel_data(b: &[u8]) -> bool {
    b.len() >= 4 && (0x40..=0x4F).contains(&b[0])
}

/// ChannelData carrying `payload` on channel `ch` (unpadded, as UDP allows).
pub fn channel_data(ch: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + payload.len());
    v.extend_from_slice(&ch.to_be_bytes());
    v.extend_from_slice(&(payload.len().min(usize::from(u16::MAX)) as u16).to_be_bytes());
    v.extend_from_slice(&payload[..payload.len().min(usize::from(u16::MAX))]);
    v
}

/// ChannelData's channel and payload. Up to 3 bytes of padding may follow.
pub fn parse_channel_data(b: &[u8]) -> Option<(u16, &[u8])> {
    if !is_channel_data(b) {
        return None;
    }
    let ch = u16::from_be_bytes([b[0], b[1]]);
    let len = u16::from_be_bytes([b[2], b[3]]) as usize;
    let payload = b.get(4..4 + len)?;
    (is_channel(ch) && b.len() - 4 - len < 4).then_some((ch, payload))
}

fn join_type(method: u16, class: Class) -> u16 {
    let c = class as u16;
    (method & 0x000F) | ((method & 0x0070) << 1) | ((method & 0x0F80) << 2) | ((c & 1) << 4) | ((c & 2) << 7)
}

fn split_type(t: u16) -> (u16, Class) {
    let method = (t & 0x000F) | ((t >> 1) & 0x0070) | ((t >> 2) & 0x0F80);
    let class = match ((t >> 4) & 1) | ((t >> 7) & 2) {
        0 => Class::Request,
        1 => Class::Indication,
        2 => Class::Success,
        _ => Class::Error,
    };
    (method, class)
}

/// FINGERPRINT's value for a message whose first `upto` bytes are `head`, with the
/// length field set as if the message were `total` bytes long.
fn fingerprint(head: &[u8], total: usize) -> u32 {
    let len = ((total - 20) as u16).to_be_bytes();
    let bytes = head[..2].iter().chain(&len).chain(&head[4..]);
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc ^ FINGERPRINT_XOR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc5769_ipv4_response() {
        // RFC 5769 section 2.2: response carrying XOR-MAPPED-ADDRESS 192.0.2.1:32853
        let tx: TxId = [0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae];
        let msg: [u8; 80] = [
            0x01, 0x01, 0x00, 0x3c, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87,
            0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76, 0x65, 0x63, 0x74, 0x6f, 0x72, 0x20,
            0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1, 0x12, 0xa6, 0x43, 0x00, 0x08, 0x00, 0x14, 0x2b, 0x91,
            0xf5, 0x99, 0xfd, 0x9e, 0x90, 0xc3, 0x8c, 0x74, 0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53, 0xf0, 0x6b, 0xe7, 0xd7,
            0x80, 0x28, 0x00, 0x04, 0xc0, 0x7d, 0x4c, 0x96,
        ];
        assert!(is_stun(&msg));
        assert_eq!(parse_binding_response(&msg, &tx), Some("192.0.2.1:32853".parse().unwrap()));
        let mut other = tx;
        other[0] ^= 1;
        assert_eq!(parse_binding_response(&msg, &other), None);
        for cut in 0..msg.len() {
            let _ = parse_binding_response(&msg[..cut], &tx);
        }
    }

    #[test]
    fn rfc5769_ipv6_response() {
        // RFC 5769 section 2.3: XOR-MAPPED-ADDRESS [2001:db8:1234:5678:11:2233:4455:6677]:32853
        let tx: TxId = [0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae];
        let mut msg = vec![0x01, 0x01, 0x00, 0x18, 0x21, 0x12, 0xa4, 0x42];
        msg.extend_from_slice(&tx);
        msg.extend_from_slice(&[
            0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47, 0x01, 0x13, 0xa9, 0xfa, 0xa5, 0xd3, 0xf1, 0x79, 0xbc, 0x25,
            0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9,
        ]);
        assert_eq!(
            parse_binding_response(&msg, &tx),
            Some("[2001:db8:1234:5678:11:2233:4455:6677]:32853".parse().unwrap())
        );
    }

    #[test]
    fn response_round_trips() {
        for addr in ["198.51.100.7:4242", "[2001:db8::9]:65535"] {
            let (tx, addr) = ([3; 12], addr.parse().unwrap());
            assert_eq!(parse_binding_request(&binding_request(&tx)), Some(tx));
            let r = binding_response(&tx, addr);
            assert_eq!(parse_binding_request(&r), None);
            assert_eq!(parse_binding_response(&r, &tx), Some(addr));
            assert_eq!(parse_binding_response(&r, &[4; 12]), None);
        }
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// RFC 5769's vectors for MESSAGE-INTEGRITY and FINGERPRINT (also checked with Python's hmac, hashlib and
    /// zlib), and the long-term key.
    #[test]
    fn rfc5769_integrity_and_fingerprint() {
        // Section 2.1: a request under short-term credentials, padded with spaces, with a FINGERPRINT.
        let req = unhex(concat!(
            "000100582112a442b7e7a701bc34d686fa87dfae802200105354554e207465737420636c69656e74002400046e0001ff",
            "80290008932ff9b151263b36000600096576746a3a68367659202020000800149aeaa70cbfd8cb56781ef2b5b2d3f249",
            "c1b571a280280004e57a3bcf",
        ));
        let m = Message::parse(&req).unwrap();
        assert_eq!((m.method, m.class), (method::BINDING, Class::Request));
        assert_eq!(m.attr(attr::USERNAME), Some(&b"evtj:h6vY"[..]));
        assert!(m.verify(b"VOkJxbRl1RmTxUk/WvJxBt") && !m.verify(b"VOkJxbRl1RmTxUk/WvJxBu"));
        assert!(m.unknown_required(), "PRIORITY is ICE's");
        let mut bad = req.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(Message::parse(&bad).is_none(), "a wrong FINGERPRINT");

        // Section 2.4: long-term credentials (the password after SASLprep).
        let req = unhex(concat!(
            "000100602112a44278ad3433c6ad72c029da412e00060012e3839ee38388e383aae38383e382afe382b900000015001c",
            "662f2f3439396b39353464364f4c33346f4c394653547679363473410014000b6578616d706c652e6f72670000080014",
            "f67024656dd64a3e02b8e0712e85c9a28ca89666",
        ));
        let m = Message::parse(&req).unwrap();
        let key = long_term_key("マトリックス", "example.org", "TheMatrIX");
        assert_eq!(unhex("e8ca7ad59d5eb0518e312911d2dab2a9"), key);
        assert_eq!(
            (m.attr(attr::REALM), m.attr(attr::NONCE)),
            (Some(&b"example.org"[..]), Some(&b"f//499k954d6OL34oL9FSTvy64sA"[..]))
        );
        assert!(m.verify(&key) && !m.verify(&long_term_key("マトリックス", "example.org", "TheMatrix")));
        assert_eq!(unhex("8493fbc53ba582fb4c044c456bdc40eb"), long_term_key("user", "realm", "pass"));
        // A flipped bit anywhere breaks the parse or the MESSAGE-INTEGRITY.
        for i in 0..req.len() {
            for bit in 0..8 {
                let mut t = req.clone();
                t[i] ^= 1 << bit;
                assert!(Message::parse(&t).is_none_or(|m| !m.verify(&key)), "byte {i} bit {bit}");
            }
        }
    }

    #[test]
    fn turn_messages_round_trip() {
        let tx = [7u8; 12];
        let key = long_term_key("app", "peerwire.test", "secret");
        let peer: SocketAddr = "[2001:db8::7]:4000".parse().unwrap();
        use method::*;
        for m in [ALLOCATE, REFRESH, SEND, DATA, CREATE_PERMISSION, CHANNEL_BIND] {
            for class in [Class::Request, Class::Indication, Class::Success, Class::Error] {
                let b = MessageBuilder::new(m, class, &tx)
                    .xor_address(attr::XOR_PEER_ADDRESS, peer)
                    .u32(attr::LIFETIME, 600)
                    .channel(0x4001)
                    .attr(attr::DATA, b"hello")
                    .error(438)
                    .integrity(&key)
                    .fingerprint()
                    .finish();
                let p = Message::parse(&b).unwrap();
                assert_eq!((p.method, p.class, p.tx), (m, class, tx));
                assert_eq!(p.xor_address(attr::XOR_PEER_ADDRESS), Some(peer));
                assert_eq!((p.lifetime(), p.channel(), p.error_code()), (Some(600), Some(0x4001), Some(438)));
                assert_eq!(p.attr(attr::DATA), Some(&b"hello"[..]));
                assert!(p.verify(&key) && !p.verify(&long_term_key("app", "peerwire.test", "Secret")));
                assert!(!p.unknown_required());
            }
        }
        // Attributes after MESSAGE-INTEGRITY are left out; with none, nothing verifies.
        let b = MessageBuilder::new(ALLOCATE, Class::Success, &tx).integrity(&key).u32(attr::LIFETIME, 1).finish();
        let p = Message::parse(&b).unwrap();
        assert!(p.verify(&key) && p.lifetime().is_none());
        let b = MessageBuilder::new(ALLOCATE, Class::Success, &tx).u32(attr::LIFETIME, 1).finish();
        assert!(!Message::parse(&b).unwrap().verify(&key));
        assert_eq!(Message::parse(&b[..b.len() - 4]).map(|m| m.tx), None, "the length field must match");
        // A channel number TURN doesn't allow, and error codes out of range.
        let p = MessageBuilder::new(CHANNEL_BIND, Class::Request, &tx).channel(0x3FFF).error(299).finish();
        assert_eq!((Message::parse(&p).unwrap().channel(), Message::parse(&p).unwrap().error_code()), (None, None));
    }

    #[test]
    fn channel_data_round_trips() {
        let cd = channel_data(0x4ABC, b"payload");
        assert!(is_channel_data(&cd) && !is_stun(&cd));
        assert_eq!(parse_channel_data(&cd), Some((0x4ABC, &b"payload"[..])));
        let mut padded = cd.clone();
        padded.push(0);
        assert_eq!(parse_channel_data(&padded), Some((0x4ABC, &b"payload"[..])), "padding may follow");
        padded.extend([0, 0, 0]);
        assert_eq!(parse_channel_data(&padded), None, "but no more than 3 bytes");
        assert_eq!(parse_channel_data(&cd[..cd.len() - 1]), None);
        assert_eq!(parse_channel_data(&channel_data(0x5000, b"x")), None, "not a channel number");
        for first in [0x00, 0x01, 0x11, 0x12, 0x13, 0x3f, 0x50] {
            assert!(!is_channel_data(&[first, 0, 0, 0]));
        }
    }

    #[test]
    fn peerwire_packets_are_not_stun() {
        let req = binding_request(&[9; 12]);
        assert!(is_stun(&req));
        for kind in 0x11..=0x13 {
            let mut p0 = req.clone();
            p0[0] = kind; // HS1, HS2, DATA
            assert!(!is_stun(&p0));
        }
    }
}
