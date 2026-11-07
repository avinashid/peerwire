# Security policy

Peerwire is pre-1.0 research-grade software and has **not** had an external audit. Don't rely on it yet to protect
high-value data.

## Reporting a vulnerability

Please report vulnerabilities privately, through GitHub's "Report a vulnerability" (Security advisories) on this
repository. Don't open a public issue. We aim to acknowledge within 7 days.

## Scope

- the `peerwire-core` handshake, wire format, relaying and NAT traversal;
- the C ABI (`peerwire-ffi`) and the JavaScript glue;
- the Bluetooth LE transport (`peerwire` features `ble` and `ble-bluez`, and the ESP32's in `peerwire-esp`);
- the signaling adapters.

## Threat model

See [docs/project/threat-model.md](docs/project/threat-model.md) for what Peerwire defends against,
its trust boundaries, and the standing findings.

## What Peerwire gives you today

- **Authenticated, encrypted links.** A `PeerId` is an Ed25519 public key; links
  run Noise IK keyed by that identity, so no signaling relay, peer relay or NAT
  can impersonate a peer or read link traffic. Signaling payloads (which carry IP
  candidates) are sealed to the recipient PeerId.
- **Replay- and flood-resistant data path.** A sliding nonce window drops replayed
  data packets; half-open handshakes and peer relaying are capped and
  rate-limited.
- **Handshake floods from spoofed addresses.** From three quarters of the
  half-open cap, a handshake from a UDP address takes no state until it echoes a
  cookie sent to that address, and each IPv4 address or IPv6 /64 holds at most 4
  half-open handshakes (SEC-4).

## What it does not give you yet

- **Signaling metadata privacy.** On the Nostr adapter, anyone who knows a room
  name can see its members' PeerIds, and anyone who knows your PeerId can probe
  for your IP candidates. Treat room names as secrets and don't publish PeerIds
  you want kept private. ([threat model](docs/project/threat-model.md) SEC-2, SEC-3, SEC-5.) On `LanSignaling`,
  every device on the local network sees that you run Peerwire, your rooms' topics,
  your inbox topic and your IP and MAC addresses, and one that knows two members'
  PeerIds can keep them from linking (NB64-6).
- **Privacy and availability within Bluetooth range.** With a radio (`ble`), any device
  in range sees that a Peerwire peer is near and the sizes and timing of its
  connections, and can jam or cut them; a link over Bluetooth proves nothing about
  distance. On BlueZ 5.87 without upstream commit `82af2beaf`, `bluetoothd` crashes on a
  UUID discovery filter, which the Linux driver set before NB-112 and any local program
  can set: use a BlueZ with that commit ([threat model](docs/project/threat-model.md),
  NB-110).
- **Connection-setup DoS resistance** against an attacker with many real
  addresses: 256 IPv4 addresses or IPv6 /64s (one IPv6 /56, which many ISPs and
  hosts give a single customer), at 4 half-open handshakes each, fill the quarter
  of the half-open table left above the mark (SEC-4).
- **In-session forward secrecy refresh:** session keys are not rotated until a
  reconnect (SEC-7).

The first review (NB-013, commit `cf9cbb0`) found no issues in the core wire,
invite and STUN parsers, the relay gating, or the handshake authentication. It did
find a memory-safety bug at the JS/FFI boundary (SEC-1) and the signaling-privacy
and DoS items above; all are tracked as NB tasks. SEC-1 is fixed (NB-021),
room-key proofs (NB-031, NB-037) stop room offers from strangers revealing your
addresses (SEC-3, for rooms), and handshake cookies (NB-024) keep spoofed
handshakes from filling the half-open table (SEC-4). The [threat model](docs/project/threat-model.md) lists each
finding's status.
