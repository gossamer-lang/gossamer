//! Module-level assembly: runtime symbol declarations +
//! per-function lowering + `llc -O3` invocation.

use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use gossamer_mir::Body;
use gossamer_types::TyCtxt;

use crate::lower::{Lowerer, StringPool};

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

/// Minimum bodies per parallel chunk, preserving inlining across small programs.
///
/// When a hot helper is compiled in a separate chunk from its caller, opt cannot
/// inline it across the module boundary. Keeping chunks at >= 10 bodies ensures
/// small programs stay in one module (full inlining) while large programs still
/// benefit from parallel codegen.
const MIN_BODIES_PER_CHUNK: usize = 10;

/// Default LLVM process fan-out is deliberately lower than CPU parallelism.
/// Each integrated Clang child commonly touches 45 to 65 MiB, so eight small
/// chunks can multiply a nominal 75 MiB compiler into a 450+ MiB process tree.
/// Repeated module parsing and declarations also make one process competitive
/// on small builds. The default is therefore one child regardless of source
/// size. `GOS_LLVM_JOBS` is the explicit throughput-first override, keeping
/// peak RAM a deliberate user choice rather than a surprise from host cores.
fn codegen_job_limit(_body_count: usize) -> usize {
    if let Ok(value) = std::env::var("GOS_LLVM_JOBS")
        && let Ok(jobs) = value.parse::<usize>()
        && jobs > 0
    {
        return jobs.min(PARALLEL_MAX_THREADS);
    }
    1
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

/// Stable cache key for one body: mixes the body name, its complete MIR
/// representation, target triple, profile, and compiler fingerprint. The
/// result is computed once per body per build and reused for cache lookup and
/// publication.
fn body_cache_key(
    body: &Body,
    triple: &str,
    profile: OptProfile,
    cabi_handler_arity: Option<usize>,
) -> String {
    use std::fmt::Write as _;

    let started = std::time::Instant::now();
    let mut digest = DigestWriter::new(b"gossamer-llvm-body-cache-v3\0");
    digest.update(body.name.as_bytes());
    digest.update(b"\0");
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
    match cabi_handler_arity {
        Some(arity) => {
            digest.update(b"runtime-handler:");
            digest.update(arity.to_string().as_bytes());
        }
        None => digest.update(b"gossamer-call-abi"),
    }
    digest.update(b"\0");
    write!(&mut digest, "{body:?}").expect("hashing MIR through fmt cannot fail");
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

fn chunk_cache_key(chunk_indices: &[usize], body_cache_keys: &[String]) -> String {
    let mut digest = DigestWriter::new(b"gossamer-llvm-chunk-cache-v2\0");
    for &idx in chunk_indices {
        digest.update(body_cache_keys[idx].as_bytes());
        digest.update(b"\0");
    }
    digest.finish()
}

/// Partitions bodies by call-graph strongly connected component, then balances
/// whole components across the requested worker count. Recursive cycles stay
/// in one LLVM module, preserving native inlining and avoiding duplicate
/// declarations inside the hottest mutually recursive paths.
fn codegen_chunks(bodies: &[Body], requested_chunks: usize) -> Vec<Vec<usize>> {
    if requested_chunks <= 1 || bodies.len() <= 1 {
        return vec![(0..bodies.len()).collect()];
    }
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
                && !edges[idx].contains(&target)
            {
                edges[idx].push(target);
            }
        }
        edges[idx].sort_unstable();
    }

    let mut tarjan = Tarjan {
        edges: &edges,
        next_index: 0,
        indices: vec![None; bodies.len()],
        low: vec![0; bodies.len()],
        stack: Vec::new(),
        on_stack: vec![false; bodies.len()],
        components: Vec::new(),
    };
    for node in 0..bodies.len() {
        if tarjan.indices[node].is_none() {
            tarjan.visit(node);
        }
    }
    tarjan.components.sort_by(|left, right| {
        right
            .len()
            .cmp(&left.len())
            .then_with(|| left[0].cmp(&right[0]))
    });
    let n_chunks = requested_chunks.min(tarjan.components.len()).max(1);
    let mut chunks = vec![Vec::new(); n_chunks];
    for component in tarjan.components {
        let target = chunks
            .iter()
            .enumerate()
            .min_by_key(|(idx, chunk)| (chunk.len(), *idx))
            .map_or(0, |(idx, _)| idx);
        chunks[target].extend(component);
    }
    for chunk in &mut chunks {
        chunk.sort_unstable();
    }
    chunks
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

/// Renders all bodies in `chunk_indices` as a single LLVM IR module.
///
/// Bodies not in the chunk get `declare` stubs; bodies in the chunk get
/// `define`. Lowering bugs are returned immediately.
fn render_chunk_module(chunk_indices: &[usize], ctx: &ModuleCtx<'_>) -> Result<String, BuildError> {
    let chunk_set: std::collections::HashSet<usize> = chunk_indices.iter().copied().collect();

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

    for &idx in chunk_indices {
        let body = &ctx.all_bodies[idx];
        if body.name == "main" {
            main_idx = Some(idx);
        }

        let mut lowerer = crate::lower::Lowerer::new(body, ctx.tcx);
        lowerer.fn_name_by_def.clone_from(ctx.fn_name_by_def);
        lowerer.param_tys_by_name.clone_from(ctx.param_tys_by_name);
        lowerer.strings = string_pool.clone();
        lowerer.capture_summary = ctx.capture_summary.clone();
        lowerer.cabi_handlers.clone_from(&cabi_handlers);

        match lowerer.lower() {
            Ok(text) => {
                globals_raw.extend(lowerer.take_module_globals());
                collect_thunk_names_in_body(body, &mut thunk_names);
                body_irs.push(text);
            }
            Err(BuildError::InternalLoweringBug(msg)) => {
                return Err(BuildError::InternalLoweringBug(msg));
            }
            Err(e) => return Err(e),
        }
    }

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

    // Extern declares for bodies outside the chunk.
    for (i, body) in ctx.all_bodies.iter().enumerate() {
        if !chunk_set.contains(&i) {
            let decl = extern_declare(body, ctx.tcx);
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
    // Emitted in every chunk that might reference them; `private` makes
    // each object file self-contained and unreferenced copies are
    // stripped by `opt`/the linker.
    let mut emitted_any_meta = false;
    for (symbol, blob) in sorted_rc_metas(ctx.tcx) {
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
        for (name, arity) in &cabi_handlers {
            let handler_idx = ctx.all_bodies.iter().position(|b| b.name == *name);
            let owns_handler = handler_idx.is_some_and(|i| chunk_set.contains(&i));
            if owns_handler {
                out.push_str(&render_cabi_handler_thunk(name, *arity));
            } else {
                let param_list = (0..*arity)
                    .map(|_| "ptr".to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(out, "declare <16 x i8> @\"{name}$cabi\"({param_list})").unwrap();
            }
            writeln!(out).unwrap();
        }
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

/// Core of the P2+P3 build path.
///
/// **Phase 1 (incremental - P3):** bodies are partitioned into N chunks
/// where N is capped by both `PARALLEL_MAX_THREADS` and a minimum
/// bodies-per-chunk threshold (10). The threshold keeps hot callees in
/// the same module as their callers so opt can inline across them; a
/// 3-body program like `spectralnorm` compiles as one module with full
/// inlining, while a 78-body program like `ironknight` splits into 8
/// chunks for parallel compilation. Each chunk's cache key mixes the
/// per-body MIR hashes for all bodies it covers.
///
/// **Phase 2 (rendering - serial):** cache-miss chunks are lowered to
/// LLVM IR via [`render_chunk_module`]. Each chunk gets one `.ll` with
/// all its bodies defined plus extern declares for bodies in other chunks.
/// Rendering is serial because [`Lowerer`] uses `Rc<RefCell<_>>` state
/// that is not `Send`; at ~microseconds per body the serial cost is
/// negligible compared to `opt`+`llc`.
///
/// **Phase 3 (compilation - parallel - P2):** one `opt`+`llc` process
/// pair per chunk, all N running concurrently. Process-launch overhead is
/// bounded to N invocations regardless of program size - for 78 bodies
/// on 8 threads this is 8 launches instead of 78.
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
    let cabi_handlers = collect_cabi_handlers(bodies);
    let body_cache_keys: Vec<String> = bodies
        .iter()
        .map(|body| {
            body_cache_key(
                body,
                &llvm_triple,
                profile,
                cabi_handlers.get(&body.name).copied(),
            )
        })
        .collect();

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

    // Partition all bodies into N chunks. Chunk assignment is deterministic
    // so the cache key is stable across builds with identical bodies.
    //
    // Bound fan-out by both available CPUs and the memory-aware LLVM job
    // policy. The bodies-per-chunk cap still preserves the inlining floor.
    //
    // The chunk count decides which bodies share a module, and therefore
    // the emitted code and its layout. Reproducible mode pins it to one
    // module so the artifact depends only on the source and the target,
    // never on the host's CPU count or a job-limit override.
    let n_chunks = if want_reproducible() {
        1
    } else {
        let available_threads = std::thread::available_parallelism()
            .map_or(PARALLEL_MAX_THREADS, std::num::NonZero::get);
        let max_threads = available_threads.min(codegen_job_limit(bodies.len()));
        let ideal_n_chunks = max_threads.min(bodies.len());
        ideal_n_chunks
            .min(bodies.len().div_ceil(MIN_BODIES_PER_CHUNK))
            .max(1)
    };
    let body_chunks = codegen_chunks(bodies, n_chunks);

    // ---------------------------------------------------------------
    // Phase 1 - chunk-level incremental cache check
    // ---------------------------------------------------------------
    let mut result_objects: Vec<(usize, PathBuf)> = Vec::new(); // (chunk_idx, path)
    // (chunk_idx, body_indices, cache_key, ll_path, obj_path)
    let mut chunks_to_compile: Vec<(usize, Vec<usize>, String, PathBuf, PathBuf)> = Vec::new();
    let fallback_bodies: Vec<String> = Vec::new();

    for (chunk_idx, body_indices) in body_chunks.into_iter().enumerate() {
        let obj_path = obj_dir.join(format!("chunk{chunk_idx}.o"));
        let ll_path = obj_dir.join(format!("chunk{chunk_idx}.ll"));

        let key = chunk_cache_key(&body_indices, &body_cache_keys);
        if let Some(hit) = cache_dir
            .as_ref()
            .map(|cd| cd.join(format!("{key}.o")))
            .filter(|p| p.exists())
        {
            if std::fs::copy(&hit, &obj_path).is_ok() {
                result_objects.push((chunk_idx, obj_path));
                continue;
            }
        }
        chunks_to_compile.push((chunk_idx, body_indices, key, ll_path, obj_path));
    }

    if chunks_to_compile.is_empty() {
        result_objects.sort_by_key(|(i, _)| *i);
        return Ok((
            result_objects.into_iter().map(|(_, p)| p).collect(),
            triple,
            fallback_bodies,
        ));
    }

    // ---------------------------------------------------------------
    // Phase 2 - render chunk .ll files (serial)
    // ---------------------------------------------------------------
    for (_, body_indices, _, ll_path, _) in &chunks_to_compile {
        let started = std::time::Instant::now();
        let ir = render_chunk_module(body_indices, &ctx).map_err(|e| match e {
            BuildError::InternalLoweringBug(msg) => {
                anyhow!("llvm backend internal lowering bug: {msg}")
            }
            BuildError::Tool(msg) => anyhow!("llvm backend: tool: {msg}"),
            BuildError::Io(err) => err,
        })?;
        std::fs::write(ll_path, ir.as_bytes())
            .with_context(|| format!("writing {}", ll_path.display()))?;
        record_phase(&RENDER_US, started);
    }

    // Stitch chunk files into unit.ll for tools / tests that expect
    // "llvm backend: IR at <path>" on stderr when GOS_LLVM_DUMP=1.
    if dump {
        let dump_path = obj_dir.join("unit.ll");
        if let Ok(mut f) = std::fs::File::create(&dump_path) {
            use std::io::Write as _;
            for (chunk_idx, _, _, ll_path, _) in &chunks_to_compile {
                if let Ok(text) = std::fs::read_to_string(ll_path) {
                    let _ = write!(f, "; === chunk{chunk_idx} ===\n{text}\n");
                }
            }
        }
        eprintln!("llvm backend: IR at {}", dump_path.display());
    }

    // ---------------------------------------------------------------
    // Phase 3 - parallel opt+llc (one process pair per chunk - P2)
    // ---------------------------------------------------------------
    let err_slot: parking_lot::Mutex<Option<anyhow::Error>> = parking_lot::Mutex::new(None);
    let compiled: parking_lot::Mutex<Vec<(usize, PathBuf)>> = parking_lot::Mutex::new(Vec::new());

    let err_ref = &err_slot;
    let compiled_ref = &compiled;
    let triple_ref: &str = &llvm_triple;
    let cache_ref = &cache_dir;

    std::thread::scope(|scope| {
        for (chunk_idx, _, cache_key, ll_path, obj_path) in &chunks_to_compile {
            let chunk_idx = *chunk_idx;
            let cache_key = cache_key.clone();
            let ll_path = ll_path.clone();
            let obj_path = obj_path.clone();
            scope.spawn(move || {
                if err_ref.lock().is_some() {
                    return;
                }
                match invoke_llc_pipeline(&ll_path, &obj_path, triple_ref, /*announce=*/ false) {
                    Ok(()) => {
                        if !dump {
                            let _ = std::fs::remove_file(&ll_path);
                        }
                        if let Some(cd) = cache_ref {
                            let _ = std::fs::copy(&obj_path, cd.join(format!("{cache_key}.o")));
                        }
                        compiled_ref.lock().push((chunk_idx, obj_path));
                    }
                    Err(e) => {
                        *err_ref.lock() = Some(e);
                    }
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
        fallback_bodies,
    ))
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

    for body in bodies {
        let mut lowerer = Lowerer::new(body, tcx);
        lowerer.fn_name_by_def.clone_from(&fn_name_by_def);
        lowerer.param_tys_by_name.clone_from(&param_tys_by_name);
        lowerer.strings = string_pool.clone();
        lowerer.capture_summary = capture_summary.clone();
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

/// Process-wide flag toggled by [`set_debug_info`] so the CLI can
/// request DWARF emission without going through an env var (which
/// would require `unsafe` to set on stable Rust 2024).
static DEBUG_INFO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Unit name and line-start offsets registered by [`set_source_lines`].
static SOURCE_LINES: std::sync::RwLock<Option<(String, Vec<u32>)>> = std::sync::RwLock::new(None);

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
/// [`compile_to_object`] / [`compile_with_fallback`] calls.
/// Called by the `gos build --release -g` flag.
pub fn set_debug_info(enabled: bool) {
    DEBUG_INFO.store(enabled, std::sync::atomic::Ordering::Release);
}

/// Records where each source file's lines begin, so the backend can turn a
/// MIR span's byte offset into the line a panic report names. The source map
/// itself does not survive the frontend, so the driver hands over this
/// compact form before codegen runs.
pub fn set_source_lines(unit: &str, line_starts: Vec<u32>) {
    let mut slot = SOURCE_LINES
        .write()
        .expect("source-line table lock poisoned");
    *slot = Some((unit.to_string(), line_starts));
}

/// The registered file name and the one-based line for `offset`, or `None`
/// when no table has been registered for this build.
pub(crate) fn source_position(offset: u32) -> Option<(String, u32)> {
    let slot = SOURCE_LINES
        .read()
        .expect("source-line table lock poisoned");
    let (unit, starts) = slot.as_ref()?;
    // `partition_point` gives the count of line starts at or before the
    // offset, which is exactly the one-based line number.
    let line = starts.partition_point(|start| *start <= offset);
    Some((unit.clone(), line.max(1) as u32))
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
fn want_reproducible() -> bool {
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
    /// Debug: use LLVM's `O1` scalar and loop pipeline without discretionary
    /// inlining, followed by the `O0` instruction selector. Checked arithmetic
    /// remains enabled.
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
/// This affects a narrowly scoped LLVM workaround for tiny-copy loops. It is
/// part of the object-cache fingerprint, so a dynamic object can never be
/// reused for a static-musl link or vice versa.
pub fn set_static_musl_link(enabled: bool) {
    STATIC_MUSL_LINK.store(enabled, std::sync::atomic::Ordering::Release);
}

fn static_musl_link_enabled() -> bool {
    STATIC_MUSL_LINK.load(std::sync::atomic::Ordering::Acquire)
}

fn disable_loop_idiom_for_target_with_static_musl(static_musl: bool, triple: &str) -> bool {
    static_musl || triple.contains("-unknown-linux-musl")
}

fn disable_loop_idiom_for_target(triple: &str) -> bool {
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
fn want_dwarf() -> bool {
    DEBUG_INFO.load(std::sync::atomic::Ordering::Acquire)
        || std::env::var("GOS_DWARF").is_ok()
        || std::env::var("GOS_BUILD_DEBUG").is_ok()
}

/// Emits LLVM debug-info metadata for every body in `bodies`.
/// Produces:
///
/// - `llvm.module.flags` declaring DWARF v4 and Debug Info v3.
/// - One `DICompileUnit` for the program, owning a single
///   synthetic `DIFile` (the source map is not yet plumbed
///   through to the lowerer; per-function file resolution is a
///   follow-up).
/// - One `DISubprogram` per body, attached to the function's
///   `define` line via `!dbg !N`. The subprogram metadata is what
///   `gdb` / `lldb` use to walk a backtrace and resolve
///   instruction pointers to function names.
fn emit_dwarf_metadata(out: &mut String, bodies: &[Body]) {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| ".".to_string());
    // 1. Tag the function definitions with `!dbg !N`. The
    //    subprogram numbers start at 100; the file is !50, the
    //    compile unit is !51.
    let mut subprogram_lines: Vec<String> = Vec::new();
    for (idx, body) in bodies.iter().enumerate() {
        let llvm_name = crate::lower::mangle_fn_name(&body.name);
        let id = 100u32 + u32::try_from(idx).unwrap_or(u32::MAX);
        // Best-effort: stamp every function with the body name and
        // a stable scopeLine of 1. Real source line numbers will
        // arrive once the SourceMap is threaded through the
        // codegen pipeline.
        subprogram_lines.push(format!(
            "!{id} = distinct !DISubprogram(name: \"{name}\", linkageName: \"{lname}\", \
             scope: !51, file: !50, line: 1, type: !52, scopeLine: 1, \
             spFlags: DISPFlagDefinition, unit: !51)",
            id = id,
            name = body.name.replace('"', "\\\""),
            lname = llvm_name.replace('"', "\\\""),
        ));
        // Attach `!dbg` to the define line.
        let needle = format!("define i64 @\"{llvm_name}\"");
        let attached = format!("define i64 @\"{llvm_name}\"");
        if let Some(pos) = out.find(&needle) {
            // Scan forward to the opening brace and insert `!dbg !N`
            // just before it.
            if let Some(brace) = out[pos..].find(" {\n") {
                let abs = pos + brace;
                let insertion = format!(" !dbg !{id}");
                out.insert_str(abs, &insertion);
                continue;
            }
            let _ = attached;
        }
        // Same scan for the `void`-returning shape.
        let needle_void = format!("define void @\"{llvm_name}\"");
        if let Some(pos) = out.find(&needle_void) {
            if let Some(brace) = out[pos..].find(" {\n") {
                let abs = pos + brace;
                let insertion = format!(" !dbg !{id}");
                out.insert_str(abs, &insertion);
            }
        }
    }
    writeln!(out).unwrap();
    writeln!(out, "!llvm.module.flags = !{{!40, !41}}").unwrap();
    writeln!(out, "!llvm.dbg.cu = !{{!51}}").unwrap();
    writeln!(out, "!40 = !{{i32 7, !\"Dwarf Version\", i32 4}}").unwrap();
    writeln!(out, "!41 = !{{i32 2, !\"Debug Info Version\", i32 3}}").unwrap();
    writeln!(
        out,
        "!50 = !DIFile(filename: \"main.gos\", directory: \"{dir}\")",
        dir = cwd.replace('"', "\\\""),
    )
    .unwrap();
    writeln!(
        out,
        "!51 = distinct !DICompileUnit(language: DW_LANG_C99, file: !50, \
         producer: \"gossamer 0.0.0\", isOptimized: true, runtimeVersion: 0, \
         emissionKind: FullDebug)"
    )
    .unwrap();
    writeln!(out, "!52 = !DISubroutineType(types: !{{}})").unwrap();
    for line in subprogram_lines {
        writeln!(out, "{line}").unwrap();
    }
}

fn audit_llvm_ir_symbols(ll_path: &std::path::Path) -> Result<()> {
    let ir = std::fs::read_to_string(ll_path)
        .with_context(|| format!("reading {}", ll_path.display()))?;
    audit_llvm_ir_symbols_text(&ir)
        .with_context(|| format!("auditing LLVM IR symbols in {}", ll_path.display()))
}

fn audit_llvm_ir_symbols_text(ir: &str) -> Result<()> {
    let mut defined = std::collections::HashSet::new();
    for line in ir.lines() {
        let trimmed = line.trim_start();
        if (trimmed.starts_with("define ") || trimmed.starts_with("declare "))
            && let Some(symbol) = first_llvm_symbol(trimmed)
        {
            defined.insert(symbol);
        } else if trimmed.starts_with('@')
            && trimmed.contains(" = ")
            && let Some(symbol) = first_llvm_symbol(trimmed)
        {
            defined.insert(symbol);
        }
    }

    let mut missing: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let mut current_fn: Option<String> = None;
    for line in ir.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("define ") {
            current_fn = first_llvm_symbol(trimmed);
        } else if trimmed == "}" {
            current_fn = None;
        }
        for symbol in llvm_symbols_in_line(line) {
            if symbol.starts_with("llvm.") || defined.contains(&symbol) {
                continue;
            }
            let scope = current_fn.clone().unwrap_or_else(|| "<module>".to_string());
            missing.entry(symbol).or_default().insert(scope);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let summary = missing
        .into_iter()
        .map(|(symbol, scopes)| {
            let scopes = scopes.into_iter().collect::<Vec<_>>().join(", ");
            format!("@{symbol} referenced from {scopes}")
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(anyhow!(
        "llvm backend internal lowering bug: undefined symbols before LLVM tools: {summary}"
    ))
}

fn first_llvm_symbol(line: &str) -> Option<String> {
    llvm_symbols_in_line(line).into_iter().next()
}

fn llvm_symbols_in_line(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut symbols = Vec::new();
    let mut i = 0usize;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if !in_string && b == b';' {
            break;
        }
        if !in_string && b == b'@' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                let mut j = i + 2;
                let mut out = String::new();
                while j < bytes.len() {
                    if bytes[j] == b'"' && bytes[j - 1] != b'\\' {
                        break;
                    }
                    out.push(bytes[j] as char);
                    j += 1;
                }
                if !out.is_empty() {
                    symbols.push(out);
                }
                i = j.saturating_add(1);
                continue;
            }
            let mut j = i + 1;
            while j < bytes.len() && is_llvm_symbol_byte(bytes[j]) {
                j += 1;
            }
            if j > i + 1 {
                symbols.push(line[i + 1..j].to_string());
            }
            i = j;
            continue;
        }
        i += 1;
    }
    symbols
}

fn is_llvm_symbol_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'$' | b'-')
}

/// Renders an `extern declare` for a body outside the current
/// chunk. The signature must match what its defining LLVM chunk
/// emits so the linker can hook them up.
/// Verifies a single module-level global declaration string has
/// the structural shape LLVM IR expects. We don't parse the full
/// grammar - we only check the prefix tokens an entry must lead
/// with. The check is cheap (string scan, no allocation) and
/// catches the realistic regression mode: a *bare* identifier
/// (e.g. `"my_const"` instead of `"@my_const = constant ..."`)
/// being inserted via `runtime_refs.insert(...)`. That class of
/// bug previously corrupted the IR module silently and forced
/// `llc` to error which then triggered the per-fn Cranelift
/// fallback for unrelated bodies.
/// Walks `body`'s MIR statements + terminators looking for
/// `gos_fn_addr("__fn_thunk_*")` references. The names matter
/// because each unique shape needs a synthesised LLVM thunk;
/// see [`render_shape_thunk`].
fn collect_thunk_names_in_body(body: &Body, out: &mut std::collections::BTreeSet<String>) {
    use gossamer_mir::{ConstValue, Operand, Rvalue, StatementKind, Terminator};
    let mut visit_args = |args: &[Operand], name: &str| {
        if name == "gos_fn_addr"
            && let Some(Operand::Const(ConstValue::Str(s))) = args.first()
            && s.starts_with("__fn_thunk_")
        {
            out.insert(s.clone());
        }
    };
    for block in &body.blocks {
        for stmt in &block.stmts {
            if let StatementKind::Assign { rvalue, .. } = &stmt.kind
                && let Rvalue::CallIntrinsic { name, args } = rvalue
            {
                visit_args(args, name);
            }
        }
        if let Terminator::Call { callee, args, .. } = &block.terminator
            && let Operand::Const(ConstValue::Str(name)) = callee
        {
            visit_args(args, name);
        }
    }
}

/// Runtime shims whose closure callback returns the 2-word `i128`
/// Option/Result, from the shared ABI table so the Cranelift backend and
/// this one agree on which callbacks cross that boundary.
const CABI_I128_COMBINATORS: &[&str] = gossamer_abi::I128_CALLBACK_SHIMS;

/// `spawn(f)`'s shim. Its callable crosses as `i128` only when it answers
/// a two-word `Result` / `Option`; the runtime reads the `ret_words`
/// argument (index 2) to decide, and calls a one-word callable as
/// `-> i64`, which already agrees on the GP register.
pub(crate) const CABI_SPAWN_SHIM: &str = "gos_rt_spawn_ex";
pub(crate) const CABI_SPAWN_RET_WORDS_ARG: usize = 2;
pub(crate) const SPAWN_RET_TWO_WORDS: i128 = 2;

/// Win64 entry the runtime is handed in place of a two-word spawn
/// callable's own address: it forwards to the callable (which returns the
/// `i128` in the GP-register pair) and re-emits the value as `<16 x i8>`,
/// the vector register rustc's `extern "C" fn(..) -> i128` reads.
pub(crate) const SPAWN_WIDE_CABI_THUNK: &str = "__gos_spawn_wide$cabi";

/// True when `body` spawns a callable that answers a two-word value.
fn body_has_wide_spawn(body: &Body) -> bool {
    use gossamer_mir::{ConstValue, Operand, Terminator};
    body.blocks.iter().any(|block| {
        let Terminator::Call { callee, args, .. } = &block.terminator else {
            return false;
        };
        matches!(callee, Operand::Const(ConstValue::Str(sym)) if sym == CABI_SPAWN_SHIM)
            && args
                .get(CABI_SPAWN_RET_WORDS_ARG)
                .and_then(|w| operand_const_int(body, w))
                == Some(SPAWN_RET_TWO_WORDS)
    })
}

/// Renders [`SPAWN_WIDE_CABI_THUNK`]. The spawn lowering hands the runtime
/// the env blob and the address at its slot 0; this thunk takes that
/// argument pair's place, so it reads slot 0 itself and calls it the way
/// gossamer code does.
fn render_spawn_wide_cabi_thunk() -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "define linkonce_odr <16 x i8> @\"{SPAWN_WIDE_CABI_THUNK}\"(ptr %env) {{"
    );
    writeln!(out, "entry:").unwrap();
    writeln!(out, "  %fn_ptr = load ptr, ptr %env").unwrap();
    writeln!(out, "  %r = call i128 %fn_ptr(ptr %env)").unwrap();
    writeln!(out, "  %v = bitcast i128 %r to <16 x i8>").unwrap();
    writeln!(out, "  ret <16 x i8> %v").unwrap();
    writeln!(out, "}}").unwrap();
    out
}

/// Resolves a fn-address local to the non-runtime function name its defining
/// `gos_fn_addr("name")` references, within `body`. The lowering assigns the
/// address directly, so a single pass over the body's statements suffices.
fn resolve_fn_addr_name(body: &Body, target: gossamer_mir::Local) -> Option<String> {
    use gossamer_mir::{ConstValue, Operand, Rvalue, StatementKind};
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { place, rvalue } = &stmt.kind else {
                continue;
            };
            if place.local != target || !place.projection.is_empty() {
                continue;
            }
            let Rvalue::CallIntrinsic { name, args } = rvalue else {
                continue;
            };
            if *name != "gos_fn_addr" {
                continue;
            }
            // `__fn_thunk_*` shape thunks are linkonce-synthesized, not MIR
            // bodies, and are shared across every call site of their shape -
            // a name-based `$cabi` redirect would both reference an undefined
            // symbol and corrupt unrelated (gossamer-invoked) uses of the same
            // shape. They are excluded here; a bare-fn / non-capturing-closure
            // callback that lowers through a shape thunk is not rewired.
            if let Some(Operand::Const(ConstValue::Str(hname))) = args.first()
                && !hname.starts_with("gos_rt_")
                && !hname.starts_with("__fn_thunk_")
            {
                return Some(hname.clone());
            }
        }
    }
    None
}

/// The integer literal `op` names, either directly or through a local bound
/// to `Use(Const(Int(n)))` (a lowering that needs the value in a local writes
/// it as a separate `let n = <literal>` statement first).
pub(crate) fn operand_const_int(body: &Body, op: &gossamer_mir::Operand) -> Option<i128> {
    use gossamer_mir::{ConstValue, Operand, Rvalue, StatementKind};
    match op {
        Operand::Const(ConstValue::Int(n)) => Some(*n),
        Operand::Copy(p) if p.projection.is_empty() => {
            for block in &body.blocks {
                for stmt in &block.stmts {
                    if let StatementKind::Assign { place, rvalue } = &stmt.kind
                        && place.local == p.local
                        && place.projection.is_empty()
                        && let Rvalue::Use(Operand::Const(ConstValue::Int(n))) = rvalue
                    {
                        return Some(*n);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// True when `op` is the integer literal 0 (the closure-env builder writes
/// the callable offset as a separate `let zero = 0` local before the
/// `gos_store`).
fn operand_is_zero_offset(body: &Body, op: &gossamer_mir::Operand) -> bool {
    operand_const_int(body, op) == Some(0)
}

/// For a closure env-blob local, resolves the callable stored at offset 0 -
/// the `gos_store(env, 0, gos_fn_addr("name"))` the lowering emits when it
/// builds the env. Returns the referenced non-runtime function name.
fn resolve_env_slot0_fn(body: &Body, env_local: gossamer_mir::Local) -> Option<String> {
    use gossamer_mir::{Operand, Rvalue, StatementKind};
    for block in &body.blocks {
        for stmt in &block.stmts {
            let StatementKind::Assign { rvalue, .. } = &stmt.kind else {
                continue;
            };
            let Rvalue::CallIntrinsic { name, args } = rvalue else {
                continue;
            };
            if *name != "gos_store" {
                continue;
            }
            let [Operand::Copy(env), off, Operand::Copy(fn_addr)] = args.as_slice() else {
                continue;
            };
            if env.local != env_local || !env.projection.is_empty() {
                continue;
            }
            if !operand_is_zero_offset(body, off) {
                continue;
            }
            if let Some(hname) = resolve_fn_addr_name(body, fn_addr.local) {
                return Some(hname);
            }
        }
    }
    None
}

/// Runtime registration shims that store a gossamer handler's
/// `gos_fn_addr` and later invoke it as `extern "C" fn(..) -> i128`,
/// mapped to the fn-addr argument's position in the shim's signature.
/// Every stored callback crosses the rustc/LLVM i128-return boundary,
/// so on Win64 it must be registered through its `<16 x i8>` `$cabi`
/// thunk. On every target the collected names also identify functions
/// entered directly from the Rust runtime, whose opaque request params
/// arrive as raw pointers.
const CABI_HANDLER_REGISTRATIONS: &[(&str, usize)] = gossamer_abi::I128_HANDLER_REGISTRATIONS;

/// Collects the gossamer functions invoked by the rustc-compiled runtime
/// through `extern "C" fn(..) -> i128`, mapped to their parameter arity:
/// handler registrations (the [`CABI_HANDLER_REGISTRATIONS`] table, keyed
/// by the fn-addr argument position) and the closure callbacks of the
/// i128-returning std combinators (whose address sits at offset 0 of the env
/// blob passed to the helper). The Win64 ABI returns the 2-word `i128` in xmm0,
/// but a gossamer `define i128`/`ret i128` returns it in the GP-register pair,
/// so each collected function needs a `<16 x i8>` return thunk taken in place
/// of its raw address on that target.
fn collect_cabi_handlers(all_bodies: &[Body]) -> std::collections::BTreeMap<String, usize> {
    use gossamer_mir::{ConstValue, Operand, Terminator};
    let mut handlers = std::collections::BTreeMap::new();
    let arity_of = |name: &str| -> usize {
        all_bodies
            .iter()
            .find(|b| b.name == name)
            .map_or(2, |b| b.arity as usize)
    };
    for body in all_bodies {
        for block in &body.blocks {
            let Terminator::Call { callee, args, .. } = &block.terminator else {
                continue;
            };
            let Operand::Const(ConstValue::Str(sym)) = callee else {
                continue;
            };
            if let Some((_, addr_idx)) = CABI_HANDLER_REGISTRATIONS
                .iter()
                .find(|(shim, _)| *shim == sym.as_str())
            {
                if let Some(Operand::Copy(addr_place)) = args.get(*addr_idx)
                    && let Some(hname) = resolve_fn_addr_name(body, addr_place.local)
                {
                    let arity = arity_of(&hname);
                    handlers.insert(hname, arity);
                }
            } else if CABI_I128_COMBINATORS.contains(&sym.as_str()) {
                for arg in args {
                    let Operand::Copy(env_place) = arg else {
                        continue;
                    };
                    if let Some(hname) = resolve_env_slot0_fn(body, env_place.local) {
                        let arity = arity_of(&hname);
                        handlers.insert(hname, arity);
                    }
                }
            }
        }
    }
    handlers
}

/// Renders the Win64 handler-return thunk `define <16 x i8> @"name$cabi"` -
/// it forwards every (pointer) argument to the real handler `@"name"`
/// (which returns the 2-word `i128` in the GP-register pair) and re-emits
/// the value as `<16 x i8>` so the rustc runtime reads it from xmm0.
/// Emitted in exactly the one chunk that owns the handler body (never duplicated).
fn render_cabi_handler_thunk(name: &str, arity: usize) -> String {
    let params: Vec<String> = (0..arity).map(|i| format!("ptr %a{i}")).collect();
    let call_args: Vec<String> = (0..arity).map(|i| format!("ptr %a{i}")).collect();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "define <16 x i8> @\"{name}$cabi\"({}) {{",
        params.join(", ")
    );
    writeln!(out, "entry:").unwrap();
    let _ = writeln!(
        out,
        "  %r = call i128 @\"{name}\"({})",
        call_args.join(", ")
    );
    writeln!(out, "  %v = bitcast i128 %r to <16 x i8>").unwrap();
    writeln!(out, "  ret <16 x i8> %v").unwrap();
    writeln!(out, "}}").unwrap();
    out
}

/// Synthesises an LLVM `define` for a per-shape callable thunk
/// named `__fn_thunk_<inputs>_<ret>`. The thunk loads the real
/// fn pointer from `env+8` and forwards the typed arguments
/// with the matching calling convention. Mirrors the Cranelift
/// backend's `define_shape_thunk` so capturing closures and
/// fn-item refs flow through identical lowering.
fn render_shape_thunk(name: &str) -> Option<String> {
    render_shape_thunk_with_linkage(name, "")
}

/// Body shared by both shape-thunk renderers; `linkage` is the empty string
/// for a plain `define` and `"linkonce_odr "` for the per-body modules.
fn render_shape_thunk_with_linkage(name: &str, linkage: &str) -> Option<String> {
    let suffix = name.strip_prefix("__fn_thunk_")?;
    let (inputs_str, ret_str) = suffix.rsplit_once('_')?;
    let ret_char = ret_str.chars().next()?;
    let ret_ty = shape_char_to_llvm_ty(ret_char)?;
    let mut input_tys: Vec<&'static str> = Vec::with_capacity(inputs_str.len());
    for c in inputs_str.chars() {
        input_tys.push(shape_char_to_llvm_ty(c)?);
    }
    let unit_ret = ret_char == 'u';
    let mut out = String::new();
    let header_ret = if unit_ret { "void" } else { ret_ty };
    let mut params = String::from("ptr %env");
    for (i, t) in input_tys.iter().enumerate() {
        let _ = write!(params, ", {t} %a{i}");
    }
    let _ = writeln!(out, "define {linkage}{header_ret} @\"{name}\"({params}) {{");
    writeln!(out, "entry:").unwrap();
    writeln!(out, "  %fn_ptr_addr = getelementptr i8, ptr %env, i64 8").unwrap();
    writeln!(out, "  %fn_ptr = load ptr, ptr %fn_ptr_addr").unwrap();
    // A `bool` crosses the ABI as a whole byte holding the canonical `0` or
    // `1` that callers store and compare against, while the body LLVM defines
    // takes and returns the `i1` its branches are built on. The thunk is where
    // the two meet: narrow on the way in, zero-extend on the way out.
    let mut call_args = String::new();
    for (i, (t, c)) in input_tys.iter().zip(inputs_str.chars()).enumerate() {
        if i > 0 {
            call_args.push_str(", ");
        }
        if c == 'b' {
            let _ = writeln!(out, "  %b{i} = trunc i8 %a{i} to i1");
            let _ = write!(call_args, "i1 %b{i}");
        } else if c == 'q' {
            // The caller hands the address of a two-word carrier's storage;
            // the callee's parameter is the carrier itself.
            let _ = writeln!(out, "  %c{i} = load i128, ptr %a{i}");
            let _ = write!(call_args, "i128 %c{i}");
        } else {
            let _ = write!(call_args, "{t} %a{i}");
        }
    }
    if unit_ret {
        let _ = writeln!(out, "  call void %fn_ptr({call_args})");
        writeln!(out, "  ret void").unwrap();
    } else if ret_char == 'b' {
        let _ = writeln!(out, "  %r = call i1 %fn_ptr({call_args})");
        let _ = writeln!(out, "  %rb = zext i1 %r to i8");
        let _ = writeln!(out, "  ret i8 %rb");
    } else {
        let _ = writeln!(out, "  %r = call {ret_ty} %fn_ptr({call_args})");
        let _ = writeln!(out, "  ret {ret_ty} %r");
    }
    writeln!(out, "}}").unwrap();
    Some(out)
}

/// Maps a shape character produced by
/// `gossamer_mir::mangle_callable_shape` to its LLVM IR type
/// name. Mirrors `shape_char_to_cl_type` on the Cranelift side.
fn shape_char_to_llvm_ty(c: char) -> Option<&'static str> {
    Some(match c {
        'q' => "ptr",
        'b' | 'y' => "i8",
        'k' => "i16",
        'c' | 'j' => "i32",
        'i' => "i64",
        'f' => "double",
        'g' => "float",
        'u' => "i64",
        // 2-word packed Result/Option.
        'r' => "i128",
        _ => return None,
    })
}

fn validate_global_decl_shape(g: &str) -> Result<()> {
    let trimmed = g.trim_start();
    let valid = trimmed.starts_with('@') || trimmed.starts_with("declare ");
    if !valid {
        return Err(anyhow!(
            "llvm backend: malformed module-level entry (expected `@symbol = ...` or \
             `declare ...`, got: {snippet:?}). This is the same shape regression that \
             caused the 2026-04-28 / 2026-04-30 silent Cranelift-fallback incidents.",
            snippet = if trimmed.len() > 80 {
                &trimmed[..80]
            } else {
                trimmed
            }
        ));
    }
    Ok(())
}

fn extern_declare(body: &Body, tcx: &TyCtxt) -> String {
    let ret_ty = crate::ty::render_ty(tcx, body.local_ty(gossamer_mir::Local::RETURN));
    let mut params = String::new();
    for i in 0..body.arity {
        if i > 0 {
            params.push_str(", ");
        }
        let local = gossamer_mir::Local(i + 1);
        let p_ty = crate::ty::param_llvm_ty(tcx, body.local_ty(local));
        let _ = write!(params, "{p_ty}");
    }
    format!(
        "declare {ret_ty} @\"{name}\"({params})\n",
        name = crate::lower::mangle_fn_name(&body.name)
    )
}

/// Returns the temp directory the LLVM pipeline emits its
/// intermediate IR / opt-bitcode artifacts into.
///
/// the reproducible-mode name was a fixed
/// `gos-llvm-reproducible`, so parallel reproducible builds (two
/// `gos build --reproducible` of distinct projects on the same
/// host) raced on `unit.ll` / `unit.opt.bc` / `unit.o`. We now
/// keep the deterministic-prefix invariant the reproducible mode
/// needs (same input → same path → same artifact bytes) by
/// hashing the entry source path into the directory name. Two
/// builds of the same source still land in the same dir; two
/// concurrent builds of different sources get distinct dirs.
fn pipeline_tmp_dir() -> Result<PathBuf> {
    use std::hash::Hasher as _;
    let tmp_dir = if want_reproducible() {
        // Hash the CWD + program path so parallel reproducible
        // builds of different inputs don't collide on a fixed
        // directory name. Same input → same hash → same dir →
        // bit-identical artifacts across two builds.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        if let Ok(cwd) = std::env::current_dir() {
            hasher.write(cwd.as_os_str().as_encoded_bytes());
        }
        if let Some(arg0) = std::env::args_os().next() {
            hasher.write(arg0.as_encoded_bytes());
        }
        let h = hasher.finish();
        std::env::temp_dir().join(format!("gos-llvm-repro-{h:016x}"))
    } else {
        // Per-pid + per-call counter so concurrent
        // `render_ir_to_string` / `compile_to_object` calls inside
        // the same process don't trample each other's `unit.ll` /
        // `unit.o`. Two parallel tests in the same `cargo test`
        // process used to share a single tmp dir and produce
        // mutually-corrupted IR.
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        std::env::temp_dir().join(format!("gos-llvm-{}-{seq}", std::process::id()))
    };
    std::fs::create_dir_all(&tmp_dir).with_context(|| format!("creating {}", tmp_dir.display()))?;
    Ok(tmp_dir)
}

/// Path-only variant of the historical `invoke_llc(ir_str, triple)
/// -> Vec<u8>`. Reads the IR from `ll_path` (already on disk) and
/// writes the resulting object directly to `obj_out`. The previous
/// API forced callers to round-trip the IR + the object through
/// memory; this one keeps both on disk and returns nothing.
///
/// Pipeline order: explicit IR verification via `opt -passes=verify`
/// → mid-end optimisation (`opt -O1`/`-O3`) → backend (`llc`). The
/// verify pass runs first so shape regressions surface with source-
/// level context (the verifier's stderr is forwarded verbatim)
/// before any optimisation rewrites obscure the offending value.
/// When `announce` is true and `GOS_LLVM_DUMP` is set, emits
/// `llvm backend: IR at <ll_path>` so callers / test harnesses can
/// locate the IR file. Pass `false` in the parallel per-body path
/// where the caller announces the concatenated dump instead.
fn invoke_llc_pipeline(
    ll_path: &std::path::Path,
    obj_out: &std::path::Path,
    triple: &str,
    announce: bool,
) -> Result<()> {
    let started = std::time::Instant::now();
    let keep_artifacts = std::env::var("GOS_LLVM_DUMP").is_ok();
    if keep_artifacts && announce {
        eprintln!("llvm backend: IR at {}", ll_path.display());
    }
    audit_llvm_ir_symbols(ll_path)?;
    let _guard = scopeguard_tool_time(started);
    let profile = opt_profile();
    let mcpu = mcpu_target(triple);
    if let Some(clang) = integrated_clang_path(triple) {
        if std::env::var_os("GOS_PIPELINE_TRACE").is_some() {
            eprintln!("llvm pipeline: clang at {}", clang.display());
        }
        return invoke_clang_pipeline(&clang, ll_path, obj_out, triple, profile, &mcpu);
    }
    if std::env::var_os("GOS_PIPELINE_TRACE").is_some() {
        eprintln!("llvm pipeline: opt + llc");
    }

    let opt_path = ll_path.with_extension("opt.bc");
    // Both profiles run `opt` because the lowerer emits some
    // non-canonical shapes (e.g. integer-typed constants in
    // floating-point store positions) that `opt`'s
    // instcombine + verifier passes fix up. Skipping `opt`
    // entirely sends those shapes straight to `llc`, which
    // rejects them.
    //
    // Debug runs the smallest pipeline that makes the emitter's output
    // usable: `sroa` promotes the alloca per local that every lowered body
    // starts with, and `early-cse` / `instcombine` / `simplifycfg` clean up
    // after it. A full `default<O1>` costs several times as much for a
    // binary that is already an order of magnitude behind `--release`, which
    // is the wrong side of the trade for the profile people iterate on.
    // Instcombine is asked not to verify its fixpoint because a pipeline of
    // this length gives it one iteration, where the check expects the
    // repeats a longer pipeline would run.
    //
    // Release profile uses `default<O3>` for full optimisation.
    //
    // The IR verifier runs first in both pipelines (as the
    // initial `verify` entry) so malformed IR surfaces with
    // source-level context before any optimisation rewrites
    // obscure the offending value.
    let (opt_passes, llc_level) = match profile {
        OptProfile::Debug => (
            "verify,sroa,early-cse,instcombine<no-verify-fixpoint>,simplifycfg",
            "-O0",
        ),
        OptProfile::Release => ("verify,default<O3>", "-O3"),
    };
    let opt_tool = find_opt()?;
    let mut opt_cmd = std::process::Command::new(&opt_tool);
    opt_cmd
        .arg(format!("-passes={opt_passes}"))
        .arg(format!("-mtriple={triple}"))
        // Match `rustc -C target-cpu=native`: tell the
        // mid-level optimiser the target's feature set so the
        // loop / SLP vectorisers can emit AVX2 / FMA when the
        // host supports them. Without this, `opt` only knows
        // the baseline triple's features.
        //
        // `GOS_LLVM_MCPU` overrides - `x86-64-v3` is the
        // documented escape hatch when the host's AVX-512
        // entry/exit transition penalty hurts short-running
        // benchmarks (the §5 release-perf investigation
        // found this on fannkuch).
        .arg(format!("-mcpu={mcpu}"))
        // `+prefer-256-bit` is an x86 AVX-512 feature flag, so only pass it
        // for x86_64 targets. Keeping the width capped avoids ZMM transition
        // costs around runtime calls.
        .args(if target_arch_from_triple(triple) == "x86_64" {
            &["-mattr=+prefer-256-bit"][..]
        } else {
            &[][..]
        })
        // Block `LoopIdiomRecognize` from rewriting trivial
        // copy / shift loops into `llvm.memcpy` / `llvm.memmove`
        // calls. Only relevant on release (debug uses a minimal
        // pass set that doesn't run this recogniser). On release,
        // the PLT call overhead around musl's `memcpy` dwarfs the
        // copy work on small n. Leaving idiom-recognise off keeps
        // the inline-loop shape that beats Cranelift on short-trip
        // benchmarks. The narrower `disable-memcpy-idiom` flag
        // no longer takes effect under LLVM 18's new pass manager.
        ;
    if matches!(profile, OptProfile::Release) && disable_loop_idiom_for_target(triple) {
        opt_cmd.arg("--disable-loop-idiom-all");
    }
    // PGO instrumentation builds an instrumented binary that emits raw
    // profile data when the program exits. Link with
    // `libclang_rt.profile-x86_64.a` (handled in
    // `gossamer-cli/src/cmd/build.rs`), then merge the resulting `.profraw`
    // with `llvm-profdata merge -output=...`. The environment variables are
    // retained only for embedding callers that predate the CLI options.
    let selected_pgo = pgo_mode();
    let pgo_collect = match selected_pgo.as_ref() {
        Some(PgoMode::Collect(path)) => Some(path.display().to_string()),
        Some(PgoMode::Profile(_)) => None,
        None => std::env::var("GOS_PGO_COLLECT").ok(),
    };
    if let Some(profraw) = pgo_collect {
        opt_cmd
            .arg("--pgo-kind=pgo-instr-gen-pipeline")
            .arg(format!("--pgo-test-profile-file={profraw}"));
    }
    // PGO optimisation feeds a previously collected and merged profile into
    // the `opt` mid-end so branch weights, inlining thresholds, and the loop
    // / SLP vectorisers are guided by real execution frequencies. A selected
    // CLI mode wins over the legacy environment to keep the two modes
    // mutually exclusive.
    let pgo_profile = match selected_pgo.as_ref() {
        Some(PgoMode::Collect(_)) => None,
        Some(PgoMode::Profile(path)) => Some(path.display().to_string()),
        None => std::env::var("GOS_PGO_PROFILE").ok(),
    };
    if let Some(profdata) = pgo_profile {
        opt_cmd
            .arg("--pgo-kind=pgo-instr-use-pipeline")
            .arg(format!("--profile-file={profdata}"));
    }
    opt_cmd.arg(ll_path).arg("-o").arg(&opt_path);
    let opt_output = run_with_timeout(opt_cmd, opt_timeout(), "opt")
        .with_context(|| format!("spawn {}", opt_tool.display()))?;
    if !opt_output.status.success() {
        if keep_artifacts {
            eprintln!("llvm backend: failing IR kept at {}", ll_path.display());
        }
        return Err(anyhow!(
            "opt failed ({status}): {stderr}\n\
             hint: if the error begins with 'Broken module' it is an IR \
             shape regression in the lowerer (dump with GOS_LLVM_DUMP=1); \
             otherwise it is an opt mid-end blowup - largest IR usually \
             drives those, inspect the function names in the IR.",
            status = opt_output.status,
            stderr = String::from_utf8_lossy(&opt_output.stderr)
        ));
    }
    // Backend: `llc -O3` → object file with PIC relocations
    // (matches the rest of the build pipeline; the linker
    // refuses non-PIC objects for default PIE binaries).
    // `-mcpu=native` lets LLVM target the host's full
    // instruction set (AVX2 / FMA / etc. on modern Ryzen) -
    // matches what `rustc -C target-cpu=native` does for the
    // bench-game references.
    let llc = find_llc()?;
    let mut llc_cmd = std::process::Command::new(&llc);
    llc_cmd
        .arg(llc_level)
        .arg("-filetype=obj")
        .arg(format!("-mtriple={triple}"))
        // COFF (Windows) has no GOT: `-relocation-model=pic` makes llc
        // emit GOT-relative relocations for external data symbols that
        // rust-lld's link flavour cannot resolve, so every `gos build`
        // fails at link time. ELF (PIE default) and Mach-O (PIC-only)
        // both require pic. Mirrors the Cranelift `is_pic` guard in
        // native/compile.rs.
        .args(if triple.contains("windows") {
            &[][..]
        } else {
            &["-relocation-model=pic"][..]
        })
        .arg(format!("-mcpu={mcpu}"))
        // Match the mid-end vector-width policy during late code generation.
        .args(if target_arch_from_triple(triple) == "x86_64" {
            &["-mattr=+prefer-256-bit"][..]
        } else {
            &[][..]
        })
        .arg(&opt_path)
        .arg("-o")
        .arg(obj_out);
    // Pin DWARF version to match what the module metadata declared
    // (`!{i32 7, "Dwarf Version", i32 4}`). `llc` may otherwise pick
    // a newer default if the host LLVM is bumped, producing object
    // files that older debuggers can't read.
    if want_dwarf() {
        llc_cmd.arg("-dwarf-version=4");
    }
    let output = run_with_timeout(llc_cmd, opt_timeout(), "llc")
        .with_context(|| format!("spawn {}", llc.display()))?;
    if !output.status.success() {
        if keep_artifacts {
            eprintln!("llvm backend: failing IR kept at {}", ll_path.display());
        }
        return Err(anyhow!(
            "llc failed ({status}): {stderr}",
            status = output.status,
            stderr = String::from_utf8_lossy(&output.stderr)
        ));
    }
    let _ = std::fs::remove_file(&opt_path);
    Ok(())
}

/// Runs LLVM's mid-end and object backend through one Clang driver process.
/// Clang consumes LLVM IR directly, so this is equivalent to the normal
/// `opt` then `llc` sequence for non-PGO builds while avoiding a second child
/// launch and the intermediate bitcode file. The split-tool path remains the
/// compatibility route for PGO and installations without Clang.
fn invoke_clang_pipeline(
    clang: &std::path::Path,
    ll_path: &std::path::Path,
    obj_out: &std::path::Path,
    triple: &str,
    profile: OptProfile,
    mcpu: &str,
) -> Result<()> {
    let mut cmd = std::process::Command::new(clang);
    cmd.arg("-x")
        .arg("ir")
        .arg("-c")
        .arg(match profile {
            OptProfile::Debug => "-O0",
            OptProfile::Release => "-O3",
        })
        .arg(format!("--target={triple}"));
    if target_arch_from_triple(triple) == "x86_64" {
        cmd.arg(format!("-march={mcpu}"));
        cmd.arg("-mprefer-vector-width=256");
    } else {
        cmd.arg(format!("-mcpu={mcpu}"));
    }
    if !triple.contains("windows") {
        cmd.arg("-fPIC");
    }
    if matches!(profile, OptProfile::Release) && disable_loop_idiom_for_target(triple) {
        cmd.arg("-mllvm").arg("-disable-loop-idiom-all");
    }
    if want_dwarf() {
        cmd.arg("-gdwarf-4");
    }
    cmd.arg(ll_path).arg("-o").arg(obj_out);
    let output = run_with_timeout(cmd, opt_timeout(), "clang")
        .with_context(|| format!("spawn {}", clang.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "clang IR-to-object pipeline failed ({status}): {stderr}",
            status = output.status,
            stderr = String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(())
}

/// Returns the wall-clock cap for the `opt` and `llc` subprocesses.
/// `GOS_LLVM_OPT_TIMEOUT_SECS=N` overrides; defaults to 10 minutes,
/// generous enough for huge monomorph fan-outs but tight enough
/// that an unbounded `opt -O3` blowup turns into a build failure
/// instead of a process holding the runner forever.
/// Target CPU passed to `opt` and `llc`. Host release builds default to
/// `native` (matching `rustc -C target-cpu=native`); `GOS_LLVM_MCPU` lets
/// callers override. Reproducible and cross builds keep a portable target
/// baseline.
/// Default LLVM `-mcpu` target used when `GOS_LLVM_MCPU` is unset.
fn mcpu_target(triple: &str) -> String {
    if let Ok(s) = std::env::var("GOS_LLVM_MCPU") {
        return s;
    }
    mcpu_for(
        triple,
        TARGET_TRIPLE_OVERRIDE.get().is_some(),
        want_reproducible(),
    )
}

/// The architecture component of an LLVM target triple
/// (`x86_64-unknown-linux-gnu` -> `"x86_64"`).
fn target_arch_from_triple(triple: &str) -> &'static str {
    match triple.split('-').next().unwrap_or("") {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        "riscv64" | "riscv64gc" => "riscv64",
        _ => "unknown",
    }
}

fn llvm_target_triple_for(triple: &str) -> String {
    llvm_target_triple_for_with_deployment(
        triple,
        std::env::var("MACOSX_DEPLOYMENT_TARGET").ok().as_deref(),
    )
}

fn llvm_target_triple_for_with_deployment(triple: &str, configured: Option<&str>) -> String {
    if !triple.ends_with("-apple-darwin") {
        return triple.to_string();
    }
    let arch = triple.split('-').next().unwrap_or("");
    if arch.is_empty() {
        return triple.to_string();
    }
    let deployment = normalized_macos_deployment_target(configured);
    format!("{arch}-apple-macosx{deployment}")
}

fn normalized_macos_deployment_target(configured: Option<&str>) -> String {
    const DEFAULT: &str = "15.0";

    let value = configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT);
    let mut parts = value.split('.');
    let Some(major) = parts.next().filter(|part| is_decimal_component(part)) else {
        return value.to_string();
    };
    let minor = parts
        .next()
        .filter(|part| is_decimal_component(part))
        .unwrap_or("0");
    let patch = parts
        .next()
        .filter(|part| is_decimal_component(part))
        .unwrap_or("0");
    format!("{major}.{minor}.{patch}")
}

fn is_decimal_component(part: &str) -> bool {
    !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
}

/// `-mcpu` for `triple`. `is_cross` is true when an explicit `--target`
/// override is active. A cross or reproducible build must never use `native`,
/// which names the host CPU.
fn mcpu_for(triple: &str, is_cross: bool, reproducible: bool) -> String {
    if !is_cross {
        return if reproducible {
            match target_arch_from_triple(triple) {
                "x86_64" => "x86-64-v3".to_string(),
                "aarch64" => "generic".to_string(),
                _ => "generic".to_string(),
            }
        } else {
            "native".to_string()
        };
    }
    match target_arch_from_triple(triple) {
        // Reproducible x86-64 baseline (AVX2 + BMI2 + FMA, ~2013+).
        "x86_64" => "x86-64-v3".to_string(),
        // Generic ARMv8-A: portable across every aarch64 device;
        // `GOS_LLVM_MCPU=cortex-a76` opts into Pi 5 tuning.
        "aarch64" => "generic".to_string(),
        _ => "generic".to_string(),
    }
}

fn opt_timeout() -> std::time::Duration {
    let secs = std::env::var("GOS_LLVM_OPT_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(600);
    std::time::Duration::from_secs(secs)
}

/// Spawns `cmd`, waits up to `timeout`, and surfaces a clear error
/// when the subprocess exceeds the cap (kills the child first so
/// it doesn't outlive the build). Captures stdout / stderr through
/// a 64 KiB-per-stream cap so a runaway `opt`/`llc` diagnostic
/// stream cannot grow unbounded. The polling cadence (50 ms) keeps
/// the steady-state overhead negligible compared to `opt -O3`'s
/// usual runtime.
fn run_with_timeout(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
    tool: &str,
) -> Result<std::process::Output> {
    use std::io::Read;
    use wait_timeout::ChildExt as _;

    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("spawn {tool}"))?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_thread = stdout_pipe.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            cap_diagnostic_stream(buf)
        })
    });
    let stderr_thread = stderr_pipe.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            cap_diagnostic_stream(buf)
        })
    });
    let status = match child.wait_timeout(timeout) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "{tool} exceeded {secs}s timeout (set GOS_LLVM_OPT_TIMEOUT_SECS to raise it)",
                secs = timeout.as_secs(),
            ));
        }
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("{tool} wait failed: {err}"));
        }
    };
    let stdout = stdout_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr_thread
        .map(|t| t.join().unwrap_or_default())
        .unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Caps a captured subprocess stream at the last 64 KiB. LLVM tools
