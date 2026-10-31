#!/usr/bin/env bash
# The release gate: every check a change must pass before merge (see docs/TEST-PLAN.md).
# Runs every step even after a failure, prints a summary and exits non-zero if any step failed.
#
#   scripts/gate.sh             Rust tests, clippy, wasm build and checks, @peerwire/react build and types, Node glue,
#                               browser tests (loopback)
#   scripts/gate.sh --quick     skip the browser tests: an author's run before handing a branch over (see AGENTS.md)
#   scripts/gate.sh --strict    clippy with -D warnings, the fuzz tests again in a debug build (overflow checks), and
#                               CI's Node tests again on CI's Node (22): the lead's one full run on the merged main,
#                               before a push
#   scripts/gate.sh --nostr     also run the public-relay Nostr tests, JS and Rust (touch nos.lol, relay.primal.net, nostr.mom,
#                               public STUN)
#   scripts/gate.sh --full      run the Rust steps (tests, clippy, fmt, fuzz) even when no Rust changed: they are
#                               skipped when every file changed since GATE_BASE is JS, docs or another non-Rust path
#
# Order: the builds first (wasm, clippy, the Rust tests with --no-run). Then the Rust tests run beside the Node tests
# (GATE_JOBS at a time) and, once those are done, beside the browser tests (one at a time). The Rust tests run under
# cargo-nextest when it is installed (doctests through cargo test --doc), else under cargo test. Each step writes its
# own log, target/gate-logs/<step>.log; a failed step's log is printed in full before the summary.
#
# Env: WASM_MAX_BYTES (default 358400 = 350 KB), PLAYWRIGHT (path to playwright's index.mjs),
#      GATE_JOBS (Node tests at once, default 4), GATE_BASE (what "changed" is measured against, default origin/main),
#      GATE_STEP_TIMEOUT (seconds a Node or browser test may run, default 300),
#      NODE_CI (the node binary --strict runs CI's Node tests on, default the newest ~/.nvm/versions/node/v<CI's>.*).
set -u
cd "$(dirname "$0")/.."

quick=0 strict=0 nostr=0 full=0
for a in "$@"; do
  case "$a" in
    --quick) quick=1 ;;
    --strict) strict=1 ;;
    --nostr) nostr=1 ;;
    --full) full=1 ;;
    -h|--help) awk 'NR > 1 && !/^#/ { exit } NR > 1' "$0"; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
if ! command -v node >/dev/null && [ -s "$HOME/.nvm/nvm.sh" ]; then . "$HOME/.nvm/nvm.sh" >/dev/null; fi

WASM_MAX_BYTES=${WASM_MAX_BYTES:-358400}
WASM=packages/js/pkg/peerwire_ffi.wasm
GATE_JOBS=${GATE_JOBS:-4}
GATE_BASE=${GATE_BASE:-origin/main}
GATE_STEP_TIMEOUT=${GATE_STEP_TIMEOUT:-300}
LOGS=target/gate-logs
rm -rf "$LOGS" && mkdir -p "$LOGS"
trap 'trap - INT TERM; kill 0' INT TERM

