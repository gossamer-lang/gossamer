//! The codegen flags of the runtime archive compiled programs link, shared by
//! the CLI build script that builds the toolchain's archive and the driver's
//! frame-pointer build of it.

/// The flags of the runtime archive a compiled program links, for a target of
/// architecture `arch`, `musl` when the target is a static-musl one, and
/// optimised when `release`.
#[must_use]
pub fn program_archive_rustflags(release: bool, arch: &str, musl: bool) -> Vec<&'static str> {
    // The archive links into programs, which register the subsystems their
    // types need (the cycle collector) instead of linking every one.
    let mut flags = vec!["--cfg gos_program_archive"];
    // A hot loop's speed on x86-64 follows how it falls across 64-byte fetch
    // lines, and at the default 16-byte function alignment that depends on how
    // much code the program links ahead of the runtime. Starting every runtime
    // function on a line keeps its loops' layout the same in every program, as
    // the compiled program's own functions are.
    if release && arch == "x86_64" {
        flags.push("-C llvm-args=-align-all-functions=6");
    }
    // A musl archive only links into static executables, where code built for
    // a fixed address reaches its thread-locals at a constant offset from the
    // thread pointer instead of through a computed base.
    if musl {
        flags.push("-C relocation-model=static");
    }
    flags
}

/// What a runtime built for profiling adds to [`program_archive_rustflags`].
/// The CPU and heap samplers reach the Gossamer function a sample belongs to
/// by following the frame-pointer chain through the runtime's frames, and the
/// cfg lets the profiles say their stacks are whole.
pub const FRAME_POINTER_RUSTFLAGS: &[&str] =
    &["-C force-frame-pointers=yes", "--cfg gos_frame_pointers"];
