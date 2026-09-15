//! High-level orchestration of the Gossamer compiler pipeline.
//! [`frontend::check_frontend`] is the single authoritative front-end
//! gate and [`frontend_cache`] makes a repeat pass over unchanged inputs
//! a deserialization. [`pipeline::compile_source`] chains the upstream
//! crates together and [`link::link`] turns the result into a
//! deterministic [`link::Artifact`]; package management and
//! cross-compilation hang new options off [`link::LinkerOptions`].

// `deny` rather than `forbid` so the single FFI liveness probe in
// `binding_runner::pid_alive` (`libc::kill` / Win32 `OpenProcess`) can opt in
// via a scoped, documented `#[allow(unsafe_code)]`. Every other site stays
// unsafe-free.
#![deny(unsafe_code)]

pub mod binding_runner;
pub mod cache_maintenance;
pub mod frontend;
pub mod frontend_cache;
pub mod link;
pub mod macos_deployment;
pub mod pipeline;
pub mod target;

pub use binding_runner::{
    BindingRunner, BindingRunnerError, DumpedItem, DumpedModule, DumpedType,
    Profile as RunnerProfile, RenderedBinding, SignatureDump, StaticBindingsLib,
    parse_signature_dump,
};

pub use frontend::{FrontendOutcome, check_frontend};
pub use frontend_cache::{
    CachedFrontend, FrontendCacheKey, cache_dir, cache_enabled, frontend_key, load_blob,
    load_blob_in, raw_blob_path, raw_blob_path_in, store_blob, store_blob_in, store_frontend,
    store_frontend_in, store_raw, store_raw_in, user_cache_root,
};
pub use link::{
    ARTIFACT_MAGIC, Artifact, LinkerOptions, Symbol, TargetTriple, TranslationUnit, fingerprint,
    link,
};
pub use pipeline::{
    CheckedFrontend, MirPhaseTimes, ReleaseBuild, ReleaseBuildPaths,
    compile_at_paths_from_frontend, compile_release_at_paths_from_frontend, compile_source,
    compile_source_native, compile_source_native_from_frontend,
    compile_source_native_from_frontend_at_path, compile_source_native_release,
    compile_source_native_release_with_fallback,
    compile_source_native_release_with_fallback_from_frontend, register_source_positions,
};
pub use target::{
    ObjectFormat, PrebuiltRuntime, REGISTERED_TARGETS, TargetInfo, all_targets, lookup_target,
};
