// The site's landing page (scripts/site/build.mjs): a hero with the install line and code in four languages (tabs
// of CSS radio buttons, no script), the features, a strip of numbers, the live demos, the project's status and a
// footer. The copy is the COPY object below; the wasm's size is measured from the build, not written down.

import { escapeHtml as esc, render } from './markdown.mjs';
import { head, topBar, footer, linker, REPO_URL, NPM_URL } from './layout.mjs';

const COPY = {
  title: 'Peerwire: a direct wire between peers',
  description: 'Peer-to-peer networking for games, whiteboards, meetings and chat. Encrypted, NAT-traversing and ' +
    'real-time, with no server to run and no relay to pay for.',
  heading: 'Peerwire',
  tagline: 'A direct wire between peers.',
  lead: 'Peer-to-peer networking for games, whiteboards, meetings and chat. Encrypted, NAT-traversing and real-time, ' +
    'with no server to run and no relay to pay for.',
  install: 'npm install @peerwire/js',
  start: ['Get started', 'docs/getting-started/introduction.html'],
  tabs: [
    {
      key: 'js', name: 'JavaScript', lang: 'js', code: `import { joinRoom } from '@peerwire/js';

const room = await joinRoom({
  appId: 'my-app',
  roomId: 'lobby',
  signaling: 'nostr',
});
room.on('peer-join', (peer) => console.log('joined:', peer.slice(0, 6)));
room.on('message', (data, peer) => console.log(peer.slice(0, 6), data));
room.send('hello');`,
    },
    {
      // packages/react/README.md's counter, "A shared counter in a dozen lines".
      key: 'react', name: 'React', lang: 'tsx', code: `import { RoomProvider, usePeers, useSharedState } from '@peerwire/react';

const named = (p: unknown): p is { name: string } =>
  typeof (p as { name?: unknown })?.name === 'string';

export const App = () => (
  <RoomProvider appId="my-app" roomId="lobby" presence={{ name: 'Ana' }}>
    <Counter />
  </RoomProvider>
);

function Counter() {
  const peers = usePeers({ accept: named }); // members with a text name
  const [count, setCount] = useSharedState('count', 0);
  const names = peers.map((p) => p.presence.name).join(', ');
  const add = () => setCount((c) => c + 1);
  return <button onClick={add}>{count} (with {names})</button>;
}`,
    },
    {
      key: 'rust', name: 'Rust', lang: 'rust', code: `use peerwire::{AsyncNode, Event, Lane, Node, NostrSignaling};

let node = Node::builder()
    .signaling(NostrSignaling::default())
    .bind("[::]:0")?;
let mut node = AsyncNode::from_node(node)?;
node.join_room("my-app", "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4");
loop {
    match node.next_event().await {
        Event::Connected { peer, .. } => {
            node.send(&peer, Lane::Reliable, b"hello")?;
        }
        Event::Message { peer, data, .. } => {
            println!("{peer}: {}", String::from_utf8_lossy(&data));
        }
        _ => {}
    }
}`,
    },
    {
      key: 'godot', name: 'Godot', lang: 'gdscript', code: `var peer := PeerwireMultiplayerPeer.new()

# on the host:
peer.host_room("my-game", "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4")

# on each player:
# peer.join_room("my-game", "9f3c1e7a2b8d4f60a1c5e9b7d3f2a8c4")

multiplayer.multiplayer_peer = peer`,
    },
  ],
  featuresTitle: 'Everything between two peers, nothing in the middle',
  // [icon, title, text]; the text is markdown (code spans); {wasm} is the built wasm's size, "~350 KB".
  features: [
    ['server', 'No backend', 'Peers meet by room name or invite over untrusted signaling: public Nostr relays, an ' +
      'invite link or QR code, or LAN multicast. There\'s no signaling server, TURN server or backend of yours.'],
    ['route', 'Through NATs', 'STUN, simultaneous open, port prediction, and birthday and lockstep punching. When no ' +
      'path punches, a peer both sides know relays the ciphertext. In simulation, 59 of 64 NAT pairings connect directly.'],
    ['lock', 'Encrypted by identity', 'A PeerId is an Ed25519 public key, and every link is a one-round-trip Noise IK ' +
      'session. Relays, NATs and peers in the path see only ciphertext.'],
    ['bolt', 'Real-time lanes', 'Unreliable for state, reliable for events (ordered, up to 1 MiB), and urgent for ' +
      'inputs (ahead of the queue, sent twice). Selective ACKs and delay-based congestion control keep p99 latency low.'],
    ['doc', 'Shared state', '`room.doc()` gives maps, sets and logs that every member holds and that converge, with ' +
      'catch-up for late joiners. Rust, Godot and C use the same frames.'],
    ['layers', 'Runs everywhere', 'One sans-IO Rust core runs natively, as a {wasm} WebAssembly module with no imports ' +
      'in browsers and Node, behind a C ABI, in Godot 4, and as a prototype on ESP32.'],
  ],
  // [value, label]
  numbers: [
    ['1 RTT', 'handshake'],
    ['{wasm}', 'wasm, 0 imports'],
    ['59 / 64', 'NAT pairings direct (simulated)'],
    ['79 ms', 'p99 lag, 512 players (simulated)'],
  ],
  numbersNote: ['Measured on a 4-core arm64 machine and in the deterministic simulator.', 'How each is measured',
    'docs/project/performance.html'],
  demosTitle: 'Live demos',
  demosLead: 'Each runs in your browser, and none needs a server of ours. Open one in two tabs, or on two devices ' +
    'where it says so.',
  status: ['Early development (v0.x): APIs still change. Tested in a NAT simulator, on loopback and LANs, and over ' +
    'public relays; not yet on real mobile carrier-grade NATs.', 'Status and roadmap', 'docs/project/status.html'],
  footer: [
    ['Learn', [['Docs', 'docs/index.html'], ['Demos', 'index.html#demos'], ['Changelog', 'CHANGELOG.html'],
      ['Security', 'SECURITY.html']]],
    ['npm', [['@peerwire/js', NPM_URL], ['@peerwire/react', 'https://www.npmjs.com/package/@peerwire/react']]],
    ['Source', [['GitHub', REPO_URL]]],
  ],
};

