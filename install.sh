#!/usr/bin/env bash
# Build and install Gossamer (`gos` toolchain + runtime static lib)
# for the host platform.
#
# Usage: ./install.sh [--system]
#   --system  Install system-wide:
#               Linux/macOS: /usr/local (requires sudo)
#               Windows:     %ProgramFiles%\Gossamer (requires admin)
#   Default:  Install per-user:
#               Linux/macOS: ~/.local
#               Windows:     %LOCALAPPDATA%\Programs\Gossamer
#
# After install, `gos` lives at <prefix>/bin/gos and the runtime
# static library at <prefix>/lib/libgossamer_runtime.a (or
# gossamer_runtime.lib on Windows). The CLI's
# `find_runtime_lib` walks `<exe_parent>/../lib/` so the standard
# layout is auto-discovered with no `GOS_RUNTIME_LIB` env needed.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

SYSTEM=0
for arg in "$@"; do
    case "$arg" in
        --system) SYSTEM=1 ;;
        -h|--help)
            sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "error: unknown argument: $arg" >&2; exit 1 ;;
    esac
done

die() { echo "error: $*" >&2; exit 1; }

app_version() {
    awk -F'"' '
        /^\[workspace\.package\]$/ { in_section = 1; next }
        /^\[/ { in_section = 0 }
        in_section && $1 ~ /^version = / { print $2; exit }
    ' "$SCRIPT_DIR/Cargo.toml"
}

build_release() {
    command -v cargo >/dev/null 2>&1 \
        || die "cargo not found on PATH; install rustup from https://rustup.rs"
    echo "==> building release binaries (cargo build --release -p gossamer-cli -p gossamer-runtime)"
    cargo build --release -p gossamer-cli -p gossamer-runtime
}

# Copies `src` over `dest` through a staged sibling. Replacing a file the
# kernel still has mapped - `gos` itself, or an archive a concurrent link is
# reading - fails with ETXTBSY on a plain write, while a rename swaps the
# inode and leaves every open handle on the old one.
install_over() {
    local src="$1" dest="$2" mode="$3" sudo_cmd="$4"
    local staged="${dest}.install.$$"
    trap "${sudo_cmd} rm -f '${staged}'" EXIT HUP INT TERM
    $sudo_cmd cp "$src" "$staged"
    $sudo_cmd chmod "$mode" "$staged"
    $sudo_cmd mv -f "$staged" "$dest"
    trap - EXIT HUP INT TERM
}

install_unix() {
    local exe_name="$1"      # gos
    local lib_name="$2"      # libgossamer_runtime.a
    local sudo_cmd=""        # populated for --system
    local musl_lib_name="libgossamer_runtime-musl.a"

    build_release

    local exe="$SCRIPT_DIR/target/release/$exe_name"
    local lib="$SCRIPT_DIR/target/release/$lib_name"
    local musl_lib="$SCRIPT_DIR/target/release/$musl_lib_name"
    [ -f "$exe" ] || die "binary not found at $exe"
    [ -f "$lib" ] || die "runtime lib not found at $lib"

    local prefix bin_dir lib_dir
    if [ "$SYSTEM" -eq 1 ]; then
        prefix=/usr/local
        sudo_cmd=sudo
    else
        prefix="$HOME/.local"
    fi
    bin_dir="$prefix/bin"
    lib_dir="$prefix/lib"

    $sudo_cmd mkdir -p "$bin_dir" "$lib_dir"
    install_over "$exe" "$bin_dir/$exe_name" 755 "$sudo_cmd"
    install_over "$lib" "$lib_dir/$lib_name" 644 "$sudo_cmd"

    # `gos build --release` on Linux links the static-musl runtime out of
    # `<prefix>/lib`, which outranks the archive baked into this tree. The two
    # archives are one toolchain and export one `gos_rt_*` surface, so the
    # install either replaces both or leaves the musl slot empty, where the
    # release link reports the fallback instead of resolving half a surface
    # against the other half.
    if [ -f "$musl_lib" ]; then
        install_over "$musl_lib" "$lib_dir/$musl_lib_name" 644 "$sudo_cmd"
    elif [ -e "$lib_dir/$musl_lib_name" ]; then
        $sudo_cmd rm -f "$lib_dir/$musl_lib_name"
        echo "Removed $lib_dir/$musl_lib_name; this build produced no musl runtime"
    fi

    # macOS only: ad-hoc resign so the freshly-copied binary
    # passes Gatekeeper / SIP for execution outside the build dir.
    if [ "$(uname)" = "Darwin" ]; then
        ${sudo_cmd:-} xattr -cr "$bin_dir/$exe_name" 2>/dev/null || true
        ${sudo_cmd:-} codesign --force --sign - --timestamp=none \
            "$bin_dir/$exe_name" >/dev/null 2>&1 || true
    fi

    local version="$(app_version)"
    echo
    echo "Installed gos ${version:-?} to $bin_dir/$exe_name"
    echo "Runtime lib at $lib_dir/$lib_name"
    if [ -f "$lib_dir/$musl_lib_name" ]; then
        echo "Static-musl runtime lib at $lib_dir/$musl_lib_name"
    fi

    if [ "$SYSTEM" -eq 0 ] && [[ ":${PATH}:" != *":$bin_dir:"* ]]; then
        echo
        echo "Note: $bin_dir is not in PATH - add it to your shell profile:"
        case "${SHELL##*/}" in
            zsh)  echo "    echo 'export PATH=\"$bin_dir:\$PATH\"' >> ~/.zshrc" ;;
            fish) echo "    fish_add_path $bin_dir" ;;
            *)    echo "    echo 'export PATH=\"$bin_dir:\$PATH\"' >> ~/.bashrc" ;;
        esac
    fi
}

