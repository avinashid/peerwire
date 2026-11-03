//! Shared documents ([`peerwire_sync`]) over the C ABI: built with the `sync` feature, which the wasm build leaves
//! off (browsers use `doc.js`).
//!
//! The host drives a `PwDocs` as it drives an endpoint, beside it: it hands it the reliable messages it receives
//! (`pw_docs_receive` says whether one was a doc's), sends what `pw_docs_poll_transmit` gives on the reliable lane
//! (one the link refuses goes back with `pw_docs_retry`), tells it who joined and left, and runs its timers.
//!
//! Strings are UTF-8 (pointer, length). Values, events, entries and status are JSON text, in the shapes doc.js
//! gives them. A doc is a `u32` id from `pw_doc_open`. Kinds: 0 map, 1 set, 2 log. Routes: 0 direct, 1 relayed by
//! a member, 2 over signaling.
//!
//! Return codes: `>= 0` success, `PW_ERR_INVALID` for an unknown doc or an argument doc.js would throw for (not
//! UTF-8 or JSON, a name or key past its cap). Outputs follow `pw_poll_event`: their length, 0 if none, or
//! `-(needed)` if the buffer is too small (nothing taken). Reads (`pw_doc_get`, `_entries`, `_status`,
//! `_snapshot`) need a buffer of at least 16 bytes (else `PW_ERR_BUFFER`), so `-(needed)` never reads as an error.
//!
//! Events, from `pw_docs_poll_event`:
//! ```text
//! {"doc":0,"change":{"name":"meta","type":"map","key":"title","value":"Plans","from":"<hex>","local":false}}
//! {"doc":0,"sync":{"state":"loading","from":"<hex>","have":12,"total":40,"source":null,"at":null}}
//! {"overflow":"<hex>"}    the member's link took nothing for too long, or a hub relayed too much for it: close it
//! ```

use crate::{bytes, bytes32, now, peer, writable, PW_ERR_BUFFER, PW_ERR_INVALID};
use peerwire_core::PeerId;
use peerwire_sync::{DocId, Docs, Error, Event, Kind, Options, Route};
use std::slice;

pub struct PwDocs {
    docs: Docs,
    /// A message or event the caller's buffer was too small for, given again next time.
    message: Option<(PeerId, Vec<u8>)>,
    event: Option<Vec<u8>>,
}

const MIN_READ: usize = 16;

unsafe fn text<'a>(p: *const u8, len: usize) -> Option<&'a str> {
    std::str::from_utf8(bytes(p, len)?).ok()
}

fn kind(k: u32) -> Option<Kind> {
    match k {
        0 => Some(Kind::Map),
        1 => Some(Kind::Set),
        2 => Some(Kind::Log),
        _ => None,
    }
}

