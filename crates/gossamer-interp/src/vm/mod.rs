#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    unsafe_op_in_unsafe_fn,
    unsafe_code,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::cognitive_complexity,
    clippy::unused_io_amount,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Register-based bytecode VM dispatch loop.
//!
//! This crate otherwise forbids `unsafe`. The exception is the
//! inner dispatch loop: register files and const pools are
//! sized at compile time from the `FnChunk`'s `register_count`,
//! `float_count`, `int_count`, and `consts.len()`, so every
//! `get_unchecked` / `get_unchecked_mut` call in this file is
//! covered by the compiler-established bound. Skipping those
//! bounds checks is the difference between a 60-second run
//! and "slower than the VM was before typed opcodes landed".
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(target_arch = "wasm32")]
use crate::jit_stub::{JitArtifact, JitFn};
use gossamer_ast::Ident;
#[cfg(not(target_arch = "wasm32"))]
use gossamer_codegen_cranelift::{JitArtifact, JitFn};
use gossamer_hir::{HirItem, HirItemKind, HirProgram};
use gossamer_mir::Body;
use gossamer_types::TyCtxt;

use crate::builtins;
use crate::bytecode;
use crate::bytecode::{FnChunk, ImmArithKind, Op};
use crate::compile::compile_fn;
use crate::jit_call;
use crate::value::{MapKey, RuntimeError, RuntimeResult, SmolStr, ThreadConfinedCell, Value};

/// Linked program: every global the VM needs to execute a call.
///
/// One comptime fold result: the source span, a raw-splice flag (set for
/// `codegen!` regions whose `String` result splices as source rather than
/// a quoted literal), and the evaluated value (`Err` when the region was
/// not compile-time-known).
pub type ComptimeFold = (gossamer_lex::Span, bool, Result<Value, String>);

/// One frame in a runtime-error call-stack snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallStackFrame {
    /// Source-level function name.
    pub function: String,
    /// Display name of the source file, when available.
    pub file: Option<String>,
    /// One-based source line, when available.
    pub line: Option<u32>,
    /// One-based source column, when available.
    pub column: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct VmCallStackFrame {
    pub(crate) function: &'static str,
    pub(crate) location: Option<crate::bytecode::SourceLocation>,
}

impl VmCallStackFrame {
    pub(crate) const fn new(function: &'static str) -> Self {
        Self {
            function,
            location: None,
        }
    }
}

/// The VM compiles HIR directly to bytecode and lowers every
/// construct natively; there is no fallback evaluator. The global
/// table holds the built-in intrinsics plus every compiled function,
/// const, and static.
pub struct Vm {
    /// Per-Vm overlay holding user-defined functions, consts, and
    /// statics. Lookups consult this first; on miss they fall back
    /// to [`Self::prelude`]. Behind `Arc` so spawned worker `Vm`s
    /// share one immutable copy. Keys are program/load-owned canonical names;
    /// dynamically assembled method spellings use [`Self::qualified_names`]
    /// and are not inserted here.
    pub(crate) globals: Arc<rustc_hash::FxHashMap<&'static str, Global>>,
    /// Whether builtin resolution on this `Vm` is subject to the
    /// compile-time capability policy. Set on the throwaway `Vm` a
    /// comptime fold builds, and inherited by any goroutine that fold
    /// spawns, so a compile-time region cannot reach a capability its
    /// `--comptime-io` level withholds by taking a detour through a
    /// method spelling or another thread.
    pub(crate) comptime_gate: Cell<bool>,
    /// Process-shared prelude of built-in callables - built once
    /// from a `OnceLock` and `Arc::clone`d into every Vm at
    /// construction. Pre-lazy: every `Vm::new` cloned all ~330
    /// entries into its own HashMap. Post-lazy: a refcount bump.
    pub(crate) prelude: Arc<rustc_hash::FxHashMap<&'static str, Global>>,
    /// Bare names of the program's own free functions. An `impl` method
    /// also registers under its bare name so a receiver whose type cannot
    /// be named still finds it; that registration must never displace a
    /// free function, which is the only thing an unqualified call can
    /// mean.
    pub(crate) free_fn_names: Arc<rustc_hash::FxHashSet<String>>,
    /// Module depth of the function that claimed each bare name. A bare call
    /// names the nearest declaration - the entry file's own item before a
    /// sibling module's - which is what the compiled tiers resolve it to.
    pub(crate) bare_fn_depth: rustc_hash::FxHashMap<&'static str, usize>,
    /// VM-owned qualified method-name cache. Dynamic `Type::method` keys are
    /// released with the VM instead of being retained by a process/thread
    /// global interner.
    pub(crate) qualified_names: RefCell<Vec<QualifiedName>>,
    /// Frame pool: reused register-file storage handed out at
    /// `run()` entry and returned on exit. Eliminates the per-
    /// call `Vec<Value>` / `Vec<f64>` / `Vec<i64>` malloc storm
    /// that dominates call-heavy programs. Stack-discipline:
    /// nested calls each pop their own buffers off the free
    /// list and push them back on return.
    pub(crate) pool: RefCell<FramePool>,
    /// Lowered MIR for the program, shared across goroutines via
    /// `Arc` so a child `Vm` can drive its own deferred JIT
    /// compile without reflowing HIR → MIR. `None` when the JIT
    /// is disabled (`gos run --no-jit` / `GOS_JIT=0`).
    /// `RefCell` so the deferred JIT can release the bodies the
    /// instant `compile_to_jit` finishes (see [`Self::jit_droppable`])
    /// rather than holding them through the whole run.
    pub(crate) mir_bodies: RefCell<Option<Arc<Vec<Body>>>>,
    /// DefId.local -> native shape index for heap enums whose values
    /// may cross the JIT boundary as raw pointers.
    pub(crate) enum_shape_defs: RefCell<Option<Arc<std::collections::HashMap<u32, u32>>>>,
    /// Strong handles for this program's native enum descriptors. The legacy
    /// index lookup table holds only `Weak`s, so dropping this program releases
    /// its descriptors instead of retaining them process-wide.
    pub(crate) enum_shape_handles: RefCell<Option<Arc<Vec<Arc<crate::value::NativeEnumShape>>>>>,
    /// DefId.local -> native struct-shape index for all-scalar user
    /// structs whose values may cross the JIT boundary as a flat
    /// field-slot block pointer.
    pub(crate) struct_shape_defs: RefCell<Option<Arc<std::collections::HashMap<u32, u32>>>>,
    /// Strong handles for this program's native scalar-struct descriptors.
    pub(crate) struct_shape_handles:
        RefCell<Option<Arc<Vec<Arc<crate::value::NativeStructShape>>>>>,
    /// Snapshot of the type context as it stood when MIR was
    /// lowered. Cranelift's `compile_to_jit` only needs `&TyCtxt`.
    /// `Arc` so spawned goroutines reuse the parent's snapshot
    /// rather than re-lowering it.
    pub(crate) tcx_snapshot: RefCell<Option<Arc<TyCtxt>>>,
    /// Names of JIT-worthy bodies that contain a loop or recursion (and
    /// that the codegen can actually lower). Their `ChunkState` starts
    /// with a hot counter of 1 so the first call reaches the JIT admission
    /// gate without waiting on a call-count threshold it would never hit.
    /// The observed-work floor still applies, avoiding compile overhead for
    /// short loops. Computed once at load, before `mir_bodies` may be
    /// released, so it survives the deferred compile that drops the bodies.
    /// Immutable promotion metadata derived during load. Worker VMs share the
    /// same allocation instead of re-walking MIR when a goroutine starts.
    pub(crate) jit_eager_names: RefCell<Arc<std::collections::HashSet<String>>>,
    /// Names a host may ask this `Vm` to call, which is what the promotion
    /// snapshot has to keep. `gos run` calls one entry, a test run calls each
    /// test, and a session that reads its next call from the user cannot say
    /// in advance - that case is spelled as an empty list, which keeps every
    /// body. A body outside the set is still callable; it simply stays on the
    /// tier that is always correct.
    pub(crate) entry_points: RefCell<Vec<String>>,
    /// Stable description of the optimized MIR/type snapshot used for JIT
    /// promotion. It keys the per-thread weak artifact cache: raw Cranelift
    /// handles never cross an OS-thread boundary, while overlapping VMs on one
    /// execution thread can reuse the same immutable code pages.
    pub(crate) jit_cache_key: RefCell<Option<Arc<str>>>,
    /// True once `load` proves the program has no goroutine spawn
    /// sites: then [`Self::try_compile_jit_lazy`] can free
    /// `mir_bodies` / `tcx_snapshot` the moment the compile lands,
    /// shrinking the live set before the allocation peak. Programs
    /// that spawn keep the Arcs so late-spawned goroutines can still
    /// inherit the MIR and tier up.
    pub(crate) jit_droppable: Cell<bool>,
    /// JIT artifact + override map filled by
    /// [`Vm::try_compile_jit_lazy`] the first time any chunk's hot
    /// counter trips on this `Vm`. Per-`Vm` (not shared across
    /// goroutines) because finalized entry handles carry raw pointers and are
    /// deliberately not `Send + Sync`.
    /// A spawned goroutine starts with an empty
    /// JIT and stay on bytecode unless their own per-`Vm` hot
    /// counter trips - which only happens for genuinely long-lived
    /// child VMs, where the per-thread compile cost amortises.
    pub(crate) jit: parking_lot::RwLock<JitState>,
    /// Hot-path fast flag: number of installed JIT overrides. When
    /// zero, `apply()` skips the `jit.read()` `RwLock` probe entirely -
    /// every call is a bytecode dispatch, and probing the `RwLock`
    /// per call costs ~6-8 ns of atomic CAS that adds up across
    /// tight recursive workloads. Updated by
    /// `try_compile_jit_lazy` once the deferred compile installs
    /// entries; only ever monotonically increases.
    pub(crate) jit_override_count: AtomicUsize,
    /// Per-`Vm` cache of marshalled `Vec<Vec<i64>>` graphs crossing the JIT
    /// boundary, keyed by source `Arc` identity so a graph reused across many
    /// native calls is marshalled once. Owned native graphs are freed when
    /// this field drops at Vm teardown (and cleared between pooled worker
    /// tasks). See [`crate::jit_call::GraphCache`].
    pub(crate) jit_graph_cache: crate::jit_call::GraphCache,
    /// Low-overhead accounting for the deferred JIT policy. The counters are
    /// per VM and use `Cell` because a VM is owned by one execution thread.
    pub(crate) jit_counters: JitCounters,
    /// Per-`Vm` cache state pinned in a never-shrinking arena. The
    /// hot dispatch loop reaches into `chunk_state_for(chunk)` once
    /// per call entry and threads `&ChunkState` through the
    /// dispatch arms. Replacing the prior shared
    /// `parking_lot::Mutex<Vec<CacheSlot>>` on `FnChunk` removes
    /// cross-goroutine cache-line bouncing while keeping the
    /// single-thread fast path lock-free (`RefCell` borrow check
    /// only). The `Box` is load-bearing: `chunk_state_for` hands
    /// out `&ChunkState` references that outlive the arena's
    /// reallocations, which only the `Box` indirection survives
    /// (a bare `Vec<ChunkState>` would move its elements on grow
    /// and invalidate every reference).
    #[allow(
        clippy::vec_box,
        reason = "Box keeps each ChunkState pinned; Vec<ChunkState> would invalidate stored &ChunkState references on grow"
    )]
    pub(crate) chunk_state_arena: RefCell<Vec<Box<ChunkState>>>,
    /// Side index into [`Self::chunk_state_arena`] keyed by
    /// `Arc::as_ptr(chunk) as usize`. The map's `&'static` lifetime
    /// is a stand-in: `chunk_state_for` casts each lookup to a
    /// borrow tied to `&self` (the arena outlives every reference
    /// it hands out). See `chunk_state_for` for the full safety
    /// argument.
    pub(crate) chunk_state_map: RefCell<HashMap<usize, &'static ChunkState>>,
    /// Single-slot last-seen-chunk cache, populated on every
    /// `chunk_state_for` lookup. Recursive / self-call patterns
    /// hit this slot before the `HashMap` probe, saving ~10 ns of
    /// hash + comparison per `apply()` call.
    pub(crate) chunk_state_last: Cell<Option<(usize, &'static ChunkState)>>,
    /// Monotonically-increasing version of [`Self::globals`]. Bumped
    /// whenever the globals map is mutated (today: only inside
    /// [`Self::load_item`] during program load; reserved for any
    /// future op that reassigns a global). Inline-cache slots stamp
    /// their resolved entry with the generation they observed and
    /// re-validate on hit so a stale slot from before a reassignment
    /// is treated as a miss instead of dispatching to the prior
    /// target. Per-`Vm`: spawned goroutines start at 1 and climb
    /// independently, mirroring the per-`Vm` `ChunkState` ownership.
    pub(crate) globals_generation: Cell<u32>,
    /// Call-stack snapshot for runtime-error diagnostics. Push on
    /// chunk entry, pop on success - on error the frame stays so
    /// `call_stack_snapshot` reports the failing chain.
    /// Names are interned `&'static str`; recursive programs do not
    /// allocate a heap String per frame.
    pub(crate) call_stack: RefCell<Vec<VmCallStackFrame>>,
    /// Current Gossamer call depth for this goroutine's VM. Incremented
    /// on every `apply` entry, decremented on return. When it reaches
    /// `MAX_CALL_DEPTH` the call is refused with `RuntimeError::StackOverflow`
    /// so unbounded mutual or direct recursion aborts instead of spinning
    /// the CPU indefinitely through heap-allocated frame allocation.
    pub(crate) call_depth: Cell<usize>,
    /// Source map published before [`Vm::load`]. The compiler resolves
    /// expression locations for runtime tracebacks whenever it is present.
    /// When runtime coverage is also enabled, statement spans additionally
    /// produce [`crate::bytecode::Op::CovHit`] instructions.
    pub(crate) source_map: Option<Arc<gossamer_lex::SourceMap>>,
    /// When set before [`Vm::load`], the loader evaluates every
    /// `comptime { ... }` block and `comptime fn` call after compiling
    /// the program and records the results in [`Self::comptime_folds`].
    /// Off by default so normal runs pay nothing.
    pub(crate) collect_comptime: Cell<bool>,
    /// Comptime evaluation results, keyed by source span: `Ok(value)`
    /// on success, `Err(message)` when the region was not
    /// compile-time-known. The `bool` marks a raw (`codegen!`) region
    /// whose `String` result splices as source rather than as a literal.
    /// Populated by `load` when [`Self::collect_comptime`] is set; drained
    /// by the CLI to splice results back into the source.
    pub(crate) comptime_folds: RefCell<Vec<ComptimeFold>>,
}

