#!/usr/bin/env bash
# Fast pre-commit gate: the checks that catch what a session most often
# breaks, in a couple of minutes.
#
# Default gates, cheapest first, so the gate that costs seconds reports
# before the one that costs minutes. The times are what each phase takes
# after an edit to the runtime and the interpreter, the common shape:
#
#   formatting    `cargo fmt` - compiles nothing                      (2s)
#   codegen       the ABI / dispatch unit gates - the boundary shapes
#                 that only break on a platform this box never runs   (14s)
#   portability   `cargo check` for wasm32 and for Windows, whose
#                 targets are built in CI and not here                (21s)
#   core          clippy (`--workspace --all-targets -D warnings`)    (77s)
#   generated     the checked-in tables and pages that go stale when a
#                 stdlib module, a CLI argument, or a doc line
#                 changes - the `gos` binary they need dominates      (90s)
#   behavior      every fixture through the pure bytecode VM and through
#                 the JIT, compared
#
# The behavior gate is where the pure-bytecode tier is covered here: the
# sweep runs every fixture once with `GOS_JIT=0` and once with the JIT
# on, so a body that answers differently once promoted is caught in
# seconds. The tier-parity walk carries the same column against the two
# native tiers as well, and it stays out of this script for the reason
# every other CI-mirroring gate does - it builds each fixture natively
# and costs tens of minutes. Run it with
# `cargo test --release -p gossamer-cli --test tier_parity`.
#
# The slow gates that mirror the rest of CI are opt-in, since they take
# tens of minutes and answer questions a dependency bump or an unsafe
# block raises rather than an ordinary edit:
#
#   --rustdoc     `cargo doc` with broken-intra-doc-links denied
#   --audit       cargo-audit (RUSTSEC advisories)
#   --deny        cargo-deny (licenses, bans, sources)
#   --musl        musl cross-compile through `cargo zigbuild`
#   --sanitizers  ASan + TSan on the unsafe-touching crates
#   --fuzz        the fuzz-target smoke run
#   --all         every one of the above
#
# Other flags:
#   --no-sweep    skip the VM-vs-JIT fixture sweep (the slowest default gate)
#   --force-jit   sweep with every body promoted at its first call
#   --verbose     show every line rather than warnings and errors only
#
# Missing optional tools cause a clean skip rather than a failure.
set -euo pipefail

# Force full backtraces so a panicking step (e.g. a failed `cargo test`)
# replays its stack in the captured output, not just the panic message and
# a note to set this. A caller's explicit RUST_BACKTRACE wins.
export RUST_BACKTRACE="${RUST_BACKTRACE:-full}"

verbose=0
run_sweep=1
force_jit=0
run_sanitizers=0
run_fuzz=0
run_musl=0
run_audit=0
run_deny=0
run_rustdoc=0
for arg in "$@"; do
    case "$arg" in
        --verbose|--full) verbose=1 ;;
        --no-sweep)      run_sweep=0 ;;
        --force-jit)     force_jit=1 ;;
        --sanitizers)    run_sanitizers=1 ;;
        --fuzz)          run_fuzz=1 ;;
        --musl)          run_musl=1 ;;
        --audit)         run_audit=1 ;;
        --deny)          run_deny=1 ;;
        --rustdoc)       run_rustdoc=1 ;;
        --all)
            run_sanitizers=1
            run_fuzz=1
            run_musl=1
            run_audit=1
            run_deny=1
            run_rustdoc=1
            ;;
        -h|--help)
            sed -n '2,/^set -euo pipefail/p' "$0" | sed 's/^# \{0,1\}//' | head -n -1
            exit 0
            ;;
        *)
            echo "unknown arg: $arg (try --help)" >&2
            exit 2
            ;;
    esac
done

started_at=$SECONDS

