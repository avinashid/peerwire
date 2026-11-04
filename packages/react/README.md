# @peerwire/react

React hooks for [Peerwire](https://github.com/avinashid/peerwire) rooms: join a peer-to-peer room, list its members,
chat, and share state across the room, with no server of your own. Written in TypeScript; the types ship with the
package.

```sh
npm install @peerwire/react @peerwire/js
```

`react` (18 or later) and `@peerwire/js` are peer dependencies. The hooks are a thin layer over `@peerwire/js`, whose
core is a WebAssembly module of 356,584 bytes with no imports (as `packages/js/build.sh` builds it on main).

## A shared counter in a dozen lines

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

Open it in two tabs: the counter is the same in both. `signaling: 'nostr'` (a `RoomProvider` prop, like every
`joinRoom` option) reaches other devices through public Nostr relays. Use `randomRoomId()` for a room only the
people you give its id can join.

## Highlights

- **`<RoomProvider>`** joins a room while mounted, StrictMode-safe, and renders on a server too (`'use client'`;
  rooms are joined in effects).
- **Members, presence and chat:** `usePeers`, `usePresence`, `useMessages`, `useRoomEvent`.
- **Shared state:** `useSharedState`, `useState` for the whole room, with late joiners caught up; and `usePeerState`
  for each member's cursor or position.
- **Checked member data:** every hook takes an `accept` check that keeps out values of the wrong type and gives the
  hook its TypeScript type, so a member can't crash your render with an object where you expect text.

## Docs

- [React guide](https://avinashid.github.io/peerwire/docs/guides/react.html): the hooks in use, checking what
  members send, shared documents and calls in React, Next.js
- [React API reference](https://avinashid.github.io/peerwire/docs/reference/react.html): every hook, option and limit
- [JavaScript API reference](https://avinashid.github.io/peerwire/docs/reference/javascript.html): the rooms underneath
- [All the docs](https://avinashid.github.io/peerwire/docs/README.html)

## Example

`example/` in this package is a Vite + React + TypeScript app with presence, members, a shared counter, chat and live
cursors: run `npx vite example` in `packages/react` and open the printed URL in two tabs.

## License

MIT OR Apache-2.0, at your option.
