#!/usr/bin/env bash
# The native test lanes of CI, built once per runner image and run in shards.
#
#   native-tests.sh build        compile every shard's test targets in one Cargo
#                                pass and pack the executables, `gos`, and the
#                                runtime archives into native-tests.tar.{zst,gz}
#   native-tests.sh run SUITE    unpack that build and run one shard:
#                                general-0..2 or parity-A-B
#
# Run from the repository root. The pack keeps workspace-relative paths, and a
# shard unpacks it into the same checkout path on the same runner image, so the
# absolute paths Cargo baked into the test binaries still resolve.
#
# These crates fork llc/linkers and execute freshly built native binaries, so a
# miscompile manifests as the test process dying by a signal (SIGSEGV/SIGBUS on
# Unix; a Windows access violation that can drop the whole runner). Each test
# binary therefore runs in its OWN process, serially, with live output teed to a
# per-binary file, so the culprit test is the last streamed line even when the
# crash takes the runner down. A single combined `cargo test` would run the
# binaries concurrently, interleave their output, and report only "N targets
# failed". Full LLVM IR dumps and preserved build temps are opt-in via
# GOSSAMER_NATIVE_CI_DIAGNOSTICS=1 because keeping them for every passing native
# build can exhaust hosted runner storage before failure artifacts can upload.
set -uo pipefail

mode="${1:-}"
workspace="$(pwd)"
# Redirect the harness scratch dirs (and gos's own build temp) at the
# collectable path. Exported after `mkdir`, not through the step's `env:`, so
# git-bash on Windows starts with a temp dir that exists.
artifacts="$workspace/native-build-artifacts"
mkdir -p "$artifacts"
export TMPDIR="$artifacts" TEMP="$artifacts" TMP="$artifacts"
logdir="$artifacts/test-logs"
mkdir -p "$logdir"
plan_dir="target/native-tests"
executable_artifacts="$plan_dir/executables.tsv"
pack_base="native-tests.tar"
if [ "${GOSSAMER_NATIVE_CI_DIAGNOSTICS:-}" = "1" ]; then
  export GOS_LLVM_DUMP=1
  export GOS_LINK_VERBOSE=1
  export GOS_KEEP_BUILD_ARTIFACTS=1
  echo "::notice::native CI diagnostics enabled; preserving LLVM IR and build temps"
fi
rc=0

package_dir() {
  case "$1" in
    gossamer-cli) echo "crates/gossamer-cli" ;;
    gossamer-codegen-cranelift) echo "crates/gossamer-codegen-cranelift" ;;
    *) echo "::error::unknown native-test crate: $1" >&2; return 1 ;;
  esac
}

normalize_executable() {
  local exe="$1"
  if command -v cygpath >/dev/null 2>&1 && [[ "$exe" =~ ^[A-Za-z]:\\ ]]; then
    cygpath -u "$exe"
  else
    printf '%s\n' "$exe"
  fi
}

native_pwd() {
  if pwd -W >/dev/null 2>&1; then
    pwd -W
  else
    pwd
  fi
}

# The passing output streams inside a collapsed `::group::` so the log stays
# tidy. GitHub does not auto-expand a group, so a failure also emits a
# top-level `::error::` annotation and re-prints the failing test names and
# panic / assertion lines FLAT, outside the group. The full streamed log stays
# in the group and in the uploaded artifacts.
run_executable() {
  local crate="$1" label="$2" exe="$3"; shift 3
  local log="$logdir/${crate}-${label}.log"
  local ok=1
  local dir
  dir="$(package_dir "$crate")" || return 1
  exe="$(normalize_executable "$exe")"
  echo "::group::test ${crate} :: ${label}"
  (
    cd "$dir"
    export CARGO_MANIFEST_DIR="$(native_pwd)"
    if [ -n "${gos_exe:-}" ]; then
      export CARGO_BIN_EXE_gos="$gos_exe"
    fi
    "$exe" "$@" --test-threads=1 --nocapture
  ) 2>&1 | tee "$log" || ok=0
  echo "::endgroup::"
  if [ "$ok" -ne 1 ]; then
    rc=1
    echo "::error::test ${crate} :: ${label} FAILED - see flat detail below / uploaded artifacts"
    echo "===================== FAILED: ${crate} :: ${label} ====================="
    grep -nE "panicked at|^test .* \.\.\. FAILED|^error(\[|:)|^failures:|assertion .*failed|left ==|right ==|diverged|Undefined symbols|linker command failed" \
      "$log" | head -n 80 || true
    echo "============ end ${crate} :: ${label} (full log above is collapsed) ============"
  fi
}

