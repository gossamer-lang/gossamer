//! Module-level assembly: runtime symbol declarations +
//! per-function lowering + `llc -O3` invocation.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use gossamer_mir::Body;
use gossamer_types::TyCtxt;

use crate::lower::{Lowerer, StringPool};

mod debug_info;
use debug_info::emit_dwarf_metadata;
mod settings;
pub(crate) use settings::{
    DEBUG_LOCATION_MARKER, DEBUG_VARIABLE_MARKER, opt_profile, source_position, want_dwarf,
    want_stack_frames,
};
pub use settings::{
    OptProfile, PgoMode, SourcePositions, loop_idiom_disabled, pgo_mode, reproducible_enabled,
    set_debug_info, set_opt_profile, set_pgo_mode, set_race_instrumentation, set_reproducible,
    set_source_positions, set_static_musl_link, set_strict_lowering, want_race_instrumentation,
};
use settings::{disable_loop_idiom_for_target, static_musl_link_enabled, want_reproducible};
mod symbol_audit;
use symbol_audit::{audit_llvm_ir_symbols, first_llvm_symbol};
mod thunks;
pub(crate) use thunks::{
    CABI_SPAWN_RET_WORDS_ARG, CABI_SPAWN_SHIM, SPAWN_RET_TWO_WORDS, SPAWN_WIDE_CABI_THUNK,
    coff_comdat_decl, operand_const_int,
};
use thunks::{
    body_has_wide_spawn, cabi_thunk_param_tys, collect_cabi_handlers, collect_cabi_thunk_sites,
    collect_sret_bodies, collect_thunk_names_in_body, extern_declare_with,
    render_cabi_handler_thunk, render_cabi_thunk_with_linkage, render_shape_thunk,
    render_shape_thunk_with_linkage, render_spawn_wide_cabi_thunk, shape_thunk_param_tys,
    validate_global_decl_shape,
};
mod toolchain;
pub use toolchain::{
    LlvmToolStatus, MINIMUM_LLVM_MAJOR, PREFERRED_LLVM_MAJOR, llvm_toolchain_status,
};
use toolchain::{
    find_llc, find_opt, host_triple, integrated_clang_path, invoke_llc_pipeline, llvm_pass_options,
    llvm_target_triple_for, mcpu_target, pipeline_tmp_dir, target_arch_from_triple,
};
pub(crate) use toolchain::{target_c_abi, target_has_preserve_most, target_is_windows};

/// LLVM IR strings that must appear in the module header but are
/// not emitted through `declare_rt()`: LLVM built-in intrinsics,
/// libc `malloc`, the stdout globals, and the three runtime symbols
/// called directly by the C `@main` shim (which is hardcoded in
/// `render_module_inner` rather than lowered from a MIR body).
const LLVM_SPECIAL_DECLS: &[&str] = &[
    "declare ptr @malloc(i64)",
    "declare void @llvm.memcpy.p0.p0.i64(ptr, ptr, i64, i1)",
    "declare void @llvm.lifetime.start.p0(i64, ptr)",
    "declare void @llvm.lifetime.end.p0(i64, ptr)",
    "@GOS_RT_STDOUT_BYTES = external local_unnamed_addr global [8192 x i8]",
    "@GOS_RT_STDOUT_LEN = external local_unnamed_addr global i64",
    // Called directly by the @main shim - not reachable via declare_rt().
    "declare void @gos_rt_set_args(i32, ptr)",
    "declare void @gos_rt_program_start()",
    "declare void @gos_rt_flush_stdout()",
    "declare i32 @gos_rt_main_exit_code(i64)",
    "declare i32 @gos_rt_main_exit_code_err(i64, i64)",
];

/// Parallel to `gossamer-codegen-cranelift::NativeObject`.
#[derive(Debug, Clone)]
pub struct NativeObject {
    /// Requested target triple (host by default).
    pub triple: String,
    /// Linker-ready object bytes (ELF / Mach-O depending on host).
    pub bytes: Vec<u8>,
}

