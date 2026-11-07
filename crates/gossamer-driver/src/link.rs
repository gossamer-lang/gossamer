//! Static linker + artifact assembly.
//! The production driver will spawn the host's linker (`lld`, `ld`,
//! `link.exe`) to combine per-crate object files with the bundled
//! runtime archive. That step is platform-specific and requires
//! shelling out, so the implementation instead models the
//! link as a deterministic in-memory artifact: every translation unit
//! contributes its symbol table and emitted CLIF text, the linker
//! merges them, and a fixed-format header identifies the artifact.
//! The resulting byte stream is stable across runs so the
//! reproducibility test can hash it.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use gossamer_codegen_cranelift::{FunctionText, Module};

/// Magic bytes identifying a Gossamer static artifact.
pub const ARTIFACT_MAGIC: &[u8; 8] = b"GOSARTv1";

/// One input to the linker: a logical module plus its symbol scope.
#[derive(Debug, Clone)]
pub struct TranslationUnit {
    /// Display name for diagnostics (usually the source-file path).
    pub name: String,
    /// Compiled module contents as rendered by
    /// [`gossamer_codegen_cranelift::emit_module`].
    pub module: Module,
}

/// Target triple, in canonical Rust form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetTriple(pub String);

impl TargetTriple {
    /// Canonical triple of the host build machine.
    #[must_use]
    pub fn host() -> Self {
        Self(gossamer_abi::target::Target::host().triple().to_string())
    }

    /// Returns the triple's textual representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Fully assembled artifact ready to be written to disk.
#[derive(Debug, Clone)]
pub struct Artifact {
    /// Entry-point symbol (typically `main`).
    pub entry: Option<String>,
    /// Target the artifact was assembled for.
    pub target: TargetTriple,
    /// Raw byte payload, including the magic header.
    pub bytes: Vec<u8>,
    /// Sorted symbol table for introspection.
    pub symbols: Vec<Symbol>,
}

/// Per-symbol linkage record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Symbol {
    /// Mangled name (`gos_<unit>_<name>`).
    pub mangled: String,
    /// Source unit that provided the symbol.
    pub unit: String,
    /// Original function name.
    pub name: String,
    /// Number of declared parameters.
    pub arity: u32,
    /// Number of basic blocks in the function body.
    pub blocks: u32,
}

/// Linker-level configuration.
#[derive(Debug, Clone)]
pub struct LinkerOptions {
    /// Entry-point symbol. `None` produces an artifact suitable for
    /// hosted use (shared library style).
    pub entry: Option<String>,
    /// Target triple driving the output header.
    pub target: TargetTriple,
    /// Prebuilt runtime archive to embed into the artifact. Provided
    /// by the toolchain for every supported target.
    pub runtime: Option<crate::target::PrebuiltRuntime>,
    /// Drop functions not reachable from the entry point before
    /// emitting the artifact. Off by default for backwards
    /// compatibility; the CLI sets it for release builds. Stream G.2.
    pub dead_code_elim: bool,
    /// Use a short hash-based symbol mangling instead of the
    /// verbose `gos_<unit>_<name>` form. Stream G.7.
    pub compact_symbols: bool,
}

impl Default for LinkerOptions {
    fn default() -> Self {
        let target = TargetTriple::host();
        let runtime = Some(crate::target::PrebuiltRuntime::stub(target.clone()));
        Self {
            entry: Some("main".to_string()),
            target,
            runtime,
            dead_code_elim: false,
            compact_symbols: false,
        }
    }
}

impl LinkerOptions {
    /// Returns a configuration targeting `triple` with the default
    /// stub runtime. Returns `None` when `triple` is not a registered
    /// target (see [`crate::target::REGISTERED_TARGETS`]).
    #[must_use]
    pub fn for_target(triple: &str) -> Option<Self> {
        let info = crate::target::lookup_target(triple)?;
        let target = info.triple.clone();
        Some(Self {
            entry: Some("main".to_string()),
            target: target.clone(),
            runtime: Some(crate::target::PrebuiltRuntime::stub(target)),
            dead_code_elim: false,
            compact_symbols: false,
        })
    }

    /// Returns a copy with DCE and compact mangling both enabled -
    /// the recommended shape for release builds.
    #[must_use]
    pub fn release(mut self) -> Self {
        self.dead_code_elim = true;
        self.compact_symbols = true;
        self
    }
}

/// Merges every [`TranslationUnit`] into a single deterministic
/// [`Artifact`].
#[must_use]
pub fn link(units: &[TranslationUnit], options: &LinkerOptions) -> Artifact {
    let effective_units: Vec<TranslationUnit> = if options.dead_code_elim {
        let reachable = reachable_symbols(units, options.entry.as_deref().unwrap_or("main"));
        dce_units(units, &reachable)
    } else {
        units.to_vec()
    };
    let mut table: BTreeMap<String, Symbol> = BTreeMap::new();
    for unit in &effective_units {
        for function in &unit.module.functions {
            let mangled = mangle(&unit.name, function, options.compact_symbols);
            let symbol = Symbol {
                mangled: mangled.clone(),
                unit: unit.name.clone(),
                name: function.name.clone(),
                arity: function.arity,
                blocks: function.block_count,
            };
            table.insert(mangled, symbol);
        }
    }
    let symbols: Vec<Symbol> = table.into_values().collect();
    let bytes = encode_artifact(options, &symbols, &effective_units);
    Artifact {
        entry: options.entry.clone(),
        target: options.target.clone(),
        bytes,
        symbols,
    }
}