# The macOS and Windows runners exist to catch failures that appear ONLY off
# Linux - platform ABI / calling convention, object format, linking, OS
# syscalls, and the Cranelift JIT / LLVM codegen. A test therefore earns its
# cross-platform run only if its outcome can differ by OS. The binaries below
# cannot: they either execute no generated code at all (front-end check /
# parse / typecheck, the `gos fmt` / `gos doc` / feature-status tooling text,
# static source and symbol-registry scans, a pure-Rust unit test) or they
# construct a target object in memory deterministically. Those tests run once
# in the non-native Ubuntu job, instead of consuming four serial native lanes.
# Anything not in the lists below remains cross-platform by default, so new
# tests cannot silently lose native coverage. `codegen_correct` is a
# platform-independent language corpus and runs once on Linux; the tier-parity
# shards retain generated-code coverage on every OS. `rc_soundness` is
# Linux-only because it self-skips elsewhere.
non_native_cli="
  client_error_display dispatch_consistency doc_emit_stdlib docs_assets
  deque_drain_is_linear ecosystem_bindings feature_status fmt_faithful
  goroutine_worker_reuse
  http_h2_alpn http_h2_conformance mcp runtime_archive_parity
  stdlib_compiled_coverage stdlib_export_drift
"
non_native_cranelift="
  coff_pic_reloc dispatch_parity emit macho_dead_strip
"
linux_only="
  codegen_correct rc_soundness
"

in_list() {
  local list="$1" name="$2" item
  for item in $list; do [ "$item" = "$name" ] && return 0; done
  return 1
}

skip_off_linux() {
  case "${RUNNER_OS:-Linux}" in Linux) return 1 ;; esac
  in_list "$linux_only" "$1"
}

