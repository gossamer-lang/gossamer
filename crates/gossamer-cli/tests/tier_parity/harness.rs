// Tier parity gate - VM, Cranelift JIT, LLVM release, LLVM debug.
//
// Every `.gos` source under `examples/` and
// `feature-testing-examples/` is run in the bytecode VM, the forced
// Cranelift JIT, and the LLVM AOT release binary, and the captured
// stdout / exit code must match. A justified subset also runs through
// the LLVM AOT *debug* binary, whose MIR profile, `opt`/`llc` levels,
// and integer-overflow semantics differ from release. The harness is
// the single source of truth for cross-tier behaviour: a regression in
// any backend turns this suite red.
//
// Examples needing CLI args, stdin, or running an HTTP server
// carry a row in `SPECS` describing the fixture. Server-style
// examples are bounded with a hard 60 s wall clock cap so a
// regression that hangs a tier cannot stall CI.
//
// `llvm_strict_lower_group_N` builds every spec with `gos build
// --release` on its own, surfacing an LLVM lowering gap (a hard build
// error) distinct from an output-level parity failure.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

// Ceiling for a single fixture's tier run, sized to catch a genuine hang
// (an infinite loop lowers to the same never-returns shape) while tolerating
// a saturated machine: under `cargo test --workspace` this walk runs beside
// every other crate's tests, so a fixture that builds and runs in well under a
// second standalone (e.g. an `-O3` monomorphising build) can still be starved
// for tens of seconds. Five minutes leaves ample headroom for the load without
// letting a real hang run unbounded.
const PER_RUN_TIMEOUT: Duration = Duration::from_mins(5);

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn fresh_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-parity-{pid}-{n}-{tag}",
        pid = std::process::id(),
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Vm,
    /// `gos run` with `GOS_JIT=0`: the bytecode VM with no native
    /// promotion at all. The default `Vm` tier JITs at the usual
    /// threshold, so this is the only column that proves the pure
    /// interpreter - the tier `comptime` folds on.
    Bytecode,
    Cranelift,
    Llvm,
    /// `gos build` without `--release`: a different MIR optimisation
    /// profile, `opt -O1` + `llc -O0` instead of `-O3`, and panicking
    /// (rather than wrapping) integer overflow.
    LlvmDebug,
}

impl Tier {
    fn label(self) -> &'static str {
        match self {
            Tier::Vm => "vm",
            Tier::Bytecode => "bytecode",
            Tier::Cranelift => "cranelift",
            Tier::Llvm => "llvm",
            Tier::LlvmDebug => "llvm-debug",
        }
    }
}

struct Spec {
    /// Path relative to the workspace root.
    path: &'static str,
    /// Args appended after the source on `gos`, or passed
    /// directly to the compiled binary.
    args: &'static [&'static str],
    /// Stdin to feed to every tier's run.
    stdin: &'static [u8],
    /// Stdout is non-deterministic; compare line multisets only.
    nondeterministic: bool,
    /// Allow non-zero exit (must still match across tiers).
    allow_nonzero: bool,
    /// Skip parity entirely; the VM still has to run cleanly.
    skip_parity: Option<&'static str>,
    /// Skip everything (including the VM run) with a reason.
    skip_all: Option<&'static str>,
    /// Skip the debug-AOT tier only, with the reason the fixture is
    /// expected to read differently there (debug builds panic on
    /// integer overflow where release wraps).
    skip_debug_aot: Option<&'static str>,
    /// HTTP-server fixture: spawn, sleep `boot_ms`, send a probe,
    /// kill, compare the probe response across tiers.
    server: Option<ServerFixture>,
}

#[derive(Clone, Copy)]
struct ServerFixture {
    /// Wait this long after launch before issuing the probe.
    boot_ms: u64,
    /// Probe path, e.g. `/health`.
    probe_path: &'static str,
}

const fn spec(path: &'static str) -> Spec {
    Spec {
        path,
        args: &[],
        stdin: &[],
        nondeterministic: false,
        allow_nonzero: false,
        skip_parity: None,
        skip_all: None,
        skip_debug_aot: None,
        server: None,
    }
}

