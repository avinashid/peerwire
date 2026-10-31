#!/bin/sh
# Builds the static site (the landing page, README and docs as HTML, the browser demos) into target/site/, as
# .github/workflows/pages.yml deploys it to GitHub Pages. See scripts/site/build.mjs.
#
#   scripts/site.sh            build the wasm (packages/js/build.sh), then the site
#   scripts/site.sh --serve    the same, then serve it as Pages will, under a subpath, on loopback only:
#                              http://127.0.0.1:${PORT:-7781}/peerwire/   (Ctrl-C stops)
set -e
cd "$(dirname "$0")/.."
./packages/js/build.sh
node scripts/site/build.mjs --out target/site
if [ "${1:-}" = --serve ]; then
  mkdir -p target/site-root
  ln -sfn ../site target/site-root/peerwire
  echo "serving http://127.0.0.1:${PORT:-7781}/peerwire/"
  exec python3 -m http.server "${PORT:-7781}" --bind 127.0.0.1 --directory target/site-root
fi