# Node tests, no browser: "step name|command". They run GATE_JOBS at a time, so none may use a fixed port or file.
NODE=(
  "node glue|node packages/js/test/node_pair.mjs"
  "node wasm source|node packages/js/test/wasm_source.mjs"
  "node privacy|node packages/js/test/privacy.mjs"
  "node room|node packages/js/test/room_node.mjs"
  "node peer ids|node packages/js/test/peer_id.mjs"
  "node nostr bounds|node packages/js/test/nostr_bounds.mjs"
  "node inbox|node packages/js/test/inbox.mjs"
  "node rust hello|node packages/js/test/rust_hello.mjs"
  "node rust doc|node packages/js/test/rust_doc.mjs"
  "node room proof|node packages/js/test/room_proof.mjs"
  "node pump re-arm|node packages/js/test/pump_rearm.mjs"
  "node endpoint closed|node packages/js/test/endpoint_closed.mjs"
  "node scratch buffers|node packages/js/test/scratch_buffers.mjs"
  "node receive from|node packages/js/test/receive_from.mjs"
  "node room drain|node packages/js/test/room_drain.mjs"
  "node fuzz|node packages/js/test/fuzz.mjs"
  "node room leave|node packages/js/test/room_leave.mjs"
  "node room welcome|node packages/js/test/room_welcome.mjs"
  "node doc crdt|node packages/js/test/doc_crdt.mjs"
  "node doc relays|node packages/js/test/doc_relays.mjs"
  "node room doc|node packages/js/test/room_doc.mjs"
  "node doc catchup|node packages/js/test/doc_catchup.mjs"
  "node room fallback|node packages/js/test/room_fallback.mjs"
  "node pipe batches|node packages/js/test/pipe_batches.mjs"
  "node room relay move|node packages/js/test/room_relay_move.mjs"
  "node room relay latency|node packages/js/test/room_relay_latency.mjs"
  "node room adopt|node packages/js/test/room_adopt.mjs"
  "node room same nat|node packages/js/test/room_same_nat.mjs"
  "node room retry|node packages/js/test/room_retry.mjs"
  "node retry backoff|node packages/js/test/retry_backoff.mjs"
  "node nat prediction|node packages/js/test/nat_predict.mjs"
  "node late ice|node packages/js/test/late_ice.mjs"
  "node restart budget|node packages/js/test/restart_budget.mjs"
  "node birthday primitives|node packages/js/test/birthday.mjs"
  "node qr|node packages/js/test/qr.mjs"
  "node site build|node packages/js/test/site_build.mjs"
  "node react store|node packages/react/test/store.mjs"
  "node react hooks|node packages/react/test/hooks.mjs"
)
NODE_FIRST='^node[0-9]* (doc catchup|room adopt|room retry|room same nat|fuzz|room fallback|nostr bounds|pump re-arm)$'
# Browser tests (headless Chromium on loopback): one at a time, as WebRTC timing suffers under load.
BROWSER=(
  "browser|node packages/js/test/browser.mjs 2"
  "arena|node packages/js/test/arena.mjs 8 10"
  "readme chat|node packages/js/test/readme.mjs"
  "wasm url|node packages/js/test/wasm_url.mjs"
  "poll|node packages/js/test/poll.mjs 3"
  "chat|node packages/js/test/chat.mjs"
  "webrtc signals|node packages/js/test/webrtc_signals.mjs"
  "ice restart|node packages/js/test/ice_restart.mjs"
  "retry direct|node packages/js/test/retry_direct.mjs"
  "birthday fleet|node packages/js/test/birthday_fleet.mjs 32"
  "netlab birthday|node packages/js/test/netlab_birthday.mjs"
  "netlab carry|node packages/js/test/netlab_carry.mjs"
  "tab close|node packages/js/test/leave.mjs --runs=5 --crash=0"
  "doc store|node packages/js/test/doc_store.mjs"
  "netlab|env RTC_OFF=2 node packages/js/test/netlab.mjs 3"
  "netlab rooms|node packages/js/test/netlab_rooms.mjs"
  "netlab mic|node packages/js/test/netlab_mic.mjs"
  "netlab punch close|node packages/js/test/netlab_punch_close.mjs"
  "react example|node packages/react/test/example.mjs"
  "site|node packages/js/test/site.mjs"
)

# Paths that cannot change a Rust test, clippy or fmt result. Anything else runs the Rust steps: a new crate,
# Cargo.lock, scripts/ (but the site's), examples/, a crate README (they run as doctests).
NOT_RUST='^(packages/js/|packages/react/|docs/|\.github/|docker/|scripts/site(\.sh$|/)|[^/]+\.md$|LICENSE-|Dockerfile$|docker-compose\.yml$|\.dockerignore$)'

slug() { printf '%s' "${1,,}" | tr -cs 'a-z0-9' '-' | sed 's/-$//'; }