const SPECS: &[Spec] = &[
    // --- examples/ ---
    spec("examples/archive_zip.gos"),
    spec("examples/big_numbers.gos"),
    spec("examples/collection_patterns.gos"),
    spec("examples/compress_demo.gos"),
    spec("examples/crypto_hashing.gos"),
    spec("examples/derive.gos"),
    spec("examples/edge_nan_propagation.gos"),
    spec("examples/encoding_codecs.gos"),
    spec("examples/generic_struct.gos"),
    spec("examples/json_structs.gos"),
    spec("examples/semicolon_separators.gos"),
    spec("examples/tuples.gos"),
    spec("examples/vec_literals.gos"),
    spec("examples/binary_search.gos"),
    spec("examples/map_hashable_keys.gos"),
    spec("examples/bubble_sort.gos"),
    spec("examples/caesar_cipher.gos"),
    spec("examples/defer_cleanup.gos"),
    Spec {
        args: &[
            "--name",
            "jane",
            "--port",
            "9000",
            "--verbose",
            "alpha",
            "beta",
        ],
        ..spec("examples/cli_args.gos")
    },
    // A trait implemented for every kind of receiver a program can name. A
    // scalar carries its own type to the call, so several scalar impls of one
    // trait pick by receiver; every other type reaches a method as a handle.
    spec("feature-testing-examples/trait_impl_receiver_types.gos"),
    // A built-in type's own surface answers a name it carries, and an `impl`
    // block answers one it does not, so which body a call reaches follows from
    // the receiver's type rather than from which spelling was written last.
    spec("feature-testing-examples/builtin_impl_precedence.gos"),
    // A `&self` method reads its receiver the same way whichever spelling the
    // body uses: `*self` is the explicit load and a bare `self` is the same
    // read written shorter, so an operator, a comparison, and a method call on
    // `self` all answer what the receiver names rather than where it lives.
    spec("feature-testing-examples/ref_self_receiver_reads.gos"),
    // Character and byte reads over the same content. `s[i]` counts Unicode
    // scalars and `s.byte_at(i)` counts bytes, so a string outside ASCII
    // answers two different sequences, and a read past the first index block
    // starts from a recorded offset rather than from the content's start.
    spec("feature-testing-examples/string_scan_index.gos"),
    // A generic reaches its call site as a copy specialised for the types it
    // was called with: its element reads, its register classes, and the
    // callable it takes all follow the instantiation rather than an opaque
    // parameter slot.
    spec("feature-testing-examples/generic_specialisation.gos"),
    spec("feature-testing-examples/triple_quoted_strings.gos"),
    // A field-less struct is the zero-slot aggregate: it compares, orders,
    // renders, keys a map, and pads a tuple element identically on every
    // tier, whether it is spelled `Marker` or `Empty {}`.
    spec("feature-testing-examples/unit_struct_values.gos"),
    // `env::vars()` crosses the C-ABI as a Map<String, String>. The
    // fixture reads back only names it set itself, so the transcript is
    // host-independent whatever else the environment carries.
    spec("feature-testing-examples/env_vars.gos"),
    // `break` / `continue` inside a loop over a map's pairs. The
    // map-pairs loop is lowered to its own counter walk, so it registers
    // the same break and continue targets the generic sequence loop does.
    spec("feature-testing-examples/map_loop_control_flow.gos"),
    // Assigning a whole element into a Vec of tuples or structs: the
    // element is a block of slots, not one word, and its reference-counted
    // children are handed over with the copy.
    spec("feature-testing-examples/vec_tuple_element_assign.gos"),
    // A closure called through its value coerces its arguments the way
    // a named callee does: a fixed array reaching a `&[T]` parameter
    // becomes a borrowing view rather than a flat buffer (#219).
    spec("feature-testing-examples/closure_slice_param.gos"),
    // Callback shorthands: a std free function named in value position
    // and a `$`-headed projection both stand for the closure that calls
    // them, so every tier sees the same closure.
    spec("feature-testing-examples/callback_shorthands.gos"),
    // A closure capture keeps the type of the value it holds, whatever
    // expression reaches it and whatever the closure returns. The env
    // slot is reference-counted by that type on the compiled tiers.
    spec("feature-testing-examples/closure_capture_types.gos"),
    // A callable's `Result` / `Option` return crosses the indirect call
    // through its env blob as the two-word carrier the compiled body
    // answers, rather than as the first of those two words.
    spec("feature-testing-examples/callable_carrier_return.gos"),
    spec("feature-testing-examples/combinator_element_crossings.gos"),
    spec("feature-testing-examples/const_array_length.gos"),
    spec("feature-testing-examples/vec_eager_combinator_methods.gos"),
    spec("feature-testing-examples/combinator_element_kinds.gos"),
    spec("feature-testing-examples/debug_impl_dispatch.gos"),
    spec("feature-testing-examples/debugfmt_nested_adts.gos"),
    spec("feature-testing-examples/display_impl_dispatch.gos"),
    spec("feature-testing-examples/jit_admission_shapes.gos"),
    spec("feature-testing-examples/sequence_method_compilability.gos"),
    spec("feature-testing-examples/unit_main_goroutine_drain.gos"),
    // Structured concurrency. Both fixtures print only after the cohort
    // they describe has finished, so their transcripts are determined
    // even though the work inside them is concurrent.
    spec("feature-testing-examples/cohort_basics.gos"),
    spec("feature-testing-examples/cohort_cancel.gos"),
    spec("feature-testing-examples/cohort_join_outcome.gos"),
    spec("examples/structured_concurrency.gos"),
    spec("feature-testing-examples/jit_map_local_promotion.gos"),
    // The same promotion for a map keyed by a tuple, struct, String-bearing
    // tuple, or fixed array: the content-hashing key path is native, so a hot
    // body holding one compiles instead of pinning its whole call tree to
    // bytecode.
    spec("feature-testing-examples/jit_aggregate_key_map_promotion.gos"),
    spec("feature-testing-examples/string_append_self_consuming.gos"),
    // `to_string` and `join` render through the same formatter `{}` uses, for
    // every element shape either reaches.
    spec("feature-testing-examples/display_rendering.gos"),
    // `zip` pairs its sequences in the order the call writes them, and each
    // half keeps its own element type.
    spec("feature-testing-examples/zip_pair_elements.gos"),
    // A `Vec<String>` membership result computed beside a range-`for` in
    // the same body survives the loop. The JIT is the tier at risk: the
    // counted loop and the earlier call compete for the same frame slots.
    spec("feature-testing-examples/vec_contains_before_range_loop.gos"),
    // A user `impl` method wins over a builtin of the same name. An enum
    // value carries only its variant name at run time, so the receiver's
    // type has to come from the call site.
    spec("feature-testing-examples/enum_method_dispatch_and_generics.gos"),
    spec("feature-testing-examples/enum_tuple_payload_collections.gos"),
    spec("feature-testing-examples/enum_carrier_payload.gos"),
    spec("feature-testing-examples/lazy_iter_element_classes.gos"),
    spec("feature-testing-examples/container_impl_and_literals.gos"),
    spec("feature-testing-examples/container_in_tuple_array_vec.gos"),
    spec("feature-testing-examples/array_repeat_container_elements.gos"),
    spec("feature-testing-examples/user_type_named_box.gos"),
    spec("feature-testing-examples/carrier_unwrap_or_else.gos"),
    spec("feature-testing-examples/char_escape_literals.gos"),
    spec("feature-testing-examples/by_value_container_semantics.gos"),
    // An indexed carrier store hands the sequence its own share of the
    // payload, so a later allocation cannot land on a block an element
    // still names.
    spec("feature-testing-examples/carrier_element_store_ownership.gos"),
    // A carrier payload no flat tag names renders through the descriptor
    // walk, which measures the slots each element spans.
    spec("feature-testing-examples/carrier_payload_debug_shapes.gos"),
    // Every goroutine blocks at some point here; a pending handoff is
    // progress, so none of it reads as a deadlock.
    spec("feature-testing-examples/channel_progress_not_deadlock.gos"),
    // Waiters outnumber the scheduler's workers, so the group only
    // completes if a wait parks its goroutine instead of holding a worker.
    spec("feature-testing-examples/waitgroup_many_waiters.gos"),
    // A by-reference parameter reaches the caller's own storage from every
    // call site, while a by-value one still takes its own copy.
    spec("feature-testing-examples/mut_ref_aggregate_args.gos"),
    // A fixed-array parameter is a flat block of slots on every tier, so a
    // promoted body indexes it the same way the interpreter does and a
    // `&mut` one writes through to the caller's array.
    spec("feature-testing-examples/fixed_array_params.gos"),
    // A cancelled context ends a sleep and a group wait at once, and each
    // reports which of the two outcomes it saw.
    spec("feature-testing-examples/context_aware_waits.gos"),
    // Cancellation, inheritance, deadlines, done-channel readiness, and a
    // cancellation-aware receive answer the same on every tier, whether the
    // context node lives in the interpreter's registry or the runtime's.
    spec("feature-testing-examples/context_lifecycle.gos"),
    // Reflection over a struct, a tuple struct, an enum, and a generic type
    // at two instantiations, all folded during compilation.
    spec("feature-testing-examples/typeinfo_enums_and_generics.gos"),
    // A nominal alias is a checker-only distinction over an unchanged
    // runtime value, so every tier must produce the representation's
    // behaviour and the identical output.
    spec("feature-testing-examples/opaque_nominal_alias.gos"),
    // Profiles are rendered by one implementation in the runtime, so the
    // shape assertions hold on every tier even though the sample counts
    // behind them differ run to run.
    spec("feature-testing-examples/pprof_profiles.gos"),
    // Recursion that genuinely consumes stack reports the same GX0008 and
    // the same exit status whether the VM refused the call or the guard
    // page caught the frame.
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/stack_overflow_parity.gos")
    },
    // Copy-update carries every unnamed field from the base and leaves the
    // base usable, including through String and Vec fields.
    spec("feature-testing-examples/struct_copy_update.gos"),
    // Fan-in, fan-out, pipeline, and many-channel shapes at counts that hold
    // far more workers than the pool starts with.
    spec("feature-testing-examples/concurrency_stress_shapes.gos"),
    // Lazy adapters answer with an iterator and terminals materialise, from
    // a Range receiver as much as an Iterator one.
    spec("feature-testing-examples/iterator_lazy_model.gos"),
    // A `&self` / `&mut self` impl on a primitive receives an address, from a
    // local, an element, a loop binding, or a resolved type parameter.
    spec("feature-testing-examples/trait_impl_primitive_receiver.gos"),
    // An identity update inside a loop keeps the accumulator and the loop
    // itself, through plain, `if`, `match`, `Option`, and enum arms.
    spec("feature-testing-examples/identity_update_in_loop.gos"),
    // The `std::iter` free functions hand back what they declare, including
    // the pair-splitting, element-typed, and accumulator-first shapes.
    spec("feature-testing-examples/iter_free_function_contracts.gos"),
    // `iter()` answers with an iterator on every collection that offers one;
    // `to_vec` is the spelling that materialises.
    spec("feature-testing-examples/collection_iter_contracts.gos"),
    // The `std::fs` and `std::env` surface a program can rely on regardless
    // of host. Registered here so the CI matrix runs them on Linux x64,
    // Linux arm64, macOS arm64, and Windows x64.
    spec("feature-testing-examples/stdlib_fs_portable.gos"),
    spec("feature-testing-examples/stdlib_env_portable.gos"),
    // The channel contract: rendezvous, capacity, close-then-drain, and
    // `select` arm order with a non-blocking `default`.
    spec("feature-testing-examples/channel_semantics_conformance.gos"),
    spec("feature-testing-examples/keyword_and_default_arguments.gos"),
    spec("examples/concurrency.gos"),
    spec("examples/control_flow.gos"),
    spec("examples/data_structures.gos"),
    spec("examples/digit_sum.gos"),
    spec("examples/environment.gos"),
    spec("examples/errors.gos"),
    // Entry-point `Err` must print to stderr and exit nonzero identically on
    // every tier (not silently succeed on `gos` while `gos build` exits 1).
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/entry_result_err.gos")
    },
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/entry_toplevel_err.gos")
    },
    // `fs::read_to_string` on a missing path must return `Err` on every tier,
    // not a silent `Ok("")` on the native tier.
    spec("feature-testing-examples/fs_read_to_string_missing.gos"),
    spec("examples/factorial.gos"),
    spec("examples/fibonacci.gos"),
    spec("examples/file_io.gos"),
    spec("examples/fizz_buzz.gos"),
    spec("examples/fnv_hash.gos"),
    spec("examples/function_piping.gos"),
    spec("examples/gcd.gos"),
    Spec {
        nondeterministic: true,
        skip_parity: Some(
            "goroutine completion count differs across tiers under scheduling pressure",
        ),
        ..spec("examples/go_spawn.gos")
    },
    Spec {
        args: &["needle"],
        stdin: b"alpha line\nneedle hidden here\nanother needle\nclosing\n",
        ..spec("examples/grep.gos")
    },
    spec("examples/hello_world.gos"),
    spec("examples/json_derive_test.gos"),
    spec("examples/line_count.gos"),
    spec("examples/linked_list.gos"),
    // Lists the live working directory and prints each entry's mtime, so the
    // output depends on filesystem state that differs between the sequential
    // per-tier runs (running `gos` writes a `.gos-cache`, mtimes advance) -
    // it cannot be a stable cross-tier stdout comparison. Still runs on the VM
    // for the crash check; only the parity diff is skipped.
    Spec {
        skip_parity: Some(
            "lists the live cwd with per-entry mtimes; output varies run-to-run and between tiers",
        ),
        ..spec("examples/list_dir.gos")
    },
    spec("examples/mime_demo.gos"),
    spec("examples/netip_demo.gos"),
    spec("examples/os_user_demo.gos"),
    spec("examples/prime_check.gos"),
    spec("examples/range_sum.gos"),
    spec("examples/regex.gos"),
    spec("examples/reverse_string.gos"),
    spec("examples/shapes.gos"),
    spec("examples/sleep_demo.gos"),
    spec("examples/temperature.gos"),
    Spec {
        skip_parity: Some(
            "fn main is empty stub - coverage comes from `gos test examples/testing.gos`",
        ),
        ..spec("examples/testing.gos")
    },
    spec("examples/toml_demo.gos"),
    spec("examples/url_escape_demo.gos"),
    Spec {
        // v4/v7 produce fresh random / time-ordered values each run;
        // exit code is 0 and the format checks (lengths, validity,
        // normalize, simple) deterministic across tiers - but the
        // raw stdout bytes differ run-to-run.
        nondeterministic: true,
        ..spec("examples/uuid_demo.gos")
    },
    spec("examples/vowel_count.gos"),
    Spec {
        server: Some(ServerFixture {
            boot_ms: 800,
            probe_path: "/health",
        }),
        ..spec("examples/web_server.gos")
    },
    spec("examples/word_count.gos"),
    // --- feature-testing-examples/ ---
    // `os::args()` must hand back owned, refcounted gos strings: cloning
    // one arg while others are live must not corrupt any of them. The
    // first arg ("Qwen3.6-35B") is held while the rest are cloned in a
    // loop, the classic shape that exposed raw-argv-pointer corruption.
    Spec {
        args: &["Qwen3.6-35B", "a", "b", "c", "d"],
        ..spec("feature-testing-examples/os_args_clone_roundtrip.gos")
    },
    // The arguments belong to the process, so a goroutine reads the list the
    // main goroutine reads. Held per thread they would answer empty on every
    // goroutine but the first, and would do it on one tier and not the others.
    Spec {
        args: &["alpha", "beta", "gamma"],
        ..spec("feature-testing-examples/program_args_in_goroutine.gos")
    },
    // Phase 7A: the compiled tiers inline the `Vec<(i64,f64)>` scalar-projection
    // get (`table[j].1`) and `buf.set_byte` on the call route; the LCG-driven
    // probe sum must match the VM's opaque-call path bit-for-bit.
    spec("feature-testing-examples/p7_fasta_probe.gos"),
    // Phase 7B: `*out += format!("{}", n)` on an enum-payload binding fuses to a
    // direct append, and `?`-heavy code inlines the Result i128 carrier bit-ops.
    spec("feature-testing-examples/p7_deref_format.gos"),
    // Phase 7C: `seq.substring(i, i+k)` + `m.inc(kmer)` fuses to the borrowed
    // map probe on the compiled tiers; counts must match the VM's alloc path.
    spec("feature-testing-examples/p7_substring_inc.gos"),
    // Typed i64 for-range loops: fused back-edges, constant operands, the
    // accumulator write-back, continue/break/label routing, and empty/
    // single-iteration bounds must stay bit-identical across tiers.
    spec("feature-testing-examples/loop_arith_fusion.gos"),
    // `iter::` combinator chains over integer ranges fuse to a single loop
    // with the stage/terminal closures inlined (filter/map/sum_by/sum/
    // count/product/fold/for_each/any/all). Every fused shape must produce
    // the same result the eager combinator path would across all tiers.
    spec("feature-testing-examples/iter_pipeline_fusion.gos"),
    // `static mut` scalar load/store lowered natively: an LCG advances a
    // static-mut seed in a helper while `main`'s hot loop is JIT-promoted.
    // The compiled backing cell and the VM's shared cell must agree, and the
    // static must not poison whole-module JIT the way the VM-only decline did.
    spec("feature-testing-examples/static_mut_hot_loop.gos"),
    // A recursive Box-enum cloned in a loop (the original stays live) must
    // retain each iteration's clone; the loop-carried read must not be
    // move-elided. Covers the sequential and goroutine-shared (captured) paths
    // that double-freed the enum's nodes and corrupted the heap at exit.
    spec("feature-testing-examples/rc_loop_carried_clone.gos"),
    // Byte-packed `[u8]` storage (stride 1): array index, byte-array slice,
    // Vec<u8> push, iteration, and high-bit zero-extension must be
    // bit-identical across tiers (the unbounded-cache memory fix).
    spec("feature-testing-examples/byte_vec_packed.gos"),
    // A `Vec<u8>` reaches a builtin in whichever representation the VM chose
    // for it, and the byte-taking APIs - push_utf8, the utf8 helpers, the
    // bytes helpers, the encoders - have to read all of them and answer the
    // same thing on every tier.
    spec("feature-testing-examples/byte_buffer_representations.gos"),
    // A slice and a `Vec` share one runtime representation and are written
    // differently, so the spelling has to come from the static type on every
    // tier rather than from the value the formatter is handed.
    spec("feature-testing-examples/sequence_spelling.gos"),
    // In-place append fast paths: a tail-position `v.push(x)` inside an `if` /
    // `match` arm, `s += x` / `*out += x`, and the `&mut`-arg write-back cell
    // move (with its clone fallback when a sibling argument reads the same
    // local). These avoid the per-call copy that made build loops O(n^2) on
    // the VM; output must stay bit-identical across tiers.
    spec("feature-testing-examples/inplace_mut_append_parity.gos"),
    // Arithmetic operator overloading: `+ - * /` on a user struct route to its
    // `add`/`sub`/`mul`/`div` impl method; the result is the method's return
    // type (incl. a heterogeneous `Mul -> f64`). Bit-identical across tiers.
    spec("feature-testing-examples/operator_overload_arith.gos"),
    // Vector-math operator overloading: `Vec3` with `impl Add / Sub / Mul /
    // Neg / Index` drives `a + b`, `v * s`, `-v`, `v[i]`, and compound
    // assignment (`+=`, `*=`) through the same impls, inside a hot helper
    // loop so the JIT tier compiles it. Bit-identical across tiers.
    spec("feature-testing-examples/operator_overload_vec3.gos"),
    // Operator overloading on enums (`+`, unary `-`, `+=` on bound locals
    // and inline constructors) and on generic structs (`impl<T> Add for
    // Wrap<T>`), including a chained field read of the operator result
    // (`(a + b).v`) with an `f64` payload. Bit-identical across tiers.
    spec("feature-testing-examples/operator_overload_enum_generic.gos"),
    // Byte literals compare against the integer byte view without a cast
    // (`s.as_bytes()[i] == b'>'`); a byte literal is an `Int` value on
    // every tier.
    spec("feature-testing-examples/byte_literal_compare.gos"),
    // The two String index spaces stay distinct: `len`/`[]`/iteration count
    // Unicode scalars (`s[i]` is a `char`), while `byte_len`/`substring`/
    // `byte_at`/`as_bytes` take byte offsets and `byte_at` yields the byte
    // as an `i64`. Bit-identical across tiers.
    spec("feature-testing-examples/string_char_index_scan.gos"),
    // Move-on-last-use: draining a uniquely-owned consumable scrutinee in a
    // guard-free `match` must be suppressed for an arm whose refutable
    // sub-pattern (a literal) can fail after a field is emptied and fall
    // through to a later arm that re-reads the same variant. Also covers the
    // all-binding drain shape and the for-loop element drain over a rebuilt
    // recursive enum. Bit-identical across tiers.
    spec("feature-testing-examples/move_on_last_use_match.gos"),
    // `from_json` infers its type argument from the binding annotation, so the
    // turbofish is optional; the decode is identical on every tier.
    spec("feature-testing-examples/from_json_infer.gos"),
    // Auto-derived serde over Option / tuple / Vec / nested-struct fields.
    spec("feature-testing-examples/serde_more_field_kinds.gos"),
    // The inline Option/Result enum payload crosses fn boundaries as i128;
    // combined with wide shifts and comparisons this pins the i128 ABI and
    // instruction lowering bit-identically across tiers (the aarch64 backend
    // in particular, exercised on the native-arm and cross CI jobs).
    spec("feature-testing-examples/i128_enum_payload_arith.gos"),
    // Structural aggregate comparison: fixed-array / Vec `==`/`!=` and tuple
    // ordering (all six operators) are bit-identical across tiers (the VM
    // walks them at runtime; compiled routes to gos_rt_tuple_cmp / vec_eq).
    spec("feature-testing-examples/aggregate_compare.gos"),
    // `sort_by_key` / `sort_by_key_desc` Vec methods with scalar and tuple
    // (multi-key) keys; the key body is inlined into a `sort_by` comparator
    // that orders with `<`, identical on every tier.
    spec("feature-testing-examples/sort_by_key.gos"),
    // Comptime: `comptime { ... }` blocks and `comptime fn` calls are
    // evaluated on the VM during compilation and spliced into the source as
    // literals, so every tier compiles the identical constant (scalar /
    // string / float / bool / char results, const initializers, nesting).
    spec("feature-testing-examples/comptime_fold.gos"),
    // Comptime reflection (`typeInfo::<T>()`) + codegen: a comptime fn
    // consumes the reflected fields to generate a string (SQL DDL, field
    // lists), folded to a literal identical on every tier.
    spec("feature-testing-examples/comptime_reflection.gos"),
    // Comptime parameters (`fn f(comptime n, ...)`) fold their argument at
    // the call site, and the `regex!` / `sql!` validation macros validate at
    // build time and fold to the validated string on every tier.
    spec("feature-testing-examples/comptime_params_validate.gos"),
    // Code-emitting comptime (`codegen!(...)`): a comptime fn reflects a
    // type's fields and emits a native serializer body, spliced as raw
    // source. The emitted field code is identical on every tier and carries
    // no runtime reflection.
    spec("feature-testing-examples/comptime_codegen.gos"),
    // Phase 2 staged reflection: a `for` over `typeInfo::<T>()` is unrolled
    // per field in the single compile (no fold pass), `field_of` projects
    // the concrete field, and a `match` over the comptime field type folds
    // to the taken arm. Native field code, identical on every tier.
    spec("feature-testing-examples/comptime_inline_for.gos"),
    // Transparent `type X = T` aliases: interchangeable with the underlying
    // type in lets, params, returns, struct fields, composites, and chains,
    // lowering identically on every tier (no opaque nominal alias).
    spec("feature-testing-examples/type_alias_transparent.gos"),
    // Tuple structs: construction, positional `.N` access, and destructuring
    // (let / match / fn params), modelled as named fields "0".."N-1".
    spec("feature-testing-examples/tuple_structs.gos"),
    // Every `for` loop source shape: literal, `enumerate` over a literal or
    // a binding, `iter().enumerate()`, and an `Iterator<T>` binding. Each
    // must yield its elements once rather than restarting at index zero.
    spec("feature-testing-examples/iterator_loop_sources.gos"),
    // Tuple surface: heterogeneous elements, chained `.N.M` reads through a
    // nested tuple, positional assignment, structural compare/sort, and
    // `{}` / `{:?}` of a nested tuple.
    spec("feature-testing-examples/tuple_surface.gos"),
    // Named struct construction: keyed, positional, and mixed brace literals
    // all lower to declaration-order fields on every tier.
    spec("feature-testing-examples/named_struct_brace_construction.gos"),
    // Structs / enums compare and order by value with no `#[derive(...)]`:
    // auto-synthesized `eq` / `cmp`, plus `..` rest in multi-field variants.
    spec("feature-testing-examples/structural_comparison.gos"),
    spec("feature-testing-examples/nested_function_items.gos"),
    spec("feature-testing-examples/nested_struct_items.gos"),
    // Operator overloading (`% - ! | & ^ << >> []`), the desugar macros
    // (`matches!` / `dbg!`), and `x.into()` routing to `B::from(x)`.
    spec("feature-testing-examples/operator_overloads.gos"),
    // A written `cmp` decides the order every sequence ordering site reads,
    // and a type with none keeps the field-by-field order through the same
    // path: sort, sort_stable, min / max, and the sorted-sequence searches.
    spec("feature-testing-examples/user_cmp_ordering.gos"),
    // Pattern destructuring in function parameters (tuple / struct /
    // tuple-struct), bound via a fresh local + injected destructuring let.
    spec("feature-testing-examples/param_destructure.gos"),
    // Tuple-struct serde: position-keyed JSON object round-trip.
    spec("feature-testing-examples/tuple_struct_serde.gos"),
    // Phase 1 BTreeMap: String keys, i64 values, key-sorted iteration.
    spec("feature-testing-examples/btreemap_i64_keys.gos"),
    // Every hashable key shape - tuple, String-bearing tuple, struct, enum
    // (unit and payload), fixed array - keys by value on every tier, and
    // `keys()` rebuilds the aggregate the program wrote.
    spec("feature-testing-examples/hashable_map_keys.gos"),
    spec("feature-testing-examples/map_string_key_lengths.gos"),
    // `Map::from` / `BTreeMap::from` over a runtime sequence of pairs, the
    // tuple-returning `map` closure that feeds one, and the positional `let`
    // that takes a `split` result apart.
    spec("feature-testing-examples/map_from_sequence.gos"),
    // Traversal methods on `&Map` / `&Set` walk the collection the reference
    // names, matching the same call written on the collection itself.
    spec("feature-testing-examples/keyed_traversal_through_ref.gos"),
    // A map is equal to a map holding the same entries and a set to a set
    // holding the same members, whatever order they went in and whichever
    // representation each settled into; a value that stands for a whole value
    // compares by its content rather than by the word that reaches it.
    spec("feature-testing-examples/map_set_content_equality.gos"),
    // A byte walk is bounded by the string's UTF-8 length, not by its scalar
    // count, so a multi-byte string keeps its trailing bytes.
    spec("feature-testing-examples/string_byte_iteration.gos"),
    // `DynValue` is constructed and read back from pure Gossamer - by kind,
    // by runtime arm name, and by position - identically on every tier.
    spec("feature-testing-examples/dyn_value_surface.gos"),
    // An or-pattern binding one name from different payload positions reads
    // the field the alternative that matched supplies.
    spec("feature-testing-examples/or_pattern_payload_binding.gos"),
    // Trimming one edge of a string answers the same text everywhere.
    spec("feature-testing-examples/string_trim_edges.gos"),
    // A generic function and a generic method under a trait bound each
    // specialise per call site, free-standing and on a receiver alike.
    spec("feature-testing-examples/generic_trait_bound_method.gos"),
    // Mapping over an `Option` of a struct answers a value of the
    // closure's own type, whatever the receiver held.
    spec("feature-testing-examples/option_map_struct_field.gos"),
    // A `&mut Option<T>` reads and writes the carrier its caller holds, and
    // the payload outlives the frame that built it.
    spec("feature-testing-examples/mut_option_ref_writeback.gos"),
    // A fixed array's elements fill whole slots, so a `char`, a `bool`, or a
    // narrow integer reads back as the value it was written as.
    spec("feature-testing-examples/fixed_array_narrow_slots.gos"),
    // A map value that stands for a whole value - a sequence, an aggregate,
    // a float - compares by its content on every tier.
    spec("feature-testing-examples/map_set_aggregate_value_equality.gos"),
    // A sequence element pairing a scalar with a struct holds the struct's
    // words inline, so a destructured binding reads each part at its own slot
    // offset and borrows the element the sequence still owns.
    spec("feature-testing-examples/seq_pair_struct_elements.gos"),
    // A `u64` / `usize` reads as unsigned wherever a value directly holds one -
    // on its own, as a sequence element, an `Option` / `Result` arm, or a
    // struct field - so a value at or above `i64::MAX` renders as its own
    // decimal on every tier.
    spec("feature-testing-examples/unsigned_width_rendering.gos"),
    // A slot-backed container holds one word per element, and that word holds
    // whichever scalar the type names - each integer width, `f32` / `f64`,
    // `bool`, `char` - with a heap ordering by the element's own comparison.
    spec("feature-testing-examples/slot_container_element_types.gos"),
    // A `Deque` / `Queue` / `Stack` holds an element of any type, and a
    // `MaxHeap` / `MinHeap` any element the language orders - a `String`, a
    // tuple, a struct, an enum, a fixed array, a nested container, an
    // `Option` - rendered and ordered identically on every tier.
    spec("feature-testing-examples/slot_container_any_element.gos"),
    // A method call binds to the receiver's own type: a same-named method on
    // another type, a `&mut self` method elsewhere, and a free function of
    // that name are all beside the point. A container hands a heap value out
    // owned, so a map read stays live for its reader.
    spec("feature-testing-examples/method_dispatch_by_receiver_type.gos"),
    // A `&mut` borrow of a struct field writes through to its owner, a `Set`
    // answers emptiness from its own count, and a payload read on an arm that
    // did not run reaches for no node at all.
    spec("feature-testing-examples/enum_and_reference_dispatch.gos"),
    // A `Set` answers membership and set algebra and walks through `iter()`
    // in an order it promises nothing about; a `BTreeSet` reads sorted, and
    // both render sorted. Every tier reproduces the same walk.
    spec("feature-testing-examples/set_unordered_contract.gos"),
    // A type's identity is the module declaring it, so two modules may each
    // declare `Point` and `Tag` without their constructors, `{:?}`, `==`,
    // map keying, or serde symbols colliding.
    spec("feature-testing-examples/module_scoped_type_names.gos"),
    // VecDeque both-ends ops: push/pop/peek front and back.
    spec("feature-testing-examples/vecdeque_full.gos"),
    // A generic function's call result keeps its instantiated concrete type
    // when used inline (`println!("{}", id(s))`), selecting the right
    // formatter; the compiled tiers must match the VM across scalar / string
    // / float / struct results, multi-parameter generics, and recursion.
    spec("feature-testing-examples/generic_call_result.gos"),
    // Generic struct TYPES holding `T` by value and `impl<T>` methods on
    // them: per-instantiation field layout and method specialisation make
    // `Wrapper<Point>` / `Wrapper<i64>::get` bit-identical across tiers
    // (scalar / string / float / struct payloads, two type parameters,
    // nesting, and an array of generic structs).
    spec("feature-testing-examples/generic_struct_types.gos"),
    // A method on a bounded `impl<T: Trait>` block dispatching through its
    // type parameter, with the receiver reached through a field. The concrete
    // impl takes `&self`, so the specialised copy passes its address.
    spec("feature-testing-examples/generic_impl_bound_dispatch.gos"),
    // A variable-bound range loop must not advance the binding that supplied
    // its start, so re-entering it from an enclosing loop starts over.
    spec("feature-testing-examples/range_loop_bound_not_mutated.gos"),
    // A tagged-pointer enum reached through the combinator surface, through a
    // closure argument at a direct call site, and a generic struct whose field
    // offsets come from the per-instantiation layout rather than the declared
    // one. Each shape previously read a slot address as a handle.
    spec("feature-testing-examples/aggr_enum_vec_combinators.gos"),
    spec("feature-testing-examples/aggr_enum_closure_arg.gos"),
    spec("feature-testing-examples/aggr_generic_struct_layout.gos"),
    // Two bounds on one parameter, the same pair written as a `where`
    // clause, one clause constraining two parameters, and impl-level plus
    // method-level bounds side by side - every bound resolves its own
    // trait's methods.
    spec("feature-testing-examples/typing_multi_bound.gos"),
    // Product and nested-payload match coverage: a tuple scrutinee, a fixed
    // array with a rest pattern, a nested `Option<Result<..>>`, and a
    // struct-variant pattern binding named fields through a reference.
    spec("feature-testing-examples/typing_match_exhaustiveness.gos"),
    // An enum-variant payload (`Ok(..)` / `Some(..)`) whose struct has a
    // nested struct-typed field: reading `v.inner.field` after the match must
    // resolve the leaf against the inner struct's type on every tier, so the
    // compiled tiers walk the payload's flat slots instead of misrouting the
    // read to a dynamic JSON lookup (which the VM tolerated but native did not).
    spec("feature-testing-examples/nested_struct_variant_payload.gos"),
    // Perceus reuse: an owned local reassigned in a loop recycles its dropped
    // block in place on the compiled tiers (the VM does not). Reuse is
    // observationally transparent, so the result must match across tiers; the
    // RC-child variant exercises child release before recycle.
    spec("feature-testing-examples/perceus_reuse.gos"),
    // A HashMap that crosses the `go` boundary is marked shared at the spawn,
    // so its biased lock synchronizes the two goroutines' concurrent inserts:
    // the per-key totals are deterministic and identical on every tier (a
    // goroutine-local map takes the lock-free fast path instead).
    spec("feature-testing-examples/goroutine_shared_map.gos"),
    // Scalar source indexing panics on every tier rather than yielding zero
    // or silently dropping an invalid write; valid indexed access remains
    // observable before the failure.
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/oob_index_scalar_panic.gos")
    },
    // Loop-versioning bounds-check elision for affine `xs[base + counter]`
    // accesses: the in-range unchecked clone and the out-of-range checked
    // fallback both stay bit-identical across the three tiers.
    spec("feature-testing-examples/bce_loop_versioning.gos"),
    // Out-of-range read of an aggregate-element Vec panics identically on
    // every tier (was a compiled segfault / VM field-access error).
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/oob_index_aggregate_panic.gos")
    },
    // Vec insert/remove are invariant mutators in method and qualified forms:
    // invalid indices panic instead of clamping or silently no-oping.
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/vec_method_oob_panic.gos")
    },
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/vec_method_remove_oob_panic.gos")
    },
    // `swap` is an indexed write, not a resize: an out-of-range index is a
    // bounds panic on every tier rather than the silent no-op it once was.
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/vec_method_swap_oob_panic.gos")
    },
    // Integer divide-by-zero panics with GX0005 + exit 101 identically on
    // every tier (the SIGFPE-vs-clean-panic class had no 3-tier gate).
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/div_zero_panic.gos")
    },
    // A match that slips past exhaustiveness (nested int payloads) panics
    // cleanly and identically on the VM and the compiled backstop - was a
    // VM-returns-zero / compiled-returns-garbage divergence.
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/non_exhaustive_match_panic.gos")
    },
    // A `[v; n]` whose `n * elem_bytes` overflows aborts with a clean
    // `capacity overflow` panic on every tier (was a heap-corruption OOB
    // write / SIGSEGV on the compiled tiers). "before" is flushed first.
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/vec_capacity_overflow.gos")
    },
    // Out-of-range `HashMap.inc_at` window returns 0 and inserts nothing on
    // every tier (was an unbounded slice / OOB read on the compiled tier).
    spec("feature-testing-examples/map_inc_at_oob.gos"),
    // Unbounded recursion yields a clean stack-overflow diagnostic instead of
    // a raw SIGSEGV on every tier now that the AOT `@main` installs the guard.
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/deep_recursion_stack_overflow.gos")
    },
    // A byte-range `substring` that clamps mid-codepoint stays valid UTF-8
    // via the same lossy repair the interpreter uses (was raw invalid bytes
    // feeding `from_utf8_unchecked` on the compiled tier).
    spec("feature-testing-examples/str_substring_utf8_boundary.gos"),
    // Win B stdlib differential-parity coverage: every function in these
    // module groups produces bit-identical output on the VM, Cranelift, and
    // LLVM tiers (the sweep that found and fixed the split/equal_fold/parse,
    // path/time, and crypto-coerce divergences).
    spec("feature-testing-examples/winb_text_strings.gos"),
    spec("feature-testing-examples/winb_text_strconv.gos"),
    spec("feature-testing-examples/winb_text_utf8.gos"),
    spec("feature-testing-examples/winb_text_unicode.gos"),
    spec("feature-testing-examples/winb_text_fmt.gos"),
    spec("feature-testing-examples/winb_data_crypto.gos"),
    spec("feature-testing-examples/winb_data_encoding.gos"),
    spec("feature-testing-examples/binary_u8_varint_encode.gos"),
    spec("feature-testing-examples/winb_data_math.gos"),
    spec("feature-testing-examples/winb_data_regex.gos"),
    spec("feature-testing-examples/winb_coll_vec.gos"),
    spec("feature-testing-examples/winb_coll_map.gos"),
    spec("feature-testing-examples/winb_coll_set.gos"),
    spec("feature-testing-examples/winb_coll_iter.gos"),
    spec("feature-testing-examples/winb_coll_optres.gos"),
    spec("feature-testing-examples/winb_sys_path.gos"),
    spec("feature-testing-examples/winb_sys_time.gos"),
    spec("feature-testing-examples/winb_sys_bytes.gos"),
    spec("feature-testing-examples/winb_sys_misc.gos"),
    // Win B integrator-fix coverage: segfaults (Vec<Struct>::new+push,
    // HashSet<i64>::insert, regex::find_all bound-iter), silent-wrong
    // (map contains, parse_u64, JSON integer precision), and dispatch gaps
    // (HashSet to_vec/iter/clear, Vec method insert/remove, BTreeMap keys).
    spec("feature-testing-examples/winb2_vec_new_struct.gos"),
    spec("feature-testing-examples/winb2_hashset_i64.gos"),
    spec("feature-testing-examples/hashset_struct_keys.gos"),
    spec("feature-testing-examples/winb2_regex_find_all_bound.gos"),
    spec("feature-testing-examples/winb2_map_contains.gos"),
    spec("feature-testing-examples/winb2_parse_u64.gos"),
    spec("feature-testing-examples/winb2_json_int_precision.gos"),
    spec("feature-testing-examples/winb2_hashset_to_vec.gos"),
    spec("feature-testing-examples/module_aggregate_slots.gos"),
    spec("feature-testing-examples/winb2_vec_insert_remove.gos"),
    spec("feature-testing-examples/winb2_btreemap_keys.gos"),
    // 0.18.0 smaller items: String::from identity, parse-error Display,
    // scalar fixed-array out-of-range lenient zero-value.
    spec("feature-testing-examples/winb2_smaller_items.gos"),
    // JIT widening coverage fixtures (inliner edge-dissolving,
    // aggregate-interior bodies, char-field enums, mixed-arity).
    spec("feature-testing-examples/jit_inline_chain.gos"),
    spec("feature-testing-examples/jit_aggregate_local.gos"),
    // A hot loop constructing, copying, mutating, and dropping an aggregate
    // with an RC (String) field each iteration: the JIT must memcpy the copy
    // into its own frame slot (a mutation of the copy must not alias the
    // source) and reuse that slot rather than leaking a heap block per round.
    spec("feature-testing-examples/jit_aggregate_drop.gos"),
    spec("feature-testing-examples/jit_inline_aggregate_return.gos"),
    // A JIT-promoted hot loop that returns a 3-tuple, a struct, a `[i64; 4]`
    // array, and a monomorphised generic method's struct by value. Each call
    // lowers through the generalised structural-return (sret) ABI: the caller
    // allocates one correctly-sized stack slot per call site and the callee
    // writes the aggregate through it, so the loop is RSS-flat with no leak and
    // the returned pointer never dangles. Bit-identical VM / Cranelift / LLVM.
    spec("feature-testing-examples/aggregate_return_sret.gos"),
    spec("feature-testing-examples/jit_inline_const_args.gos"),
    spec("feature-testing-examples/jit_enum_char_field.gos"),
    spec("feature-testing-examples/jit_inline_vec_ops.gos"),
    // Word-stride Vec<f64>/Vec<i64> element get/set lower to inline
    // load/store off the GosVec header on the compiled tiers; covers read,
    // write, nested Vec, scalar lenient OOB, and an aggregate element type.
    spec("feature-testing-examples/vec_f64_inline_index.gos"),
    spec("feature-testing-examples/vec_get_method.gos"),
    spec("feature-testing-examples/jit_mixed_arity6.gos"),
    spec("feature-testing-examples/jit_aggregate_param.gos"),
    // Bytecode VM user-function inliner - must stay bit-identical to the
    // MIR-tier inlining already present in the compiled tiers.
    spec("feature-testing-examples/inline_scalar_kernel.gos"),
    spec("feature-testing-examples/temporary_wrap.gos"),
    spec("feature-testing-examples/temporary_method_dispatch.gos"),
    spec("feature-testing-examples/vecdeque_element_typing.gos"),
    spec("feature-testing-examples/method_dispatch_collisions.gos"),
    spec("feature-testing-examples/fmt_struct_enum.gos"),
    spec("feature-testing-examples/fmt_tuple_map.gos"),
    spec("feature-testing-examples/string_concat_chain.gos"),
    // Irrefutable let-pattern destructuring (struct / tuple-struct / enum
    // variant / nested / or-pattern) and const generic array length.
    spec("feature-testing-examples/let_destructure_struct.gos"),
    // Tuple-destructuring assignment over bindings, fields, indices, tuple
    // positions, nested tuples, and `_`.
    spec("feature-testing-examples/destructuring_assignment.gos"),
    // A container built into an aggregate the callee returns: the frame that
    // made it releases its own reference.
    spec("feature-testing-examples/returned_aggregate_container_share.gos"),
    spec("feature-testing-examples/const_generic_array_len.gos"),
    spec("feature-testing-examples/container_display.gos"),
    spec("feature-testing-examples/container_reassign_loop.gos"),
    // Closure capturing an inline aggregate reads every field, and the heap
    // box survives an escaping closure.
    spec("feature-testing-examples/closure_capture_aggregate.gos"),
    // Let-chains, open-ended range patterns, fixed-array slice patterns,
    // bounds-safe String.byte_at, and in-place / flat numeric Vec growth.
    spec("feature-testing-examples/let_chains.gos"),
    spec("feature-testing-examples/open_ended_ranges.gos"),
    spec("feature-testing-examples/slice_pattern_fixed_array.gos"),
    spec("feature-testing-examples/string_byte_at_oob.gos"),
    spec("feature-testing-examples/vec_inplace_growth.gos"),
    spec("feature-testing-examples/record_update.gos"),
    spec("feature-testing-examples/nested_struct_record_update.gos"),
    spec("feature-testing-examples/map_iter_destructure.gos"),
    spec("feature-testing-examples/trait_bounds.gos"),
    spec("feature-testing-examples/nested_field_access.gos"),
    spec("feature-testing-examples/rc_elision.gos"),
    spec("feature-testing-examples/bounds_check_elim.gos"),
    spec("feature-testing-examples/borrowed_option_result.gos"),
    spec("feature-testing-examples/aggregate_binding.gos"),
    spec("feature-testing-examples/fs_metadata.gos"),
    spec("feature-testing-examples/html_escape.gos"),
    spec("feature-testing-examples/html_template_render_json.gos"),
    spec("feature-testing-examples/jwt_roundtrip.gos"),
    spec("feature-testing-examples/crypto_ecdsa.gos"),
    spec("feature-testing-examples/validate_errors.gos"),
    spec("feature-testing-examples/validate_errors_return.gos"),
    spec("feature-testing-examples/sync_rwlock.gos"),
    spec("feature-testing-examples/context_cancel.gos"),
    spec("feature-testing-examples/metrics_observability.gos"),
    spec("feature-testing-examples/trace_observability.gos"),
    spec("feature-testing-examples/os_signal_subscribe.gos"),
    spec("feature-testing-examples/array_bounds_probe.gos"),
    spec("feature-testing-examples/array_literal_vec_methods.gos"),
    spec("feature-testing-examples/vec_aggregate_rc_ownership.gos"),
    // A by-value struct's `Vec` / `[T]` field: construction moves the vector in,
    // struct copy / `..base` share it (retained through the vec count), a field
    // extract borrows it, and every owner frees it once at death. Covers the
    // drop pass's Vec-field teardown plus the Cranelift single-slot-aggregate
    // and struct-update (`..base` projected operand) lowering.
    spec("feature-testing-examples/struct_vec_field.gos"),
    // A `&self` / `&mut self` method returning a heap field by value: the
    // caller's share is minted through the reference, so the receiver's own
    // buffer survives the caller's release and the next iteration's growth.
    spec("feature-testing-examples/field_return_through_ref.gos"),
    // A sequence or map whose element is wider than one slot walks through a
    // cursor that carries the element's address, so a chain runs per element
    // pulled and a terminal rebuilds storage of the element's own shape.
    spec("feature-testing-examples/lazy_iter_aggregate_elements.gos"),
    // Nested vectors: build `Vec<Vec<i64>>` by push and an `[[i64]]` literal,
    // double-index `a[i][j]`, iterate an inner row via the outer index, mutate
    // an inner element and grow an inner row, and drop the whole structure.
    spec("feature-testing-examples/nested_vec_ops.gos"),
    spec("feature-testing-examples/mut_ref_scalar_writeback.gos"),
    spec("feature-testing-examples/mut_ref_string_writeback.gos"),
    spec("feature-testing-examples/mutability_explicit_parity.gos"),
    spec("feature-testing-examples/fixed_array_mut_param_copy.gos"),
    Spec {
        skip_all: Some(
            "contains an intentional narrow-integer overflow whose debug panic and release wrapping are tested directly by spec_conformance",
        ),
        ..spec("feature-testing-examples/byte_vec_i64_model.gos")
    },
    spec("feature-testing-examples/map_iteration_order.gos"),
    spec("feature-testing-examples/usize_compare.gos"),
    spec("feature-testing-examples/u64_unsigned.gos"),
    spec("feature-testing-examples/channel_close_drain.gos"),
    spec("feature-testing-examples/chan_struct_payload.gos"),
    spec("feature-testing-examples/chan_carrier_elements.gos"),
    spec("feature-testing-examples/doctest_fences.gos"),
    spec("feature-testing-examples/map_struct_field_semantics.gos"),
    spec("feature-testing-examples/map_field_in_vec_element.gos"),
    spec("feature-testing-examples/seq_methods_on_projected_receivers.gos"),
    spec("feature-testing-examples/float_bits_unsigned_args.gos"),
    spec("feature-testing-examples/channel_timers.gos"),
    Spec {
        nondeterministic: true,
        ..spec("feature-testing-examples/channel_fan_in.gos")
    },
    spec("feature-testing-examples/capture_by_reference.gos"),
    spec("feature-testing-examples/closure_capture_mutation.gos"),
    spec("feature-testing-examples/closure_lifetime_inference.gos"),
    spec("feature-testing-examples/closure_payload_typing.gos"),
    spec("feature-testing-examples/combinator_sweep.gos"),
    spec("feature-testing-examples/mut_ref_params.gos"),
    spec("feature-testing-examples/mut_ref_container_params.gos"),
    spec("feature-testing-examples/mut_ref_container_assign.gos"),
    spec("feature-testing-examples/http_surface.gos"),
    spec("feature-testing-examples/http_form_multipart.gos"),
    spec("feature-testing-examples/option_none_variant_collision.gos"),
    spec("feature-testing-examples/method_name_collision.gos"),
    spec("feature-testing-examples/select_multiplex.gos"),
    spec("feature-testing-examples/select_closed_chan_ready.gos"),
    spec("feature-testing-examples/select_ctx_cancel.gos"),
    spec("feature-testing-examples/let_else_binding.gos"),
    spec("feature-testing-examples/slice_param_coercion.gos"),
    spec("feature-testing-examples/enum_param_rc_repro.gos"),
    spec("feature-testing-examples/vec_param_aggregate_ctor.gos"),
    spec("feature-testing-examples/sort_struct_field_closure.gos"),
    spec("feature-testing-examples/sql_driverless.gos"),
    spec("feature-testing-examples/sql_ident_quoting.gos"),
    spec("feature-testing-examples/struct_copy_reclaim.gos"),
    spec("feature-testing-examples/struct_copy_followups.gos"),
    spec("feature-testing-examples/struct_container_reclaim.gos"),
    spec("feature-testing-examples/enum_unit_local.gos"),
    spec("feature-testing-examples/panic_hook.gos"),
    spec("feature-testing-examples/arena_blocks.gos"),
    spec("feature-testing-examples/result_struct_payload.gos"),
    spec("feature-testing-examples/vec_literal_coercion.gos"),
    spec("feature-testing-examples/derive_traits.gos"),
    spec("feature-testing-examples/derive_struct_variant.gos"),
    spec("feature-testing-examples/struct_map_keys.gos"),
    spec("feature-testing-examples/atomic_bool.gos"),
    spec("feature-testing-examples/cycle_collector.gos"),
    spec("feature-testing-examples/cycle_reclaim.gos"),
    // The collector treats shared (escaped) objects as external live
    // edges while goroutines churn them: no trial-deletion through the
    // shared boundary, freed cycle nodes release their out-edges once.
    spec("feature-testing-examples/cycle_shared_goroutines.gos"),
    // A Vec-bearing enum payload survives escaping its constructing
    // frame (by-value call argument, returned through a second
    // boundary) and is reclaimed exactly once wherever the enum dies.
    spec("feature-testing-examples/enum_vec_payload_escape.gos"),
    spec("feature-testing-examples/jit_native_marshal.gos"),
    spec("feature-testing-examples/arena_regions.gos"),
    spec("feature-testing-examples/auto_regions.gos"),
    spec("feature-testing-examples/auto_regions_for.gos"),
    spec("feature-testing-examples/auto_regions_map_iter.gos"),
    spec("feature-testing-examples/auto_regions_closure_body.gos"),
    spec("feature-testing-examples/bool_vec_byte_stride.gos"),
    spec("feature-testing-examples/tuple_extract_region.gos"),
    spec("feature-testing-examples/defer_unwind_order.gos"),
    spec("feature-testing-examples/early_break_materializers.gos"),
    spec("feature-testing-examples/empty_vec_growth.gos"),
    spec("feature-testing-examples/vec_multislot_growth.gos"),
    spec("feature-testing-examples/doc_test_vs_unit_test_drift.gos"),
    spec("feature-testing-examples/error_chain_inspection.gos"),
    spec("feature-testing-examples/error_question_mark_propagation.gos"),
    spec("feature-testing-examples/float_cast_drift.gos"),
    spec("feature-testing-examples/format_precision_padding.gos"),
    spec("feature-testing-examples/format_spec.gos"),
    spec("feature-testing-examples/format_width_pad_parity.gos"),
    spec("feature-testing-examples/map_bare_iteration_binding.gos"),
    spec("feature-testing-examples/packed_enum_repr.gos"),
    spec("feature-testing-examples/contextual_keywords.gos"),
    spec("feature-testing-examples/carrier_return_tiers.gos"),
    spec("feature-testing-examples/closure_env_ownership.gos"),
    spec("feature-testing-examples/cohort_drain_deadline.gos"),
    spec("feature-testing-examples/cohort_enumeration.gos"),
    spec("feature-testing-examples/cohort_library_combinators.gos"),
    spec("feature-testing-examples/cohort_header_settings.gos"),
    spec("feature-testing-examples/slot_container_value_semantics.gos"),
    spec("feature-testing-examples/defer_exit_edges.gos"),
    spec("feature-testing-examples/format_spec_matrix.gos"),
    spec("feature-testing-examples/format_argument_forms.gos"),
    Spec {
        nondeterministic: true,
        skip_parity: Some("goroutines reach stdout in the order the scheduler runs them"),
        ..spec("feature-testing-examples/format_sinks_and_atomicity.gos")
    },
    spec("feature-testing-examples/binary_offset_accessors.gos"),
    spec("feature-testing-examples/float_bit_reinterpret.gos"),
    spec("feature-testing-examples/vec_bulk_and_binary_search.gos"),
    spec("feature-testing-examples/fs_error_text.gos"),
    spec("feature-testing-examples/fs_file_positional_io.gos"),
    spec("feature-testing-examples/fs_temp_file_lifecycle.gos"),
    spec("feature-testing-examples/fs_temp_resources.gos"),
    spec("feature-testing-examples/fs_dir_ops.gos"),
    spec("feature-testing-examples/for_over_free_call.gos"),
    spec("feature-testing-examples/fs_permission_modes.gos"),
    spec("feature-testing-examples/json_null_rendering.gos"),
    spec("feature-testing-examples/map_tuple_key_tuple_value.gos"),
    spec("feature-testing-examples/mut_param_is_a_copy.gos"),
    spec("feature-testing-examples/set_literal_rendering.gos"),
    spec("feature-testing-examples/vec_literal_rendering.gos"),
    spec("feature-testing-examples/container_in_aggregate_rendering.gos"),
    spec("feature-testing-examples/single_slot_aggregate_elements.gos"),
    spec("feature-testing-examples/process_run_in.gos"),
    spec("feature-testing-examples/path_split.gos"),
    spec("feature-testing-examples/path_value.gos"),
    spec("feature-testing-examples/base32_decode.gos"),
    spec("feature-testing-examples/json_yaml_encode.gos"),
    spec("feature-testing-examples/bounded_channel.gos"),
    spec("feature-testing-examples/generic_function_monomorphization.gos"),
    spec("feature-testing-examples/generic_fn_param.gos"),
    spec("feature-testing-examples/named_function_item_coercion.gos"),
    spec("feature-testing-examples/goroutine_panic_isolation.gos"),
    Spec {
        nondeterministic: true,
        ..spec("feature-testing-examples/hashmap_counter_race.gos")
    },
    spec("feature-testing-examples/hashset_algebra.gos"),
    spec("feature-testing-examples/http2_push.gos"),
    spec("feature-testing-examples/http2_trailers.gos"),
    spec("feature-testing-examples/http_cookie.gos"),
    spec("feature-testing-examples/http_csrf.gos"),
    spec("feature-testing-examples/http_csrf_attach.gos"),
    spec("feature-testing-examples/http_session.gos"),
    spec("feature-testing-examples/http_session_roundtrip.gos"),
    spec("feature-testing-examples/http_form_urlencoded.gos"),
    spec("feature-testing-examples/httptest_static_server.gos"),
    spec("examples/http_diagnostics_transport.gos"),
    spec("feature-testing-examples/http_router_lookup.gos"),
    spec("feature-testing-examples/http_serve_err_binding.gos"),
    spec("feature-testing-examples/http3_serve_err_binding.gos"),
    Spec {
        skip_all: Some(
            "contains an intentional unsigned underflow whose debug panic and release wrapping are tested directly by spec_conformance",
        ),
        ..spec("feature-testing-examples/integer_overflow_edges.gos")
    },
    spec("feature-testing-examples/intcode_day2_mut_slice_native.gos"),
    spec("feature-testing-examples/intcode_day2_native.gos"),
    spec("feature-testing-examples/iter_combinator_chain.gos"),
    spec("feature-testing-examples/iter_extra.gos"),
    spec("feature-testing-examples/sync_extra.gos"),
    spec("feature-testing-examples/math_rand.gos"),
    spec("feature-testing-examples/bytes_builder.gos"),
    spec("feature-testing-examples/net_ip.gos"),
    spec("feature-testing-examples/net_tcp_echo.gos"),
    spec("feature-testing-examples/net_tcp_read_deadline.gos"),
    spec("feature-testing-examples/net_read_into.gos"),
    spec("feature-testing-examples/string_push_utf8.gos"),
    spec("feature-testing-examples/aggr_map_field_return.gos"),
    spec("feature-testing-examples/aggr_by_value_param_storage.gos"),
    spec("feature-testing-examples/aggr_map_field_value_semantics.gos"),
    spec("feature-testing-examples/aggr_container_field_value_semantics.gos"),
    spec("feature-testing-examples/vec_container_field_elements.gos"),
    spec("feature-testing-examples/net_smtp_send.gos"),
    Spec {
        // Unix-domain sockets are POSIX-only; on Windows every entry
        // point returns an Err, so the program prints a bind-failure
        // message whose format differs between VM and native.
        skip_all: if cfg!(windows) {
            Some("Unix-domain sockets are not available on Windows")
        } else {
            None
        },
        ..spec("feature-testing-examples/net_unix_echo.gos")
    },
    spec("feature-testing-examples/vec_remove_inplace.gos"),
    spec("feature-testing-examples/map_value_heap_children.gos"),
    spec("feature-testing-examples/map_pop_then_drop.gos"),
    spec("feature-testing-examples/rc_move_elision.gos"),
    spec("feature-testing-examples/map_struct_value_access.gos"),
    spec("feature-testing-examples/map_iter_wildcard_destructure.gos"),
    spec("feature-testing-examples/mut_self_method_dispatch.gos"),
    spec("feature-testing-examples/single_field_struct_aggregate.gos"),
    spec("feature-testing-examples/struct_tuple_map_key.gos"),
    spec("feature-testing-examples/struct_keyed_map_value_iter.gos"),
    spec("feature-testing-examples/debug_option_result.gos"),
    spec("feature-testing-examples/debugfmt_floats.gos"),
    spec("feature-testing-examples/debugfmt_aggregates.gos"),
    spec("feature-testing-examples/debugfmt_nested_floats.gos"),
    spec("feature-testing-examples/goroutine_panic_join.gos"),
    spec("feature-testing-examples/chan_struct_local_recv.gos"),
    spec("feature-testing-examples/chan_select_struct_payload.gos"),
    spec("feature-testing-examples/net_tls_client.gos"),
    spec("feature-testing-examples/net_tls_client_modes.gos"),
    spec("feature-testing-examples/json_round_trip_fuzz.gos"),
    spec("feature-testing-examples/json_set_update.gos"),
    spec("feature-testing-examples/option_result_chain_methods.gos"),
    spec("feature-testing-examples/process_spawn_piped.gos"),
    spec("feature-testing-examples/method_dispatch_collision.gos"),
    spec("feature-testing-examples/module_qualified_enum_ctor.gos"),
    spec("feature-testing-examples/module_same_fn_names.gos"),
    spec("feature-testing-examples/mutex_poison_recovery.gos"),
    spec("feature-testing-examples/mutex_vs_channel_counter.gos"),
    spec("feature-testing-examples/numeric_conversion_matrix.gos"),
    spec("feature-testing-examples/option_default.gos"),
    spec("feature-testing-examples/option_unwrap_chain.gos"),
    spec("feature-testing-examples/result_default.gos"),
    spec("feature-testing-examples/try_option_propagation.gos"),
    spec("feature-testing-examples/try_err_conversion.gos"),
    spec("feature-testing-examples/crypto_sha_hex.gos"),
    spec("feature-testing-examples/os_signal_handler.gos"),
    spec("feature-testing-examples/panic_recover_round_trip.gos"),
    spec("feature-testing-examples/enum_clone_share.gos"),
    spec("feature-testing-examples/format_mutually_recursive_types.gos"),
    spec("feature-testing-examples/match_guard_payload.gos"),
    spec("feature-testing-examples/match_struct_payload_slot.gos"),
    spec("feature-testing-examples/pattern_match_exhaustiveness.gos"),
    spec("feature-testing-examples/pipe_operator_precedence.gos"),
    spec("feature-testing-examples/pipe_closure_step.gos"),
    Spec {
        // The example exercises `exec::run` against `echo`, `printf`,
        // `sh`, `true`, `false` - all Unix-only standalone executables
        // (on Windows `echo`/`true`/`false` are `cmd` builtins, not
        // resolvable via `Command::new`, and `sh`/`printf` aren't
        // present at all). Cross-platform shape would defeat the
        // demo's purpose. Linux + macOS cover the surface.
        skip_all: if cfg!(windows) {
            Some("uses Unix-only commands (echo, sh, printf, true, false)")
        } else {
            None
        },
        ..spec("feature-testing-examples/process_spawn_pipe.gos")
    },
    spec("feature-testing-examples/rc_release_drops.gos"),
    spec("feature-testing-examples/map_insert_borrowed_key_rc.gos"),
    spec("feature-testing-examples/map_owned_struct_value_rc.gos"),
    spec("feature-testing-examples/mut_string_return.gos"),
    spec("feature-testing-examples/string_accumulator_return.gos"),
    spec("feature-testing-examples/weak_refs.gos"),
    // `w.upgrade()` hands out an owned strong reference pinned in a
    // frame-owned shadow local: repeated / rebound / discarded upgrades
    // keep the accounting balanced on every tier.
    spec("feature-testing-examples/weak_upgrade_ownership.gos"),
    // `x.downgrade()` on a by-value aggregate: the referent lives in an RC
    // cell pinned by the creating scope, so liveness never depends on the
    // source binding's last read on any tier.
    spec("feature-testing-examples/weak_value_referent.gos"),
    // A `Weak` into a member of the classic reference-cycle shape: aggregate
    // stores copy and `downgrade()` pins its referent for the enclosing
    // scope, so `upgrade()` never observes when the collector ran.
    spec("feature-testing-examples/weak_into_strong_cycle.gos"),
    // A Gossamer `String` carries a byte length and may hold interior NULs;
    // every compiled-tier shim must read it through that length rather than
    // scanning for a terminator.
    spec("feature-testing-examples/nul_in_strings.gos"),
    spec("feature-testing-examples/recursive_enum_walk.gos"),
    spec("feature-testing-examples/heap_enum_by_value_param.gos"),
    // Structural `==` / `!=` on heap (recursive / Box / Vec-bearing) enums:
    // equal-but-distinct allocations compare true on every tier.
    spec("feature-testing-examples/enum_struct_eq.gos"),
    spec("feature-testing-examples/reference_alias_mutation.gos"),
    spec("feature-testing-examples/regex_unicode_categories.gos"),
    Spec {
        skip_parity: Some("poll-attempt count is scheduler-dependent; output varies across tiers"),
        ..spec("feature-testing-examples/select_default_timing.gos")
    },
    spec("feature-testing-examples/slice_methods.gos"),
    spec("feature-testing-examples/slice_subslicing.gos"),
    spec("feature-testing-examples/sort_with_closure.gos"),
    spec("feature-testing-examples/spawn_join.gos"),
    spec("feature-testing-examples/string_build.gos"),
    spec("feature-testing-examples/string_concatenation_stress.gos"),
    spec("feature-testing-examples/string_method_surface.gos"),
    spec("feature-testing-examples/string_unicode_boundaries.gos"),
    spec("feature-testing-examples/fast_string_path_scan.gos"),
    // Byte-range `substring` reads its source length from the O(1) string
    // header (not an O(len) strlen), so a sliding-window k-mer scan stays
    // linear; covers literal + built sources, clamping, and HashMap<String>
    // k-mer counting.
    spec("feature-testing-examples/str_substring_kmer.gos"),
    // `m.iter()` on a `&HashMap` parameter materialises real entries on the
    // compiled tiers (the receiver type is peeled past `&` before the map
    // dispatch); previously a borrowed map iterated as a bogus Vec.
    spec("feature-testing-examples/hashmap_ref_param_iter.gos"),
    spec("feature-testing-examples/time_monotonic_vs_wall.gos"),
    spec("feature-testing-examples/time_civil.gos"),
    spec("feature-testing-examples/tw_go_block.gos"),
    spec("feature-testing-examples/trait_object_dispatch.gos"),
    Spec {
        nondeterministic: true,
        ..spec("feature-testing-examples/tuple_destructuring_loop.gos")
    },
    spec("feature-testing-examples/variable_shadowing_ladder.gos"),
    spec("feature-testing-examples/same_scope_shadow_assignment.gos"),
    spec("feature-testing-examples/literal_forms.gos"),
    spec("feature-testing-examples/loop_continue.gos"),
    spec("feature-testing-examples/match_or_patterns.gos"),
    spec("feature-testing-examples/or_patterns.gos"),
    spec("feature-testing-examples/string_match_patterns.gos"),
    spec("feature-testing-examples/string_char_needle.gos"),
    spec("feature-testing-examples/static_items.gos"),
    spec("feature-testing-examples/stdlib_expansion.gos"),
    spec("feature-testing-examples/strconv_radix_quote.gos"),
    spec("feature-testing-examples/stdlib_strings_free.gos"),
    spec("feature-testing-examples/stdlib_compiled_wiring.gos"),
    spec("feature-testing-examples/stdlib_path_free.gos"),
    spec("feature-testing-examples/json_encode_aggregates.gos"),
    spec("feature-testing-examples/vec_elem_option_accessors.gos"),
    spec("feature-testing-examples/module_impl_ordering.gos"),
    spec("feature-testing-examples/derive_ord_option_field.gos"),
    spec("feature-testing-examples/shared_across_goroutines.gos"),
    spec("feature-testing-examples/stdlib_path_glob.gos"),
    spec("feature-testing-examples/stdlib_sort_module.gos"),
    spec("feature-testing-examples/stdlib_errors_chain.gos"),
    spec("feature-testing-examples/stdlib_io_adapters.gos"),
    spec("feature-testing-examples/stdlib_time_free.gos"),
    spec("feature-testing-examples/stdlib_hash.gos"),
    spec("feature-testing-examples/stdlib_math_bits.gos"),
    spec("feature-testing-examples/stdlib_math_pred.gos"),
    spec("feature-testing-examples/stdlib_os_introspection.gos"),
    spec("feature-testing-examples/stdlib_fs_rename.gos"),
    spec("feature-testing-examples/stdlib_json_as_bool.gos"),
    spec("feature-testing-examples/stdlib_thread_yield.gos"),
    Spec {
        stdin: b"alpha\nbeta\ngamma\n",
        ..spec("feature-testing-examples/stdlib_io_read_all.gos")
    },
    Spec {
        stdin: b"one two three",
        ..spec("feature-testing-examples/stdlib_io_copy.gos")
    },
    spec("feature-testing-examples/stdlib_alias_wiring.gos"),
    spec("feature-testing-examples/stdlib_math_scalar.gos"),
    spec("feature-testing-examples/stdlib_math_const.gos"),
    spec("feature-testing-examples/stdlib_unicode_norm.gos"),
    spec("feature-testing-examples/stdlib_process.gos"),
    spec("feature-testing-examples/stdlib_time_methods.gos"),
    spec("feature-testing-examples/duration_methods.gos"),
    spec("feature-testing-examples/flag_cell_duration.gos"),
    spec("feature-testing-examples/instant_methods.gos"),
    spec("feature-testing-examples/time_param_dispatch.gos"),
    Spec {
        // Debug execution intentionally checks overflow while an optimised
        // release build wraps, matching Rust's profile-dependent arithmetic.
        // `spec_conformance::spec_3_1_native_profiles_check_then_wrap_overflow`
        // exercises both profiles directly, so comparing this fixture's VM
        // result with LLVM release would assert a behavior that must differ.
        skip_all: Some("debug overflow checks intentionally differ from release wrapping"),
        ..spec("feature-testing-examples/neg_int_min_wraps.gos")
    },
    spec("feature-testing-examples/stdlib_net_dns.gos"),
    spec("feature-testing-examples/stdlib_json_dynamic.gos"),
    spec("feature-testing-examples/stdlib_netip.gos"),
    spec("feature-testing-examples/stdlib_strconv.gos"),
    spec("feature-testing-examples/stdlib_fs_ops.gos"),
    spec("feature-testing-examples/stdlib_encoding_crypto.gos"),
    spec("feature-testing-examples/stdlib_text_codec.gos"),
    spec("feature-testing-examples/stdlib_pem.gos"),
    spec("feature-testing-examples/stdlib_x509.gos"),
    spec("feature-testing-examples/stdlib_archive.gos"),
    spec("feature-testing-examples/struct_update_base.gos"),
    spec("feature-testing-examples/at_binding_subpattern.gos"),
    spec("feature-testing-examples/scheduler_drain.gos"),
    spec("feature-testing-examples/static_mut_basic.gos"),
    spec("feature-testing-examples/static_mut_goroutines.gos"),
    spec("feature-testing-examples/closure_goroutine.gos"),
    spec("feature-testing-examples/go_stdlib_spawn.gos"),
    spec("feature-testing-examples/spawn_operand_order.gos"),
    spec("feature-testing-examples/yaml_autoderive.gos"),
    spec("feature-testing-examples/sync_map_demo.gos"),
    spec("feature-testing-examples/autoderive_int_widths.gos"),
    spec("feature-testing-examples/write_file_bytes.gos"),
    spec("feature-testing-examples/unicode_full.gos"),
    spec("feature-testing-examples/string_len_bytes.gos"),
    spec("feature-testing-examples/concurrent_atomic.gos"),
    // Same cross-goroutine-registry class as concurrent_atomic, for the
    // HashSet and VecDeque handle registries: a handle built before a
    // channel yield (which the scheduler may resume on another worker
    // thread) must stay usable afterward. A thread-local registry lost
    // the handle across the migration; the registries are now global.
    spec("feature-testing-examples/goroutine_set_handle.gos"),
    spec("feature-testing-examples/goroutine_deque_handle.gos"),
    spec("feature-testing-examples/stdlib_parity_batch.gos"),
    spec("feature-testing-examples/compress_zstd.gos"),
    spec("feature-testing-examples/compress_bzip2.gos"),
    spec("feature-testing-examples/crypto_password.gos"),
    spec("feature-testing-examples/crypto_extra.gos"),
    spec("feature-testing-examples/crypto_aead.gos"),
    spec("feature-testing-examples/encoding_xml.gos"),
    spec("feature-testing-examples/misc_class_a.gos"),
    spec("feature-testing-examples/hashmap_get_some_field.gos"),
    spec("feature-testing-examples/hashmap_field_through_result.gos"),
    Spec {
        skip_all: if cfg!(windows) {
            Some("uses Unix-only commands (printf, tr, sort, head)")
        } else {
            None
        },
        ..spec("feature-testing-examples/exec_pipeline.gos")
    },
    Spec {
        skip_all: if cfg!(windows) {
            Some("uses Unix-only /bin/true and /bin/sleep")
        } else {
            None
        },
        ..spec("feature-testing-examples/exec_wait_timeout.gos")
    },
    Spec {
        skip_all: if cfg!(windows) {
            Some("uses Unix-only /bin/sleep, /bin/sh, SIGTERM")
        } else {
            None
        },
        ..spec("feature-testing-examples/exec_signal_group.gos")
    },
    spec("feature-testing-examples/vec_runtime_repeat.gos"),
    spec("feature-testing-examples/vec_with_capacity.gos"),
    // Push-built / runtime-repeat scalar vecs ride the VM's flat typed
    // storage (IntArray / FloatVec); the whole Vec surface, plus mixed
    // vec-vs-fixed-array structural `==`, must agree on every tier.
    spec("feature-testing-examples/vec_push_typed_storage.gos"),
    spec("feature-testing-examples/vec_swap_in_place.gos"),
    // Whole-program shapes that no single-feature fixture covered: whole-Vec
    // rebinding across passes, indexed struct-field compound assignment,
    // pre-sized containers with a Vec-as-queue walk, line-oriented byte
    // streaming, and index expressions that read through another index.
    spec("feature-testing-examples/bench_shape_buffer_pingpong.gos"),
    spec("feature-testing-examples/bench_shape_struct_array_fields.gos"),
    spec("feature-testing-examples/bench_shape_graph_and_list.gos"),
    spec("feature-testing-examples/bench_shape_byte_stream.gos"),
    spec("feature-testing-examples/bench_shape_index_chains.gos"),
    spec("feature-testing-examples/single_field_struct_field_read.gos"),
    spec("feature-testing-examples/range_non_i64.gos"),
    spec("feature-testing-examples/string_push_char.gos"),
    spec("feature-testing-examples/vec_deque.gos"),
    spec("feature-testing-examples/tuple_match_patterns.gos"),
    spec("feature-testing-examples/for_tuple_and_mut_vec.gos"),
    spec("feature-testing-examples/clone_builtin_dispatch.gos"),
    spec("feature-testing-examples/nested_vec_mutation.gos"),
    spec("feature-testing-examples/deref_string_concat.gos"),
    spec("feature-testing-examples/vec_single_field_struct.gos"),
    spec("feature-testing-examples/inline_index_remap.gos"),
    spec("feature-testing-examples/string_append_realloc.gos"),
    spec("feature-testing-examples/byte_literal_arith.gos"),
    spec("feature-testing-examples/closure_env_container_capture.gos"),
    spec("feature-testing-examples/inferred_map_dispatch.gos"),
    spec("feature-testing-examples/iterator_trait_user_impl.gos"),
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/jit_panic_trace.gos")
    },
    spec("feature-testing-examples/map_entry_and_format_paths.gos"),
    spec("feature-testing-examples/nested_repeat_literal.gos"),
    spec("feature-testing-examples/range_pipeline_iter.gos"),
    // A `for` header over iterator state (a range, `.rev()`, or an adapter
    // chain over either) walks the pipeline once. Re-pulling the iterand per
    // iteration restarts it at element zero, so every tier must agree on the
    // full element sequence, the bound-local shape, and early `break`.
    spec("feature-testing-examples/for_lazy_iterator_source.gos"),
    // An `Iterator<T>` parameter receiving a range, a `.iter()`, and an
    // adapter chain. The argument's element type is often still open at the
    // call, so the parameter has to pin it through the `Iterator` constructor.
    spec("feature-testing-examples/iterator_param_argument.gos"),
    // Element bindings of a `&mut` sequence loop: scalar write-through, a
    // heap element mutated through its slot pointer, slot replacement, and a
    // shared `&[T]` binding by value. The slot-address form read a Vec header
    // out of the outer buffer and faulted natively.
    spec("feature-testing-examples/mut_ref_element_loops.gos"),
    // A nested stdlib function called by its leaf-module name, the json
    // query surface reached as methods, and `{:?}` of a variant whose
    // payload is a collection or a document. Each of these answered on one
    // tier and failed on the other.
    spec("feature-testing-examples/stdlib_leaf_calls_and_json_queries.gos"),
    // A closure route and a named-handler route registered on one router.
    // The closure form emitted an undefined `Handler::serve` and failed to
    // link, while the VM registered it fine.
    spec("feature-testing-examples/router_closure_route.gos"),
    // `Set::from(values)` where the elements come from a runtime sequence
    // rather than a literal list.
    spec("feature-testing-examples/set_from_sequence.gos"),
    spec("feature-testing-examples/seq_method_combinators.gos"),
    spec("feature-testing-examples/stdlib_slog.gos"),
    // Top-level statements (implicit `fn main`): plain, `?`-propagation,
    // mixed-with-items, and an explicit process exit code.
    spec("examples/top_level_statements.gos"),
    spec("feature-testing-examples/top_level_hello.gos"),
    spec("feature-testing-examples/top_level_question.gos"),
    spec("feature-testing-examples/top_level_mixed.gos"),
    Spec {
        allow_nonzero: true,
        ..spec("feature-testing-examples/top_level_exit_code.gos")
    },
    // Front-end features: labelled loops (`break 'l`/`continue 'l`) and
    // slice / rest patterns (`[a, b]`, `[first, ..rest]`, `[.., last]`).
    spec("feature-testing-examples/labeled_loops.gos"),
    spec("feature-testing-examples/slice_patterns.gos"),
    // Gossamer-native SQL driver dispatch: a `.gos` struct registers
    // itself as a std::database::sql driver (sql::register_native) and
    // is driven through the full Conn/Stmt/Rows facade. Cross-tier
    // gate for the register_native bridge + native_* side-channel.
    spec("feature-testing-examples/sql_native_driver.gos"),
    // Qualified type-path annotation (`util::Rec` in `&util::Rec` param and
    // `&mut util::Rec` param) resolves to the struct's Adt on all tiers so
    // field access lowers to a real Field projection instead of falling
    // through to the json accessor.
    spec("feature-testing-examples/cross_module_struct_fields.gos"),
    // Struct-`self` method JIT (Lever 1a): an all-scalar user struct whose
    // `&mut self` mutator and `&self` reader run as in-process Cranelift JIT
    // code. The struct crosses the VM<->native boundary as a flat field-slot
    // block; a `&mut self` call's in-place field mutations are written back
    // into the caller's binding. Mixed i64/f64/bool/char fields, a hot loop
    // (so the methods promote), and `&self` recursion - bit-identical across
    // the bytecode VM, Cranelift JIT, and LLVM AOT tiers.
    spec("feature-testing-examples/struct_self_jit.gos"),
    // `Vec<Vec<i64>>` / `[[i64]]` crosses the VM<->native boundary as the AOT
    // nested layout (outer vec of inner `GosVec<i64>` pointers), marshalled
    // once per source `Arc` and reused via the identity cache across repeated
    // calls. A `&[[i64]]` function called in a hot loop promotes to Cranelift
    // JIT and reads `g[i]` + iterates inner vecs - the graph-bfs shape.
    // Bit-identical across the bytecode VM, Cranelift JIT, and LLVM AOT tiers.
    spec("feature-testing-examples/vec_vec_i64_jit.gos"),
    // A recursive enum (`Node`) with a `Vec<Node>` (`List`) and a
    // `Vec<(String, Node)>` (`Map`) variant: a `parse`-like
    // `Result<Node, _>`-returning builder called in a hot loop promotes to the
    // Cranelift JIT, where its String-in / Result<enum>-out boundary marshals
    // once each. The native body builds nested `List` / `Map` DOMs (heap enum
    // nodes, `GosVec` fields, AGGR_OWNED tuple vecs) which the trampoline reads
    // back into a `Value::Variant` tree and frees - the json-serde parse shape.
    // Bit-identical across the bytecode VM, Cranelift JIT, and LLVM AOT tiers.
    spec("feature-testing-examples/json_parse_jit.gos"),
    // Recursive heap enum crossing the JIT boundary in BOTH directions: a
    // by-value `transform(Node) -> Node` (enum in, freshly built enum out,
    // with `Vec<Node>` and `Vec<(String, Node)>` variant fields marshalled
    // each way) and a `serialize_into(&Node, &mut String)` that writes the
    // string back through the `&mut` cell. The recursive builders promote to
    // the Cranelift JIT; output is bit-identical across the VM, JIT, and AOT.
    spec("feature-testing-examples/enum_transform_jit.gos"),
    // `for (k, v)` over a `Vec<(String, Enum)>` bound from an enum-variant
    // pattern - the tuple-destructure for-vec path - plus a fixed-array
    // literal local stored as an enum payload, which must materialize a heap
    // GosVec so the nested variant iterates correctly. Bit-identical across
    // the VM, Cranelift JIT, and LLVM AOT.
    spec("feature-testing-examples/for_kv_enum_payload.gos"),
    // Display-rendering `join` on scalar / String sequences and the strict
    // `to_i64` / `to_f64` / `to_bool` String parses, which lower to the
    // `gos_rt_str_to_*_opt` Option carriers on the compiled tiers. Guards the
    // MIR dispatch wiring so the parses stay bit-identical across the VM,
    // Cranelift JIT, and LLVM AOT.
    spec("feature-testing-examples/stdlib_surface_join_parse_take.gos"),
    // Eager combinator shims index a `GosVec`, while a range is lazy iterator
    // state. A chain whose element or output type keeps it off the lazy path
    // has to snapshot its source first, so the snapshot must happen on every
    // tier: the VM walked the range while the compiled tiers read the lazy
    // handle's words as a Vec header.
    spec("feature-testing-examples/nested_vec_capture.gos"),
    // Associated types: projected through a bound, defaulted by the trait,
    // and pinned by an `Item = T` equality constraint. Each projection
    // resolves to a concrete type before lowering, so the three tiers agree.
    spec("feature-testing-examples/assoc_type_through_bound.gos"),
    spec("feature-testing-examples/assoc_type_default.gos"),
    spec("feature-testing-examples/assoc_type_binding.gos"),
    // Associated constants read through a concrete type, `Self`, and a bound
    // parameter. Each hoists to a top-level constant, so the value is the
    // same one every tier folds.
    spec("feature-testing-examples/assoc_const_read.gos"),
    Spec {
        skip_all: Some("rejected at check: the impl omits a required associated type"),
        ..spec("feature-testing-examples/assoc_missing_impl_item.gos")
    },
    Spec {
        skip_all: Some("rejected at check: the `break` label names no enclosing loop"),
        ..spec("feature-testing-examples/break_unknown_label.gos")
    },
    // Element-typed combinator surfaces: the element's own class decides the
    // runtime helper, the callback's register classes, and the terminal's
    // result type, so every tier reads the same bits.
    spec("feature-testing-examples/elemty_float_terminals.gos"),
    spec("feature-testing-examples/elemty_float_eager.gos"),
    spec("feature-testing-examples/elemty_string_closures.gos"),
    spec("feature-testing-examples/elemty_aggregate_elements.gos"),
    spec("feature-testing-examples/elemty_struct_keyed_map.gos"),
    spec("feature-testing-examples/elemty_btreemap_shapes.gos"),
    spec("feature-testing-examples/elemty_narrow_element_stride.gos"),
    // A `Vec` payload wrapped in an `Option` / `Result`: the carrier must
    // take its own reference, or `unwrap` leaves one reference with two
    // owners and the second release frees storage the first returned.
    spec("feature-testing-examples/option_vec_payload_ownership.gos"),
    // A two-word `Option` / `Result` carrier at an odd word offset inside a
    // struct, which is the shape the emitted datalayout's 8-byte `i128`
    // alignment exists for.
    spec("feature-testing-examples/carrier_at_odd_slot_offset.gos"),
    // `m.insert(k.clone(), v)` on a key that reached the frame by value:
    // the clone has to be a share of its own, or the consuming insert
    // takes the caller's.
    spec("feature-testing-examples/clone_into_consuming_call.gos"),
    // A struct taken by value and written through a `&mut self` method:
    // the frame's shallow copy has to own shares of its heap fields, or
    // the write frees what the caller still holds.
    spec("feature-testing-examples/by_value_param_owns_its_fields.gos"),
    // The test surface: a handler called in memory, and a wall clock a test
    // controls rather than waits on.
    spec("feature-testing-examples/httptest_record_and_clock.gos"),
    // Assets folded into the program at compile time, resolved against the
    // source that embeds them rather than the build's working directory.
    spec("feature-testing-examples/comptime_embedded_assets.gos"),
];

