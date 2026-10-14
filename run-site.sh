#!/usr/bin/env bash
# Run the Gossamer website locally for browser testing. No commit, no deploy.
#
#   ./run-site.sh                serve the landing site (home, tour, playground) on :8000
#   ./run-site.sh --port 9000    pick a port
#   ./run-site.sh --wasm         also build the real playground wasm VM (needs the
#                                wasm32 toolchain + the gossamer-playground crate)
#   ./run-site.sh --docs         also build the mkdocs docs so docs/ links resolve
#                                (needs mkdocs-material: pip install mkdocs-material)
#
# ES modules and the esm.sh imports require http(s); opening index.html as a
# file:// URL will not load the editor. Until the wasm VM is built (--wasm or
# CI), the playground falls back to a JS stub that prints placeholder output.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PORT=8000
BUILD_WASM=0
BUILD_DOCS=0

while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="$2"; shift 2 ;;
    --port=*) PORT="${1#*=}"; shift ;;
    --wasm) BUILD_WASM=1; shift ;;
    --docs) BUILD_DOCS=1; shift ;;
    -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
  esac
done

SERVE_DIR="$ROOT/landing"

# --docs: stage a full copy (landing + built docs) so docs/ links resolve,
# mirroring the deployed layout. Served from target/ (git-ignored).
if [ "$BUILD_DOCS" = 1 ]; then
  SERVE_DIR="$ROOT/target/site-preview"
  rm -rf "$SERVE_DIR"
  mkdir -p "$SERVE_DIR"
  cp -r "$ROOT/landing/." "$SERVE_DIR/"
  if command -v mkdocs >/dev/null 2>&1; then
    ( cd "$ROOT" && mkdocs build --site-dir "$SERVE_DIR/docs" )
  else
    echo "note: mkdocs not found; skipping docs (pip install mkdocs-material)" >&2
  fi
fi

# --wasm: compile the bytecode VM to wasm and drop the wasm-bindgen module
# into the served playground dir, so the playground runs real Gossamer.
if [ "$BUILD_WASM" = 1 ]; then
  if cargo build --release --target wasm32-unknown-unknown -p gossamer-playground \
       && command -v wasm-bindgen >/dev/null 2>&1; then
    wasm-bindgen "${CARGO_TARGET_DIR:-$ROOT/target}/wasm32-unknown-unknown/release/gossamer_playground.wasm" \
      --out-dir "$SERVE_DIR/playground" --target web
    echo "built the real wasm VM into $SERVE_DIR/playground"
  else
    echo "note: could not build the playground wasm (engine port incomplete or" \
         "wasm-bindgen missing); the playground will use the JS stub" >&2
  fi
fi

if [ -f "$SERVE_DIR/playground/gossamer_playground.js" ]; then
  RUNTIME="real wasm VM"
else
  RUNTIME="JS stub (placeholder output - build with --wasm for the real VM)"
fi

echo
echo "Gossamer site  ->  http://localhost:$PORT/"
echo "  home      http://localhost:$PORT/index.html"
echo "  tour      http://localhost:$PORT/tour/"
echo "  playground http://localhost:$PORT/playground/"
echo "  runtime   $RUNTIME"
[ "$BUILD_DOCS" = 1 ] || echo "  (docs/ links 404 unless you pass --docs)"
echo "  Ctrl-C to stop."
echo

# The project's own web stack serves the project's own site: a
# directory resolves to the index.html inside it, so /tour/ and
# /playground/ work exactly as they do deployed.
GOS="${GOS_BIN:-$ROOT/target/debug/gos}"
if [ ! -x "$GOS" ]; then
  echo "building $GOS" >&2
  ( cd "$ROOT" && cargo build --bin gos )
fi
exec "$GOS" run "$ROOT/scripts/serve_site.gos" "$PORT" "$SERVE_DIR"