/// One lazily materialised `Type::method` spelling owned by a [`Vm`]. A compact
/// vector is intentional: method resolution only reaches this cache on an
/// inline-cache miss, and typical programs have few distinct method pairs.
/// It avoids allocating a temporary concatenated key on every probe.
pub(crate) struct QualifiedName {
    type_name: Box<str>,
    method: Box<str>,
    key: Arc<str>,
}

/// Per-`Vm` per-chunk dispatch caches. Pinned inside
/// [`Vm::chunk_state_arena`]; references handed out by
/// [`Vm::chunk_state_for`] are valid for the lifetime of the
/// owning `Vm`.
/// Memoised JIT-override resolution for a chunk. `Unresolved` until
/// the first call after a JIT install, then fixed until the next install
/// invalidates all chunk resolutions. A cached `Some` stays valid even if
/// the override map later evicts the entry because it owns the prepared
/// dispatch data.
#[derive(Clone, Default)]
pub(crate) enum JitResolve {
    /// Not yet resolved against the installed override map.
    #[default]
    Unresolved,
    /// Resolved: this chunk has no native override (stays bytecode).
    None,
    /// Resolved: pre-computed dispatch data to call through.
    Some(std::rc::Rc<crate::jit_call::Prepared>),
}

/// A point-in-time view of deferred-JIT activity for one [`Vm`].
///
/// These counters make tier-up decisions inspectable without enabling trace
/// output or changing execution. `resident_functions` is the number of native
/// dispatch entries currently retained by the VM; it is zero when promotion
/// was skipped or produced no VM-callable body.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitMetrics {
    /// Number of hot-counter expirations observed by bytecode dispatch.
    pub tier_up_requests: u64,
    /// Requests deferred because their observed bytecode work was too small.
    pub work_floor_deferrals: u64,
    /// Times this VM actually started a Cranelift compilation.
    pub compile_attempts: u64,
    /// Successful compilations that installed at least one dispatch entry.
    pub successful_compiles: u64,
    /// Native functions currently reachable through this VM's dispatch map.
    pub resident_functions: usize,
    /// Finalised artifacts dropped immediately because no body was admissible.
    pub discarded_artifacts: u64,
    /// MIR/type snapshots released after a spawn-free compilation.
    pub released_snapshots: u64,
    /// Finalized native artifacts reused from this thread's immutable cache.
    /// A reuse does not increment `compile_attempts` because Cranelift did no
    /// work for this VM.
    pub reused_artifacts: u64,
    /// Compile attempts skipped because `GOS_JIT_MAX_RSS_MB` was set and the
    /// process was already at or above that resident-memory cap.
    pub ram_skipped_compiles: u64,
    /// Artifacts rejected because installing them would exceed
    /// `GOS_JIT_MAX_CODE_BYTES` for this VM.
    pub code_size_skipped_compiles: u64,
    /// Last process RSS observed by the JIT tier-up gate, in bytes. `0` means
    /// no RSS sample was available.
    pub last_observed_rss_bytes: u64,
    /// Largest process RSS sample taken while considering or compiling a JIT
    /// artifact. This is process-wide RSS, not an estimate of code bytes.
    pub peak_observed_rss_bytes: u64,
    /// Total time spent inside Cranelift promotion compilation, in microseconds.
    pub total_compile_time_us: u64,
    /// Duration of the most recent Cranelift promotion compilation, in
    /// microseconds. A failed compile is still recorded because it consumed
    /// compiler work.
    pub last_compile_time_us: u64,
    /// Total VM-callable entries installed from promoted artifacts.
    pub promoted_functions: u64,
    /// Number of VM-callable entries installed by the latest artifact.
    pub last_promoted_functions: u64,
    /// Exact bytes Cranelift generated for successfully installed user bodies,
    /// including each function's machine code, jump tables, and constants.
    /// This is distinct from process RSS and executable-page allocation.
    pub emitted_code_bytes: u64,
    /// Exact generated-code bytes in the most recently installed artifact.
    pub last_emitted_code_bytes: u64,
    /// Lower-bound count of bytecode instructions bypassed by successful
    /// native dispatches. This counts each directly dispatched chunk; native
    /// calls made entirely inside JIT code are deliberately not guessed.
    pub saved_vm_instructions: u64,
    /// Native graph bytes currently retained by the boundary cache.
    pub graph_cache_bytes: usize,
    /// Reuses of an already-marshalled graph.
    pub graph_cache_hits: u64,
    /// Graph lookups that required marshalling.
    pub graph_cache_misses: u64,
    /// Graphs removed to stay within the byte budget.
    pub graph_cache_evictions: u64,
    /// Lower-bound estimate of MIR and shape bytes retained for deferred JIT.
    pub retained_jit_preparation_bytes: usize,
    /// Compilations rejected by the pre-Cranelift MIR complexity budget.
    pub pre_admission_skipped_compiles: u64,
}

/// Interior counters backing [`JitMetrics`]. Kept separate from `JitState`
/// so hot-counter observations do not take the JIT lock.
#[derive(Default)]
pub(crate) struct JitCounters {
    tier_up_requests: Cell<u64>,
    work_floor_deferrals: Cell<u64>,
    compile_attempts: Cell<u64>,
    successful_compiles: Cell<u64>,
    discarded_artifacts: Cell<u64>,
    released_snapshots: Cell<u64>,
    reused_artifacts: Cell<u64>,
    ram_skipped_compiles: Cell<u64>,
    code_size_skipped_compiles: Cell<u64>,
    last_observed_rss_bytes: Cell<u64>,
    peak_observed_rss_bytes: Cell<u64>,
    total_compile_time_us: Cell<u64>,
    last_compile_time_us: Cell<u64>,
    promoted_functions: Cell<u64>,
    last_promoted_functions: Cell<u64>,
    emitted_code_bytes: Cell<u64>,
    last_emitted_code_bytes: Cell<u64>,
    saved_vm_instructions: Cell<u64>,
    retained_jit_preparation_bytes: Cell<usize>,
    pre_admission_skipped_compiles: Cell<u64>,
}

impl JitCounters {
    #[inline]
    fn bump(counter: &Cell<u64>) {
        counter.set(counter.get().saturating_add(1));
    }

    #[inline]
    pub(crate) fn tier_up_requested(&self) {
        Self::bump(&self.tier_up_requests);
    }

    #[inline]
    pub(crate) fn work_floor_deferred(&self) {
        Self::bump(&self.work_floor_deferrals);
    }

    #[inline]
    pub(crate) fn compile_started(&self) {
        Self::bump(&self.compile_attempts);
    }

    #[inline]
    pub(crate) fn compile_succeeded(&self) {
        Self::bump(&self.successful_compiles);
    }

    #[inline]
    pub(crate) fn artifact_discarded(&self) {
        Self::bump(&self.discarded_artifacts);
    }

    #[inline]
    pub(crate) fn snapshots_released(&self) {
        Self::bump(&self.released_snapshots);
    }

    #[inline]
    pub(crate) fn artifact_reused(&self) {
        Self::bump(&self.reused_artifacts);
    }

    #[inline]
    pub(crate) fn ram_skipped_compile(&self, rss_bytes: u64) {
        Self::bump(&self.ram_skipped_compiles);
        self.last_observed_rss_bytes.set(rss_bytes);
    }

    #[inline]
    pub(crate) fn code_size_skipped_compile(&self) {
        Self::bump(&self.code_size_skipped_compiles);
    }

    #[inline]
    pub(crate) fn observed_rss(&self, rss_bytes: u64) {
        self.last_observed_rss_bytes.set(rss_bytes);
        self.peak_observed_rss_bytes
            .set(self.peak_observed_rss_bytes.get().max(rss_bytes));
    }

    #[inline]
    pub(crate) fn compile_finished(&self, elapsed: std::time::Duration) {
        let elapsed_us = elapsed.as_micros().try_into().unwrap_or(u64::MAX);
        self.last_compile_time_us.set(elapsed_us);
        self.total_compile_time_us
            .set(self.total_compile_time_us.get().saturating_add(elapsed_us));
    }

    #[inline]
    pub(crate) fn promoted_functions(&self, count: usize) {
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        self.last_promoted_functions.set(count);
        self.promoted_functions
            .set(self.promoted_functions.get().saturating_add(count));
    }

    #[inline]
    pub(crate) fn emitted_code_bytes(&self, count: u64) {
        self.last_emitted_code_bytes.set(count);
        self.emitted_code_bytes
            .set(self.emitted_code_bytes.get().saturating_add(count));
    }

    #[inline]
    pub(crate) fn saved_vm_instructions(&self, count: u64) {
        self.saved_vm_instructions
            .set(self.saved_vm_instructions.get().saturating_add(count));
    }

    #[inline]
    pub(crate) fn retained_preparation_bytes(&self, count: usize) {
        self.retained_jit_preparation_bytes.set(count);
    }

    #[inline]
    pub(crate) fn pre_admission_skipped_compile(&self) {
        Self::bump(&self.pre_admission_skipped_compiles);
    }
}

pub(crate) struct ChunkState {
    /// Inline-cache slots for `Op::Call` / `Op::MethodCall` sites.
    /// One slot per `cache_idx`. `RefCell` because the dispatch
    /// arms read the slot, then on miss take a brief `borrow_mut`
    /// to refill it - never held across a sub-call.
    pub(crate) call_caches: RefCell<Vec<crate::bytecode::CacheSlot>>,
    /// Adaptive-arith cache slots. The outer `Vec` is fixed at
    /// chunk-construction time so no outer cell is needed; each
    /// slot's `Cell<u8>` handles the per-shape transition.
    pub(crate) arith_caches: Vec<crate::bytecode::ArithCacheSlot>,
    /// PEP 659-style field-access cache. Indexed by the
    /// `cache_idx` field on `Op::FieldGet`. Each slot remembers
    /// the receiver's interned-type-name pointer + the offset
    /// the named field resolved to, so hot-path field reads
    /// skip the linear name scan.
    pub(crate) field_caches: Vec<crate::bytecode::FieldCacheSlot>,
    /// Tier-D2 hot counter - decremented on every call into the
    /// chunk; trips a deferred whole-program JIT compile at zero.
    /// `Cell<i32>` (single-thread mutation only - each `Vm` owns
    /// its own counter, so cross-thread atomicity is unneeded).
    pub(crate) hot_counter: Cell<i32>,
    /// Approximate bytecode work observed for this chunk on this VM.
    pub(crate) jit_observed_work: Cell<u64>,
    /// Work floor that must be reached before a hot-counter trip may
    /// instantiate the Cranelift JIT.
    pub(crate) jit_min_work: u64,
    /// Bytecode instruction count, used as the per-entry work increment.
    pub(crate) instr_count: u64,
    /// Per-chunk memoised JIT override (see [`JitResolve`]). Lets the
    /// hot path skip the shared `RwLock<JitState>` probe and the
    /// `HashMap<String>` name lookup after the first post-install call.
    pub(crate) jit_resolve: RefCell<JitResolve>,
}

impl ChunkState {
    fn new(
        call_cache_count: u16,
        arith_cache_count: u16,
        field_cache_count: u16,
        instr_count: usize,
        jit_disabled: bool,
        eager: bool,
    ) -> Self {
        let initial = if jit_disabled {
            crate::bytecode::HOT_DISABLED
        } else if eager {
            // A statically loop-bearing body compiles before its first call.
            // Entry counting cannot observe loop backedges without OSR, so a
            // dynamic work floor would otherwise defer it forever.
            1
        } else {
            crate::bytecode::hot_threshold_for(instr_count)
        };
        Self {
            call_caches: RefCell::new(vec![
                crate::bytecode::CacheSlot::default();
                call_cache_count as usize
            ]),
            arith_caches: (0..arith_cache_count)
                .map(|_| crate::bytecode::ArithCacheSlot::default())
                .collect(),
            field_caches: (0..field_cache_count)
                .map(|_| crate::bytecode::FieldCacheSlot::default())
                .collect(),
            hot_counter: Cell::new(initial),
            jit_observed_work: Cell::new(0),
            jit_min_work: if eager {
                0
            } else {
                crate::bytecode::jit_min_work_for(instr_count)
            },
            instr_count: instr_count as u64,
            jit_resolve: RefCell::new(JitResolve::Unresolved),
        }
    }
}