# Run a step and surface only warnings / errors by default. Each
# step's stdout+stderr is captured to a temp file; if the step
# exits non-zero we print the whole capture so the failure is
# diagnosable. On success we grep the capture for warning / error
# lines and emit those (cargo and clang-style diagnostics both use
# `error:` / `warning:` prefixes, which is what we match).
run_step() {
    local label="$1"
    shift
    local log step_started
    log="$(mktemp)"
    step_started=$SECONDS
    echo "==> $label"
    if [[ $verbose -eq 1 ]]; then
        if ! "$@" 2>&1 | tee "$log"; then
            rm -f "$log"
            exit 1
        fi
    else
        if ! "$@" >"$log" 2>&1; then
            echo "$label FAILED - full output:" >&2
            cat "$log" >&2
            rm -f "$log"
            exit 1
        fi
        # Surface warning / error lines (plus a couple of lines of
        # following context - diagnostics usually print the source
        # excerpt right after the header) so problems aren't silent
        # even when the step succeeds with warnings.
        grep -E -i -A 2 '^(warning|error)[:\[]|: warning:|: error:' "$log" || true
    fi
    rm -f "$log"
    echo "    ($((SECONDS - step_started))s)"
}

phase() {
    echo
    echo "-- $1 --"
}

# `cargo fmt` compiles nothing, so it reports before any other gate has
# finished linking.
phase "formatting gate"
run_step "cargo fmt"                                       cargo fmt

# The boundary shapes. A wrong one is invisible on this machine - it
# miscompiles on a target the dev box never runs - so these unit gates
# stand in for the platform: the Cranelift ABI tests build a Win64 ISA on
# any host, and the dispatch-parity test proves every runtime helper the
# codegen names is one the runtime defines. Two crates' worth of build,
# which is why they precede the whole-workspace gates.
phase "codegen boundary gates"
run_step "cargo test -p gossamer-codegen-cranelift --lib" \
    cargo test -p gossamer-codegen-cranelift --lib
run_step "cargo test -p gossamer-codegen-cranelift --test dispatch_parity" \
    cargo test -p gossamer-codegen-cranelift --test dispatch_parity

# Targets CI builds and this machine does not. `wasm32` breaks whenever a
# crate picks up a native-only dependency; the Windows target catches the
# `#[cfg(unix)]`-shaped edit that compiles fine here. A crate subset per
# target rather than the workspace, so both land before clippy does.
phase "portability gates"
if rustup target list --installed 2>/dev/null | grep -q '^wasm32-unknown-unknown$'; then
    run_step "cargo check --target wasm32-unknown-unknown (wasm-portable crates)" \
        cargo check -p gossamer-abi -p gossamer-binding-macros \
        -p gossamer-interp -p gossamer-lsp -p gossamer-playground \
        --target wasm32-unknown-unknown
else
    echo "wasm32 check skipped (run \`rustup target add wasm32-unknown-unknown\` to enable)"
fi
# The browser build is the only tier nothing else executes: a `cargo check`
# proves it compiles, and a Rust panic under it aborts the module at run time.
if rustup target list --installed 2>/dev/null | grep -q '^wasm32-unknown-unknown$' \
    && command -v wasm-bindgen >/dev/null 2>&1 && command -v node >/dev/null 2>&1; then
    run_step "playground wasm smoke (every fixture must answer)" \
        cargo run --quiet --bin gos -- run scripts/playground_smoke.gos
else
    echo "playground smoke skipped (needs the wasm32 target, wasm-bindgen, and node)"
fi
if rustup target list --installed 2>/dev/null | grep -q '^x86_64-pc-windows-gnu$'; then
    run_step "cargo check --target x86_64-pc-windows-gnu (runtime + backends)" \
        cargo check -p gossamer-runtime -p gossamer-codegen-cranelift \
        -p gossamer-codegen-llvm -p gossamer-interp \
        --target x86_64-pc-windows-gnu
else
    echo "windows check skipped (run \`rustup target add x86_64-pc-windows-gnu\` to enable)"
fi

# The whole workspace, every target, under clippy - the broadest of the
# compile-bound gates, so the narrower ones above have already reported.
phase "core Rust gates"
run_step "cargo clippy --workspace --all-targets"          cargo clippy --workspace --all-targets -- -D warnings