/// Reasons the LLVM backend refuses a build after frontend validation.
#[derive(Debug)]
pub enum BuildError {
    /// Valid MIR reached an impossible or unimplemented LLVM lowering shape.
    InternalLoweringBug(&'static str),
    /// `llc` not reachable or returned non-zero.
    Tool(String),
    /// IR rendering or temp-file I/O failed.
    Io(anyhow::Error),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InternalLoweringBug(what) => {
                write!(f, "llvm backend internal lowering bug: {what}")
            }
            Self::Tool(msg) => write!(f, "llvm backend: tool: {msg}"),
            Self::Io(err) => write!(f, "llvm backend: {err}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// Tarjan state for condensing the body call graph before codegen partitioning.
struct Tarjan<'a> {
    edges: &'a [Vec<usize>],
    next_index: usize,
    indices: Vec<Option<usize>>,
    low: Vec<usize>,
    stack: Vec<usize>,
    on_stack: Vec<bool>,
    components: Vec<Vec<usize>>,
}

impl Tarjan<'_> {
    fn visit(&mut self, node: usize) {
        let index = self.next_index;
        self.next_index += 1;
        self.indices[node] = Some(index);
        self.low[node] = index;
        self.stack.push(node);
        self.on_stack[node] = true;
        for &next in &self.edges[node] {
            if self.indices[next].is_none() {
                self.visit(next);
                self.low[node] = self.low[node].min(self.low[next]);
            } else if self.on_stack[next] {
                self.low[node] = self.low[node].min(self.indices[next].unwrap_or(index));
            }
        }
        if self.low[node] == index {
            let mut component = Vec::new();
            loop {
                let member = self.stack.pop().expect("Tarjan stack is non-empty");
                self.on_stack[member] = false;
                component.push(member);
                if member == node {
                    break;
                }
            }
            component.sort_unstable();
            self.components.push(component);
        }
    }
}

/// Module-level TBAA metadata tree emitted once per LLVM module (both render
/// paths), right after the empty `!0` node.
///
/// Two sibling scalar type nodes - an aggregate *header* node (`!2`) and a
/// *payload* node (`!3`) - split every tagged access into two never-aliasing
/// classes via the access tags `!4` (header) and `!5` (payload).
///
/// The header class covers a `GosVec` / `GosI64Vec` / `GosU8Vec`
/// len/cap/elem_bytes/data-pointer and a string's rc/cap/len/tag prefix. The
/// payload class covers element-buffer and string-content bytes plus the flat
/// i64 slot slabs that hold struct fields, tuple elements, and fixed-array
/// elements.
///
/// This is sound because the two never alias: a header and its element buffer
/// are separate allocations or disjoint byte ranges of one allocation, slot
/// slabs are separate allocations again, and no single access spans a header
/// and a payload byte. With the distinction in place `-O3` can prove a payload
/// store does not clobber a hoisted `len`/`cap`/`elem_bytes`/`data` load, so
/// LICM hoists the data pointer and the loop vectorizer fires on element loops.
///
/// The IDs (1-5) never collide with the DWARF metadata (`!40`+, and `!100`+
/// per subprogram) that [`emit_dwarf_metadata`] appends on the `-g` path.
const TBAA_METADATA: &str = r#"!1 = !{!"gos_tbaa_root"}
!2 = !{!"gos_agg_header", !1, i64 0}
!3 = !{!"gos_agg_data", !1, i64 0}
!4 = !{!2, !2, i64 0}
!5 = !{!3, !3, i64 0}
"#;

/// Outcome of an LLVM object build.
///
/// `fallback_bodies` is retained for API compatibility with older
/// drivers. LLVM lowering bugs are now hard errors, so successful
/// builds leave it empty.
#[derive(Debug, Clone)]
pub struct CompileOutcome {
    /// Object file with the LLVM-lowered bodies.
    pub object: NativeObject,
    /// Always empty for successful LLVM builds.
    pub fallback_bodies: Vec<String>,
}

/// Lowers a list of MIR bodies into a native object file via
/// `llc -O3`. The signature mirrors
/// `gossamer-codegen-cranelift::compile_to_object` exactly so
/// the driver can dispatch between the two on the `--release`
/// flag.
pub fn compile_to_object(bodies: &[Body], tcx: &TyCtxt) -> Result<NativeObject> {
    if std::env::var("GOS_LLVM_DUMP_MIR").is_ok() {
        dump_mir(bodies, tcx);
    }
    let triple = host_triple();
    let llvm_triple = llvm_target_triple_for(&triple);
    let tmp_dir = pipeline_tmp_dir()?;
    let ll_path = tmp_dir.join("unit.ll");
    let _ = render_module_to_path(bodies, tcx, &ll_path, /*allow_fallback=*/ false)?;
    let obj_path = tmp_dir.join("unit.o");
    invoke_llc_pipeline(&ll_path, &obj_path, &llvm_triple, /*announce=*/ true)?;
    let bytes =
        std::fs::read(&obj_path).with_context(|| format!("reading {}", obj_path.display()))?;
    if std::env::var("GOS_LLVM_DUMP").is_err() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
    Ok(NativeObject { triple, bytes })
}

/// Path-oriented variant of [`compile_to_object`]: writes the LLVM
/// object directly to `obj_out` instead of returning bytes. Used
/// by the AOT release driver so the LLVM object never lives in
/// the parent process's heap, only on disk.
pub fn compile_to_object_at_path(
    bodies: &[Body],
    tcx: &TyCtxt,
    obj_out: &std::path::Path,
) -> Result<String> {
    if std::env::var("GOS_LLVM_DUMP_MIR").is_ok() {
        dump_mir(bodies, tcx);
    }
    let triple = host_triple();
    let llvm_triple = llvm_target_triple_for(&triple);
    let tmp_dir = pipeline_tmp_dir()?;
    let ll_path = tmp_dir.join("unit.ll");
    let _ = render_module_to_path(bodies, tcx, &ll_path, /*allow_fallback=*/ false)?;
    invoke_llc_pipeline(&ll_path, obj_out, &llvm_triple, /*announce=*/ true)?;
    if std::env::var("GOS_LLVM_DUMP").is_err() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
    Ok(triple)
}

/// Lowers `bodies` through the standard LLVM pipeline and returns the
/// resulting `.ll` IR as a UTF-8 string instead of writing an object.
/// Used by snapshot / smoke tests that need to inspect the IR shape
/// without driving `opt`+`llc` over it. `allow_fallback` is retained
/// for API compatibility; LLVM lowering bugs are always hard errors.
pub fn render_ir_to_string(bodies: &[Body], tcx: &TyCtxt, allow_fallback: bool) -> Result<String> {
    let tmp_dir = pipeline_tmp_dir()?;
    let ll_path = tmp_dir.join("unit.ll");
    let _ = render_module_to_path(bodies, tcx, &ll_path, allow_fallback)?;
    let ir = std::fs::read_to_string(&ll_path)
        .with_context(|| format!("reading {}", ll_path.display()))?;
    if std::env::var("GOS_LLVM_DUMP").is_err() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
    Ok(ir)
}

// ---------------------------------------------------------------------------
// P2 + P3: parallel per-body compilation with incremental object cache
// ---------------------------------------------------------------------------

/// Maximum number of concurrent `opt`+`llc` worker threads.
const PARALLEL_MAX_THREADS: usize = 8;

/// A program with fewer bodies than this compiles as one module: below it the
/// cost of starting a second LLVM child outweighs anything a split saves.
const MIN_BODIES_PER_CHUNK: usize = 10;

/// Ceiling on concurrent LLVM children. Each one commonly touches 45 to 65 MiB,
/// so the fan-out buys wall time with resident memory. Eight is where the wall
/// time stops falling on the build benchmarks, and a 42,000-line project's
/// process tree stays around 140 MiB there.
const JOB_CEILING: usize = 8;

/// Concurrent LLVM children: one per core up to [`JOB_CEILING`], in both
/// profiles. A release chunk boundary is not an inlining boundary, because
/// every chunk carries `available_externally` copies of the callees it reaches
/// in other chunks (see [`chunk_imports`]). `GOS_LLVM_JOBS` overrides the
/// default, for a host where the memory or the throughput matters more than
/// the default trade.
fn codegen_job_limit() -> usize {
    if let Ok(value) = std::env::var("GOS_LLVM_JOBS")
        && let Ok(jobs) = value.parse::<usize>()
        && jobs > 0
    {
        return jobs.min(PARALLEL_MAX_THREADS);
    }
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(JOB_CEILING)
}

/// FNV-1a 64-bit hash - deterministic, no `std` hasher randomisation,
/// so cache keys are stable across process restarts.
fn fnv1a_64(data: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    fnv1a_64_update(OFFSET, data)
}

/// Folds `data` into a running FNV-1a state, for a hash built from many
/// pieces without joining them into one buffer first.
fn fnv1a_64_update(mut hash: u64, data: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    for &b in data {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Fingerprint of implementation inputs that can change emitted LLVM IR.
/// This intentionally avoids the `gos` executable mtime: local reinstalls
/// rebuild that wrapper often, and using its timestamp made unchanged
/// programs miss the object cache and rerun release LLVM codegen.
fn compiler_fingerprint() -> u64 {
    static FP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FP.get_or_init(|| {
        // The registry has well over a thousand entries and this runs on the
        // first body of every build, so each field is folded in as its own
        // bytes rather than through a formatter and a string per entry.
        let mut hash = fnv1a_64(b"gossamer-llvm-");
        hash = fnv1a_64_update(hash, env!("CARGO_PKG_VERSION").as_bytes());
        hash = fnv1a_64_update(hash, b"|codegen=");
        hash = fnv1a_64_update(hash, env!("GOSSAMER_LLVM_CODEGEN_CACHE_STAMP").as_bytes());
        for entry in gossamer_abi::REGISTRY {
            hash = fnv1a_64_update(hash, b"|");
            hash = fnv1a_64_update(hash, entry.name.as_bytes());
            hash = fnv1a_64_update(
                hash,
                &[
                    entry.sig.ret as u8,
                    entry.tier as u8,
                    u8::from(entry.noreturn),
                    u8::from(entry.unwinds),
                ],
            );
            for param in entry.sig.params {
                hash = fnv1a_64_update(hash, &[*param as u8]);
            }
        }
        hash
    })
}

/// `fmt::Write` adapter that feeds structured formatter output directly into
/// SHA-256. MIR does not yet have a stable serde schema, so its complete Debug
/// representation remains the cache identity for compatibility, but it never
/// materialises as a second full-size `String`.
struct DigestWriter(sha2::Sha256);

impl std::fmt::Write for DigestWriter {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        use sha2::Digest as _;
        self.0.update(text.as_bytes());
        Ok(())
    }
}

impl DigestWriter {
    fn new(domain: &[u8]) -> Self {
        use sha2::Digest as _;
        let mut digest = sha2::Sha256::new();
        digest.update(domain);
        Self(digest)
    }

    fn update(&mut self, bytes: &[u8]) {
        use sha2::Digest as _;
        self.0.update(bytes);
    }

    fn finish(self) -> String {
        use sha2::Digest as _;
        format!("{:x}", self.0.finalize())
    }
}

/// Wall time each codegen phase spent, in microseconds, accumulated for the
/// life of the process.
///
/// A build reports these through `--timings`, where they say whether a slow
/// codegen is the emitter writing IR, the identity hash that decides a cache
/// hit, or the LLVM child compiling the result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodegenPhaseTimes {
    /// Hashing MIR into the per-body object-cache identity.
    pub cache_key_us: u64,
    /// Lowering MIR to LLVM IR text and writing it out.
    pub render_us: u64,
    /// The LLVM child processes compiling that IR to objects.
    pub tool_us: u64,
}

static CACHE_KEY_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RENDER_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TOOL_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn record_phase(slot: &std::sync::atomic::AtomicU64, started: std::time::Instant) {
    slot.fetch_add(
        u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// Records LLVM child-process time when the returned value is dropped, so
/// every exit path out of the pipeline is measured.
fn scopeguard_tool_time(started: std::time::Instant) -> ToolTimeGuard {
    ToolTimeGuard(started)
}

struct ToolTimeGuard(std::time::Instant);

impl Drop for ToolTimeGuard {
    fn drop(&mut self) {
        record_phase(&TOOL_US, self.0);
    }
}

/// The codegen phase times recorded so far.
#[must_use]
pub fn codegen_phase_times() -> CodegenPhaseTimes {
    use std::sync::atomic::Ordering::Relaxed;
    CodegenPhaseTimes {
        cache_key_us: CACHE_KEY_US.load(Relaxed),
        render_us: RENDER_US.load(Relaxed),
        tool_us: TOOL_US.load(Relaxed),
    }
}

/// Object-cache key for one rendered chunk module.
///
/// The key is the module text itself plus every setting outside it that
/// changes what `opt` and `llc` make of it. The text is exactly what LLVM
/// compiles, so two builds share an object only when LLVM would be handed the
/// same input: source positions, resolver ids and interning order that do not
/// reach the IR cannot split the cache, and nothing that does reach it -
/// including an imported callee's body - can be missed.
fn chunk_cache_key(ir: &str, triple: &str, profile: OptProfile) -> String {
    let started = std::time::Instant::now();
    let mut digest = DigestWriter::new(b"gossamer-llvm-chunk-ir-cache-v1\0");
    digest.update(triple.as_bytes());
    digest.update(b"\0");
    digest.update(if matches!(profile, OptProfile::Debug) {
        b"debug"
    } else {
        b"release"
    });
    digest.update(b"\0");
    digest.update(&compiler_fingerprint().to_le_bytes());
    digest.update(b"\0");
    digest.update(codegen_configuration_fingerprint(triple, profile).as_bytes());
    digest.update(b"\0");
    digest.update(ir.as_bytes());
    let key = digest.finish();
    record_phase(&CACHE_KEY_US, started);
    key
}

/// Every setting outside MIR that can change emitted machine code. Keeping
/// this in the object-cache identity prevents a debug, PGO, cross-target, or
/// different-LLVM build from reusing an incompatible object produced for the
/// same body.
fn codegen_configuration_fingerprint(triple: &str, profile: OptProfile) -> String {
    // Every input here is a process-level setting or a tool on disk, and a
    // build reads the fingerprint once per body, so the answer is kept for
    // the settings it was computed under rather than restatted each time.
    static CACHE: std::sync::OnceLock<parking_lot::Mutex<HashMap<String, String>>> =
        std::sync::OnceLock::new();
    let key = format!("{triple}|{profile:?}|{:?}", pgo_mode());
    if let Some(hit) = CACHE
        .get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
        .lock()
        .get(&key)
    {
        return hit.clone();
    }
    let text = codegen_configuration_fingerprint_uncached(triple, profile);
    CACHE
        .get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
        .lock()
        .insert(key, text.clone());
    text
}

fn codegen_configuration_fingerprint_uncached(triple: &str, profile: OptProfile) -> String {
    let mut text = format!(
        "triple={triple}|profile={profile:?}|mcpu={}|dwarf={}|repro={}|race={}|static_musl={}",
        mcpu_target(triple),
        want_dwarf(),
        want_reproducible(),
        want_race_instrumentation(),
        static_musl_link_enabled(),
    );
    text.push_str("|options=");
    text.push_str(&llvm_pass_options(triple, profile).join(","));
    let selected_pgo = pgo_mode();
    match selected_pgo {
        Some(PgoMode::Collect(path)) => {
            text.push_str("|pgo-collect=");
            text.push_str(&path.to_string_lossy());
        }
        Some(PgoMode::Profile(path)) => {
            text.push_str("|pgo-profile=");
            text.push_str(&file_identity(&path));
        }
        None => {
            if let Ok(path) = std::env::var("GOS_PGO_COLLECT") {
                text.push_str("|pgo-collect-env=");
                text.push_str(&path);
            }
            if let Ok(path) = std::env::var("GOS_PGO_PROFILE") {
                text.push_str("|pgo-profile-env=");
                text.push_str(&file_identity(std::path::Path::new(&path)));
            }
        }
    }
    if let Some(clang) = integrated_clang_path(triple) {
        text.push_str("|pipeline=clang|");
        text.push_str(&file_identity(&clang));
    } else {
        text.push_str("|pipeline=opt-llc|");
        if let Ok(opt) = find_opt() {
            text.push_str(&file_identity(&opt));
        }
        text.push('|');
        if let Ok(llc) = find_llc() {
            text.push_str(&file_identity(&llc));
        }
    }
    text
}

fn file_identity(path: &std::path::Path) -> String {
    let mut text = path.to_string_lossy().into_owned();
    if let Ok(meta) = std::fs::metadata(path) {
        text.push_str(&format!("@{}", meta.len()));
        if let Ok(modified) = meta.modified()
            && let Ok(age) = modified.duration_since(std::time::UNIX_EPOCH)
        {
            text.push_str(&format!("+{}", age.as_nanos()));
        }
    }
    text
}

/// Process-level override for the incremental cache directory.
/// Set by [`set_cache_dir`]; takes precedence over `GOS_BUILD_CACHE`
/// and the platform default.
static CACHE_DIR_OVERRIDE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Configures the incremental object cache directory for subsequent
/// builds. Calling this before the first `compile_with_fallback_at_path`
/// lets the CLI anchor the cache next to the project (or in a CI-
/// controlled location) without relying on the `GOS_BUILD_CACHE` env
/// var. Has no effect if called after the first cache lookup.
pub fn set_cache_dir(dir: PathBuf) {
    let _ = CACHE_DIR_OVERRIDE.set(Some(dir));
}

/// Process-level target-triple override for cross-compilation.
/// Set by the CLI from `--target`; consulted by [`host_triple`]
/// ahead of the `TARGET` env var and host detection.
///
/// This joins the same set-once compiler-configuration idiom as
/// [`set_cache_dir`], [`set_opt_profile`], and [`set_strict_lowering`]:
/// codegen is configured before the first lowering call rather than
/// threaded through every signature. The target is read by `host_triple`
/// alone, and `host_triple` is consulted at many internal sites - the
/// `-mtriple` passed to opt/llc, the Win64-vs-SysV i128 ABI marshalling
/// in `target_is_windows` (deep in per-operation lowering), the parallel
/// codegen workers, and the incremental object-cache key. Threading a
/// target parameter through all of those would be pervasive; this one
/// override makes them target-aware at the single chokepoint.
static TARGET_TRIPLE_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

static LIBRARY_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Builds a library named `name` rather than a program: the module that
/// defines the `#[export]` entries also defines `<name>_shutdown`, which
/// runs the library's `runtime::at_exit` hooks when its host calls it.
pub fn set_library_name(name: String) {
    let _ = LIBRARY_NAME.set(name);
}

/// `define <name>_shutdown`, for the chunk holding the export entries of a
/// library build.
fn library_shutdown_entry(bodies: &[&Body]) -> Option<String> {
    let name = LIBRARY_NAME.get()?;
    bodies
        .iter()
        .any(|body| body.name == gossamer_mir::FFI_EXPORTS_FN)
        .then(|| {
            let linkage = if target_is_windows() {
                "dllexport"
            } else {
                "dso_local"
            };
            format!(
                "declare void @gos_rt_library_shutdown()\n\
                 define {linkage} void @\"{name}_shutdown\"() {{\n  \
                 call void @gos_rt_library_shutdown()\n  ret void\n}}\n"
            )
        })
}

/// Configures the LLVM target triple for subsequent builds. No effect
/// once a build has begun reading the triple.
pub fn set_target_triple(triple: String) {
    let _ = TARGET_TRIPLE_OVERRIDE.set(triple);
}

/// The triple this process compiles for: the `--target` override when one
/// was set, otherwise the detected host. Callers fold it into cache
/// identities so an artifact never crosses a target boundary.
#[must_use]
pub fn active_target_triple() -> String {
    host_triple()
}

/// Resolves the active incremental cache directory in priority order:
/// 1. [`set_cache_dir`] override (process-level)
/// 2. `GOS_BUILD_CACHE` env var
/// 3. `XDG_CACHE_HOME/gossamer/ir-cache` (Linux/macOS XDG)
/// 4. `$HOME/.cache/gossamer/ir-cache`
/// 5. `%LOCALAPPDATA%\gossamer\ir-cache` (Windows)
///
/// Returns `None` when `GOS_NO_CACHE=1` is set or no home dir can be
/// found.
fn active_cache_dir() -> Option<PathBuf> {
    if let Some(Some(dir)) = CACHE_DIR_OVERRIDE.get() {
        return Some(dir.clone());
    }
    if std::env::var("GOS_NO_CACHE").is_ok() {
        return None;
    }
    if let Ok(d) = std::env::var("GOS_BUILD_CACHE") {
        return Some(PathBuf::from(d));
    }
    toolchain_cache_dir()
}

/// The user-wide cache directory, for notes about the machine's toolchain
/// rather than about a program being built.
///
/// A project may point its object cache elsewhere; what LLVM version a tool
/// on this machine is does not belong to any one project, and re-deriving it
/// per project is what a cache is meant to avoid.
fn toolchain_cache_dir() -> Option<PathBuf> {
    if std::env::var("GOS_NO_CACHE").is_ok() {
        return None;
    }
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("gossamer").join("ir-cache"));
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg).join("gossamer").join("ir-cache"));
    }
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(".cache")
            .join("gossamer")
            .join("ir-cache")
    })
}

