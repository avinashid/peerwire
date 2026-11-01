# React API

`@peerwire/react` is a thin layer of React hooks over [`@peerwire/js`](javascript.md): join a room while a component
is mounted, list its members, chat, and share state across the room. It is written in TypeScript and ships its types.
This page lists every export, checked against [`packages/react/src`](../../packages/react/src/index.ts). How to use
them: the [React guide](../guides/react.md).

```sh
npm install @peerwire/react @peerwire/js
```

| | |
|---|---|
| Version | 0.1.1 (`packages/react/package.json`) |
| Peer dependencies | `react` 18 or later, `@peerwire/js` 0.1.0 or later |
| Module | ES module marked `'use client'`; rooms are joined in effects only, so it renders on a server too |

**Exports.** Components and hooks: [`RoomProvider`](#roomprovider), [`useRoom`](#useroom),
[`usePeerwireRoom`](#usepeerwireroom), [`usePeers`](#usepeers), [`usePresence`](#usepresence),
[`useMessages`](#usemessages), [`useRoomEvent`](#useroomevent), [`useSharedState`](#usesharedstate),
[`usePeerState`](#usepeerstate). Also [`randomRoomId`](javascript.md#randomroomid), the [limits](#limits) as
constants, and [types](#types).

## Which room a hook uses

Every hook but `useRoom` takes an options object last. Its `room` (a `Room`, or `null` for none) works on that room
instead of the nearest provider's:

```ts
interface RoomOption { room?: Room | null }
```

With no `room` and no provider, a hook has no room: it returns its empty value (`[]`, `undefined`, `initial`) and its
setters do nothing.

## RoomProvider

```tsx
<RoomProvider appId="my-app" roomId="lobby" presence={{ name: 'Ana' }}>{children}</RoomProvider>
```

```ts
function RoomProvider(props: JoinRoomOptions & { children?: ReactNode }): ReactElement
```

Joins the room its props name while it is mounted, for the hooks in the components below it. Its props are
[`joinRoom`'s options](javascript.md#joinroom). Changing `presence` sends it to the room. Changing any other option
leaves and joins again: options count by their JSON, functions and classes (a signalling factory,
`RTCPeerConnection`) don't count and are taken from the render that joins, and bytes count by content up to 1 KiB (a
`secret`) and by identity above that (a `wasm` module).

## useRoom

```ts
function useRoom(): RoomState
```

The nearest `RoomProvider`'s room, status and error. Throws outside a `RoomProvider`.

```ts
interface RoomState {
  room: Room | null;     // the joined room, or null until it is joined
  status: RoomStatus;    // 'idle' | 'joining' | 'joined' | 'error'
  error: Error | null;   // why joining failed, with status 'error'
}
```

`'idle'` means no options, so no room (`usePeerwireRoom(null)`); a provider is `'joining'`, then `'joined'` or
`'error'`.

## usePeerwireRoom

```ts
function usePeerwireRoom(options: JoinRoomOptions | null | undefined): RoomState
```

What `RoomProvider` uses: joins a room while the component is mounted, with no provider, and leaves on unmount or
when an option changes (all but `presence`, which is sent instead). `null` stays out of any room. StrictMode-safe: a
join that finishes after its effect was cleaned up is left at once. A presence the room refuses (over 1 MiB as JSON,
or not JSON) fails the join with status `'error'`.

```tsx
const { room, status } = usePeerwireRoom(ready ? { appId: 'my-app', roomId } : null);
const peers = usePeers({ room });
```

## usePeers

```ts
function usePeers<P = unknown>(options?: { accept?: Accept<P> } & RoomOption): readonly Peer<P>[]
```

The other members, oldest first, as `{ id, presence }`. With `accept`, a member whose presence fails it is left out,
so `presence` always has the type `accept` checks.

## usePresence

```ts
function usePresence<P = Presence>(options?: RoomOption): [P | undefined, SetState<P>]
```

Our presence and a setter, like `useState`: every member sees the change. The setter takes a value or a function of
the previous one.

## useMessages

```ts
function useMessages<T = unknown>(options?: UseMessagesOptions<T>): UseMessagesResult<T>
```

The room's messages as a list, for a chat or an event log.

| Option | Default | |
|---|---|---|
| `limit` | 200 | How many to keep, newest last (at least 1). A new limit applies from the next message. |
| `accept` | none | `(data, info) => boolean` or a type guard: keep only messages that pass; `info` is `{ lane, kind }`. Without it, every message is kept as sent. |
| `room` | the provider's | See [Which room a hook uses](#which-room-a-hook-uses). |

| Result | |
|---|---|
| `messages` | `readonly Message<T>[]`: messages since this component mounted (or the room joined), oldest first, at most `limit`. A new room empties it. |
| `send(data, { to, lane }?)` | `room.send`, and the message is added with `mine: true`. Returns how many peers it went to (0 with no room). |
| `clear()` | Empties the list. |

```ts
interface Message<T = unknown> {
  id: number;          // unique within the page, for React keys
  peer: PeerId;        // who sent it: a member, or room.self for ours
  data: T;             // as sent; from another member it is untrusted input
  lane: 'reliable' | 'unreliable';
  kind: 'text' | 'bytes' | 'json';
  at: number;          // Date.now() when it arrived or was sent
  mine: boolean;       // true for a message we sent with send
}
```

It does not fetch history: a room has none. For a history, use a [shared document](../guides/shared-documents.md)'s
log.

## useRoomEvent

```ts
function useRoomEvent<E extends keyof RoomEvents>(event: E, handler: RoomEvents[E], options?: RoomOption): void
```

Calls `handler` for each `'peer-join'`, `'peer-leave'`, `'presence'` or `'message'` event, always the latest handler
passed, so it can be an inline function. `'message'` skips the messages this package sends for shared and per-peer
state. Arguments come from other members as sent: check them.

## useSharedState

```ts
function useSharedState<T extends string | number | boolean>(key: string, initial: T, options?: RoomOption): [T, SetState<T>]
function useSharedState<T>(key: string, initial: T, options: { accept: Accept<T> } & RoomOption): [T, SetState<T>]
```

State shared by the whole room, like `useState`: every member that uses `key` sees the same value, and a member that
joins later catches up. Until the room is joined the value is `initial`, and setting it does nothing.

- **Who wins.** Each write carries a clock and the latest wins, then the higher member id. The clock is the writer's
  time in milliseconds, or one past the value it replaces when that is later, so a member whose clock is behind still
  writes over the current value. A write stamped more than 5 minutes ahead of a member's clock (`MAX_CLOCK_DRIFT`) is
  ignored by that member.
- **`accept`** may be left out only when `initial` is a string, number or boolean: then a value must have its
  `typeof` (and `0` holds any number). For an array, an object or `null`, `accept` is required: TypeScript says so,
  and the hook throws `TypeError` without it. A value that fails `accept` reads as `initial`, on every member alike.
- **Setting** a key over 256 characters, or a value over 64 KiB as JSON or nested over 64 deep, throws `RangeError`.
  When shared state is full, a write that doesn't fit is dropped with a console warning; the setter doesn't throw.

## usePeerState

```ts
function usePeerState<T = unknown>(key: string, options?: { accept?: Accept<T> } & RoomOption):
  [Readonly<Record<PeerId, T>>, (value: T) => void]
```

Each other member's latest value for `key`, such as a cursor or a controller's input, and a setter for ours (ours
isn't in the record). Values go on the unreliable lane as they change, then once on the reliable lane when they have
been still for 250 ms (`SETTLE_MS`), so the final value always arrives. A value over one datagram (about 1 KB) goes
reliably instead. A member that leaves drops out; one that joins gets everyone's current value. With `accept`, a
value that fails it is left out. Our own values past the per-peer limits throw `RangeError`.

## Limits

Exported as constants, from `packages/react/src/store.ts`. A member's value past a limit is ignored.

| Constant | Value | |
|---|---|---|
| `MAX_KEY_LENGTH` | 256 | The longest key, in UTF-16 code units. |
| `MAX_VALUE_LENGTH` | 65536 | The largest value, as JSON text. |
| `MAX_VALUE_DEPTH` | 64 | How deeply a value's arrays and objects nest. |
| `MAX_KEYS` | 1024 | Shared keys a room's store keeps. |
| `MAX_STORE_LENGTH` | 1048576 | Shared values in all, as JSON text (1 MiB). |
| `MAX_PEER_KEYS` | 64 | Per-peer keys each member can have, ours included. |
| `MAX_PEER_LENGTH` | 262144 | Each member's per-peer values in all, as JSON text (256 KiB), ours included. |
| `MAX_CLOCK_DRIFT` | 300000 | How far ahead of ours another member's clock can be, in ms (5 minutes). |
| `SETTLE_MS` | 250 | How long a per-peer value is still before it goes once more, reliably. |

- **Full shared state.** A member's new key is ignored, unless a mounted hook reads it: then it takes the room of the
  oldest keys no hook reads. So does our own write. A key no mounted hook reads may be dropped when shared state is
  full: a hook that mounts for it later reads `initial` until the key is written again. Shared values stay after
  their writer leaves; a member that leaves frees its per-peer values.
- **Catching up.** A member that joins gets each member's shared and per-peer state, once: first a list of the
  versions each side holds, then the values the other lacks. So a join costs each member about its state, up to 1 MiB
  and its 256 KiB of per-peer values, which is slow over a relayed link. A store answers at most 4 catch-up requests
  from a member while it stays, and never sends it the same value twice.
- **On the wire.** Shared and per-peer state travel as messages with a `$peerwire` field. The hooks hide them, but
  code that calls `room.on('message')` directly sees them.
- **Trust.** Members are trusted alike, as in the room itself: any member can write any shared key. A check keeps
  out values of the wrong type, not a member who writes well-typed nonsense.

## Types

```ts
type RoomStatus = 'idle' | 'joining' | 'joined' | 'error';
type SetState<T> = (next: T | ((prev: T) => T)) => void;
type Accept<T> = ((value: unknown) => value is T) | ((value: unknown) => boolean);
interface Peer<P = unknown> { id: PeerId; presence: P }
```

`Accept<T>` is a check of a value another member sent, which TypeScript also takes as the value's type. A check that
throws counts as failed. It should be a plain function of the value; when it changes, the new one applies from the
next change in the room.

Re-exported from `@peerwire/js`: `JoinRoomOptions`, `MessageInfo`, `PeerId`, `Presence`, `Room`, `RoomEvents`,
`SendOptions`. Also exported: `RoomState`, `RoomProviderProps`, `RoomOption`, `Message`, `UseMessagesOptions`,
`UseMessagesResult`.

## Not in the hooks

`room.doc()` (shared documents), `room.call()` and `openInbox` have no hooks yet: use them on the `room` from
`useRoom()` in an effect, as the [React guide](../guides/react.md#shared-documents-and-calls) shows.

## Example and tests

- `packages/react/example/`: a Vite + React + TypeScript app with presence, members, a shared counter, chat and live
  cursors. Run `npx vite example` in `packages/react` and open the printed URL in two tabs. Add `&signal=nostr` to
  reach other devices.
- `npm run build`: `tsc` to `dist/` (JS and `.d.ts`). `npm pack` builds first.
- `npm run typecheck`: the package, the example and `test/types.tsx`, whose `@ts-expect-error` lines must stay
  errors.
- `node test/hooks.mjs`: the hooks in Node, in react-dom over jsdom, with members of one room over BroadcastChannel:
  server rendering, StrictMode with no ghost member, presence, messages and their limit, shared state, per-peer
  values, leaving, switching rooms, a failed join, members that send malformed, flooding or far-ahead state, and
  `accept`. Needs `packages/js/build.sh` first.
- `node test/store.mjs`: the store on fake rooms: the caps and quotas, keys a hook reads pushing out keys none reads,
  the clock and its drift limit, and catching up.
- `node test/example.mjs [screenshot-prefix]`: builds the example with Vite and plays it in three tabs of headless
  Chromium on loopback, with a fourth member outside React that sends the wrong types.