/// occasionally emit hundreds of MB of repetitive diagnostics (e.g.
/// instcombine loops on pathological IR); without a cap, the parent
/// build process would mirror that growth in RSS while waiting for
/// the child to exit. The tail is kept rather than the head because
/// the actionable error (the failure point) is invariably last.
fn cap_diagnostic_stream(buf: Vec<u8>) -> Vec<u8> {
    const CAP: usize = 64 * 1024;
    if buf.len() <= CAP {
        return buf;
    }
    let trimmed_start = buf.len() - CAP;
    let mut out = Vec::with_capacity(CAP + 64);
    out.extend_from_slice(
        format!("[diagnostic stream truncated: dropped {trimmed_start} bytes]\n").as_bytes(),
    );
    out.extend_from_slice(&buf[trimmed_start..]);
    out
}

/// The LLVM major discovery prefers: the one `rustc` bundles.
///
/// What this buys is one optimiser between a developer's machine and
/// CI. The emitted IR is optimised by whichever `clang` / `opt` / `llc`
/// discovery finds, so an unpinned toolchain means two machines compile
/// the same program through two different optimisers - and in a project
/// whose quality argument is that any tier divergence is a bug, a
/// divergence that is really an LLVM-version artifact costs a real
/// investigation. Preferring the major `rustc` already bundles means
/// the whole toolchain has one version to name.
///
/// It is pinned to `rust-toolchain.toml`, not to LLVM's release
/// cadence: `preferred_llvm_major_matches_rustc` fails the build when
/// the two drift apart, so a toolchain bump is one deliberate change to
/// both.
pub const PREFERRED_LLVM_MAJOR: u32 = 22;