/// Variant of [`render_shape_thunk`] that uses `linkonce_odr` linkage
/// instead of the default `define`. Required in per-body modules where
/// the same thunk shape may be emitted by multiple compilation units -
/// `linkonce_odr` lets the linker keep one copy and discard the rest
/// without a duplicate-symbol error.
fn render_shape_thunk_linkonce(name: &str) -> Option<String> {
    render_shape_thunk_with_linkage(name, "linkonce_odr ")
}

/// Shared, program-wide context threaded into each per-body renderer.
struct ModuleCtx<'a> {
    all_bodies: &'a [Body],
    tcx: &'a TyCtxt,
    fn_name_by_def: &'a std::collections::HashMap<u32, String>,
    param_tys_by_name: &'a std::collections::HashMap<String, Vec<gossamer_types::Ty>>,
    capture_summary: &'a gossamer_mir::CaptureSummary,
    triple: &'a str,
}

/// Module data layout for 64-bit native targets: the LLVM defaults with one
/// deviation - `i128` ABI alignment is 8, not 16. The runtime stores
/// every value in flat 8-byte slots, so a by-value `{disc, payload}`
/// Option/Result living at an odd word offset inside a struct is only
/// ever 8-aligned; without an explicit layout, opt assumes the target's
/// 16-byte i128 alignment and expands copies of such fields into
/// over-aligned operations that can fault or let the optimizer assume an
/// alignment Gossamer does not provide.
fn module_datalayout(triple: &str) -> Option<String> {
    let mangling = if triple.contains("apple") || triple.contains("darwin") {
        "m:o"
    } else if triple.contains("windows") {
        "m:w"
    } else {
        "m:e"
    };
    match target_arch_from_triple(triple) {
        "x86_64" => Some(format!(
            "e-{mangling}-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:64-f80:128-n8:16:32:64-S128"
        )),
        "aarch64" => Some(format!("e-{mangling}-i64:64-i128:64-n32:64-S128")),
        _ => None,
    }
}

/// Direct call edges between bodies, by index, deduplicated and sorted.
fn call_edges(bodies: &[Body]) -> Vec<Vec<usize>> {
    let by_name: std::collections::HashMap<&str, usize> = bodies
        .iter()
        .enumerate()
        .map(|(idx, body)| (body.name.as_str(), idx))
        .collect();
    let by_def: std::collections::HashMap<u32, usize> = bodies
        .iter()
        .enumerate()
        .filter_map(|(idx, body)| body.def.map(|def| (def.local, idx)))
        .collect();
    let mut edges = vec![Vec::new(); bodies.len()];
    for (idx, body) in bodies.iter().enumerate() {
        for block in &body.blocks {
            let gossamer_mir::Terminator::Call { callee, .. } = &block.terminator else {
                continue;
            };
            let target = match callee {
                gossamer_mir::Operand::Const(gossamer_mir::ConstValue::Str(name)) => {
                    by_name.get(name.as_str()).copied()
                }
                gossamer_mir::Operand::FnRef { def, .. } => by_def.get(&def.local).copied(),
                _ => None,
            };
            if let Some(target) = target
                && target != idx
                && !edges[idx].contains(&target)
            {
                edges[idx].push(target);
            }
        }
        edges[idx].sort_unstable();
    }
    edges
}

/// The source module a body belongs to: the first segment of its qualified
/// name, or the entry file (the empty string) for an unqualified one. A
/// monomorphised instance, a lifted closure and a synthesised helper carry no
/// module prefix and file under the entry.
fn body_module(name: &str) -> &str {
    name.split_once("::").map_or("", |(module, _)| module)
}

