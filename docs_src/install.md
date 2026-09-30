# Installing Gossamer

Pre-release - the only supported install path today is a source
build.

## From source

```sh
git clone https://github.com/gossamer-lang/gossamer
cd gossamer
cargo build --workspace --release
./target/release/gos --version
```

The `gos` binary is self-contained. Copy it anywhere on your
`PATH`:

```sh
install -m 0755 target/release/gos /usr/local/bin/gos
```

On macOS, a locally built `gos` carries only the linker's ad-hoc
code signature, which the system invalidates once the binary is
moved - a relocated copy is killed at launch with `Killed: 9`.
Re-sign it after copying:

```sh
codesign --force --sign - /usr/local/bin/gos
```

Published release binaries are already re-signed, so this step
applies only to a `cargo build` binary you relocate yourself.

## Dependencies

- **Rust toolchain** - 1.95.0, edition 2024, MSRV 1.95.
  `rust-toolchain.toml` pins the exact version and `profile =
  "minimal"`; rustup installs it on first `cargo` invocation.
  Bumps happen consciously, not via `stable` drift.
- **A C linker** - required by Cargo, not by Gossamer. `cc` /
  `gcc` / `clang` will do.

## Native builds need LLVM

`gos run`, `gos test`, the REPL, and the JIT need nothing beyond the `gos`
binary. `gos build` compiles through LLVM's `opt`, `llc`, and `clang`, which
it finds in this order:

1. `GOS_LLVM_OPT`, `GOS_LLC`, and `GOS_LLVM_CLANG`, each the absolute path of
   one tool.
2. An LLVM bundled with `gos`: `llvm/bin/` beside the binary, or
   `lib/gossamer/llvm/bin/` under its install prefix.
3. The host's LLVM, newest major first: `opt-22` down to `opt-18` on `PATH`,
   then the bare `opt`, then the usual apt, Homebrew, and MSYS2 locations.

LLVM 18 through 22 are supported. 22 is the version the toolchain is built
against; an older major builds the same program and prints a note once. A
major below 18 is refused. `gos env` prints the tool each role resolves to
and its version, and a missing tool fails `gos build` with the install command
for the host:

```sh
sudo apt-get install -y llvm clang        # Debian, Ubuntu (18 or newer)
brew install llvm@22                      # macOS
pacman -S mingw-w64-x86_64-llvm           # Windows, from MSYS2
```

## Verifying

```sh
gos --version
gos new example.com/hello --path /tmp/hello
cd /tmp/hello
gos run src/main.gos
```

You should see `hello from hello`.

## Supported platforms

Gossamer goroutines are stackful coroutines (corosensei).
Switching contexts requires a per-architecture inline-assembly
implementation, so the supported platform matrix is narrower than
"anything Rust can build":

The precise target contract is maintained in the executable
[supported-target matrix](supported_targets.md). Tier 1 executes the pure
bytecode VM, JIT-enabled VM, and LLVM AOT binaries on native CI for Linux
x86_64/aarch64, Apple Silicon macOS, and Windows x86_64. Linux
x86_64/aarch64-musl AOT output is Tier 2: it is executed natively or under
QEMU and compared with the pure bytecode VM. Intel macOS is artifact-only;
armv7, riscv64, and wasm are not supported execution targets.

`aarch64` Linux - including Raspberry Pi OS 64-bit - is exercised in CI
across all three tiers (the bytecode VM, the in-process Cranelift JIT,
and native `gos build`), not just cross-built. `gos` is fully
self-contained on a Pi; native compilation there uses the device's
system LLVM, as described under [Native builds need LLVM](#native-builds-need-llvm).

## Target toolchains

`gos build --target <triple>` validates the triple against the registered
set, which is broader than the supported matrix. Linux-musl
`{x86_64,aarch64}-unknown-linux-musl` AOT output is the cross-host path with
QEMU-backed release evidence. The gnu targets are native Tier 1 targets on
their matching Linux architecture; cross-host glibc links require an external
sysroot and are not part of the support contract. Cross-compiling *to* macOS
or Windows as a target remains out of scope (needs external SDKs). (A
fully-static single-file binary also comes from `gos build
--release` on a Linux host with the musl rustup target installed - no
`--target` needed.)

Musl targets (`x86_64-unknown-linux-musl`,
`aarch64-unknown-linux-musl`) are gated behind the `musl` Cargo
feature. Rebuild with:

```sh
cargo build --workspace --release -p gossamer-driver --features musl
```

## Editor support

Pre-built plug-ins for VSCode, Vim, Neovim, Helix, Emacs, Sublime,
and Zed (plus a tree-sitter grammar) live at
[`gossamer-lang/gossamer-editor-support`](https://github.com/gossamer-lang/gossamer-editor-support).
Each one drives `gos lsp` for diagnostics, hover, completion,
go-to-definition, references, rename, and inlay hints.

## Next

- [Running](running.md)
- [Syntax](syntax.md)
- [Collection literals](collection_literals.md)
