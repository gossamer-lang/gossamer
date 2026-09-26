#!/usr/bin/env bash
# Walks each example project, runs it with `gos run`, and builds
# and runs it with `gos build` when BUILD=1 is set in the
# environment.
set -euo pipefail

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
GOSSAMER_ROOT="$(cd "${SELF_DIR}/.." && pwd)"

echo "=> building gos..."
( cd "${GOSSAMER_ROOT}" && cargo build -p gossamer-cli >/dev/null )
GOS="${GOSSAMER_ROOT}/target/debug/gos"

if [[ ! -x "${GOS}" ]]; then
    echo "run_examples.sh: gos binary not produced at ${GOS}" >&2
    exit 2
fi

for ex in 01-gossamer-aware 02-plain-rust-wrapped; do
    echo "=> example: ${ex}"
    cd "${SELF_DIR}/${ex}"

    echo "  -- gos run"
    "${GOS}" run src/main.gos

    if [[ -n "${BUILD:-}" ]]; then
        echo "  -- gos build (debug)"
        "${GOS}" build src/main.gos

        binary="$(ls target/debug/* 2>/dev/null | head -n1 || true)"
        if [[ -x "${binary}" ]]; then
            echo "  -- running ${binary}"
            "${binary}"
        fi
    fi
done

echo "=> all examples ok"