/// Partitions bodies into LLVM modules by the source module that declares
/// them, so an edit confined to one module leaves every other module's
/// rendered IR, and therefore its cached object, untouched.
///
/// A partition that depended on how many bodies the whole program has - a
/// balance across workers, say - would move bodies between modules whenever
/// any function was added or removed anywhere, and every module would miss
/// the cache. Modules joined by a recursive call cycle share one LLVM module,
/// so the cycle's members stay inlinable into each other. A program below
/// twice [`MIN_BODIES_PER_CHUNK`] bodies stays one module.
fn codegen_chunks(bodies: &[Body]) -> Vec<Vec<usize>> {
    if bodies.len() < 2 * MIN_BODIES_PER_CHUNK {
        return vec![(0..bodies.len()).collect()];
    }
    let edges = call_edges(bodies);
    let mut modules: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for body in bodies {
        let next = modules.len();
        modules.entry(body_module(&body.name)).or_insert(next);
    }
    let module_of: Vec<usize> = bodies
        .iter()
        .map(|body| modules[body_module(&body.name)])
        .collect();
    let mut parent: Vec<usize> = (0..modules.len()).collect();
    for component in strongly_connected_components(&edges) {
        let Some((&first, rest)) = component.split_first() else {
            continue;
        };
        for &member in rest {
            let a = union_find_root(&mut parent, module_of[first]);
            let b = union_find_root(&mut parent, module_of[member]);
            if a != b {
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    // A chunk is named, and ordered, by the first module name it holds, which
    // the modules alone decide.
    let mut group_name: std::collections::HashMap<usize, &str> = std::collections::HashMap::new();
    for (name, &id) in &modules {
        let group = union_find_root(&mut parent, id);
        group_name.entry(group).or_insert(name);
    }
    let mut chunks: std::collections::BTreeMap<&str, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (idx, &module) in module_of.iter().enumerate() {
        let group = union_find_root(&mut parent, module);
        chunks.entry(group_name[&group]).or_default().push(idx);
    }
    chunks.into_values().collect()
}

/// Bodies each chunk may inline from other chunks, by chunk.
///
/// A release chunk carries an `available_externally` definition of every
/// callee in another chunk that is small enough for `opt` to inline, so a
/// module boundary costs no inlining: `opt` sees the callee's body exactly as
/// it would inside one whole-program module, and drops the copy once it has
/// served. The chunk's cache key covers the copies, so editing a callee
/// rebuilds every chunk that inlined it. Callees of an imported body are
/// imported in turn, the way ThinLTO imports a chain, with the size budget
/// halving at each step. A debug build runs no inliner and imports nothing.
fn chunk_imports(bodies: &[Body], chunks: &[Vec<usize>], profile: OptProfile) -> Vec<Vec<usize>> {
    if matches!(profile, OptProfile::Debug) || chunks.len() < 2 {
        return vec![Vec::new(); chunks.len()];
    }
    let edges = call_edges(bodies);
    let cabi_handlers = collect_cabi_handlers(bodies);
    let importable: Vec<bool> = bodies
        .iter()
        .map(|body| {
            body.name != "main"
                && !cabi_handlers.contains_key(&body.name)
                && !body_has_wide_spawn(body)
        })
        .collect();
    let cost: Vec<usize> = bodies
        .iter()
        .map(|body| body.blocks.iter().map(|b| b.stmts.len() + 1).sum())
        .collect();
    chunks
        .iter()
        .map(|chunk| {
            let members: std::collections::HashSet<usize> = chunk.iter().copied().collect();
            let mut imported: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
            let mut frontier: Vec<usize> = chunk.clone();
            let mut budget = IMPORT_COST_LIMIT;
            while !frontier.is_empty() && budget > 0 {
                let mut next = Vec::new();
                for &caller in &frontier {
                    for &callee in &edges[caller] {
                        if members.contains(&callee)
                            || !importable[callee]
                            || cost[callee] > budget
                            || !imported.insert(callee)
                        {
                            continue;
                        }
                        next.push(callee);
                    }
                }
                frontier = next;
                budget /= 2;
            }
            imported.into_iter().collect()
        })
        .collect()
}

/// Largest callee, in MIR statements plus terminators, a chunk imports from
/// another chunk. The release MIR inliner has already spliced the smallest
/// callees into their callers, so what `opt` can still usefully inline across
/// a module boundary is the band just above that inliner's own limit. On the
/// LangArena suite a budget of 40 measures the same instruction counts as one
/// whole-program module and as a budget of 160, and every body imported costs
/// compile time in each chunk that carries it and widens what an edit to it
/// rebuilds.
const IMPORT_COST_LIMIT: usize = 40;

/// The representative of `node`'s set in a union-find forest, halving the
/// path on the way up.
fn union_find_root(parent: &mut [usize], mut node: usize) -> usize {
    while parent[node] != node {
        parent[node] = parent[parent[node]];
        node = parent[node];
    }
    node
}

/// Tarjan's strongly connected components over `edges`, each sorted.
fn strongly_connected_components(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut tarjan = Tarjan {
        edges,
        next_index: 0,
        indices: vec![None; edges.len()],
        low: vec![0; edges.len()],
        stack: Vec::new(),
        on_stack: vec![false; edges.len()],
        components: Vec::new(),
    };
    for node in 0..edges.len() {
        if tarjan.indices[node].is_none() {
            tarjan.visit(node);
        }
    }
    tarjan.components
}

/// RC type-meta blobs in symbol order.
///
/// The type context stores them in a hash map, and their emission order
/// fixes their relative layout in the object's constant pool, so the
/// emitter imposes a total order before writing them out.
fn sorted_rc_metas(tcx: &TyCtxt) -> Vec<(&str, &[i64])> {
    let mut metas: Vec<(&str, &[i64])> = tcx.rc_metas().collect();
    metas.sort_unstable_by_key(|(symbol, _)| *symbol);
    metas
}

/// Adds every quoted global name (`@"name"`) in `text` to `out`.
fn collect_quoted_symbols<'t>(text: &'t str, out: &mut std::collections::HashSet<&'t str>) {
    let mut rest = text;
    while let Some(at) = rest.find("@\"") {
        let after = &rest[at + 2..];
        let Some(close) = after.find('"') else {
            break;
        };
        out.insert(&after[..close]);
        rest = &after[close + 1..];
    }
}

/// Renders all bodies in `chunk_indices` as a single LLVM IR module, with an
/// `available_externally` copy of each body in `imports`.
///
/// Only what the rendered bodies name is declared: another body, or an RC
/// type-meta blob, the chunk never references would put the whole program's
/// shape into every chunk's text, and every chunk's cache key with it.
/// Lowering bugs are returned immediately.
fn render_chunk_module(
    chunk_indices: &[usize],
    imports: &[usize],
    ctx: &ModuleCtx<'_>,
) -> Result<String, BuildError> {
    let _exports = crate::lower::ExportScope::enter(ctx.all_bodies);
    let string_pool =
        std::rc::Rc::new(std::cell::RefCell::new(crate::lower::StringPool::default()));

    let mut body_irs: Vec<String> = Vec::new();
    let mut globals_raw: Vec<String> = Vec::new();
    let mut thunk_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut main_idx: Option<usize> = None;

    // Handler ABI bridge: user functions whose address is
    // handed to a runtime server-start shim are called by the rustc
    // runtime through `extern "C" fn(..) -> i128` (xmm0 return), so their
    // `gos_fn_addr` must point at a `<16 x i8>` return thunk. Empty off
    // Windows for return thunks, but the collected set is still used by
    // function setup to bind raw runtime pointer params correctly.
    let cabi_handlers = collect_cabi_handlers(ctx.all_bodies);
    let cabi_thunk_sites = collect_cabi_thunk_sites(ctx.all_bodies);
    let mut cabi_shape_thunks: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let sret_bodies = collect_sret_bodies(ctx.all_bodies, ctx.tcx);

    let defined = chunk_indices.iter().map(|&idx| (idx, false));
    let copied = imports.iter().map(|&idx| (idx, true));
    for (idx, available_externally) in defined.chain(copied) {
        let body = &ctx.all_bodies[idx];
        if body.name == "main" && !available_externally {
            main_idx = Some(idx);
        }

        let mut lowerer = crate::lower::Lowerer::new(body, ctx.tcx);
        lowerer.fn_name_by_def.clone_from(ctx.fn_name_by_def);
        lowerer.param_tys_by_name.clone_from(ctx.param_tys_by_name);
        lowerer.strings = string_pool.clone();
        lowerer.capture_summary = ctx.capture_summary.clone();
        lowerer.cabi_handlers.clone_from(&cabi_handlers);
        if let Some(sites) = cabi_thunk_sites.get(&body.name) {
            lowerer.cabi_thunk_sites = sites.keys().copied().collect();
            cabi_shape_thunks.extend(sites.values().cloned());
        }
        lowerer.sret_bodies.clone_from(&sret_bodies);

        let text = lowerer.lower()?;
        globals_raw.extend(lowerer.take_module_globals());
        collect_thunk_names_in_body(body, &mut thunk_names);
        if available_externally {
            let Some(rest) = text.strip_prefix("define ") else {
                return Err(BuildError::InternalLoweringBug(
                    "a lowered body does not open with its `define` line",
                ));
            };
            body_irs.push(format!("define available_externally {rest}"));
        } else {
            body_irs.push(text);
        }
    }

    // Every `@"name"` the module's own text mentions. Bodies and metas
    // outside this set are left out of the module.
    let mut referenced: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for text in body_irs.iter().chain(globals_raw.iter()) {
        collect_quoted_symbols(text, &mut referenced);
    }
    let local_symbols: std::collections::HashSet<String> = chunk_indices
        .iter()
        .chain(imports)
        .map(|&idx| crate::lower::mangle_fn_name(&ctx.all_bodies[idx].name).into_owned())
        .collect();

    let mut out = String::new();
    writeln!(out, "; ModuleID = \"gossamer\"").unwrap();
    if let Some(dl) = module_datalayout(ctx.triple) {
        writeln!(out, "target datalayout = \"{dl}\"").unwrap();
    }
    writeln!(out, "target triple = \"{}\"", ctx.triple).unwrap();
    writeln!(out).unwrap();
    // Every generated body carries `#0`. A profiler samples an arbitrary
    // instruction and has to walk out of it, and DWARF unwinding is not
    // async-signal-safe, so the frame chain has to be there. This must be
    // an IR attribute: `clang -x ir` ignores `-fno-omit-frame-pointer`,
    // which only sets it when clang generates the IR itself.
    writeln!(out, "attributes #0 = {{ \"frame-pointer\"=\"all\" }}").unwrap();
    writeln!(out).unwrap();
    for d in LLVM_SPECIAL_DECLS {
        writeln!(out, "{d}").unwrap();
    }
    writeln!(out).unwrap();

    // Extern declares for the bodies in other chunks this module calls.
    for body in ctx.all_bodies {
        let symbol = crate::lower::mangle_fn_name(&body.name);
        if referenced.contains(symbol.as_ref()) && !local_symbols.contains(symbol.as_ref()) {
            let decl = extern_declare_with(body, ctx.tcx, &sret_bodies);
            out.push_str(decl.trim_end());
            writeln!(out).unwrap();
        }
    }
    writeln!(out).unwrap();

    // Runtime declares - dedup by symbol name. Other module globals
    // (e.g. `static mut` `linkonce_odr` definitions a chunk emits once
    // per referencing body) dedup by their full line, since the same
    // static yields a byte-identical definition at every access site.
    let mut emitted_syms: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut emitted_lines: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for g in &globals_raw {
        if let Ok(()) = validate_global_decl_shape(g) {
            if let Some(rest) = g.strip_prefix("declare ") {
                if let Some(at_idx) = rest.find('@')
                    && let Some(open_idx) = rest[at_idx..].find('(')
                {
                    let sym = &rest[at_idx + 1..at_idx + open_idx];
                    let sym = sym.trim_matches('"');
                    if !emitted_syms.insert(sym.to_string()) {
                        continue;
                    }
                }
            } else if !emitted_lines.insert(g.as_str()) {
                continue;
            }
            writeln!(out, "{g}").unwrap();
        }
    }
    if !globals_raw.is_empty() {
        writeln!(out).unwrap();
    }

    // String pool - `private` so sequential IDs are safe within the chunk.
    let pool_text = string_pool.borrow().render();
    if !pool_text.is_empty() {
        out.push_str(&pool_text);
        writeln!(out).unwrap();
    }

    // RC type-meta blobs - one `private constant [N x i64]` per
    // RC-managed allocation shape, referenced by `gos_rc_alloc` sites.
    // Emitted in each chunk that references them; `private` makes each
    // object file self-contained.
    let mut emitted_any_meta = false;
    for (symbol, blob) in sorted_rc_metas(ctx.tcx) {
        if !referenced.contains(symbol) {
            continue;
        }
        let elems: Vec<String> = blob.iter().map(|v| format!("i64 {v}")).collect();
        writeln!(
            out,
            "@\"{symbol}\" = private constant [{} x i64] [{}]",
            blob.len(),
            elems.join(", ")
        )
        .unwrap();
        emitted_any_meta = true;
    }
    if emitted_any_meta {
        writeln!(out).unwrap();
    }

    for ir in &body_irs {
        out.push_str(ir);
        writeln!(out).unwrap();
    }

    // Closure thunks - `linkonce_odr` so the linker deduplicates across chunks.
    for name in &thunk_names {
        if let Some(thunk) = render_shape_thunk_linkonce(name) {
            out.push_str(&thunk);
            writeln!(out).unwrap();
        }
    }

    if target_is_windows()
        && chunk_indices
            .iter()
            .any(|&idx| body_has_wide_spawn(&ctx.all_bodies[idx]))
    {
        out.push_str(&render_spawn_wide_cabi_thunk());
        writeln!(out).unwrap();
    }

    if target_is_windows() {
        // Win64 handler-return thunks (`name$cabi`): emitted as a plain `define`
        // in the one chunk that owns the handler body, and as an extern `declare`
        // in every other chunk. Emitting `linkonce_odr` in every chunk would work
        // on ELF (which deduplicates `linkonce_odr` implicitly), but lld-link
        // (COFF/PE, Windows) requires an explicit COMDAT section for dedup and
        // treats bare `linkonce_odr` as a duplicate strong symbol error.
        // A shape thunk's wrapper is `linkonce_odr`, as the thunk it wraps is:
        // every chunk reaching that shape renders both, and the linker keeps
        // one.
        for name in &cabi_shape_thunks {
            let Some(param_tys) = shape_thunk_param_tys(name) else {
                continue;
            };
            out.push_str(&render_cabi_thunk_with_linkage(
                name,
                &param_tys,
                "linkonce_odr ",
            ));
            writeln!(out).unwrap();
        }
        for (name, arity) in &cabi_handlers {
            let handler_idx = ctx.all_bodies.iter().position(|b| b.name == *name);
            let owns_handler = handler_idx.is_some_and(|i| chunk_indices.contains(&i));
            let param_tys = cabi_thunk_param_tys(ctx.all_bodies, ctx.tcx, name, *arity);
            if owns_handler {
                out.push_str(&render_cabi_handler_thunk(name, &param_tys));
            } else {
                let param_list = param_tys.join(", ");
                writeln!(out, "declare <16 x i8> @\"{name}$cabi\"({param_list})").unwrap();
            }
            writeln!(out).unwrap();
        }
    }

    let chunk_bodies: Vec<&Body> = chunk_indices.iter().map(|&i| &ctx.all_bodies[i]).collect();
    if let Some(entry) = library_shutdown_entry(&chunk_bodies) {
        out.push_str(&entry);
    }

    // C `@main` shim lives in the chunk that owns `main`. Emitted whether
    // or not `main` was LLVM-lowered: when it fell back, `@"gos_main"` is
    // declared extern above and resolved at link time by the Cranelift companion.
    if let Some(idx) = main_idx {
        let main_body = &ctx.all_bodies[idx];
        let ret_ty = main_body.local_ty(gossamer_mir::Local::RETURN);
        let ret_is_unit = matches!(ctx.tcx.kind(ret_ty), Some(gossamer_types::TyKind::Unit));
        // A `Result`-returning main (explicit `-> Result<..>` or the implicit
        // `?`-desugared top-level main) lowers to a 2-word (i128) value packed
        // `(payload << 64) | disc`. The i64 path would truncate to the disc,
        // dropping the error payload; read the full i128 and hand the unpacked
        // disc + payload to the error-aware exit handler so an `Err` entry-point
        // result prints its Display chain to stderr and exits nonzero.
        let ret_is_result = !ret_is_unit && ctx.tcx.slot_bytes(ret_ty) == 16;
        writeln!(out, "define i32 @main(i32 %argc, ptr %argv) {{").unwrap();
        writeln!(out, "entry:").unwrap();
        writeln!(out, "  call void @gos_rt_program_start()").unwrap();
        writeln!(out, "  call void @gos_rt_set_args(i32 %argc, ptr %argv)").unwrap();
        if ret_is_unit {
            writeln!(out, "  call void @\"gos_main\"()").unwrap();
            // Routed through the same exit handler as a value-returning main so
            // goroutines still running when `main` falls off the end are
            // drained, and their output reaches the user, on every tier.
            writeln!(out, "  %code = call i32 @gos_rt_main_exit_code(i64 0)").unwrap();
            writeln!(out, "  ret i32 %code").unwrap();
        } else if ret_is_result {
            writeln!(out, "  %r = call i128 @\"gos_main\"()").unwrap();
            writeln!(out, "  %disc = trunc i128 %r to i64").unwrap();
            writeln!(out, "  %hi = lshr i128 %r, 64").unwrap();
            writeln!(out, "  %payload = trunc i128 %hi to i64").unwrap();
            writeln!(
                out,
                "  %code = call i32 @gos_rt_main_exit_code_err(i64 %disc, i64 %payload)"
            )
            .unwrap();
            writeln!(out, "  ret i32 %code").unwrap();
        } else {
            writeln!(out, "  %r = call i64 @\"gos_main\"()").unwrap();
            writeln!(out, "  call void @gos_rt_flush_stdout()").unwrap();
            writeln!(out, "  %code = call i32 @gos_rt_main_exit_code(i64 %r)").unwrap();
            writeln!(out, "  ret i32 %code").unwrap();
        }
        writeln!(out, "}}").unwrap();
    }

    writeln!(out).unwrap();
    writeln!(out, "!0 = !{{}}").unwrap();
    out.push_str(TBAA_METADATA);

    Ok(out)
}

/// Core of the parallel, incremental build path.
///
/// **Partition.** Bodies split into LLVM modules by the source module that
/// declares them ([`codegen_chunks`]), and a release chunk also carries
/// `available_externally` copies of the callees it reaches in other chunks
/// ([`chunk_imports`]).
///
/// **Render and key (serial).** Every chunk is lowered to IR text, and the
/// text is the cache identity ([`chunk_cache_key`]): a chunk whose text an
/// earlier build compiled is served from the object cache, and only the rest
/// reach LLVM. Rendering is serial because [`Lowerer`] uses `Rc<RefCell<_>>`
/// state that is not `Send`; it is a small fraction of what `opt` + `llc`
/// cost.
///
/// **Compile (parallel).** Cache misses run one `opt` + `llc` pair each on a
/// pool of [`codegen_job_limit`] workers.
///
/// Returns `(object_paths, triple, fallback_body_names)`.
fn compile_bodies_parallel_incremental(
    bodies: &[Body],
    tcx: &TyCtxt,
    obj_dir: &std::path::Path,
    _allow_fallback: bool,
) -> Result<(Vec<PathBuf>, String, Vec<String>)> {
    let triple = host_triple();
    let llvm_triple = llvm_target_triple_for(&triple);
    let profile = opt_profile();
    let dump = std::env::var("GOS_LLVM_DUMP").is_ok();

    // Precompute program-wide lookup tables shared across all lowerers.
    let mut fn_name_by_def: std::collections::HashMap<u32, String> =
        std::collections::HashMap::new();
    let mut param_tys_by_name: std::collections::HashMap<String, Vec<gossamer_types::Ty>> =
        std::collections::HashMap::new();
    for body in bodies {
        if let Some(def) = body.def {
            fn_name_by_def.insert(def.local, body.name.clone());
        }
        let param_tys: Vec<gossamer_types::Ty> = (0..body.arity)
            .map(|i| body.local_ty(gossamer_mir::Local(i + 1)))
            .collect();
        param_tys_by_name.insert(body.name.clone(), param_tys);
    }
    let capture_summary = gossamer_mir::build_capture_summary(bodies);

    let cache_dir = active_cache_dir().filter(|_| !dump);
    if let Some(ref cd) = cache_dir {
        let _ = std::fs::create_dir_all(cd);
    }

    let ctx = ModuleCtx {
        all_bodies: bodies,
        tcx,
        fn_name_by_def: &fn_name_by_def,
        param_tys_by_name: &param_tys_by_name,
        capture_summary: &capture_summary,
        triple: &llvm_triple,
    };

    // Reproducible mode pins the build to one module so the artifact depends
    // only on the source and the target.
    let body_chunks = if want_reproducible() {
        vec![(0..bodies.len()).collect()]
    } else {
        codegen_chunks(bodies)
    };
    let imports = chunk_imports(bodies, &body_chunks, profile);

    // (chunk_idx, cache_key, ll_path, obj_path)
    let mut chunks_to_compile: Vec<(usize, String, PathBuf, PathBuf)> = Vec::new();
    let mut result_objects: Vec<(usize, PathBuf)> = Vec::new();
    for (chunk_idx, body_indices) in body_chunks.iter().enumerate() {
        let obj_path = obj_dir.join(format!("chunk{chunk_idx}.o"));
        let ll_path = obj_dir.join(format!("chunk{chunk_idx}.ll"));
        let started = std::time::Instant::now();
        let ir =
            render_chunk_module(body_indices, &imports[chunk_idx], &ctx).map_err(|e| match e {
                BuildError::InternalLoweringBug(msg) => {
                    anyhow!("llvm backend internal lowering bug: {msg}")
                }
                BuildError::Tool(msg) => anyhow!("llvm backend: tool: {msg}"),
                BuildError::Io(err) => err,
            })?;
        record_phase(&RENDER_US, started);
        let key = chunk_cache_key(&ir, &llvm_triple, profile);
        let hit = cache_dir
            .as_ref()
            .map(|cd| cd.join(format!("{key}.o")))
            .filter(|p| p.exists())
            .is_some_and(|hit| std::fs::copy(&hit, &obj_path).is_ok());
        if std::env::var_os("GOS_PIPELINE_TRACE").is_some() {
            eprintln!(
                "llvm pipeline: chunk{chunk_idx} `{}` {} bodies, {} imported, {} IR bytes, {}",
                body_indices
                    .first()
                    .map_or("", |&idx| body_module(&bodies[idx].name)),
                body_indices.len(),
                imports[chunk_idx].len(),
                ir.len(),
                if hit { "cached" } else { "compiling" },
            );
        }
        if hit {
            result_objects.push((chunk_idx, obj_path));
            continue;
        }
        std::fs::write(&ll_path, ir.as_bytes())
            .with_context(|| format!("writing {}", ll_path.display()))?;
        chunks_to_compile.push((chunk_idx, key, ll_path, obj_path));
    }

    // Stitch chunk files into unit.ll for tools / tests that expect
    // "llvm backend: IR at <path>" on stderr when GOS_LLVM_DUMP=1.
    if dump {
        let dump_path = obj_dir.join("unit.ll");
        if let Ok(mut f) = std::fs::File::create(&dump_path) {
            use std::io::Write as _;
            for (chunk_idx, _, ll_path, _) in &chunks_to_compile {
                if let Ok(text) = std::fs::read_to_string(ll_path) {
                    let _ = write!(f, "; === chunk{chunk_idx} ===\n{text}\n");
                }
            }
        }
        eprintln!("llvm backend: IR at {}", dump_path.display());
    }

    let err_slot: parking_lot::Mutex<Option<anyhow::Error>> = parking_lot::Mutex::new(None);
    let compiled: parking_lot::Mutex<Vec<(usize, PathBuf)>> = parking_lot::Mutex::new(Vec::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = codegen_job_limit().min(chunks_to_compile.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let slot = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((chunk_idx, cache_key, ll_path, obj_path)) =
                        chunks_to_compile.get(slot)
                    else {
                        return;
                    };
                    if err_slot.lock().is_some() {
                        return;
                    }
                    if let Err(e) = invoke_llc_pipeline(
                        ll_path,
                        obj_path,
                        &llvm_triple,
                        /*announce=*/ false,
                    ) {
                        *err_slot.lock() = Some(e);
                        return;
                    }
                    if !dump {
                        let _ = std::fs::remove_file(ll_path);
                    }
                    if let Some(cd) = &cache_dir {
                        publish_cached_object(obj_path, cd, cache_key);
                    }
                    compiled.lock().push((*chunk_idx, obj_path.clone()));
                }
            });
        }
    });

    if let Some(err) = err_slot.into_inner() {
        return Err(err);
    }
    result_objects.extend(compiled.into_inner());
    result_objects.sort_by_key(|(i, _)| *i);
    Ok((
        result_objects.into_iter().map(|(_, p)| p).collect(),
        triple,
        Vec::new(),
    ))
}