/// Owns the cranelift JIT state once the deferred compile has
/// run. The `artifact` keeps every code page alive; the
/// `chunk_overrides` lets `apply` route a `Global::Fn(chunk)` call
/// through native dispatch by stable chunk identity. `compiled` collapses the
/// previous `jit_attempted` flag so two goroutines tripping the
/// hot counter concurrently can't both kick a compile - the
/// first transitions `Pending → InProgress`, the others see
/// `InProgress` / `Done` / `Failed` and skip.
#[derive(Default)]
pub(crate) struct JitState {
    /// Owns the detached native allocation heap; dropped along with the Vm so
    /// the code pages outlive every reachable `JitFn` handle.
    pub(crate) artifact: Option<Rc<JitArtifact>>,
    /// Map from the bytecode chunk's stable `Arc` allocation address to the
    /// JIT entry. Impl methods have qualified JIT names
    /// (`Type::method`) but their shared bytecode chunk can still be named
    /// only `method`; pointer-keyed lookup lets `apply` find the exact
    /// promoted method without relying on collision-prone bare names.
    pub(crate) chunk_overrides: HashMap<usize, Arc<JitFn>>,
    /// State machine for the one-shot deferred compile. Once it
    /// reaches `Done` or `Failed` no thread retries.
    pub(crate) compiled: JitCompileState,
}

/// A weak, per-thread cache of finalized JIT artifacts. `JitArtifact` and its
/// raw entry pointers are deliberately not `Send`/`Sync`; a thread-local cache
/// gives overlapping VM executions reuse without pretending that Cranelift's
/// module ownership is cross-thread safe. Weak entries do not retain code
/// pages after the last VM using an artifact finishes.
struct ThreadJitArtifact {
    key: Arc<str>,
    artifact: Weak<JitArtifact>,
}

thread_local! {
    static THREAD_JIT_ARTIFACTS: RefCell<VecDeque<ThreadJitArtifact>> = const {
        RefCell::new(VecDeque::new())
    };
}

const THREAD_JIT_ARTIFACT_CACHE_CAP: usize = 8;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JitCompileState {
    /// No tier-up trigger has fired on this `Vm` yet.
    #[default]
    Pending,
    /// `compile_to_jit` is mid-flight on this thread. Other
    /// hot-counter trips on the same `Vm` skip; child VMs in
    /// other goroutines run their own per-`Vm` compile.
    InProgress,
    /// JIT artefact installed on this `Vm`.
    Done,
    /// `compile_to_jit` returned `Err`, or the JIT is disabled
    /// outright. Future hot-counter trips short-circuit.
    Failed,
}

/// The out-of-range index panic every sequence access reports, worded the way
/// the compiled tiers' bounds assert words it so a program's failure text does
/// not depend on which tier ran it.
pub(crate) fn index_oob_panic(index: i64, len: usize) -> RuntimeError {
    RuntimeError::Panic(format!(
        "vec index out of bounds: the len is {len} but the index is {index}"
    ))
}
/// Per-VM free list of register-file `Vec`s. Stack-discipline:
/// `take_*` pops a Vec sized to the requested length (or
/// allocates a fresh one when the list is empty); `give_*`
/// pushes it back on return so the next call at this depth
/// reuses the capacity.
#[derive(Default)]
pub(crate) struct FramePool {
    pub(crate) values: Vec<Vec<Value>>,
    pub(crate) floats: Vec<Vec<f64>>,
    pub(crate) ints: Vec<Vec<i64>>,
    /// Pool of `Vec<Value>` reused for `Op::Call` argument
    /// marshaling. Each call grabs one to collect args, hands
    /// it to `apply`, and the callee's `run` returns it to
    /// the pool when the args have been moved into the new
    /// frame's register file.
    pub(crate) args: Vec<Vec<Value>>,
    /// Highest `n` requested from each `take_*` since the last
    /// `shrink_to`. Used to drive exponential hysteresis: once
    /// several consecutive tasks have observed `< 50%` capacity
    /// utilisation we reclaim the excess so a single large
    /// goroutine can't permanently fatten every subsequent task
    /// on the same worker.
    pub(crate) peak_value: usize,
    pub(crate) peak_float: usize,
    pub(crate) peak_int: usize,
    pub(crate) peak_args: usize,
    /// Consecutive low-utilisation runs are intentionally tracked per
    /// buffer class. A large integer frame must not keep unrelated value,
    /// float, or call-argument buffers at its high-water mark.
    pub(crate) low_value_runs: u32,
    pub(crate) low_float_runs: u32,
    pub(crate) low_int_runs: u32,
    pub(crate) low_arg_runs: u32,
}

/// Threshold of consecutive low-utilisation `shrink_to` calls
/// before per-buffer capacity reclamation kicks in. Picked low
/// enough that a worker that just handled one big task gets its
/// buffers back to size within a few subsequent tasks, high
/// enough that a noisy mix of small and big tasks doesn't
/// thrash on the shrink path.
const HYSTERESIS_RUNS: u32 = 4;

/// Floor under which `shrink_to` will not reclaim a buffer's
/// capacity. Sized so that small short-lived tasks (think a
/// handful of arguments and a tiny number of locals) keep the
/// re-use win without the next `take_*` having to grow.
const SHRINK_FLOOR: usize = 16;

/// Free-list depth per buffer class. Only one buffer per active
/// nested call is ever in flight, so this covers realistic call
/// nesting while keeping the pool's own footprint constant for a
/// goroutine that never completes a task.
const MAX_POOLED_BUFFERS: usize = 64;

impl FramePool {
    fn take_values(&mut self, n: usize) -> Vec<Value> {
        // Fast path: pool hit. We rely on the prior owner's
        // `give_values` to have already cleared the buffer, so
        // the pop is constant-time. `resize` to the requested
        // length re-fills with `Value::Void`.
        if n > self.peak_value {
            self.peak_value = n;
        }
        let mut v = match self.values.pop() {
            Some(v) => {
                crate::profile::bump_pool_value_hit();
                v
            }
            None => {
                crate::profile::bump_pool_value_miss();
                Vec::new()
            }
        };
        v.resize(n, Value::Void);
        v
    }
    fn give_values(&mut self, mut v: Vec<Value>) {
        // Drop Arc-payload registers eagerly - otherwise the
        // pool would hold strings, arrays, and structs captive
        // for the lifetime of the VM, defeating ref-count
        // collection. clear() iterates dropping each; for a
        // 32-byte enum that's a tag dispatch + per-variant
        // Arc decrement, fast in the common Void/Int/Float case.
        v.clear();
        if self.values.len() < MAX_POOLED_BUFFERS {
            self.values.push(v);
        }
    }
    fn take_floats(&mut self, n: usize) -> Vec<f64> {
        if n > self.peak_float {
            self.peak_float = n;
        }
        let mut v = match self.floats.pop() {
            Some(v) => {
                crate::profile::bump_pool_float_hit();
                v
            }
            None => {
                crate::profile::bump_pool_float_miss();
                Vec::new()
            }
        };
        // A compiler bug must not make a typed register read uninitialized
        // memory. Chunk validation proves write-before-read, but zero-fill
        // keeps this boundary sound even if a future validator regression
        // misses an opcode. The buffers retain their capacity across calls.
        v.resize(n, 0.0);
        v
    }
    fn give_floats(&mut self, mut v: Vec<f64>) {
        v.clear();
        if self.floats.len() < MAX_POOLED_BUFFERS {
            self.floats.push(v);
        }
    }
    fn take_ints(&mut self, n: usize) -> Vec<i64> {
        if n > self.peak_int {
            self.peak_int = n;
        }
        let mut v = match self.ints.pop() {
            Some(v) => {
                crate::profile::bump_pool_int_hit();
                v
            }
            None => {
                crate::profile::bump_pool_int_miss();
                Vec::new()
            }
        };
        v.resize(n, 0);
        v
    }
    fn give_ints(&mut self, mut v: Vec<i64>) {
        v.clear();
        if self.ints.len() < MAX_POOLED_BUFFERS {
            self.ints.push(v);
        }
    }
    fn take_args(&mut self, capacity: usize) -> Vec<Value> {
        if capacity > self.peak_args {
            self.peak_args = capacity;
        }
        let mut v = match self.args.pop() {
            Some(v) => {
                crate::profile::bump_pool_arg_hit();
                v
            }
            None => {
                crate::profile::bump_pool_arg_miss();
                Vec::new()
            }
        };
        // `clear()` drops any leftovers (paranoia - `give_args`
        // already empties), then reserve so the upcoming pushes
        // don't reallocate.
        v.clear();
        v.reserve(capacity);
        v
    }
    fn give_args(&mut self, mut v: Vec<Value>) {
        v.clear();
        if self.args.len() < MAX_POOLED_BUFFERS {
            self.args.push(v);
        }
    }

    /// Drains pool buffers above `keep_per_kind`, dropping the
    /// excess to release backing capacity. Called after each
    /// goroutine task completes so a worker `Vm` does not ratchet
    /// to high-water and stay there for the rest of the program.
    ///
    /// Also applies exponential hysteresis independently to each surviving
    /// buffer class. Once a class has been below 50% utilisation for several
    /// consecutive tasks, its buffers are shrunk to
    /// `max(peak * 2, SHRINK_FLOOR)`. This preserves reuse for a noisy mix
    /// of tasks without letting one large task permanently raise the worker's
    /// memory floor.
    fn shrink_to(&mut self, keep_per_kind: usize) {
        if self.values.len() > keep_per_kind {
            self.values.truncate(keep_per_kind);
        }
        if self.floats.len() > keep_per_kind {
            self.floats.truncate(keep_per_kind);
        }
        if self.ints.len() > keep_per_kind {
            self.ints.truncate(keep_per_kind);
        }
        if self.args.len() > keep_per_kind {
            self.args.truncate(keep_per_kind);
        }

        let cap_v = self.values.iter().map(Vec::capacity).max().unwrap_or(0);
        let cap_f = self.floats.iter().map(Vec::capacity).max().unwrap_or(0);
        let cap_i = self.ints.iter().map(Vec::capacity).max().unwrap_or(0);
        let cap_a = self.args.iter().map(Vec::capacity).max().unwrap_or(0);
        update_low_util_runs(&mut self.low_value_runs, self.peak_value, cap_v);
        update_low_util_runs(&mut self.low_float_runs, self.peak_float, cap_f);
        update_low_util_runs(&mut self.low_int_runs, self.peak_int, cap_i);
        update_low_util_runs(&mut self.low_arg_runs, self.peak_args, cap_a);

        if self.low_value_runs >= HYSTERESIS_RUNS {
            let target = self.peak_value.saturating_mul(2).max(SHRINK_FLOOR);
            for buf in &mut self.values {
                if buf.capacity() > target {
                    buf.shrink_to(target);
                }
            }
            self.low_value_runs = 0;
        }
        if self.low_float_runs >= HYSTERESIS_RUNS {
            let target = self.peak_float.saturating_mul(2).max(SHRINK_FLOOR);
            for buf in &mut self.floats {
                if buf.capacity() > target {
                    buf.shrink_to(target);
                }
            }
            self.low_float_runs = 0;
        }
        if self.low_int_runs >= HYSTERESIS_RUNS {
            let target = self.peak_int.saturating_mul(2).max(SHRINK_FLOOR);
            for buf in &mut self.ints {
                if buf.capacity() > target {
                    buf.shrink_to(target);
                }
            }
            self.low_int_runs = 0;
        }
        if self.low_arg_runs >= HYSTERESIS_RUNS {
            let target = self.peak_args.saturating_mul(2).max(SHRINK_FLOOR);
            for buf in &mut self.args {
                if buf.capacity() > target {
                    buf.shrink_to(target);
                }
            }
            self.low_arg_runs = 0;
        }
        self.peak_value = 0;
        self.peak_float = 0;
        self.peak_int = 0;
        self.peak_args = 0;

        // The free-list headers themselves can grow when a deeply nested task
        // completes. Keep modest headroom but release a pathological peak.
        self.values.shrink_to(keep_per_kind.saturating_mul(2));
        self.floats.shrink_to(keep_per_kind.saturating_mul(2));
        self.ints.shrink_to(keep_per_kind.saturating_mul(2));
        self.args.shrink_to(keep_per_kind.saturating_mul(2));
    }
}

fn update_low_util_runs(runs: &mut u32, peak: usize, capacity: usize) {
    if peak.saturating_mul(2) < capacity {
        *runs = runs.saturating_add(1);
    } else {
        *runs = 0;
    }
}

/// RAII guard that lends three register-file `Vec`s out of the
/// pool for the duration of one `run()` call. On `Drop`, the
/// buffers go back to the pool - including on early returns or
/// `?` propagation from inside the dispatch loop. Without this,
/// every `?` in the loop body would have to be hand-rewritten
/// to reunite with the buffers before bubbling out.
pub(crate) struct FrameGuard<'a> {
    pub(crate) pool: &'a RefCell<FramePool>,
    pub(crate) registers: std::mem::ManuallyDrop<Vec<Value>>,
    pub(crate) floats: std::mem::ManuallyDrop<Vec<f64>>,
    pub(crate) ints: std::mem::ManuallyDrop<Vec<i64>>,
    /// Set when the real register files were moved into a
    /// [`run::SuspendedFrame`]. In that state the `ManuallyDrop` fields hold only
    /// empty placeholders left by `std::mem::take`; returning those empty Vecs
    /// to the pool poisons the LIFO free list and makes the callee reallocate
    /// on every non-tail bytecode call.
    pub(crate) suspended: bool,
}