const DEDICATED_FEATURE_TESTING_EXAMPLES: &[&str] = &[
    // Driven by `http_channel_pool_{vm,cranelift,llvm}` in `server_parity.rs`:
    // it needs concurrent clients, which the SPECS walk does not drive.
    "http_channel_pool.gos",
    "http_bare_handler.gos",
    "http_plain_fn_handler.gos",
    "http_middleware_controls.gos",
    "lifecycle_readiness_shutdown.gos",
    "http_server_object.gos",
    "http_request_context.gos",
    "http_response_stream.gos",
    "http_request_cohort_arena.gos",
    "http_bare_aliases.gos",
    "http_client_cookie_jar.gos",
    "http_client_verbs.gos",
    "http_serve_tls_roundtrip.gos",
    "http_server_headers.gos",
    "http_middleware_bearer.gos",
    "http_middleware_compose.gos",
    "stdlib_http_middleware_stack.gos",
    "http_middleware_ws.gos",
    "http_router_params.gos",
    "http_router_typed_params.gos",
    "http_router_chain.gos",
    "http_next_chunk.gos",
    "http_proxy_stream.gos",
    "http_raw_bytes.gos",
    "http_redirect_policy.gos",
    "http_request_headers.gos",
    "http_request_values.gos",
    "http_request_query_pairs.gos",
    "http_request_form_auth.gos",
    "http_form_file.gos",
    "http_response_headers.gos",
    "http_roundtrip.gos",
    "http_static_file.gos",
    "http_static_range.gos",
    "http_websocket_accept.gos",
    "websocket_echo.gos",
];