/// The oldest LLVM major that still compiles this emitter's IR.
///
/// It stays where the toolchain has always had it. Every fixture that
/// builds under 18 builds bit-identically under 20, and the two changes
/// this emitter would have had to care about - LLVM 21's `nocapture`
/// rename and the debug-record transition - reach IR it does not write:
/// it emits only `nounwind` and `noalias`, and no `llvm.dbg.*` at all.
/// An older major therefore produces the same program, and Ubuntu 24.04
/// still ships 18, so raising this floor would cost those users a manual
/// LLVM install and buy them nothing.
pub const MINIMUM_LLVM_MAJOR: u32 = 18;

/// A floor above the preferred major would leave no version satisfying
/// both, so the pair is checked where it is written rather than at run
/// time.
const _: () = assert!(MINIMUM_LLVM_MAJOR <= PREFERRED_LLVM_MAJOR);

/// The LLVM major `tool` reports, or `None` when it does not answer a
/// recognisable `--version`.
fn llvm_tool_major(tool: &Path) -> Option<u32> {
    if let Some(remembered) = remembered_llvm_major(tool) {
        return remembered.0;
    }
    let major = probe_llvm_major(tool);
    remember_llvm_major(tool, major);
    major
}

/// A version answer read back from the cache. The inner `Option` is the
/// answer itself, which is `None` for a tool whose banner said nothing.
struct RememberedMajor(Option<u32>);