impl<'a> FrameGuard<'a> {
    fn take(pool: &'a RefCell<FramePool>, n_val: usize, n_float: usize, n_int: usize) -> Self {
        let (registers, floats, ints) = {
            let mut p = pool.borrow_mut();
            (
                p.take_values(n_val),
                p.take_floats(n_float),
                p.take_ints(n_int),
            )
        };
        Self {
            pool,
            registers: std::mem::ManuallyDrop::new(registers),
            floats: std::mem::ManuallyDrop::new(floats),
            ints: std::mem::ManuallyDrop::new(ints),
            suspended: false,
        }
    }

    /// Re-wrap register files that were suspended across a direct bytecode
    /// call. Resumed frames still return their buffers through the ordinary
    /// per-VM pool on every normal exit.
    fn from_parts(
        pool: &'a RefCell<FramePool>,
        registers: Vec<Value>,
        floats: Vec<f64>,
        ints: Vec<i64>,
    ) -> Self {
        Self {
            pool,
            registers: std::mem::ManuallyDrop::new(registers),
            floats: std::mem::ManuallyDrop::new(floats),
            ints: std::mem::ManuallyDrop::new(ints),
            suspended: false,
        }
    }
}

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        if self.suspended {
            return;
        }
        // SAFETY: `take` runs exactly once at construction and
        // `Drop` runs exactly once at end-of-scope; the inner
        // `ManuallyDrop`s are never observed empty by anyone.
        let registers = unsafe { std::mem::ManuallyDrop::take(&mut self.registers) };
        let floats = unsafe { std::mem::ManuallyDrop::take(&mut self.floats) };
        let ints = unsafe { std::mem::ManuallyDrop::take(&mut self.ints) };
        let mut p = self.pool.borrow_mut();
        p.give_values(registers);
        p.give_floats(floats);
        p.give_ints(ints);
    }
}

