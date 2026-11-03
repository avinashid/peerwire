# Always-on peer

Peerwire needs no server: peers find each other and link directly. An always-on peer is optional. It is an ordinary
peer that stays online, runs `peerwire-peer` from [`crates/peerwire-peer`](../../crates/peerwire-peer), and does two
things for the peers linked to it:

- **It relays.** Two peers whose NATs can't punch each other (both behind symmetric NATs, such as two phones on
  hotspots) link through a third peer they are both linked to. An always-on peer with a reachable address is that
  third peer. Relayed traffic stays end-to-end encrypted between the two peers; the relay sees their addresses,
  packet sizes and timing.
- **It keeps one identity.** Its peer id survives restarts, so the invite you hand out keeps pointing at the same
  peer.

It sends no app data of its own and drops messages sent to it. It speaks UDP, so it serves native peers (Rust, C,
Godot); browsers speak WebRTC and can't reach it. It doesn't yet join rooms over Nostr, act as a room's super-peer, or
store messages for peers that are offline.

## Run it

### With Docker

From the repo root:

```sh
docker compose -f docker/compose.yml up -d --build
docker compose -f docker/compose.yml logs peer                       # its peer id and pw: invite
docker compose -f docker/compose.yml exec peer peerwire-peer check   # ok: linked <peer id>
open http://127.0.0.1:7780                                           # the browser demos, served next to it
docker compose -f docker/compose.yml down                            # add -v to delete its identity too
```

