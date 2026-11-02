#!/usr/bin/env bash
# The release gate: every check a change must pass before merge (see docs/project/testing.md).
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
# Order: two lanes from the start. The Rust lane, at a lower CPU priority: clippy, fmt, the test builds, then the
# tests. The JS lane: the wasm and React builds, then the Node tests (GATE_JOBS at a time) beside the browser tests
# (GATE_BROWSER_JOBS at a time). The Rust tests are built with the gate profile (release, built incrementally) and
# the debug fuzzers with the fuzz one (Cargo.toml), and run under cargo-nextest when it is installed (doctests
# through cargo test --doc), else under cargo test. Each step writes its own log, target/gate-logs/<step>.log; a
# failed step's log is printed in full before the summary.
# The docs' samples are tested too (NB-070): the Rust ones compile, never run, in "rust doc samples" (on a change to
# Rust or Markdown); the JS ones and every link in "node docs samples" and "node docs links".
#
# Env: WASM_MAX_BYTES (default 358400 = 350 KB), PLAYWRIGHT (path to playwright's index.mjs),
#      GATE_JOBS (Node tests at once, default 4), GATE_BROWSER_JOBS (browser tests at once, default 3),
#      GATE_BASE (what "changed" is measured against, default origin/main),
#      GATE_STEP_TIMEOUT (seconds a Node or browser test may run, default 300),
#      GATE_RETRY (1, the default: a Node or browser test that failed runs once more alone, a pass then is FLAKY;
#      0: no second run), GATE_RETRY_MAX (more failed tests than this, default 5, run no second time),
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
GATE_BROWSER_JOBS=${GATE_BROWSER_JOBS:-3}
GATE_BASE=${GATE_BASE:-origin/main}
GATE_STEP_TIMEOUT=${GATE_STEP_TIMEOUT:-300}
GATE_RETRY=${GATE_RETRY:-1}
GATE_RETRY_MAX=${GATE_RETRY_MAX:-5}
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
  "node call|node packages/js/test/call.mjs"
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
  "node docs samples|node packages/js/test/docs_samples.mjs"
  "node docs links|node packages/js/test/docs_links.mjs"
  "node react store|node packages/react/test/store.mjs"
  "node react hooks|node packages/react/test/hooks.mjs"
)
NODE_FIRST='^node[0-9]* (doc catchup|room adopt|room retry|room same nat|fuzz|room fallback|nostr bounds|pump re-arm)$'
# Browser tests (headless Chromium on loopback): GATE_BROWSER_JOBS at a time, not more, as WebRTC timing suffers
# under load. Each serves its page on a port the OS picks (serve.mjs), so they can run side by side.
BROWSER=(
  "browser|node packages/js/test/browser.mjs 2"
  "arena|node packages/js/test/arena.mjs 8 10"
  "readme chat|node packages/js/test/readme.mjs"
  "wasm url|node packages/js/test/wasm_url.mjs"
  "poll|node packages/js/test/poll.mjs 3"
  "chat|node packages/js/test/chat.mjs"
  "call|node packages/js/test/call_browser.mjs"
  "meet|node packages/js/test/meet.mjs 3"
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
# The slowest browser tests (16-66 s on 2 Nov), started first.
BROWSER_FIRST='^(netlab punch close|netlab mic|call|netlab birthday|meet)$'

# Paths that cannot change a Rust test, clippy or fmt result. Anything else runs the Rust steps: a new crate,
# Cargo.lock, scripts/ (but the site's), examples/, a crate README (they run as doctests).
NOT_RUST='^(packages/js/|packages/react/|docs/|\.github/|docker/|scripts/site(\.sh$|/)|[^/]+\.md$|LICENSE-|\.editorconfig$|Dockerfile$|docker-compose\.yml$|\.dockerignore$)'

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
# The Cargo profiles of Cargo.toml: gate (release code, built incrementally) for the tests, fuzz (a debug build's
# overflow checks and debug assertions, at opt-level 1) for the fuzzers. nextest takes them as --cargo-profile.
GATE=(--profile gate) FUZZ_PROFILE=(--profile fuzz)
[ $nextest = 0 ] || GATE=(--cargo-profile gate) FUZZ_PROFILE=(--cargo-profile fuzz)
FUZZ=(--lib -p peerwire-core -p peerwire -p peerwire-ffi -p peerwire-sim -p peerwire-sync "${FUZZ_PROFILE[@]}" fuzz)
# peerwire-ffi's shared docs (`sync`) are off in the wasm build and so in the workspace's: built, linted and tested
# on their own, so they can't rot.
FFI_SYNC=(-p peerwire-ffi --features sync "${GATE[@]}")
# The same with overflow checks, beside the fuzzers: its length and buffer sums must not overflow either.
FFI_SYNC_DEBUG=(-p peerwire-ffi --features sync --lib "${FUZZ_PROFILE[@]}")

# Every Rust test: nextest runs each test in its own process, many at once, but skips doctests.
rust_tests() {
  local r=0
  if [ $nextest = 0 ]; then
    cargo test --workspace "${GATE[@]}" || r=1
    cargo test "${FFI_SYNC[@]}" || r=1
    return $r
  fi
  cargo nextest run --workspace "${GATE[@]}" --profile gate || r=1
  cargo nextest run "${FFI_SYNC[@]}" --profile gate || r=1
  cargo test --doc --workspace --profile gate || r=1
  return $r
}

# The Rust samples of README.md and docs/ (mod doc_samples in crates/peerwire/src/lib.rs): doc tests that only
# compile. --cfg docs_samples brings them in (a plain cargo test leaves them out), and rustdoc "runs" each with
# `true`: many are host loops, or dial public relays.
doc_samples() {
  RUSTDOCFLAGS="${RUSTDOCFLAGS:+$RUSTDOCFLAGS }--cfg docs_samples --test-runtool true" \
    cargo test --doc --workspace --profile gate doc_samples
}

# Release builds wrap on integer overflow; the parser fuzzers must also run where it panics: the fuzz profile.
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

# Build every test binary first, so a test step's time is the tests': 1 if any build failed. A build error shows up
# again, and is reported, in the step that runs the tests.
rust_build() {
  local r=0 run=(cargo test)
  [ $nextest = 0 ] || run=(cargo nextest run)
  "${run[@]}" --workspace "${GATE[@]}" --no-run || r=1
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
samples_skip=""
if [ -n "$rust_skip" ] && ! grep -qE '\.md$' <<<"$changed"; then
  samples_skip="no Markdown or Rust change since $GATE_BASE $(git rev-parse --short "$base")"
fi

# node_entry "name|node args": the entry with --no-wasm-tier-up after the node binary.
node_entry() {
  local cmd=${1#*|}
  echo "${1%%|*}|${cmd%% *} --no-wasm-tier-up ${cmd#* }"
}

# The Node tests, GATE_JOBS at a time, at nice 5: most wait on timers (116 CPU s in 423 s of tests on 3 Nov), and
# the browser tests beside them, whose WebRTC timing suffers most under load, get the CPU first. Run in a
# subshell, so its job table holds only these.
# --no-wasm-tier-up: on a busy host, Node 26.7 sometimes deadlocks in process.exit() while V8 is still tiering the
# wasm up in the background (room_drain.mjs hung after "all passed" in 5 of 280 runs four at a time, 0 of 100 one at
# a time, 0 of 360 four at a time with the flag). The tests run as fast without it.
# The slowest (5-72 s) start first, so the pool doesn't end waiting on one of them. CI's tests on CI's Node
# (--strict) share the pool and the flag. The pool alone on 30 Oct, 4 jobs: 92 s, then 59 s with doc catchup
# (72 s) moved first; --strict 104 s, then 88 s.
node_pool() {
  local s first slow
  renice -n 5 -p $BASHPID >/dev/null
  for first in 1 0; do
    for s in "${NODE[@]}" "${CI_TESTS[@]}"; do
      slow=0
      [[ ${s%%|*} =~ $NODE_FIRST ]] && slow=1
      [ $slow = $first ] || continue
      while [ "$(jobs -rp | wc -l)" -ge "$GATE_JOBS" ]; do wait -n; done
      entry "$(node_entry "$s")" &
    done
  done
  wait
}

# The browser tests, GATE_BROWSER_JOBS at a time, the slowest first. Run in a subshell, like node_pool.
browser_pool() {
  local s first slow
  if [ $quick = 1 ]; then
    for s in "${BROWSER[@]}"; do skip "${s%%|*}" "--quick"; done
    return
  fi
  for first in 1 0; do
    for s in "${BROWSER[@]}"; do
      slow=0
      [[ ${s%%|*} =~ $BROWSER_FIRST ]] && slow=1
      [ $slow = $first ] || continue
      while [ "$(jobs -rp | wc -l)" -ge "$GATE_BROWSER_JOBS" ]; do wait -n; done
      entry "$s" &
    done
  done
  wait
}

# The Rust lane: clippy, fmt, the test builds, then the tests, at nice 10, so the Node and browser tests beside it
# keep their timing.
rust_lane() {
  renice -n 10 -p $BASHPID >/dev/null
  if [ -n "$rust_skip" ]; then
    but=""
    [ -n "$samples_skip" ] || but=", but for the docs' Rust samples"
    echo "==> Rust steps skipped: $rust_skip$but (pass --full to run them)"
    skip "cargo test" "$rust_skip"
    skip "cargo clippy" "$rust_skip"
    [ $strict = 0 ] || skip "fuzz (debug)" "$rust_skip"
    skip "cargo fmt" "$rust_skip"
    [ -z "$samples_skip" ] || skip "rust doc samples" "$samples_skip"
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
  fi
  if [ -z "$rust_skip" ]; then
    echo "==> build the Rust tests (log: $LOGS/rust-build.log)"
    rust_build >"$LOGS/rust-build.log" 2>&1 </dev/null || echo "==> a Rust test build failed: see $LOGS/rust-build.log"
    step "cargo test" rust_tests
    step "rust doc samples" doc_samples
    [ $strict = 0 ] || step "fuzz (debug)" fuzz_tests
  elif [ -z "$samples_skip" ]; then
    step "rust doc samples" doc_samples
  fi
}

# The lanes, from the start: the Rust lane at a lower priority, and the JS lane, whose builds (wasm, then React)
# come before its tests, the Node and browser pools side by side. The wasm build has its own target dir
# (target/web), so it doesn't wait on cargo's lock for the Rust lane's. A warm --strict took 453 s on 2 Nov with
# the tests one lane after another (browser tests one at a time), 222 s with the browser tests 3 at a time after
# the Node tests (NB-075), and 128-131 s on 3 Nov with this order (NB-076); 145 s after a one-line change to
# peerwire-core, 279 s before. With both pools at nice 0, netlab birthday failed in 2 runs of 3; with 4 browser jobs
# something failed in each of 3 runs; with 6 Node jobs room leave and receive from failed. With this split only
# doc store failed, in 1 run of 3: a race in the IndexedDB store (NB-077). room_adopt.mjs once failed beside
# Chromium and nextest ("batches still went over signalling"); that flake is fixed: the test by 2c80929, its cause
# in webrtc.js by 5a7e4ba, and it passed 66 of 66 runs on a4299d3.
js_lane() {
  step "wasm build" ./packages/js/build.sh
  step "wasm size+imports" wasm_check
  step "react build+types" react_build
  (node_pool) &
  (browser_pool) &
  wait
}

echo "logs: $LOGS/   rust tests: $([ $nextest = 1 ] && echo cargo-nextest || echo cargo test)   node jobs: $GATE_JOBS" \
  "  browser jobs: $GATE_BROWSER_JOBS"
[ ${#CI_TESTS[@]} = 0 ] || echo "CI's Node tests: ${#CI_TESTS[@]}, on $CI_NODE"

rust_lane &
js_lane
wait

# A Node or browser test that failed beside the others runs once more, alone, now that the host is quiet. A pass
# then is FLAKY: a load flake (NB-060), listed in the summary with the log of the run that failed (<step>.first.log),
# but not a failed gate. Over GATE_RETRY_MAX failures run no more: that is no load flake.
again=()
for s in "${NODE[@]}" "${CI_TESTS[@]}"; do
  grep -qs '^FAIL' "$LOGS/$(slug "${s%%|*}").res" && again+=("$(node_entry "$s")")
done
for s in "${BROWSER[@]}"; do
  grep -qs '^FAIL' "$LOGS/$(slug "${s%%|*}").res" && again+=("$s")
done
if [ "$GATE_RETRY" = 1 ] && [ ${#again[@]} -gt 0 ] && [ ${#again[@]} -le "$GATE_RETRY_MAX" ]; then
  echo "==> ${#again[@]} failed test(s) again, one at a time"
  for s in "${again[@]}"; do
    f=$LOGS/$(slug "${s%%|*}")
    t1=$(grep -oE '[0-9]+s\)$' "$f.res")
    mv "$f.log" "$f.first.log"
    entry "$s"
    t2=$(grep -oE '[0-9]+s\)$' "$f.res")
    if grep -q '^PASS' "$f.res"; then
      echo "FLAKY ${s%%|*} (failed in ${t1%)} beside the others, passed alone in ${t2%)}; that log:" \
        "$f.first.log)" >"$f.res"
    else
      echo "FAIL  ${s%%|*} (failed in ${t1%)} beside the others, and alone in ${t2%)})" >"$f.res"
    fi
  done
fi

if [ $nostr = 1 ]; then
  step "nostr" node packages/js/test/nostr.mjs
  step "nostr (rust)" cargo test -p peerwire --profile gate --test nostr -- --ignored --nocapture
else
  skip "nostr" "public relays; pass --nostr"
  skip "nostr (rust)" "public relays; pass --nostr"
fi

# The summary, in the order of the old one-at-a-time gate.
order=("cargo test" "rust doc samples" "cargo clippy")
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
flaky=$(cat "$LOGS"/*.res | grep -c '^FLAKY')
[ "$flaky" = 0 ] || echo "==> $flaky flaky: failed beside the others, passed alone (NB-060); see each one's .first.log"
[ $failed = 0 ] && echo "GATE PASSED" || echo "GATE FAILED"
exit $failed