impl std::fmt::Debug for Vm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Intentionally non-exhaustive: the FramePool, JIT
        // artifact, and tcx snapshot are gnarly to render and add
        // no debugging signal beyond the global names.
        f.debug_struct("Vm")
            .field("globals", &self.globals.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Entries in the global table. Visible to `bytecode::CacheSlot`
/// so inline-cache slots can hold a resolved dispatch target
/// directly - no downcast on the hit path.
#[derive(Debug, Clone)]
pub(crate) enum Global {
    Fn(Arc<FnChunk>),
    Value(Value),
    /// A `static mut` cell. The shared `Arc<Mutex<Value>>` is cloned
    /// into every spawned goroutine's `globals` map (which itself is an
    /// `Arc`), so writes from one goroutine are observable in all
    /// others; the `Mutex` makes the concurrent access sound. Reads
    /// (`LoadGlobal`) load the current value; writes (`Op::StoreStatic`)
    /// replace it.
    MutStatic(Arc<parking_lot::Mutex<Value>>),
}

/// Maximum Gossamer call frames per goroutine before `StackOverflow`.
///
/// Direct named bytecode calls live in heap-owned VM frames, so they do not
/// consume the Rust stack. A finite cap still bounds adversarial recursion's
/// register-file memory and gives programs a deterministic `GX0008` rather
/// than exhausting process memory.
const MAX_CALL_DEPTH: usize = 4_096;

/// Native-stack depth at which direct bytecode calls switch to the heap-frame
/// trampoline.
///
/// The trampoline is required for adversarial deep recursion, but shallow
/// bytecode calls are common in real workloads: binary trees, AST rewrite, and
/// ordinary helper calls rarely exceed a few dozen live language frames. Those
/// shapes were paying a full register-file suspension and resume on every
/// call. Keep a conservative native-stack budget for the fast path, then fall
/// back to suspended VM frames before stack pressure becomes material.
#[cfg(debug_assertions)]
pub(crate) const DIRECT_BYTECODE_CALL_DEPTH: usize = 16;
#[cfg(not(debug_assertions))]
pub(crate) const DIRECT_BYTECODE_CALL_DEPTH: usize = 128;

/// Maximum logical tail frames retained for diagnostics in one trampoline
/// chain. Tail calls reuse their physical VM frame, so [`MAX_CALL_DEPTH`]
/// cannot bound an unbounded tail-recursive program. This remains above the
/// 10,000-step tail-recursion regression while ensuring a malformed program
/// reaches `GX0008` instead of spinning indefinitely.
const MAX_TAIL_CALL_DEPTH: usize = 65_536;

/// Native stack reserved for every OS thread that executes the
/// bytecode VM - the main `gos-vm` thread and each goroutine worker.
///
/// Named bytecode calls now suspend heap-owned VM frames rather than nesting
/// one interpreter dispatch frame per language call. Native/JIT recursion can
/// still consume this stack, and the byte-budget guard turns exhaustion into
/// `GX0008`, but a 64 MiB reservation per main VM and worker thread was no
/// longer justified. Sixteen MiB preserves ample backend headroom while
/// cutting reserved address space by 75 percent per VM thread.
pub const VM_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

mod call_dispatch;
pub(crate) mod goroutine;
mod lifecycle;
mod native_dispatch;
mod resolve;
pub(crate) mod run;

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

/// Validates each immutable bytecode chunk before it reaches the unsafe
/// dispatch loop in [`Vm::run`]. The O(bytecode) cost is paid once at load
/// time, while a malformed chunk would otherwise turn a compiler regression
/// into release-build undefined behavior.
fn validate_chunk_for_execution(chunk: &FnChunk) -> RuntimeResult<()> {
    crate::validate::validate_chunk(chunk)
        .map_err(|e| RuntimeError::Type(format!("invalid bytecode for `{}`: {e}", chunk.name)))
}

/// Recognises `fn name(p) { intrinsic_path(p) }` (a single
/// parameter, no other statements, body is exactly one call
/// forwarding the parameter) and returns the intrinsic's
/// path segments so the VM compiler can fold `name(x)` into
/// a direct intrinsic op at every call site.
fn detect_trivial_wrapper(decl: &gossamer_hir::HirFn) -> Option<Vec<String>> {
    if decl.params.len() != 1 {
        return None;
    }
    let body = decl.body.as_ref()?;
    if !body.block.stmts.is_empty() {
        return None;
    }
    let tail = body.block.tail.as_deref()?;
    // The tail may be the call itself, or a block whose tail
    // is the call. We only inline the former shape to keep
    // the matcher simple and the wrapper table small.
    let call_expr = match &tail.kind {
        gossamer_hir::HirExprKind::Call { .. } => tail,
        gossamer_hir::HirExprKind::Block(inner) if inner.stmts.is_empty() => {
            inner.tail.as_deref()?
        }
        _ => return None,
    };
    let gossamer_hir::HirExprKind::Call { callee, args } = &call_expr.kind else {
        return None;
    };
    if args.len() != 1 {
        return None;
    }
    let gossamer_hir::HirExprKind::Path {
        segments: arg_segments,
        ..
    } = &args[0].kind
    else {
        return None;
    };
    if arg_segments.len() != 1 {
        return None;
    }
    let param_name = match &decl.params[0].pattern.kind {
        gossamer_hir::HirPatKind::Binding { name, .. } => &name.name,
        _ => return None,
    };
    if arg_segments[0].name != *param_name {
        return None;
    }
    let gossamer_hir::HirExprKind::Path { segments, .. } = &callee.kind else {
        return None;
    };
    Some(segments.iter().map(|s| s.name.clone()).collect())
}

/// Checked source indexing: an out-of-range index always panics.
fn index_get_checked(base: &Value, idx: &Value) -> RuntimeResult<Value> {
    index_get(base, idx)
}

pub(crate) fn index_value(idx: &Value) -> RuntimeResult<i64> {
    match idx {
        Value::Int(n) => Ok(*n),
        Value::Uint(n) => i64::try_from(*n).map_err(|_| {
            RuntimeError::Panic(format!(
                "index {n} exceeds the maximum supported collection size"
            ))
        }),
        _ => Err(RuntimeError::Type("index must be integer".to_string())),
    }
}

pub(crate) fn sequence_index(idx: &Value) -> RuntimeResult<usize> {
    let idx = index_value(idx)?;
    usize::try_from(idx)
        .map_err(|_| RuntimeError::Arithmetic("negative index into sequence".to_string()))
}

fn index_get(base: &Value, idx: &Value) -> RuntimeResult<Value> {
    let raw = match idx {
        Value::Int(_) | Value::Uint(_) => index_value(idx)?,
        Value::LazyIter(id) => {
            let Some((start, end, inclusive, start_open, end_open)) =
                crate::stdlib_builtins::iter::lazy_range_bounds(id.id())
            else {
                return Err(RuntimeError::Type(
                    "index range is no longer valid".to_string(),
                ));
            };
            return index_range_get(base, start, end, inclusive, start_open, end_open);
        }
        _ => return Err(RuntimeError::Type("index must be integer".to_string())),
    };
    let len = match base {
        Value::Array(items) => items.len(),
        Value::Tuple(items) => items.len(),
        Value::IntArray(d) => d.len(),
        Value::ByteArray(d) => d.len(),
        Value::InlineByteArray(d) => d.len(),
        Value::ByteVec(d) => d.len(),
        Value::FloatVec(d) => d.len(),
        Value::String(s) => s.len(),
        Value::FloatArray(fa) if fa.stride > 0 => fa.data.len() / fa.stride as usize,
        Value::FloatArray(_) => 0,
        _ => {
            return Err(RuntimeError::Type(format!(
                "value of kind `{base}` is not indexable"
            )));
        }
    };
    if raw < 0 || raw as usize >= len {
        return Err(crate::vm::index_oob_panic(raw, len));
    }
    let i = raw as usize;
    match base {
        Value::Array(items) => Ok(items[i].clone()),
        Value::Tuple(items) => Ok(items[i].clone()),
        // Rehydrate a single element into `Value::Struct` so generic
        // indexed-access code keeps working when the array was compiled to
        // flat f64 storage.
        Value::FloatArray(fa_inner) => {
            let stride = fa_inner.stride as usize;
            let base_idx = i * stride;
            let mut fields: Vec<(&'static str, Value)> =
                Vec::with_capacity(fa_inner.field_names.len());
            for (j, fname) in fa_inner.field_names.iter().enumerate() {
                fields.push((
                    crate::value::intern_type_name(fname.as_str()),
                    Value::Float(fa_inner.data[base_idx + j]),
                ));
            }
            Ok(Value::struct_(
                fa_inner.name,
                Arc::unwrap_or_clone(Arc::new(fields)),
            ))
        }
        Value::String(s) => Ok(Value::Char(
            s.char_at(i)
                .expect("string index was checked against scalar length"),
        )),
        Value::IntArray(data) => Ok(Value::Int(data[i])),
        Value::ByteArray(data) => Ok(Value::Int(i64::from(data[i]))),
        Value::InlineByteArray(data) => Ok(Value::Int(i64::from(data[i]))),
        Value::ByteVec(data) => Ok(Value::Int(i64::from(data[i]))),
        Value::FloatVec(data) => Ok(Value::Float(data[i])),
        _ => unreachable!("len computed above for this variant"),
    }
}

fn index_range_get(
    base: &Value,
    start: i64,
    end: i64,
    inclusive: bool,
    start_open: bool,
    end_open: bool,
) -> RuntimeResult<Value> {
    let len = match base {
        Value::Array(items) => items.len(),
        Value::Tuple(items) => items.len(),
        Value::IntArray(d) => d.len(),
        Value::ByteArray(d) => d.len(),
        Value::InlineByteArray(d) => d.len(),
        Value::ByteVec(d) => d.len(),
        Value::FloatVec(d) => d.len(),
        Value::String(s) => s.len(),
        Value::FloatArray(fa) if fa.stride > 0 => fa.data.len() / fa.stride as usize,
        Value::FloatArray(_) => 0,
        _ => {
            return Err(RuntimeError::Type(format!(
                "value of kind `{base}` does not support range indexing"
            )));
        }
    };
    let len_i64 = i64::try_from(len).unwrap_or(i64::MAX);
    let lo_raw = if start_open { 0 } else { start };
    let hi_raw = if end_open {
        len_i64
    } else if inclusive {
        end.saturating_add(1)
    } else {
        end
    };
    let lo = lo_raw.max(0).min(len_i64) as usize;
    let hi = hi_raw.max(lo as i64).min(len_i64) as usize;
    match base {
        Value::Array(items) => Ok(Value::Array(Arc::new(items[lo..hi].to_vec()))),
        Value::Tuple(items) => Ok(Value::Array(Arc::new(items[lo..hi].to_vec()))),
        Value::IntArray(data) => Ok(Value::IntArray(Arc::new(data[lo..hi].to_vec()))),
        Value::ByteArray(data) => Ok(Value::ByteArray(Arc::new(data[lo..hi].to_vec().into()))),
        Value::InlineByteArray(data) => Ok(Value::InlineByteArray(Arc::new(
            data[lo..hi].iter().copied().collect(),
        ))),
        Value::ByteVec(data) => Ok(Value::ByteVec(Arc::new(data[lo..hi].to_vec()))),
        Value::FloatVec(data) => Ok(Value::FloatVec(Arc::new(data[lo..hi].to_vec()))),
        Value::String(s) => {
            let piece = crate::builtins::str_substring_inline(s, lo as i64, hi as i64);
            Ok(Value::String(piece))
        }
        Value::FloatArray(rx) => {
            let Value::Array(view) = Value::FloatArray(rx.clone()).float_array_to_value_array()
            else {
                return Ok(Value::Array(Arc::new(Vec::new())));
            };
            Ok(Value::Array(Arc::new(view[lo..hi].to_vec())))
        }
        _ => unreachable!("len computed above for this variant"),
    }
}

/// Element read that drains a uniquely-owned `Array` / `Tuple` slot,
/// leaving `Value::Void` behind, and otherwise mirrors [`index_get`]
/// exactly. A shared aggregate (refcount > 1) or a non-`Array`/`Tuple`
/// base falls back to the checked cloning reader so `Op::IndexGetConsume`
/// stays bit-identical to `Op::IndexGet` on every path except the unique-owner
/// fast move.
fn index_get_consume(base: &mut Value, raw: i64) -> RuntimeResult<Value> {
    match base {
        Value::Array(arc) | Value::Tuple(arc) => {
            let len = arc.len();
            if raw < 0 || raw as usize >= len {
                return Err(crate::vm::index_oob_panic(raw, len));
            }
            let i = raw as usize;
            match std::sync::Arc::get_mut(arc) {
                Some(items) => Ok(std::mem::replace(&mut items[i], Value::Void)),
                None => Ok(arc[i].clone()),
            }
        }
        // Flat / string / unindexable bases are never drained: defer to
        // the shared cloning reader for identical results.
        other => index_get(other, &Value::Int(raw)),
    }
}

impl Vm {
    /// Builds the `TypeName::method` global-table key for a nominal receiver.
    /// Keys are VM-owned: a workload that creates many short-lived VMs no
    /// longer turns distinct dynamic method spellings into permanent process
    /// allocations.
    fn qualified_key(&self, receiver: &Value, method: &str) -> Option<Arc<str>> {
        match receiver {
            Value::Struct(inner) => Some(self.intern_qualified(inner.name.as_str(), method)),
            Value::MutCell(cell) => {
                let inner = cell.lock();
                self.qualified_key(&inner, method)
            }
            Value::Channel(_) => Some(self.intern_qualified("Channel", method)),
            Value::String(_) => Some(self.intern_qualified("String", method)),
            Value::Variant(inner)
                if matches!(inner.name.as_str(), "Some" | "None") && method == "iter" =>
            {
                Some(self.intern_qualified("Option", method))
            }
            // `Vec`-receiver methods resolve by type so a bare name shared
            // with another module's free function cannot override the builtin.
            Value::Array(_)
            | Value::IntArray(_)
            | Value::ByteArray(_)
            | Value::InlineByteArray(_)
            | Value::ByteVec(_)
            | Value::FloatVec(_)
            | Value::FloatArray(_) => Some(self.intern_qualified("Vec", method)),
            Value::LazyIter(_) => Some(self.intern_qualified("Iterator", method)),
            // A parsed document answers the `json::` query surface by
            // receiver type; without this the bare-name fallback reaches a
            // reader for a different shape and returns `None`.
            Value::Json(_) => Some(self.intern_qualified("json", method)),
            // Every map representation answers the one `Map::` surface. All
            // three are the same type to a program; the split is a storage
            // optimisation.
            Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_) => {
                Some(self.intern_qualified("Map", method))
            }
            // A tuple `impl` registers under the arity its receiver carries,
            // which is what a value in hand can be identified as when the call
            // reaches this dispatch through a type parameter.
            Value::Tuple(inner) => {
                Some(self.intern_qualified(&format!("tuple_{}", inner.len()), method))
            }
            // Scalars, so a method on one cannot be captured by a free
            // function of the same name.
            Value::Int(_) => Some(self.intern_qualified("i64", method)),
            Value::Uint(_) => Some(self.intern_qualified("u64", method)),
            Value::Float(_) => Some(self.intern_qualified("f64", method)),
            Value::Bool(_) => Some(self.intern_qualified("bool", method)),
            Value::Char(_) => Some(self.intern_qualified("char", method)),
            // An enum receiver resolves through the enum that declares the
            // variant, which the variant value itself does not carry.
            Value::Variant(inner) => {
                variant_owner(inner.name.as_str()).map(|owner| self.intern_qualified(owner, method))
            }
            Value::Weak(_) => Some(self.intern_qualified("Weak", method)),
            Value::Unit | Value::Void => None,
            Value::Closure(_)
            | Value::Builtin(_)
            | Value::Native(_)
            | Value::NativeEnum(_)
            | Value::CaptureCell(_) => None,
        }
    }

    /// Bare-name fallback for a method call whose receiver type did not name
    /// a method.
    ///
    /// The stdlib registers most of its methods under a bare name, so the
    /// fallback has to stay for them. It must never reach a *user* function,
    /// though: a user `impl` method is also registered bare (so that a
    /// receiver whose type cannot be named still finds it), and without this
    /// filter a method call on one type resolves to a same-named method on
    /// an unrelated one, or to a free function that is not a method at all.
    /// A user method is reachable through its own type's qualified name,
    /// which is where a method call should find it.
    fn lookup_builtin_method(&self, name: &str) -> Option<Global> {
        // An already-qualified name is a target the compiler resolved, not a
        // guess: honour it as written.
        if name.contains("::") {
            return self.lookup_global(name);
        }
        if self.comptime_denial(name).is_some() {
            return None;
        }
        // The prelude alone, never the program's own globals: a user function
        // sharing a builtin's bare name is a free function, and a receiver
        // that answers that name still answers with the builtin - the shadow
        // belongs to the call spelled without a receiver.
        if let Some(found) = self.prelude.get(name) {
            return Some(found.clone());
        }
        match self.globals.get(name) {
            Some(Global::Fn(_)) | None => None,
            found => found.cloned(),
        }
    }

    fn intern_qualified(&self, type_name: &str, method: &str) -> Arc<str> {
        let mut names = self.qualified_names.borrow_mut();
        if let Some(entry) = names
            .iter()
            .find(|entry| entry.type_name.as_ref() == type_name && entry.method.as_ref() == method)
        {
            return Arc::clone(&entry.key);
        }
        let mut key = String::with_capacity(type_name.len() + 2 + method.len());
        key.push_str(type_name);
        key.push_str("::");
        key.push_str(method);
        let key: Arc<str> = Arc::from(key);
        names.push(QualifiedName {
            type_name: type_name.into(),
            method: method.into(),
            key: Arc::clone(&key),
        });
        key
    }
}

/// A stable identity for a method-call receiver, used as the IC's
/// guard. Two calls with the same `TypeToken` resolve to the same
/// `Global`. Token `0` (`TAG_NONE`) means "no stable identity, do
/// not cache".
///
/// For struct / variant receivers the token is the interned
/// type-name pointer in the low bits OR'd with a per-variant tag in
/// the high byte. `intern_type_name` returns a `&'static str` whose
/// `as_ptr()` is stable across every `Value::clone` of a struct
/// with the same name, so the cache hit path is one u64 compare.
pub(crate) fn type_token(v: &Value) -> u64 {
    const TAG_NONE: u64 = 0;
    const TAG_STRUCT: u64 = 1 << 56;
    const TAG_CHANNEL: u64 = 2 << 56;
    const TAG_STRING: u64 = 3 << 56;
    const TAG_ARRAY: u64 = 4 << 56;
    const TAG_TUPLE: u64 = 5 << 56;
    const TAG_VARIANT: u64 = 6 << 56;
    const TAG_LAZY_ITER: u64 = 7 << 56;
    match v {
        Value::Struct(inner) => {
            // `inner.name` is already a globally-interned `&'static str`
            // (every `Value::struct_` routes the name through
            // `value::intern_type_name`), so its pointer is canonical and
            // stable across every clone of any instance of this type - the
            // same identity `Op::StructIs` relies on via `ptr::eq`. Use it
            // directly instead of re-hashing through a second pool.
            TAG_STRUCT | (u64::from(inner.name.id()) & 0x00FF_FFFF_FFFF_FFFF)
        }
        Value::Channel(_) => TAG_CHANNEL,
        Value::String(_) => TAG_STRING,
        Value::Array(_) | Value::FloatArray(_) | Value::IntArray(_) | Value::FloatVec(_) => {
            TAG_ARRAY
        }
        // A tuple's arity is part of its identity here: an `impl` block for a
        // tuple registers under it, so two arities reaching one call site are
        // two receivers and must not share an inline-cache slot.
        Value::Tuple(inner) => TAG_TUPLE ^ ((inner.len() as u64) << 8),
        Value::LazyIter(_) => TAG_LAZY_ITER,
        Value::Variant(inner) => {
            // Globally-interned canonical pointer (see the `Struct` arm).
            TAG_VARIANT | (u64::from(inner.name.id()) & 0x00FF_FFFF_FFFF_FFFF)
        }
        Value::MutCell(cell) => {
            let inner = cell.lock();
            type_token(&inner)
        }
        // Primitives + non-cacheable receivers fall through to the
        // slow path on every call. The IC slot stores token=0 and
        // never matches a non-zero `type_token` result.
        _ => TAG_NONE,
    }
}

/// Builds a fresh inline-cache slot from a resolved [`Global`].
/// Pulls out the raw builtin fn pointer when the global is a
/// `Value::Builtin` so the steady-state dispatch is a direct
/// indirect call rather than `match Global::Value(Value::Builtin
/// { call, .. })`. Mirrors `CPython` 3.11's specialisation of
/// `LOAD_METHOD_NO_DICT` (where the resolved `__call__` is cached
/// alongside the type-version guard).
fn fill_cache_slot(
    token: u64,
    generation: u32,
    callee_name: Option<SmolStr>,
    g: &Global,
) -> crate::bytecode::CacheSlot {
    let builtin_fn = match g {
        Global::Value(Value::Builtin(inner)) => Some(inner.call),
        _ => None,
    };
    let fn_chunk = match g {
        Global::Fn(chunk) => Some(Arc::clone(chunk)),
        Global::Value(_) | Global::MutStatic(_) => None,
    };
    crate::bytecode::CacheSlot {
        type_token: token,
        callee_name,
        generation,
        builtin_fn,
        fn_chunk,
    }
}

/// Stable identity for an `Op::Call` callee - keyed by the
/// resolved-name string for `Value::String` callees (the bytecode
/// VM's idiom for "named global function"). Other callee shapes
/// (closures, builtins-passed-as-values, etc.) return `0` so the
/// IC slot stays cold and the slow path is taken every time -
/// those receivers don't have a stable identity worth caching.
pub(crate) fn call_token(v: &Value) -> u64 {
    match v {
        // Cache slots retain and compare the exact `SmolStr` spelling, so all
        // named calls can share this class token without collisions or a
        // process-global `&'static str` interner.
        Value::String(_) => NAMED_CALL_TOKEN,
        _ => 0,
    }
}

/// Cache identity shared by all named calls. Exact spelling remains part of
/// the cache key, so unrelated globals cannot collide.
pub(crate) const NAMED_CALL_TOKEN: u64 = 1 << 56;

/// Binary arithmetic that dispatches on operand kind. Ints use
/// `int_fn`; floats use `float_fn`; mixed kinds promote to
/// float. String concat (Add on two strings) is handled at the
/// caller before this runs.
fn bin_arith(
    a: &Value,
    b: &Value,
    int_fn: fn(i64, i64) -> i64,
    float_fn: fn(f64, f64) -> f64,
    label: &str,
) -> RuntimeResult<Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(int_fn(*x, *y))),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(float_fn(*x, *y))),
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float(float_fn(*x as f64, *y))),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(float_fn(*x, *y as f64))),
        // String concat is handled separately in the dispatch
        // loop (Add on two strings).
        _ => Err(RuntimeError::Type(format!(
            "{label} on unsupported value kinds"
        ))),
    }
}

/// Tier C2 - classify `(a, b)` into one of the `ARITH_*` shape
/// constants for the purposes of inline-cache specialisation.
/// Anything outside the four narrow shapes (II, FF, SS, II/FF
/// mixed) ends up [`bytecode::ARITH_POLYMORPHIC`].
fn classify_pair(a: &Value, b: &Value, allow_string: bool) -> u8 {
    match (a, b) {
        (Value::Int(_), Value::Int(_)) => bytecode::ARITH_INT_INT,
        (Value::Float(_), Value::Float(_)) => bytecode::ARITH_FLOAT_FLOAT,
        (Value::String(_), Value::String(_)) if allow_string => bytecode::ARITH_STRING_STRING,
        _ => bytecode::ARITH_POLYMORPHIC,
    }
}

/// Updates the shape slot for the arith op at `cache_idx` after
/// observing one operand pair. Sticky transitions: any move off
/// the initial specialised shape goes straight to
/// [`bytecode::ARITH_POLYMORPHIC`] so subsequent dispatches skip
/// the re-observation cost.
fn record_shape(state: &ChunkState, cache_idx: u16, observed: u8) {
    let slot = &state.arith_caches[cache_idx as usize];
    let cur = slot.shape.get();
    if cur == bytecode::ARITH_UNKNOWN {
        slot.shape.set(observed);
    } else if cur != observed {
        slot.shape.set(bytecode::ARITH_POLYMORPHIC);
    }
}

