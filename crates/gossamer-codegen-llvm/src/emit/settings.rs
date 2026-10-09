//! Process-wide build settings the CLI sets before lowering: debug info and source positions, reproducibility, PGO, the optimization profile, and static musl linking.

use std::path::PathBuf;

use super::target_arch_from_triple;

/// Process-wide flag toggled by [`set_debug_info`] so the CLI can
/// request DWARF emission without going through an env var (which
/// would require `unsafe` to set on stable Rust 2024).
static DEBUG_INFO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Process-wide flag toggled by [`set_reproducible`] requesting
/// bit-identical builds across runs. Sets `SOURCE_DATE_EPOCH`
/// (read by `llc`), strips embedded paths from the IR module
/// header, and forces a sorted symbol table on the output.
static REPRODUCIBLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Process-wide flag retained for embedding callers. Native lowering bugs are
/// always hard errors; this switch no longer enables a per-function fallback.
static STRICT_LOWERING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Process-wide flag toggled by [`set_race_instrumentation`]. When on,
/// the LLVM emitter wraps every `gos_load` / `gos_store` raw-heap
/// intrinsic with a `gos_rt_race_access(addr, write)` call so the
/// runtime detector can observe the access. Off by default; the CLI
/// flips it for `gos test --race` / `gos build --race`.
static RACE_INSTRUMENTATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Process-wide optimisation-profile flag toggled by
/// [`set_opt_profile`]. `0` = release (full `opt -O3 | llc -O3`
/// pipeline); `1` = debug (minimal canonicalising `opt` passes followed by
/// `llc -O0`).
/// Default is release so callers that don't configure the profile
/// see the historical behaviour. `gos build` flips this to debug
/// when the user omits `--release`.
static OPT_PROFILE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Linux musl used to need a global LLVM switch that disabled loop idiom
/// recognition for every release target. Keep that measured workaround only
/// for the static-musl link shape that motivated it. The CLI sets this before
/// lowering, so a host GNU triple that will link statically is represented
/// correctly too.
static STATIC_MUSL_LINK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// LLVM profile-guided optimisation mode selected by the CLI. Environment
/// variables remain a compatibility fallback for embedding callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgoMode {
    /// Emit instrumentation that writes raw profile data to this path.
    Collect(PathBuf),
    /// Optimise with this merged LLVM profile data file.
    Profile(PathBuf),
}

static PGO_MODE: std::sync::LazyLock<std::sync::RwLock<Option<PgoMode>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(None));

/// Selects PGO mode for subsequent LLVM emissions in this process.
pub fn set_pgo_mode(mode: Option<PgoMode>) {
    *PGO_MODE.write().expect("PGO mode lock poisoned") = mode;
}

/// Returns the PGO mode selected through [`set_pgo_mode`], if any.
#[must_use]
pub fn pgo_mode() -> Option<PgoMode> {
    PGO_MODE.read().expect("PGO mode lock poisoned").clone()
}

/// Enables (or disables) DWARF emission for subsequent
/// [`compile_to_object`](super::compile_to_object) /
/// [`compile_with_fallback`](super::compile_with_fallback) calls.
/// Called by the `gos build --release -g` flag.
pub fn set_debug_info(enabled: bool) {
    DEBUG_INFO.store(enabled, std::sync::atomic::Ordering::Release);
}

/// `true` when the build should maintain the runtime call-stack a panic
/// report walks. Debug profile only - the bookkeeping is a call per function
/// entry, per return, and per source line.
pub(crate) fn want_stack_frames() -> bool {
    matches!(opt_profile(), OptProfile::Debug)
}

/// Enables (or disables) reproducible-build mode. Used by
/// `gos build --reproducible`.
pub fn set_reproducible(enabled: bool) {
    REPRODUCIBLE.store(enabled, std::sync::atomic::Ordering::Release);
}

/// `true` when reproducible-build mode is on.
pub(super) fn want_reproducible() -> bool {
    REPRODUCIBLE.load(std::sync::atomic::Ordering::Acquire)
}

/// Reports whether reproducible native output was requested. The CLI uses
/// this process-level setting in its final-artifact cache identity.
#[must_use]
pub fn reproducible_enabled() -> bool {
    want_reproducible()
}

/// Enables or disables the legacy strict-lowering flag for embedding callers.
/// Native lowering bugs remain hard errors regardless of this value.
pub fn set_strict_lowering(enabled: bool) {
    STRICT_LOWERING.store(enabled, std::sync::atomic::Ordering::Release);
}