/// Copies a freshly compiled object into the cache under `key`. The copy is
/// written beside its final name and renamed into place, so a concurrent
/// build reading the cache sees either the whole object or none of it.
fn publish_cached_object(obj_path: &std::path::Path, cache_dir: &std::path::Path, key: &str) {
    let staged = cache_dir.join(format!("{key}.o.{}.tmp", std::process::id()));
    if std::fs::copy(obj_path, &staged).is_ok()
        && std::fs::rename(&staged, cache_dir.join(format!("{key}.o"))).is_err()
    {
        let _ = std::fs::remove_file(&staged);
    }
}

fn dump_mir(bodies: &[Body], tcx: &TyCtxt) {
    for body in bodies {
        eprintln!("=== MIR {} ===", body.name);
        for (i, local) in body.locals.iter().enumerate() {
            eprintln!(
                "  local {i}: {}",
                gossamer_types::printer::render_ty(tcx, local.ty)
            );
        }
        for (i, block) in body.blocks.iter().enumerate() {
            eprintln!("  bb{i}:");
            for stmt in &block.stmts {
                eprintln!("    {:?}", stmt.kind);
            }
            eprintln!("    -> {:?}", block.terminator);
        }
    }
}

/// LLVM build entry point with the legacy fallback-shaped return
/// type. Lowering bugs are hard errors, so successful calls return
/// an empty `fallback_bodies` list.
pub fn compile_with_fallback(bodies: &[Body], tcx: &TyCtxt) -> Result<CompileOutcome> {
    if std::env::var("GOS_LLVM_DUMP_MIR").is_ok() {
        dump_mir(bodies, tcx);
    }
    let triple = host_triple();
    let llvm_triple = llvm_target_triple_for(&triple);
    let tmp_dir = pipeline_tmp_dir()?;
    let ll_path = tmp_dir.join("unit.ll");
    let fallback_bodies =
        render_module_to_path(bodies, tcx, &ll_path, /*allow_fallback=*/ true)?;
    let obj_path = tmp_dir.join("unit.o");
    invoke_llc_pipeline(&ll_path, &obj_path, &llvm_triple, /*announce=*/ true)?;
    let bytes =
        std::fs::read(&obj_path).with_context(|| format!("reading {}", obj_path.display()))?;
    if std::env::var("GOS_LLVM_DUMP").is_err() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
    Ok(CompileOutcome {
        object: NativeObject { triple, bytes },
        fallback_bodies,
    })
}