/// Specialised dispatch for `Op::AddInt`. The hot path is a
/// single discriminant check; the cold path observes the operand
/// shape and quickens the slot. String concatenation lives here
/// because `+` is the only Gossamer operator that overloads onto
/// `Value::String`.
#[inline]
fn adaptive_add(
    state: &ChunkState,
    cache_idx: u16,
    shape: u8,
    a: &Value,
    b: &Value,
) -> RuntimeResult<Value> {
    match shape {
        bytecode::ARITH_INT_INT => {
            if let (Value::Int(x), Value::Int(y)) = (a, b) {
                return Ok(Value::Int(x.wrapping_add(*y)));
            }
        }
        bytecode::ARITH_FLOAT_FLOAT => {
            if let (Value::Float(x), Value::Float(y)) = (a, b) {
                return Ok(Value::Float(*x + *y));
            }
        }
        bytecode::ARITH_STRING_STRING => {
            if let (Value::String(x), Value::String(y)) = (a, b) {
                let mut s = String::with_capacity(x.len() + y.len());
                s.push_str(x);
                s.push_str(y);
                return Ok(Value::String(s.into()));
            }
        }
        _ => {}
    }
    record_shape(state, cache_idx, classify_pair(a, b, true));
    if let (Value::String(x), Value::String(y)) = (a, b) {
        let mut s = String::with_capacity(x.len() + y.len());
        s.push_str(x);
        s.push_str(y);
        return Ok(Value::String(s.into()));
    }
    bin_arith(a, b, i64::wrapping_add, |x, y| x + y, "addition")
}

/// Specialised dispatch for `Op::SubInt` / `Op::MulInt`. Sub and
/// Mul share the shape of binary numeric ops, so the helper
/// takes the int/float operations and a label for the polymorphic
/// fallback path's error message.
#[allow(
    clippy::too_many_arguments,
    reason = "lowering plumbing - every parameter is needed by the surrounding pipeline"
)]
#[inline]
fn adaptive_arith(
    state: &ChunkState,
    cache_idx: u16,
    shape: u8,
    a: &Value,
    b: &Value,
    int_fn: fn(i64, i64) -> i64,
    float_fn: fn(f64, f64) -> f64,
    label: &str,
) -> RuntimeResult<Value> {
    match shape {
        bytecode::ARITH_INT_INT => {
            if let (Value::Int(x), Value::Int(y)) = (a, b) {
                return Ok(Value::Int(int_fn(*x, *y)));
            }
        }
        bytecode::ARITH_FLOAT_FLOAT => {
            if let (Value::Float(x), Value::Float(y)) = (a, b) {
                return Ok(Value::Float(float_fn(*x, *y)));
            }
        }
        _ => {}
    }
    record_shape(state, cache_idx, classify_pair(a, b, false));
    bin_arith(a, b, int_fn, float_fn, label)
}

/// Specialised dispatch for `Op::DivInt`. Integer divide-by-zero
/// surfaces as a runtime error, so the int-int hot path still
/// has to branch on `y == 0`. Float division never errors.
#[inline]
fn adaptive_div(
    state: &ChunkState,
    cache_idx: u16,
    shape: u8,
    a: &Value,
    b: &Value,
) -> RuntimeResult<Value> {
    match shape {
        bytecode::ARITH_INT_INT => {
            if let (Value::Int(x), Value::Int(y)) = (a, b) {
                if *y == 0 {
                    return Err(RuntimeError::Panic("divide by zero".to_string()));
                }
                return Ok(Value::Int(x.wrapping_div(*y)));
            }
        }
        bytecode::ARITH_FLOAT_FLOAT => {
            if let (Value::Float(x), Value::Float(y)) = (a, b) {
                return Ok(Value::Float(*x / *y));
            }
        }
        _ => {}
    }
    record_shape(state, cache_idx, classify_pair(a, b, false));
    div_int(a, b)
}

/// Specialised dispatch for `Op::RemInt`. Mirrors [`adaptive_div`].
#[inline]
fn adaptive_rem(
    state: &ChunkState,
    cache_idx: u16,
    shape: u8,
    a: &Value,
    b: &Value,
) -> RuntimeResult<Value> {
    match shape {
        bytecode::ARITH_INT_INT => {
            if let (Value::Int(x), Value::Int(y)) = (a, b) {
                if *y == 0 {
                    return Err(RuntimeError::Panic("divide by zero".to_string()));
                }
                return Ok(Value::Int(x.wrapping_rem(*y)));
            }
        }
        bytecode::ARITH_FLOAT_FLOAT => {
            if let (Value::Float(x), Value::Float(y)) = (a, b) {
                return Ok(Value::Float(*x % *y));
            }
        }
        _ => {}
    }
    record_shape(state, cache_idx, classify_pair(a, b, false));
    rem_int(a, b)
}

fn div_int(a: &Value, b: &Value) -> RuntimeResult<Value> {
    match (a, b) {
        (Value::Int(_), Value::Int(0)) => Err(RuntimeError::Panic("divide by zero".to_string())),
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x.wrapping_div(*y))),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x / y)),
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float((*x as f64) / y)),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(x / (*y as f64))),
        _ => Err(RuntimeError::Type(
            "division on non-numeric values".to_string(),
        )),
    }
}

fn rem_int(a: &Value, b: &Value) -> RuntimeResult<Value> {
    match (a, b) {
        (Value::Int(_), Value::Int(0)) => Err(RuntimeError::Panic("divide by zero".to_string())),
        (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x.wrapping_rem(*y))),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x % y)),
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float((*x as f64) % y)),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(x % (*y as f64))),
        _ => Err(RuntimeError::Type("modulo on non-int values".to_string())),
    }
}

fn neg(v: &Value) -> RuntimeResult<Value> {
    match v {
        // `i64::MIN` negates to itself (wraps), matching the typed-int
        // negation path and the documented integer-overflow semantics.
        Value::Int(i) => Ok(Value::Int(i.wrapping_neg())),
        Value::Float(f) => Ok(Value::Float(-*f)),
        _ => Err(RuntimeError::Type("neg on non-numeric".to_string())),
    }
}

fn not(v: &Value) -> RuntimeResult<Value> {
    match v {
        Value::Bool(b) => Ok(Value::Bool(!b)),
        _ => Err(RuntimeError::Type("not on non-bool".to_string())),
    }
}

fn compare(
    a: &Value,
    b: &Value,
    order: std::cmp::Ordering,
    or_equal: bool,
) -> RuntimeResult<Value> {
    // Scalars compare by natural order; tuples and vec/array values compare
    // lexicographically (`value_ordering` recurses and auto-derefs cells).
    let result = value_ordering(a, b)?;
    let matches = if or_equal {
        result == order || result == std::cmp::Ordering::Equal
    } else {
        result == order
    };
    Ok(Value::Bool(matches))
}

/// Cheap, conservative HIR gate for programs that may contain a recursive
/// call graph. The authoritative MIR admission pass decides afterward. This
/// gate includes impl methods, trait defaults, and calls between distinct
/// bodies so it cannot hide mutual recursion from that pass.
fn has_jit_eligible_fn(program: &HirProgram) -> bool {
    let mut bodies = Vec::new();
    for item in &program.items {
        match &item.kind {
            HirItemKind::Fn(decl) => bodies.push(decl),
            HirItemKind::Impl(decl) => bodies.extend(decl.methods.iter()),
            HirItemKind::Trait(decl) => bodies.extend(decl.methods.iter()),
            HirItemKind::Const(_) | HirItemKind::Static(_) | HirItemKind::Adt(_) => {}
        }
    }
    let names: rustc_hash::FxHashSet<&str> =
        bodies.iter().map(|decl| decl.name.name.as_str()).collect();
    // One walk per body, collecting the names it calls, rather than one walk
    // per (body, name) pair: the second shape costs the square of the
    // program's function count and dominates the load it guards.
    let mut called: rustc_hash::FxHashSet<&str> = rustc_hash::FxHashSet::default();
    bodies.iter().any(|decl| {
        decl.body.as_ref().is_some_and(|body| {
            if hir_block_has_slice_pattern(&body.block) {
                return false;
            }
            if hir_block_has_loop(&body.block) {
                return true;
            }
            called.clear();
            hir_block_direct_callees(&body.block, &mut called);
            !called.is_disjoint(&names)
        })
    })
}

/// Collects the single-segment names `block` calls directly.
///
/// Recursion goes through the shared child walker, which has one arm per
/// expression kind, so a new kind cannot silently acquire an unvisited edge.
fn hir_block_direct_callees<'a>(
    block: &'a gossamer_hir::HirBlock,
    out: &mut rustc_hash::FxHashSet<&'a str>,
) {
    gossamer_hir::for_each_child_expr_in_block(block, &mut |expr| {
        hir_expr_direct_callees(expr, out);
    });
}

/// [`hir_block_direct_callees`] for one expression and everything below it.
fn hir_expr_direct_callees<'a>(
    expr: &'a gossamer_hir::HirExpr,
    out: &mut rustc_hash::FxHashSet<&'a str>,
) {
    if let gossamer_hir::HirExprKind::Call { callee, .. } = &expr.kind
        && let gossamer_hir::HirExprKind::Path { segments, .. } = &callee.kind
        && segments.len() == 1
    {
        out.insert(segments[0].name.as_str());
    }
    gossamer_hir::for_each_child_expr(expr, &mut |child| {
        hir_expr_direct_callees(child, out);
    });
}

/// Inlining a `static mut` accessor into another body leaves the original MIR
/// body as a second accessor. The JIT and VM currently use separate backing
/// cells, so preserving the original call graph lets the admission pass keep
/// every accessor on the same tier.
fn jit_bodies_access_mut_static(bodies: &[gossamer_mir::Body]) -> bool {
    use gossamer_mir::{Rvalue, StatementKind};
    bodies.iter().any(|body| {
        body.blocks.iter().any(|block| {
            block.stmts.iter().any(|stmt| {
                matches!(
                    stmt.kind,
                    StatementKind::Assign {
                        rvalue: Rvalue::StaticLoad(_),
                        ..
                    } | StatementKind::StaticStore { .. }
                )
            })
        })
    })
}

/// Names of bodies that must remain on bytecode because their slice-pattern
/// failure path is not yet faithfully represented by native lowering. Native
/// admission removes only these bodies; unrelated loops can still promote.
fn jit_slice_pattern_body_names(program: &HirProgram) -> std::collections::HashSet<String> {
    program
        .items
        .iter()
        .filter_map(|item| match &item.kind {
            HirItemKind::Fn(decl)
                if decl
                    .body
                    .as_ref()
                    .is_some_and(|body| hir_block_has_slice_pattern(&body.block)) =>
            {
                Some(decl.name.name.clone())
            }
            _ => None,
        })
        .collect()
}

fn hir_block_has_slice_pattern(block: &gossamer_hir::HirBlock) -> bool {
    block.stmts.iter().any(|stmt| match &stmt.kind {
        gossamer_hir::HirStmtKind::Let { init, .. } => {
            init.as_ref().is_some_and(hir_expr_has_slice_pattern)
        }
        gossamer_hir::HirStmtKind::Expr { expr, .. } | gossamer_hir::HirStmtKind::Defer(expr) => {
            hir_expr_has_slice_pattern(expr)
        }
        gossamer_hir::HirStmtKind::Item(item) => match &item.kind {
            HirItemKind::Fn(decl) => decl
                .body
                .as_ref()
                .is_some_and(|body| hir_block_has_slice_pattern(&body.block)),
            _ => false,
        },
    }) || block
        .tail
        .as_deref()
        .is_some_and(hir_expr_has_slice_pattern)
}

fn hir_pat_has_slice_pattern(pat: &gossamer_hir::HirPat) -> bool {
    use gossamer_hir::HirPatKind as P;
    match &pat.kind {
        P::Slice { .. } => true,
        P::Tuple(parts) | P::Variant { fields: parts, .. } | P::Or(parts) => {
            parts.iter().any(hir_pat_has_slice_pattern)
        }
        P::Struct { fields, .. } => fields.iter().any(|field| {
            field
                .pattern
                .as_ref()
                .is_some_and(hir_pat_has_slice_pattern)
        }),
        P::Ref { inner, .. } | P::At { sub: inner, .. } => hir_pat_has_slice_pattern(inner),
        P::Wildcard | P::Binding { .. } | P::Literal(_) | P::Rest | P::Range { .. } => false,
    }
}