# The generated tables and pages that track the stdlib manifest and the
# CLI surface. Each check itself runs in seconds - what an ordinary edit
# (a new stdlib module, a reworded argument, a moved doc line) makes
# stale - but they need the `gos` binary, so the phase carries a full
# workspace build.
phase "generated-artifact gates"
run_step "cargo build --bin gos"                           cargo build --bin gos
run_step "gos doc --emit-stdlib --check"                   ./target/debug/gos doc --emit-stdlib docs_src/stdlib --check
run_step "cargo xtask docs-llm --check"                    cargo xtask docs-llm --check
run_step "cargo xtask item-fixtures --check"               cargo xtask item-fixtures --check
# Do not filter by status: a filtered check can hide a broken contract class.
run_step "gos feature-status --check"                      ./target/debug/gos feature-status --check
run_step "cargo test -p gossamer-std --test resolver_manifest_items" \
    cargo test -p gossamer-std --test resolver_manifest_items
run_step "cargo test -p gossamer-resolve --lib stdlib_exports" \
    cargo test -p gossamer-resolve --lib stdlib_exports
# The C-ABI registry is binary-searched, so an entry filed out of order
# hides every entry past it; the drift check catches a name the resolver
# still exports after the runtime stopped registering it. Both are
# table-shaped edits a session makes without running the crate that owns
# the table, and both are seconds once `gos` is built.
run_step "cargo test -p gossamer-abi" cargo test -p gossamer-abi
run_step "cargo test -p gossamer-cli --test dispatch_consistency --test stdlib_export_drift" \
    cargo test -p gossamer-cli --test dispatch_consistency --test stdlib_export_drift
# The whole lib suite rather than one module: it also holds the check
# that the committed tier-parity evidence still names every stdlib module
# a fixture imports, and it finishes in under two seconds.
run_step "cargo test -p gossamer-cli --lib" cargo test -p gossamer-cli --lib

# The workflow definitions themselves. A file that does not parse fails
# the whole CI run before any job starts, which reports only as "this
# run likely failed because of a workflow file issue" - no job, no log,
# no annotation. This gate costs milliseconds and names the line.
run_step "GitHub workflow files parse" \
    "${GOS_HARNESS_BIN:-./target/debug/gos}" run scripts/check_workflows.gos

# Every fixture through both execution paths of `gos run`. The full
# tier-parity walk also builds each fixture natively and takes tens of
# minutes; this compares the two paths that need no compiler invocation,
# which is where a promotion-admission change lands first.
if [[ $run_sweep -eq 1 ]]; then
    phase "behavior gates"
    sweep_args=()
    [[ $force_jit -eq 1 ]] && sweep_args+=(--force-jit)
    # The sweep is itself a Gossamer program. `GOS_HARNESS_BIN` runs it
    # on a different toolchain than the one under test, so a compiler
    # bug fails the fixtures rather than the harness that reports them.
    harness_gos="${GOS_HARNESS_BIN:-./target/debug/gos}"
    run_step "VM-vs-JIT fixture sweep" \
        "$harness_gos" run scripts/jit_parity_sweep.gos "${sweep_args[@]}"
fi

if [[ $run_deny -eq 1 || $run_audit -eq 1 || $run_rustdoc -eq 1 ]]; then
    phase "policy and documentation gates"
fi

# cargo-deny - license + advisory + bans + sources gate
# (`.github/workflows/ci.yml` deny job). Skip cleanly if
# `cargo-deny` isn't installed so the local pass keeps moving.
if [[ $run_deny -eq 1 ]]; then
    if command -v cargo-deny >/dev/null 2>&1; then
        run_step "cargo deny check" cargo deny check
    else
        echo "cargo deny skipped (run \`cargo install cargo-deny\` to enable)"
    fi
fi