/// Path-oriented variant of [`compile_with_fallback`].
///
/// Writes LLVM objects into `obj_dir` and returns the list of object
/// paths, the host triple, and an empty fallback-body list. When the
/// program has fewer than two bodies or DWARF emission is requested,
/// the function uses the serial single-file pipeline for simplicity.
///
/// The parallel path compiles each body in its own mini `.ll` module
/// and runs `opt` + `llc` concurrently across up to
/// `PARALLEL_MAX_THREADS` threads. Objects for bodies whose MIR
/// hash matches a previously cached result are reused directly from
/// the incremental cache, skipping lowering and compilation entirely.
pub fn compile_with_fallback_at_path(
    bodies: &[Body],
    tcx: &TyCtxt,
    obj_dir: &std::path::Path,
) -> Result<(Vec<PathBuf>, String, Vec<String>)> {
    if std::env::var("GOS_LLVM_DUMP_MIR").is_ok() {
        dump_mir(bodies, tcx);
    }
    std::fs::create_dir_all(obj_dir)
        .with_context(|| format!("creating obj_dir {}", obj_dir.display()))?;

    // Serial path: DWARF needs the whole-module in-memory mutator,
    // and single-body programs gain nothing from parallelism.
    if want_dwarf() || bodies.len() < 2 {
        let triple = host_triple();
        let llvm_triple = llvm_target_triple_for(&triple);
        let ll_path = obj_dir.join("unit.ll");
        let obj_path = obj_dir.join("unit.o");
        let fallback_bodies =
            render_module_to_path(bodies, tcx, &ll_path, /*allow_fallback=*/ true)?;
        invoke_llc_pipeline(&ll_path, &obj_path, &llvm_triple, /*announce=*/ true)?;
        if std::env::var("GOS_LLVM_DUMP").is_err() {
            let _ = std::fs::remove_file(&ll_path);
        }
        return Ok((vec![obj_path], triple, fallback_bodies));
    }

    compile_bodies_parallel_incremental(bodies, tcx, obj_dir, /*allow_fallback=*/ true)
}

