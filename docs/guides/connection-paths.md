# Choosing paths

A link between two peers can take several paths: straight between them, through a TURN server, through another
peer, or over the signalling relays. With no policy, Peerwire tries every path it has and keeps the fastest. A
**connection policy** lets you choose which paths a link may take and in what order, bring your own servers, and
change both while the app runs.

## The paths

| Path | JavaScript | Rust | Who carries the packets | Cost |
|---|---|---|---|---|
| Direct | `'direct'` | `Direct` and `Birthday` | nobody: straight between the peers, through their NATs | none; each peer sees the other's address |
| TURN | `'turn'` | none yet | your TURN server (coturn, Cloudflare TURN, …), named in `iceServers` | the server's bandwidth; it sees both addresses |
| Another peer | `'peers'` | `Peers` | a member linked directly to both | that member's upload, and a detour |
| Your relay peer | none | `RelayPeers` | your own always-on `peerwire-peer` ([always-on peer](always-on-peer.md)) | its upload, and a detour |
| Signalling | `'signaling'` | none | the Nostr relays the room meets on, as sealed events | slow and rate-limited: a last resort |

In Rust, `Birthday` is the extra punching for symmetric NATs (see [NAT traversal](../concepts/nat-traversal.md));
in JavaScript it comes with `'direct'` (`birthday: false` turns it off). A relay peer is Rust's equivalent of your own
TURN server. Rust has no TURN client yet.

Every path is end-to-end encrypted: a TURN server, a relaying peer or a signalling relay sees sizes and timing, never
the content.

## How a policy is read

The same rules hold in both languages:

- **A path missing from the order is off.** No policy at all is every path, exactly as before policies.
- **Order is the order of trying.** The dialling side tries the first path first and starts each later one when
  those before it fail or time out (`fallbackMs` in JavaScript, `Policy::fallback` in Rust).
- **A link moves only to an earlier path.** A link through a relay ranked before the direct paths stays there: it
  never punches to a direct path. So `['peers', 'direct']` (or `[Peers, Direct]`) keeps a relayed link relayed.
- **Live changes move links.** A link on a path a new policy turns off moves to a path still allowed, or closes:
  `'peer-leave'` with reason `'policy'` in JavaScript, `Disconnected` with `DisconnectReason::PathOff` in Rust.
- **Each end has its own.** A policy governs what this end dials, moves to and keeps. A peer whose policy differs
  may still dial this end over a path it has off; that link moves to an allowed path or closes.

Two details differ:

- In JavaScript, `'direct'` and `'turn'` next to each other are one WebRTC attempt, in which ICE prefers the direct
  pairs. Put a fallback between them to make TURN a later step: `['direct', 'peers', 'signaling', 'turn']`.
- In Rust, `Direct` and `Birthday` next to each other are one step, and `Peers` needs a peer linked directly to both,
  so with both direct kinds off only relay peers can relay.

## Recipes

| You want | JavaScript `order` | Rust `order` |
|---|---|---|
| Every path, fastest first | no policy (or `fallback: 'auto'`) | no policy |
| No other member carries your packets | `['direct', 'turn']` (`fallback: 'off'`) | `[Direct, Birthday, RelayPeers]` |
| TURN only when nothing free works | `['direct', 'peers', 'signaling', 'turn']` | not available |
| Peers never see your IP address | `['turn']`, with your TURN server | `Privacy::RelayOnly`, not a policy |
| Your own relay first, direct only if it fails | not available | `[RelayPeers, Direct, Birthday]` |
| Only direct links, nothing relayed | `['direct']` | `[Direct, Birthday]` |

With `['turn']` alone, WebRTC offers relay candidates only (`iceTransportPolicy: 'relay'`), so no address of yours
reaches the peer; the TURN server still sees it. A Rust policy chooses the paths links take, not the addresses a node
gives out: under `[RelayPeers]` its offers still carry its addresses. To hide them, use `Privacy::RelayOnly`, which
links through one relay (see [privacy](privacy.md)).

## Bring your own servers

Nothing has to go through servers someone else runs. In JavaScript, put your STUN and TURN servers in `iceServers`
and your Nostr relays in `relays`, on `joinRoom` or later with `setPolicy`:

```js
const myTurn = { urls: 'turns:turn.example.com:5349', username: 'app', credential: turnPassword };
const room = await joinRoom({
  appId: 'my-app', roomId, signaling: 'nostr',
  policy: {
    order: ['direct', 'peers', 'signaling', 'turn'],
    iceServers: [{ urls: 'stun:stun.example.com:3478' }, myTurn],
    relays: ['wss://relay.example.com'],
  },
});

// Later, live: hide our address from peers. Links move onto TURN or drop and dial again.
room.setPolicy({ order: ['turn'] });
room.route(peer); // { kind: 'direct', turn: true, rttMs: 41 }
```

`room.policy` shows `iceServers` as their urls only, never a username or credential, and errors name a server by its
index. Use short-lived TURN credentials where your server supports them, as anyone with the page can read them.

In Rust, set your STUN servers, relay peers and policy on the builder, and change them on the node:

```rust
use peerwire::{Node, PathKind, Policy};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let relay = "pw:…"; // your always-on peer's invite
    let mut node = Node::builder()
        .stun_servers(["stun.example.com:3478"])
        .relay_peers([relay])
        .policy(Policy::new([PathKind::Direct, PathKind::Birthday, PathKind::RelayPeers]))
        .bind("0.0.0.0:0")?;

    // Later, live: through the relay peer only. Direct links move to it, or close with PathOff.
    node.set_policy(Policy::new([PathKind::RelayPeers]))?;
    node.set_stun_servers(["stun2.example.com:3478"]);
    Ok(())
}
```

`Node::set_signaling` swaps the signalling carrier (`NostrSignaling::new([...])` with other relays, say) the same
way: links stay up, and the node says hello on the new one. `set_policy(None)` goes back to every path.

## What each server learns

| Server | Learns |
|---|---|
| STUN | your public address, when you ask it |
| TURN | your address and the peer's, and the size and timing of your packets |
| A relaying member or relay peer | both addresses, sizes and timing |
| Signalling relays | your address, hashed topics and sealed events (see [security model](../concepts/security-model.md)) |

## Limits

- TURN is WebRTC's, so only JavaScript has it. A Rust node relays through peers and relay peers.
- The `'signaling'` path is JavaScript's; a Rust node uses signalling to meet peers, not to carry a link.
- After a link moves off a member, the peer can still send through that member for a while: the core takes relayed
  packets from any member that forwards them.
- In JavaScript, `privacy` is set when the room is joined; `setPolicy` refuses a different one.

The full API is in the [JavaScript](../reference/javascript.md#policy-and-setpolicy) and
[Rust](../reference/rust.md#policy) references.
