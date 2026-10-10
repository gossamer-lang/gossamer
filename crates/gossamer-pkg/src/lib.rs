//! Package manager for Gossamer.
//!
//! Reads `project.toml`, resolves transitive dependencies, fetches
//! every source kind into a content-addressed cache (with optional
//! disk persistence), pins the result to `project.lock`, and drives
//! the `gos publish` / `gos yank` flow for the registry.

// `deny`, not `forbid`: the crate is unsafe-free except for one audited
// Win32 ACL FFI block (`credentials::restrict_to_owner`, the Windows
// `chmod 0600` analogue) that carries a local `#[allow(unsafe_code)]`.
// `forbid` cannot be locally overridden; `deny` denies everywhere else.
#![deny(unsafe_code)]

// The Gossamer wasm playground links only the bytecode VM, which
// reaches `gossamer_pkg` for exactly one thing: `sha256` (a pure,
// self-contained hash used by `std::crypto` and the runtime crypto
// shims). The package-manager surface - registry transport, signing,
// tarball I/O, manifest/lockfile editing - is inert in a browser and
// pulls native-only crypto/network crates (rustls, ed25519-dalek), so
// it is gated out of the wasm build. Native is unaffected.
pub mod sha256;

// The toolchain version a manifest states is parsed as a `Version`, which
// the package surface above carries and the wasm build does not.
#[cfg(not(target_arch = "wasm32"))]
mod language_version;

#[cfg(not(target_arch = "wasm32"))]
pub use language_version::{parse_gossamer_version, toolchain_version};

/// Security advisories: the feed format and matching it against a
/// resolved dependency set. Native-only, like the version and JSON
/// machinery it reads a feed with.
#[cfg(not(target_arch = "wasm32"))]
pub mod advisory;

#[cfg(not(target_arch = "wasm32"))]
pub mod features;

/// Project source bundling: assembling an entry file plus its sibling
/// modules and path dependencies into one compilation unit. Reads the
/// project layout from disk, so it is native-only.
#[cfg(not(target_arch = "wasm32"))]
pub mod bundle;
/// Where each byte range of a bundled unit was read from. Plain data, so
/// a tool that never bundles still names it on every target.
pub mod bundled_span;

#[cfg(not(target_arch = "wasm32"))]
pub mod cache;
#[cfg(not(target_arch = "wasm32"))]
pub mod credentials;
#[cfg(not(target_arch = "wasm32"))]
pub mod edit;
/// Which file is a project's compilation root. Reads the project layout
/// from disk, so it is native-only.
#[cfg(not(target_arch = "wasm32"))]
pub mod entry;
#[cfg(not(target_arch = "wasm32"))]
pub mod fetch;
#[cfg(not(target_arch = "wasm32"))]
pub mod id;
#[cfg(not(target_arch = "wasm32"))]
pub mod lockfile;
#[cfg(not(target_arch = "wasm32"))]
pub mod manifest;
#[cfg(not(target_arch = "wasm32"))]
pub mod publish;
#[cfg(not(target_arch = "wasm32"))]
pub mod registry_server;
#[cfg(not(target_arch = "wasm32"))]
pub mod resolver;
#[cfg(not(target_arch = "wasm32"))]
pub mod scaffold;
#[cfg(not(target_arch = "wasm32"))]
pub mod signing;
#[cfg(not(target_arch = "wasm32"))]
pub mod tar;
#[cfg(not(target_arch = "wasm32"))]
pub mod transport;
#[cfg(not(target_arch = "wasm32"))]
pub mod version;

#[cfg(not(target_arch = "wasm32"))]
pub use cache::{Cache, CacheError, CachedSource, Fetched, default_cache_root};
#[cfg(not(target_arch = "wasm32"))]
pub use credentials::{Credential, CredentialStore, CredentialStoreError};
#[cfg(not(target_arch = "wasm32"))]
pub use edit::{add_registry, pin_to_resolved, remove, tidy};
#[cfg(not(target_arch = "wasm32"))]
pub use entry::{EntryError, enclosing_project_entry, resolve_project_entry};
#[cfg(not(target_arch = "wasm32"))]
pub use fetch::{FetchOptions, Fetcher, vendor};
#[cfg(not(target_arch = "wasm32"))]
pub use id::{ProjectId, ProjectIdError};
#[cfg(not(target_arch = "wasm32"))]
pub use lockfile::{LOCKFILE_FILENAME, LOCKFILE_HEADER, LockedEntry, Lockfile, LockfileError};
#[cfg(not(target_arch = "wasm32"))]
pub use manifest::{
    DependencyFeatures, DependencySpec, GitRef, InlineDependency, LibKind, LibTarget, Manifest,
    ManifestError, NativeSpec, ProjectTable, RustBindingSpec, find_manifest,
};
#[cfg(not(target_arch = "wasm32"))]
pub use publish::{
    PackError, PublishError, PublishedArtifact, StreamingArtifact, StreamingPublishRequest,
    pack_crate, pack_crate_streaming, pack_crate_streaming_with_limits, pack_crate_with_limits,
    upload_streaming_with,
};
#[cfg(not(target_arch = "wasm32"))]
pub use resolver::{
    CacheBackedLoader, CatalogueEntry, FnLoader, NoopLoader, Requirement, RequirementSpec,
    ResolveError, Resolved, ResolvedSource, Resolver, TransitiveLoader, VersionCatalogue,
    dependency_identity, resolve_transitive,
};
#[cfg(not(target_arch = "wasm32"))]
pub use scaffold::{render_initial_manifest, render_main_source};
#[cfg(not(target_arch = "wasm32"))]
pub use signing::{
    SigningError, SigningKey, VerifyingKey, hex_encode, sign_bytes, verify_bytes,
    verify_signature_hex,
};
#[cfg(not(target_arch = "wasm32"))]
pub use transport::{
    HttpTransport, HttpsTransport, StaticTransport, Transport, TransportError, fetch_verified,
};
#[cfg(not(target_arch = "wasm32"))]
pub use version::{Version, VersionBound, VersionError, VersionReq};