# step NAME CMD...: run one step, its output to $LOGS/<slug>.log and its summary line to $LOGS/<slug>.res.
step() {
  local name=$1; shift
  local f t0=$SECONDS r=PASS
  f=$LOGS/$(slug "$name")
  echo "==> $name: $*"
  "$@" >"$f.log" 2>&1 </dev/null
  case "$?/$1" in
    0/*) ;;
    124/timeout|137/timeout)
      r=FAIL
      echo "gate: no exit after GATE_STEP_TIMEOUT=${GATE_STEP_TIMEOUT}s, killed" >>"$f.log" ;;
    *) r=FAIL ;;
  esac
  echo "$r  $name ($((SECONDS - t0))s)" >"$f.res"
  echo "    $r  $name ($((SECONDS - t0))s)"
}

skip() {
  echo "SKIP  $1 ($2)" >"$LOGS/$(slug "$1").res"
}

# bad NAME REASON [LOG]: a step that failed before it could run.
bad() {
  echo "FAIL  $1 ($2)" >"$LOGS/$(slug "$1").res"
  printf '%s\n' "${3:-$2}" >"$LOGS/$(slug "$1").log"
}

# --strict: CI's `wasm + node` tests again, on the Node CI runs them on (NB-055: Node 22's JSON.stringify recurses
# and 26's doesn't, so NB-053 failed only in CI). Both come from CI's wasm job, so neither can drift from it: its
# `run: node packages/<pkg>/test/<file>.mjs` steps, named as in NODE but tagged ("node22 room drain"), and its
# node-version. They run in the Node pool, against the wasm and React builds above. No such Node here is a SKIP;
# a job with no test this can read, or a test line it can't, is a FAIL.
CI_YML=.github/workflows/ci.yml
CI_TESTS=() CI_NAMES=() CI_NODE=""
ci_tests() {
  local job major tag bin v line p name s missed=""
  local re='^ *(- )?run: node (packages/[A-Za-z0-9_-]+/test/[A-Za-z0-9_.-]+\.mjs( [A-Za-z0-9=._/-]+)*) *$'
  job=$(awk '/^  wasm:/ { on = 1; next } on && /^  [^ ]/ { exit } on' "$CI_YML" 2>/dev/null)
  major=$(sed -nE 's/^ *node-version: *([0-9]+) *$/\1/p' <<<"$job" | head -1)
  if [ -z "$major" ]; then
    CI_NAMES=("node ci")
    bad "node ci" "no node-version in the wasm job of $CI_YML"
    return
  fi
  tag=node$major
  bin=${NODE_CI:-$(ls -d "$HOME/.nvm/versions/node/v$major".*/bin/node 2>/dev/null | sort -V | tail -1)}
  if [ -z "$bin" ] || ! v=$("$bin" --version 2>/dev/null); then
    CI_NAMES=("$tag")
    skip "$tag" "no Node $major, CI's, here: nvm install $major, or set NODE_CI"
    return
  fi
  if [[ $v != v$major.* ]]; then
    CI_NAMES=("$tag")
    skip "$tag" "NODE_CI=$bin is $v, not CI's $major"
    return
  fi
  CI_NODE="$bin ($v)"
  while IFS= read -r line; do
    if ! [[ $line =~ $re ]]; then
      missed+="$line"$'\n'
      continue
    fi
    p=${BASH_REMATCH[2]} name=""
    for s in "${NODE[@]}"; do [ "${s#*|}" = "node $p" ] && name=${s%%|*}; done
    if [ -z "$name" ]; then
      name=${p%% *} name=${name##*/} name="node ${name%.mjs}"
    fi
    CI_TESTS+=("$tag ${name#node }|$bin $p")
    CI_NAMES+=("$tag ${name#node }")
  done < <(grep -vE '^ *#' <<<"$job" | grep -E '/test/[^ ]*\.mjs')
  if [ ${#CI_TESTS[@]} = 0 ] || [ -n "$missed" ]; then
    CI_NAMES+=("$tag list")
    if [ -n "$missed" ]; then
      bad "$tag list" "CI test steps this can't read" "$missed"
    else
      bad "$tag list" "no run: node packages/<pkg>/test/<file>.mjs step in the wasm job of $CI_YML"
    fi
  fi
}
[ $strict = 0 ] || ci_tests

# entry "name|command": one step from the NODE or BROWSER list. A step that runs past GATE_STEP_TIMEOUT fails.
entry() {
  local cmd
  read -ra cmd <<<"${1#*|}"
  step "${1%%|*}" timeout -k 10 "$GATE_STEP_TIMEOUT" "${cmd[@]}"
}

# Size and import count of the browser module: it must stay small and import nothing.
wasm_check() {
  node -e '
    const fs = require("fs");
    const [path, max] = [process.argv[1], Number(process.argv[2])];
    const bytes = fs.readFileSync(path);
    const imports = WebAssembly.Module.imports(new WebAssembly.Module(bytes));
    console.log(`${path}: ${bytes.length} bytes (limit ${max}), ${imports.length} imports`);
    for (const i of imports) console.log(`  import ${i.module}.${i.name} (${i.kind})`);
    process.exit(bytes.length <= max && imports.length === 0 ? 0 : 1);
  ' "$WASM" "$WASM_MAX_BYTES"
}

# @peerwire/react: its dev dependencies (npm ci, again only when package-lock.json changes), then tsc: the build
# its tests load, and the types of the package, its example and test/types.tsx.
react_build() {
  local d=packages/react lock
  lock=$(sha256sum "$d/package-lock.json" | cut -d' ' -f1)
  if [ "$(cat "$d/node_modules/.gate-lock" 2>/dev/null)" != "$lock" ]; then
    (cd "$d" && npm ci --no-audit --no-fund) || return 1
    echo "$lock" >"$d/node_modules/.gate-lock"
  fi
  (cd "$d" && npm run --silent build && npm run --silent typecheck)
}

nextest=0
cargo nextest --version >/dev/null 2>&1 && nextest=1
FUZZ=(--lib -p peerwire-core -p peerwire -p peerwire-ffi -p peerwire-sim -p peerwire-sync fuzz)
# peerwire-ffi's shared docs (`sync`) are off in the wasm build and so in the workspace's: built, linted and tested
# on their own, so they can't rot.
FFI_SYNC=(-p peerwire-ffi --features sync --release)
# The same in a debug build, beside the fuzzers: its length and buffer sums must not overflow either.
FFI_SYNC_DEBUG=(-p peerwire-ffi --features sync --lib)

# Every Rust test: nextest runs each test in its own process, many at once, but skips doctests.
rust_tests() {
  local r=0
  if [ $nextest = 0 ]; then
    cargo test --workspace --release || r=1
    cargo test "${FFI_SYNC[@]}" || r=1
    return $r
  fi
  cargo nextest run --workspace --release --profile gate || r=1
  cargo nextest run "${FFI_SYNC[@]}" --profile gate || r=1
  cargo test --doc --workspace --release || r=1
  return $r
}

# Release builds wrap on integer overflow; the parser fuzzers must also run where it panics.
fuzz_tests() {
  local r=0
  if [ $nextest = 1 ]; then
    cargo nextest run --profile gate "${FUZZ[@]}" || r=1
    cargo nextest run --profile gate "${FFI_SYNC_DEBUG[@]}" || r=1
  else
    cargo test "${FUZZ[@]}" || r=1
    cargo test "${FFI_SYNC_DEBUG[@]}" || r=1
  fi
  return $r
}

# Build every test binary, so the run phase doesn't wait on cargo's lock: 1 if any build failed. A build error
# shows up again, and is reported, in the step that runs the tests.
rust_build() {
  local r=0 run=(cargo test)
  [ $nextest = 0 ] || run=(cargo nextest run)
  "${run[@]}" --workspace --release --no-run || r=1
  "${run[@]}" "${FFI_SYNC[@]}" --no-run || r=1
  if [ $strict = 1 ]; then
    "${run[@]}" "${FUZZ[@]}" --no-run || r=1
    "${run[@]}" "${FFI_SYNC_DEBUG[@]}" --no-run || r=1
  fi
  return $r
}

clippy() {
  cargo clippy --workspace --all-targets "$@" && cargo clippy -p peerwire-ffi --features sync --all-targets "$@"
}

# Skip the Rust steps when every file changed since GATE_BASE (committed, staged, unstaged or untracked) is on
# the NOT_RUST list. No changes at all, or a base that doesn't resolve, runs everything.
rust_skip=""
if [ $full = 0 ] && base=$(git merge-base "$GATE_BASE" HEAD 2>/dev/null); then
  changed=$( { git diff --name-only --no-renames "$base"; git ls-files --others --exclude-standard; } | sort -u)
  if [ -n "$changed" ] && ! grep -qvE "$NOT_RUST" <<<"$changed"; then
    rust_skip="no Rust change since $GATE_BASE $(git rev-parse --short "$base")"
  fi
fi

# The Node tests, GATE_JOBS at a time. Run in a subshell, so its job table holds only these.
# --no-wasm-tier-up: on a busy host, Node 26.7 sometimes deadlocks in process.exit() while V8 is still tiering the
# wasm up in the background (room_drain.mjs hung after "all passed" in 5 of 280 runs four at a time, 0 of 100 one at
# a time, 0 of 360 four at a time with the flag). The tests run as fast without it.
# The slowest (5-72 s) start first, so the pool doesn't end waiting on one of them. CI's tests on CI's Node
# (--strict) share the pool and the flag. The pool alone on 30 Oct, 4 jobs: 92 s, then 59 s with doc catchup
# (72 s) moved first; --strict 104 s, then 88 s.
node_pool() {
  local s cmd first slow
  for first in 1 0; do
    for s in "${NODE[@]}" "${CI_TESTS[@]}"; do
      slow=0
      [[ ${s%%|*} =~ $NODE_FIRST ]] && slow=1
      [ $slow = $first ] || continue
      while [ "$(jobs -rp | wc -l)" -ge "$GATE_JOBS" ]; do wait -n; done
      cmd=${s#*|}
      entry "${s%%|*}|${cmd%% *} --no-wasm-tier-up ${cmd#* }" &
    done
  done
  wait
}

browser_lane() {
  local s
  if [ $quick = 1 ]; then
    for s in "${BROWSER[@]}"; do skip "${s%%|*}" "--quick"; done
  else
    for s in "${BROWSER[@]}"; do entry "$s"; done
  fi
}

# The Rust tests run from start to end; the Node tests beside them, then the browser tests, which overlap only the
# Rust tests. That order came in when room_adopt.mjs failed beside Chromium and nextest ("batches still went over
# signalling"). The flake is fixed: the test by 2c80929, its cause in webrtc.js by 5a7e4ba. On a4299d3 it passed
# 66 of 66 runs: 20 in the gate's Node mix, 18 pinned to one core with a CPU burner, 16 beside cargo nextest and
# 12 beside the browser tests.
lanes() {
  if [ -z "$rust_skip" ]; then
    { step "cargo test" rust_tests; [ $strict = 0 ] || step "fuzz (debug)" fuzz_tests; } &
  fi
  (node_pool)
  browser_lane
  wait
}

echo "logs: $LOGS/   rust tests: $([ $nextest = 1 ] && echo cargo-nextest || echo cargo test)   node jobs: $GATE_JOBS"
[ ${#CI_TESTS[@]} = 0 ] || echo "CI's Node tests: ${#CI_TESTS[@]}, on $CI_NODE"

# 1. Builds, one after another: they share target/ and cargo's lock.
step "wasm build" ./packages/js/build.sh
step "wasm size+imports" wasm_check
step "react build+types" react_build
if [ -n "$rust_skip" ]; then
  echo "==> Rust steps skipped: $rust_skip (pass --full to run them)"
  skip "cargo test" "$rust_skip"
  skip "cargo clippy" "$rust_skip"
  [ $strict = 0 ] || skip "fuzz (debug)" "$rust_skip"
  skip "cargo fmt" "$rust_skip"
else
  if [ $strict = 1 ]; then
    step "cargo clippy" clippy -- -D warnings
  else
    step "cargo clippy" clippy
  fi
  if cargo fmt --version >/dev/null 2>&1; then
    step "cargo fmt" cargo fmt --all -- --check
  else
    skip "cargo fmt" "rustfmt not installed"
  fi
  echo "==> build the Rust tests (log: $LOGS/rust-build.log)"
  rust_build >"$LOGS/rust-build.log" 2>&1 </dev/null || echo "==> a Rust test build failed: see $LOGS/rust-build.log"
fi

# 2. The Rust tests and the Node tests side by side, then the browser tests one at a time.
lanes

if [ $nostr = 1 ]; then
  step "nostr" node packages/js/test/nostr.mjs
  step "nostr (rust)" cargo test -p peerwire --release --test nostr -- --ignored --nocapture
else
  skip "nostr" "public relays; pass --nostr"
  skip "nostr (rust)" "public relays; pass --nostr"
fi

# The summary, in the order of the old one-at-a-time gate.
order=("cargo test" "cargo clippy")
[ $strict = 0 ] || order+=("fuzz (debug)")
order+=("cargo fmt" "wasm build" "wasm size+imports" "react build+types")
for s in "${NODE[@]}"; do order+=("${s%%|*}"); done
order+=("${CI_NAMES[@]}")
for s in "${BROWSER[@]}"; do order+=("${s%%|*}"); done
order+=("nostr" "nostr (rust)")

failed=0
for n in "${order[@]}"; do
  f=$LOGS/$(slug "$n")
  [ -f "$f.res" ] || echo "FAIL  $n (no result)" >"$f.res"
  if grep -q '^FAIL' "$f.res"; then
    failed=1
    echo
    echo "==> $n failed; its log ($f.log):"
    cat "$f.log" 2>/dev/null
  fi
done
echo
echo "==> gate summary (logs in $LOGS/)"
for n in "${order[@]}"; do printf '  %s\n' "$(cat "$LOGS/$(slug "$n").res")"; done
[ $failed = 0 ] && echo "GATE PASSED" || echo "GATE FAILED"
exit $failed