/// Path of the note recording `tool`'s major version, or `None` when this
/// build keeps no cache.
///
/// Asking a tool its version runs it, and an LLVM tool's startup maps the
/// whole shared library, which on a small program costs more than every
/// other piece of cache bookkeeping together. The answer changes only when
/// the binary does, so the note is keyed by the binary's identity.
fn llvm_major_note_path(tool: &Path) -> Option<PathBuf> {
    let dir = toolchain_cache_dir()?;
    let key = fnv1a_64(file_identity(tool).as_bytes());
    Some(dir.join(format!("llvm-major-{key:016x}")))
}

fn remembered_llvm_major(tool: &Path) -> Option<RememberedMajor> {
    let text = std::fs::read_to_string(llvm_major_note_path(tool)?).ok()?;
    let text = text.trim();
    if text == "unknown" {
        return Some(RememberedMajor(None));
    }
    text.parse().ok().map(|major| RememberedMajor(Some(major)))
}

fn remember_llvm_major(tool: &Path, major: Option<u32>) {
    let Some(path) = llvm_major_note_path(tool) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        path,
        major.map_or_else(|| "unknown".to_string(), |m| m.to_string()),
    );
}

fn probe_llvm_major(tool: &Path) -> Option<u32> {
    let out = std::process::Command::new(tool)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    parse_llvm_major(&text)
}