fn hir_expr_has_slice_pattern(expr: &gossamer_hir::HirExpr) -> bool {
    use gossamer_hir::HirExprKind as K;
    match &expr.kind {
        K::Match { scrutinee, arms } => {
            hir_expr_has_slice_pattern(scrutinee)
                || arms.iter().any(|arm| {
                    hir_pat_has_slice_pattern(&arm.pattern)
                        || arm.guard.as_ref().is_some_and(hir_expr_has_slice_pattern)
                        || hir_expr_has_slice_pattern(&arm.body)
                })
        }
        K::Call { callee, args } => {
            hir_expr_has_slice_pattern(callee) || args.iter().any(hir_expr_has_slice_pattern)
        }
        K::MethodCall { receiver, args, .. } => {
            hir_expr_has_slice_pattern(receiver) || args.iter().any(hir_expr_has_slice_pattern)
        }
        K::Field { receiver, .. } | K::TupleIndex { receiver, .. } => {
            hir_expr_has_slice_pattern(receiver)
        }
        K::Index { base, index } => {
            hir_expr_has_slice_pattern(base) || hir_expr_has_slice_pattern(index)
        }
        K::Unary { operand, .. } | K::Cast { value: operand, .. } => {
            hir_expr_has_slice_pattern(operand)
        }
        K::Binary { lhs, rhs, .. } => {
            hir_expr_has_slice_pattern(lhs) || hir_expr_has_slice_pattern(rhs)
        }
        K::Assign { place, value } => {
            hir_expr_has_slice_pattern(place) || hir_expr_has_slice_pattern(value)
        }
        K::If {
            condition,
            then_branch,
            else_branch,
        } => {
            hir_expr_has_slice_pattern(condition)
                || hir_expr_has_slice_pattern(then_branch)
                || else_branch
                    .as_deref()
                    .is_some_and(hir_expr_has_slice_pattern)
        }
        K::Loop { body, .. } => hir_expr_has_slice_pattern(body),
        K::While {
            condition, body, ..
        } => hir_expr_has_slice_pattern(condition) || hir_expr_has_slice_pattern(body),
        K::Block(block) => hir_block_has_slice_pattern(block),
        K::Closure { body, .. } => hir_expr_has_slice_pattern(body),
        K::LiftedClosure { captures, .. } => captures.iter().any(hir_expr_has_slice_pattern),
        K::Tuple(elems) => elems.iter().any(hir_expr_has_slice_pattern),
        K::Array(arr) => match arr {
            gossamer_hir::HirArrayExpr::List(elems) => elems.iter().any(hir_expr_has_slice_pattern),
            gossamer_hir::HirArrayExpr::Repeat { value, count } => {
                hir_expr_has_slice_pattern(value) || hir_expr_has_slice_pattern(count)
            }
        },
        K::Range { start, end, .. } => {
            start.as_deref().is_some_and(hir_expr_has_slice_pattern)
                || end.as_deref().is_some_and(hir_expr_has_slice_pattern)
        }
        K::Return(value) | K::Break { value, .. } => {
            value.as_deref().is_some_and(hir_expr_has_slice_pattern)
        }
        K::Select { arms } => arms.iter().any(|arm| {
            let op_has_pattern = match &arm.op {
                gossamer_hir::HirSelectOp::Recv { pattern, channel } => {
                    hir_pat_has_slice_pattern(pattern) || hir_expr_has_slice_pattern(channel)
                }
                gossamer_hir::HirSelectOp::Send { channel, value } => {
                    hir_expr_has_slice_pattern(channel) || hir_expr_has_slice_pattern(value)
                }
                gossamer_hir::HirSelectOp::Default => false,
            };
            op_has_pattern || hir_expr_has_slice_pattern(&arm.body)
        }),
        K::Path { .. } | K::Literal(_) | K::Continue { .. } | K::Placeholder => false,
    }
}

fn hir_block_has_loop(block: &gossamer_hir::HirBlock) -> bool {
    block.stmts.iter().any(|stmt| match &stmt.kind {
        gossamer_hir::HirStmtKind::Let { init, .. } => init.as_ref().is_some_and(hir_expr_has_loop),
        gossamer_hir::HirStmtKind::Expr { expr, .. } | gossamer_hir::HirStmtKind::Defer(expr) => {
            hir_expr_has_loop(expr)
        }
        gossamer_hir::HirStmtKind::Item(item) => match &item.kind {
            HirItemKind::Fn(decl) => decl
                .body
                .as_ref()
                .is_some_and(|body| hir_block_has_loop(&body.block)),
            _ => false,
        },
    }) || block.tail.as_deref().is_some_and(hir_expr_has_loop)
}

fn hir_expr_has_loop(expr: &gossamer_hir::HirExpr) -> bool {
    use gossamer_hir::HirExprKind as K;
    match &expr.kind {
        K::Loop { .. } | K::While { .. } => true,
        K::Call { callee, args } => hir_expr_has_loop(callee) || args.iter().any(hir_expr_has_loop),
        K::MethodCall { receiver, args, .. } => {
            hir_expr_has_loop(receiver) || args.iter().any(hir_expr_has_loop)
        }
        K::Field { receiver, .. } | K::TupleIndex { receiver, .. } => hir_expr_has_loop(receiver),
        K::Index { base, index } => hir_expr_has_loop(base) || hir_expr_has_loop(index),
        K::Unary { operand, .. } | K::Cast { value: operand, .. } => hir_expr_has_loop(operand),
        K::Binary { lhs, rhs, .. } => hir_expr_has_loop(lhs) || hir_expr_has_loop(rhs),
        K::Assign { place, value } => hir_expr_has_loop(place) || hir_expr_has_loop(value),
        K::If {
            condition,
            then_branch,
            else_branch,
        } => {
            hir_expr_has_loop(condition)
                || hir_expr_has_loop(then_branch)
                || else_branch.as_deref().is_some_and(hir_expr_has_loop)
        }
        K::Match { scrutinee, arms } => {
            hir_expr_has_loop(scrutinee)
                || arms.iter().any(|arm| {
                    arm.guard.as_ref().is_some_and(hir_expr_has_loop)
                        || hir_expr_has_loop(&arm.body)
                })
        }
        K::Block(block) => hir_block_has_loop(block),
        K::Closure { body, .. } => hir_expr_has_loop(body),
        K::LiftedClosure { captures, .. } => captures.iter().any(hir_expr_has_loop),
        K::Tuple(elems) => elems.iter().any(hir_expr_has_loop),
        K::Array(arr) => match arr {
            gossamer_hir::HirArrayExpr::List(elems) => elems.iter().any(hir_expr_has_loop),
            gossamer_hir::HirArrayExpr::Repeat { value, count } => {
                hir_expr_has_loop(value) || hir_expr_has_loop(count)
            }
        },
        K::Range { start, end, .. } => {
            start.as_deref().is_some_and(hir_expr_has_loop)
                || end.as_deref().is_some_and(hir_expr_has_loop)
        }
        K::Return(value) | K::Break { value, .. } => {
            value.as_deref().is_some_and(hir_expr_has_loop)
        }
        K::Select { arms } => arms.iter().any(|arm| {
            let op_has_loop = match &arm.op {
                gossamer_hir::HirSelectOp::Recv { channel, .. } => hir_expr_has_loop(channel),
                gossamer_hir::HirSelectOp::Send { channel, value } => {
                    hir_expr_has_loop(channel) || hir_expr_has_loop(value)
                }
                gossamer_hir::HirSelectOp::Default => false,
            };
            op_has_loop || hir_expr_has_loop(&arm.body)
        }),
        K::Path { .. } | K::Literal(_) | K::Continue { .. } | K::Placeholder => false,
    }
}

/// Conservative scan for any goroutine-spawn site reachable from the
/// loaded program: the `spawn(f)` JoinHandle builtin appears as a call
/// referencing the `spawn` global. A spawn-free program hands its MIR to
/// no child Vm, so
/// the deferred JIT can free it early. Erring toward "spawns" only
/// forfeits a goroutine tier-up optimization, never correctness.
fn program_has_spawn_sites(globals: &rustc_hash::FxHashMap<&'static str, Global>) -> bool {
    globals
        .values()
        .any(|g| matches!(g, Global::Fn(chunk) if chunk_has_spawn(chunk)))
}

fn chunk_has_spawn(chunk: &crate::bytecode::FnChunk) -> bool {
    chunk.globals.iter().any(|name| &**name == "spawn")
        || chunk
            .closure_protos
            .iter()
            .any(|proto| chunk_has_spawn(&proto.chunk))
}

fn truthy(v: &Value) -> RuntimeResult<bool> {
    // 0.7.0 flag::Cell auto-deref so `if flags.verbose { … }`
    // works without `*`.
    let deref = auto_deref_cell(v);
    let value = deref.as_ref().unwrap_or(v);
    match value {
        Value::Bool(b) => Ok(*b),
        _ => Err(RuntimeError::Type(
            "branch condition must be bool".to_string(),
        )),
    }
}

pub(crate) fn values_equal(a: &Value, b: &Value) -> bool {
    // 0.7.0 flag::Cell auto-deref. Either side being a `__Cell`
    // struct unwraps to its current value before comparison, so
    // `flags.output == "text"` works without the user typing `*`.
    let a_deref = auto_deref_cell(a);
    let b_deref = auto_deref_cell(b);
    let a_ref = a_deref.as_ref().unwrap_or(a);
    let b_ref = b_deref.as_ref().unwrap_or(b);
    match (a_ref, b_ref) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Char(x), Value::Char(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        // Enum variants (incl. Option / Result) compare structurally: same
        // variant, same payload. The compiled tiers already do this; without
        // it `Some(5) == Some(5)` was `false` on the VM, and a derived enum's
        // `==` (which routes to a field-wise `eq`) couldn't be matched.
        (Value::Variant(va), Value::Variant(vb)) => {
            va.name == vb.name
                && va.fields.len() == vb.fields.len()
                && va
                    .fields
                    .iter()
                    .zip(vb.fields.iter())
                    .all(|(x, y)| values_equal(x, y))
        }
        // Struct-payload enum variants (`Rect { w, h }`) are stored as
        // `Value::Struct` keyed by the variant name, so a derived enum's
        // field-wise `==` reaches here: same name, same fields by name+value.
        (Value::Struct(sa), Value::Struct(sb)) => {
            // A set is a handle struct over a registry slot, so comparing its
            // fields would compare handle identity. Two sets are equal when
            // they hold the same elements, as they are in Rust.
            if sa.name == sb.name
                && let Some(xa) = crate::stdlib_builtins::set::set_entries_of(a_ref)
                && let Some(xb) = crate::stdlib_builtins::set::set_entries_of(b_ref)
            {
                return xa.len() == xb.len() && xa.keys().all(|key| xb.contains_key(key));
            }
            sa.name == sb.name
                && sa.fields.len() == sb.fields.len()
                && sa
                    .fields
                    .iter()
                    .zip(sb.fields.iter())
                    .all(|((na, x), (nb, y))| na == nb && values_equal(x, y))
        }
        // Tuples and vec/array values compare structurally, element-wise.
        // The compiled tiers route these to a field-wise desugar / runtime
        // helper; without this `(1, 2) == (1, 2)` and `[1, 2] == [1, 2]`
        // were identity-`false` on the VM.
        (Value::Tuple(xa), Value::Tuple(xb)) => {
            xa.len() == xb.len() && xa.iter().zip(xb.iter()).all(|(x, y)| values_equal(x, y))
        }
        (Value::FloatArray(xa), Value::FloatArray(xb)) => {
            xa.stride == xb.stride && xa.data == xb.data
        }
        // A map is equal to a map holding the same entries, whatever order
        // they went in and whichever of the typed representations each side
        // happens to carry - the same contract Rust's `HashMap` has.
        (
            Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_),
            Value::Map(_) | Value::IntMap(_) | Value::StrIntMap(_),
        ) => {
            let xa = map_entries(a_ref);
            let xb = map_entries(b_ref);
            xa.len() == xb.len()
                && xa
                    .iter()
                    .all(|(key, value)| xb.get(key).is_some_and(|other| values_equal(value, other)))
        }
        // Native enum handles compare structurally through the boxed
        // representation (rare fallback; derived `==` routes through
        // match dispatch instead).
        (Value::NativeEnum(a), _) => values_equal(&crate::value::native_enum_to_variant(a), b_ref),
        (_, Value::NativeEnum(b)) => values_equal(a_ref, &crate::value::native_enum_to_variant(b)),
        _ => match (seq_elements(a_ref), seq_elements(b_ref)) {
            (Some(xa), Some(xb)) => {
                xa.len() == xb.len() && xa.iter().zip(xb.iter()).all(|(x, y)| values_equal(x, y))
            }
            _ => false,
        },
    }
}

/// Materializes a map's entries, normalizing the typed `IntMap` / `StrIntMap`
/// representations onto the generic one, so two maps holding the same entries
/// compare equal whichever representation each of them settled into.
fn map_entries(v: &Value) -> rustc_hash::FxHashMap<crate::value::MapKey, Value> {
    use crate::value::MapKey;
    match v {
        Value::Map(m) => m
            .lock()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        Value::IntMap(m) => m
            .lock()
            .iter()
            .map(|(key, value)| (MapKey::Int(*key), Value::Int(*value)))
            .collect(),
        Value::StrIntMap(m) => m
            .lock()
            .iter()
            .map(|(key, value)| (MapKey::Str(key.clone()), Value::Int(*value)))
            .collect(),
        _ => rustc_hash::FxHashMap::default(),
    }
}

