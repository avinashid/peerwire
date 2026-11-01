# React

[`@peerwire/react`](../reference/react.md) joins a room while a component is mounted and gives you hooks for its
members, messages, shared state and per-peer values, each with a check for what other members send. It is written in
TypeScript. This guide walks through it; the [React reference](../reference/react.md) has every hook and limit.

```sh
npm install @peerwire/react @peerwire/js
```

## A shared counter

```tsx
import { RoomProvider, usePeers, useSharedState } from '@peerwire/react';

const named = (p: unknown): p is { name: string } => typeof (p as { name?: unknown })?.name === 'string';

export function App() {
  return (
    <RoomProvider appId="my-app" roomId="lobby" presence={{ name: 'Ana' }}>
      <Counter />
    </RoomProvider>
  );
}

function Counter() {
  const peers = usePeers({ accept: named }); // members whose presence has a text name
  const [count, setCount] = useSharedState('count', 0); // numbers only, as `initial` is one
  return <button onClick={() => setCount((c) => c + 1)}>{count} (with {peers.map((p) => p.presence.name).join(', ')})</button>;
}
```

Open it in two tabs: the counter is the same in both. Every `joinRoom` option is a `RoomProvider` prop:
`signaling="nostr"` reaches other devices through public Nostr relays, and a `roomId` from `randomRoomId()` makes a
room only the people you give it to can join (see [Rooms and invites](rooms.md)). `useRoom()` gives the room and its
`status` (`'joining'`, `'joined'` or `'error'`) to show while it joins.

## Members and presence

```tsx
const peers = usePeers({ accept: named });     // [{ id, presence }], oldest first
const [me, setMe] = usePresence<{ name: string; typing?: boolean }>();
setMe((p) => ({ ...p!, typing: true }));       // every member sees it
```

Changing the provider's `presence` prop sends it too; changing any other prop leaves the room and joins again. So
pass the options' values (`appId`, `roomId`), not an options object made on each render.

## Chat

```tsx
function Chat() {
  const { messages, send } = useMessages({ accept: (data) => typeof data === 'string', limit: 100 });
  return (
    <>
      <ul>{messages.map((m) => <li key={m.id}>{m.mine ? 'me' : m.peer.slice(0, 6)}: {m.data}</li>)}</ul>
      <button onClick={() => send('hi')}>Say hi</button>
    </>
  );
}
```

`messages` holds what arrived since the component mounted, at most `limit` (200 by default). A room keeps no history:
for one, keep a log in a [shared document](shared-documents.md). For other events, `useRoomEvent('peer-join', fn)`
calls the latest `fn` you pass.

## Shared state and per-peer state

```tsx
const [todos, setTodos] = useSharedState('todos', [] as { text: string }[], { accept: isTodos }); // checks: below
const [cursors, setCursor] = usePeerState('cursor', { accept: isPoint }); // { [peer]: { x, y } }, not ours
return <div onPointerMove={(e) => setCursor({ x: e.clientX, y: e.clientY })}>{/* draw todos and cursors */}</div>;
```

- `useSharedState` is `useState` for the whole room. A write goes to every member, and one who joins later catches up.
  When two members write at once, every member settles on the same one: the latest clock wins, then the higher
  member id.
- `usePeerState` suits values that change many times a second: a pointer, a player's position, a phone used as a
  controller. Values go on the unreliable lane, then once reliably when they have been still for 250 ms, so the final
  one always arrives.