/// The major from an LLVM tool's `--version` banner, which spells the
/// version either as `LLVM version 22.1.2` or, for a vendor clang, as
/// `clang version 22.1.8`.
fn parse_llvm_major(text: &str) -> Option<u32> {
    for marker in ["LLVM version ", "clang version "] {
        if let Some(rest) = text.split(marker).nth(1) {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(major) = digits.parse::<u32>() {
                return Some(major);
            }
        }
    }
    None
}

/// Reports the found tool's major once, when it is not the preferred
/// one. An older major still builds a correct program; what it cannot
/// do is take part in cross-language LTO, so the warning names that and
/// not something vaguer. Below the minimum is an error, because the IR
/// this emitter writes is not known to parse there at all.
fn check_llvm_major(tool_name: &str, path: &Path) -> std::result::Result<(), String> {
    static WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let Some(major) = llvm_tool_major(path) else {
        return Ok(());
    };
    if major == PREFERRED_LLVM_MAJOR {
        return Ok(());
    }
    if major < MINIMUM_LLVM_MAJOR {
        return Err(format!(
            "{tool_name} at {} is LLVM {major}, below the minimum this toolchain \
             emits IR for (LLVM {MINIMUM_LLVM_MAJOR}). Install LLVM \
             {PREFERRED_LLVM_MAJOR}, or set the tool override to a newer one.",
            path.display(),
        ));
    }
    WARNED.get_or_init(|| {
        eprintln!(
            "note: building with LLVM {major} ({}), not the LLVM \
             {PREFERRED_LLVM_MAJOR} this toolchain is built against. The program is \
             the same; a timing or a codegen difference measured against a \
             build made with LLVM {PREFERRED_LLVM_MAJOR} is not comparable.",
            path.display(),
        );
    });
    Ok(())
}