/// Materializes the elements of a Vec/array-like value (normalizing the
/// specialized `IntArray` / `FloatVec` representations) for structural
/// comparison. Returns `None` for non-sequence values; tuples are handled
/// by their own match arm so a tuple never compares equal to a vec.
fn seq_elements(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::Array(xs) => Some(xs.as_ref().clone()),
        Value::IntArray(xs) => Some(xs.iter().map(|&i| Value::Int(i)).collect()),
        Value::FloatVec(xs) => Some(xs.iter().map(|&f| Value::Float(f)).collect()),
        // A byte sequence has four packed spellings, and which one a value
        // carries follows how it was built rather than what it means: a
        // literal, a slice borrow, and a runtime read of the same bytes are
        // one value and compare as one.
        Value::ByteArray(xs) => Some(xs.iter().map(|&b| Value::Int(i64::from(b))).collect()),
        Value::InlineByteArray(xs) => Some(xs.iter().map(|&b| Value::Int(i64::from(b))).collect()),
        Value::ByteVec(xs) => Some(xs.iter().map(|&b| Value::Int(i64::from(b))).collect()),
        _ => None,
    }
}

/// Total-ish structural ordering over comparable values: scalars by their
/// natural order, tuples and vec/array values lexicographically (recursing
/// element-wise). Errors on NaN and on genuinely incomparable kinds.
pub(crate) fn value_ordering(a: &Value, b: &Value) -> RuntimeResult<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let a_deref = auto_deref_cell(a);
    let b_deref = auto_deref_cell(b);
    let a_ref = a_deref.as_ref().unwrap_or(a);
    let b_ref = b_deref.as_ref().unwrap_or(b);
    match (a_ref, b_ref) {
        (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Ok(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => x
            .partial_cmp(y)
            .ok_or_else(|| RuntimeError::Arithmetic("NaN comparison".to_string())),
        (Value::Char(x), Value::Char(y)) => Ok(x.cmp(y)),
        (Value::String(x), Value::String(y)) => Ok(x.cmp(y)),
        (Value::Tuple(xa), Value::Tuple(xb)) => seq_ordering(xa, xb),
        (Value::Struct(xa), Value::Struct(xb)) if xa.name == "Reverse" && xb.name == "Reverse" => {
            let x = xa
                .fields
                .iter()
                .find_map(|(name, value)| (*name == "0").then_some(value));
            let y = xb
                .fields
                .iter()
                .find_map(|(name, value)| (*name == "0").then_some(value));
            match (x, y) {
                (Some(x), Some(y)) => value_ordering(y, x),
                _ => Ok(Ordering::Equal),
            }
        }
        (Value::Struct(xa), Value::Struct(xb)) if xa.name == xb.name => {
            let x_values: Vec<_> = xa.fields.iter().map(|(_, value)| value.clone()).collect();
            let y_values: Vec<_> = xb.fields.iter().map(|(_, value)| value.clone()).collect();
            seq_ordering(&x_values, &y_values)
        }
        // An enum orders by variant rank - the position its variant was
        // declared at, which is the discriminant every compiled tier
        // compares - and then by payload, field by field.
        (Value::Variant(xa), Value::Variant(xb)) => {
            let rank = |name: &str| crate::builtins::variant_rank_of(name);
            match (rank(xa.name.as_str()), rank(xb.name.as_str())) {
                (Some(ra), Some(rb)) if ra != rb => Ok(ra.cmp(&rb)),
                _ if xa.name != xb.name => Ok(xa.name.as_str().cmp(xb.name.as_str())),
                _ => seq_ordering(&xa.fields, &xb.fields),
            }
        }
        _ => match (seq_elements(a_ref), seq_elements(b_ref)) {
            (Some(xa), Some(xb)) => seq_ordering(&xa, &xb),
            _ => Err(RuntimeError::Type(
                "comparison on unsupported kinds".to_string(),
            )),
        },
    }
}

/// Lexicographic ordering of two element slices: the first non-equal pair
/// decides; on a shared prefix the shorter sequence is less.
fn seq_ordering(xa: &[Value], xb: &[Value]) -> RuntimeResult<std::cmp::Ordering> {
    for (x, y) in xa.iter().zip(xb.iter()) {
        let o = value_ordering(x, y)?;
        if o != std::cmp::Ordering::Equal {
            return Ok(o);
        }
    }
    Ok(xa.len().cmp(&xb.len()))
}

/// Unwraps a `__Cell` flag handle to its current backing value.
/// Returns `Some(unwrapped)` for `__Cell` structs, `None` for any
/// other shape so callers can keep the original borrow.
pub(crate) fn auto_deref_cell(v: &Value) -> Option<Value> {
    let Value::Struct(inner) = v else {
        return None;
    };
    if inner.name != "__Cell" {
        return None;
    }
    let mut set_id: u64 = 0;
    let mut flag_name = String::new();
    for (ident, val) in &inner.fields {
        if (*ident) == "__set_id"
            && let Value::Int(n) = val
        {
            set_id = *n as u64;
        }
        if (*ident) == "__flag_name"
            && let Value::String(s) = val
        {
            flag_name = s.as_str().to_string();
        }
    }
    crate::builtins::resolve_cell(set_id, &flag_name)
}

/// Native struct-field read.
fn field_get(receiver: &Value, name: &str) -> RuntimeResult<Value> {
    if let Value::Struct(inner) = receiver {
        if let Some((_, v)) = inner.fields.iter().find(|(ident, _)| (**ident) == name) {
            return Ok(v.clone());
        }
        return Err(RuntimeError::Type(format!(
            "unknown field `{name}` on struct value"
        )));
    }
    Err(RuntimeError::Type(format!(
        "field access on non-struct `{receiver}`"
    )))
}

/// Native struct-field write. Mutates the register's struct
/// in place using `Arc::make_mut`, so aliasing values see the
/// new state only if they share the same `Arc` (value-aggregate
/// semantics) when the receiver is a local (register) binding.
fn field_set(receiver: &mut Value, name: &str, new_value: Value) -> RuntimeResult<()> {
    let Value::Struct(struct_arc) = receiver else {
        return Err(RuntimeError::Type(format!(
            "cannot assign to field `{name}` on non-struct `{receiver}`"
        )));
    };
    let struct_inner = Arc::make_mut(struct_arc);
    let slots = &mut struct_inner.fields;
    for (ident, slot) in slots.iter_mut() {
        if (*ident) == name {
            *slot = new_value;
            return Ok(());
        }
    }
    let mut grown = std::mem::take(slots).into_vec();
    grown.push((crate::value::intern_type_name(name), new_value));
    *slots = crate::value::StructFields::new(grown);
    Ok(())
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::bytecode::Op;
    use crate::validate::validate_chunk;

    fn hir_for_jit_admission(source: &str) -> HirProgram {
        let mut map = gossamer_lex::SourceMap::new();
        let file = map.add_file("jit-admission.gos", source.to_string());
        let (mut sf, parse_diagnostics) = gossamer_parse::parse_source_file(source, file);
        assert!(parse_diagnostics.is_empty(), "parse: {parse_diagnostics:?}");
        let (resolutions, resolve_diagnostics) = gossamer_resolve::resolve_source_file(&sf);
        let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &resolutions);
        assert!(
            resolve_diagnostics.is_empty(),
            "resolve: {resolve_diagnostics:?}"
        );
        let mut tcx = gossamer_types::TyCtxt::new();
        let (table, type_diagnostics) =
            gossamer_types::typecheck_source_file(&sf, &resolutions, &mut tcx);
        assert!(
            type_diagnostics.is_empty(),
            "typecheck: {type_diagnostics:?}"
        );
        gossamer_hir::lower_source_file(&sf, &resolutions, &table, &mut tcx)
    }

    fn empty_chunk(register_count: u16) -> FnChunk {
        FnChunk {
            name: "vm_test",
            arity: 0,
            register_count,
            float_count: 0,
            int_count: 0,
            instrs: Vec::new(),
            instruction_locations: Vec::new(),
            wide_ops: Vec::new(),
            consts: vec![Value::Int(0)],
            f64_consts: Vec::new(),
            i64_consts: Vec::new(),
            globals: Vec::new(),
            shape_names: Vec::new(),
            call_cache_count: 0,
            arith_cache_count: 0,
            field_cache_count: 0,
            mut_ref_params: Vec::new(),
            i64_params: Vec::new(),
            closure_protos: Vec::new(),
            select_arms: Vec::new(),
        }
    }

    #[test]
    fn execution_validation_accepts_well_formed_bytecode() {
        let mut chunk = empty_chunk(2);
        chunk.instrs.push(Op::LoadConst { dst: 0, idx: 0 });
        chunk.instrs.push(Op::Return { value: 0 });
        assert!(validate_chunk(&chunk).is_ok());
        assert!(validate_chunk_for_execution(&chunk).is_ok());
    }

    #[test]
    fn execution_validation_rejects_malformed_bytecode() {
        let mut chunk = empty_chunk(2);
        chunk.instrs.push(Op::Move { dst: 99, src: 0 });
        let err = validate_chunk_for_execution(&chunk).expect_err("must reject");
        match err {
            RuntimeError::Type(msg) => assert!(msg.contains("invalid bytecode")),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn packed_byte_arrays_and_vectors_keep_index_and_slice_semantics() {
        let fixed = Value::ByteArray(Arc::new(vec![1, 2, 3].into()));
        let growable = Value::ByteVec(Arc::new(vec![4, 5, 6]));

        assert!(matches!(
            index_get(&fixed, &Value::Int(1)),
            Ok(Value::Int(2))
        ));
        assert!(matches!(
            index_get(&growable, &Value::Int(2)),
            Ok(Value::Int(6))
        ));
        assert!(matches!(
            index_range_get(&fixed, 1, 3, false, false, false),
            Ok(Value::ByteArray(values)) if values[..] == [2, 3]
        ));
        assert!(matches!(
            index_range_get(&growable, 0, 2, false, false, false),
            Ok(Value::ByteVec(values)) if values.as_slice() == [4, 5]
        ));
        assert!(index_get(&fixed, &Value::Int(3)).is_err());
        assert!(index_get(&growable, &Value::Int(-1)).is_err());
    }

    #[test]
    fn vec_push_promotes_inline_byte_storage_without_dropping_values() {
        let mut receiver =
            Value::InlineByteArray(Arc::new(smallvec::SmallVec::<[u8; 1024]>::default()));

        for value in [3, 43, 83, 123, 163, 203] {
            run::vec_push_value(&mut receiver, Value::Int(value));
        }

        assert!(matches!(
            receiver,
            Value::ByteVec(values) if values.as_slice() == [3, 43, 83, 123, 163, 203]
        ));
    }

    #[test]
    fn loop_entry_tiering_spends_its_admission_counter_on_first_call() {
        let eager = ChunkState::new(0, 0, 0, 128, false, true);
        assert_eq!(
            eager.hot_counter.get(),
            1,
            "a loop-bearing chunk must reach JIT admission on its first call"
        );
        let ordinary = ChunkState::new(0, 0, 0, 128, false, false);
        assert!(ordinary.hot_counter.get() > 1);
    }

    #[test]
    fn qualified_name_cache_is_owned_by_its_vm() {
        let weak = {
            let vm = Vm::new();
            let receiver = Value::struct_("RequestScopedType", Vec::new());
            let first = vm
                .qualified_key(&receiver, "very_dynamic_method")
                .expect("struct receiver has a qualified key");
            let second = vm
                .qualified_key(&receiver, "very_dynamic_method")
                .expect("same receiver has a qualified key");
            assert!(Arc::ptr_eq(&first, &second));
            assert_eq!(vm.qualified_names.borrow().len(), 1);
            let weak = Arc::downgrade(&first);
            drop(first);
            drop(second);
            // The cache retains the spelling during the VM's lifetime.
            assert!(weak.upgrade().is_some());
            weak
        };
        // Regression: this used to be an immortal thread-local `Box::leak`.
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn jit_hir_admission_conservatively_finds_user_call_graphs() {
        assert!(
            !has_jit_eligible_fn(&hir_for_jit_admission("fn main() -> i64 { 42i64 }")),
            "straight-line programs must not retain JIT MIR state"
        );
        assert!(has_jit_eligible_fn(&hir_for_jit_admission(
            "fn main() -> i64 { let mut n = 0i64\nwhile n < 2i64 { n += 1i64 }\nn }"
        )));
        assert!(has_jit_eligible_fn(&hir_for_jit_admission(
            "fn f(n: i64) -> i64 { if n == 0i64 { 0i64 } else { f(n - 1i64) } } fn main() -> i64 { f(2i64) }"
        )));
        assert!(has_jit_eligible_fn(&hir_for_jit_admission(
            "fn even(n: i64) -> bool { if n == 0i64 { true } else { odd(n - 1i64) } } fn odd(n: i64) -> bool { if n == 0i64 { false } else { even(n - 1i64) } } fn main() -> bool { even(4i64) }"
        )));
        assert!(
            !has_jit_eligible_fn(&hir_for_jit_admission(
                "fn main() -> i64 { let mut n = 0i64\nwhile n < 2i64 { n += 1i64 }\nlet xs: Vec<i64> = Vec::from([1i64])\nmatch xs { [x, ..] => x, _ => 0i64 } }"
            )),
            "slice-pattern programs must stay on bytecode until native lowering preserves failed-arm control flow"
        );
    }
}

/// The enum that declares `variant`, when the program declared one.
///
/// A `Value::Variant` carries only the variant's own name, so the declaring
/// enum is looked up rather than stored on every value. `Some` / `None` /
/// `Ok` / `Err` are built in and answer their own types.
fn variant_owner(variant: &str) -> Option<&'static str> {
    match variant {
        "Some" | "None" => Some("Option"),
        "Ok" | "Err" => Some("Result"),
        _ => crate::builtins::variant_owner_of(variant),
    }
}
