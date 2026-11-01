# Contributing

Thanks for helping with Peerwire. It is early (v0.1), so the APIs still move; an issue first is the quickest way to
agree on a larger change before you write it. [Architecture](docs/concepts/architecture.md) is the place to start
reading. Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).

- **Bugs and ideas:** open an issue from one of the templates.
- **Security problems:** never in a public issue. Report them privately, as [SECURITY.md](SECURITY.md) says.
- **AI coding agents:** read [AGENTS.md](AGENTS.md) too (Claude Code loads it through [CLAUDE.md](CLAUDE.md)).

## Prerequisites

| Tool | For | |
|---|---|---|
| Rust, stable | everything | with clippy, and the wasm target: `rustup target add wasm32-unknown-unknown` |
| Node.js 20 or newer | the JS packages and their tests | CI uses Node 22, and `scripts/gate.sh --strict` runs CI's Node tests on it |
| Python 3 | the browser tests | they serve files with `python3 -m http.server` on loopback |
| Playwright and Chromium | the browser tests | set `PLAYWRIGHT` to Playwright's `index.mjs` (in its `node_modules/playwright/`) |
| Godot 4.3 or newer | optional: the Godot binding's demo and test | see [the Godot guide](docs/guides/godot.md) |
| Docker with Compose v2 | optional: the always-on peer and the examples server | see [the always-on peer guide](docs/guides/always-on-peer.md) |
| The ESP32 toolchain | optional: `ports/esp32` | see its [README](ports/esp32/README.md) |

`rustfmt` and `cargo-nextest` are optional: the gate checks formatting when rustfmt is installed (CI always does) and
runs the Rust tests under nextest when it is installed.

## Build

```sh
cargo build --workspace                        # the crates and the Godot binding
./packages/js/build.sh                         # the wasm module: packages/js/pkg/peerwire_ffi.wasm
(cd packages/react && npm ci && npm run build) # @peerwire/react: dist/
```

To try the browser demos, serve `packages/js` (`python3 -m http.server -d packages/js 8000`) and open
`http://127.0.0.1:8000/demo/`. [examples/README.md](examples/README.md) lists every example and how to run it.

## Test

One test at a time:

```sh
cargo test -p peerwire-core                    # one crate (or --workspace --release for all)
node packages/js/test/room_node.mjs            # one Node or browser test
```

All of them, through the gate. It runs every step even after a failure, and prints a summary:

| Command | Runs |
|---|---|
| `scripts/gate.sh` | Rust tests, clippy, the wasm build with its size and import checks, the `@peerwire/react` build and types, the Node tests, then the browser tests (headless Chromium, loopback) |
| `scripts/gate.sh --quick` | the same without the browser tests |
| `scripts/gate.sh --strict` | adds clippy with `-D warnings`, the fuzz tests again in a debug build (overflow checks), and CI's Node tests again on CI's Node |
| `scripts/gate.sh --nostr` | adds the tests that use the public Nostr relays and public STUN |
| `scripts/gate.sh --full` | runs the Rust steps even when no Rust changed (by default they're skipped when every changed file is JS, docs or another non-Rust path) |

The flags combine. Before you open a pull request, run `scripts/gate.sh --quick`, or `--quick --strict` if you changed
Rust, and each browser test next to what you changed. CI runs the Rust and Node steps on every pull request, but not
the browser tests. A change to docs or comments only needs no run. Each step logs to `target/gate-logs/<step>.log`;
`GATE_JOBS` sets how many Node tests run at once (default 4). [Testing](docs/project/testing.md) describes what each
test covers.

Tests stay local: loopback, the LAN or the simulator. The only outside services they may touch are public STUN
(Google, Cloudflare) and the Nostr relays `nos.lol`, `relay.primal.net` and `nostr.mom`, in small tests behind
`--nostr`.

## Repo layout

| Path | What it is |
|---|---|
| `crates/peerwire-core` | the sans-IO session core: identity, the Noise IK handshake, lanes, congestion control, relay, NAT traversal |
| `crates/peerwire` | nodes over UDP: STUN, rooms, signalling (Nostr, LAN), NAT traversal, an async host |
| `crates/peerwire-sync` | shared documents: maps, sets and logs that converge |
| `crates/peerwire-ffi` | the C ABI (`pw_*`), built as the zero-import wasm module and as a native library |
| `crates/peerwire-sim` | a deterministic network simulator and the benchmarks |
| `crates/peerwire-game` | deterministic fixed-point maths, Hilbert-curve islands, desync detection |
| `crates/peerwire-peer` | the optional always-on peer (a binary) |
| `bindings/godot` | the Godot 4 GDExtension |
| `packages/js` | `@peerwire/js`: the JS host, WebRTC transport, signalling, demos and tests |
| `packages/react` | `@peerwire/react`: React hooks, an example app and tests |
| `examples/` | runnable examples: Rust programs and the Godot demo |
| `ports/esp32` | the ESP32-C3 prototype (not in the workspace) |
| `docker/` | the Dockerfile, the compose file and the examples server's config |
| `docs/` | the documentation, indexed by [docs/README.md](docs/README.md) |
| `scripts/` | the gate (`gate.sh`), the site build (`site.sh`, `site/`) and benchmarks |

## Rules for code

- Keep `peerwire-core` **sans-IO and deterministic**: no sockets, threads, clocks or randomness inside it. The host
  passes time and bytes in, which is what makes the simulator and the wasm build possible.
- Keep the wasm build **free of imports**: no wasm-bindgen, no std I/O in `peerwire-ffi`.
- A change to the wire format, crypto, relaying or signalling needs a security review and a test.
- Every performance number in the docs must be reproducible from the repo, or labelled **theoretical** with its
  formula.
- A change comes with its test.
- Match the code around it: 120 columns, the indentation in [.editorconfig](.editorconfig), Rust formatted as
  [rustfmt.toml](rustfmt.toml) says.
- Keep new dependencies few, and say why in the commit message.

## Docs

The docs live in [docs/](docs/README.md), in sections: getting started, guides, concepts, reference and project. A new
page goes in its section and gets a line in [docs/README.md](docs/README.md), the index. The README, the package
READMEs and the docs are also built into the site with the demos: `scripts/site.sh --serve` previews it on loopback,
and a link to a missing file or `#anchor` fails its build.

## Branches, commits and pull requests

- **Branches:** `<area>/<short-slug>`, such as `js/room-timeout`. Maintainers add the task number:
  `<area>/NB-XXX-slug`.
- **Commits:** small, one change each. The subject says what changed in plain words, with the task or issue number at
  the end when there is one: `peerwire: LanSignaling binds the group address (NB64-1)`. The body, if any, says why.
  Commit messages describe the change only, with no tool trailers or signatures.
- **Pull requests:** one topic each. Fill in the template: what, why, how it was tested, and what the gate printed.

## Security reports

Report vulnerabilities privately through GitHub's "Report a vulnerability" on this repository, never in a public
issue or pull request. [SECURITY.md](SECURITY.md) says what is in scope, and
[the threat model](docs/project/threat-model.md) lists the known findings.

## License

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this work, as
defined in the Apache-2.0 license, is dual licensed under the [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE)
licenses, without any additional terms or conditions.