#[cfg(test)]
mod llvm_version_tests {
    use super::parse_llvm_major;

    /// The two banners the tools this backend shells out to actually
    /// print: `opt` and `llc` name LLVM, a vendor clang names itself.
    #[test]
    fn a_major_is_read_from_either_banner_spelling() {
        assert_eq!(
            parse_llvm_major("Ubuntu LLVM version 22.1.2\n  Optimized build.\n"),
            Some(22)
        );
        assert_eq!(
            parse_llvm_major("Ubuntu clang version 22.1.8 (++20260714)\nTarget: x86_64\n"),
            Some(22)
        );
    }

    #[test]
    fn a_banner_with_no_version_answers_nothing() {
        assert_eq!(parse_llvm_major("some other tool\n"), None);
        assert_eq!(parse_llvm_major(""), None);
    }

    /// A tool that answers nothing recognisable is left alone rather
    /// than refused: a wrapper script that prints its own banner is
    /// still the tool the caller chose.
    #[test]
    fn an_unparsed_banner_does_not_look_like_a_mismatch() {
        assert!(parse_llvm_major("clang wrapper\n").is_none());
    }
}

fn find_opt() -> Result<PathBuf> {
    static OPT_PATH: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    OPT_PATH
        .get_or_init(|| find_llvm_tool("opt", "GOS_LLVM_OPT", OPT_CANDIDATES))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn find_llc() -> Result<PathBuf> {
    static LLC_PATH: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    LLC_PATH
        .get_or_init(|| find_llvm_tool("llc", "GOS_LLC", LLC_CANDIDATES))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn find_clang() -> Result<PathBuf> {
    static CLANG_PATH: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    CLANG_PATH
        .get_or_init(|| {
            if let Ok(path) = std::env::var("GOS_LLVM_CLANG") {
                let path = PathBuf::from(path);
                check_llvm_major("clang", &path)?;
                return Ok(path);
            }
            // Prefer the Clang beside the selected `opt`; this avoids mixing
            // LLVM major versions when several installations are present.
            if let Ok(opt) = find_opt()
                && let Some(dir) = opt.parent()
            {
                for name in if cfg!(windows) {
                    &["clang.exe"][..]
                } else {
                    &[
                        "clang", "clang-22", "clang-21", "clang-20", "clang-19", "clang-18",
                    ][..]
                } {
                    let candidate = dir.join(name);
                    if candidate.is_file() {
                        check_llvm_major("clang", &candidate)?;
                        return Ok(candidate);
                    }
                }
            }
            find_llvm_tool("clang", "GOS_LLVM_CLANG", CLANG_CANDIDATES)
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

/// Selects the single-process IR-to-object route. Apple targets deliberately
/// retain the explicit `opt -O3` then `llc -O3` pipeline: the system Apple
/// Clang driver is not a substitute for the selected LLVM toolchain and made
/// release performance indistinguishable from debug in issue #102. Debug keeps
/// the split path because its minimal `mem2reg` pass is essential for usable
/// loop code, while Clang `-O0` leaves the emitted alloca-heavy IR in memory.
/// PGO also keeps explicit `opt` because its pipeline flags and profile-file
/// semantics are not interchangeable with the Clang driver's source-oriented
/// PGO switches.
fn integrated_clang_path(triple: &str) -> Option<PathBuf> {
    if matches!(opt_profile(), OptProfile::Debug)
        || triple.contains("apple")
        || std::env::var("GOS_LLVM_SPLIT_TOOLS").is_ok()
        || pgo_mode().is_some()
        || std::env::var("GOS_PGO_COLLECT").is_ok()
        || std::env::var("GOS_PGO_PROFILE").is_ok()
    {
        return None;
    }
    find_clang().ok()
}

fn find_llvm_tool(
    tool: &str,
    env_var: &str,
    candidates: &[&str],
) -> std::result::Result<PathBuf, String> {
    // The override says which tool to use, not that its version stops
    // mattering: a major below the floor cannot compile this emitter's
    // IR whoever chose it.
    let found = if let Ok(path) = std::env::var(env_var) {
        PathBuf::from(path)
    } else {
        candidates
            .iter()
            .find(|candidate| is_executable(candidate))
            .map(PathBuf::from)
            .ok_or_else(|| missing_llvm_tool_message(tool, env_var))?
    };
    check_llvm_major(tool, &found)?;
    Ok(found)
}

/// Cross-platform candidate list for the LLVM `opt` driver. Order
/// matters: PATH-resolvable bare names first (cheap), then well-known
/// system locations on Linux (apt), macOS (Homebrew, both Apple Silicon
/// and Intel prefixes), and Windows (MSYS2 mingw - which is the only
/// commonly-installed source that actually ships `opt.exe` / `llc.exe`
/// on Windows, since the upstream LLVM installer ships only the clang
/// front-end).
///
/// Version-suffixed entries lead with [`PREFERRED_LLVM_MAJOR`], the
/// major `rustc` bundles, so a host that has it uses it, and descend to
/// [`MINIMUM_LLVM_MAJOR`]. Which one gets picked used to depend on the
/// order a list happened to be written in - the previous ordering put
/// 18 ahead of 20, so a host with both silently built with the older -
/// and [`check_llvm_major`] now says which one answered. The bare name
/// comes after the suffixed ones because it is whichever major the
/// distro's `llvm` metapackage last pointed at.
const OPT_CANDIDATES: &[&str] = &[
    // PATH lookups
    "opt-22",
    "opt-21",
    "opt-20",
    "opt-19",
    "opt-18",
    "opt",
    // Linux (apt-installed)
    "/usr/lib/llvm-22/bin/opt",
    "/usr/lib/llvm-21/bin/opt",
    "/usr/lib/llvm-20/bin/opt",
    "/usr/lib/llvm-19/bin/opt",
    "/usr/lib/llvm-18/bin/opt",
    // macOS Homebrew (Apple Silicon)
    "/opt/homebrew/opt/llvm@22/bin/opt",
    "/opt/homebrew/opt/llvm@21/bin/opt",
    "/opt/homebrew/opt/llvm@20/bin/opt",
    "/opt/homebrew/opt/llvm@19/bin/opt",
    "/opt/homebrew/opt/llvm@18/bin/opt",
    "/opt/homebrew/opt/llvm/bin/opt",
    "/opt/homebrew/bin/opt",
    // macOS Homebrew (Intel)
    "/usr/local/opt/llvm@22/bin/opt",
    "/usr/local/opt/llvm@21/bin/opt",
    "/usr/local/opt/llvm@20/bin/opt",
    "/usr/local/opt/llvm@19/bin/opt",
    "/usr/local/opt/llvm@18/bin/opt",
    "/usr/local/opt/llvm/bin/opt",
    "/usr/local/bin/opt",
    // Windows (MSYS2 mingw - full LLVM via `pacman -S
    // mingw-w64-x86_64-llvm`; also `mingw-w64-clang-x86_64-llvm`
    // under `clang64/`).
    "C:\\msys64\\mingw64\\bin\\opt.exe",
    "C:\\msys64\\clang64\\bin\\opt.exe",
    "C:\\msys64\\ucrt64\\bin\\opt.exe",
    // Windows (LLVM upstream installer - usually clang-only,
    // but a custom-built distribution may include opt; kept as a
    // last-resort path).
    "C:\\Program Files\\LLVM\\bin\\opt.exe",
    "C:\\Program Files (x86)\\LLVM\\bin\\opt.exe",
];

/// Parallel candidate list for `llc`. See [`OPT_CANDIDATES`] for the
/// ordering rationale; the entries mirror it directly.
const LLC_CANDIDATES: &[&str] = &[
    "llc-22",
    "llc-21",
    "llc-20",
    "llc-19",
    "llc-18",
    "llc",
    "/usr/lib/llvm-22/bin/llc",
    "/usr/lib/llvm-21/bin/llc",
    "/usr/lib/llvm-20/bin/llc",
    "/usr/lib/llvm-19/bin/llc",
    "/usr/lib/llvm-18/bin/llc",
    "/opt/homebrew/opt/llvm@22/bin/llc",
    "/opt/homebrew/opt/llvm@21/bin/llc",
    "/opt/homebrew/opt/llvm@20/bin/llc",
    "/opt/homebrew/opt/llvm@19/bin/llc",
    "/opt/homebrew/opt/llvm@18/bin/llc",
    "/opt/homebrew/opt/llvm/bin/llc",
    "/opt/homebrew/bin/llc",
    "/usr/local/opt/llvm@22/bin/llc",
    "/usr/local/opt/llvm@21/bin/llc",
    "/usr/local/opt/llvm@20/bin/llc",
    "/usr/local/opt/llvm@19/bin/llc",
    "/usr/local/opt/llvm@18/bin/llc",
    "/usr/local/opt/llvm/bin/llc",
    "/usr/local/bin/llc",
    "C:\\msys64\\mingw64\\bin\\llc.exe",
    "C:\\msys64\\clang64\\bin\\llc.exe",
    "C:\\msys64\\ucrt64\\bin\\llc.exe",
    "C:\\Program Files\\LLVM\\bin\\llc.exe",
    "C:\\Program Files (x86)\\LLVM\\bin\\llc.exe",
];

/// Clang candidates used for the integrated LLVM IR-to-object pipeline.
/// Ordered like [`OPT_CANDIDATES`].
const CLANG_CANDIDATES: &[&str] = &[
    "clang-22",
    "clang-21",
    "clang-20",
    "clang-19",
    "clang-18",
    "clang",
    "/usr/lib/llvm-22/bin/clang",
    "/usr/lib/llvm-21/bin/clang",
    "/usr/lib/llvm-20/bin/clang",
    "/usr/lib/llvm-19/bin/clang",
    "/usr/lib/llvm-18/bin/clang",
    "/opt/homebrew/opt/llvm@22/bin/clang",
    "/opt/homebrew/opt/llvm@21/bin/clang",
    "/opt/homebrew/opt/llvm@20/bin/clang",
    "/opt/homebrew/opt/llvm@19/bin/clang",
    "/opt/homebrew/opt/llvm@18/bin/clang",
    "/opt/homebrew/opt/llvm/bin/clang",
    "/usr/local/opt/llvm@22/bin/clang",
    "/usr/local/opt/llvm@21/bin/clang",
    "/usr/local/opt/llvm@20/bin/clang",
    "/usr/local/opt/llvm@19/bin/clang",
    "/usr/local/opt/llvm@18/bin/clang",
    "/usr/local/opt/llvm/bin/clang",
    "C:\\msys64\\mingw64\\bin\\clang.exe",
    "C:\\msys64\\clang64\\bin\\clang.exe",
    "C:\\msys64\\ucrt64\\bin\\clang.exe",
    "C:\\Program Files\\LLVM\\bin\\clang.exe",
    "C:\\Program Files (x86)\\LLVM\\bin\\clang.exe",
];

fn missing_llvm_tool_message(tool: &str, env_var: &str) -> String {
    format!(
        "{tool} (LLVM toolchain) not found. Install LLVM {MINIMUM_LLVM_MAJOR} or \
         newer and retry ({PREFERRED_LLVM_MAJOR} is what this toolchain is built \
         against):\n  \
         Linux:   apt install llvm-{MINIMUM_LLVM_MAJOR}-dev clang-{MINIMUM_LLVM_MAJOR}\n           \
                  (for LLVM {PREFERRED_LLVM_MAJOR}, which most distros do not\n           \
                  package yet, add the apt.llvm.org repository)\n  \
         macOS:   brew install llvm@{PREFERRED_LLVM_MAJOR}\n  \
         Windows: pacman -S mingw-w64-x86_64-llvm       (from MSYS2; the upstream LLVM\n           \
                                                         Windows installer ships clang\n           \
                                                         but not `opt`/`llc`)\n\
         Or set `{env_var}` to the absolute path of `{tool}`."
    )
}

fn is_executable(path: &str) -> bool {
    if let Ok(meta) = std::fs::metadata(path) {
        return meta.is_file();
    }
    // Bare name (no path separator)? Walk `PATH` looking for it.
    // Use `std::env::split_paths` so the separator is correct on
    // every platform (`:` on Unix, `;` on Windows), and try the
    // `.exe` suffix on Windows when the caller passed a bare stem.
    let has_separator = path.contains('/') || path.contains('\\');
    if has_separator {
        return false;
    }
    let Ok(paths) = std::env::var("PATH") else {
        return false;
    };
    let suffixes: &[&str] = if cfg!(windows) && !path.to_ascii_lowercase().ends_with(".exe") {
        &["", ".exe"]
    } else {
        &[""]
    };
    for dir in std::env::split_paths(&paths) {
        for suffix in suffixes {
            let candidate = dir.join(format!("{path}{suffix}"));
            if std::fs::metadata(&candidate).is_ok_and(|m| m.is_file()) {
                return true;
            }
        }
    }
    false
}

fn host_triple() -> String {
    // An explicit `--target` (via `set_target_triple`) wins over
    // everything so a cross `gos build` drives opt/llc, the i128 ABI
    // marshalling, and the object-cache key at the requested triple.
    if let Some(triple) = TARGET_TRIPLE_OVERRIDE.get() {
        return triple.clone();
    }
    detect_host_triple()
}

/// The host's own LLVM target triple, ignoring any cross-compile
/// override. `llc` uses this to pick the object-file format (ELF on
/// Linux, Mach-O on Darwin, COFF on Windows); getting the OS portion
/// wrong produces an object the host's `ld` rejects as "unknown file
/// type". `TARGET` (set by cargo build scripts) takes precedence so a
/// build-script-driven cross still works; otherwise arch + OS come
/// from `std::env::consts` (cross-platform, no subprocess).
fn detect_host_triple() -> String {
    if let Ok(triple) = std::env::var("TARGET") {
        return triple;
    }
    let arch = std::env::consts::ARCH;
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-gnu",
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        "freebsd" => "unknown-freebsd",
        "ios" => "apple-ios",
        // Conservative default - Linux is the dev host. Any
        // unrecognised target will produce a clear `llc` error
        // rather than a silently mis-formatted object.
        _ => "unknown-linux-gnu",
    };
    format!("{arch}-{os}")
}

/// True when the build target is `x86_64-pc-windows-*`, driving the
/// Win64 i128 (Fat-aggregate) calling-convention adjustments at the
/// `gos_rt_*` C-ABI boundary. Derived from the resolved target triple
/// ([`host_triple`], which honours `TARGET`) rather than `cfg!(windows)`
/// so a Linux-hosted cross-build to a Windows triple emits the Win64
/// marshalling instead of the host's SysV shape - the two disagree on
/// how `extern "C"` returns/passes a 2-word `i128` (xmm `<16 x i8>` vs
/// a GP-register pair), and keying off the host silently miscompiled
/// every cross-target build.
pub(crate) fn target_is_windows() -> bool {
    host_triple().contains("windows")
}

#[cfg(test)]
mod shape_validation_tests {
    use super::validate_global_decl_shape;

    #[test]
    fn accepts_constant_definition() {
        let g = "@.str_0 = private unnamed_addr constant [6 x i8] c\"hello\\00\"";
        assert!(validate_global_decl_shape(g).is_ok());
    }

    #[test]
    fn accepts_extern_global() {
        let g = "@GOS_RT_STDOUT_LEN = external local_unnamed_addr global i64";
        assert!(validate_global_decl_shape(g).is_ok());
    }

    #[test]
    fn accepts_function_declaration() {
        let g = "declare void @gos_rt_print_str(ptr)";
        assert!(validate_global_decl_shape(g).is_ok());
    }

    #[test]
    fn rejects_bare_identifier() {
        // The exact regression shape: a runtime symbol name
        // accidentally inserted as a bare string instead of a
        // full `@name = constant ...` declaration.
        let g = "gos_rt_arena_save";
        let err = validate_global_decl_shape(g).unwrap_err();
        assert!(
            err.to_string().contains("malformed module-level entry"),
            "expected shape diagnostic, got: {err}"
        );
    }

    #[test]
    fn rejects_random_text() {
        let g = "this is not LLVM IR";
        assert!(validate_global_decl_shape(g).is_err());
    }
}

#[cfg(test)]
mod host_triple_tests {
    use super::{
        detect_host_triple, disable_loop_idiom_for_target_with_static_musl, integrated_clang_path,
        llvm_target_triple_for_with_deployment, mcpu_for, module_datalayout,
        normalized_macos_deployment_target, target_arch_from_triple,
    };

    /// `llc` selects the object-file format from the OS portion of
    /// the triple - ELF on a Linux triple, Mach-O on `apple-darwin`,
    /// COFF on `pc-windows-msvc`. If `host_triple` hardcoded
    /// `unknown-linux-gnu` on every host (as it used to), macOS /
    /// Windows builds linked with `ld: unknown file type` because
    /// the object format was wrong. This regression pins the OS
    /// portion of the triple to the running host so any future
    /// drift fails at unit-test time rather than at `gos build`
    /// time.
    ///
    /// Cargo sets `TARGET` for build scripts, not for normal test
    /// binaries - in `cargo test` runs the env var is unset and
    /// the function exercises its OS-detection branch, which is
    /// exactly what we want to cover here.
    #[test]
    fn host_triple_matches_running_os() {
        if std::env::var("TARGET").is_ok() {
            // Cross-compilation override is active - the function
            // is just echoing back `TARGET` and the host-detection
            // branch isn't covered. Skip rather than assert a
            // mismatch we can't control.
            return;
        }
        // Call the detection helper directly, not `host_triple`: the
        // latter consults the process-wide target override, which a
        // sibling test may have set, and would pollute this assertion.
        let triple = detect_host_triple();
        let expected_os_part = match std::env::consts::OS {
            "linux" => "unknown-linux-gnu",
            "macos" => "apple-darwin",
            "windows" => "pc-windows-msvc",
            "freebsd" => "unknown-freebsd",
            "ios" => "apple-ios",
            _ => "unknown-linux-gnu",
        };
        assert!(
            triple.ends_with(expected_os_part),
            "host_triple {triple:?} does not end with {expected_os_part:?} \
             for OS {os:?}; llc would emit the wrong object format and the \
             system linker would reject it",
            os = std::env::consts::OS,
        );
        assert!(
            triple.starts_with(std::env::consts::ARCH),
            "host_triple {triple:?} does not start with arch {arch:?}",
            arch = std::env::consts::ARCH,
        );
    }

    #[test]
    fn mcpu_cross_target_is_portable_not_host() {
        // A cross build to aarch64 must not inherit an x86 -mcpu, and
        // must never use `native` (the host CPU). Tested through the
        // pure helper so it needs no process-wide override.
        assert_eq!(
            mcpu_for("aarch64-unknown-linux-gnu", true, false),
            "generic"
        );
        assert_eq!(
            mcpu_for("aarch64-unknown-linux-musl", true, false),
            "generic"
        );
        assert_eq!(
            mcpu_for("x86_64-unknown-linux-gnu", true, false),
            "x86-64-v3"
        );
    }

    #[test]
    fn mcpu_native_host_uses_host_cpu_unless_reproducible() {
        assert_eq!(mcpu_for("x86_64-unknown-linux-gnu", false, false), "native");
        assert_eq!(
            mcpu_for("x86_64-unknown-linux-gnu", false, true),
            "x86-64-v3"
        );
    }

    #[test]
    fn prefer_256_bit_only_for_x86_target() {
        // The AVX-512 width cap is x86-only; an aarch64 target (cross
        // from any host) must not receive it.
        assert_eq!(
            target_arch_from_triple("aarch64-unknown-linux-musl"),
            "aarch64"
        );
        assert_eq!(
            target_arch_from_triple("x86_64-unknown-linux-gnu"),
            "x86_64"
        );
    }

    #[test]
    fn loop_idiom_workaround_is_scoped_to_static_musl() {
        assert!(disable_loop_idiom_for_target_with_static_musl(
            true,
            "x86_64-unknown-linux-gnu"
        ));
        assert!(disable_loop_idiom_for_target_with_static_musl(
            false,
            "x86_64-unknown-linux-musl"
        ));
        for triple in [
            "x86_64-unknown-linux-gnu",
            "aarch64-apple-macosx15.0.0",
            "x86_64-pc-windows-msvc",
        ] {
            assert!(
                !disable_loop_idiom_for_target_with_static_musl(false, triple),
                "{triple} must keep LLVM loop idiom recognition"
            );
        }
    }

    #[test]
    fn apple_targets_keep_the_explicit_llvm_release_pipeline() {
        assert!(integrated_clang_path("aarch64-apple-macosx15.0.0").is_none());
    }

    #[test]
    fn darwin_llvm_triple_pins_deployment_target() {
        assert_eq!(
            llvm_target_triple_for_with_deployment("aarch64-apple-darwin", None),
            "aarch64-apple-macosx15.0.0"
        );
        assert_eq!(
            llvm_target_triple_for_with_deployment("x86_64-apple-darwin", Some("14.2")),
            "x86_64-apple-macosx14.2.0"
        );
        assert_eq!(
            llvm_target_triple_for_with_deployment("aarch64-unknown-linux-gnu", Some("14.2")),
            "aarch64-unknown-linux-gnu"
        );
    }

    #[test]
    fn macos_deployment_target_normalizes_for_llvm() {
        assert_eq!(normalized_macos_deployment_target(None), "15.0.0");
        assert_eq!(normalized_macos_deployment_target(Some("15")), "15.0.0");
        assert_eq!(normalized_macos_deployment_target(Some("15.1")), "15.1.0");
        assert_eq!(normalized_macos_deployment_target(Some("15.1.2")), "15.1.2");
    }

    #[test]
    fn datalayout_keeps_i128_at_flat_slot_alignment() {
        let x86 = module_datalayout("x86_64-unknown-linux-gnu").expect("x86_64 layout");
        assert!(x86.contains("i128:64"), "{x86}");
        let arm = module_datalayout("aarch64-apple-macosx15.0.0").expect("aarch64 layout");
        assert!(arm.contains("m:o"), "{arm}");
        assert!(arm.contains("i128:64"), "{arm}");
        assert!(arm.contains("n32:64"), "{arm}");
    }
}

#[cfg(test)]
mod symbol_audit_tests {
    use super::audit_llvm_ir_symbols_text;

    #[test]
    fn audit_accepts_defined_declared_and_global_symbols() {
        let ir = r#"
; ModuleID = "gossamer"
@G = private constant [3 x i8] c"a@b"
declare void @puts(ptr)
define void @"main"() {
entry:
  call void @puts(ptr @G)
  ret void
}
"#;
        audit_llvm_ir_symbols_text(ir).expect("IR symbols should be complete");
    }

    #[test]
    fn audit_reports_missing_symbol_with_function_context() {
        let ir = r#"
define void @"main"() {
entry:
  %t0 = call i1 @"is_halted"(ptr null)
  ret void
}
"#;
        let err = audit_llvm_ir_symbols_text(ir).unwrap_err().to_string();
        assert!(err.contains("@is_halted referenced from main"), "{err}");
        assert!(err.contains("undefined symbols before LLVM tools"), "{err}");
    }

    #[test]
    fn audit_ignores_symbols_inside_string_constants_and_comments() {
        let ir = r#"
@S = private constant [17 x i8] c"user@example.com\00"
define void @"main"() {
entry:
  ; @missing_in_comment
  ret void
}
"#;
        audit_llvm_ir_symbols_text(ir).expect("string/comment @ signs are not symbol refs");
    }
}

#[cfg(test)]
mod cabi_thunk_tests {
    use super::render_cabi_handler_thunk;

    /// `render_cabi_handler_thunk` must emit a plain `define`, not
    /// `define linkonce_odr`. On ELF, `linkonce_odr` deduplicates across
    /// translation units implicitly, but lld-link (Windows COFF) requires an
    /// explicit COMDAT section for dedup and treats bare `linkonce_odr` as a
    /// duplicate strong symbol when the same thunk appears in multiple chunks.
    /// The fix emits the thunk once (in the chunk that owns the handler body),
    /// so `linkonce_odr` is no longer needed - and no longer safe on COFF.
    #[test]
    fn cabi_thunk_uses_plain_define_not_linkonce_odr() {
        let ir = render_cabi_handler_thunk("App::serve", 2);
        assert!(
            ir.contains("define <16 x i8>"),
            "expected plain `define`, got:\n{ir}"
        );
        assert!(
            !ir.contains("linkonce_odr"),
            "must not use linkonce_odr (causes duplicate-symbol on Windows COFF lld-link):\n{ir}"
        );
    }

    #[test]
    fn cabi_thunk_calls_the_real_handler_and_bitcasts() {
        let ir = render_cabi_handler_thunk("Proxy::serve", 2);
        assert!(
            ir.contains("call i128 @\"Proxy::serve\""),
            "must call real handler"
        );
        assert!(
            ir.contains("bitcast i128"),
            "must bitcast i128 to <16 x i8>"
        );
        assert!(ir.contains("ret <16 x i8>"), "must return <16 x i8>");
    }

    /// Every runtime shim that invokes a gossamer callback as
    /// `extern "C" fn(..) -> i128` must be collected, so the callback is
    /// reached through its `<16 x i8>` thunk on Win64. `gos_rt_fs_walk_dir`
    /// takes its visitor as an env blob whose slot 0 holds the callable,
    /// the same shape as the i128 combinators.
    #[test]
    fn walk_dir_visitor_is_collected_as_a_cabi_handler() {
        let handlers = super::collect_cabi_handlers(&[env_callback_body("gos_rt_fs_walk_dir")]);
        assert!(
            handlers.contains_key("visit"),
            "walk_dir visitor must be collected, got: {handlers:?}"
        );
    }

    /// `spawn(f)` hands the runtime a callable the runtime invokes as
    /// `extern "C-unwind" fn(usize) -> i128` - the same crossing the
    /// combinators make, so a two-word spawn needs the Win64 forwarding
    /// thunk.
    #[test]
    fn two_word_spawn_needs_the_win64_forwarding_thunk() {
        assert!(super::body_has_wide_spawn(&spawn_body(2)));
    }

    /// A one-word spawn callable is invoked as `-> i64`, which already
    /// agrees on the GP register: routing it through the `<16 x i8>` thunk
    /// would hand the runtime a vector register it never reads.
    #[test]
    fn one_word_spawn_stays_on_the_gp_register() {
        assert!(!super::body_has_wide_spawn(&spawn_body(1)));
    }

    /// The thunk reads the callable at slot 0 of the env blob - the same
    /// address the spawn lowering passes - and re-emits its `i128` in the
    /// vector register.
    #[test]
    fn spawn_wide_thunk_forwards_slot_zero_and_bitcasts() {
        let ir = super::render_spawn_wide_cabi_thunk();
        assert!(ir.contains("load ptr, ptr %env"), "{ir}");
        assert!(ir.contains("call i128 %fn_ptr(ptr %env)"), "{ir}");
        assert!(ir.contains("ret <16 x i8>"), "{ir}");
    }

    /// Builds a body shaped like the `spawn(f)` lowering: the callable's
    /// address sits at offset 0 of the env blob, and `ret_words` reaches
    /// the shim through a local bound to a literal.
    fn spawn_body(ret_words: i128) -> gossamer_mir::Body {
        use gossamer_mir::{ConstValue, Operand, Place, Rvalue, StatementKind, Terminator};
        let mut body = env_callback_body("gos_rt_spawn_ex");
        let block = &mut body.blocks[0];
        let words = gossamer_mir::Local(5);
        let span = block.span;
        block.stmts.push(gossamer_mir::Statement {
            kind: StatementKind::Assign {
                place: Place::local(words),
                rvalue: Rvalue::Use(Operand::Const(ConstValue::Int(ret_words))),
            },
            span,
        });
        if let Terminator::Call { args, .. } = &mut block.terminator {
            args.push(Operand::Copy(Place::local(words)));
            args.push(Operand::Const(ConstValue::Int(0)));
        }
        body
    }

    /// Builds a body shaped like the env-blob callback lowering: the
    /// callable's address is stored at offset 0 of the env, and the env is
    /// handed to `shim` as its second argument.
    fn env_callback_body(shim: &str) -> gossamer_mir::Body {
        use gossamer_lex::{SourceMap, Span};
        use gossamer_mir::{
            BasicBlock, BlockId, Body, ConstValue, Local, Operand, Place, Rvalue, Statement,
            StatementKind, Terminator,
        };

        let mut map = SourceMap::new();
        let span = Span::new(map.add_file("walk.gos", ""), 0, 0);
        let (root, env, addr) = (Local(1), Local(2), Local(3));
        let assign = |place: Local, rvalue: Rvalue| Statement {
            kind: StatementKind::Assign {
                place: Place::local(place),
                rvalue,
            },
            span,
        };
        Body {
            name: "main".to_string(),
            def: None,
            arity: 0,
            locals: Vec::new(),
            blocks: vec![BasicBlock {
                id: BlockId(0),
                stmts: vec![
                    assign(
                        addr,
                        Rvalue::CallIntrinsic {
                            name: "gos_fn_addr",
                            args: vec![Operand::Const(ConstValue::Str("visit".to_string()))],
                        },
                    ),
                    assign(
                        Local(4),
                        Rvalue::CallIntrinsic {
                            name: "gos_store",
                            args: vec![
                                Operand::Copy(Place::local(env)),
                                Operand::Const(ConstValue::Int(0)),
                                Operand::Copy(Place::local(addr)),
                            ],
                        },
                    ),
                ],
                terminator: Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(shim.to_string())),
                    args: vec![
                        Operand::Copy(Place::local(root)),
                        Operand::Copy(Place::local(env)),
                    ],
                    destination: Place::local(Local(0)),
                    target: None,
                },
                span,
            }],
            span,
        }
    }
}

#[cfg(test)]
mod codegen_partition_tests {
    use super::{OptProfile, body_cache_key, codegen_chunks, codegen_job_limit};
    use gossamer_lex::{SourceMap, Span};
    use gossamer_mir::{BasicBlock, BlockId, Body, ConstValue, Local, Operand, Place, Terminator};

    fn span() -> Span {
        let mut map = SourceMap::new();
        let file = map.add_file("partition.gos", "");
        Span::new(file, 0, 0)
    }

    fn body(name: &str, callee: Option<&str>) -> Body {
        let span = span();
        Body {
            name: name.to_string(),
            def: None,
            arity: 0,
            locals: Vec::new(),
            blocks: vec![BasicBlock {
                id: BlockId(0),
                stmts: Vec::new(),
                terminator: callee.map_or(Terminator::Return, |callee| Terminator::Call {
                    callee: Operand::Const(ConstValue::Str(callee.to_string())),
                    args: Vec::new(),
                    destination: Place::local(Local(0)),
                    target: None,
                }),
                span,
            }],
            span,
        }
    }

    #[test]
    fn recursive_scc_stays_in_one_codegen_chunk() {
        let bodies = vec![
            body("left", Some("right")),
            body("right", Some("left")),
            body("leaf_a", None),
            body("leaf_b", None),
        ];
        let chunks = codegen_chunks(&bodies, 3);
        let left_chunk = chunks
            .iter()
            .position(|chunk| chunk.contains(&0))
            .expect("left body assigned");
        let right_chunk = chunks
            .iter()
            .position(|chunk| chunk.contains(&1))
            .expect("right body assigned");
        assert_eq!(
            left_chunk, right_chunk,
            "recursive bodies split: {chunks:?}"
        );
        assert_eq!(
            chunks,
            codegen_chunks(&bodies, 3),
            "partition must be stable"
        );
    }

    #[test]
    fn object_cache_separates_runtime_handler_abi_from_gossamer_call_abi() {
        let handler = body("App::serve", None);
        let ordinary = body_cache_key(
            &handler,
            "x86_64-unknown-linux-gnu",
            OptProfile::Debug,
            None,
        );
        let runtime = body_cache_key(
            &handler,
            "x86_64-unknown-linux-gnu",
            OptProfile::Debug,
            Some(2),
        );
        assert_ne!(ordinary, runtime);
    }

    #[test]
    fn default_codegen_jobs_bound_small_program_memory() {
        if std::env::var_os("GOS_LLVM_JOBS").is_some() {
            return;
        }
        assert_eq!(codegen_job_limit(80), 1);
        assert_eq!(codegen_job_limit(500), 1);
        assert_eq!(codegen_job_limit(3_000), 1);
    }
}