install_windows() {
    build_release

    local exe="$SCRIPT_DIR/target/release/gos.exe"
    local lib="$SCRIPT_DIR/target/release/gossamer_runtime.lib"
    [ -f "$exe" ] || die "binary not found at $exe"
    [ -f "$lib" ] || die "runtime lib not found at $lib"

    local prefix bin_dir lib_dir
    if [ "$SYSTEM" -eq 1 ]; then
        # `ProgramFiles` is set in the bash environment under Git Bash /
        # MSYS2; fall back to the conventional path on a stripped env.
        prefix="${PROGRAMFILES:-/c/Program Files}/Gossamer"
    else
        # Per-user install: `%LOCALAPPDATA%\Programs\Gossamer`. The
        # `Programs` subdir is the convention Squirrel/installers use
        # for per-user CLI tools.
        local local_app
        local_app="${LOCALAPPDATA:-$HOME/AppData/Local}"
        prefix="$local_app/Programs/Gossamer"
    fi
    bin_dir="$prefix/bin"
    lib_dir="$prefix/lib"

    mkdir -p "$bin_dir" "$lib_dir" \
        || die "could not create $prefix - re-run from an admin shell for --system"
    cp "$exe" "$bin_dir/gos.exe"
    cp "$lib" "$lib_dir/gossamer_runtime.lib"

    local version="$(app_version)"
    echo
    echo "Installed gos.exe ${version:-?} to $bin_dir\\gos.exe"
    echo "Runtime lib at $lib_dir\\gossamer_runtime.lib"
    echo
    echo "Note: add $bin_dir to your PATH so 'gos' resolves in any shell:"
    echo "    setx PATH \"%PATH%;$bin_dir\""
}

OS="$(uname -s 2>/dev/null || echo unknown)"
case "$OS" in
    Linux)             install_unix gos libgossamer_runtime.a ;;
    Darwin)            install_unix gos libgossamer_runtime.a ;;
    MINGW*|MSYS*|CYGWIN*) install_windows ;;
    *)                 die "unsupported OS: $OS" ;;
esac
