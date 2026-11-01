// The code samples in the Markdown (NB-070):
//  1. JavaScript and TypeScript parse: ```js blocks and the <script type="module"> of ```html ones with
//     `node --check`; ```ts, tsx and jsx with the TypeScript packages/react installs (syntax only: no types).
//     `ignore` in the info string (```js ignore) skips a block's syntax check, not its imports.
//  2. Every name they import from @peerwire/js or @peerwire/react is one the package exports: packages/js/src/index.js
//     (in TypeScript index.d.ts too) or packages/react/src/index.ts. A subpath is in the package's "exports".
//  3. Every page with a ```rust block is a doc test, so `cargo test` compiles its samples: an include_str! in a doc
//     attribute (README.md and docs/ go in mod doc_samples of crates/peerwire/src/lib.rs). A doc-test page has no
//     bare ``` block, which rustdoc compiles as Rust (```text), and a sample that runs doesn't use NostrSignaling:
//     the tests stay off public relays (```rust,no_run).
// The export lists are read from the source, and checked first against what index.js and the React build export.
// BASELINE exempts pages as they were when this check came in.
// node packages/js/test/docs_samples.mjs [checkout]   (needs packages/react built: npm ci && npm run build there)

import { blocks, files, lineAt, pages, read, root } from './markdown.mjs';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const P = path.posix;
const fileSet = new Set(files);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};

// Pages exempt as they were on 6ba7cd9, each pinned to a hash of its code blocks: an edit to one of them ends the
// exemption, and this fails until its line is deleted. NB-070 lists what each fails.
const BASELINE = {
  // The old README, which NB-066 rewrites: as doc tests, its 4 samples fail.
  'README.md': '7f85bd15672f',
  // A fragment: peerwire-sync's lib.rs doesn't include its README.
  'crates/peerwire-sync/README.md': '8673a29fdd77',
};
const hashOf = (bs) => createHash('sha256').update(JSON.stringify(bs.map((b) => [b.info, b.body]))).digest('hex').slice(0, 12);

const problems = { syntax: [], imports: [], rust: [] };
const problem = (kind, page, line, what) => problems[kind].push({ page, line, what });
const exempt = new Set();
const report = (kind) => {
  problems[kind].sort((a, b) => a.page.localeCompare(b.page) || a.line - b.line);
  for (const p of problems[kind]) console.log(`  ${exempt.has(p.page) ? 'baseline ' : ''}${p.page}:${p.line}: ${p.what}`);
  return problems[kind].every((p) => exempt.has(p.page));
};
// Comments blanked, offsets kept.
const uncomment = (src) => src.replace(/\/\*[\s\S]*?\*\/|(?<=^|\s)\/\/.*$/gm, (c) => c.replace(/[^\n]/g, ' '));