#[test]
fn specs_cover_every_feature_testing_example() {
    use std::collections::BTreeSet;

    let root = workspace_root();
    let dir = root.join("feature-testing-examples");
    let on_disk: BTreeSet<String> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("gos"))
        .filter_map(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    let mut covered: BTreeSet<String> = SPECS
        .iter()
        .filter_map(|spec| spec.path.strip_prefix("feature-testing-examples/"))
        .map(str::to_string)
        .collect();
    covered.extend(
        DEDICATED_FEATURE_TESTING_EXAMPLES
            .iter()
            .map(|path| (*path).to_string()),
    );
    let missing: Vec<_> = on_disk.difference(&covered).cloned().collect();
    assert!(
        missing.is_empty(),
        "feature-testing-examples fixtures missing from SPECS: {}",
        missing.join(", ")
    );
}

#[derive(Debug)]
struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    /// Crash cause instead of an opaque number (signal name on unix,
    /// NTSTATUS name on Windows); `None` only if the process never
    /// reported an exit status.
    exit_text: Option<String>,
    /// True when the deadline elapsed and the child was killed.
    timed_out: bool,
    /// Executable path that was launched.
    exe: PathBuf,
    /// Space-joined command line (exe + args), for reproduction.
    cmdline: String,
    /// Working directory the child ran in.
    workdir: PathBuf,
}

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// Renders an `ExitStatus` via the shared helper so a native-tier
/// crash reads as its cause (signal / NTSTATUS) instead of a bare
/// number. Returns `Some` whenever the process reported a status
/// (exit code or signal); `None` only if no status was collected.
fn render_status(status: std::process::ExitStatus) -> Option<String> {
    if status.code().is_some() {
        return Some(common::describe_exit(status).text);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal().is_some() {
            return Some(common::describe_exit(status).text);
        }
    }
    let _ = status;
    None
}