/// Streaming renderer: writes the full module to `ll_path` without
/// retaining a complete IR `String` in memory. Bodies are emitted
/// directly to a temp body file as they're lowered, then spliced
/// into the final IR file behind the header / globals / pool.
///
/// Returns an empty body list on success. `allow_fallback` is retained
/// for API compatibility; lowering bugs always abort.
fn render_module_to_path(
    bodies: &[Body],
    tcx: &TyCtxt,
    ll_path: &std::path::Path,
    _allow_fallback: bool,
) -> Result<Vec<String>> {
    use std::io::{BufWriter, Write as _};

    if let Some(parent) = ll_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let body_path = ll_path.with_file_name(match ll_path.file_name() {
        Some(name) => format!("{}.body", name.to_string_lossy()),
        None => "module.body".to_string(),
    });

    let _exports = crate::lower::ExportScope::enter(bodies);
    let mut fn_name_by_def: std::collections::HashMap<u32, String> =
        std::collections::HashMap::new();
    let mut param_tys_by_name: std::collections::HashMap<String, Vec<gossamer_types::Ty>> =
        std::collections::HashMap::new();
    for body in bodies {
        if let Some(def) = body.def {
            fn_name_by_def.insert(def.local, body.name.clone());
        }
        // Per-callee param-type table: `emit_named_call` consults
        // this to pass `&Adt` arguments as the heap pointer
        // (loaded from the slot) rather than the slot address.
        // Without it, `length(&xs)` receives the slot's address
        // and the disc read at offset 0 misses the heap blob.
        let param_tys: Vec<gossamer_types::Ty> = (0..body.arity)
            .map(|i| body.local_ty(gossamer_mir::Local(i + 1)))
            .collect();
        param_tys_by_name.insert(body.name.clone(), param_tys);
    }

    let mut globals: Vec<String> = Vec::new();
    let fallback_bodies: Vec<String> = Vec::new();
    let string_pool = std::rc::Rc::new(std::cell::RefCell::new(StringPool::default()));

    // Inter-procedural capture summary: feeds the cleanup pass so
    // owning bindings whose only outbound use is a non-capturing
    // user fn can get a precise per-block drop instead of being
    // forced into the escape set.
    let capture_summary = gossamer_mir::build_capture_summary(bodies);

    let body_file = std::fs::File::create(&body_path)
        .with_context(|| format!("creating {}", body_path.display()))?;
    let mut body_w = BufWriter::with_capacity(64 * 1024, body_file);

    let sret_bodies = collect_sret_bodies(bodies, tcx);
    let cabi_handlers = collect_cabi_handlers(bodies);
    let cabi_thunk_sites = collect_cabi_thunk_sites(bodies);
    let mut cabi_shape_thunks: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for body in bodies {
        let mut lowerer = Lowerer::new(body, tcx);
        lowerer.fn_name_by_def.clone_from(&fn_name_by_def);
        lowerer.param_tys_by_name.clone_from(&param_tys_by_name);
        lowerer.strings = string_pool.clone();
        lowerer.capture_summary = capture_summary.clone();
        lowerer.cabi_handlers.clone_from(&cabi_handlers);
        if let Some(sites) = cabi_thunk_sites.get(&body.name) {
            lowerer.cabi_thunk_sites = sites.keys().copied().collect();
            cabi_shape_thunks.extend(sites.values().cloned());
        }
        lowerer.sret_bodies.clone_from(&sret_bodies);
        match lowerer.lower() {
            Ok(text) => {
                body_w
                    .write_all(text.as_bytes())
                    .with_context(|| format!("writing {}", body_path.display()))?;
                body_w
                    .write_all(b"\n")
                    .with_context(|| format!("writing {}", body_path.display()))?;
                globals.extend(lowerer.take_module_globals());
            }
            Err(BuildError::InternalLoweringBug(msg)) => {
                let _ = std::fs::remove_file(&body_path);
                return Err(anyhow!(
                    "llvm backend internal lowering bug in `{fn_name}`: {msg}",
                    fn_name = body.name,
                ));
            }
            Err(BuildError::Tool(msg)) => {
                let _ = std::fs::remove_file(&body_path);
                return Err(anyhow!("llvm backend: tool: {msg}"));
            }
            Err(BuildError::Io(err)) => {
                let _ = std::fs::remove_file(&body_path);
                return Err(err);
            }
        }
    }

    let mut thunk_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for body in bodies {
        collect_thunk_names_in_body(body, &mut thunk_names);
    }
    for name in &thunk_names {
        if let Some(text) = render_shape_thunk(name) {
            body_w
                .write_all(text.as_bytes())
                .with_context(|| format!("writing {}", body_path.display()))?;
            body_w
                .write_all(b"\n")
                .with_context(|| format!("writing {}", body_path.display()))?;
        }
    }
    if target_is_windows() && bodies.iter().any(body_has_wide_spawn) {
        body_w
            .write_all(render_spawn_wide_cabi_thunk().as_bytes())
            .with_context(|| format!("writing {}", body_path.display()))?;
        body_w
            .write_all(b"\n")
            .with_context(|| format!("writing {}", body_path.display()))?;
    }
    if target_is_windows() {
        for (name, arity) in &cabi_handlers {
            let param_tys = cabi_thunk_param_tys(bodies, tcx, name, *arity);
            body_w
                .write_all(render_cabi_handler_thunk(name, &param_tys).as_bytes())
                .with_context(|| format!("writing {}", body_path.display()))?;
        }
        for name in &cabi_shape_thunks {
            let Some(param_tys) = shape_thunk_param_tys(name) else {
                continue;
            };
            body_w
                .write_all(
                    render_cabi_thunk_with_linkage(name, &param_tys, "linkonce_odr ").as_bytes(),
                )
                .with_context(|| format!("writing {}", body_path.display()))?;
        }
    }

    let all: Vec<&Body> = bodies.iter().collect();
    if let Some(entry) = library_shutdown_entry(&all) {
        body_w.write_all(entry.as_bytes())?;
    }
    if let Some(user_main) = bodies.iter().find(|b| b.name == "main") {
        let ret_ty = user_main.local_ty(gossamer_mir::Local::RETURN);
        let ret_is_unit = matches!(tcx.kind(ret_ty), Some(gossamer_types::TyKind::Unit));
        // A `Result`-returning main lowers to a 2-word (i128) value packed
        // `(payload << 64) | disc`; the i64 path truncates to the disc and
        // drops the error payload. Read the full i128 and hand the unpacked
        // disc + payload to the error-aware exit handler so an `Err` entry-point
        // result prints its Display chain to stderr and exits nonzero.
        let ret_is_result = !ret_is_unit && tcx.slot_bytes(ret_ty) == 16;
        writeln!(body_w, "define i32 @main(i32 %argc, ptr %argv) {{")?;
        writeln!(body_w, "entry:")?;
        writeln!(body_w, "  call void @gos_rt_program_start()")?;
        writeln!(body_w, "  call void @gos_rt_set_args(i32 %argc, ptr %argv)")?;
        if ret_is_unit {
            writeln!(body_w, "  call void @\"gos_main\"()")?;
            writeln!(body_w, "  call void @gos_rt_flush_stdout()")?;
            writeln!(body_w, "  ret i32 0")?;
        } else if ret_is_result {
            writeln!(body_w, "  %r = call i128 @\"gos_main\"()")?;
            writeln!(body_w, "  %disc = trunc i128 %r to i64")?;
            writeln!(body_w, "  %hi = lshr i128 %r, 64")?;
            writeln!(body_w, "  %payload = trunc i128 %hi to i64")?;
            writeln!(
                body_w,
                "  %code = call i32 @gos_rt_main_exit_code_err(i64 %disc, i64 %payload)"
            )?;
            writeln!(body_w, "  ret i32 %code")?;
        } else {
            writeln!(body_w, "  %r = call i64 @\"gos_main\"()")?;
            writeln!(body_w, "  call void @gos_rt_flush_stdout()")?;
            writeln!(body_w, "  %code = call i32 @gos_rt_main_exit_code(i64 %r)")?;
            writeln!(body_w, "  ret i32 %code")?;
        }
        writeln!(body_w, "}}")?;
    }
    writeln!(body_w)?;
    writeln!(body_w, "!0 = !{{}}")?;
    body_w.write_all(TBAA_METADATA.as_bytes())?;

    body_w
        .flush()
        .with_context(|| format!("flushing {}", body_path.display()))?;
    drop(body_w);

    // Now write the final IR file. Header → special decls →
    // sorted/deduped globals → string pool → body file content.
    globals.sort();
    globals.dedup();
    let ll_file = std::fs::File::create(ll_path)
        .with_context(|| format!("creating {}", ll_path.display()))?;
    let mut ll_w = BufWriter::with_capacity(64 * 1024, ll_file);
    let triple = llvm_target_triple_for(&host_triple());
    writeln!(ll_w, "; ModuleID = \"gossamer\"")?;
    if let Some(dl) = module_datalayout(&triple) {
        writeln!(ll_w, "target datalayout = \"{dl}\"")?;
    }
    writeln!(ll_w, "target triple = \"{triple}\"")?;
    if want_reproducible() {
        writeln!(ll_w, "; reproducible-build = true")?;
    }
    writeln!(ll_w)?;
    for d in LLVM_SPECIAL_DECLS {
        writeln!(ll_w, "{d}")?;
    }
    writeln!(ll_w)?;
    // Shape-validate each accumulated global. The `runtime_refs`
    // BTreeSet inside `Lowerer` accepts arbitrary strings; a
    // malformed entry corrupts the IR string. Each entry must be
    // either an `@symbol = ...` definition or a `declare ...`
    // function declaration.
    // Dedupe declarations by symbol name: each lowerer body
    // accumulates its own `declare` lines, but two bodies that call
    // the same runtime helper with ABI-compatible-but-different
    // operand types (e.g. `gos_rt_result_new(i64, i64)` vs
    // `(i64, ptr)`) would each emit a `declare` and LLVM rejects
    // the redefinition. Pick the first declaration we see for a
    // given symbol; the calls themselves are individually typed and
    // the ABI tolerates the i64/ptr substitution on x86_64.
    let mut emitted_decls: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut emitted_lines: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for g in &globals {
        validate_global_decl_shape(g)?;
        if let Some(rest) = g.strip_prefix("declare ") {
            // Parse "<ret> @<name>(...)" - name is the substring
            // between '@' and '('.
            if let Some(at_idx) = rest.find('@')
                && let Some(open_idx) = rest[at_idx..].find('(')
            {
                let symbol = &rest[at_idx + 1..at_idx + open_idx];
                let symbol = symbol.trim_matches('"');
                if !emitted_decls.insert(symbol.to_string()) {
                    continue;
                }
            }
        } else if !emitted_lines.insert(g.as_str()) {
            continue;
        }
        writeln!(ll_w, "{g}")?;
    }
    if !globals.is_empty() {
        writeln!(ll_w)?;
    }
    let pool_text = string_pool.borrow().render();
    if !pool_text.is_empty() {
        ll_w.write_all(pool_text.as_bytes())?;
        writeln!(ll_w)?;
    }
    // RC type-meta blobs. The chunked renderer emits these per chunk;
    // this streaming single-unit path previously skipped them, leaving
    // every `@"gos_rc_meta_*"` reference undefined when a program with
    // RC-managed allocations routed through here (fallback bodies,
    // DWARF, single-body programs).
    let mut any_meta = false;
    for (symbol, blob) in sorted_rc_metas(tcx) {
        let elems: Vec<String> = blob.iter().map(|v| format!("i64 {v}")).collect();
        writeln!(
            ll_w,
            "@\"{symbol}\" = private constant [{} x i64] [{}]",
            blob.len(),
            elems.join(", ")
        )?;
        any_meta = true;
    }
    if any_meta {
        writeln!(ll_w)?;
    }
    let mut body_in = std::fs::File::open(&body_path)
        .with_context(|| format!("opening {}", body_path.display()))?;
    std::io::copy(&mut body_in, &mut ll_w)
        .with_context(|| format!("appending body buffer to {}", ll_path.display()))?;
    drop(body_in);
    let _ = std::fs::remove_file(&body_path);
    ll_w.flush()
        .with_context(|| format!("flushing {}", ll_path.display()))?;
    drop(ll_w);

    // DWARF emission needs to insert `!dbg !N` after each function
    // header. Defer to the in-memory string mutator on the rare `-g`
    // path; the streaming default never pays this cost.
    if want_dwarf() {
        let mut content = std::fs::read_to_string(ll_path)
            .with_context(|| format!("reading {}", ll_path.display()))?;
        emit_dwarf_metadata(&mut content, bodies);
        std::fs::write(ll_path, &content)
            .with_context(|| format!("writing {}", ll_path.display()))?;
    }

    Ok(fallback_bodies)
}

