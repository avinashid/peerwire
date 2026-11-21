# Share protocol

Peerwire Share sends files of any size from one room member to another: the receiver accepts or declines each offer,
and the file goes in pieces, each checked with SHA-256, with the whole file checked again at the end, written to disk
as it arrives and never held whole. Two programs speak it:

- the [Share page](../../packages/js/demo/share/index.html) in a browser: `files.js` has the frames and checks,
  `share.js` the transfers;
- Rust apps, through `peerwire::files` (feature `files`) and the
  [`share` example](../../examples/rust/share.rs) on it ([Rust reference](../reference/rust.md#files)).

This page is the protocol both follow: the frames, the limits, the messages and what each side does with what the
other sends. `files.js` is its source of truth; the Rust module is checked against vectors it makes
(`packages/js/test/rust_files.mjs`).

> [!NOTE]
> A browser and a native node can't link yet: a joinRoom room doesn't dial a native peer, which has no WebRTC
> ([NAT traversal](nat-traversal.md#messages)). So today browsers share with browsers and native apps with native
> apps. The two speak the same protocol all the same: `share_native.mjs` runs the page against the Rust example over
> a stand-in room and checks files both ways, byte for byte.

## Messages on a link

Every Share message is a room message on the **reliable lane**, in bytes: in a JS room (`room.send` of a
`Uint8Array`) and in Rust alike it goes as room.js frames bytes, the tag `0`, then the Share frame. Rust hands
`Files` every reliable message, and it takes those that are `0` followed by a piece or a control frame
(`files::share_frame`). Messages on the other lanes are dropped, whatever they hold.

```text
room message   0x00 (bytes) | Share frame

Share frame    PIECE    0x01 | transfer id (16) | index (u32, big-endian) | SHA-256 of the data (32) | data
               CONTROL  0x02 | JSON (UTF-8), at most 4,096 bytes
```

A piece frame's header is 53 bytes. A transfer id is 16 random bytes the sender picks, written as 32 lowercase hex
digits in control messages; a receiver keys transfers by the sender and the id, as two senders may pick the same one.

### Where data may go

Pieces, and the `end` that follows them, go only while the link to the peer is **direct**: in a browser, the
peer's own WebRTC DataChannel, not through TURN and not a channel adopted from a birthday punch (`directForFiles`);
in Rust, a link whose `Node::path_kind` is `Direct` or `Birthday` (`impl Wire for Node`). Never through another
member, a relay peer, TURN or the signalling relays. While the link is anything else, transfers wait. Control
messages go over whatever link there is. This is checked as each piece is handed to the link: in Rust, if the link's
path moves while pieces are in flight, its reliable lane may resend those (at most `WINDOW`, and an `end`) over the
new path, a relay, TURN or a pipe, still sealed end to end.

## Limits

| Name | Value | |
|---|---|---|
| `MAX_FRAME` | 1,048,576 | The room's largest reliable message, its tag byte included (`Config::max_message`) |
| `HEADER` | 53 | A piece frame's bytes before its data |
| `PIECE` | 524,288 (512 KiB) | The piece size a sender offers, the page and Rust alike |
| `MIN_PIECE` | 262,144 (256 KiB) | The smallest piece size a receiver takes |
| `MAX_PIECE` | 1,048,522 | The largest: `MAX_FRAME - 1 - HEADER`, so a piece and the room's tag fill the lane's largest message |
| `WINDOW` | 8 | Pieces a sender has in flight to one peer at most (sent, not acknowledged), over all its transfers to it: 4 MiB at `PIECE` |
| `MIN_WINDOW`, `MAX_WINDOW` | 1 MiB, 4 MiB | The page's window in bytes, within `WINDOW` pieces: see [the sender](#a-transfer) |
| `MAX_QUEUED` | 16 | Pieces a receiver holds taken but not yet written, over one peer's transfers; more means the sender ignores acks |
| `MAX_SIZE` | 2^40 (1 TiB) | The largest file |
| `MAX_CONTROL` | 4,096 | Bytes of a control message's JSON; a longer one is never parsed |
| `MAX_NAME` | 120 | Characters (code points) of a file name, once cleaned |
| `MAX_RAW_NAME` | 1,024 | UTF-16 code units of a name as sent; a longer one is no offer |
| `MAX_TYPE` | 100 | UTF-16 code units of a MIME type; a longer one shows as none |
| `MAX_OFFERS` | 32 | Offers from one peer waiting for an answer; past that, more are declined |
| `MAX_REJECTS` | 8 | Pieces refused in one transfer before the receiver cancels it |
| `RESUME` | 10 minutes | How long a transfer waits for its peer to come back after its link dropped |

## Control messages

A control frame's JSON is read as `JSON.parse(new TextDecoder().decode(bytes))` reads it: a leading BOM dropped,
each invalid UTF-8 sequence as U+FFFD, strings as UTF-16 (a `\ud800` escape is a lone surrogate), numbers as
doubles (`1e400` is Infinity, `1.0` is 1, `-0` is 0), any depth, and a key given twice keeps its last value. A frame
over `MAX_CONTROL` bytes, one that isn't JSON, and one whose value isn't an object (an array, `null`, a string)
with a string `t` is dropped. Fields nobody reads are ignored. Rust reads it the same way (`files/json.rs`).

A "whole number" below is what `Number.isSafeInteger` takes; files.js `intIn(v, lo, hi)` is one from `lo` to `hi`.

| `t` | From | Fields | Sent when |
|---|---|---|---|
| `offer` | sender | `v: 1`, `id`, `name`, `size`, `type`, `piece`, `count` | The app offers a file; again, unchanged, each time the receiver links again while it is unanswered |
| `accept` | receiver | `id`, `have` | The user accepts (`have: 0`); again on every new link while receiving, with `have` the pieces written |
| `decline` | receiver | `id` | The user declines, or the offer is past `MAX_OFFERS` |
| `ack` | receiver | `id`, `have` | A piece is checked and written: `have` is how many are |
| `end` | sender | `id`, `sha256` | Every piece has been sent: the whole file's SHA-256 in 64 lowercase hex digits |
| `done` | receiver | `id`, `ok`, `sha256` | The whole file is in and checked: `ok` whether its SHA-256 matched `end`'s (and the file could be kept), and the one it got |
| `resume` | sender | `id` | The receiver links again while a transfer is accepted: where is it? |
| `cancel` | either | `id` | Either side ends a live transfer: the user, a failed check, a failed read or write |

Each side writes its messages with the fields in the order above, as `JSON.stringify` writes them, so the two make
the same bytes: `{"t":"ack","id":"0123456789abcdef0123456789abcdef","have":3}`.

Every message but an offer needs an `id` of exactly 32 lowercase hex digits, and names a transfer between the two
peers: the sender's own (`accept`, `decline`, `ack`, `done`, `cancel`) or the one it receives from that peer
(`end`, `resume`, `cancel`). Anything else is dropped.

### Offers

```json
{"t":"offer","v":1,"id":"<32 hex>","name":"holiday.mp4","size":5368709121,"type":"video/mp4","piece":524288,"count":10241}
```

A receiver takes an offer (files.js `parseOffer`) only if:

- `id` is 32 lowercase hex digits;
- `size` is a whole number from 0 to `MAX_SIZE`;
- `piece` is a whole number from `MIN_PIECE` to `MAX_PIECE`;
- `count`, if there is one, is `ceil(size / piece)` (0 for an empty file);
- `name` is a string of at most `MAX_RAW_NAME` UTF-16 code units;
- `type` is absent or a string (`null` is neither).

`v` isn't read. The name is then cleaned and the type checked (below), and the offer waits for the user. An offer
whose id the receiver already has from that peer (sent again on a new link) is dropped; one past `MAX_OFFERS`
waiting from that peer is declined.

### Names and types

A file name is anyone's, so it is cleaned before it is shown or saved under (files.js `cleanName`, in this order):

1. cut to its first `MAX_RAW_NAME` UTF-16 code units, then every lone surrogate dropped (a pair the cut split
   leaves one);
2. NFC;
3. control characters (Cc), format characters (Cf: bidi overrides, zero-width ones, soft hyphens) and the line and
   paragraph separators (U+2028, U+2029) dropped;
4. each of `\ / : * ? " < > |` replaced with `_`;
5. each run of whitespace (JavaScript's `\s`) made one space;
6. leading dots and whitespace dropped (no hidden file, no `..`), then trailing ones;
7. a Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM0`-`COM9`, `LPT0`-`LPT9`, and `COM`/`LPT` with `¹²³`, in
   any ASCII case), alone or before a dot, gets a `_` in front;
8. past `MAX_NAME` code points, cut to fit: an extension (from the last dot, if that dot isn't the first character
   and the extension, dot included, is at most 16 UTF-16 units) is kept whole, and the part before it cut and
   stripped of trailing dots and whitespace;
9. `file` if nothing is left.

So `../../etc/passwd` is `_.._etc_passwd`, `evil‮txt.exe` is `eviltxt.exe`, and `café.txt` is `café.txt`.
A sender cleans its own name before it offers it, and the receiver cleans it again: cleaning a clean name changes
nothing. Rust takes NFC and Cf from tables `scripts/unicode-tables.mjs` makes from Node's own Unicode data (17.0
today), so a name cleans the same on both.

A MIME type (files.js `cleanType`) is kept only if it is at most `MAX_TYPE` code units and a plain `type/subtype`,
`^[a-z0-9][\w.+-]*\/[\w.+-]+$` in any ASCII case, then lower-cased; anything else is `''`. It is shown, never used to
open or save anything.

## A transfer

```text
sender                                           receiver
  offer {id, name, size, type, piece, count}  ->
                                              <-   accept {id, have: 0}        (or decline)
  piece 0, piece 1, ... piece 7               ->   each checked, written, then
                                              <-   ack {id, have: 1}, ack {have: 2}, ...
  piece 8 (once ack have: 1 is in) ...        ->
  end {id, sha256}  (after the last piece)    ->   once every piece is written:
                                              <-   done {id, ok, sha256}
```

**The sender** reads each piece from the file only when it may send it, so it holds one piece at a time:

- an `accept` with `have` exactly 0 (`-0` and `0.0` are 0) starts an offered transfer; any other is ignored;
- pieces go in order while the link is direct and the peer has fewer than `WINDOW` pieces in flight, over all the
  transfers to it (the oldest accepted transfer first), each with its own SHA-256. The page also keeps the bytes in
  flight under a window of twice the rate its acks come at times the link's round trip, from `MIN_WINDOW` to
  `MAX_WINDOW` (files.js `windowBytes`): pieces queue on the reliable lane ahead of anything else sent to the peer,
  a chat message or an `ack`, so it sends no more than keeps the link busy. A chat message sent during a transfer
  waits behind up to a window: **theoretical** window / rate, 0.1 s for 1 MiB at 10 MB/s, against 0.4 s for 4 MiB.
  Rust keeps `WINDOW` pieces;
- each piece goes into the whole file's SHA-256 once, the first time it is read, however often a resume sends it
  again;
- after the last piece, `end` with the whole file's SHA-256 (an empty file: `end` at once);
- an `ack` counts only if its `have` is a whole number from the last one to the pieces sent;
- a `done` with `ok: true` and the sender's own SHA-256 ends the transfer as delivered; any other `done` fails it,
  once every piece is acknowledged;
- a `decline` ends an offered transfer, a `cancel` any live one.

**The receiver** takes pieces only for a transfer it accepted, and only the next one:

- a piece is refused if it is past the file's last (`out-of-range`), one it has (`duplicate`), one past the next
  (`gap`), or not that piece's length (`bad-length`: every piece is `piece` bytes but the last, `size` less the
  others). Pieces for a transfer not accepted are dropped. Past `MAX_REJECTS` refused in a transfer, it cancels it;
- a piece whose data doesn't match its SHA-256 fails the transfer (and the receiver says `cancel`);
- each piece is written before its `ack`, and goes into the whole file's SHA-256 as it is written;
- past `MAX_QUEUED` pieces taken and not yet written (from one peer), the sender ignores flow control: cancel. The
  page writes asynchronously, so pieces can wait there; Rust writes each piece as it comes, so it never holds one;
- an `end` whose `sha256` isn't 64 lowercase hex digits cancels the transfer; a valid one is kept, and once every
  piece is written the receiver compares: on a match it keeps the file and says `done` with `ok: true`; on a
  mismatch, or if the file can't be kept, it drops it and says `ok: false`;
- a file is kept only once its whole SHA-256 matches: the page's save picker writes a temporary file until then, its
  download fails, and Rust's `FileSink` writes `.peerwire-<random>.part` and only then moves it to its cleaned name,
  never over an existing file.

### When a link drops

A link that drops doesn't end a transfer. Each side waits up to `RESUME` for the peer to come back, then fails it.
A peer that leaves the room on purpose (a page closed; in Rust, a link the peer closed, `ClosedByPeer`) ends its
transfers at once: it comes back, if at all, as a new peer.

When the peer is back (on any link):

- the sender offers again what is unanswered, and asks `resume` for what is accepted;
- the receiver says `accept` again with `have`, the pieces it has written (after any it was still writing);
- the sender goes on from piece `have` (files.js `resumeAt`) if that is a whole number from 0 to the pieces it has
  read, else fails the transfer and says `cancel`; it sends `end` again after the last piece, as the first may have
  been lost;
- a receiver asked `resume` for a transfer it has delivered says its `done` again (its acks may have been lost); for
  one it no longer has, or that ended any other way, `cancel`.

## What a receiver holds

Per peer: up to `MAX_OFFERS` offers waiting (a name of at most 120 characters each), and at most `MAX_QUEUED` pieces
not yet written, so about 8 MiB; per transfer one hash state. A sender holds one piece. Finished transfers are
forgotten past 200 (the page's rows, Rust's `MAX_KEPT`), oldest first (in Rust, once the app has polled their
`Ended`). Rust also caps the control messages waiting for a peer whose link takes none (`MAX_OUTBOX`, 256), so a
peer that stops reading can't grow them. Nothing caps the transfers a peer has under way once accepted, each with an
open file: a Rust app that accepts without asking caps them per peer (the `share` example's `--yes`, 32).

## Testing it

- `node packages/js/test/share.mjs`: files.js alone: framing, hashes, offers, names, `checkPiece`, `resumeAt`,
  `windowBytes`.
- `node packages/js/test/share_browser.mjs`: the page in headless Chromium, tabs sharing files, hostile peers.
- `node packages/js/test/rust_files.mjs`: the vectors files.js makes (names, types, control frames valid and
  mutated, piece checks, resumes, the frames each side writes, the streaming hash), which
  `cargo test -p peerwire --features files files::` checks Rust against. `--write` writes them after a change to
  files.js.
- `node packages/js/test/share_native.mjs`: the page against the Rust `share` example, both ways, over a stand-in
  room; `--mb=1024` for a large file.

Measured on the test host (4 aarch64 cores, loopback, 20 November 2026), 1 GiB of random bytes each way, checked
byte for byte:

| Direction | How | Speed |
|---|---|---|
| native to native | two `share` examples (release build) in one room over a local Nostr relay, on a direct UDP link | 32.4 MB/s from the accept to the verdict, about 40 MB/s once under way |
| native to browser | `share_native.mjs --mb=1024`: the example to the page, which downloads it through its service worker | 18.4 MB/s |
| browser to native | the same test, the page to the example | 17.3 MB/s |

The two browser rows run over the test's stand-in room (a WebSocket and a TCP connection on loopback), not a
Peerwire link, so they measure what the two ends do (read, hash, write, the window and its acks), not a network. The
example's resident memory peaked at 11 MB in every run: it holds a piece, not the file.
