# Peerwire documentation

Peerwire is peer-to-peer networking for games, whiteboards, meetings and chat. Apps that share a room name or an
invite find each other, punch through their NATs and talk over an end-to-end encrypted link, with no server to run.
These pages cover how to use it from Rust, JavaScript, React, Godot and C, how it works inside, and where the project
stands.

New here? Start with the [introduction](getting-started/introduction.md), then the
[quick start](getting-started/quick-start.md).

## Get started

- [Introduction](getting-started/introduction.md): what Peerwire is, what it's for, and how it works in one page.
- [Installation](getting-started/installation.md): add Peerwire to a Rust, JavaScript, React, Godot or C project.
- [Quick start](getting-started/quick-start.md): two peers chatting, on each platform, in a few lines.

## Guides

- [Rooms and invites](guides/rooms.md): the two ways peers meet, and what a room name protects.
- [Sending messages](guides/messaging.md): the three lanes, message sizes, events and the poll loop.
- [Shared documents](guides/shared-documents.md): maps, sets and logs every member holds, with catch-up for late
  joiners.
- [Calls](guides/calls.md): audio and video between room members in the browser.
- [Offline messages](guides/offline-messages.md): sealed messages to one peer, left on relays while it's away.
- [React](guides/react.md): hooks for rooms, members, chat and shared state.
- [Godot 4](guides/godot.md): Godot's own multiplayer API (`@rpc`, synchronizers, spawners) over Peerwire.
- [LAN discovery](guides/lan-discovery.md): rooms on a local network with no internet, on PCs and ESP32.
- [Privacy](guides/privacy.md): what peers learn about each other's addresses, and the modes that limit it.
- [Choosing paths](guides/connection-paths.md): which paths links take and in what order, your own STUN, TURN and
  relays, changed live.
- [Always-on peer](guides/always-on-peer.md): a peer that stays online to relay, with Docker or without.
- [ESP32](guides/esp32.md): the microcontroller prototype, and how to run it in an emulator.

## Concepts

- [Architecture](concepts/architecture.md): the sans-IO core, the hosts that drive it, and the crates around it.
- [NAT traversal](concepts/nat-traversal.md): how peers behind NATs find and reach each other, and the fallbacks.
- [Protocol](concepts/protocol.md): identity, the handshake, the wire format, lanes and invites.
- [Security model](concepts/security-model.md): who can see what, and what nobody in the path can do.
- [Scaling](concepts/scaling.md): islands and super-peers for rooms bigger than a mesh.

## Reference

- [JavaScript API](reference/javascript.md): `@peerwire/js`, from `joinRoom` to the wasm module underneath.
- [React API](reference/react.md): `@peerwire/react` hooks and components.
- [Rust crates](reference/rust.md): the crates, their main types, features and examples.
- [C ABI](reference/c-abi.md): the `pw_*` functions behind the wasm module and native bindings.

## Project

- [Status and roadmap](project/status.md): what works, what's tested, and what's next.
- [Performance](project/performance.md): measured and theoretical numbers, and how to reproduce each.
- [Comparison](project/comparison.md): Peerwire next to other peer-to-peer libraries.
- [Testing](project/testing.md): how Peerwire is tested, and the gate every change passes.
- [Threat model](project/threat-model.md): what Peerwire defends against, and every review finding with its status.
- [Offline store design](project/offline-store-design.md): keeping a room's data while nobody is online.
- [Changelog](../CHANGELOG.md): what changed in each release.
- [Contributing](../CONTRIBUTING.md): how to build, test and send a change.
- [Security policy](../SECURITY.md): how to report a vulnerability.