# cargo-audit - RUSTSEC advisory gate (`.github/workflows/ci.yml`
# audit job). Skip cleanly if `cargo-audit` isn't installed.
if [[ $run_audit -eq 1 ]]; then
    if command -v cargo-audit >/dev/null 2>&1; then
        run_step "cargo audit" cargo audit
    else
        echo "cargo audit skipped (run \`cargo install cargo-audit --locked\` to enable)"
    fi
fi

# Rustdoc broken-intra-doc-links gate - mirrors the docs job in
# `.github/workflows/ci.yml`. `--document-private-items` matches that job
# exactly: without it rustdoc skips private items, so a broken intra-doc
# link in a private fn's doc comment would pass here and fail there.
if [[ $run_rustdoc -eq 1 ]]; then
    RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links" \
        run_step "cargo doc --workspace --no-deps --document-private-items" \
        cargo doc --workspace --no-deps --document-private-items
fi

# Musl cross-compile check (via `cargo zigbuild`) - mirrors the "Build
# the target runtime archives" step shared by cross-from-linux/-macos/
# -windows in ci.yml. Exercises the native C deps (ring, mimalloc,
# zstd) that only fail to cross-compile at actual build time, not at
# `cargo check` time: a bare cross C compiler has no musl sysroot, and
# even `CC=zig cc` doesn't work standalone (cc-rs's own appended
# `--target=<rustc-triple>` flag is a form zig's parser rejects, and it
# wins over whatever `-target` we pass through `CC`) - `cargo zigbuild`
# is the maintained tool that reconciles the two. Skips cleanly when
# zig (>=0.9.0, cargo-zigbuild's own floor - a distro-packaged `zig`
# binary is commonly older), `cargo-zigbuild`, or the musl rustup
# targets aren't installed, so an unrelated stale system `zig` never
# turns into a hard failure here.
if [[ $run_musl -eq 1 ]]; then
    phase "musl cross gate"
    zig_usable=0
    if command -v zig >/dev/null 2>&1 && command -v cargo-zigbuild >/dev/null 2>&1; then
        zig_minor="$(zig version 2>/dev/null | awk -F'[.-]' '{print ($1 > 0) ? 9 : $2}')"
        [[ "${zig_minor:-0}" =~ ^[0-9]+$ && "$zig_minor" -ge 9 ]] && zig_usable=1
    fi
    if [[ $zig_usable -eq 1 ]]; then
        for t in aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
            if rustup target list --installed 2>/dev/null | grep -q "^${t}$"; then
                run_step "cargo zigbuild --release --target $t -p gossamer-runtime" \
                    cargo zigbuild --release --target "$t" -p gossamer-runtime
            else
                echo "musl cross check for $t skipped (run \`rustup target add $t\` to enable)"
            fi
        done
    else
        echo "musl cross check skipped (install zig >=0.9.0 + \`cargo install cargo-zigbuild\` to enable)"
    fi
fi

