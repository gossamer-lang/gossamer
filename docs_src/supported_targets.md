# Supported Targets

The executable matrix in
[`conformance/target_matrix.tsv`](https://github.com/gossamer-lang/gossamer/blob/main/conformance/target_matrix.tsv)
is the release contract. `stable_tier_conformance` checks that it remains
registered and that this page and `SPEC.md` name the same support classes.
The matrix is evidence-based: accepting a triple locally is not support.

## Tier 1

Tier 1 targets execute the pure bytecode VM, the JIT-enabled VM, and LLVM AOT
fixtures on native CI. They are the supported execution contract.

| Target triple | Native CI evidence |
|---|---|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| `aarch64-apple-darwin` | `xcode-27` (macOS 27) |
| `x86_64-pc-windows-msvc` | `windows-latest` |

## Tier 2

Tier 2 covers Linux-musl AOT output. CI builds it from the supported hosts,
executes it natively or under QEMU, and compares its output with the pure
bytecode VM. This is supported for release AOT deployment, but not evidence
that every host-side toolchain path has native execution coverage.

| Target triple | Differential evidence |
|---|---|
| `x86_64-unknown-linux-musl` | native/QEMU AOT-versus-VM fixture differential |
| `aarch64-unknown-linux-musl` | QEMU AOT-versus-VM fixture differential |

## Artifact-only

`x86_64-apple-darwin` has a release-artifact build in CI, but that artifact is
not executed by the target matrix. It is distributed for evaluation and is not
a supported execution target until native or emulator-backed all-tier evidence
is added.

## Registered, unsupported

The driver also recognises `riscv64gc-unknown-linux-gnu`,
`wasm32-unknown-unknown`, and `wasm32-wasi` for targeted compile checks. They
are not supported execution targets: the full runtime/scheduler and all-tier
conformance suite do not run there. In particular, `gos build --target
wasm32-...` is not a supported deployment path.

Targets not listed above are Experimental. A local build, a generated object
file, or a package artifact alone does not promote a target, ABI, allocator,
or standard-library surface to Stable.

## macOS deployment target

Release artifacts, the embedded runtime, user Rust bindings, and executables
produced by `gos build` use `MACOSX_DEPLOYMENT_TARGET=15.0`. macOS 15 is the
supported deployment baseline, and the native build regression checks it on
the macOS 27 `xcode-27` GitHub Actions runner.

Building the complete toolchain from source for macOS 11 through 14 is allowed
as an unsupported, best-effort configuration. Export one target for both the
toolchain build and every later `gos build` invocation so the runtime archive,
Rust bindings, and final executable agree:

```sh
export MACOSX_DEPLOYMENT_TARGET=11.0
cargo build --release -p gossamer-cli
./target/release/gos build --release path/to/main.gos
```

Replace `11.0` with the desired deployment target from `11.0` through `14.x`.
These older targets do not run in Gossamer CI and are not supported release
configurations. Building requires an installed Xcode or Command Line Tools SDK
that can target the selected macOS version.