/// Enables (or disables) race-detector instrumentation for
/// subsequent emits. When on, the LLVM lowerer wraps every
/// `gos_load` / `gos_store` raw-heap intrinsic with a
/// `gos_rt_race_access(addr, write)` call so the runtime
/// detector observes the access. `gos test --race` /
/// `gos build --race` flip this on.
pub fn set_race_instrumentation(enabled: bool) {
    RACE_INSTRUMENTATION.store(enabled, std::sync::atomic::Ordering::Release);
}

/// `true` when race-detector instrumentation is requested.
#[must_use]
pub fn want_race_instrumentation() -> bool {
    RACE_INSTRUMENTATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Optimisation profile selector for [`set_opt_profile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptProfile {
    /// Release: full `opt -O3 | llc -O3` pipeline. Default.
    Release,
    /// Debug: the smallest mid-end that makes the emitter's output usable,
    /// followed by the `O1` back end. Checked arithmetic remains enabled.
    Debug,
}

/// Sets the optimisation profile for subsequent emits. `gos build`
/// flips this to `Debug` when the user omits `--release`.
pub fn set_opt_profile(profile: OptProfile) {
    let v: u8 = match profile {
        OptProfile::Release => 0,
        OptProfile::Debug => 1,
    };
    OPT_PROFILE.store(v, std::sync::atomic::Ordering::Release);
}

/// Records whether the final artifact uses the static-musl linker path.
///
/// This affects a narrowly scoped LLVM workaround for tiny-copy loops on
/// targets whose static-musl link keeps musl's memory routines. It is
/// part of the object-cache fingerprint, so a dynamic object can never be
/// reused for a static-musl link or vice versa.
pub fn set_static_musl_link(enabled: bool) {
    STATIC_MUSL_LINK.store(enabled, std::sync::atomic::Ordering::Release);
}

pub(super) fn static_musl_link_enabled() -> bool {
    STATIC_MUSL_LINK.load(std::sync::atomic::Ordering::Acquire)
}

// A static-musl program on `x86_64` links the runtime's own `memcpy`,
// `memmove`, and `memset`, sized for short moves, so a loop LLVM turns into
// one of those calls costs no more than the loop did. Elsewhere those calls
// reach musl's routines, whose startup outweighs a short copy.
pub(super) fn disable_loop_idiom_for_target_with_static_musl(
    static_musl: bool,
    triple: &str,
) -> bool {
    (static_musl || triple.contains("-unknown-linux-musl"))
        && target_arch_from_triple(triple) != "x86_64"
}

/// Whether a release build for `triple`, linked statically against musl when
/// `static_musl` holds, compiles with LLVM's loop idiom recognition turned
/// off.
#[must_use]
pub fn loop_idiom_disabled(static_musl: bool, triple: &str) -> bool {
    disable_loop_idiom_for_target_with_static_musl(static_musl, triple)
}

pub(super) fn disable_loop_idiom_for_target(triple: &str) -> bool {
    disable_loop_idiom_for_target_with_static_musl(static_musl_link_enabled(), triple)
}

/// Reads the active optimisation profile.
pub(crate) fn opt_profile() -> OptProfile {
    match OPT_PROFILE.load(std::sync::atomic::Ordering::Acquire) {
        1 => OptProfile::Debug,
        _ => OptProfile::Release,
    }
}

/// `true` when the build should embed DWARF debug information.
/// Triggered by either the `GOS_DWARF` env var (used by tests),
/// the `GOS_BUILD_DEBUG` env var (CI), or [`set_debug_info`] (CLI
/// `-g` flag).
pub(crate) fn want_dwarf() -> bool {
    DEBUG_INFO.load(std::sync::atomic::Ordering::Acquire)
        || std::env::var("GOS_DWARF").is_ok()
        || std::env::var("GOS_BUILD_DEBUG").is_ok()
}

/// Comment line the lowerer writes ahead of each MIR statement's instructions
/// when the build carries DWARF: `; gos.loc <line> <column>`.
/// [`emit_dwarf_metadata`](super::debug_info::emit_dwarf_metadata) turns it into the `!dbg` location of every
/// instruction that follows, up to the next one.
pub(crate) const DEBUG_LOCATION_MARKER: &str = "; gos.loc ";

/// Comment line the lowerer writes in a function's entry block for each
/// source-level local when the build carries DWARF:
/// `; gos.var <slot> <arg> <line> <column> <type> <name>`, with `arg` the
/// parameter's 1-based position or `0` for a local. [`emit_dwarf_metadata`](super::debug_info::emit_dwarf_metadata)
/// turns it into the variable's `DILocalVariable` and the `llvm.dbg.declare`
/// that places it in its slot.
pub(crate) const DEBUG_VARIABLE_MARKER: &str = "; gos.var ";
