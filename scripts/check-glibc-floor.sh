#!/usr/bin/env sh
# Fails when an ELF binary needs a glibc newer than $GLIBC_FLOOR, the oldest
# glibc the Linux releases support.
#
#   GLIBC_FLOOR=2.39 scripts/check-glibc-floor.sh target/release/gos
set -eu
floor="${GLIBC_FLOOR:?set GLIBC_FLOOR, e.g. 2.39}"
status=0
for binary in "$@"; do
    needed=$(readelf --version-info --wide "$binary" |
        grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n 1)
    if [ -z "$needed" ]; then
        echo "$binary: needs no versioned glibc symbol"
    elif [ "$(printf '%s\n%s\n' "$needed" "$floor" | sort -V | tail -n 1)" = "$floor" ]; then
        echo "$binary: needs glibc $needed, within the $floor floor"
    else
        echo "::error::$binary needs glibc $needed, above the $floor floor" >&2
        status=1
    fi
done
exit "$status"