fn route(r: u32) -> Route {
    match r {
        1 => Route::Peer,
        2 => Route::Signaling,
        _ => Route::Direct,
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Copies `out` to `buf`: its length, or `-(needed)` if `cap` is too small.
unsafe fn put(out: &[u8], buf: *mut u8, cap: usize) -> i32 {
    if out.len() > cap {
        return -(out.len() as i32);
    }
    if !out.is_empty() {
        slice::from_raw_parts_mut(buf, out.len()).copy_from_slice(out);
    }
    out.len() as i32
}

/// A read: its JSON or bytes, 0 for none.
unsafe fn read(out: Option<Vec<u8>>, buf: *mut u8, cap: usize) -> i32 {
    if cap < MIN_READ {
        return PW_ERR_BUFFER;
    }
    out.map_or(0, |o| put(&o, buf, cap))
}

/// A write's result: 1 done, 0 refused (the doc is full, or there was nothing to delete), or PW_ERR_INVALID.
fn done(r: Result<bool, Error>) -> i32 {
    r.map_or(PW_ERR_INVALID, i32::from)
}

/// Creates the docs of member `me` (32 bytes). `seed`, 32 bytes from a CSPRNG, draws each doc's incarnation.
#[no_mangle]
pub unsafe extern "C" fn pw_docs_new(me: *const u8, seed: *const u8) -> *mut PwDocs {
    let (Some(me), Some(s)) = (peer(me), bytes32(seed)) else { return std::ptr::null_mut() };
    Box::into_raw(Box::new(PwDocs { docs: Docs::new(me, s), message: None, event: None }))
}

#[no_mangle]
pub unsafe extern "C" fn pw_docs_free(h: *mut PwDocs) {
    if !h.is_null() {
        drop(Box::from_raw(h));
    }
}

/// Opens doc `name` (or gives the one open) and asks every member for it. `copy` (or null, 0) is a snapshot to
/// start from; `flags` 1: this member is the hub of a star. Returns the doc's id, or PW_ERR_INVALID for a bad name,
/// a copy that isn't this doc's, or a null `copy` with a `copy_len` (then it isn't opened).
#[no_mangle]
pub unsafe extern "C" fn pw_doc_open(
    h: *mut PwDocs,
    now_us: u64,
    name: *const u8,
    name_len: usize,
    copy: *const u8,
    copy_len: usize,
    flags: u32,
) -> i64 {
    let (Some(h), Some(name), Some(copy)) = (h.as_mut(), text(name, name_len), bytes(copy, copy_len)) else {
        return PW_ERR_INVALID as i64;
    };
    let copy = (!copy.is_empty()).then_some(copy);
    match h.docs.open(now(now_us), name, Options { copy, hub: flags & 1 != 0 }) {
        Ok(id) => id.0 as i64,
        Err(_) => PW_ERR_INVALID as i64,
    }
}

/// Stops syncing doc `doc` and forgets it.
#[no_mangle]
pub unsafe extern "C" fn pw_doc_close(h: *mut PwDocs, doc: u32) {
    if let Some(h) = h.as_mut() {
        h.docs.close(DocId(doc));
    }
}

/// Member `peer` joined the room, reached by `route`; called again, its route changed.
#[no_mangle]
pub unsafe extern "C" fn pw_docs_joined(h: *mut PwDocs, now_us: u64, peer_id: *const u8, route_kind: u32) {
    if let (Some(h), Some(p)) = (h.as_mut(), peer(peer_id)) {
        h.docs.joined(now(now_us), p, route(route_kind));
    }
}

#[no_mangle]
pub unsafe extern "C" fn pw_docs_left(h: *mut PwDocs, now_us: u64, peer_id: *const u8) {
    if let (Some(h), Some(p)) = (h.as_mut(), peer(peer_id)) {
        h.docs.left(now(now_us), p);
    }
}

/// A reliable message from member `peer`: 1 if it was a doc message (taken), 0 if it is the app's.
#[no_mangle]
pub unsafe extern "C" fn pw_docs_receive(
    h: *mut PwDocs,
    now_us: u64,
    peer_id: *const u8,
    data: *const u8,
    len: usize,
) -> i32 {
    let (Some(h), Some(p), Some(data)) = (h.as_mut(), peer(peer_id), bytes(data, len)) else { return PW_ERR_INVALID };
    i32::from(h.docs.receive(now(now_us), p, data))
}

/// Writes the next message to send into `buf` and its member into `out_peer` (32 bytes): its length, 0 if none, or
/// `-(needed)` (it stays first). Send it on the reliable lane as it is.
#[no_mangle]
pub unsafe extern "C" fn pw_docs_poll_transmit(h: *mut PwDocs, out_peer: *mut u8, buf: *mut u8, cap: usize) -> i32 {
    let (Some(h), Some(out_peer), Some(buf)) = (h.as_mut(), writable(out_peer, 32), writable(buf, cap)) else {
        return PW_ERR_INVALID;
    };
    let Some((p, m)) = h.message.take().or_else(|| h.docs.poll_transmit()) else { return 0 };
    let n = put(&m, buf, cap);
    if n < 0 {
        h.message = Some((p, m));
    } else {
        slice::from_raw_parts_mut(out_peer, 32).copy_from_slice(&p.0);
    }
    n
}

/// A message from `pw_docs_poll_transmit` that `peer`'s link refused (would block): it goes first, once
/// `pw_docs_poll_transmit` has returned 0.
#[no_mangle]
pub unsafe extern "C" fn pw_docs_retry(h: *mut PwDocs, peer_id: *const u8, data: *const u8, len: usize) {
    if let (Some(h), Some(p), Some(data)) = (h.as_mut(), peer(peer_id), bytes(data, len)) {
        h.docs.retry(p, data.to_vec());
    }
}

/// Bytes waiting for member `peer`.
#[no_mangle]
pub unsafe extern "C" fn pw_docs_queued(h: *const PwDocs, peer_id: *const u8) -> i64 {
    h.as_ref().zip(peer(peer_id)).map_or(PW_ERR_INVALID as i64, |(h, p)| h.docs.queued(&p) as i64)
}

/// Writes the next event (see the module docs) into `buf`: its length, 0 if none, or `-(needed)` (it stays first).
#[no_mangle]
pub unsafe extern "C" fn pw_docs_poll_event(h: *mut PwDocs, buf: *mut u8, cap: usize) -> i32 {
    let (Some(h), Some(buf)) = (h.as_mut(), writable(buf, cap)) else { return PW_ERR_INVALID };
    let e = match h.event.take() {
        Some(e) => e,
        None => match h.docs.poll_event() {
            Some(Event::Change(d, c)) => format!("{{\"doc\":{},\"change\":{}}}", d.0, c.json()).into_bytes(),
            Some(Event::Sync(d, s)) => format!("{{\"doc\":{},\"sync\":{}}}", d.0, s.json()).into_bytes(),
            Some(Event::Overflow(p)) => format!("{{\"overflow\":\"{}\"}}", hex(&p.0)).into_bytes(),
            None => return 0,
        },
    };
    let n = put(&e, buf, cap);
    if n < 0 {
        h.event = Some(e);
    }
    n
}

/// Next deadline in microseconds, or -1 if none.
#[no_mangle]
pub unsafe extern "C" fn pw_docs_poll_timeout(h: *const PwDocs) -> i64 {
    h.as_ref().and_then(|h| h.docs.poll_timeout()).map_or(-1, |t| t.as_micros() as i64)
}

#[no_mangle]
pub unsafe extern "C" fn pw_docs_handle_timeout(h: *mut PwDocs, now_us: u64) {
    if let Some(h) = h.as_mut() {
        h.docs.handle_timeout(now(now_us));
    }
}

// Writes. Each goes to every member at once.

/// Sets map `coll`'s `key` to `value` (JSON text).
#[no_mangle]
pub unsafe extern "C" fn pw_doc_map_set(
    h: *mut PwDocs,
    doc: u32,
    coll: *const u8,
    coll_len: usize,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> i32 {
    let (Some(h), Some(c), Some(k), Some(v)) =
        (h.as_mut(), text(coll, coll_len), text(key, key_len), text(value, value_len))
    else {
        return PW_ERR_INVALID;
    };
    h.docs.doc(DocId(doc)).map_or(PW_ERR_INVALID, |mut d| done(d.map_set(c, k, v)))
}

/// Deletes map `coll`'s `key`: 1, or 0 if it held nothing.
#[no_mangle]
pub unsafe extern "C" fn pw_doc_map_delete(
    h: *mut PwDocs,
    doc: u32,
    coll: *const u8,
    coll_len: usize,
    key: *const u8,
    key_len: usize,
) -> i32 {
    let (Some(h), Some(c), Some(k)) = (h.as_mut(), text(coll, coll_len), text(key, key_len)) else {
        return PW_ERR_INVALID;
    };
    h.docs.doc(DocId(doc)).map_or(PW_ERR_INVALID, |mut d| done(d.map_delete(c, k)))
}

/// Adds `value` to a set (`kind` 1) or appends it to a log (2), and writes its id into `out_id` (at least 96
/// bytes, else PW_ERR_BUFFER and nothing is written): the id's length, or 0 if the doc refused it.
#[no_mangle]
pub unsafe extern "C" fn pw_doc_add(
    h: *mut PwDocs,
    doc: u32,
    kind_of: u32,
    coll: *const u8,
    coll_len: usize,
    value: *const u8,
    value_len: usize,
    out_id: *mut u8,
    cap: usize,
) -> i32 {
    if cap < 96 {
        return PW_ERR_BUFFER;
    }
    let (Some(h), Some(c), Some(v), Some(out_id)) =
        (h.as_mut(), text(coll, coll_len), text(value, value_len), writable(out_id, cap))
    else {
        return PW_ERR_INVALID;
    };
    let Some(mut d) = h.docs.doc(DocId(doc)) else { return PW_ERR_INVALID };
    let r = match kind(kind_of) {
        Some(Kind::Set) => d.set_add(c, v),
        Some(Kind::Log) => d.log_append(c, v),
        _ => return PW_ERR_INVALID,
    };
    match r {
        Ok(Some(id)) => put(id.as_bytes(), out_id, cap),
        Ok(None) => 0,
        Err(_) => PW_ERR_INVALID,
    }
}

/// Deletes item `id` of set `coll`: 1, or 0 if it held none.
#[no_mangle]
pub unsafe extern "C" fn pw_doc_set_delete(
    h: *mut PwDocs,
    doc: u32,
    coll: *const u8,
    coll_len: usize,
    id: *const u8,
    id_len: usize,
) -> i32 {
    let (Some(h), Some(c), Some(i)) = (h.as_mut(), text(coll, coll_len), text(id, id_len)) else {
        return PW_ERR_INVALID;
    };
    h.docs.doc(DocId(doc)).map_or(PW_ERR_INVALID, |mut d| done(d.set_delete(c, i)))
}

/// How many entries log `coll` keeps (1 to 10000): 0, or PW_ERR_INVALID.
#[no_mangle]
pub unsafe extern "C" fn pw_doc_log_keep(h: *mut PwDocs, doc: u32, coll: *const u8, coll_len: usize, keep: u64) -> i32 {
    let (Some(h), Some(c)) = (h.as_mut(), text(coll, coll_len)) else { return PW_ERR_INVALID };
    h.docs.doc(DocId(doc)).map_or(PW_ERR_INVALID, |mut d| d.log_keep(c, keep).map_or(PW_ERR_INVALID, |_| 0))
}

// Reads.

/// What map `coll`'s `key`, or set or log `coll`'s item `key` (an id), shows, as JSON text: its length, 0 if
/// none, or `-(needed)`.
#[no_mangle]
pub unsafe extern "C" fn pw_doc_get(
    h: *mut PwDocs,
    doc: u32,
    kind_of: u32,
    coll: *const u8,
    coll_len: usize,
    key: *const u8,
    key_len: usize,
    buf: *mut u8,
    cap: usize,
) -> i32 {
    let (Some(h), Some(k), Some(c), Some(key), Some(buf)) =
        (h.as_mut(), kind(kind_of), text(coll, coll_len), text(key, key_len), writable(buf, cap))
    else {
        return PW_ERR_INVALID;
    };
    let Some(d) = h.docs.doc(DocId(doc)) else { return PW_ERR_INVALID };
    read(d.get(k, c, key).map(|v| v.as_bytes().to_vec()), buf, cap)
}

/// A collection's entries as JSON, as doc.js's `entries()` gives them.
#[no_mangle]
pub unsafe extern "C" fn pw_doc_entries(
    h: *mut PwDocs,
    doc: u32,
    kind_of: u32,
    coll: *const u8,
    coll_len: usize,
    buf: *mut u8,
    cap: usize,
) -> i32 {
    let (Some(h), Some(k), Some(c), Some(buf)) = (h.as_mut(), kind(kind_of), text(coll, coll_len), writable(buf, cap))
    else {
        return PW_ERR_INVALID;
    };
    let Some(d) = h.docs.doc(DocId(doc)) else { return PW_ERR_INVALID };
    read(Some(d.entries_json(k, c).into_bytes()), buf, cap)
}

/// Where catching up stands, as JSON, as doc.js's `doc.sync`:
/// `{"state":…,"from":…,"have":…,"total":…,"source":…,"at":…}`. `source` is `"members"` once caught up, else
/// `null`; `at` is `null`, as no copy is kept on the relays.
#[no_mangle]
pub unsafe extern "C" fn pw_doc_status(h: *mut PwDocs, doc: u32, buf: *mut u8, cap: usize) -> i32 {
    let (Some(h), Some(buf)) = (h.as_mut(), writable(buf, cap)) else { return PW_ERR_INVALID };
    let Some(d) = h.docs.doc(DocId(doc)) else { return PW_ERR_INVALID };
    read(Some(d.sync().json().into_bytes()), buf, cap)
}

/// The doc as a copy to keep, for `pw_doc_open`'s `copy` (it holds what members wrote: keep it private).
#[no_mangle]
pub unsafe extern "C" fn pw_doc_snapshot(h: *mut PwDocs, doc: u32, buf: *mut u8, cap: usize) -> i32 {
    let (Some(h), Some(buf)) = (h.as_mut(), writable(buf, cap)) else { return PW_ERR_INVALID };
    let Some(d) = h.docs.doc(DocId(doc)) else { return PW_ERR_INVALID };
    read(Some(d.snapshot()), buf, cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PW_ERR_BUFFER, PW_ERR_INVALID};

    struct Member {
        h: *mut PwDocs,
        id: [u8; 32],
        doc: u32,
    }

    fn member(b: u8, hub: bool) -> Member {
        let (id, seed) = ([b; 32], [b ^ 0x5a; 32]);
        let h = unsafe { pw_docs_new(id.as_ptr(), seed.as_ptr()) };
        let doc = unsafe { pw_doc_open(h, 0, b"board".as_ptr(), 5, std::ptr::null(), 0, hub as u32) };
        assert!(doc >= 0);
        Member { h, id, doc: doc as u32 }
    }

    /// Delivers every message, as a host would, until none is left; a small buffer is answered with what it needs.
    fn pump(ms: &[&Member]) {
        loop {
            let mut moved = false;
            for m in ms {
                let mut small = [0u8; 8];
                let mut p = [0u8; 32];
                loop {
                    let n = unsafe { pw_docs_poll_transmit(m.h, p.as_mut_ptr(), small.as_mut_ptr(), small.len()) };
                    if n == 0 {
                        break;
                    }
                    assert!(n < -8, "{n}");
                    let mut buf = vec![0u8; (-n) as usize];
                    let n = unsafe { pw_docs_poll_transmit(m.h, p.as_mut_ptr(), buf.as_mut_ptr(), buf.len()) };
                    assert_eq!(n as usize, buf.len());
                    let to = ms.iter().find(|x| x.id == p).unwrap();
                    assert_eq!(unsafe { pw_docs_receive(to.h, 0, m.id.as_ptr(), buf.as_ptr(), buf.len()) }, 1);
                    moved = true;
                }
            }
            if !moved {
                return;
            }
        }
    }

    fn events(m: &Member) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        loop {
            let n = unsafe { pw_docs_poll_event(m.h, buf.as_mut_ptr(), 4) };
            if n == 0 {
                return out;
            }
            assert!(n < -4);
            let n = unsafe { pw_docs_poll_event(m.h, buf.as_mut_ptr(), buf.len()) };
            out.push(String::from_utf8(buf[..n as usize].to_vec()).unwrap());
        }
    }

    fn get(m: &Member, kind: u32, coll: &str, key: &str) -> Option<String> {
        let mut buf = vec![0u8; 1024];
        let n = unsafe {
            pw_doc_get(
                m.h,
                m.doc,
                kind,
                coll.as_ptr(),
                coll.len(),
                key.as_ptr(),
                key.len(),
                buf.as_mut_ptr(),
                buf.len(),
            )
        };
        assert!(n >= 0);
        (n > 0).then(|| String::from_utf8(buf[..n as usize].to_vec()).unwrap())
    }

    #[test]
    fn two_members_share_a_doc_through_the_c_abi() {
        let (a, b) = (member(0xaa, false), member(0xbb, false));
        unsafe {
            pw_docs_joined(a.h, 0, b.id.as_ptr(), 0);
            pw_docs_joined(b.h, 0, a.id.as_ptr(), 1);
        }
        pump(&[&a, &b]);
        let (c, k, v) = ("meta", "title", r#""Plans""#);
        let r = unsafe { pw_doc_map_set(a.h, a.doc, c.as_ptr(), c.len(), k.as_ptr(), k.len(), v.as_ptr(), v.len()) };
        assert_eq!(r, 1);
        let mut id = [0u8; 96];
        let (s, item) = ("strokes", "[1,2,-0]");
        let n =
            unsafe { pw_doc_add(b.h, b.doc, 1, s.as_ptr(), s.len(), item.as_ptr(), item.len(), id.as_mut_ptr(), 96) };
        assert_eq!(n, 72 + 2);
        assert_eq!(&id[72..74], b".1");
        pump(&[&a, &b]);
        assert_eq!(get(&b, 0, "meta", "title").as_deref(), Some(r#""Plans""#));
        assert_eq!(get(&a, 1, "strokes", std::str::from_utf8(&id[..74]).unwrap()).as_deref(), Some("[1,2,0]"));
        assert_eq!(get(&a, 0, "meta", "none"), None);
        let ev = events(&b);
        let change = format!(
            r#"{{"doc":0,"change":{{"name":"meta","type":"map","key":"title","value":"Plans","from":"{}","local":false}}}}"#,
            "aa".repeat(32)
        );
        assert!(ev.contains(&change), "{ev:?}");
        assert!(ev.iter().any(|e| e.contains(r#""sync":{"state":"synced""#)), "{ev:?}");
        let mut buf = vec![0u8; 4096];
        let n = unsafe { pw_doc_entries(a.h, a.doc, 1, s.as_ptr(), s.len(), buf.as_mut_ptr(), buf.len()) };
        assert!(String::from_utf8_lossy(&buf[..n as usize]).starts_with(r#"[{"id":"bbbb"#));
        let n = unsafe { pw_doc_status(a.h, a.doc, buf.as_mut_ptr(), buf.len()) };
        let synced = br#"{"state":"synced","from":null,"have":0,"total":0,"source":"members","at":null}"#;
        assert_eq!(&buf[..n as usize], synced);
        // A copy opens the doc again elsewhere.
        let n = unsafe { pw_doc_snapshot(a.h, a.doc, buf.as_mut_ptr(), buf.len()) };
        assert!(n > 0);
        let h = unsafe { pw_docs_new([0xcc; 32].as_ptr(), [1; 32].as_ptr()) };
        let d = unsafe { pw_doc_open(h, 0, b"board".as_ptr(), 5, buf.as_ptr(), n as usize, 0) };
        let c = Member { h, id: [0xcc; 32], doc: d as u32 };
        assert_eq!(get(&c, 0, "meta", "title").as_deref(), Some(r#""Plans""#));
        for m in [a, b, c] {
            unsafe { pw_docs_free(m.h) };
        }
    }

    #[test]
    fn the_c_abi_refuses_what_doc_js_throws_for() {
        let a = member(0xaa, true);
        let bad = [0xffu8, 0xfe];
        unsafe {
            let (c, k) = ("meta", "k");
            assert_eq!(
                pw_doc_map_set(a.h, a.doc, c.as_ptr(), c.len(), k.as_ptr(), 1, b"{".as_ptr(), 1),
                PW_ERR_INVALID
            );
            assert_eq!(pw_doc_map_set(a.h, a.doc, bad.as_ptr(), 2, k.as_ptr(), 1, b"1".as_ptr(), 1), PW_ERR_INVALID);
            assert_eq!(pw_doc_map_set(a.h, 9, c.as_ptr(), c.len(), k.as_ptr(), 1, b"1".as_ptr(), 1), PW_ERR_INVALID);
            assert_eq!(pw_doc_map_set(a.h, a.doc, b"".as_ptr(), 0, k.as_ptr(), 1, b"1".as_ptr(), 1), PW_ERR_INVALID);
            assert_eq!(pw_doc_map_delete(a.h, a.doc, c.as_ptr(), c.len(), k.as_ptr(), 1), 0);
            assert_eq!(pw_doc_log_keep(a.h, a.doc, c.as_ptr(), c.len(), 0), PW_ERR_INVALID);
            assert_eq!(pw_doc_log_keep(a.h, a.doc, c.as_ptr(), c.len(), 5), 0);
            let mut id = [0u8; 96];
            assert_eq!(
                pw_doc_add(a.h, a.doc, 1, c.as_ptr(), c.len(), b"1".as_ptr(), 1, id.as_mut_ptr(), 95),
                PW_ERR_BUFFER
            );
            assert_eq!(
                pw_doc_add(a.h, a.doc, 0, c.as_ptr(), c.len(), b"1".as_ptr(), 1, id.as_mut_ptr(), 96),
                PW_ERR_INVALID
            );
            let mut buf = [0u8; 15];
            assert_eq!(pw_doc_status(a.h, a.doc, buf.as_mut_ptr(), 15), PW_ERR_BUFFER);
            let mut buf = [0u8; 16];
            let alone = r#"{"state":"alone","from":null,"have":0,"total":0,"source":null,"at":null}"#;
            assert_eq!(pw_doc_status(a.h, a.doc, buf.as_mut_ptr(), 16), -(alone.len() as i32));
            assert_eq!(pw_doc_open(a.h, 0, b"x".as_ptr(), 1, b"{}".as_ptr(), 2, 0), PW_ERR_INVALID as i64);
            assert_eq!(pw_doc_open(a.h, 0, bad.as_ptr(), 2, std::ptr::null(), 0, 0), PW_ERR_INVALID as i64);
            assert_eq!(pw_doc_open(a.h, 0, b"board".as_ptr(), 5, std::ptr::null(), 0, 0), a.doc as i64, "open already");
            assert_eq!(pw_docs_receive(a.h, 0, [1u8; 32].as_ptr(), b"\x01app".as_ptr(), 4), 0);
            assert_eq!(pw_docs_poll_timeout(a.h), -1);
            assert!(pw_docs_new(std::ptr::null(), [0; 32].as_ptr()).is_null());
            pw_doc_close(a.h, a.doc);
            assert_eq!(pw_doc_status(a.h, a.doc, buf.as_mut_ptr(), 16), PW_ERR_INVALID);
            pw_docs_free(a.h);
        }
    }

    /// NB-085 (S1): a null handle, a null output, or a null input with a length over 0 is refused, as in
    /// `src/lib.rs`, and nothing is taken: the message a refused poll would have given is still first.
    #[test]
    fn null_pointers_are_refused() {
        let a = member(0xaa, false);
        let (null, nullm, nh) = (std::ptr::null::<u8>(), std::ptr::null_mut::<u8>(), std::ptr::null_mut());
        let (b, c, k, v) = ([0xbbu8; 32], "meta", "k", "1");
        let (mut p, mut buf, mut id) = ([0u8; 32], [0u8; 4096], [0u8; 96]);
        let bp = b.as_ptr();
        unsafe {
            assert!(pw_docs_new(bp, null).is_null());
            assert_eq!(pw_doc_open(nh, 0, b"x".as_ptr(), 1, null, 0, 0), PW_ERR_INVALID as i64);
            assert_eq!(pw_doc_open(a.h, 0, null, 1, null, 0, 0), PW_ERR_INVALID as i64);
            assert_eq!(pw_doc_open(a.h, 0, b"x".as_ptr(), 1, null, 5, 0), PW_ERR_INVALID as i64);
            pw_doc_close(nh, a.doc);
            pw_docs_joined(nh, 0, bp, 0);
            pw_docs_left(nh, 0, bp);
            pw_docs_handle_timeout(nh, 0);
            assert_eq!(pw_docs_poll_timeout(nh), -1);
            assert_eq!(pw_docs_queued(nh, bp), PW_ERR_INVALID as i64);
            assert_eq!(pw_docs_receive(nh, 0, bp, b"\x01app".as_ptr(), 4), PW_ERR_INVALID);
            assert_eq!(pw_docs_receive(a.h, 0, bp, null, 4), PW_ERR_INVALID);
            pw_docs_retry(nh, bp, b"x".as_ptr(), 1);
            pw_docs_retry(a.h, bp, null, 1);
            assert_eq!(pw_docs_poll_event(nh, buf.as_mut_ptr(), buf.len()), PW_ERR_INVALID);
            assert_eq!(pw_docs_poll_event(a.h, nullm, buf.len()), PW_ERR_INVALID);
            // b joins: a asks it for the doc.
            pw_docs_joined(a.h, 0, bp, 0);
            assert_eq!(pw_docs_poll_transmit(nh, p.as_mut_ptr(), buf.as_mut_ptr(), buf.len()), PW_ERR_INVALID);
            assert_eq!(pw_docs_poll_transmit(a.h, nullm, buf.as_mut_ptr(), buf.len()), PW_ERR_INVALID);
            assert_eq!(pw_docs_poll_transmit(a.h, p.as_mut_ptr(), nullm, buf.len()), PW_ERR_INVALID);
            let need = pw_docs_poll_transmit(a.h, p.as_mut_ptr(), nullm, 0);
            assert!(need < -5, "room for nothing: {need}");
            assert_eq!(pw_docs_poll_transmit(a.h, p.as_mut_ptr(), buf.as_mut_ptr(), buf.len()), -need, "still first");
            assert_eq!(p, b);
            // Writes and reads.
            let (cp, kp, vp) = (c.as_ptr(), k.as_ptr(), v.as_ptr());
            assert_eq!(pw_doc_map_set(nh, a.doc, cp, 4, kp, 1, vp, 1), PW_ERR_INVALID);
            assert_eq!(pw_doc_map_set(a.h, a.doc, null, 4, kp, 1, vp, 1), PW_ERR_INVALID);
            assert_eq!(pw_doc_map_set(a.h, a.doc, cp, 4, kp, 1, null, 1), PW_ERR_INVALID);
            assert_eq!(pw_doc_map_delete(nh, a.doc, cp, 4, kp, 1), PW_ERR_INVALID);
            assert_eq!(pw_doc_map_delete(a.h, a.doc, cp, 4, null, 1), PW_ERR_INVALID);
            assert_eq!(pw_doc_add(nh, a.doc, 1, cp, 4, vp, 1, id.as_mut_ptr(), 96), PW_ERR_INVALID);
            assert_eq!(pw_doc_add(a.h, a.doc, 1, cp, 4, vp, 1, nullm, 96), PW_ERR_INVALID);
            assert_eq!(pw_doc_set_delete(nh, a.doc, cp, 4, kp, 1), PW_ERR_INVALID);
            assert_eq!(pw_doc_set_delete(a.h, a.doc, cp, 4, null, 1), PW_ERR_INVALID);
            assert_eq!(pw_doc_log_keep(nh, a.doc, cp, 4, 5), PW_ERR_INVALID);
            assert_eq!(pw_doc_log_keep(a.h, a.doc, null, 4, 5), PW_ERR_INVALID);
            let n = buf.len();
            assert_eq!(pw_doc_get(nh, a.doc, 0, cp, 4, kp, 1, buf.as_mut_ptr(), n), PW_ERR_INVALID);
            assert_eq!(pw_doc_get(a.h, a.doc, 0, cp, 4, kp, 1, nullm, n), PW_ERR_INVALID);
            assert_eq!(pw_doc_entries(nh, a.doc, 0, cp, 4, buf.as_mut_ptr(), n), PW_ERR_INVALID);
            assert_eq!(pw_doc_entries(a.h, a.doc, 0, cp, 4, nullm, n), PW_ERR_INVALID);
            assert_eq!(pw_doc_status(nh, a.doc, buf.as_mut_ptr(), n), PW_ERR_INVALID);
            assert_eq!(pw_doc_status(a.h, a.doc, nullm, n), PW_ERR_INVALID);
            assert_eq!(pw_doc_snapshot(nh, a.doc, buf.as_mut_ptr(), n), PW_ERR_INVALID);
            assert_eq!(pw_doc_snapshot(a.h, a.doc, nullm, n), PW_ERR_INVALID);
            assert_eq!(get(&a, 0, c, k), None, "nothing was written");
            assert_eq!(pw_doc_map_set(a.h, a.doc, cp, 4, kp, 1, vp, 1), 1);
            assert_eq!(get(&a, 0, c, k).as_deref(), Some("1"));
            pw_docs_free(nh);
            pw_docs_free(a.h);
        }
    }
}
