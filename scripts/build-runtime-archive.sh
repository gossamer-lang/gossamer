#!/usr/bin/env bash
# Builds the runtime archive compiled programs link, for one target triple,
# with the codegen flags the CLI build script gives the archives it builds
# (`build_runtime_into` in crates/gossamer-cli/build.rs). Release jobs use it
# for every archive the CLI build script does not produce for them, so a
# program links the same runtime whichever archive it gets.
#
# usage: scripts/build-runtime-archive.sh TRIPLE [build|zigbuild]
#
# The flags are added to any RUSTFLAGS already set (a target CPU, a static
# relocation model).
set -euo pipefail

triple="${1:?usage: $0 TRIPLE [build|zigbuild]}"
subcommand="${2:-build}"

flags="-C force-frame-pointers=yes --cfg gos_program_archive"
case "$triple" in
    x86_64-*) flags="$flags -C llvm-args=-align-all-functions=6" ;;
esac
case "$triple" in
    *-linux-musl) flags="$flags -C relocation-model=static" ;;
esac

RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$flags" \
    cargo "$subcommand" --release --target "$triple" -p gossamer-runtime