fn run_with_timeout(
    mut child: Child,
    stdin: &[u8],
    deadline: Instant,
    exe: PathBuf,
    cmdline: String,
    workdir: PathBuf,
) -> Run {
    if let Some(mut sin) = child.stdin.take() {
        let _ = sin.write_all(stdin);
        drop(sin);
    }
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    dump_stuck_child_forensics(child.id());
                    // Under the gdb debug harness, interrupt instead of
                    // killing first: batch gdb then prints the inferior's
                    // backtrace into the captured stdout before exiting.
                    if env::var("GOS_PARITY_GDB").is_ok() {
                        let _ = Command::new("kill")
                            .args(["-INT", &child.id().to_string()])
                            .status();
                        std::thread::sleep(Duration::from_secs(10));
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(_) => break,
        }
    }
    let out = child.wait_with_output().expect("wait_with_output");
    Run {
        stdout: normalize_newlines(&String::from_utf8_lossy(&out.stdout)),
        stderr: normalize_newlines(&String::from_utf8_lossy(&out.stderr)),
        code: out.status.code(),
        exit_text: render_status(out.status),
        timed_out,
        exe,
        cmdline,
        workdir,
    }
}

/// Dumps per-thread scheduler state of a child that outlived the run
/// deadline, so a timeout report shows WHERE the process is stuck
/// (thread names, wait channels, run state) instead of only that it
/// was killed. Best-effort: /proc reads that fail are skipped.
#[cfg(target_os = "linux")]
fn dump_stuck_child_forensics(pid: u32) {
    eprintln!("--- stuck-child forensics for pid {pid} ---");
    if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
        for line in status.lines() {
            if line.starts_with("State") || line.starts_with("Threads") {
                eprintln!("  {line}");
            }
        }
    }
    if let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) {
        for t in tasks.flatten() {
            let tid = t.file_name().to_string_lossy().to_string();
            let comm = fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let wchan =
                fs::read_to_string(format!("/proc/{pid}/task/{tid}/wchan")).unwrap_or_default();
            // Comm may contain spaces, so the run-state field is found
            // after the last `)` of `/proc/<pid>/stat`.
            let state = fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit(')')
                        .next()
                        .and_then(|tail| tail.split_whitespace().next())
                        .map(std::string::ToString::to_string)
                })
                .unwrap_or_default();
            eprintln!("  tid={tid} comm={comm} state={state} wchan={wchan}");
        }
    }
    eprintln!("--- end forensics ---");
}

