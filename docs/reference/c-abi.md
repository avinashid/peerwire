# C ABI

`crates/peerwire-ffi` puts the `peerwire-core` endpoint behind plain C functions named `pw_*`. The same functions are
the browser's WebAssembly module, which has no imports, and a native shared or static library. Like the core, it
does no I/O: the host passes in time and datagrams, and sends the datagrams that come out.

| | |
|---|---|
| Crate | `crates/peerwire-ffi`: `src/lib.rs` (the endpoint) and `src/sync.rs` (shared documents) |
| Builds | `cdylib`, `staticlib` and `rlib`; `packages/js/pkg/peerwire_ffi.wasm` for `wasm32-unknown-unknown` |
| Functions | 27 in every build, and 22 more for shared documents with the `sync` feature |
| Header | none in the repo. The declarations on this page are translated from the Rust signatures |
| Status | early development (v0.1). The ABI may still change |

The crate's own notes are in [`crates/peerwire-ffi/README.md`](../../crates/peerwire-ffi/README.md); each function's
full contract is its doc comment in `src/`.

## Build

```sh
./packages/js/build.sh                                  # -> packages/js/pkg/peerwire_ffi.wasm
cargo build --release -p peerwire-ffi                   # -> target/release/libpeerwire_ffi.so (.dylib on macOS), .a
cargo build --release -p peerwire-ffi --features sync   # the same, with the shared-document functions
```

From `crates/peerwire-ffi/Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib", "staticlib", "rlib"]

[features]
sync = ["dep:peerwire-sync"]
```

`sync` is off by default, so the wasm build and a plain `cargo build` leave out `pw_docs_*` and `pw_doc_*`. The gate
and CI build, lint and test them on their own (`cargo test -p peerwire-ffi --features sync`).

`packages/js/build.sh` builds for `wasm32-unknown-unknown` into `target/web` with `opt-level=s`, LTO, one codegen
unit, stripped symbols and `panic=abort`, then copies the module to `packages/js/pkg/`. `scripts/gate.sh` and CI
(`.github/scripts/wasm-check.mjs`) fail if the module is over `WASM_MAX_BYTES` (358,400 bytes, 350 KiB) or has any
import. On `main` it is 356,584 bytes with no imports, as the gate's `wasm size+imports` step reports it after
`packages/js/build.sh`:

```text
packages/js/pkg/peerwire_ffi.wasm: 356584 bytes (limit 358400), 0 imports
```

## Conventions

- **Handles.** `pw_endpoint_new` and `pw_docs_new` return an opaque pointer (`PwEndpoint *`, `PwDocs *`), or null
  when a required argument is null. Free each once with `pw_endpoint_free` or `pw_docs_free`, which ignore null.
  Every other function refuses a null handle (see below); a freed handle is undefined behaviour.
- **Time.** `now_us` is microseconds on the host's monotonic clock, as a `uint64_t`. The deadlines from
  `pw_poll_timeout` and `pw_docs_poll_timeout` are instants on that clock, not delays.
- **Addresses** are `uint32_t` handles. The host picks them and maps each to its own transport (a socket address, a
  WebRTC DataChannel, a signalling pipe); the endpoint gives them back in `pw_poll_transmit` and in events. Pass
  `share_addresses` 0 to `pw_endpoint_new` when handles are local numbers rather than network addresses: the
  endpoint then doesn't tell peers the addresses it sees them at.
- **Peer ids** are 32-byte buffers (Ed25519 public keys).
- **Null pointers.** A null handle, a null peer id, a null output, or a null input whose length isn't 0 gets
  `PW_ERR_INVALID` from the functions that return a code, is ignored by the functions that return nothing, gives 0
  from `pw_is_connected` and -1 from `pw_rtt_us`, `pw_path_rtt_us`, `pw_poll_timeout` and `pw_docs_poll_timeout`.
  Nothing is read, written or taken from a queue. The `_new` functions give null.
- **Fixed-size inputs** (peer ids, secrets, seeds, Nostr messages) take no length: exactly 32 bytes are read. A
  shorter buffer is read past its end.
- **Byte inputs** are a pointer and a length. With length 0 the pointer isn't read and may be null, and so may an
  output whose capacity is 0. Strings, in the shared-document functions, are UTF-8 and not NUL-terminated.
- **Outputs** go into buffers the caller owns (see the table below). The library never keeps a pointer it was given.
- **Memory.** `pw_alloc(len)` returns `len` bytes (at least 1) from the library's allocator, and `pw_free(p, len)`
  frees them; it must get the same `len`. A wasm host has to use them, as the module can only reach its own memory.
  A native host can pass any memory it owns, on the stack or from `malloc`. Never `free()` a `pw_alloc` buffer, or
  `pw_free` one from `malloc`.
- **Threads.** The source promises nothing about threads and takes no locks: use each handle from one thread at a
  time.
- **Settings.** `share_addresses` is the only endpoint setting the ABI exposes. Everything else is the core's
  `Config::default()`.

### Return values

