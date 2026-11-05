#!/usr/bin/env bash
# Publishes the Rust crates to crates.io, in dependency order (NB-102). A dry run unless --execute: nothing leaves
# this machine then but reads of the crates.io index.
#
#   scripts/publish-crates.sh             dry run: the checks below, then cargo publish --dry-run of every crate, which
#                                         packs each one and builds it from its package (a crate not yet on crates.io
#                                         comes from its package too); then each package's files, size and whether
#                                         its version is on crates.io already
#   scripts/publish-crates.sh --execute   publish them one at a time, each once the one before is in the index
#
# --execute takes the token from CARGO_REGISTRY_TOKEN in the environment, where cargo reads it, and never prints it.
# From a .env file:  CARGO_REGISTRY_TOKEN=$(sed -n 's/^CARGO_REGISTRY_TOKEN=//p' .env) scripts/publish-crates.sh --execute
# It needs a clean tree whose HEAD is on origin/main (each package names the commit it came from), skips a crate whose
# version is on crates.io already, so a run that stopped can start again, and waits up to PUBLISH_WAIT seconds
# (default 600) for each crate to reach the index. A published version can be yanked, never replaced or deleted.
#
# Checks, both modes: each crate takes the workspace's version; its licence files are the repo's; it comes after the
# crates it depends on; and its package holds only Cargo.toml, Cargo.lock, README.md, the licences and src/**/*.rs.
set -euo pipefail
set +x # no trace: it would show the token check

# Dependency order: each crate after every crate it depends on (checked below).
CRATES=(peerwire-core peerwire-game peerwire-sync peerwire peerwire-ffi peerwire-sim peerwire-peer)

cd "$(dirname "$0")/.."
die() { printf 'publish-crates: %s\n' "$*" >&2; exit 1; }
say() { printf 'publish-crates: %s\n' "$*"; }

EXECUTE=0
case "${1:-}" in
  "") ;;
  --execute) EXECUTE=1 ;;
  -h | --help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
  *) die "unknown argument $1 (--execute publishes; no argument is a dry run)" ;;
esac
[ $# -le 1 ] || die "one argument at most"

VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
[ -n "$VERSION" ] || die "no version in Cargo.toml's [workspace.package]"
TARGET=${CARGO_TARGET_DIR:-target}

# cargo's notes that it leaves out the tests and examples, whose files a package doesn't hold (include in each
# Cargo.toml): expected, so not shown.
quiet() { sed -E '/^warning: ignoring (example|test|bench) `[^`]+` as `[^`]+` is not included in the published package$/d'; }

# The crate's line in the crates.io sparse index: 0 if it lists $VERSION, 1 if not (or no such crate), 2 if the index
# can't be read. Every name here has four characters or more: ab/cd/abcd...
on_index() {
  local out code
  out=$(curl -sS --max-time 20 -H 'Cache-Control: no-cache' -w '\n%{http_code}' \
    "https://index.crates.io/${1:0:2}/${1:2:2}/$1" 2>/dev/null) || return 2
  code=${out##*$'\n'}
  case $code in
    200) grep -q "\"vers\":\"$VERSION\"" <<<"${out%$'\n'*}" && return 0 || return 1 ;;
    404) return 1 ;;
    *) return 2 ;;
  esac
}

# --- checks -------------------------------------------------------------------------------------------------------
DIRTY=()
if [ -n "$(git status --porcelain)" ]; then
  [ $EXECUTE = 0 ] || die "the tree has uncommitted changes: publish from a clean checkout"
  say "the tree has uncommitted changes: packing them anyway, as this is a dry run"
  DIRTY=(--allow-dirty)
fi
if [ $EXECUTE = 1 ]; then
  [ -n "${CARGO_REGISTRY_TOKEN:-}" ] || die "set CARGO_REGISTRY_TOKEN (a crates.io token with publish-new and publish-update)"
  git merge-base --is-ancestor HEAD origin/main 2>/dev/null ||
    die "HEAD $(git rev-parse --short HEAD) isn't on origin/main: push it first, as each package names its commit"
fi