/// macOS mirror of the Linux forensics: `sample` (ships with the OS)
/// captures every thread's stack of the stuck child, so a timeout
/// report shows WHERE the process is wedged (a parked worker, a
/// spinning collector, a lost channel wakeup) instead of only that it
/// was killed. Best-effort: a missing or failing `sample` is skipped.
#[cfg(target_os = "macos")]
fn dump_stuck_child_forensics(pid: u32) {
    eprintln!("--- stuck-child forensics for pid {pid} ---");
    match Command::new("sample")
        .args([&pid.to_string(), "2", "-mayDie"])
        .output()
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            // The call-graph section is the useful part; the binary
            // image list below it is noise for a hang report.
            let graph = text
                .split("Binary Images:")
                .next()
                .unwrap_or(&text)
                .trim_end();
            for line in graph.lines() {
                eprintln!("  {line}");
            }
            if !out.status.success() {
                eprintln!("  (sample exited {:?})", out.status.code());
            }
        }
        Err(e) => eprintln!("  (sample unavailable: {e})"),
    }
    eprintln!("--- end forensics ---");
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn dump_stuck_child_forensics(_pid: u32) {}

/// Formats the full per-tier execution report for a CI failure dump.
/// Every field is shown even when empty so a crashed-before-output
/// tier still surfaces executable path, command line, and exit cause.
fn tier_report(label: &str, run: &Run) -> String {
    let exit = match (run.code, &run.exit_text) {
        (Some(c), Some(text)) => format!("{c} ({text})"),
        (Some(c), None) => format!("{c}"),
        (None, Some(text)) => format!("none ({text})"),
        (None, None) => "none".to_string(),
    };
    let timeout = if run.timed_out { "yes" } else { "no" };
    format!(
        "{label}:\n  exit={exit}\n  timed_out={timeout}\n  exe={}\n  cmdline={}\n  workdir={}\n  stdout={:?}\n  stderr={:?}",
        run.exe.display(),
        run.cmdline,
        run.workdir.display(),
        run.stdout,
        run.stderr,
    )
}