`export COMPOSE_FILE=docker/compose.yml` lets you leave out `-f docker/compose.yml`. The compose file runs the peer
and a static server for the browser demos, both on this machine only (see [The containers](#the-containers)).

Docker Desktop on macOS and Windows runs containers in a VM, and host networking there reaches only that VM. Run the
binary natively instead.

### Without Docker

`peerwire-peer` builds with stable Rust on Linux and macOS (it uses Unix file permissions; on Windows, use WSL). Give
it a directory outside the repo for its identity, so the secret key can't be committed by mistake:

```sh
mkdir -p ~/.local/share/peerwire-peer
export PEERWIRE_DATA=~/.local/share/peerwire-peer
cargo run --release -p peerwire-peer              # prints its peer id and invite, then runs until stopped
cargo run --release -p peerwire-peer -- check     # in another shell: links to it from loopback, exits 0 if it answers
```

Without `PEERWIRE_DATA` it makes a new identity on each start and writes no files.

## Use it from an app

Hand its invite to the app's peers and they keep a link to it, redialling when the link drops:

- Rust: `node.set_relay_peers(&["pw:…"])?` on a `peerwire::Node`; `node.relay_peers()` shows the links.
- Godot: `peer.set_relay_peers(PackedStringArray(["pw:…"]))` on a `PeerwireMultiplayerPeer`;
  `get_relay_peers_linked()` counts the links. See [Relay peers](godot.md#relay-peers) in the Godot guide.

Relay links carry no app traffic. When two of the app's peers can't punch each other, they link through the always-on
peer, and move to a direct path if one starts working later. [NAT traversal](../concepts/nat-traversal.md#relay-peers)
covers the details.

## What it does

- **Identity.** On first start it writes a new identity to `$PEERWIRE_DATA/identity.key`: 32 secret bytes, mode 0600.
  Later starts reuse it. The directory must exist.
- **Invite.** On every start it prints its peer id and `pw:` invite, and writes the invite to `$PEERWIRE_DATA/invite`
  (mode 0600, replaced in one step, so `check` never reads half of one). When STUN reports a new public address it
  prints and writes a new invite. Each start puts a fresh invite key in the invite. That key only gates requests
  over signalling, which this peer doesn't use, so an invite from before a restart still links while the address is
  the same.
- **Links.** Anyone who knows its peer id and an address (the invite has both) can link to it, up to
  `PEERWIRE_MAX_PEERS` at once. Past that, the core drops a new handshake before any key exchange: the newcomer gets
  no answer and nothing relayed. Refusals are counted, not logged one by one, so a flood of handshakes can't fill the
  log.
- **Relay.** It forwards packets between two peers linked to it when one asks, within a per-peer rate and a total
  rate (below).
- **Health check.** `peerwire-peer check [FILE]` links to the invite in `FILE` (default `$PEERWIRE_DATA/invite`) from
  `127.0.0.1` and exits 0 if the handshake completes within 5 s, 1 otherwise. It is the image's `HEALTHCHECK`.
- **Logs.** One line per link and unlink, and a stats line every `PEERWIRE_STATS_SECS`: links, refused links, dropped
  handshakes, packets in and out, packets relayed and relayed packets dropped, failed authentications.
  **The logs hold every linked peer's IP address and port**, which is personal data. The compose file caps them at
  three 10 MB files per service; keep or ship them accordingly.

```text
peer 9400d6be8b07 on 127.0.0.1:7700 (relay true, max 256 peers)
pw:…
linked 1fa0a52d1c32 Direct(127.0.0.1:46793) (1 peers)
unlinked 1fa0a52d1c32 ClosedByPeer (0 peers)
stats: 0 peers, 0 refused (over 256), 0 handshakes dropped, rx 3 tx 1 packets, relayed 0 dropped 0, rx auth failed 0 malformed 0
```

## Settings

All from the environment. With Docker, set them in your shell or in `docker/.env` (Compose reads the `.env` next to
the compose file, not one at the repo root); `PEERWIRE_DATA` is fixed to the `/data` volume there.

| Variable | Default | |
|---|---|---|
| `PEERWIRE_BIND` | `127.0.0.1:7700` | UDP address to bind. `0.0.0.0:7700` (IPv4) or `[::]:7700` (IPv4 and IPv6) makes it reachable from other hosts |
| `PEERWIRE_DATA` | unset natively, `/data` in the image | directory for `identity.key` and `invite`. Unset: a new identity each start, no files |
| `PEERWIRE_STUN` | the default servers (Google, Cloudflare) | comma-separated `host:port` list; `none` turns STUN off. A loopback bind never uses STUN |
| `PEERWIRE_MAX_PEERS` | `256` | most links at once. Handshakes under way don't count; the core caps those at 4096 |
| `PEERWIRE_RELAY` | `1` | `0`: accept links but never relay |
| `PEERWIRE_RELAY_RATE` | `262144` | bytes per second each peer may have relayed, with a 64 KiB burst |
| `PEERWIRE_RELAY_TOTAL` | `8388608` | bytes per second relayed for all peers together, with a burst of a quarter of that. `0`: no limit |
| `PEERWIRE_STATS_SECS` | `60` | seconds between stats lines. `0`: none |

## Expose it

By default nothing beyond this machine can reach it. To run a public always-on peer on a host with its own address:

```sh
PEERWIRE_BIND=0.0.0.0:7700 docker compose -f docker/compose.yml up -d peer
```

Open UDP port 7700 in your firewall yourself. The invite then lists the host's LAN or public address and loopback,
plus its public address from STUN if it is behind a NAT. Hand out the invite printed after the change.

Before you do, size what you give away:

- **Bandwidth.** Identities are free to make, so one host can take every slot. The most it can have relayed out of
  your server is `PEERWIRE_RELAY_TOTAL` (8 MiB/s by default), and it can aim all of it at one linked peer, from your
  address. Lower `PEERWIRE_RELAY_TOTAL` and `PEERWIRE_MAX_PEERS` to what you want to give.
- **Memory.** Each linked peer can make the server hold about 5.7 MB of received data (about 6.1 MB allocated), so 256
  peers can pin about 1.6 GB. Size `PEERWIRE_MAX_PEERS` to the memory you have. The compose file limits the peer's
  container to 128 MB (`mem_limit`), the worst case for about 19 peers: past that, peers that fill their buffers can
  get it killed and restarted. Raise `mem_limit` or lower `PEERWIRE_MAX_PEERS` to match.
- **Privacy.** The server sees which peers talk to each other, when, and how much, and logs their addresses.

The [threat model](../project/threat-model.md) lists these bounds with the rest.

The examples server is static files only, and has no TLS. To show it on your LAN, change the `127.0.0.1:` prefix of
its `ports` entry in `docker/compose.yml` (`PEERWIRE_EXAMPLES_PORT` changes the port). Put it behind your own HTTPS
proxy before exposing it further.

## The containers

| Service | Image | Listens on (default) | User | Root filesystem |
|---|---|---|---|---|
| `peer` | `peerwire-peer` (target `peer`): the binary on `gcr.io/distroless/cc-debian12:nonroot`, no shell | UDP `127.0.0.1:7700`, host network | 65532 | read-only; the identity in the `peerwire_peer-data` volume at `/data` |
| `examples` | `peerwire-examples` (target `examples`): `packages/js` `demo/`, `src/` and the built `pkg/peerwire_ffi.wasm` on `nginx:stable-alpine` | TCP `127.0.0.1:7780` → 8080 | `nginx` (101) | read-only; tmpfs `/tmp` |

Both drop every Linux capability, set `no-new-privileges`, and have memory and process limits.

**Why host networking for the peer.** A peer gives out the addresses its socket is bound to, and sees the real source
address of each packet. Behind Docker's port mapping it would give out the container's bridge address and see
Docker's gateway as every peer's address, which breaks invites, relaying and hole punching. With `network_mode: host`
and a bind of `127.0.0.1:7700`, it is on loopback only, like any local process.

**The examples server** answers GET and HEAD only (anything else gets 403, and so does any dotfile). It sends a
Content-Security-Policy that allows same-origin scripts, styles and wasm, connections to the same origin and `wss:`
(the Nostr relays), and no framing.

The files are in [`docker/`](../../docker): `Dockerfile` (both images), `compose.yml`, `examples-nginx.conf`, and
`Dockerfile.dockerignore`, which keeps build output, `.git`, keys and `.env` files out of the build. The build
context is the repo root, so build an image on its own from there:

```sh
docker build -f docker/Dockerfile -t peerwire-peer .                          # the peer (the default target)
docker build -f docker/Dockerfile --target examples -t peerwire-examples .    # the examples server
```

## Check it

Tested on loopback (Docker 29.7, Compose v5.5, aarch64):

```sh
docker compose -f docker/compose.yml up -d --build
docker compose -f docker/compose.yml ps                              # both (healthy)
docker compose -f docker/compose.yml exec peer peerwire-peer check   # ok: linked <peer id>
ss -lun | grep 7700; ss -lnt | grep 7780                             # 127.0.0.1 only
curl -sI http://127.0.0.1:7780/pkg/peerwire_ffi.wasm                 # 200, application/wasm
docker compose -f docker/compose.yml restart peer
docker compose -f docker/compose.yml logs peer | grep -o 'peer [0-9a-f]*'   # the same peer id twice
docker compose -f docker/compose.yml down -v
```
