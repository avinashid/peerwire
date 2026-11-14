# Connectivity in the field

Peerwire's NAT traversal is measured in a simulator: in a room of two, 59 of the 64 pairings of eight NAT kinds
connect directly ([status](status.md#how-far-its-been-tested)). This page is for results from real networks, each
with its date and how it was measured. A row says what one network did on one day; carrier NATs differ by region,
cell and load, so read it as one sample, not a guarantee.

## Method

Run the doctor on the network under test. It prints a report and uploads nothing; `--redact` masks public IP
addresses before you paste it anywhere, and `--json` gives the same report as JSON.

```sh
cargo run -p peerwire --features nostr --example doctor -- --redact   # every check
npx @peerwire/js doctor --redact                                       # Node: no port mapping or LAN check
```

`npx @peerwire/js doctor` needs the first `@peerwire/js` release after 0.3.0; from a checkout,
`node packages/js/bin/peerwire.js doctor` runs the same command. Both doctors report:

| Check | How | Results |
|---|---|---|
| NAT mapping, IPv4 and IPv6 | STUN Binding requests from one socket to four destinations: Google's and Cloudflare's servers, two ports each | `no-nat`; `endpoint-independent` (a cone, which punches easily); `address-dependent` or `address-and-port-dependent` (symmetric, keyed on the server's IP or on IP and port); `dependent` (symmetric, too few destinations to say which); `pooled` (the port kept but the public IP varying); `blocked` (no answers); `unknown`; `no-route` |
| NAT filtering, IPv4 | The RFC 5780 test against a server named with `--stun5780 host:port`: does an answer from the server's other IP, or only from its other port, get in? | `endpoint-independent`, `address-dependent`, `address-and-port-dependent`; `not-measured` without such a server (Google's and Cloudflare's can't answer from another address); `unsupported` when the server named ignores or refuses the request |
| Router port mapping (Rust) | A node with `Builder::port_mapping` on asks the router by PCP, NAT-PMP and UPnP, as an app's node would; the mapping is removed before the doctor exits | `mapped`; `not-public` (the router's own address is carrier-grade NAT or private, so another NAT sits beyond it); `unavailable`; `no-answer`. The JS doctor says `not-available`: browsers can't map ports |
| Nostr relays | For each default relay (`relay.primal.net`, `relay.nostr.net`, `nostr.oxtr.dev`): two connections, and one ephemeral event on a random topic from one to the other, signed with a throwaway key | `reachable` with the time to open and the round trip; `no-echo` (connected, but the event didn't come back); `unreachable` |
| LAN signaling (Rust) | One `LanSignaling` frame to the group, heard by a second carrier on the same machine | `works-here` (this interface takes multicast or broadcast; other devices hear it only if the network passes it between them); `silent`; `unavailable`. The JS doctor says `not-available` |

The doctor ends with a few lines saying what the results mean for links from that network. To see a link itself,
run the Rust doctor with `room <name>` on two machines on different networks: each reports whether the link came
up direct, relayed through a peer, or not at all, and when.

What it can't tell: how long the NAT keeps an idle mapping, whether its behaviour changes under load, whether a Wi-Fi
network keeps its clients apart (client isolation) for a LAN room, or how a browser's ICE fares beyond the NAT
mapping its STUN meets (Node has no WebRTC).

## Results

| Network | Date | Method | IPv4 mapping | Filtering | IPv6 | Port mapping | Nostr relays | LAN |
|---|---|---|---|---|---|---|---|---|
| Oracle Cloud VM (arm64, a private address behind the cloud's NAT) | 2026-11-05 | Rust and JS doctors (`c1538e6`, `96ca434`), default servers | `endpoint-independent`, port preserved (4 of 4 STUN answers, 13–22 ms) | not measured | no route | `unavailable` | 3 of 3 `reachable`: open in 0.5–1.1 s, round trip 232–335 ms | `silent` |
| Jio mobile data (carrier-grade NAT) | pending | | | | | | | |
| Fusionnet home broadband | pending | | | | | | | |
| A phone hotspot | pending | | | | | | | |
| Home Wi-Fi | pending | | | | | | | |

The VM is a server in a data centre, not a home or mobile network: it shows the doctor working, not how peers at
home fare. The pending rows are run on the networks themselves.

## Earlier field tests

Each of these is written up where it applies:

- **October 2026,** an Android phone on mobile data and a laptop on its hotspot, in Chrome: linked directly in 1.2 s
  with microphone permission on both pages, and over the signaling relays without it
  ([privacy](../guides/privacy.md#javascript)).
- **October 2026,** a Windows PC behind a symmetric carrier NAT and an Android phone on mobile data: Net Lab's
  birthday punch formed the channel in 11.1 s ([NAT traversal](../concepts/nat-traversal.md#in-browsers)).
- **3 November 2026,** two Android phones in Chrome on Jio mobile data: linked directly over IPv6 1.2 s after
  joining; both carrier NATs mapped as cones ([NAT traversal](../concepts/nat-traversal.md#ipv6-dual-stack)).

## Adding a result

Run the doctor on the network with `--redact`, then add a row with the date, the doctor's version and options, and
the report's kinds and timings. Leave out addresses and anything else that names the network's subscriber. A
`room <name>` run between two networks belongs in a row of its own, with both ends named.