#[cfg(test)]
mod codegen_partition_tests {
    use super::{
        JOB_CEILING, OptProfile, chunk_cache_key, chunk_imports, codegen_chunks, codegen_job_limit,
        collect_quoted_symbols,
    };
    use gossamer_lex::{SourceMap, Span};
    use gossamer_mir::{
        BasicBlock, BlockId, Body, ConstValue, Local, Operand, Place, Statement, StatementKind,
        Terminator,
    };

    fn span() -> Span {
        let mut map = SourceMap::new();
        let file = map.add_file("partition.gos", "");
        Span::new(file, 0, 0)
    }

    fn body_with_cost(name: &str, callees: &[&str], stmts: usize) -> Body {
        let span = span();
        let mut blocks: Vec<BasicBlock> = callees
            .iter()
            .enumerate()
            .map(|(i, callee)| BasicBlock {
                id: BlockId(u32::try_from(i).unwrap_or(0)),
                stmts: Vec::new(),
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str((*callee).to_string())),
                    args: Vec::new(),
                    destination: Place::local(Local(0)),
                    target: Some(BlockId(u32::try_from(i + 1).unwrap_or(0))),
                },
                span,
                terminator_span: None,
                terminator_inlined: None,
            })
            .collect();
        blocks.push(BasicBlock {
            id: BlockId(u32::try_from(callees.len()).unwrap_or(0)),
            stmts: (0..stmts)
                .map(|_| Statement {
                    kind: StatementKind::Nop,
                    span,
                    inlined: None,
                })
                .collect(),
            terminator: Terminator::Return,
            span,
            terminator_span: None,
            terminator_inlined: None,
        });
        Body {
            name: name.to_string(),
            def: None,
            arity: 0,
            locals: Vec::new(),
            blocks,
            span,
        }
    }

    fn body(name: &str, callees: &[&str]) -> Body {
        body_with_cost(name, callees, 0)
    }

    /// A program large enough to be split, with `extra` appended.
    fn program(extra: Vec<Body>) -> Vec<Body> {
        let mut bodies = Vec::new();
        for module in ["alpha", "beta", "gamma"] {
            for i in 0..8 {
                bodies.push(body(&format!("{module}::f{i}"), &[]));
            }
        }
        bodies.extend(extra);
        bodies
    }

    fn chunk_names(bodies: &[Body], chunks: &[Vec<usize>]) -> Vec<Vec<String>> {
        chunks
            .iter()
            .map(|chunk| chunk.iter().map(|&i| bodies[i].name.clone()).collect())
            .collect()
    }

    #[test]
    fn small_program_compiles_as_one_module() {
        let bodies = vec![
            body("alpha::a", &[]),
            body("beta::b", &[]),
            body("main", &[]),
        ];
        assert_eq!(codegen_chunks(&bodies), vec![vec![0, 1, 2]]);
    }

    #[test]
    fn chunks_follow_source_modules() {
        let bodies = program(vec![body("main", &["alpha::f0"])]);
        let names = chunk_names(&bodies, &codegen_chunks(&bodies));
        assert_eq!(names.len(), 4, "{names:?}");
        assert_eq!(names[0], vec!["main".to_string()]);
        assert!(names[1].iter().all(|n| n.starts_with("alpha::")));
        assert!(names[3].iter().all(|n| n.starts_with("gamma::")));
    }

    #[test]
    fn adding_a_body_leaves_other_modules_chunks_unchanged() {
        let before = program(vec![body("main", &[])]);
        let after = program(vec![body("main", &[]), body("beta::added", &[])]);
        let before_names = chunk_names(&before, &codegen_chunks(&before));
        let after_names = chunk_names(&after, &codegen_chunks(&after));
        assert_eq!(before_names[1], after_names[1], "alpha moved");
        assert_eq!(before_names[3], after_names[3], "gamma moved");
        assert_ne!(before_names[2], after_names[2]);
    }

    #[test]
    fn recursive_cycle_across_modules_shares_one_chunk() {
        let bodies = program(vec![
            body("alpha::left", &["gamma::right"]),
            body("gamma::right", &["alpha::left"]),
        ]);
        let chunks = codegen_chunks(&bodies);
        let left = bodies.iter().position(|b| b.name == "alpha::left");
        let right = bodies.iter().position(|b| b.name == "gamma::right");
        let chunk_of = |idx: Option<usize>| {
            chunks
                .iter()
                .position(|chunk| idx.is_some_and(|i| chunk.contains(&i)))
        };
        assert!(chunk_of(left).is_some());
        assert_eq!(chunk_of(left), chunk_of(right), "{chunks:?}");
        assert_eq!(chunks, codegen_chunks(&bodies), "partition must be stable");
    }

    #[test]
    fn release_imports_small_callees_from_other_chunks_transitively() {
        let bodies = program(vec![
            body("main", &["alpha::hot"]),
            body_with_cost("alpha::hot", &["beta::leaf"], 4),
            body_with_cost("beta::leaf", &[], 2),
            body("gamma::caller", &["alpha::huge", "main"]),
            body_with_cost("alpha::huge", &[], 10_000),
        ]);
        let chunks = codegen_chunks(&bodies);
        let imports = chunk_imports(&bodies, &chunks, OptProfile::Release);
        let named = |chunk: usize| -> Vec<&str> {
            imports[chunk]
                .iter()
                .map(|&i| bodies[i].name.as_str())
                .collect()
        };
        let chunk_holding = |name: &str| {
            chunks
                .iter()
                .position(|c| c.iter().any(|&i| bodies[i].name == name))
                .unwrap_or(usize::MAX)
        };
        let main_imports = named(chunk_holding("main"));
        assert!(main_imports.contains(&"alpha::hot"), "{main_imports:?}");
        assert!(main_imports.contains(&"beta::leaf"), "{main_imports:?}");
        let gamma_imports = named(chunk_holding("gamma::caller"));
        assert!(!gamma_imports.contains(&"alpha::huge"), "{gamma_imports:?}");
        assert!(!gamma_imports.contains(&"main"), "{gamma_imports:?}");
        let debug = chunk_imports(&bodies, &chunks, OptProfile::Debug);
        assert!(debug.iter().all(Vec::is_empty));
    }

    #[test]
    fn object_cache_key_is_the_module_text_under_its_settings() {
        let triple = "x86_64-unknown-linux-gnu";
        let a = chunk_cache_key("define i64 @\"f\"() #0 {}", triple, OptProfile::Release);
        let same = chunk_cache_key("define i64 @\"f\"() #0 {}", triple, OptProfile::Release);
        let other_text = chunk_cache_key("define i64 @\"g\"() #0 {}", triple, OptProfile::Release);
        let other_profile = chunk_cache_key("define i64 @\"f\"() #0 {}", triple, OptProfile::Debug);
        assert_eq!(a, same);
        assert_ne!(a, other_text);
        assert_ne!(a, other_profile);
    }

    #[test]
    fn quoted_symbols_are_collected_from_module_text() {
        let text =
            "  %1 = call i64 @\"alpha::f\"(ptr @\"gos_rc_meta_x\")\n  call void @gos_rt_x()\n";
        let mut out = std::collections::HashSet::new();
        collect_quoted_symbols(text, &mut out);
        assert!(out.contains("alpha::f"));
        assert!(out.contains("gos_rc_meta_x"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn default_codegen_jobs_stay_under_the_ceiling() {
        if std::env::var_os("GOS_LLVM_JOBS").is_some() {
            return;
        }
        let jobs = codegen_job_limit();
        assert!((1..=JOB_CEILING).contains(&jobs), "{jobs}");
    }
}