#[test]
fn tier_report_shows_exit_ntstatus_and_streams() {
    let run = Run {
        stdout: String::from("ok\n"),
        stderr: String::new(),
        code: Some(0),
        exit_text: Some("exit 0".to_string()),
        timed_out: false,
        exe: PathBuf::from("/tmp/gos"),
        cmdline: "/tmp/gos run examples/hello.gos".to_string(),
        workdir: PathBuf::from("/home/daniel/dev/gossamer"),
    };
    let report = tier_report("vm", &run);
    assert!(report.starts_with("vm:\n  exit=0 (exit 0)"));
    assert!(report.contains("timed_out=no"));
    assert!(report.contains("exe=/tmp/gos"));
    assert!(report.contains("cmdline=/tmp/gos run examples/hello.gos"));
    assert!(report.contains("workdir=/home/daniel/dev/gossamer"));
    assert!(report.contains("stdout=\"ok\\n\""));
    assert!(report.contains("stderr=\"\""));
}

#[test]
fn tier_report_handles_crash_and_timeout() {
    // 0xC0000005 reinterpreted as i32 is -1073741819. This is how
    // Rust's ExitStatus::code() surfaces a Windows NTSTATUS exit.
    // exit_text carries the decoded name so the CI log reads as the
    // cause, not a number.
    let run = Run {
        stdout: String::new(),
        stderr: String::from("fault"),
        code: Some(-1073741819),
        exit_text: Some("exit code 0xc0000005 (STATUS_ACCESS_VIOLATION)".to_string()),
        timed_out: true,
        exe: PathBuf::from("C:\\scratch\\hello.exe"),
        cmdline: "C:\\scratch\\hello.exe".to_string(),
        workdir: PathBuf::from("C:\\ci"),
    };
    let report = tier_report("cranelift", &run);
    assert!(report.contains("exit=-1073741819 (exit code 0xc0000005 (STATUS_ACCESS_VIOLATION))"));
    assert!(report.contains("timed_out=yes"));
    assert!(report.contains("exe=C:\\scratch\\hello.exe"));
    assert!(report.contains("stdout=\"\""));
    assert!(report.contains("stderr=\"fault\""));
}

fn run_vm(src: &Path, args: &[&str], stdin: &[u8]) -> Run {
    let gos = gos_bin();
    let mut cmd = Command::new(&gos);
    cmd.arg("run").arg(src);
    let mut parts: Vec<String> = vec![gos.display().to_string()];
    parts.push("run".to_string());
    parts.push(src.display().to_string());
    if !args.is_empty() {
        cmd.args(args);
        parts.extend(args.iter().map(std::string::ToString::to_string));
    }
    let workdir = cmd.get_current_dir().map_or_else(
        || env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        std::path::Path::to_path_buf,
    );
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    run_with_timeout(
        child,
        stdin,
        Instant::now() + PER_RUN_TIMEOUT,
        gos,
        parts.join(" "),
        workdir,
    )
}

/// The pure-bytecode tier: `gos run` with the JIT switched off, so no
/// body is ever promoted to native code.
fn run_bytecode(src: &Path, args: &[&str], stdin: &[u8]) -> Run {
    let gos = gos_bin();
    let mut cmd = Command::new(&gos);
    cmd.arg("run")
        .arg(src)
        .env("GOS_JIT", "0")
        .env_remove("GOSSAMER_JIT_THRESHOLD");
    let mut parts: Vec<String> = vec!["GOS_JIT=0".to_string(), gos.display().to_string()];
    parts.push("run".to_string());
    parts.push(src.display().to_string());
    if !args.is_empty() {
        cmd.args(args);
        parts.extend(args.iter().map(std::string::ToString::to_string));
    }
    let workdir = cmd.get_current_dir().map_or_else(
        || env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        std::path::Path::to_path_buf,
    );
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    run_with_timeout(
        child,
        stdin,
        Instant::now() + PER_RUN_TIMEOUT,
        gos,
        parts.join(" "),
        workdir,
    )
}

fn run_jit(src: &Path, args: &[&str], stdin: &[u8]) -> Run {
    let gos = gos_bin();
    let mut cmd = Command::new(&gos);
    cmd.arg("run")
        .arg(src)
        .env("GOSSAMER_JIT_THRESHOLD", "1")
        // Makes the Cranelift tier report how many native entries it
        // installed, so `parity_walk` can prove the run was not a second
        // bytecode-VM execution wearing the JIT's label.
        .env("GOS_JIT_STATS", "1")
        .env_remove("GOS_JIT");
    let mut parts: Vec<String> = vec![
        "GOSSAMER_JIT_THRESHOLD=1".to_string(),
        gos.display().to_string(),
    ];
    parts.push("run".to_string());
    parts.push(src.display().to_string());
    if !args.is_empty() {
        cmd.args(args);
        parts.extend(args.iter().map(std::string::ToString::to_string));
    }
    let workdir = cmd.get_current_dir().map_or_else(
        || env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        std::path::Path::to_path_buf,
    );
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos with JIT");
    run_with_timeout(
        child,
        stdin,
        Instant::now() + PER_RUN_TIMEOUT,
        src.to_path_buf(),
        parts.join(" "),
        workdir,
    )
}