// Line icons, 24 × 24, drawn with currentColor.
const ICONS = {
  server: '<rect x="4" y="4" width="16" height="6.5" rx="2"/><rect x="4" y="13.5" width="16" height="6.5" rx="2"/>' +
    '<path d="M3 21 21 3"/>',
  lock: '<rect x="5" y="10.5" width="14" height="10" rx="2.5"/><path d="M8.5 10.5V8a3.5 3.5 0 0 1 7 0v2.5"/>' +
    '<path d="M12 14.5v2"/>',
  route: '<circle cx="6" cy="18" r="2.5"/><circle cx="18" cy="6" r="2.5"/>' +
    '<path d="M8.5 18H15a3 3 0 0 0 0-6H9a3 3 0 0 1 0-6h6.5"/>',
  bolt: '<path d="M13.5 3 5.5 13.5H12L10.5 21l8-10.5H12z"/>',
  layers: '<path d="m12 3.5 8.5 4.5-8.5 4.5L3.5 8z"/><path d="m3.5 12 8.5 4.5 8.5-4.5"/><path d="m3.5 16 8.5 4.5 8.5-4.5"/>',
  doc: '<path d="M6.5 3h7l4.5 4.5V21h-11.5z"/><path d="M13.5 3v4.5H18"/><path d="M9.5 12.5h5M9.5 16h5"/>',
};
const icon = (name) => `<svg class="icon" viewBox="0 0 24 24" width="24" height="24" fill="none" stroke="currentColor" ` +
  `stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`;