| Type | Functions | Values |
|---|---|---|
| `int32_t` | most | `>= 0` success (a length or a count where one is returned); `< 0` a `PW_ERR_*` code or `-(needed)` |
| `int64_t` | `pw_poll_timeout`, `pw_docs_poll_timeout` | the next deadline in µs, or -1 if none |
| `int64_t` | `pw_rtt_us`, `pw_path_rtt_us` | µs, or -1 if unknown |
| `int64_t` | `pw_doc_open`, `pw_docs_queued` | the doc id or a byte count, or `PW_ERR_INVALID` |
| `uint32_t` | `pw_is_connected` | 1 or 0 |
| pointer | `pw_alloc`, `pw_endpoint_new`, `pw_docs_new` | the buffer or handle; the `_new` functions give null for a null required argument |
| `void` | the rest | nothing |

### Output buffers

| Output | Size |
|---|---|
| `out_peer` of `pw_endpoint_new` and `pw_docs_poll_transmit`, `out` of `pw_secret`, `out_from` of `pw_open` | 32 bytes |
| `out_secret` and `out_pub` of `pw_nostr_keypair` | 32 bytes each |
| `out_sig` of `pw_nostr_sign` | 64 bytes |
| `out` of `pw_stats` | 16 `uint64_t` (128 bytes), at any alignment |
| `out_addr` of `pw_poll_transmit` | one `uint32_t`, at any alignment |
| `buf` of `pw_poll_transmit` | at least 1232 bytes (`MAX_DATAGRAM`), else `PW_ERR_BUFFER` |
| `out` of `pw_open` | at least `len`, the sealed message's length, else `PW_ERR_BUFFER` |
| `out_id` of `pw_doc_add` | at least 96 bytes, else `PW_ERR_BUFFER` |
| `buf` of `pw_doc_get`, `pw_doc_entries`, `pw_doc_status`, `pw_doc_snapshot` | at least 16 bytes, else `PW_ERR_BUFFER`; then `-(needed)` if the result doesn't fit |
| `buf` of `pw_poll_event`, `pw_docs_poll_event`, `pw_docs_poll_transmit`; `out` of `pw_seal` | any: `-(needed)` if the result doesn't fit |

A buffer that is too small is answered in one of two ways, and neither loses anything:

- **`PW_ERR_BUFFER`** where the function has a fixed minimum. Nothing was consumed.
- **`-(needed)`** where the result's size varies: minus the bytes it needs. `pw_poll_event`, `pw_docs_poll_event`
  and `pw_docs_poll_transmit` keep the item first in line; call again with at least that many bytes. These values
  never read as a `PW_ERR_*` code: those three functions return no other code than `PW_ERR_INVALID`, for a null
  pointer, and what they give is always over 5 bytes (an event at least 33, a doc message a tag and a JSON object);
  `pw_seal` needs at least 128 bytes, and the doc reads ask for 16 bytes up front.

## Error codes

Defined in `src/lib.rs` as `pub const PW_ERR_*: i32`:

| Code | Value | Meaning |
|---|---|---|
| `PW_ERR_NOT_CONNECTED` | -1 | `pw_send`: there is no link to that peer (not linked yet, or not any more) |
| `PW_ERR_TOO_LARGE` | -2 | `pw_send`: over the lane's limit (see [Sending](#sending)). `pw_seal`: over 65,407 bytes of plaintext |
| `PW_ERR_WOULD_BLOCK` | -3 | `pw_send` on the reliable or urgent lane: the send buffer (4 MiB) is full. Send again once the peer has acknowledged more, after more `pw_handle_datagram` calls |
| `PW_ERR_INVALID` | -4 | An argument the function can't use: a null peer id; this endpoint's own id (`pw_connect`, `pw_connect_via`); an id that isn't a valid key (those two and `pw_seal`); a sealed message not for this endpoint or changed on the way (`pw_open`); a Nostr seed or secret that isn't a valid key; an unknown doc, or an argument doc.js would throw for (see [Shared documents](#shared-documents)) |
| `PW_ERR_BUFFER` | -5 | The output buffer is under the function's fixed minimum; nothing was consumed |

They are Rust constants, not symbols in the library. With no header, define them in C:

```c
#define PW_ERR_NOT_CONNECTED (-1)
#define PW_ERR_TOO_LARGE     (-2)
#define PW_ERR_WOULD_BLOCK   (-3)
#define PW_ERR_INVALID       (-4)
#define PW_ERR_BUFFER        (-5)
```

## Functions

Every declaration below assumes:

```c
#include <stddef.h>
#include <stdint.h>

typedef struct PwEndpoint PwEndpoint;   /* opaque */
typedef struct PwDocs PwDocs;           /* opaque; the sync feature */
```

### Memory

```c
uint8_t *pw_alloc(size_t len);
void pw_free(uint8_t *p, size_t len);
```

| Function | What it does | Returns |
|---|---|---|
| `pw_alloc` | Allocates `len` bytes (1 if `len` is 0) in the library's memory. They are not zeroed | the pointer |
| `pw_free` | Frees a `pw_alloc` buffer. `len` must be the length it was allocated with; null is ignored | nothing |

### Endpoint

```c
PwEndpoint *pw_endpoint_new(const uint8_t *secret, const uint8_t *seed, uint32_t share_addresses, uint8_t *out_peer);
void pw_endpoint_free(PwEndpoint *h);
int32_t pw_secret(const PwEndpoint *h, uint8_t *out);
```

| Function | What it does | Returns |
|---|---|---|
| `pw_endpoint_new` | Creates an endpoint. `secret` is a 32-byte Ed25519 secret key that restores an identity, or null for a new identity drawn from `seed`. `seed` is 32 bytes from a CSPRNG and is always required: it also seeds the endpoint's randomness. `share_addresses`: see [Conventions](#conventions). Writes the 32-byte peer id to `out_peer` | the handle, or null if `seed` or `out_peer` is null |
| `pw_endpoint_free` | Frees the endpoint. Null is ignored | nothing |
| `pw_secret` | Copies the 32-byte secret key to `out`, so the host can keep the identity and pass it back to `pw_endpoint_new` | 0; `PW_ERR_INVALID`, writing nothing, for a null `h` or `out` |

### Links

```c
int32_t pw_connect(PwEndpoint *h, uint64_t now_us, uint32_t addr, const uint8_t *peer_id);
int32_t pw_connect_via(PwEndpoint *h, uint64_t now_us, const uint8_t *relay, const uint8_t *peer_id);
int32_t pw_add_candidate(PwEndpoint *h, uint64_t now_us, uint32_t addr, const uint8_t *peer_id);
void pw_set_slow(PwEndpoint *h, const uint8_t *peer_id, uint32_t addr, uint32_t on);
int32_t pw_add_relay(PwEndpoint *h, uint64_t now_us, const uint8_t *relay, const uint8_t *peer_id);
void pw_set_relay_only(PwEndpoint *h, uint32_t on);
void pw_disconnect(PwEndpoint *h, const uint8_t *peer_id);
```

| Function | What it does | Returns |
|---|---|---|
| `pw_connect` | Dials `peer_id` at handle `addr`. A no-op if already linked. A dial already running takes `addr` as one more path, except while relay-only if the dial has a relay. The outcome comes later as a CONNECTED or CONNECT_FAILED event | 0; `PW_ERR_INVALID` for a null peer id, this endpoint's own id, or an id that isn't a valid key |
| `pw_connect_via` | Dials `peer_id` through `relay`, a connected peer | 0; `PW_ERR_INVALID` as for `pw_connect`, or for a null `relay` |
| `pw_add_candidate` | Gives a relayed or dialling peer a direct address: the endpoint punches to `addr` and moves the link there once the peer answers from it. Does nothing for a peer linked directly or unknown, or, while relay-only, for one linked or being dialled through a relay | 0; `PW_ERR_INVALID` for a null peer id |
| `pw_set_slow` | Marks `addr` as a slow address of the linked `peer_id` (a signalling pipe, say), or clears the mark (`on` 0). On it, the link takes a relay offered with `pw_add_relay` once a probe through it is answered, and goes back to `addr` when that relay goes | nothing |
| `pw_add_relay` | Offers `relay`, a peer linked directly, to carry the link to `peer_id`. While that link is on its slow address, probes go through `relay` and the link moves there (PATH_CHANGED) once `peer_id` answers. While it is on another relay, the two are raced and `relay` is taken if clearly faster (see `pw_path_rtt_us`). Ignored while relay-only | 0; `PW_ERR_INVALID` if either id is null |
| `pw_set_relay_only` | `on` != 0 turns relay-only on: relayed links stay relayed (no punching, no direct path), dials through a relay send nothing direct, relays are asked not to tag this endpoint's packets with its address, and a direct handshake is answered only from a peer it is dialling directly or already has a direct link with. A direct dial later given a relay keeps its direct addresses until the next call with `on` != 0: each such call sends every dial that has a relay through it alone | nothing |
| `pw_disconnect` | Closes the link to `peer_id` and tells the peer so. No event is emitted for that peer | nothing |

There is no invite function: invites (`pw:`) and rooms are built above the ABI, in `peerwire.js` and `room.js` for
the browser and in the Rust `peerwire` crate.

### Link state

```c
uint32_t pw_is_connected(const PwEndpoint *h, const uint8_t *peer_id);
int64_t pw_rtt_us(const PwEndpoint *h, const uint8_t *peer_id);
int64_t pw_path_rtt_us(const PwEndpoint *h, const uint8_t *peer_id);
int32_t pw_stats(const PwEndpoint *h, uint64_t *out);
```

| Function | What it does | Returns |
|---|---|---|
| `pw_is_connected` | Whether `peer_id` is linked | 1, or 0 (also for a null peer id) |
| `pw_rtt_us` | The link's smoothed round-trip time | µs, or -1 if unknown |
| `pw_path_rtt_us` | The round-trip time over the path the link is on now, measured since it moved there. Links race other paths of their kind and take one clearly faster | µs, or -1 if unknown |
| `pw_stats` | Writes 16 counters to `out`, in this order: `tx_packets`, `rx_packets`, `rx_malformed`, `rx_unknown_session`, `rx_auth_failed`, `rx_replayed`, `rx_handshake_dropped`, `retransmits`, `unreliable_dropped`, `roams`, `relayed`, `relay_dropped`, `punches_sent`, `punch_succeeded`, `punch_failed`, `direct_fallbacks` | 0; `PW_ERR_INVALID`, writing nothing, for a null `h` or `out` |

The core counts more (`spray_probes`, `rx_wrong_peer`, `path_probes`, `path_switches`, `cookies_sent`); the ABI
doesn't give those.

### Sending

```c
int32_t pw_send(PwEndpoint *h, const uint8_t *peer_id, uint32_t lane, const uint8_t *data, size_t len);
```

Queues `len` bytes for `peer_id` on a lane. They go out through `pw_poll_transmit`.

| `lane` | Lane | Largest message |
|---|---|---|
| 0 | reliable: ordered, exactly once | 1 MiB |
| 1 | unreliable: at most once. Past 256 queued messages the oldest is dropped (`unreliable_dropped`) | 1100 bytes |
| 2 | urgent: reliable, each fragment sent twice, ahead of queued reliable messages; it arrives as reliable | 64 KiB |

Any other `lane` value is the unreliable lane. Returns 0, or `PW_ERR_INVALID` (null peer id),
`PW_ERR_NOT_CONNECTED`, `PW_ERR_TOO_LARGE` or `PW_ERR_WOULD_BLOCK`. The lanes are described in
[Sending messages](../guides/messaging.md).

### Host loop

```c
void pw_handle_datagram(PwEndpoint *h, uint64_t now_us, uint32_t addr, const uint8_t *data, size_t len);
int32_t pw_handle_datagram_from(PwEndpoint *h, uint64_t now_us, uint32_t addr, const uint8_t *peer_id,
                                const uint8_t *data, size_t len);
void pw_handle_timeout(PwEndpoint *h, uint64_t now_us);
int64_t pw_poll_timeout(const PwEndpoint *h);
int32_t pw_poll_transmit(PwEndpoint *h, uint64_t now_us, uint32_t *out_addr, uint8_t *buf, size_t cap);
int32_t pw_poll_event(PwEndpoint *h, uint8_t *buf, size_t cap);
```

| Function | What it does | Returns |
|---|---|---|
| `pw_handle_datagram` | Feeds in a datagram that arrived on handle `addr`. Use it for addresses anyone can send from (UDP): a link roams to wherever its peer's packets come from | nothing |
| `pw_handle_datagram_from` | The same, for a handle only `peer_id` sends on (its DataChannel, its signalling pipe). A packet there for another peer's link, or a handshake from anyone else, is dropped unread, so no peer can move another's link onto its own channel | 0; `PW_ERR_INVALID`, with nothing read, for a null peer id |
| `pw_handle_timeout` | Runs the timers due at `now_us` (retransmits, keepalives, punching). Call it once the `pw_poll_timeout` deadline has passed | nothing |
| `pw_poll_timeout` | The next deadline | µs on the host's clock, or -1 if none |
| `pw_poll_transmit` | Writes the next datagram to `buf` and its destination handle to `*out_addr` | its length, 0 if none; `PW_ERR_BUFFER`, taking nothing, if `cap` is under 1232; `PW_ERR_INVALID`, taking nothing, for a null pointer |
| `pw_poll_event` | Writes the next event to `buf` (see [Events](#events)) | its length, 0 if none, or `-(needed)`: the event stays queued; `PW_ERR_INVALID`, taking nothing, for a null `h`, or a null `buf` with a `cap` |

The loop, from the crate README:

```text
pw_handle_datagram(h, now_us, addr, data, len)      for every datagram received
                                                    (pw_handle_datagram_from with the peer, on a peer's own channel)
pw_handle_timeout(h, now_us)                        once pw_poll_timeout(h) has passed
while (n = pw_poll_transmit(h, now_us, &addr, buf, 1232)) > 0: send buf[..n] to addr
while (n = pw_poll_event(h, buf, cap)) > 0: handle the event in buf[..n]
                                             (n < 0: buf needs -n bytes; the event stays queued)
```

After a round of calls, read `pw_poll_timeout` again and wait until then or the next datagram. A MESSAGE event holds
up to 1 MiB of data, so a host that starts with a small event buffer grows it on `-(needed)`.

### Events

An event is `kind(u8) peer(32)` and then:

| `kind` | Event | Then |
|---|---|---|
| 1 | CONNECTED | `initiator` (u8: 1 if this side dialled), then a path |
| 2 | MESSAGE | `lane` (u8: 0 reliable or urgent, 1 unreliable), then the data, to the end |
| 3 | DISCONNECTED | `reason` (u8: 0 timeout, 1 closed by the peer, 2 replaced, 3 protocol violation, 4 path off: never raised here, as it comes only from a `peerwire` node's connection policy) |
| 4 | CONNECT_FAILED | nothing |
| 5 | PATH_CHANGED | a path. The link moved: relayed to direct after a punch, direct to relayed after the direct path went silent, or to a new address |
| 6 | ADDR_OBSERVED | `addr` (u32, little-endian): the address a directly connected peer, `peer`, sees this endpoint at |

A path is `0` and a handle (u32, little-endian), or `1` and the 32-byte peer id of the relay.

### Sealed messages

```c
int32_t pw_seal(PwEndpoint *h, const uint8_t *to, const uint8_t *data, size_t len, uint8_t *out, size_t cap);
int32_t pw_open(const PwEndpoint *h, const uint8_t *data, size_t len, uint8_t *out_from, uint8_t *out, size_t cap);
```

A sealed message is encrypted to a peer id and authenticated as the sender's, with no session
(`Noise_X_25519_ChaChaPoly_BLAKE2s`). It is for signalling over carriers nobody trusts, such as a WebRTC offer on a
Nostr relay. It has no replay protection: put a timestamp or nonce inside if that matters.

| Function | What it does | Returns |
|---|---|---|
| `pw_seal` | Seals `len` bytes to peer `to`. The result is `len` + 128 bytes | its length; `-(needed)` if `cap` is smaller; `PW_ERR_TOO_LARGE` over 65,407 bytes; `PW_ERR_INVALID` for a null `to` or one that isn't a valid key |
| `pw_open` | Opens a message sealed to this endpoint. Writes the sender's peer id to `out_from` (32 bytes) and the plaintext to `out` | the plaintext's length; `PW_ERR_BUFFER` if `cap` is under `len`; `PW_ERR_INVALID` if it isn't for this endpoint or was changed |

### Nostr keys

```c
int32_t pw_nostr_keypair(const uint8_t *seed, uint8_t *out_secret, uint8_t *out_pub);
int32_t pw_nostr_sign(const uint8_t *secret, const uint8_t *msg, const uint8_t *aux, uint8_t *out_sig);
```

These sign Nostr events for signalling on hosts with no secp256k1 library. They take no handle.

| Function | What it does | Returns |
|---|---|---|
| `pw_nostr_keypair` | Derives a secp256k1 key from 32 seed bytes (a fresh key per session is the intended use). Writes the 32-byte secret and the 32-byte x-only public key | 0; `PW_ERR_INVALID` if the seed isn't a valid secret key: draw another |
| `pw_nostr_sign` | BIP-340 Schnorr signature of a 32-byte message (a Nostr event id) with 32 bytes of auxiliary randomness. Writes 64 bytes to `out_sig` | 0; `PW_ERR_INVALID` for an invalid secret |

### Shared documents

Only with `--features sync`. A `PwDocs` runs beside an endpoint, driven the same way: the host hands it the reliable
messages it receives, sends what it gives on the reliable lane, tells it who joined and left, and runs its timers.
The docs are the ones `room.doc()` holds in `@peerwire/js` (see [Shared documents](../guides/shared-documents.md)):
values, events, entries and status are JSON text in the shapes doc.js gives them.

| Argument | Values |
|---|---|
| `kind_of` | 0 map, 1 set, 2 log |
| `route_kind` | 0 direct, 1 relayed by a member, 2 over signalling (any other value: direct) |
| `flags` of `pw_doc_open` | bit 0 (1): this member is the hub of a star. Other bits are ignored |

`PW_ERR_INVALID` means an unknown doc (never opened, or closed) or an argument doc.js would throw for: not UTF-8,
not JSON, a doc or collection name that isn't 1 to 128 UTF-16 code units, a map key over 256 of them, a value over
65,536 of them as JSON or nested over 64 deep, a `keep` that isn't 1 to 10,000, or a copy that isn't this doc's. A
write the doc refuses because it is full (10,000 entries, or 4 Mi UTF-16 code units of JSON) returns 0. The caps are
in `crates/peerwire-sync/src/lib.rs`.

Docs and members:

```c
PwDocs *pw_docs_new(const uint8_t *me, const uint8_t *seed);
void pw_docs_free(PwDocs *h);
int64_t pw_doc_open(PwDocs *h, uint64_t now_us, const uint8_t *name, size_t name_len,
                    const uint8_t *copy, size_t copy_len, uint32_t flags);
void pw_doc_close(PwDocs *h, uint32_t doc);
void pw_docs_joined(PwDocs *h, uint64_t now_us, const uint8_t *peer_id, uint32_t route_kind);
void pw_docs_left(PwDocs *h, uint64_t now_us, const uint8_t *peer_id);
```

| Function | What it does | Returns |
|---|---|---|
| `pw_docs_new` | Creates the docs of member `me` (its 32-byte peer id). `seed`, 32 bytes from a CSPRNG, draws each doc's incarnation | the handle, or null if `me` or `seed` is null |
| `pw_docs_free` | Frees them. Null is ignored | nothing |
| `pw_doc_open` | Opens doc `name`, or gives the one already open, and asks every member for it. `copy` (or null and 0) is a snapshot from `pw_doc_snapshot` to start from | the doc id (`>= 0`); `PW_ERR_INVALID` for a bad name or a copy that isn't this doc's, and then the doc isn't opened |
| `pw_doc_close` | Stops syncing the doc and forgets it | nothing |
| `pw_docs_joined` | Member `peer_id` joined the room, reached by `route_kind`. Called again for the same member, its route changed | nothing |
| `pw_docs_left` | Member `peer_id` left; what was queued for it goes | nothing |

Messages, events and timers:

```c
int32_t pw_docs_receive(PwDocs *h, uint64_t now_us, const uint8_t *peer_id, const uint8_t *data, size_t len);
int32_t pw_docs_poll_transmit(PwDocs *h, uint8_t *out_peer, uint8_t *buf, size_t cap);
void pw_docs_retry(PwDocs *h, const uint8_t *peer_id, const uint8_t *data, size_t len);
int64_t pw_docs_queued(const PwDocs *h, const uint8_t *peer_id);
int32_t pw_docs_poll_event(PwDocs *h, uint8_t *buf, size_t cap);
int64_t pw_docs_poll_timeout(const PwDocs *h);
void pw_docs_handle_timeout(PwDocs *h, uint64_t now_us);
```

| Function | What it does | Returns |
|---|---|---|
| `pw_docs_receive` | Hands over a reliable message from member `peer_id` | 1 if it was a doc message (first byte 7, or 8, which is dropped) and was taken; 0 if it is the app's; `PW_ERR_INVALID` for a null peer id |
| `pw_docs_poll_transmit` | Writes the next message to `buf` and its member to `out_peer` (32 bytes). Send it as it is with `pw_send` on lane 0 | its length, 0 if none, or `-(needed)`: it stays first |
| `pw_docs_retry` | Gives back a message from `pw_docs_poll_transmit` that the member's link refused (`PW_ERR_WOULD_BLOCK`). It goes first once `pw_docs_poll_transmit` has returned 0 | nothing |
| `pw_docs_queued` | Bytes waiting for member `peer_id` | the count (0 for an unknown member); `PW_ERR_INVALID` for a null peer id |
| `pw_docs_poll_event` | Writes the next event, as JSON, to `buf` | its length, 0 if none, or `-(needed)`: it stays first |
| `pw_docs_poll_timeout` | The next deadline | µs on the host's clock, or -1 if none |
| `pw_docs_handle_timeout` | Runs the timers due at `now_us` | nothing |

The events, from `src/sync.rs`:

```text
{"doc":0,"change":{"name":"meta","type":"map","key":"title","value":"Plans","from":"<hex>","local":false}}
{"doc":0,"sync":{"state":"loading","from":"<hex>","have":12,"total":40,"source":null,"at":null}}
{"overflow":"<hex>"}    the member's link took nothing for too long, or a hub relayed too much for it: close it
```

Writes go to every member at once:

```c
int32_t pw_doc_map_set(PwDocs *h, uint32_t doc, const uint8_t *coll, size_t coll_len,
                       const uint8_t *key, size_t key_len, const uint8_t *value, size_t value_len);
int32_t pw_doc_map_delete(PwDocs *h, uint32_t doc, const uint8_t *coll, size_t coll_len,
                          const uint8_t *key, size_t key_len);
int32_t pw_doc_add(PwDocs *h, uint32_t doc, uint32_t kind_of, const uint8_t *coll, size_t coll_len,
                   const uint8_t *value, size_t value_len, uint8_t *out_id, size_t cap);
int32_t pw_doc_set_delete(PwDocs *h, uint32_t doc, const uint8_t *coll, size_t coll_len,
                          const uint8_t *id, size_t id_len);
int32_t pw_doc_log_keep(PwDocs *h, uint32_t doc, const uint8_t *coll, size_t coll_len, uint64_t keep);
```

| Function | What it does | Returns |
|---|---|---|
| `pw_doc_map_set` | Sets map `coll`'s `key` to `value` (JSON text) | 1; 0 if the doc refused it; `PW_ERR_INVALID` |
| `pw_doc_map_delete` | Deletes map `coll`'s `key` | 1; 0 if it held nothing; `PW_ERR_INVALID` |
| `pw_doc_add` | Adds `value` to a set (`kind_of` 1) or appends it to a log (2), and writes its id to `out_id` | the id's length; 0 if the doc refused it; `PW_ERR_BUFFER`, writing nothing, if `cap` is under 96; `PW_ERR_INVALID`, also for a `kind_of` other than 1 or 2 |
| `pw_doc_set_delete` | Deletes item `id` of set `coll` | 1; 0 if it held none; `PW_ERR_INVALID` |
| `pw_doc_log_keep` | How many entries log `coll` keeps (1 to 10,000; 1000 by default). Call it on every member, right after opening | 0; `PW_ERR_INVALID` |

Reads need a buffer of at least 16 bytes, else `PW_ERR_BUFFER`:

```c
int32_t pw_doc_get(PwDocs *h, uint32_t doc, uint32_t kind_of, const uint8_t *coll, size_t coll_len,
                   const uint8_t *key, size_t key_len, uint8_t *buf, size_t cap);
int32_t pw_doc_entries(PwDocs *h, uint32_t doc, uint32_t kind_of, const uint8_t *coll, size_t coll_len,
                       uint8_t *buf, size_t cap);
int32_t pw_doc_status(PwDocs *h, uint32_t doc, uint8_t *buf, size_t cap);
int32_t pw_doc_snapshot(PwDocs *h, uint32_t doc, uint8_t *buf, size_t cap);
```

| Function | What it does | Returns |
|---|---|---|
| `pw_doc_get` | What map `coll`'s `key`, or set or log `coll`'s item `key` (an id), shows, as JSON text | its length, 0 if none, or `-(needed)`; `PW_ERR_INVALID` |
| `pw_doc_entries` | A collection's entries as JSON, as doc.js's `entries()` gives them | its length or `-(needed)`; `PW_ERR_INVALID` |
| `pw_doc_status` | Where catching up stands, as JSON, as doc.js's `doc.sync`: `{"state":…,"from":…,"have":…,"total":…,"source":…,"at":…}`. `source` is `"members"` once caught up, else `null`; `at` is `null` | its length or `-(needed)`; `PW_ERR_INVALID` |
| `pw_doc_snapshot` | The doc as a copy to keep, for `pw_doc_open`'s `copy`. It holds what members wrote: keep it private | its length or `-(needed)`; `PW_ERR_INVALID` |

## Example in C

Two endpoints in one process, linked over an in-memory wire, as in the crate's `two_endpoints_through_the_c_abi`
test. One sends `hello` on the reliable lane; the other reads its events. Everything happens at two instants, so
there are no timers to run here; a real host also follows `pw_poll_timeout` (see [Host loop](#host-loop)).

```c
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>   /* getrandom: Linux. Use your platform's CSPRNG elsewhere. */

typedef struct PwEndpoint PwEndpoint;
PwEndpoint *pw_endpoint_new(const uint8_t *secret, const uint8_t *seed, uint32_t share_addresses, uint8_t *out_peer);
void pw_endpoint_free(PwEndpoint *h);
int32_t pw_connect(PwEndpoint *h, uint64_t now_us, uint32_t addr, const uint8_t *peer_id);
uint32_t pw_is_connected(const PwEndpoint *h, const uint8_t *peer_id);
int32_t pw_send(PwEndpoint *h, const uint8_t *peer_id, uint32_t lane, const uint8_t *data, size_t len);
void pw_handle_datagram(PwEndpoint *h, uint64_t now_us, uint32_t addr, const uint8_t *data, size_t len);
int32_t pw_poll_transmit(PwEndpoint *h, uint64_t now_us, uint32_t *out_addr, uint8_t *buf, size_t cap);
int32_t pw_poll_event(PwEndpoint *h, uint8_t *buf, size_t cap);

/* An in-memory wire: a reaches b at handle 2, b reaches a at handle 1. */
static void pump(PwEndpoint *a, PwEndpoint *b, uint64_t now_us) {
    uint8_t buf[1232];
    uint32_t addr;
    int32_t n;
    for (int moved = 1; moved;) {
        moved = 0;
        while ((n = pw_poll_transmit(a, now_us, &addr, buf, sizeof buf)) > 0) {
            pw_handle_datagram(b, now_us, 1, buf, (size_t)n);
            moved = 1;
        }
        while ((n = pw_poll_transmit(b, now_us, &addr, buf, sizeof buf)) > 0) {
            pw_handle_datagram(a, now_us, 2, buf, (size_t)n);
            moved = 1;
        }
    }
}

int main(void) {
    uint8_t seed[32], ida[32], idb[32];
    if (getrandom(seed, sizeof seed, 0) != sizeof seed) return 1;
    PwEndpoint *a = pw_endpoint_new(NULL, seed, 0, ida);
    if (getrandom(seed, sizeof seed, 0) != sizeof seed) return 1;
    PwEndpoint *b = pw_endpoint_new(NULL, seed, 0, idb);

    pw_connect(a, 0, 2, idb);
    pump(a, b, 1000);
    printf("linked: %u\n", pw_is_connected(a, idb));

    const char *msg = "hello";
    int32_t r = pw_send(a, idb, 0, (const uint8_t *)msg, strlen(msg));   /* lane 0: reliable */
    if (r < 0) printf("send failed: %d\n", r);
    pump(a, b, 2000);

    size_t cap = 64;
    uint8_t *ev = malloc(cap);
    for (int32_t n; (n = pw_poll_event(b, ev, cap)) != 0;) {
        if (n < 0) {                     /* too small: the event stays queued */
            cap = (size_t)-n;
            ev = realloc(ev, cap);
            continue;
        }
        if (ev[0] == 1) printf("connected\n");
        if (ev[0] == 2) printf("message: %.*s\n", (int)(n - 34), (const char *)ev + 34);
    }
    free(ev);
    pw_endpoint_free(a);
    pw_endpoint_free(b);
    return 0;
}
```

It prints `linked: 1`, `connected` and `message: hello`. The buffers are the program's own (stack and `malloc`): a
native host needs no `pw_alloc`.

## WebAssembly

`packages/js/build.sh` builds the crate without `sync`, so the module exports `memory` and the 27 functions of
`src/lib.rs`, and imports nothing. [`packages/js/src/peerwire.js`](../../packages/js/src/peerwire.js) is its host; the
API it gives apps is in the [JavaScript API](javascript.md).

- **Loading.** `loadPeerwire(source)` takes a URL, a path or the module's bytes. In Node, `file:` URLs and absolute
  paths are read from disk; anything else is fetched. It instantiates the module with an empty import object
  (`WebAssembly.instantiate(bytes, {})`) and wraps the exports in a `Peerwire`. `joinRoom` loads the package's
  `pkg/peerwire_ffi.wasm` (or its `wasm` option) once per source and shares it between rooms and inboxes.
- **Types.** wasm32 pointers and `size_t` are 32-bit, so JS passes them as numbers. `uint64_t` and `int64_t` are
  BigInts: `now_us` is `performance.now()` (or `Date.now()`) times 1000 as a BigInt, and `pw_poll_timeout`,
  `pw_rtt_us` and `pw_path_rtt_us` come back as BigInts.
- **Memory.** Every input is copied into the module's memory. `Peerwire.put(bytes)` calls `pw_alloc` and writes
  through a `Uint8Array` view of `memory.buffer`; the caller frees it with `pw_free` and the same length. When the
  memory grows, the old buffer is detached, so `mem()` makes a new view whenever the buffer changed.
- **Scratch buffers.** Each `Endpoint` allocates its buffers once and frees them in `close()`: 32 bytes for a peer
  id, 1232 each to transmit and receive, 16 KiB to send (a bigger message gets a buffer of its own), 4 for the
  address, and 64 KiB for events, freed and allocated again at `-(needed)` when an event doesn't fit.
- **Fixed sizes are checked first.** Peer ids, secrets and Nostr seeds are checked to be exactly 32 bytes before
  they are copied in, since the module reads 32 bytes whatever it is given.
- **Randomness and time** come from JS: `crypto.getRandomValues` for the seed of `pw_endpoint_new` and the
  auxiliary bytes of `pw_nostr_sign`, and a `setTimeout` to the `pw_poll_timeout` deadline that calls
  `pw_handle_timeout` and pumps.
- **The pump.** `pump()` drains `pw_poll_transmit` into `onTransmit(addr, bytes)` and `pw_poll_event` into `onEvent`
  (decoded by `decodeEvent`), then sets the timer again. Sends, received datagrams and `addRelay` schedule one pump
  per JS task (a microtask); `connect`, `connectVia`, `addCandidate` and `disconnect` pump at once.
- **Handles are local.** An address is a DataChannel or a signalling pipe, so `endpoint()` passes
  `share_addresses` 0 unless it is given `shareAddresses: true`. `receive(addr, bytes, peer)` calls
  `pw_handle_datagram_from` when it knows the channel's peer, and `pw_handle_datagram` otherwise.
- **Sealing.** `seal` gives `pw_seal` an output of the plaintext's length plus 128; `open` gives `pw_open` one of
  the sealed length.
- **After `close()`** the handle and buffers are freed: sends and `connect*` throw, other calls do nothing or return
  null or false (`stats()` the counters as they were at close), so nothing reaches freed memory.

Shared documents in the browser are `doc.js`, not `pw_docs_*`. They speak the same frames as `crates/peerwire-sync`,
which `pw_docs_*` wraps.

## Native libraries

One `cargo build --release -p peerwire-ffi` gives both libraries in `target/release/`. There is no header: declare the
functions you call, as above, and the `PW_ERR_*` values.

```sh
cargo build --release -p peerwire-ffi
cc -o app app.c -Ltarget/release -lpeerwire_ffi      # against libpeerwire_ffi.so
LD_LIBRARY_PATH=target/release ./app
cc -o app app.c target/release/libpeerwire_ffi.a     # static
```

A static link may also need the system libraries Rust's standard library uses on the platform. rustc lists them when
it builds the static library with `--print native-static-libs`.

Who links it:

| Host | Uses |
|---|---|
| Browsers and Node.js (`@peerwire/js`) | the wasm build of this ABI, through `peerwire.js` |
| Godot (`bindings/godot`) | the Rust crates `peerwire` and `peerwire-sync` directly, not this ABI (see [Godot 4](../guides/godot.md)) |
| ESP32 (`ports/esp32`) | the Rust crate `peerwire` directly, not this ABI |
| Swift, Kotlin, Dart, C | the native library. No binding for them is in the repo yet; higher-level bindings are planned |

Rust programs use the crates themselves: see [Rust crates](rust.md).

## Tests

- `cargo test -p peerwire-ffi` (`src/lib.rs`): two endpoints link through the ABI and a message crosses, read
  through a too-small event buffer first; an identity comes back from `pw_secret`; a sealed message opens for its
  recipient and not for a third endpoint; a Nostr signature verifies. Other tests cover `pw_path_rtt_us`,
  `pw_handle_datagram_from`, `pw_add_candidate`, `pw_set_slow` with `pw_add_relay`, and relay-only dials.
  `null_pointers_are_refused` passes every function a null handle, and each output and input null in turn, and
  checks the answer and that nothing was read or taken; it also writes `pw_stats` and `out_addr` at an odd address.
- `fuzz_the_c_abi` (same file) drives two endpoints with random calls and arguments, random and mutated datagrams
  and sealed blobs. Every return code must be one the ABI documents, a too-small event buffer must keep the event,
  and every event must have the documented encoding.
- `cargo test -p peerwire-ffi --features sync` adds `src/sync.rs`: two members share a doc through the ABI, a copy
  opens it again elsewhere, and the ABI refuses what doc.js throws for, and null pointers.
- `packages/js/test/node_pair.mjs` links two endpoints of the wasm module in Node. Others there cover calls after
  `close()` (`endpoint_closed.mjs`), scratch-buffer reuse across memory growth (`scratch_buffers.mjs`), peer-id
  checks (`peer_id.mjs`), `pw_handle_datagram_from` (`receive_from.mjs`) and the event decoder (`fuzz.mjs`).

Nothing in the gate or CI compiles C against the library. See [Testing](../project/testing.md) for the gate.

## Limits

- **No C header** and no C test, as above. The declarations on this page follow `src/` as it is.
- **The ABI may change** before a release; it is v0.1.
- **The endpoint only.** Rooms, invites, signalling, STUN and transports are not in the ABI. A C host brings its
  own, as `peerwire.js` and `room.js` do in the browser.
- **One setting.** Only `share_addresses` can be set; the rest of the endpoint's `Config` is the core's default.
- **Misuse isn't caught, past null pointers.** A freed handle, a pointer to too little memory (a short fixed-size
  buffer, a `len` or `cap` past its buffer's end), or a `pw_free` with the wrong length is undefined behaviour, not
  an error code.
