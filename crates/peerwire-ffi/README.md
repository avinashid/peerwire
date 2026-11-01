# peerwire-ffi

A C ABI over the [`peerwire-core`](../peerwire-core) `Endpoint`: one surface for every host
that isn't Rust.

- Built for `wasm32-unknown-unknown`, it is a WebAssembly module of about 350 KB with
  **no imports** (no wasm-bindgen, no WASI). The browser package [`@peerwire/js`](../../packages/js)
  loads it and supplies WebRTC DataChannels, timers and randomness from JavaScript.
- Built natively, it is a shared or static library (`cdylib`, `staticlib`) for Dart FFI,
  Swift or C.

Like the core, it does no I/O. Addresses are `u32` handles that the host maps to its own
transports (a DataChannel, a socket), so the same endpoint runs over anything that carries
datagrams.

## Build

```sh
./packages/js/build.sh                  # -> packages/js/pkg/peerwire_ffi.wasm (size-optimised)
cargo build --release -p peerwire-ffi     # -> target/release/libpeerwire_ffi.{so,dylib,dll,a}
```

CI and `scripts/gate.sh` fail the wasm build if it grows past 350 KiB (`WASM_MAX_BYTES`,
358400 bytes) or gains an import.

## Conventions

- Times are microseconds on the host's monotonic clock.
- Peer ids are 32-byte buffers.
- Buffers are allocated in the module's memory with `pw_alloc` and freed with `pw_free`.
- Return codes: `>= 0` is success (a length, where one is returned), `< 0` is an error:
  `PW_ERR_NOT_CONNECTED` (-1), `PW_ERR_TOO_LARGE` (-2), `PW_ERR_WOULD_BLOCK` (-3),
  `PW_ERR_INVALID` (-4), `PW_ERR_BUFFER` (-5, the output buffer is too small and nothing
  was consumed).

## Functions

49 `pw_*` functions: 27 in every build (`src/lib.rs`: memory, the endpoint, links, sending,
the host loop, sealed messages, Nostr keys) and 22 for shared documents, `pw_docs_*` and
`pw_doc_*` (`src/sync.rs`), with the `sync` feature, which the wasm build leaves off.

The **[C ABI reference](../../docs/reference/c-abi.md)** lists every function with its C
declaration, the buffer rules, the event encoding and an example in C. The full contract of
each function is in its doc comment.

## Tests

`cargo test -p peerwire-ffi --features sync` runs 9 tests through the C ABI:
- two endpoints link, restore an identity from its secret, round-trip a sealed message and
  verify a Nostr signature;
- a relayed link moves to a direct address (`pw_add_candidate`), and a slow link moves
  through a relay and back (`pw_add_relay`); a relay-only dial sends nothing direct;
- `pw_handle_datagram_from` drops another peer's packets; path RTT is timed from the
  handshake;
- two members share a doc, and the ABI refuses what `doc.js` throws for;
- a fuzz of the whole ABI checks every event's encoding.

The wasm module is also tested from JavaScript, in the gate and in CI: the
`packages/js/test/*.mjs` tests, starting with `node_pair.mjs`, which links two endpoints in
Node.js.

## Status

Early development (v0.1). The ABI may still change; there is no C header yet.