# Prints the `crate label kind target` rows general shard $1 runs. With $2 set,
# also announces each target the general shards leave to another job.
plan_general() {
  local shard="$1" announce="${2:-}" index=0 crate f name
  in_shard() {
    local assigned
    case "$1:$2" in
      gossamer-cli:cli)
        # The two dominant targets are pinned to shards of their own so the
        # round-robin below cannot land them together. Off Linux
        # codegen_correct is covered by the tier parity lanes, so put cli on
        # the otherwise lighter shard.
        case "${RUNNER_OS:-Linux}" in
          Linux) assigned=0 ;;
          *) assigned=1 ;;
        esac
        ;;
      gossamer-codegen-cranelift:codegen_correct) assigned=1 ;;
      gossamer-codegen-cranelift:lib) assigned=0 ;;
      *) assigned=$((index % 3)) ;;
    esac
    index=$((index + 1))
    [ "$assigned" = "$shard" ]
  }
  notice() {
    [ -n "$announce" ] && echo "::notice::$1" >&2
    return 0
  }
  for crate in gossamer-cli gossamer-codegen-cranelift; do
    if [ "$crate" != gossamer-cli ] && in_shard "$crate" lib; then
      printf '%s\t%s\t%s\t%s\n' "$crate" lib lib gossamer_codegen_cranelift
    fi
    for f in crates/"$crate"/tests/*.rs; do
      [ -e "$f" ] || continue
      name="$(basename "$f" .rs)"
      # Tier parity has its own shards.
      [ "$name" = tier_parity ] && continue
      if [ "$crate" = gossamer-cli ] && [ "$name" = repl ]; then
        notice "skip ${crate}::${name}; covered by Test non-native CLI and codegen"
        continue
      fi
      # Typing is a property of the language, not of a target, so the gate
      # runs once in its own job rather than once per OS shard.
      if [ "$crate" = gossamer-cli ] && [ "$name" = type_system_gate ]; then
        notice "skip ${crate}::${name}; covered by the Type system gate job"
        continue
      fi
      if [ "$crate" = gossamer-cli ] && in_list "$non_native_cli" "$name"; then
        notice "skip ${crate}::${name}; covered by Test non-native CLI and codegen"
        continue
      fi
      if [ "$crate" = gossamer-codegen-cranelift ] && in_list "$non_native_cranelift" "$name"; then
        notice "skip ${crate}::${name}; covered by Test non-native CLI and codegen"
        continue
      fi
      if skip_off_linux "$name"; then
        notice "skip ${crate}::${name} on ${RUNNER_OS} (covered on Linux and by cross-platform tier parity)"
        continue
      fi
      in_shard "$crate" "$name" || continue
      printf '%s\t%s\t%s\t%s\n' "$crate" "$name" test "$name"
    done
  done
}

executable_for_target() {
  local crate="$1" kind="$2" target="$3"
  awk -F '\t' -v crate="$crate" -v kind="$kind" -v target="$target" '
    $1 ~ "/crates/" crate "#" && $2 ~ kind && $3 == target {
      print $4
      found = 1
      exit
    }
    END { if (!found) exit 1 }
  ' "$executable_artifacts"
}

# Workspace-relative spelling of an absolute path Cargo reported.
relative_path() {
  local path
  path="$(normalize_executable "$1")"
  printf '%s\n' "${path#"$workspace"/}"
}

build() {
  mkdir -p "$plan_dir"
  local shard
  for shard in 0 1 2; do
    plan_general "$shard" "$([ "$shard" = 0 ] && echo announce)" > "$plan_dir/general-$shard.tsv"
  done
  printf '%s\t%s\t%s\t%s\n' gossamer-cli tier_parity test tier_parity > "$plan_dir/parity.tsv"

  local build_args=(test --no-run --message-format=json)
  local crate label kind target
  while IFS=$'\t' read -r crate label kind target; do
    [[ " ${build_args[*]} " == *" -p $crate "* ]] || build_args+=(-p "$crate")
    case "$kind" in
      lib) [[ " ${build_args[*]} " == *" --lib "* ]] || build_args+=(--lib) ;;
      test) build_args+=(--test "$target") ;;
    esac
  done < <(cat "$plan_dir"/general-*.tsv "$plan_dir/parity.tsv")

  local json_log="$logdir/00-build.jsonl"
  echo "::group::build every native test target"
  if ! "$CARGO_BIN" "${build_args[@]}" > "$json_log" 2> >(tee "$logdir/00-build.log" >&2); then
    echo "::endgroup::"
    echo "::error::native test target build failed - see build log"
    return 1
  fi
  echo "::endgroup::"
  jq -r '
    select(.reason == "compiler-artifact" and .executable != null)
    | [.package_id, (.target.kind | join(",")), .target.name, .executable]
    | @tsv
  ' "$json_log" > "$executable_artifacts" || {
    echo "::error::failed to parse Cargo JSON build output"
    return 1
  }

  # Every planned executable (with its Windows debug database), the files
  # `gos` resolves beside itself, every runtime archive the build baked a
  # path to (the static-musl one sits in its own nested target directory),
  # and the plans themselves.
  local list="$logdir/pack-list.txt" exe pdb
  find target/debug -maxdepth 1 -type f ! -name '.cargo-lock' > "$list"
  find target -maxdepth 4 -type f \
    \( -name 'libgossamer_runtime*.a' -o -name 'gossamer_runtime*.lib' \) \
    ! -path '*/deps/*' >> "$list"
  find "$plan_dir" -type f >> "$list"
  while IFS=$'\t' read -r crate label kind target; do
    exe="$(executable_for_target "$crate" "$kind" "$target")" || {
      echo "::error::missing built executable for ${crate} :: ${label}"
      return 1
    }
    exe="$(relative_path "$exe")"
    echo "$exe" >> "$list"
    pdb="${exe%.exe}.pdb"
    if [ "$pdb" != "$exe" ] && [ -f "$pdb" ]; then
      echo "$pdb" >> "$list"
    fi
  done < <(cat "$plan_dir"/general-*.tsv "$plan_dir/parity.tsv")
  sort -u -o "$list" "$list"
  if command -v zstd >/dev/null 2>&1; then
    tar -cf - -T "$list" | zstd -T0 -3 -q -o "$pack_base.zst"
  else
    tar -cf - -T "$list" | gzip -1 > "$pack_base.gz"
  fi
  ls -l "$pack_base".*
}

run_suite() {
  local suite="$1"
  if [ -f "$pack_base.zst" ]; then
    zstd -dc "$pack_base.zst" | tar -xf -
  elif [ -f "$pack_base.gz" ]; then
    gzip -dc "$pack_base.gz" | tar -xf -
  else
    echo "::error::no packed native test build ($pack_base.zst or .gz) in $workspace"
    return 1
  fi
  df -h "$TMPDIR" || true
  gos_exe="$(
    awk -F '\t' '$2 ~ /bin/ && $3 == "gos" { print $4; exit }' "$executable_artifacts"
  )"
  local crate label kind target exe
  case "$suite" in
    general-[0-2])
      while IFS=$'\t' read -r crate label kind target; do
        [ -n "$crate" ] || continue
        exe="$(executable_for_target "$crate" "$kind" "$target")" || {
          rc=1
          echo "::error::missing built executable for ${crate} :: ${label}"
          continue
        }
        run_executable "$crate" "$label" "$exe"
      done < "$plan_dir/${suite}.tsv"
      ;;
    parity-*-*)
      exe="$(executable_for_target gossamer-cli test tier_parity)" || {
        echo "::error::missing built executable for gossamer-cli :: tier_parity"
        return 1
      }
      local groups="${suite#parity-}" group
      local first="${groups%-*}" last="${groups#*-}"
      if [ "$first" = 0 ]; then
        run_executable gossamer-cli tier_parity_vm "$exe" \
          vm_runs_every_example_without_crashing
      fi
      for group in $(seq "$first" "$last"); do
        run_executable gossamer-cli "tier_parity_bytecode_${group}" \
          "$exe" "bytecode_parity_group_${group}"
        run_executable gossamer-cli "tier_parity_cranelift_${group}" \
          "$exe" "cranelift_parity_group_${group}"
        run_executable gossamer-cli "tier_parity_llvm_${group}" \
          "$exe" "llvm_parity_group_${group}"
        run_executable gossamer-cli "tier_parity_llvm_debug_${group}" \
          "$exe" "llvm_debug_parity_group_${group}"
        run_executable gossamer-cli "tier_parity_strict_${group}" \
          "$exe" "llvm_strict_lower_group_${group}"
      done
      # The dedicated tests - servers, protocol fixtures, the harness's own
      # checks - are every test the group walks above leave out.
      if [ "$last" = 5 ]; then
        run_executable gossamer-cli tier_parity_dedicated "$exe" \
          --skip _parity_group_ --skip llvm_strict_lower_group_ \
          --skip vm_runs_every_example_without_crashing
      fi
      ;;
    *)
      echo "::error::unknown native test suite: $suite"
      return 1
      ;;
  esac
}

case "$mode" in
  build) build || rc=1 ;;
  run) run_suite "${2:?usage: native-tests.sh run SUITE}" || rc=1 ;;
  *) echo "usage: native-tests.sh build | run SUITE" >&2; exit 2 ;;
esac
exit "$rc"