fn build_native(src: &Path, release: bool, scratch: &Path) -> Result<PathBuf, String> {
    let mut cmd = Command::new(gos_bin());
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.arg("--out-dir").arg(scratch).arg(src);
    let out = cmd.output().expect("spawn gos build");
    if !out.status.success() {
        return Err(format!(
            "gos build {flag} failed:\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            flag = if release { "--release" } else { "" },
        ));
    }
    // The unit name is manifest-derived (project id tail) for sources
    // inside a project, or the file stem for loose-file builds. Scan
    // the scratch dir for a single executable instead of guessing.
    let mut binaries: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(scratch)
        .map_err(|e| format!("read_dir {}: {e}", scratch.display()))?
        .flatten()
    {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if is_executable(&p) {
            binaries.push(p);
        }
    }
    if binaries.is_empty() {
        return Err(format!(
            "gos build produced no executable in {}",
            scratch.display(),
        ));
    }
    if binaries.len() > 1 {
        return Err(format!(
            "gos build produced multiple executables in {}: {binaries:?}",
            scratch.display(),
        ));
    }
    Ok(binaries.into_iter().next().expect("checked len == 1"))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

fn run_native(bin: &Path, args: &[&str], stdin: &[u8]) -> Run {
    // Debug harness mode: run the binary under gdb (its direct parent, so
    // Yama's restricted ptrace scope permits it); the deadline path sends
    // SIGINT and batch gdb prints the backtrace of wherever the inferior
    // is spinning before the kill.
    let mut cmd = if env::var("GOS_PARITY_GDB").is_ok() {
        let mut c = Command::new("gdb");
        c.args(["--batch", "-ex", "run", "-ex", "bt", "--args"]);
        c.arg(bin);
        c.args(args);
        c
    } else {
        let mut c = Command::new(bin);
        c.args(args);
        c
    };
    let mut parts: Vec<String> = vec![bin.display().to_string()];
    parts.extend(args.iter().map(std::string::ToString::to_string));
    let workdir = cmd.get_current_dir().map_or_else(
        || env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        std::path::Path::to_path_buf,
    );
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn native binary");
    run_with_timeout(
        child,
        stdin,
        Instant::now() + PER_RUN_TIMEOUT,
        bin.to_path_buf(),
        parts.join(" "),
        workdir,
    )
}

fn run_tier(spec: &Spec, tier: Tier) -> Result<Run, String> {
    let src = workspace_root().join(spec.path);
    match tier {
        Tier::Vm => Ok(run_vm(&src, spec.args, spec.stdin)),
        Tier::Bytecode => Ok(run_bytecode(&src, spec.args, spec.stdin)),
        Tier::Cranelift => Ok(run_jit(&src, spec.args, spec.stdin)),
        Tier::Llvm | Tier::LlvmDebug => {
            let release = tier == Tier::Llvm;
            let scratch = fresh_dir(&format!(
                "{prefix}-{tag}",
                prefix = if release { "ll" } else { "lldbg" },
                tag = file_tag(spec.path),
            ));
            let bin = build_native(&src, release, &scratch)?;
            let run = run_native(&bin, spec.args, spec.stdin);
            let _ = fs::remove_dir_all(&scratch);
            Ok(run)
        }
    }
}

fn file_tag(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("x")
        .to_string()
}

fn stdout_matches(a: &str, b: &str, nondeterministic: bool) -> bool {
    if nondeterministic {
        let mut la: Vec<&str> = a.lines().collect();
        let mut lb: Vec<&str> = b.lines().collect();
        la.sort_unstable();
        lb.sort_unstable();
        la == lb
    } else {
        a == b
    }
}

fn divergence(spec: &Spec, lhs: (Tier, &Run), rhs: (Tier, &Run)) -> Option<String> {
    if !stdout_matches(&lhs.1.stdout, &rhs.1.stdout, spec.nondeterministic) {
        return Some(format!(
            "{path}: stdout diverged between {a} and {b}\n  {a}: {astdout:?}\n  {b}: {bstdout:?}\n\n--- per-tier execution report ---\n{report}",
            path = spec.path,
            a = lhs.0.label(),
            b = rhs.0.label(),
            astdout = lhs.1.stdout,
            bstdout = rhs.1.stdout,
            report = tier_report(lhs.0.label(), lhs.1) + "\n" + &tier_report(rhs.0.label(), rhs.1),
        ));
    }
    if !spec.allow_nonzero && lhs.1.code != rhs.1.code {
        return Some(format!(
            "{path}: exit code diverged: {a}={ac:?} {b}={bc:?}\n\n--- per-tier execution report ---\n{report}",
            path = spec.path,
            a = lhs.0.label(),
            ac = lhs.1.code,
            b = rhs.0.label(),
            bc = rhs.1.code,
            report = tier_report(lhs.0.label(), lhs.1) + "\n" + &tier_report(rhs.0.label(), rhs.1),
        ));
    }
    None
}

#[test]
fn vm_runs_every_example_without_crashing() {
    let mut failures = Vec::new();
    for spec in SPECS {
        if let Some(reason) = spec.skip_all {
            eprintln!("skip vm: {} ({reason})", spec.path);
            continue;
        }
        if spec.server.is_some() {
            // Server VM coverage lives in `web_server_smoke_vm`.
            continue;
        }
        let run = match run_tier(spec, Tier::Vm) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!(
                    "{path}: vm error (no Run produced - tier failed before execution):\n  {e}",
                    path = spec.path,
                ));
                continue;
            }
        };
        if !spec.allow_nonzero && run.code != Some(0) {
            failures.push(format!(
                "{path}: vm exit={code:?}\n\n--- per-tier execution report ---\n{report}",
                path = spec.path,
                code = run.code,
                report = tier_report(Tier::Vm.label(), &run),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} VM run failures:\n{}",
        failures.len(),
        failures.join("\n\n"),
    );
}

// The parity battery is split into `PARITY_GROUPS` round-robin groups
// per tier so a single failing example fails only its small group test
// (e.g. `llvm_parity_group_2`) instead of the whole "every example"
// suite - narrower to find, faster to re-run. The failure message
// still names the exact example. Keep the group tests below in sync
// with this count.
const PARITY_GROUPS: usize = 6;

macro_rules! parity_group_tests {
    ($($g:literal => $bytecode:ident, $cranelift:ident, $llvm:ident, $llvm_debug:ident, $strict:ident;)*) => {
        $(
            #[test]
            fn $bytecode() {
                parity_walk(Tier::Bytecode, $g);
            }
            #[test]
            fn $cranelift() {
                parity_walk(Tier::Cranelift, $g);
            }
            #[test]
            fn $llvm() {
                parity_walk(Tier::Llvm, $g);
            }
            #[test]
            fn $llvm_debug() {
                parity_walk(Tier::LlvmDebug, $g);
            }
            #[test]
            fn $strict() {
                strict_lowering_group($g);
            }
        )*
    };
}

parity_group_tests! {
    0 => bytecode_parity_group_0, cranelift_parity_group_0, llvm_parity_group_0, llvm_debug_parity_group_0, llvm_strict_lower_group_0;
    1 => bytecode_parity_group_1, cranelift_parity_group_1, llvm_parity_group_1, llvm_debug_parity_group_1, llvm_strict_lower_group_1;
    2 => bytecode_parity_group_2, cranelift_parity_group_2, llvm_parity_group_2, llvm_debug_parity_group_2, llvm_strict_lower_group_2;
    3 => bytecode_parity_group_3, cranelift_parity_group_3, llvm_parity_group_3, llvm_debug_parity_group_3, llvm_strict_lower_group_3;
    4 => bytecode_parity_group_4, cranelift_parity_group_4, llvm_parity_group_4, llvm_debug_parity_group_4, llvm_strict_lower_group_4;
    5 => bytecode_parity_group_5, cranelift_parity_group_5, llvm_parity_group_5, llvm_debug_parity_group_5, llvm_strict_lower_group_5;
}

/// The overflow fixtures `skip_all` excludes from the parity walk, because
/// their VM result must differ from an optimised release build: debug
/// execution checks integer overflow where release wraps.
///
/// The debug-AOT tier keeps the checking semantics, so these fixtures DO have
/// a defined cross-tier contract there - one the release-only walk could never
/// state. Running them here is the coverage `skip_all` gives up.
const DEBUG_AOT_OVERFLOW_FIXTURES: &[&str] = &[
    "feature-testing-examples/byte_vec_i64_model.gos",
    "feature-testing-examples/integer_overflow_edges.gos",
    "feature-testing-examples/neg_int_min_wraps.gos",
];

#[test]
fn debug_aot_matches_vm_on_overflow_checked_fixtures() {
    let mut failures = Vec::new();
    for path in DEBUG_AOT_OVERFLOW_FIXTURES {
        let fixture = Spec {
            allow_nonzero: true,
            ..spec(path)
        };
        let vm = match run_tier(&fixture, Tier::Vm) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{path}: vm error: {e}"));
                continue;
            }
        };
        let debug_aot = match run_tier(&fixture, Tier::LlvmDebug) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{path}: llvm-debug error: {e}"));
                continue;
            }
        };
        if let Some(d) = divergence(&fixture, (Tier::Vm, &vm), (Tier::LlvmDebug, &debug_aot)) {
            failures.push(d);
        }
    }
    assert!(
        failures.is_empty(),
        "{} debug-AOT overflow parity failures:\n{}",
        failures.len(),
        failures.join("\n\n"),
    );
}

/// Highest native-entry count the Cranelift tier reported on stderr, or
/// `None` when it never reported one.
///
/// `GOS_JIT_STATS=1` makes every JIT compilation attempt emit one
/// `gos-jit-stats: compiled=N` line. A program compiles more than once (the
/// VM re-promotes as new bodies get hot), so the run installed native code
/// whenever the highest reported count is non-zero.
fn jit_installed_entries(stderr: &str) -> Option<usize> {
    stderr
        .lines()
        .filter_map(|line| line.trim().strip_prefix("gos-jit-stats: compiled="))
        .filter_map(|n| n.trim().parse::<usize>().ok())
        .max()
}

/// Fixtures whose Cranelift-tier run installs no native entry, each with the
/// admission reason `GOS_JIT_TRACE` reports for it. Every other fixture must
/// compile at least one body: otherwise the "cranelift" column of the parity
/// walk is a second bytecode-VM run and proves nothing about the JIT.
///
/// The rows live in `jit_no_compile.tsv` rather than in a Rust literal so the
/// list stays one line per fixture and reads as data. Its header documents
/// each reason. Shrinking the file is the point: every row that disappears is
/// a fixture whose Cranelift column started meaning something.
fn jit_compiles_nothing() -> &'static [(String, String)] {
    static ROWS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    ROWS.get_or_init(|| {
        include_str!("jit_no_compile.tsv")
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let (path, reason) = line.split_once('\t').unwrap_or_else(|| {
                    panic!("jit_no_compile.tsv row is not TAB-separated: {line:?}")
                });
                (path.to_string(), reason.to_string())
            })
            .collect()
    })
}

#[test]
fn jit_allowlist_rows_name_real_specs() {
    use std::collections::BTreeSet;

    let known: BTreeSet<&str> = SPECS.iter().map(|spec| spec.path).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut problems = Vec::new();
    for (path, reason) in jit_compiles_nothing() {
        if !known.contains(path.as_str()) {
            problems.push(format!("{path}: not a SPECS row"));
        }
        if !seen.insert(path.as_str()) {
            problems.push(format!("{path}: duplicate row"));
        }
        if reason.is_empty() {
            problems.push(format!("{path}: empty reason"));
        }
    }
    assert!(
        problems.is_empty(),
        "jit_no_compile.tsv is stale:\n  {}",
        problems.join("\n  "),
    );
}

#[test]
fn jit_stats_line_is_parsed_from_stderr() {
    assert_eq!(
        jit_installed_entries("gos-jit-stats: compiled=0\n"),
        Some(0)
    );
    assert_eq!(
        jit_installed_entries("noise\ngos-jit-stats: compiled=3\ngos-jit-stats: compiled=1\n"),
        Some(3),
    );
    assert_eq!(jit_installed_entries("no stats here\n"), None);
}

/// Serialises every parity walk so concurrent test functions can't
/// race on examples whose fixtures share `/tmp/gossamer_test_*`
/// paths (notably `fs_temp_file_lifecycle.gos`). The grouped tests run
/// sequentially under this lock - the round-robin split shrinks the
/// failing unit without reintroducing the cross-test fixture race.
static PARITY_WALK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn parity_walk(compiled: Tier, group: usize) {
    let _guard = PARITY_WALK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let trace = env::var_os("GOS_PARITY_TRACE").is_some();
    let mut failures = Vec::new();
    for (idx, spec) in SPECS.iter().enumerate() {
        if idx % PARITY_GROUPS != group {
            continue;
        }
        if spec.skip_all.is_some() || spec.skip_parity.is_some() || spec.server.is_some() {
            continue;
        }
        if compiled == Tier::LlvmDebug && spec.skip_debug_aot.is_some() {
            continue;
        }
        if trace {
            eprintln!(
                "tier-parity: {} group {group}: {}",
                compiled.label(),
                spec.path
            );
        }
        let vm = match run_tier(spec, Tier::Vm) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!(
                    "{path}: vm error (no Run produced - tier failed before execution):\n  {e}",
                    path = spec.path,
                ));
                continue;
            }
        };
        let other = match run_tier(spec, compiled) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!(
                    "{path}: {tier} error (no Run produced - tier failed before execution):\n  {e}",
                    path = spec.path,
                    tier = compiled.label(),
                ));
                continue;
            }
        };
        if let Some(d) = divergence(spec, (Tier::Vm, &vm), (compiled, &other)) {
            failures.push(d);
        }
        if compiled == Tier::Cranelift {
            let allowlisted = jit_compiles_nothing()
                .iter()
                .any(|(path, _)| path == spec.path);
            let installed = jit_installed_entries(&other.stderr).unwrap_or(0);
            if !allowlisted && installed == 0 {
                failures.push(format!(
                    "{path}: the cranelift tier installed no native entry, so this row \
                     compared the bytecode VM against itself. Run `GOS_JIT_TRACE=1 \
                     GOSSAMER_JIT_THRESHOLD=1 gos run {path}` to see which admission \
                     rule excluded every body, then either restore JIT coverage or add \
                     the fixture to jit_no_compile.tsv with the reason.",
                    path = spec.path,
                ));
            }
            if allowlisted && installed > 0 {
                failures.push(format!(
                    "{path}: listed in jit_no_compile.tsv but the cranelift tier \
                     installed {installed} native entries. Delete the row - this \
                     fixture now exercises the JIT.",
                    path = spec.path,
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} {} parity failures:\n{}",
        failures.len(),
        compiled.label(),
        failures.join("\n\n"),
    );
}

#[cfg(test)]
mod evidence_ledger_tests {
    use super::SPECS;

    /// The stdlib evidence ledger claims that every fixture it cites runs
    /// on each tier and each host in the CI matrix. That is only true of a
    /// program registered here, so the two must agree.
    #[test]
    fn every_cited_fixture_is_registered_for_tier_parity() {
        for (item, fixtures) in gossamer_std::manifest::feature_status::ITEM_FIXTURES {
            for fixture in *fixtures {
                assert!(
                    SPECS.iter().any(|spec| spec.path == *fixture),
                    "{item} cites {fixture}, which is not registered in SPECS, \
                     so it does not run across tiers or hosts"
                );
            }
        }
    }
}