for c in "${CRATES[@]}"; do
  dir=crates/$c
  [ -f "$dir/Cargo.toml" ] || die "$c: no $dir/Cargo.toml"
  grep -qx 'version.workspace = true' "$dir/Cargo.toml" || die "$c: its version isn't the workspace's ($VERSION)"
  for f in LICENSE-MIT LICENSE-APACHE; do
    cmp -s "$f" "$dir/$f" || die "$c: $dir/$f isn't a copy of the repo's $f"
  done
done

i=0
for c in "${CRATES[@]}"; do
  # Its direct dependencies, every feature on: "name vX (path)" lines, the crate itself first.
  while read -r dep _; do
    [ "$dep" != "$c" ] || continue
    for ((k = 0; k < ${#CRATES[@]}; k++)); do
      if [ "${CRATES[k]}" = "$dep" ] && [ $k -gt $i ]; then
        die "$c depends on $dep, which comes after it in CRATES"
      fi
    done
  done < <(cargo tree --locked -q -p "$c" -e normal,build --all-features --depth 1 --prefix none)
  i=$((i + 1))
done

FILES=() # each package's file count, in CRATES' order (bash 3 has no maps)
for c in "${CRATES[@]}"; do
  list=$(cargo package --locked -q --list -p "$c" ${DIRTY[@]+"${DIRTY[@]}"})
  bad=$(grep -Ev '^(Cargo\.toml|Cargo\.toml\.orig|Cargo\.lock|\.cargo_vcs_info\.json|README\.md|LICENSE-MIT|LICENSE-APACHE|src/.+\.rs)$' <<<"$list" || true)
  [ -z "$bad" ] || die "$c: its package would hold $(tr '\n' ' ' <<<"$bad")(see include in crates/$c/Cargo.toml)"
  FILES+=($(wc -l <<<"$list"))
done
say "checks passed: ${#CRATES[@]} crates at $VERSION, in order, licences copied, packages hold only sources"

# --- dry run ------------------------------------------------------------------------------------------------------
if [ $EXECUTE = 0 ]; then
  args=()
  for c in "${CRATES[@]}"; do args+=(-p "$c"); done
  say "cargo publish --dry-run: packs each crate and builds it from its package"
  cargo publish --dry-run --locked ${DIRTY[@]+"${DIRTY[@]}"} "${args[@]}" 2>&1 | quiet
  printf '\n%-15s %5s %10s  %s\n' crate files package crates.io
  for ((i = 0; i < ${#CRATES[@]}; i++)); do
    c=${CRATES[i]}
    tenths=$((($(wc -c <"$TARGET/package/$c-$VERSION.crate") * 10 + 512) / 1024)) # KiB, rounded as cargo does
    rc=0
    on_index "$c" || rc=$?
    case $rc in
      0) state="$VERSION is there already: --execute skips it" ;;
      1) state="not there: --execute publishes it" ;;
      *) state="index unreachable" ;;
    esac
    printf '%-15s %5s %10s  %s\n' "$c" "${FILES[i]}" "$((tenths / 10)).$((tenths % 10)) KiB" "$state"
  done
  say "dry run passed; nothing was uploaded. scripts/publish-crates.sh --execute publishes"
  exit 0
fi

# --- publish ------------------------------------------------------------------------------------------------------
WAIT=${PUBLISH_WAIT:-600}
for c in "${CRATES[@]}"; do
  rc=0
  on_index "$c" || rc=$?
  case $rc in
    0) say "$c $VERSION is on crates.io already: skipped"; continue ;;
    1) ;;
    *) die "can't read the crates.io index for $c: stopped before publishing it" ;;
  esac
  say "publishing $c $VERSION"
  cargo publish --locked -p "$c" 2>&1 | quiet
  # cargo waits for the index too, but gives up after a minute and carries on: the next crate's build needs this one.
  deadline=$((SECONDS + WAIT))
  until on_index "$c"; do
    [ $SECONDS -lt $deadline ] ||
      die "$c $VERSION isn't in the index after ${WAIT}s: run again once it is (published crates are skipped)"
    sleep 5
  done
  say "$c $VERSION is in the index: https://crates.io/crates/$c/$VERSION"
done
say "all ${#CRATES[@]} crates are on crates.io at $VERSION"