- Keep shared state small: a member that joins is sent each member's state, up to 1 MiB of shared values and 256 KiB
  of per-peer values each, which is slow over a relayed link. The caps are in the
  [reference](../reference/react.md#limits).

## Check what members send

Another member can send any JSON in place of what your types say: an object where you expect a name, a string where
you expect a number. React throws when it renders an object as text, and with no error boundary the whole app
unmounts, which also leaves the room. So give the hooks an `accept` check. A type guard there also gives the hook its
type, with no generic to write:

```tsx
const isPoint = (v: unknown): v is { x: number; y: number } =>
  Number.isFinite((v as any)?.x) && Number.isFinite((v as any)?.y);
const isTodos = (v: unknown): v is { text: string }[] =>
  Array.isArray(v) && v.every((t) => typeof t?.text === 'string');

const { messages } = useMessages({ accept: (data) => typeof data === 'string' }); // Message<string>[]
const [cursors] = usePeerState('cursor', { accept: isPoint });                    // { [peer]: { x, y } }
const [todos, setTodos] = useSharedState('todos', [], { accept: isTodos });       // { text: string }[]
```

- What fails the check is kept out: a message isn't added, a member's value is left out of `usePeerState`, a member
  whose presence fails is left out of `usePeers`, and a shared value that fails reads as `initial`, on every member
  alike.
- Check every field you render, however deep: an array of todos whose `text` is an object throws as surely as an
  object in place of the array.
- `useSharedState` can leave out `accept` only when `initial` is a string, number or boolean: then a value must have
  its `typeof`. For an array, an object or `null`, `accept` is required: TypeScript says so, and the hook throws a
  `TypeError` without it.
- With no check, member data is `unknown` to TypeScript, so rendering it unchecked doesn't compile. A type argument
  (`usePeerState<Point>(key)`) asserts the type without checking it.
- Put an [error boundary](https://react.dev/reference/react/Component#catching-rendering-errors-with-an-error-boundary)
  **inside** `RoomProvider`, around your app. Then a render error that a check missed shows the boundary's fallback
  instead of unmounting the provider, which would leave the room. `packages/react/example/` has one.

A check keeps out values of the wrong type, not a member who writes well-typed nonsense: members are trusted alike,
as in the room itself.

## Shared documents and calls

`room.doc()` and `room.call()` have no hooks yet. Use them on the room from `useRoom()` in an effect, and close what
you open in its cleanup:

```tsx
import { useEffect, useState } from 'react';
import type { DocItem } from '@peerwire/js';
import { useRoom } from '@peerwire/react';

function Strokes() {
  const { room } = useRoom();
  const [strokes, setStrokes] = useState<readonly DocItem[]>([]);
  useEffect(() => {
    if (!room) return;
    const doc = room.doc('board');
    const set = doc.set('strokes');
    const update = () => setStrokes(set.entries()); // the same array until the set changes
    update();
    const off = doc.on('change', update);
    return () => { off(); void doc.close(); };
  }, [room]);
  return <p>{strokes.length} strokes</p>;
}
```

A doc closed and opened again, as a component that remounts does, catches up from the members each time. Check each
item's `value` before you render it, as with messages. See [Shared documents](shared-documents.md) and
[Calls](calls.md).

## Without the provider

```tsx
const { room, status } = usePeerwireRoom(ready ? { appId: 'my-app', roomId } : null); // null: no room
const peers = usePeers({ room });
```

Every hook takes `{ room }` to work on a room other than the nearest provider's.

## Next.js and server rendering

The package is marked `'use client'`, and rooms are joined in effects, so components using it render on a server
(without a room) and join in the browser. Importing it, or `@peerwire/js`, touches no browser globals. In Next.js, use
the hooks in client components.

## The wasm with a bundler

Bundlers find `peerwire_ffi.wasm` next to `@peerwire/js`; Vite does in dev and in a build. To choose the file
yourself, pass its URL as the `wasm` option: `import wasmUrl from '@peerwire/js/peerwire_ffi.wasm?url'` (Vite), then
`<RoomProvider wasm={wasmUrl} ...>`. Root-relative URLs work too, such as `wasm="/peerwire_ffi.wasm"` for a copy in
Vite's `public/`.

## Your own hook

Without `@peerwire/react`, join in an effect and leave in its cleanup, so the room lives as long as the component:

```jsx
import { useEffect, useState } from 'react';
import { joinRoom } from '@peerwire/js';

export function useRoom(appId, roomId) {
  const [room, setRoom] = useState(null);
  const [peers, setPeers] = useState([]);
  const [messages, setMessages] = useState([]);
  const [error, setError] = useState(null);
  useEffect(() => {
    let gone = false, joined = null;
    joinRoom({ appId, roomId }).then((r) => {
      if (gone) return r.leave(); // cleaned up before the join resolved
      joined = r;
      const sync = () => setPeers([...r.peers]);
      r.on('peer-join', sync);
      r.on('peer-leave', sync);
      r.on('message', (data, peer) =>
        typeof data === 'string' && setMessages((m) => [...m.slice(-99), { peer, data }]));
      setRoom(r);
      sync();
    }, (e) => gone || setError(e));
    return () => { gone = true; joined?.leave(); setRoom(null); setPeers([]); setMessages([]); };
  }, [appId, roomId]);
  return { room, peers, messages, error };
}
```

- **StrictMode** mounts, cleans up and mounts again in development, so the first join can resolve after its cleanup
  has run. The `gone` check leaves that room at once; without it the page stays in the room twice and the other
  members see a second peer for as long as the page is open.
- **Dependencies:** list the options' values, not an options object made on each render, or every render leaves and
  joins again. Change presence with `room.setPresence`, not by joining again.
- **Check what a member sent.** The hook keeps strings only, and only the last 100, so a member who floods the room
  can't grow the list without bound.
- **Messages in state** suit a chat. For a game, handle `'message'` in a ref or a store outside React, and render
  from it each frame.