// What a module exports, read from its source: { values, types }. `export *` is followed into relative modules.
function exportsOf(file, seen = new Set()) {
  const out = { values: new Set(), types: new Set() };
  if (seen.has(file)) return out;
  seen.add(file);
  const src = uncomment(read(file));
  const decl = new RegExp(String.raw`\bexport\s+(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?` +
    String.raw`(function\*?|class|const|let|var|enum|namespace|interface|type)\s+([A-Za-z_$][\w$]*)`, 'g');
  for (const [, kind, name] of src.matchAll(decl)) {
    if (kind !== 'interface' && kind !== 'type') out.values.add(name);
    if (kind === 'interface' || kind === 'type' || kind === 'class' || kind === 'enum') out.types.add(name);
  }
  if (/\bexport\s+default\b/.test(src)) out.values.add('default');
  for (const [, type, list] of src.matchAll(/\bexport\s+(type\s+)?\{([^}]*)\}/g)) {
    for (const s of specs(list)) (type || s.type ? out.types : out.values).add(s.as);
  }
  for (const m of src.matchAll(/\bexport\s*\*\s*(?:as\s+([\w$]+)\s+)?from\s*(['"])([^'"\n]+)\2/g)) {
    if (m[1]) {
      out.values.add(m[1]);
      continue;
    }
    const base = P.join(P.dirname(file), m[3]);
    const from = [base, base.replace(/\.js$/, '.ts'), base.replace(/\.js$/, '.d.ts'), `${base}.js`, `${base}.ts`]
      .find((f) => m[3].startsWith('.') && fileSet.has(f));
    if (!from) throw new Error(`${file}: can't follow export * from '${m[3]}'`);
    const inner = exportsOf(from, seen);
    for (const v of inner.values) if (v !== 'default') out.values.add(v);
    for (const t of inner.types) out.types.add(t);
  }
  return out;
}

// `a, b as c, type d` (an import or export list): [{ name, as, type }].
function specs(list) {
  return list.split(',').map((s) => s.trim()).filter(Boolean).map((s) => {
    const type = /^type\s/.test(s);
    const [name, as = name] = s.replace(/^type\s+/, '').split(/\s+as\s+/).map((x) => x.trim());
    return { name, as, type };
  });
}

// The packages: names a sample may import, and the subpaths of "exports".
const packages = {
  '@peerwire/js': { values: exportsOf('packages/js/src/index.js').values, types: exportsOf('packages/js/src/index.d.ts') },
  '@peerwire/react': { ...exportsOf('packages/react/src/index.ts') },
};
packages['@peerwire/js'].types = new Set([...packages['@peerwire/js'].types.values, ...packages['@peerwire/js'].types.types]);
for (const [name, pkg] of Object.entries(packages)) {
  pkg.subpaths = new Set(Object.keys(JSON.parse(read(`packages/${name.split('/')[1]}/package.json`)).exports ?? { '.': 0 }));
}

// 0. The export lists, against the modules themselves.
const differ = (read, runtime) => [...[...read].filter((x) => !runtime.has(x)).map((x) => `-${x}`),
  ...[...runtime].filter((x) => !read.has(x)).map((x) => `+${x}`)].join(' ');
const jsRuntime = new Set(Object.keys(await import(pathToFileURL(path.join(root, 'packages/js/src/index.js')))));
const jsDiff = differ(packages['@peerwire/js'].values, jsRuntime);
check(!jsDiff, `@peerwire/js: the ${jsRuntime.size} exports read from src/index.js are the module's${jsDiff && ` ${jsDiff}`}`);
const reactBuild = path.join(root, 'packages/react/dist/index.js');
const reactRuntime = fs.existsSync(reactBuild) ? new Set(Object.keys(await import(pathToFileURL(reactBuild)))) : null;
const reactDiff = reactRuntime && differ(packages['@peerwire/react'].values, reactRuntime);
check(reactRuntime && !reactDiff, reactRuntime
  ? `@peerwire/react: the ${reactRuntime.size} exports read from src/index.ts are dist/index.js's` +
    (reactDiff ? ` ${reactDiff} (or dist is stale: npm run build in packages/react)` : '')
  : '@peerwire/react: no dist/index.js to check the exports against (npm run build in packages/react)');

// The samples: { page, line (the code's first line), kind, code, ignore }.
const KIND = { js: 'js', javascript: 'js', mjs: 'js', jsx: 'jsx', ts: 'ts', typescript: 'ts', tsx: 'tsx' };
const samples = [];
const pageBlocks = new Map(pages.map((p) => [p, blocks(read(p))]));
const stale = Object.entries(BASELINE).filter(([page, hash]) => {
  if (pageBlocks.has(page) && hashOf(pageBlocks.get(page)) === hash) exempt.add(page);
  return !exempt.has(page);
});
check(stale.length === 0, `BASELINE: ${exempt.size} pages exempt as on 6ba7cd9` + stale.map(([page]) => pageBlocks.has(page)
  ? `; ${page}'s code changed (now ${hashOf(pageBlocks.get(page))}): its samples are checked, delete its line`
  : `; no ${page}: delete its line`).join(''));
for (const [page, bs] of pageBlocks) {
  for (const b of bs) {
    const ignore = b.attrs.includes('ignore');
    if (KIND[b.lang]) samples.push({ page, line: b.line + 1, kind: KIND[b.lang], code: b.body, ignore });
    if (b.lang !== 'html') continue;
    for (const m of b.body.matchAll(/<script\b[^>]*\btype\s*=\s*["']?module\b[^>]*>([\s\S]*?)<\/script>/gi)) {
      const at = m.index + m[0].indexOf('>') + 1;
      samples.push({ page, line: b.line + lineAt(b.body, at), kind: 'js', code: m[1], ignore });
    }
  }
}

// 1. Syntax.
const nodeCheck = (code) => new Promise((resolve) => {
  const p = spawn(process.execPath, ['--check', '--input-type=module', '-'], { stdio: ['pipe', 'ignore', 'pipe'] });
  let err = '';
  p.stderr.on('data', (d) => (err += d));
  p.on('close', (status) => resolve(status === 0 ? null : err));
  p.stdin.end(code);
});
const js = samples.filter((s) => s.kind === 'js' && !s.ignore);
for (let i = 0; i < js.length; i += 4) {
  await Promise.all(js.slice(i, i + 4).map(async (s) => {
    const err = await nodeCheck(s.code);
    if (err === null) return;
    const at = /\[stdin\]:(\d+)/.exec(err);
    const what = /^\w*Error: .*$/m.exec(err)?.[0] ?? err.trim().split('\n').pop();
    problem('syntax', s.page, s.line + (at ? +at[1] - 1 : 0), what);
  }));
}
const typed = samples.filter((s) => s.kind !== 'js' && !s.ignore);
const tsc = path.join(root, 'packages/react/node_modules/typescript/bin/tsc');
if (typed.length && !fs.existsSync(tsc)) {
  problem('syntax', typed[0].page, typed[0].line,
    `no TypeScript to parse ${typed.length} ts/tsx/jsx samples: npm ci in packages/react`);
} else if (typed.length) {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'peerwire-docs-samples-'));
  const names = typed.map((s, i) => `s${i}.${s.kind}`);
  typed.forEach((s, i) => fs.writeFileSync(path.join(tmp, names[i]), s.code));
  const out = await new Promise((resolve) => {
    const p = spawn(process.execPath, [tsc, '--noEmit', '--noCheck', '--pretty', 'false', '--allowJs', '--jsx', 'preserve',
      '--target', 'esnext', '--module', 'esnext', '--moduleDetection', 'force', '--noResolve', '--types', '', ...names],
    { cwd: tmp, stdio: ['ignore', 'pipe', 'pipe'] });
    let text = '';
    p.stdout.on('data', (d) => (text += d));
    p.stderr.on('data', (d) => (text += d));
    p.on('close', (status) => resolve({ status, text }));
  });
  fs.rmSync(tmp, { recursive: true, force: true });
  let mapped = 0;
  for (const m of out.text.matchAll(/^s(\d+)\.\w+\((\d+),\d+\): error (TS\d+): (.*)$/gm)) {
    const s = typed[+m[1]];
    problem('syntax', s.page, s.line + +m[2] - 1, `${m[4]} (${m[3]})`);
    mapped++;
  }
  if (out.status !== 0 && !mapped) problem('syntax', typed[0].page, typed[0].line, `tsc: ${out.text.trim()}`);
}
check(report('syntax'), `${samples.length} JS/TS samples (${samples.filter((s) => s.ignore).length} ignored) parse`);

// 2. Imports.
let imported = 0;
for (const s of samples) {
  const code = uncomment(s.code);
  const typescript = s.kind === 'ts' || s.kind === 'tsx';
  const uses = []; // [offset, specifier, [{ name, type }], namespace]
  const clause = /\bimport\s+(type\s+)?((?:[\w$]+\s*,?\s*)?(?:\*\s*as\s+[\w$]+|\{[^}]*\})?)\s*from\s*(['"])([^'"\n]+)\3/g;
  for (const m of code.matchAll(clause)) {
    const [, type, what] = m;
    const names = [];
    let ns = null;
    const def = /^\s*([\w$]+)/.exec(what);
    if (def) names.push({ name: 'default', type });
    if (/\*\s*as\s+([\w$]+)/.test(what)) ns = /\*\s*as\s+([\w$]+)/.exec(what)[1];
    const list = /\{([^}]*)\}/.exec(what);
    if (list) for (const sp of specs(list[1])) names.push({ name: sp.name, type: type || sp.type });
    uses.push([m.index, m[4], names, ns]);
  }
  for (const m of code.matchAll(/\bexport\s+(type\s+)?\{([^}]*)\}\s*from\s*(['"])([^'"\n]+)\3/g)) {
    uses.push([m.index, m[4], specs(m[2]).map((sp) => ({ name: sp.name, type: m[1] || sp.type })), null]);
  }
  for (const m of code.matchAll(/\b(?:import|export\s*\*(?:\s*as\s+[\w$]+)?\s*from)\s*(['"])([^'"\n]+)\1/g)) {
    uses.push([m.index, m[2], [], null]);
  }
  for (const m of code.matchAll(/(?:\{([^}]*)\}\s*=\s*(?:await\s+)?)?\bimport\s*\(\s*(['"])([^'"\n]+)\2\s*\)/g)) {
    const names = (m[1] ?? '').split(',').map((x) => x.trim().split(/\s*:/)[0]).filter((x) => x && !x.startsWith('...'));
    uses.push([m.index, m[3], names.map((name) => ({ name })), null]);
  }
  for (const [at, spec, names, ns] of uses) {
    const pk = /(?:^|\/|npm:)(@peerwire\/[\w.-]+?)(?:@[^/?#]*)?(\/[^?#]*)?(?:[?#].*)?$/.exec(spec);
    if (!pk) continue;
    imported++;
    const line = s.line + lineAt(code, at) - 1;
    const pkg = packages[pk[1]];
    const sub = `.${(pk[2] ?? '').replace(/\/\+esm$/, '')}`;
    if (!pkg) {
      problem('imports', s.page, line, `'${spec}': there is no ${pk[1]}`);
      continue;
    }
    if (!pkg.subpaths.has(sub)) {
      problem('imports', s.page, line, `'${spec}': ${pk[1]} exports no '${sub}'`);
      continue;
    }
    if (sub !== '.') continue;
    const members = ns ? [...code.matchAll(new RegExp(`(?<![\\w$.])${ns.replace(/\$/g, '\\$')}\\.([A-Za-z_$][\\w$]*)`, 'g'))]
      .map((m) => ({ name: m[1] })) : [];
    for (const { name, type } of [...names, ...members]) {
      if (pkg.values.has(name) || ((typescript || type) && pkg.types.has(name))) continue;
      problem('imports', s.page, line, name === 'default'
        ? `${pk[1]} has no default export: import { … } from '${spec}'`
        : pkg.types.has(name) ? `${name} is a type in ${pk[1]}, not a value: import it in TypeScript`
          : `${pk[1]} exports no ${name}`);
    }
  }
}
check(report('imports'), `${imported} imports from @peerwire/* name what the packages export`);

// 3. Rust: the pages each doc attribute's include_str! makes a doc test.
const doctests = new Map();
for (const rs of files.filter((f) => f.endsWith('.rs'))) {
  const src = read(rs);
  for (const m of src.matchAll(/#!?\[\s*doc\s*=\s*include_str!\(\s*"([^"]+)"\s*\)\s*\]/g)) {
    const page = P.normalize(P.join(P.dirname(rs), m[1]));
    if (!fileSet.has(page)) problem('rust', rs, lineAt(src, m.index), `include_str!("${m[1]}"): no ${page}`);
    doctests.set(page, rs);
  }
}
const item = (page) => page.replace(/^docs\//, '').replace(/\.md$/, '').split(/[/._-]+/)
  .map((w) => w[0].toUpperCase() + w.slice(1).toLowerCase()).join('');
let rustPages = 0;
for (const [page, bs] of pageBlocks) {
  const rust = bs.filter((b) => b.lang === 'rust');
  if (rust.length) rustPages++;
  if (rust.length && !doctests.has(page)) {
    const crate = /^(crates\/[^/]+)\/README\.md$/.exec(page)?.[1];
    problem('rust', page, rust[0].line, `${rust.length} \`\`\`rust block(s) no doc test compiles: ` + (crate
      ? `#[cfg(doctest)] #[doc = include_str!("../README.md")] in ${crate}/src/lib.rs`
      : `list the page in mod doc_samples of crates/peerwire/src/lib.rs: ` +
        `#[doc = include_str!("${P.relative('crates/peerwire/src', page)}")] struct ${item(page)};`));
  }
  if (!doctests.has(page)) continue;
  for (const b of bs) {
    if (b.info === '') {
      problem('rust', page, b.line, 'a bare ``` block, which rustdoc compiles as Rust: give it a language (```text)');
    }
    const runs = b.lang === 'rust' && !b.attrs.some((a) => ['no_run', 'ignore', 'compile_fail'].includes(a));
    if (runs && /\bNostrSignaling\b/.test(b.body)) {
      problem('rust', page, b.line, 'runs against the public Nostr relays in `cargo test`: make it ```rust,no_run');
    }
  }
}
check(report('rust'), `Rust: ${rustPages} pages with samples, each a doc test (${doctests.size} pages are); no bare ` +
  'block on those, no sample run on public relays');

console.log(failed ? `${failed} check(s) failed` : 'all passed');
process.exit(failed ? 1 : 0);