// demos: build.mjs's DEMOS; sizes: { wasm, gzip }, the wasm's in KB; highlight(code, lang): HTML.
export function landingPage({ id, commit, demos, sizes, highlight }) {
  const href = linker('index.html');
  const c = COPY;
  const fill = (s) => s.replace(/\{wasm\}/g, `~${Math.round(sizes.wasm / 10) * 10} KB`);
  const tabs = `<div class="tabs">
${c.tabs.map((t, k) => `<input type="radio" name="code" id="tab-${t.key}"${k ? '' : ' checked'}>`).join('\n')}
<div class="tab-bar">${c.tabs.map((t) => `<label for="tab-${t.key}">${esc(t.name)}</label>`).join('')}</div>
${c.tabs.map((t) => `<pre class="tab tab-${t.key}"><code class="language-${t.lang}">${highlight(t.code, t.lang)}</code></pre>`)
    .join('\n')}
</div>`;
  const features = c.features.map(([ic, title, text]) => `<li class="feature">${icon(ic)}<h3>${esc(title)}</h3>` +
    `${render(fill(text)).html}</li>`).join('\n');
  const numbers = c.numbers.map(([value, label]) => `<li><strong>${esc(fill(value))}</strong>` +
    `<span>${esc(fill(label))}</span></li>`).join('\n');
  const demoCards = demos.map((d) => `<li class="demo"><h3><a href="${href(`demo/${d.page}`)}">${esc(d.name)}</a></h3>
<p>${esc(d.blurb)}</p>
<p class="how">${esc(d.how)}${(d.extra ?? []).map(([to, text]) => ` <a href="${href(`demo/${to}`)}">Open the ` +
    `${esc(text)}</a>.`).join('')}</p></li>`).join('\n');
  const columns = c.footer.map(([name, links]) => `<div><p>${esc(name)}</p><ul>${links.map(([text, to]) =>
    `<li><a href="${/^https:/.test(to) ? to : href(to)}">${esc(text)}</a></li>`).join('')}</ul></div>`).join('\n');
  return `${head({ href, id, title: c.title, description: c.description })}
<body class="home">
${topBar({ href, id, on: 'home' })}
<main>
<section class="hero">
<div class="hero-in">
<div class="hero-text">
<h1>${esc(c.heading)}</h1>
<p class="tagline">${esc(c.tagline)}</p>
<p class="lead">${esc(c.lead)}</p>
<p class="install"><span aria-hidden="true">$</span><code>${esc(c.install)}</code></p>
<p class="cta"><a class="button primary" href="${href(c.start[1])}">${esc(c.start[0])}</a>` +
    `<a class="button" href="${REPO_URL}">GitHub</a></p>
</div>
<div class="hero-code">
${tabs}
</div>
</div>
</section>
<section class="band">
<h2 id="features">${esc(c.featuresTitle)}</h2>
<ul class="features">
${features}
</ul>
</section>
<section class="numbers" aria-label="In numbers">
<ul>
${numbers}
</ul>
<p>${esc(c.numbersNote[0])} <a href="${href(c.numbersNote[2])}">${esc(c.numbersNote[1])}</a></p>
</section>
<section class="band" id="demos">
<h2>${esc(c.demosTitle)}</h2>
<p class="band-lead">${esc(c.demosLead)}</p>
<ul class="demos">
${demoCards}
</ul>
</section>
<section class="status">
<p><span class="dot" aria-hidden="true"></span><span>${esc(c.status[0])} <a href="${href(c.status[2])}">${esc(c.status[1])}</a></span></p>
</section>
</main>
${footer({ commit, extra: `<div class="foot-cols">\n<div class="foot-brand"><img src="${href(`assets/${id}/img/favicon.svg`)}" ` +
    `alt="" width="28" height="28"><p>A direct wire between peers.</p></div>\n${columns}\n</div>\n` })}
</body>
</html>
`;
}