# ASan / TSan - mirrors `.github/workflows/sanitizers.yml`. Needs a
# nightly toolchain with the `rust-src` component (so `-Z build-std`
# can recompile std + the sanitizer runtimes). CI pins a specific
# nightly date for reproducibility; locally we honor that pin when
# it's installed and otherwise fall back to plain `nightly` so dev
# machines stay usable without an extra rustup install.
if [[ $run_sanitizers -eq 1 ]]; then
    phase "sanitizer gates"
    asan_pinned="nightly-2026-04-14"
    asan_toolchain=""
    if rustup toolchain list 2>/dev/null | grep -q "^${asan_pinned}"; then
        asan_toolchain="$asan_pinned"
    else
        # Pick the first `nightly-*` line. `nightly` (no triple) and
        # `nightly-<triple>` both qualify; rustup canonicalises to
        # the triple form on most installs.
        asan_toolchain="$(rustup toolchain list 2>/dev/null \
            | awk '/^nightly/{ sub(/ .*/, ""); print; exit }')"
        if [[ -n "$asan_toolchain" && "$asan_toolchain" != "$asan_pinned" ]]; then
            echo "sanitizers: pinned $asan_pinned not installed; falling back to $asan_toolchain"
            echo "            CI uses the pinned date - install with"
            echo "              rustup toolchain install $asan_pinned --component rust-src"
        fi
    fi
    if [[ -z "$asan_toolchain" ]]; then
        echo "sanitizers skipped (no nightly toolchain - install with"
        echo "  rustup toolchain install $asan_pinned --component rust-src)"
    elif ! rustup component list --installed --toolchain "$asan_toolchain" 2>/dev/null \
            | grep -q rust-src; then
        echo "sanitizers skipped (run \`rustup component add rust-src --toolchain $asan_toolchain\` to enable)"
    else
        # ASan tests touch sigaltstack-dependent code in
        # gossamer-runtime; the public `stack_guard::install()`
        # honors ASAN_OPTIONS by skipping our handler so libasan's
        # signal stack stays the only one in play.
        RUSTFLAGS="-Z sanitizer=address" \
        RUSTDOCFLAGS="-Z sanitizer=address" \
        ASAN_OPTIONS="detect_leaks=0:abort_on_error=1:halt_on_error=1" \
            run_step "cargo +$asan_toolchain test (ASan)" \
            cargo "+$asan_toolchain" test \
                -Z build-std \
                --target x86_64-unknown-linux-gnu \
                --lib \
                -p gossamer-runtime \
                -p gossamer-interp \
                -p gossamer-coro \
                -p gossamer-mir \
                -p gossamer-binding

        RUSTFLAGS="-Z sanitizer=thread" \
        RUSTDOCFLAGS="-Z sanitizer=thread" \
        TSAN_OPTIONS="halt_on_error=1:second_deadlock_stack=1" \
            run_step "cargo +$asan_toolchain test (TSan)" \
            cargo "+$asan_toolchain" test \
                -Z build-std \
                --target x86_64-unknown-linux-gnu \
                --lib \
                -p gossamer-runtime \
                -p gossamer-sched \
                -p gossamer-coro
    fi
fi

# Fuzz smoke - mirrors `.github/workflows/fuzz.yml` so adversarial
# inputs that CI would flag also fail locally. Each target runs
# briefly (10 s by default; override with GOSSAMER_FUZZ_SECS) and
# replays its seed corpus.
if [[ $run_fuzz -eq 1 ]]; then
    fuzz_secs="${GOSSAMER_FUZZ_SECS:-10}"
    if command -v cargo-fuzz >/dev/null 2>&1 && rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
        phase "fuzz smoke gates"
        echo "==> fuzz smoke (${fuzz_secs}s per target)"
        fuzz_log="$(mktemp -d)/fuzz.log"
        for target in lex parse manifest http_request typecheck resolve mir_lower hir_lower vm_compile vm_run; do
            if [[ $verbose -eq 1 ]]; then
                echo "  -> $target"
                if ! ( cd fuzz && cargo +nightly fuzz run "$target" -- \
                        -max_total_time="$fuzz_secs" -max_len=65536 -rss_limit_mb=2048 -malloc_limit_mb=2048 -timeout=30 ); then
                    exit 1
                fi
            else
                if ! ( cd fuzz && cargo +nightly fuzz run "$target" -- \
                        -max_total_time="$fuzz_secs" -max_len=65536 -rss_limit_mb=2048 -malloc_limit_mb=2048 -timeout=30 ) \
                        >"$fuzz_log" 2>&1; then
                    echo "fuzz target '$target' failed:" >&2
                    tail -c 4096 "$fuzz_log" >&2
                    exit 1
                fi
            fi
        done
        rm -f "$fuzz_log"
    else
        echo "fuzz smoke skipped (need nightly toolchain + 'cargo install cargo-fuzz')"
    fi
fi

# `set -e` aborts on the first failing step, so reaching here means every gate
# that ran passed. Print an explicit terminal banner: a non-zero exit already
# signals failure, but piping this script (e.g. `./quick-check.sh | tail`) discards
# its exit code, so the banner is the in-stream success/failure signal.
echo
echo "quick-check.sh: ALL QUICK GATES PASSED in $((SECONDS - started_at))s"
