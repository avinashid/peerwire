# Installation

Peerwire ships as an npm package for browsers and Node, React hooks on top of it, Rust crates, a Godot extension and
a C library. Pick the one for your platform.

## JavaScript

```sh
npm install @peerwire/js
```

It runs in browsers and in Node 20 or later. The package is plain ES modules with TypeScript types, plus one
WebAssembly file, `peerwire_ffi.wasm` (~350 KB, no imports). Bundlers such as Vite, webpack and esbuild find the
wasm next to the package on their own.

Node has no WebRTC of its own. Rooms with `signaling: 'local'` work in one Node process (and its workers) without
it; to reach other machines from Node, pass an `RTCPeerConnection` from a WebRTC package, such as
node-datachannel's polyfill. See [`joinRoom`](../reference/javascript.md).

Without a bundler, serve the package's files and map the import in your page:

```html
<script type="importmap">{ "imports": { "@peerwire/js": "/node_modules/@peerwire/js/src/index.js" } }</script>
```

## React

```sh
npm install @peerwire/react @peerwire/js
```

`react` 18 or later and `@peerwire/js` are peer dependencies. The hooks are written in TypeScript and the types ship
with the package. The [React guide](../guides/react.md) shows how to use them.

## Rust

Peerwire isn't on crates.io yet. Add it from git:

```toml
[dependencies]
peerwire = { git = "https://github.com/avinashid/peerwire", features = ["nostr", "tokio"] }
```

| Feature | Adds |
|---|---|
| `nostr` | `NostrSignaling`: rooms over public Nostr relays (a websocket client with rustls) |
| `tokio` | `AsyncNode`: the node as a tokio task that waits on its socket and timers |

With neither feature, you get the blocking `Node`, invites, `MemoryHub` for tests and `LanSignaling` for the local
network. Pin a commit with `rev = "…"` if you need builds to stay reproducible while the API changes.

The other crates come from the same repository the same way: `peerwire-core` (the sans-IO core on its own),
`peerwire-sync` (shared documents), `peerwire-game` and `peerwire-sim`. [Rust crates](../reference/rust.md) says
what each one is for.

## Godot 4

The extension is built from source for each desktop platform you ship:

```sh
git clone https://github.com/avinashid/peerwire && cd peerwire
cargo build --release -p peerwire-godot     # -> target/release/libpeerwire_godot.so (.dll, .dylib)
```

Copy the library and [`bindings/godot/peerwire.gdextension`](../../bindings/godot/peerwire.gdextension) into your
project under `res://addons/peerwire/`, with the library in `res://addons/peerwire/bin/` under the name the
`.gdextension` file lists for your platform. Godot loads it at the next start, and `PeerwireMultiplayerPeer` appears
as a built-in class. It needs Godot 4.3 or later; see the [Godot guide](../guides/godot.md).

## C and other languages

The C ABI is a shared or static library:

```sh
cargo build --release -p peerwire-ffi       # -> target/release/libpeerwire_ffi.{so,dylib,dll,a}
```

Swift, Kotlin, Dart and anything else with a C FFI can call it. [C ABI](../reference/c-abi.md) lists the functions
and their conventions.

## Docker

The optional always-on peer, and the browser demos, run with Docker, from the repo root:
`docker compose -f docker/compose.yml up -d --build`. The peer also runs without Docker. See
[always-on peer](../guides/always-on-peer.md).

## Building from source

You need:

- **Rust**, stable, through [rustup](https://rustup.rs), with the WebAssembly target for the JS package:
  `rustup target add wasm32-unknown-unknown`;
- **Node.js** 20 or later, for the JS and React packages.

```sh
git clone https://github.com/avinashid/peerwire && cd peerwire
cargo build --workspace --release            # every crate
./packages/js/build.sh                       # -> packages/js/pkg/peerwire_ffi.wasm
```

To use the JS package from `main` in your own app, pack it and install the tarball:

```sh
cd packages/js && npm pack                   # builds the wasm, writes peerwire-js-<version>.tgz
cd ../react && npm ci && npm pack            # @peerwire/react, if you use it
cd /path/to/your-app && npm install /path/to/peerwire/packages/js/peerwire-js-*.tgz
```

To try the browser demos locally, serve `packages/js` and open `http://localhost:8000/demo/` in two tabs:

```sh
python3 -m http.server -d packages/js 8000
```

[Contributing](../../CONTRIBUTING.md) covers running the tests and the gate.
