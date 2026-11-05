// The lock against the MSRV (NB-110): every crate a published crate builds with, as Cargo.lock pins it, declares a
// rust-version no newer than that crate's own. CI's msrv job builds them on that Rust (cargo check --locked
// --all-features); the gate has no such toolchain, so it compares the declarations instead. From each workspace
// member with a rust-version (the published ones; bindings/godot has none), it follows normal and build
// dependencies, every feature on, for every platform; dev-dependencies are left out, as neither cargo check nor a
// user builds them. A crate that declares no rust-version passes: only a build can tell.
// node scripts/rust-version.mjs

import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const meta = JSON.parse(execFileSync('cargo', ['metadata', '--locked', '--all-features', '--format-version', '1'], {
  cwd: root,
  maxBuffer: 1 << 28,
  stdio: ['ignore', 'pipe', 'inherit'],
}));

const parse = (v) => v.split('.').map(Number);
const newer = (a, b) => {
  const [x, y] = [parse(a), parse(b)];
  for (let i = 0; i < 3; i++) if ((x[i] || 0) !== (y[i] || 0)) return (x[i] || 0) > (y[i] || 0);
  return false;
};

const pkgs = new Map(meta.packages.map((p) => [p.id, p]));
const deps = new Map(meta.resolve.nodes.map((n) => [
  n.id,
  n.deps.filter((d) => d.dep_kinds.some((k) => k.kind !== 'dev')).map((d) => d.pkg),
]));
const name = (id) => `${pkgs.get(id).name} ${pkgs.get(id).version}`;

const over = new Map();
const reached = new Set();
let members = 0, top = null;
for (const id of meta.workspace_members) {
  const msrv = pkgs.get(id).rust_version;
  if (!msrv) continue;
  members++;
  const from = new Map([[id, null]]);
  const queue = [id];
  while (queue.length) {
    const at = queue.shift();
    const rv = pkgs.get(at).rust_version;
    if (rv && newer(rv, msrv) && !over.has(at)) {
      const chain = [];
      for (let p = at; p; p = from.get(p)) chain.unshift(name(p));
      over.set(at, `${name(at)} declares rust-version ${rv}, over ${msrv}: ${chain.join(' -> ')}`);
    }
    if (rv && pkgs.get(at).source && (!top || newer(rv, pkgs.get(top).rust_version))) top = at;
    for (const d of deps.get(at) || []) {
      if (!from.has(d)) {
        from.set(d, at);
        queue.push(d);
      }
    }
  }
  for (const p of from.keys()) reached.add(p);
}

if (!members) {
  console.error('no workspace member declares a rust-version');
  process.exit(1);
}
for (const line of over.values()) console.error(line);
const most = top ? `; the newest a dependency asks for: ${pkgs.get(top).rust_version} (${name(top)})` : '';
console.log(`${members} members with a rust-version, ${reached.size} crates they build with${most}`);
process.exit(over.size ? 1 : 0);