fn mangle(unit: &str, function: &FunctionText, compact: bool) -> String {
    if compact {
        let digest = short_hash(&format!("{unit}::{}", function.name));
        format!("g{digest}")
    } else {
        format!(
            "gos_{unit}_{name}",
            unit = unit_tag(unit),
            name = function.name
        )
    }
}

fn unit_tag(unit: &str) -> String {
    unit.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

/// FNV-1a 64-bit hash rendered as 12-character lowercase hex. Used
/// by [`mangle`] when `compact_symbols` is on.
fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{:012x}", hash & 0xffff_ffff_ffff)
}

/// Set of `(unit, fn)` pairs reachable from `entry`. A pass runs a
/// simple textual search over each function's emitted body for
/// references to other functions - fine for the text-stub codegen,
/// accurate enough to identify the transitive closure.
fn reachable_symbols(units: &[TranslationUnit], entry: &str) -> BTreeMap<String, Vec<String>> {
    let mut function_by_name: BTreeMap<String, (String, &FunctionText)> = BTreeMap::new();
    for unit in units {
        for function in &unit.module.functions {
            function_by_name.insert(function.name.clone(), (unit.name.clone(), function));
        }
    }
    let mut reachable: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut stack: Vec<String> = vec![entry.to_string()];
    while let Some(name) = stack.pop() {
        let Some((unit_name, function)) = function_by_name.get(&name) else {
            continue;
        };
        let entry = reachable.entry(unit_name.clone()).or_default();
        if entry.iter().any(|n| n == &function.name) {
            continue;
        }
        entry.push(function.name.clone());
        for other in function_by_name.keys() {
            if other == &function.name {
                continue;
            }
            if function.text.contains(other.as_str()) {
                stack.push(other.clone());
            }
        }
    }
    reachable
}

fn dce_units(
    units: &[TranslationUnit],
    reachable: &BTreeMap<String, Vec<String>>,
) -> Vec<TranslationUnit> {
    units
        .iter()
        .filter_map(|unit| {
            let allowed = reachable.get(&unit.name)?;
            let functions: Vec<FunctionText> = unit
                .module
                .functions
                .iter()
                .filter(|f| allowed.iter().any(|name| name == &f.name))
                .cloned()
                .collect();
            if functions.is_empty() {
                return None;
            }
            Some(TranslationUnit {
                name: unit.name.clone(),
                module: gossamer_codegen_cranelift::Module { functions },
            })
        })
        .collect()
}

fn encode_artifact(
    options: &LinkerOptions,
    symbols: &[Symbol],
    units: &[TranslationUnit],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(ARTIFACT_MAGIC);
    push_len_prefixed(&mut out, options.target.as_str().as_bytes());
    match &options.entry {
        Some(entry) => push_len_prefixed(&mut out, entry.as_bytes()),
        None => push_len_prefixed(&mut out, b""),
    }
    if let Some(runtime) = &options.runtime {
        push_len_prefixed(&mut out, runtime.digest.as_bytes());
        push_len_prefixed(&mut out, &runtime.archive);
    } else {
        push_len_prefixed(&mut out, b"");
        push_len_prefixed(&mut out, b"");
    }
    push_u32(&mut out, u32::try_from(symbols.len()).unwrap_or(u32::MAX));
    for symbol in symbols {
        push_len_prefixed(&mut out, symbol.mangled.as_bytes());
        push_len_prefixed(&mut out, symbol.unit.as_bytes());
        push_len_prefixed(&mut out, symbol.name.as_bytes());
        push_u32(&mut out, symbol.arity);
        push_u32(&mut out, symbol.blocks);
    }
    // Sort units by name before serialising to keep the output
    // independent of input order.
    let mut sorted_units: Vec<&TranslationUnit> = units.iter().collect();
    sorted_units.sort_by(|a, b| a.name.cmp(&b.name));
    push_u32(
        &mut out,
        u32::try_from(sorted_units.len()).unwrap_or(u32::MAX),
    );
    for unit in sorted_units {
        push_len_prefixed(&mut out, unit.name.as_bytes());
        let mut fns: Vec<&FunctionText> = unit.module.functions.iter().collect();
        fns.sort_by(|a, b| a.name.cmp(&b.name));
        push_u32(&mut out, u32::try_from(fns.len()).unwrap_or(u32::MAX));
        for function in fns {
            push_len_prefixed(&mut out, function.name.as_bytes());
            push_len_prefixed(&mut out, function.text.as_bytes());
        }
    }
    out
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    push_u32(out, u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(bytes);
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Computes a deterministic fingerprint for an artifact, useful for
/// the reproducibility test.
#[must_use]
pub fn fingerprint(artifact: &Artifact) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in &artifact.bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}
