//! Workspace automation entry point.
//! Invoked via `cargo xtask <command>`. Current subcommands:
//! - `docs-stdlib` - regenerate `docs_src/stdlib.md` from the
//!   `gossamer_std` manifest so the rendered reference page tracks
//!   the single source of truth.
//! - `docs-lints` - regenerate `docs_src/toolchain/lints.md` from the
//!   `gossamer-lint` crate's `DAY_ONE_LINTS` + `lint_explanation`.
//! - `docs-diagnostics` - regenerate `docs_src/toolchain/diagnostics.md`
//!   from the curated catalogue in this file.
//! - `stdlib-coverage` - regenerate `docs_src/stdlib_coverage.md`
//!   from the per-module support state recorded in
//!   `STDLIB_SUPPORT`.
//! - `docs-llm` - regenerate the checked-in, machine-readable
//!   public-stdlib catalogue for LLM and MCP consumers.
//! - `docs-all` - run every generator above in one invocation.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gossamer_std::{StdItemKind, StdModule, modules};
use serde::Serialize;

/// Entry point that dispatches to the requested xtask subcommand.
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {
            println!("usage: cargo xtask <subcommand>");
            println!("subcommands:");
            println!("  docs-stdlib         regenerate docs_src/stdlib.md");
            println!("  docs-lints          regenerate docs_src/toolchain/lints.md");
            println!("  docs-diagnostics    regenerate docs_src/toolchain/diagnostics.md");
            println!("  stdlib-coverage     regenerate docs_src/stdlib_coverage.md");
            println!("  docs-llm [--check]  regenerate or check docs_src/api/stdlib.json");
            println!("  docs-all            run every docs generator");
            println!("  item-fixtures [--check]  regenerate the ITEM_FIXTURES ledger");
            println!("  lint-budget         tally #[allow(...)] sites per crate");
            println!("  audit-allows        list every #[allow(...)] with surrounding context");
            println!("  migrate-struct-constructors <PATH>  rewrite braced struct constructors");
            Ok(())
        }
        Some("docs-stdlib") => regenerate_stdlib_docs(),
        Some("docs-lints") => regenerate_lint_docs(),
        Some("docs-diagnostics") => regenerate_diagnostic_docs(),
        Some("stdlib-coverage") => regenerate_stdlib_coverage(),
        Some("docs-llm") => regenerate_llm_docs(args.get(1).map(String::as_str) == Some("--check")),
        Some("docs-all") => {
            regenerate_stdlib_docs()?;
            regenerate_lint_docs()?;
            regenerate_diagnostic_docs()?;
            regenerate_stdlib_coverage()?;
            regenerate_llm_docs(false)
        }
        Some("item-fixtures") => {
            regenerate_item_fixtures(args.get(1).map(String::as_str) == Some("--check"))
        }
        Some("lint-budget") => report_lint_budget(),
        Some("audit-allows") => audit_allows(),
        Some("migrate-struct-constructors") => {
            let path = args
                .get(1)
                .context("migrate-struct-constructors requires a path")?;
            migrate_struct_constructors(Path::new(path))
        }
        Some(other) => {
            eprintln!("xtask: unknown subcommand {other:?}");
            std::process::exit(2);
        }
    }
}

fn migrate_struct_constructors(path: &Path) -> Result<()> {
    let mut files = Vec::new();
    collect_gos_files(path, &mut files)?;
    for file in files {
        let source =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let mut map = gossamer_lex::SourceMap::new();
        let file_id = map.add_file(file.display().to_string(), source.clone());
        let migrated =
            gossamer_parse::autoderive::migrate_braced_struct_constructors(&source, file_id)
                .map_err(|diags| {
                    anyhow::anyhow!("{} parse error(s) in {}", diags.len(), file.display())
                })?;
        if migrated != source {
            fs::write(&file, migrated).with_context(|| format!("writing {}", file.display()))?;
            println!("xtask: migrated {}", file.display());
        }
    }
    Ok(())
}

fn collect_gos_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        if path.extension().is_some_and(|ext| ext == "gos") {
            out.push(path.to_path_buf());
        }
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry?;
        collect_gos_files(&entry.path(), out)?;
    }
    Ok(())
}

/// Rewrites `docs_src/stdlib.md` using the data in
/// [`gossamer_std::modules`]. The generated page starts with a
/// marker line so the regeneration is idempotent and consumers can
/// diff against the previous version.
fn regenerate_stdlib_docs() -> Result<()> {
    let workspace_root = locate_workspace_root()?;
    let out_path = workspace_root.join("docs_src/stdlib.md");
    let page = render_stdlib_page(modules());
    fs::write(&out_path, page).with_context(|| format!("writing {}", out_path.display()))?;
    println!("xtask: wrote {}", out_path.display());
    Ok(())
}

/// Walks parent directories from `CARGO_MANIFEST_DIR` until it finds
/// one containing a workspace-root `Cargo.toml`.
fn locate_workspace_root() -> Result<PathBuf> {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").map_or_else(|_| PathBuf::from("."), PathBuf::from);
    let mut cursor: &Path = &manifest_dir;
    loop {
        if cursor.join("Cargo.lock").exists() {
            return Ok(cursor.to_path_buf());
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => {
                anyhow::bail!(
                    "could not locate workspace root from {}",
                    manifest_dir.display()
                );
            }
        }
    }
}

/// Versioned, deterministic catalogue consumed by documentation tools.
///
/// The manifest owns the public name, kind, and description. The checker owns
/// function signatures. Entries that cannot yet meet the runnable-example
/// contract remain in this catalogue but are explicitly marked `catalog_only`;
/// they must not be copied into `llms-full.txt` as if they were verified.
#[derive(Debug, Serialize)]
struct PublicApiCatalog {
    schema_version: u32,
    entries: Vec<PublicApiEntry>,
}

/// One joined source-facing public API record.
#[derive(Debug, Serialize)]
struct PublicApiEntry {
    /// Stable identifier for MCP and diagnostic links.
    id: String,
    /// Canonical, fully-qualified Gossamer path.
    name: String,
    /// Source item classification, never inferred from a Rust `pub` item.
    kind: &'static str,
    /// Checker-owned source signature for functions; absent means no guess.
    signature: Option<&'static str>,
    /// Manifest-owned one-sentence description.
    description: &'static str,
    /// Lifecycle status inherited from the canonical manifest record.
    lifecycle: &'static str,
    /// Item-level tier evidence is not available yet, so this is explicit.
    tier_support: [&'static str; 3],
    /// Platform evidence is not available at item granularity yet.
    platform_support: &'static str,
    /// Documented resource or semantic limits, when item metadata gains them.
    limits: Vec<String>,
    /// Intent-oriented cookbook identifiers, when verified recipes land.
    cookbook_tags: Vec<String>,
    /// Stable docs source anchor.
    doc_anchor: String,
    /// Stable runnable fixture ID; absent entries cannot enter the full reference.
    example_id: Option<String>,
    /// `catalog_only` until a standalone checked/run/built example exists.
    reference_status: &'static str,
}

/// Regenerates, or byte-checks, the deterministic public stdlib catalogue.
fn regenerate_llm_docs(check: bool) -> Result<()> {
    let workspace_root = locate_workspace_root()?;
    let catalogue = build_public_api_catalog()?;
    let skill_card = fs::read_to_string(workspace_root.join("SKILL.md"))
        .context("reading checked LLM primer SKILL.md")?;
    // The catalogue lives under the mkdocs `docs_dir` (`docs_src`), not the
    // rendered `site_dir` (`docs`): mkdocs clears `site_dir` on every build and
    // only restores what it renders itself. Being a docs source keeps these
    // files published at `docs/api/*.json` while mkdocs copies them forward.
    let outputs = vec![
        (
            workspace_root.join("docs_src/api/stdlib.json"),
            serialize_public_api_catalog(&catalogue)?,
        ),
        (
            workspace_root.join("docs_src/api/cookbook.json"),
            render_empty_cookbook_catalog(),
        ),
        (
            workspace_root.join("llms.txt"),
            render_llms_index(&catalogue),
        ),
        (
            workspace_root.join("llms-full.txt"),
            render_llms_full(&catalogue, &skill_card)?,
        ),
    ];
    if check {
        let drift: Vec<String> = outputs
            .iter()
            .filter(|(path, generated)| {
                fs::read_to_string(path).map_or(true, |on_disk| on_disk != *generated)
            })
            .map(|(path, _)| path.display().to_string())
            .collect();
        if drift.is_empty() {
            println!("xtask: docs-llm is in sync ({} files)", outputs.len());
            return Ok(());
        }
        anyhow::bail!(
            "LLM documentation drift detected: {} (run `cargo xtask docs-llm`)",
            drift.join(", ")
        );
    }
    for (path, generated) in &outputs {
        let parent = path.parent().expect("generated file has a parent");
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
        fs::write(path, generated).with_context(|| format!("writing {}", path.display()))?;
        println!("xtask: wrote {}", path.display());
    }
    let full_bytes = outputs[3].1.len();
    println!(
        "xtask: docs-llm catalogue={} entries, full-reference={} bytes (~{} tokens)",
        catalogue.entries.len(),
        full_bytes,
        full_bytes.div_ceil(4)
    );
    Ok(())
}

/// Joins canonical manifest records to checker signatures without scraping Rust
/// implementation details. A missing or duplicate function signature aborts
/// generation, making documentation drift a build failure rather than a guess.
fn build_public_api_catalog() -> Result<PublicApiCatalog> {
    let records = gossamer_std::item_records();
    let function_paths: std::collections::HashSet<String> = records
        .iter()
        .filter(|item| item.kind == StdItemKind::Function)
        .map(|item| item.path.clone())
        .collect();
    let mut seen_signatures = std::collections::HashSet::new();
    for signature in gossamer_types::STD_FUNCTION_SIGNATURES {
        let path = format!("{}::{}", signature.module_path, signature.name);
        if !seen_signatures.insert(path.clone()) {
            anyhow::bail!("duplicate checker signature for {path}");
        }
        if !function_paths.contains(&path) {
            anyhow::bail!("checker signature has no canonical manifest function: {path}");
        }
    }

    let mut entries = Vec::new();
    for item in records {
        let signature = if item.kind == StdItemKind::Function {
            Some(
                gossamer_types::stdlib_function_signature(item.module_path, item.name)
                    .with_context(|| format!("missing checker signature for {}", item.path))?,
            )
        } else {
            None
        };
        entries.push(PublicApiEntry {
            id: public_api_id(&item.path),
            name: item.path,
            kind: public_api_kind(item.kind),
            signature,
            description: item.doc,
            lifecycle: item.status.tag(),
            tier_support: ["not_audited", "not_audited", "not_audited"],
            platform_support: "not_audited",
            limits: Vec::new(),
            cookbook_tags: Vec::new(),
            doc_anchor: format!("docs_src/stdlib.md#{}", module_anchor(item.module_path)),
            example_id: None,
            reference_status: "catalog_only",
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    if entries.windows(2).any(|pair| pair[0].name == pair[1].name) {
        anyhow::bail!("duplicate canonical public API name in manifest");
    }
    Ok(PublicApiCatalog {
        schema_version: 1,
        entries,
    })
}

/// Serializes the public catalogue with no timestamp or other volatile value.
fn serialize_public_api_catalog(catalogue: &PublicApiCatalog) -> Result<String> {
    let mut json =
        serde_json::to_string_pretty(catalogue).context("serializing LLM API catalogue")?;
    json.push('\n');
    Ok(json)
}

/// Compatibility helper for callers/tests that need the checked-in JSON bytes.
#[cfg(test)]
fn render_public_api_catalog() -> Result<String> {
    serialize_public_api_catalog(&build_public_api_catalog()?)
}

/// Empty, versioned recipe registry. Recipes are intentionally absent until
/// their standalone check/run/build fixtures exist; this keeps the LLM index
/// link-stable without pretending that a prose snippet is verified.
fn render_empty_cookbook_catalog() -> String {
    "{\n  \"schema_version\": 1,\n  \"recipes\": []\n}\n".to_string()
}

/// Generates the small root LLM discovery index rather than a second primer.
fn render_llms_index(catalogue: &PublicApiCatalog) -> String {
    format!(
        "# Gossamer\n\nGossamer is a Rust-flavoured language with goroutines and deterministic memory management; use `gos` to check, execute, build, test, format, and query programs. This generated index points models and agents at the reviewed primer and canonical API data.\n\n- [Compact LLM reference](llms-full.txt)\n- [Reviewed skill card](SKILL.md)\n- [Machine-readable stdlib API catalog](docs_src/api/stdlib.json) ({entries} entries)\n- [Cookbook registry](docs_src/api/cookbook.json) (recipes appear only after fixture verification)\n- [Language specification](SPEC.md)\n- [Examples](examples/)\n- [Examples guide](docs_src/examples.md)\n\n## Tooling\n\nUse `gos check FILE` before `gos run FILE`; validate compiled behavior with `gos build FILE`. For agent integration, run `gos mcp` over stdio, then use its `check`, `execute`, `build`, `doc`, `explain`, and semantic-navigation tools.\n",
        entries = catalogue.entries.len()
    )
}

/// Maximum size for the pasteable reference. The cap leaves room for a compact
/// primer while preventing an accidental docs-site dump from entering context.
const MAX_LLM_FULL_BYTES: usize = 200_000;

/// Renders the compact, scoped LLM reference. An API entry requires a stable
/// example ID before it is emitted here; catalog-only entries remain available
/// to structured tooling but are not elevated to a prose recommendation.
fn render_llms_full(catalogue: &PublicApiCatalog, skill_card: &str) -> Result<String> {
    let verified_entries = catalogue
        .entries
        .iter()
        .filter(|entry| entry.example_id.is_some() && entry.reference_status == "verified")
        .count();
    let mut out = format!(
        "# Gossamer LLM Reference\n\nGenerated from `SKILL.md` and `docs_src/api/stdlib.json`. This is deliberately scoped: only catalog entries with a standalone executable example and explicit verified status may appear as API recommendations.\n\n## Language primer\n\n{skill_card}\n\n## Verified API reference\n\n"
    );
    if verified_entries == 0 {
        out.push_str(
            "No stdlib entry is eligible for this section yet. Consult the structured \
             [`docs_src/api/stdlib.json`](docs_src/api/stdlib.json) catalog with `gos check`/`gos`; \
             its entries are deliberately quarantined until their fixtures and tier evidence land.\n",
        );
    }
    if out.len() > MAX_LLM_FULL_BYTES {
        anyhow::bail!(
            "llms-full.txt is {} bytes, above the {} byte budget",
            out.len(),
            MAX_LLM_FULL_BYTES
        );
    }
    Ok(out)
}

#[cfg(test)]
mod llm_catalog_tests {
    use super::*;

    #[test]
    fn public_api_catalog_is_deterministic_and_has_no_guessed_function_signatures() {
        let json = render_public_api_catalog().expect("manifest/signature join must be valid");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["schema_version"], 1);
        let entries = value["entries"].as_array().expect("entries array");
        assert!(entries.windows(2).all(|pair| {
            pair[0]["name"].as_str().expect("entry name")
                < pair[1]["name"].as_str().expect("entry name")
        }));
        assert!(
            entries
                .iter()
                .all(|entry| { entry["kind"] != "function" || entry["signature"].is_string() })
        );
        assert!(entries.iter().all(|entry| entry["example_id"].is_null()));
        assert!(
            entries
                .iter()
                .all(|entry| entry["reference_status"] == "catalog_only")
        );
    }

    #[test]
    fn scoped_llm_outputs_link_to_the_catalog_without_promoting_quarantined_entries() {
        let catalogue = build_public_api_catalog().expect("valid catalogue");
        let index = render_llms_index(&catalogue);
        for link in [
            "llms-full.txt",
            "SKILL.md",
            "docs_src/api/stdlib.json",
            "docs_src/api/cookbook.json",
            "SPEC.md",
            "examples/",
        ] {
            assert!(index.contains(link), "index is missing {link}");
        }
        assert_eq!(
            render_empty_cookbook_catalog(),
            "{\n  \"schema_version\": 1,\n  \"recipes\": []\n}\n"
        );
        let full = render_llms_full(&catalogue, "# Reviewed primer\n")
            .expect("compact reference within size budget");
        assert!(full.len() <= MAX_LLM_FULL_BYTES);
        assert!(full.contains("No stdlib entry is eligible"));
    }
}

/// Converts a canonical public path into a stable, URL-safe identifier.
fn public_api_id(path: &str) -> String {
    path.replace("::", "-").replace('_', "-")
}

/// JSON spelling for the manifest's closed item-kind enum.
const fn public_api_kind(kind: StdItemKind) -> &'static str {
    match kind {
        StdItemKind::Function => "function",
        StdItemKind::Type => "type",
        StdItemKind::Trait => "trait",
        StdItemKind::Builtin => "builtin",
        StdItemKind::Const => "const",
    }
}

/// Tallies `#[allow(...)]` and `#![allow(...)]` sites across the
/// workspace by crate. Prints a one-line-per-crate summary plus a
/// total. Used as a regression gauge: every PR that adds an `allow`
/// owes a reason comment, and the total should not climb.
fn report_lint_budget() -> Result<()> {
    let workspace_root = locate_workspace_root()?;
    let crates_dir = workspace_root.join("crates");
    let mut crate_dirs: Vec<PathBuf> = fs::read_dir(&crates_dir)
        .with_context(|| format!("read {}", crates_dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    crate_dirs.push(workspace_root.join("xtask"));
    crate_dirs.sort();
    let mut grand_total = 0usize;
    let mut rows: Vec<(String, usize)> = Vec::new();
    for dir in &crate_dirs {
        let count = count_allows_in(dir)?;
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        rows.push((name, count));
        grand_total += count;
    }
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let pad = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    println!("crate{:width$}  allows", "", width = pad.saturating_sub(5));
    for (name, count) in &rows {
        if *count == 0 {
            continue;
        }
        println!("{name:<pad$}  {count:>5}");
    }
    println!("{:-<width$}", "", width = pad + 9);
    println!("{:<pad$}  {grand_total:>5}", "total");
    Ok(())
}

/// Walks every `*.rs` file under `dir` and counts attribute `#[allow(`
/// occurrences (item- and crate-level). Approximates the real total -
/// macros may expand into more - but is the right surface to track at
/// the source level.
fn count_allows_in(dir: &Path) -> Result<usize> {
    let mut total = 0usize;
    for entry in walk_rs_files(dir) {
        let body =
            fs::read_to_string(&entry).with_context(|| format!("read {}", entry.display()))?;
        for line in body.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("#[allow(") || trimmed.starts_with("#![allow(") {
                total += 1;
            }
        }
    }
    Ok(total)
}

/// Prints every `#[allow(...)]` / `#![allow(...)]` site in the
/// workspace with a few lines of context, so reviewers can verify
/// each one carries a `reason = "..."` justification or a comment.
fn audit_allows() -> Result<()> {
    let workspace_root = locate_workspace_root()?;
    let mut targets: Vec<PathBuf> =
        vec![workspace_root.join("crates"), workspace_root.join("xtask")];
    targets.sort();
    for root in &targets {
        for entry in walk_rs_files(root) {
            let rel = entry
                .strip_prefix(&workspace_root)
                .unwrap_or(entry.as_path());
            let Ok(body) = fs::read_to_string(&entry) else {
                continue;
            };
            let lines: Vec<&str> = body.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("#[allow(") || trimmed.starts_with("#![allow(") {
                    println!("--- {}:{} ---", rel.display(), i + 1);
                    let from = i.saturating_sub(2);
                    let to = (i + 4).min(lines.len());
                    for (j, src) in lines[from..to].iter().enumerate() {
                        let marker = if from + j == i { ">>" } else { "  " };
                        println!("{marker} {:>4}  {src}", from + j + 1);
                    }
                    println!();
                }
            }
        }
    }
    Ok(())
}

/// Yields every `*.rs` path under `root`, skipping `target/` and any
/// dot-prefixed directory.
fn walk_rs_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if path.is_dir() {
                if name == "target" || name.starts_with('.') {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Renders the stdlib reference page as Markdown.
fn render_stdlib_page(modules: &[StdModule]) -> String {
    let mut out = String::new();
    writeln!(out, "<!-- generated by `cargo xtask docs-stdlib` -->").unwrap();
    writeln!(out, "# Standard library").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "This is the Gossamer {version} standard library reference.\n\
         Gossamer's standard library ships as a Rust-implemented host\n\
         crate (`gossamer-std`) with a manifest describing every\n\
         module and item. This page is auto-generated from that\n\
         manifest via `cargo xtask docs-stdlib`; hand edits are\n\
         overwritten on the next regeneration.",
        version = env!("CARGO_PKG_VERSION")
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "The manifest itself lives at [`crates/gossamer-std/src/manifest.rs`](\
         https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/manifest.rs)."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Names available without any import - the print macros, \
         `min`/`max`/`clamp`, `spawn`, assertions, and the synthesized \
         `from_json::<T>` family - are listed on the \
         [Prelude page](prelude.md)."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Every module listed below requires an explicit `use`, such as \
         `use std::env` before calling `env::args()`. Importing one module \
         does not implicitly import sibling modules or its individual functions."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Receiver methods on built-in types such as `String`, `Vec`, \
         `Map`, `Option`, and `Result` are listed in \
         [Methods by type](method_support.md)."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "## Modules").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Module | Items | Summary |").unwrap();
    writeln!(out, "|--------|------:|---------|").unwrap();
    let mut sorted: Vec<&StdModule> = modules.iter().collect();
    sorted.sort_by_key(|m| m.path);
    for module in &sorted {
        writeln!(
            out,
            "| [`{path}`](#{anchor}) | {count} | {summary} |",
            path = module.path,
            anchor = module_anchor(module.path),
            count = module.items.len(),
            summary = module.summary,
        )
        .unwrap();
    }
    writeln!(out).unwrap();
    for module in &sorted {
        write_module_section(&mut out, module);
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Emits one module's detail block - heading, summary, item table.
fn write_module_section(out: &mut String, module: &StdModule) {
    writeln!(out, "## `{}`", module.path).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{}", module.summary).unwrap();
    writeln!(out).unwrap();
    if module.items.is_empty() {
        writeln!(out, "*No items exported yet.*").unwrap();
        writeln!(out).unwrap();
        return;
    }
    writeln!(out, "| Item | Kind | Doc |").unwrap();
    writeln!(out, "|------|------|-----|").unwrap();
    let mut items: Vec<&gossamer_std::StdItem> = module.items.iter().collect();
    items.sort_by_key(|i| (kind_rank(i.kind), i.name));
    for item in items {
        writeln!(
            out,
            "| `{name}` | {kind} | {doc} |",
            name = item.name,
            kind = kind_label(item.kind),
            doc = item.doc,
        )
        .unwrap();
    }
    writeln!(out).unwrap();
}

/// Slug for a module path matching python-markdown's `toc` anchor for the
/// section heading. `toc` keeps `[A-Za-z0-9_-]` (underscore is a word
/// character), so module paths like `std::collections::ordered_vec` must
/// retain the `_` to resolve against the generated `#stdcollectionsordered_vec`.
fn module_anchor(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            'A'..='Z' => out.push(ch.to_ascii_lowercase()),
            'a'..='z' | '0'..='9' | '-' | '_' => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Human-friendly column label for a manifest `StdItemKind`.
fn kind_label(kind: StdItemKind) -> &'static str {
    match kind {
        StdItemKind::Function => "fn",
        StdItemKind::Type => "type",
        StdItemKind::Trait => "trait",
        StdItemKind::Builtin => "builtin",
        StdItemKind::Const => "const",
    }
}

/// Sort weight so item tables list types first, then traits, then
/// functions, then macros, then constants - matches the progression
/// a reader scanning for "what's here" expects.
fn kind_rank(kind: StdItemKind) -> u8 {
    match kind {
        StdItemKind::Type => 0,
        StdItemKind::Trait => 1,
        StdItemKind::Function => 2,
        StdItemKind::Builtin => 3,
        StdItemKind::Const => 4,
    }
}

/// Rewrites `docs_src/toolchain/lints.md` from the lint crate's
/// day-one ID list and `lint_explanation` entries.
fn regenerate_lint_docs() -> Result<()> {
    let workspace_root = locate_workspace_root()?;
    let out_dir = workspace_root.join("docs_src/toolchain");
    fs::create_dir_all(&out_dir).with_context(|| format!("mkdir {}", out_dir.display()))?;
    let out_path = out_dir.join("lints.md");
    let page = render_lints_page(gossamer_lint::DAY_ONE_LINTS);
    fs::write(&out_path, page).with_context(|| format!("writing {}", out_path.display()))?;
    println!("xtask: wrote {}", out_path.display());
    Ok(())
}

fn render_lints_page(ids: &[&str]) -> String {
    let mut out = String::new();
    writeln!(out, "<!-- generated by `cargo xtask docs-lints` -->").unwrap();
    writeln!(out, "# Lints").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "The Gossamer linter ships {n} day-one checks. Each has a short\n\
         identifier suitable for `#[lint(allow(...))]` and a long-form\n\
         explanation available via `gos lint --explain <id>`. This page\n\
         is auto-generated from `gossamer-lint`; hand edits are\n\
         overwritten on the next run of `cargo xtask docs-lints`.",
        n = ids.len()
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Code | Identifier | Default |").unwrap();
    writeln!(out, "|------|------------|---------|").unwrap();
    for id in ids {
        let code = gossamer_lint::lint_code(id);
        let default = default_level_for(id);
        writeln!(out, "| `{code}` | [`{id}`](#{id}) | {default} |").unwrap();
    }
    writeln!(out).unwrap();
    for id in ids {
        writeln!(out, "## `{id}`").unwrap();
        writeln!(out).unwrap();
        match gossamer_lint::lint_explanation(id) {
            Some(explanation) => {
                for line in explanation.lines() {
                    writeln!(out, "{}", line.trim_start()).unwrap();
                }
            }
            None => {
                writeln!(out, "*No explanation registered.*").unwrap();
            }
        }
        writeln!(out).unwrap();
    }
    out
}

fn default_level_for(id: &str) -> &'static str {
    let registry = gossamer_lint::Registry::with_defaults();
    match registry.level(id) {
        gossamer_lint::Level::Deny => "deny",
        gossamer_lint::Level::Warn => "warn",
        gossamer_lint::Level::Allow => "allow",
    }
}

/// Rewrites `docs_src/toolchain/diagnostics.md` from the catalogue
/// of diagnostic codes emitted by the compiler phases.
fn regenerate_diagnostic_docs() -> Result<()> {
    let workspace_root = locate_workspace_root()?;
    let out_dir = workspace_root.join("docs_src/toolchain");
    fs::create_dir_all(&out_dir).with_context(|| format!("mkdir {}", out_dir.display()))?;
    let out_path = out_dir.join("diagnostics.md");
    let page = render_diagnostics_page(DIAGNOSTIC_CATALOGUE);
    fs::write(&out_path, page).with_context(|| format!("writing {}", out_path.display()))?;
    println!("xtask: wrote {}", out_path.display());
    Ok(())
}

/// Catalogue of diagnostic codes emitted by the compiler. Each entry
/// is `(code, phase, title, explanation)`. Keep in sync with the
/// emitters in `gossamer-parse`, `gossamer-resolve`, `gossamer-types`,
/// and `gossamer-pkg`.
const DIAGNOSTIC_CATALOGUE: &[(&str, &str, &str, &str)] = &[
    (
        "GP0001",
        "Parser",
        "unexpected token",
        "The parser saw a token where it expected a different one. Check for missing punctuation, an unmatched delimiter, or an out-of-place keyword.",
    ),
    (
        "GP0002",
        "Parser",
        "unexpected end of input",
        "The parser reached end-of-file in the middle of a construct. Finish the expression, statement, or item - or remove it.",
    ),
    (
        "GP0003",
        "Parser",
        "unterminated delimiter",
        "A balanced construct (block, tuple, array, string literal) was left unterminated. Add the matching closing delimiter.",
    ),
    (
        "GP0004",
        "Parser",
        "chained comparison without parentheses",
        "Comparison operators like `==` / `!=` / `<` are not associative. Parenthesise the operands: `(a == b) && (b == c)`.",
    ),
    (
        "GP0005",
        "Parser",
        "chained range operator",
        "Range operators (`..`, `..=`) are not associative. Parenthesise the operands: `(a..b)..c`.",
    ),
    (
        "GP0006",
        "Parser",
        "struct literal in scrutinee",
        "A braced struct literal in the scrutinee of `if`/`while`/`match` is ambiguous with the block. Wrap the literal in `(...)`.",
    ),
    (
        "GP0007",
        "Parser",
        "pipe right-hand side not callable",
        "The right-hand side of `|>` must be a callable: a function reference, a method call, or a closure.",
    ),
    (
        "GP0008",
        "Parser",
        "assignment outside statement position",
        "Assignment (`=`, `+=`, …) only appears at statement position. If you need an expression, return the right-hand side directly.",
    ),
    (
        "GP0009",
        "Parser",
        "expected integer literal",
        "An integer literal is required at this position.",
    ),
    (
        "GP0010",
        "Parser",
        "expected string literal",
        "A string literal is required at this position.",
    ),
    (
        "GP0011",
        "Parser",
        "invalid tuple index",
        "A tuple index must be a plain decimal integer (`p.0`, `p.1`). Hex, binary, or octal indices are not accepted.",
    ),
    (
        "GP0012",
        "Parser",
        "malformed label",
        "A label identifier is required after the leading `'`.",
    ),
    (
        "GP0013",
        "Parser",
        "malformed attribute",
        "An attribute is malformed. Accepted forms are `#[attr]`, `#[attr(args)]`, and `#[attr = value]`.",
    ),
    (
        "GP0014",
        "Parser",
        "malformed `use` declaration",
        "A `use` declaration could not be parsed. Check the path for stray punctuation or an unfinished brace list.",
    ),
    (
        "GP0015",
        "Parser",
        "unexpected construct",
        "Two consecutive tokens formed something the parser does not recognise.",
    ),
    (
        "GP0016",
        "Parser",
        "reserved `extern` keyword",
        "The `extern` keyword is reserved but has no source-level item form. Gossamer's FFI surface is the `[rust-bindings]` section of `project.toml` plus the `gossamer-binding` crate.",
    ),
    (
        "GP0017",
        "Parser",
        "parser recursion limit",
        "An expression exceeded the parser's nesting limit. Split it into smaller helpers.",
    ),
    (
        "GP0018",
        "Lexer",
        "malformed token",
        "The lexer rejected a malformed string, comment, escape, or token spelling.",
    ),
    (
        "GP0019",
        "Parser",
        "statement outside entry file",
        "Executable statements belong in the entry file or inside a function, not in a module body.",
    ),
    (
        "GP0020",
        "Parser",
        "mixed entry forms",
        "An entry file cannot combine bare top-level statements with an explicit `fn main`.",
    ),
    (
        "GP0021",
        "Parser",
        "malformed format placeholder",
        "A format placeholder must be a binding name, format specification, or positional placeholder.",
    ),
    (
        "GP0022",
        "Parser",
        "unserializable derived field",
        "Automatic serialization cannot be generated for a field with an unsupported type.",
    ),
    (
        "GP0023",
        "Parser",
        "format argument count mismatch",
        "The number of positional arguments must equal the number of positional placeholders.",
    ),
    (
        "GP0024",
        "Parser",
        "non-literal format template",
        "Format macros require a literal template so placeholders can be checked at compile time.",
    ),
    (
        "GP0026",
        "Parser",
        "inclusive range missing upper bound",
        "The inclusive range operator `..=` requires an upper bound. Use `..` for an open upper end.",
    ),
    (
        "GP0027",
        "Parser",
        "`$` is not part of the language",
        "`$` used to spell the slot a `|>` step filled. A step that needs the value in a particular slot is a closure, as in `x |> |v| f(a, v)`; a method already chains, as in `x.trim()`; a callback is a closure or a function named in value position.",
    ),
    (
        "GP0029",
        "Parser",
        "match arm missing arrow",
        "Add `=>` after the match arm pattern and optional guard.",
    ),
    (
        "GP0030",
        "Parser",
        "match arm missing body",
        "Add the expression or block produced by the match arm.",
    ),
    (
        "GP0031",
        "Parser",
        "match arm missing separator",
        "Separate same-line expression arms with a comma, or start the next arm on a new line.",
    ),
    (
        "GP0032",
        "Parser",
        "removed collection literal",
        "A bracket spelling that used to build a container is no longer syntax. Construct the container through its type: `Type::new()` or `Type::from([a, b, c])`.",
    ),
    (
        "GP0033",
        "Parser",
        "text after a triple-quote opener",
        "A multi-line `\"\"\"` literal carried text on the same line as its opening delimiter. The body starts on the next line, and the indentation it shares with the closing `\"\"\"` is stripped from every line.",
    ),
    (
        "GP0034",
        "Parser",
        "missing item type",
        "A `const` or `static` item was declared without a type annotation. These items are never inferred from their initialiser, so the type is written after the name: `const EPS: f64 = 1e-12`.",
    ),
    (
        "GP0035",
        "Parser",
        "duplicate slice-pattern rest",
        "A slice pattern wrote more than one `..`. One rest binding splits the elements into a prefix and a suffix, as in `[first, ..rest, last]`.",
    ),
    (
        "GP0036",
        "Parser",
        "duplicate struct-literal spread",
        "A struct literal wrote more than one `..base` functional update. Keep a single base value and list every overridden field explicitly.",
    ),
    (
        "GP0037",
        "Parser",
        "refutable let without else",
        "A `let` whose pattern can fail to match was written without an `else` block. Give the failure a diverging path: `let Some(x) = opt else { return }`.",
    ),
    (
        "GP0038",
        "Parser",
        "unsupported visibility restriction",
        "The three visibilities are private (no annotation), `pub(package)`, and `pub`.",
    ),
    (
        "GP0039",
        "Parser",
        "serde turbofish names an uncovered type",
        "A codec is synthesized per concrete struct whose fields the synthesizer can classify. Exchange a concrete struct, read the document dynamically with `json::parse`, or hand-write the function.",
    ),
    (
        "GP0040",
        "Parser",
        "hyphen in a `use` path",
        "`-` is subtraction, never part of an identifier. A dependency's module name is its package name with each `-` replaced by `_`, so `pgsql-gos` is imported as `use pgsql_gos`.",
    ),
    (
        "GP0041",
        "Parser",
        "pipe step with arguments is not a closure",
        "A `|>` step that writes its own arguments leaves the piped value no slot. Write it as a closure whose parameter is that slot: `x |> |v| f(a, v)`.",
    ),
    (
        "GP0042",
        "Parser",
        "targets written as a parenthesised tuple",
        "A binding and an assignment both list their targets without parentheses: `let a, b = value` and `a, b = x, y`. Parentheses group a pattern that sits beside others - a `match` arm, a `for` binding, a parameter, or a nested element of the list.",
    ),
    (
        "GP0043",
        "Parser",
        "`mod` declaration ended in `;`",
        "A statement ends at its newline, and a trailing semicolon is invalid everywhere else in the language. Write `mod name`.",
    ),
    (
        "GP0044",
        "Parser",
        "callable type written as something other than `Fn`",
        "The language has one callable type, `Fn(args) -> ret`. There is no raw function-pointer shape and no `FnMut` / `FnOnce` distinction to draw.",
    ),
    (
        "GP0045",
        "Parser",
        "argument label bound with `=`",
        "Every keyed form in the language binds with `:` - a struct field, a map entry, a `cohort` header setting. Write `name: value`.",
    ),
    (
        "GP0046",
        "Parser",
        "`unsafe` grants nothing",
        "No operation is withheld outside an `unsafe` block or from a safe `fn`. The keyword stays reserved for a future raw-FFI story.",
    ),
    (
        "GP0047",
        "Parser",
        "opaque alias written `type X = new T`",
        "`new` reads as allocation to a reader arriving from any other language. Write `newtype X = T`.",
    ),
    (
        "GP0048",
        "Parser",
        "`go` is retired",
        "Every goroutine is a `spawn` attached to the `cohort { }` it is written inside, which every `spawn` outside `main` must have (GT0086). `go` evaluated its operands at the spawn site, so `--fix` hoists a computed operand into a temporary.",
    ),
    (
        "GP0049",
        "Parser",
        "compiler-known call written with a `!`",
        "The set of compiler-known names is closed and recognised at the `(`, so there is nothing a sigil disambiguates. Write `println(..)`, `format(..)`, `matches(..)`.",
    ),
    (
        "GP0050",
        "Parser",
        "enum representation is not an unsigned width",
        "A discriminant is a count, so its store is unsigned. Write `enum Name : u8 { .. }`, with a width between 1 and 64 bits.",
    ),
    (
        "GP0051",
        "Parser",
        "`regex!` / `sql!` moved into their modules",
        "Write `regex::compile(\"…\")` and `sql::statement(\"…\")`. A literal argument is still validated while the program is compiled.",
    ),
    (
        "GT0079",
        "Types",
        "pointer wrapper written",
        "Every value is heap-shared and reference-counted already, so `Box<T>` / `Rc<T>` / `Arc<T>` name no choice the writer has to make. Write the inner type.",
    ),
    (
        "GT0080",
        "Types",
        "`String::parse` called",
        "The parse surface is `to_i64`, `to_f64`, and `to_bool`: each parses the whole string and answers an `Option<T>`. Add `.ok_or(..)` where a `Result` is wanted.",
    ),
    (
        "GT0081",
        "Types",
        "enum representation too narrow for its variants",
        "A declared width is exact - it is what the discriminant is stored in - so it is never widened silently. Write a width that holds every variant, or drop it.",
    ),
    (
        "GT0082",
        "Types",
        "reference passed where a value is taken",
        "A parameter that is not `&mut` takes the value, and a reference names one rather than being one, so the compiled tiers would hand the callee the address. Write `*x`.",
    ),
    (
        "GT0083",
        "Types",
        "write through a value that is not a reference",
        "`*` reaches the place a reference names, and a value has no such place, so the write would reach nothing. Write the binding directly, or make the parameter `&mut T` and pass `&mut` at the call site.",
    ),
    (
        "GT0084",
        "Types",
        "impl of a trait the language supplies itself",
        "Hashing, copying, release, marker safety, and the `Into` / `TryInto` / `IntoIterator` directions are the language's, not a per-type choice, so the block would declare a contract nothing dispatches through. The diagnostic names what the language does and what to write in its place - a `From` impl on the target, a `defer`, an inherent method - or remove the block.",
    ),
    (
        "GT0085",
        "Types",
        "ordered container over a type that writes its own `cmp`",
        "A heap, a `BTreeSet`, or a `BTreeMap` keeps its elements in the order they went in and reads them back with no comparator to call, so an element or key whose type writes its own `cmp` would silently not be ordered by it. A sequence orders on demand and does route through the type's `cmp`: sort a `Vec<T>`, or key the container on a value that carries the order.",
    ),
    (
        "GT0086",
        "Types",
        "`spawn` outside a `cohort` block",
        "A `spawn` must be written lexically inside a `cohort { }` in its own function body; `main` is the one exemption, since the root cohort's extent is main's own. Elsewhere the child attaches to whatever cohort the caller happens to be inside, which neither the callee's signature nor the call site says, and to the root cohort - whose extent is the process - when the program has none. Open a block around the spawns and the code that collects from them.",
    ),
    (
        "GP0052",
        "Parser",
        "build-time validated call without a literal",
        "`sql::statement` was handed something other than a literal. The statement is checked while the program is compiled, so it has to be there to check; a statement built at run time is an ordinary `String` and needs no wrapper.",
    ),
    (
        "GP0056",
        "Parser",
        "retired cohort isolation spelling",
        "`context::Context` is the cancellation type a cohort may one day inherit; this setting decides whether a child gets an OS thread of its own, so it is `isolation: Isolation::Shared` or `isolation: Isolation::Thread`. `--fix` rewrites it.",
    ),
    (
        "GP0053",
        "Parser",
        "`Display` rendering declared as `to_string`",
        "Both rendering contracts declare one method that answers a `String`, so both are written `fn fmt`; the `impl` header is what decides which channel a value reaches, `{}` taking `Display` and `{:?}` taking `Debug`. `x.to_string()` still renders through `Display`.",
    ),
    (
        "GP0054",
        "Parser",
        "shared reference in parameter position",
        "An argument is passed without copying whatever its type, and a callee cannot write to the caller's variable unless the parameter says `&mut`, so `f(m: &Map)` and `f(m: Map)` have the same cost and the same guarantee. A sequence view is written `[T]`, and `&mut [T]` is the form that writes through. `--fix` drops the `&`.",
    ),
    (
        "GP0055",
        "Parser",
        "shared reference on a call argument",
        "No parameter is a shared reference, so the sigil changes nothing: an argument is passed without copying either way, and a callee writes to the caller's variable only through a `&mut` parameter, which is spelled `&mut` at the call too. `--fix` drops the `&`.",
    ),
    (
        "GR0001",
        "Resolve",
        "unresolved name",
        "A name used in source could not be resolved to a declaration. Check the spelling, whether a `use` brings the name into scope, and whether the item is visible at this location.",
    ),
    (
        "GR0002",
        "Resolve",
        "wrong namespace",
        "A name was resolved to the wrong namespace (value vs. type). Check the declaration and the spelling.",
    ),
    (
        "GR0003",
        "Resolve",
        "duplicate item",
        "Two items in the same module share a name. Rename one of them or move it into a distinct `mod`.",
    ),
    (
        "GR0004",
        "Resolve",
        "duplicate import",
        "The same path was imported twice in the same `use` list. Drop the duplicate.",
    ),
    (
        "GR0005",
        "Resolve",
        "unknown module path",
        "The `use` names a `std::` module path that does not exist. Every module has exactly one canonical path (e.g. JSON lives at `std::encoding::json`); check `gos doc` or the stdlib reference for the module's path.",
    ),
    (
        "GR0006",
        "Resolve",
        "removed stdlib item",
        "A container spelling that a canonical name replaced. Each container has exactly one name - import and write that one.",
    ),
    (
        "GR0007",
        "Resolve",
        "unknown stdlib item",
        "The `use` names a module that exists but an item that module does not export. Check the item spelling; `gos doc std::<module>` lists every name a module exports.",
    ),
    (
        "GR0017",
        "Resolver",
        "break or continue with no loop",
        "A `break` or `continue` has no loop to act on: either none encloses it, or the label it names is not carried by any enclosing loop. A closure body is a separate function, so a loop outside it is not a target.",
    ),
    (
        "GR0018",
        "Resolver",
        "compiler-known call named as a value path",
        "A path named one of the compiler-known calls - `println`, `format`, `panic`, and the rest of the fixed set. Each expands where it is written and the runtime binds no callable for it, so the path has nothing to call or pass as a value. Write it as `name(..)`; it needs no import.",
    ),
    (
        "GR0019",
        "Resolver",
        "two dependency packages under one module name",
        "Two dependency packages are reached under one module name. A `-` is not part of an identifier, so a package name carrying one is reached from source as the same name with `_` in its place, which two packages can share. Give one of them a name of its own in `[dependencies]`, or import each through `use \"id\" as name`.",
    ),
    (
        "GR0020",
        "Resolver",
        "declaration under a compiler-known call name",
        "A `fn` or a binding was declared under one of the compiler-known call names - `println`, `format`, `matches`, and the rest of the fixed set. Each expands where it is written, so the declaration could never be reached. Give it a name of its own.",
    ),
    (
        "GR0021",
        "Resolve",
        "std free call written data-last",
        "A std free function was called with its data argument in the slot it occupied before every module took its data first. The call still means what it did, so nothing else in the body is affected; write the data argument first, which is what makes `iter::map(xs, f)` read as the `xs.map(f)` it stands for.",
    ),
    (
        "GT0001",
        "Types",
        "type mismatch",
        "The type checker could not reconcile two types it expected to match. The primary label shows the location of the mismatch; the `note:` line names the conflicting types.",
    ),
    (
        "GT0002",
        "Types",
        "unresolved method",
        "The type checker could not find a method with the supplied name on the receiver type. Check for a typo, a missing `use`, or a trait impl that lives in an unreachable module.",
    ),
    (
        "GT0003",
        "Types",
        "unresolved operator",
        "The operator is not defined for the operand types. Check the operand types and use the correct operator.",
    ),
    (
        "GT0004",
        "Match exhaustiveness",
        "non-exhaustive match",
        "A `match` expression does not cover every possible value. Add an arm for the pattern(s) listed under `help:`.",
    ),
    (
        "GT0005",
        "Types",
        "non-primitive cast",
        "The `as` cast is restricted to a whitelist: numeric ↔ numeric, `bool`/`char` → integer, `u8` → `char`, and same-type no-ops. Struct / enum / String sources are rejected. Use a conversion method when you need serialisation; `as` does not run code.",
    ),
    (
        "GT0044",
        "Types",
        "generic return type not inferred",
        "A generic return payload cannot be inferred from call arguments alone. Add an explicit generic argument or assign the expression to an expected `Result` type.",
    ),
    (
        "GT0045",
        "Types",
        "question mark not supported here",
        "The `?` operator can only unwrap `Result` inside a `Result`-returning function or `Option` inside an `Option`-returning function.",
    ),
];

fn render_diagnostics_page(entries: &[(&str, &str, &str, &str)]) -> String {
    let mut out = String::new();
    writeln!(out, "<!-- generated by `cargo xtask docs-diagnostics` -->").unwrap();
    writeln!(out, "# Diagnostic codes").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Every compiler diagnostic carries a four-character prefix plus\n\
         a four-digit number: `GP` for the parser / lexer, `GR` for\n\
         name resolution, `GT` for the type checker, `GM` for match\n\
         exhaustiveness, `GL` for lint framework, `GK` for the package\n\
         manager. Use `gos explain <code>` for the interactive\n\
         version. This page is auto-generated from the catalogue in\n\
         `xtask/src/main.rs`; hand edits are overwritten by\n\
         `cargo xtask docs-diagnostics`."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Code | Phase | Title |").unwrap();
    writeln!(out, "|------|-------|-------|").unwrap();
    for (code, phase, title, _) in entries {
        let anchor = code.to_ascii_lowercase();
        writeln!(out, "| [`{code}`](#{anchor}) | {phase} | {title} |").unwrap();
    }
    writeln!(out).unwrap();
    for (code, phase, title, explanation) in entries {
        let anchor = code.to_ascii_lowercase();
        writeln!(out, "## `{code}` <a id=\"{anchor}\"></a>").unwrap();
        writeln!(out).unwrap();
        writeln!(out, "**{phase}** - {title}").unwrap();
        writeln!(out).unwrap();
        writeln!(out, "{explanation}").unwrap();
        writeln!(out).unwrap();
    }
    out
}

/// Legacy module-level evidence across the toolchain's execution
/// paths. This does not imply that every item in the module is wired.
#[derive(Clone, Copy)]
struct StdlibSupport {
    path: &'static str,
    interp: Coverage,
    compiled: Coverage,
    tested: Coverage,
    notes: &'static str,
}

#[derive(Clone, Copy)]
#[allow(
    dead_code,
    reason = "Missing is used by the table only when a module regresses"
)]
enum Coverage {
    Full,
    Partial,
    Missing,
}

impl Coverage {
    fn label(self) -> &'static str {
        match self {
            Self::Full => "module-only",
            Self::Partial => "partial",
            Self::Missing => "none",
        }
    }
}

const STDLIB_SUPPORT: &[StdlibSupport] = &[
    item(
        "std::fmt",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "println / print / eprintln / eprint / format / write / writeln.",
    ),
    item(
        "std::io",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "stdout, stderr, stdin, write, write_byte, write_byte_array, flush, read_line, read_to_string.",
    ),
    item(
        "std::os",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "args, env, exit, read_file, write_file, mkdir, mkdir_all, read_dir.",
    ),
    item(
        "std::os::exec",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "Command builder + output / status / spawn / kill / wait. Wired through interp builtins, MIR lower, and C ABI.",
    ),
    item(
        "std::os::signal",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "on(signum) + Notifier::wait/try_wait. Wired through interp builtins, MIR lower, and C ABI.",
    ),
    item(
        "std::strings",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "split, trim, contains, find, replace, to_lower, to_upper, starts_with, ends_with.",
    ),
    item(
        "std::strconv",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "parse_i64, parse_u64, parse_f64, parse_bool, format_i64, format_f64.",
    ),
    item(
        "std::collections",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "Vec, Map, Set, Deque, Queue, Stack, heaps, and ordered maps/sets.",
    ),
    item(
        "std::net",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "TcpListener, TcpStream. UdpSocket partial.",
    ),
    item(
        "std::http",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "HTTP/1.1 + HTTP/2 server + client (push + trailers); HTTP/3 via std::http_h3.",
    ),
    item(
        "std::encoding::json",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "encode + decode + Value.",
    ),
    item(
        "std::encoding::base64",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "encode + decode.",
    ),
    item(
        "std::encoding::hex",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "encode + decode.",
    ),
    item(
        "std::encoding::binary",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "put_u16/u32/u64/i16/i32/i64 and get_u16/u32/u64/i16/i32/i64, both be and le variants.",
    ),
    item(
        "std::sync",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "Mutex, WaitGroup, AtomicI64. RwLock, Once partial.",
    ),
    item(
        "std::time",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "now, sleep, format_rfc3339, parse_rfc3339.",
    ),
    item(
        "std::panic",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "panic + catch_unwind.",
    ),
    item(
        "std::errors",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "new, newf, wrap, is, join.",
    ),
    item(
        "std::flag",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "Set with string/int/uint/float/bool/duration/string_list, --help, equals form. Subcommands deferred to v1.x.",
    ),
    item(
        "std::path",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "join, split, base, dir, ext, clean.",
    ),
    item(
        "std::fs",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "read_dir, walk_dir, mkdir_all, remove_all, copy, rename.",
    ),
    item(
        "std::bytes",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "Buffer, Builder, index_of, split, replace.",
    ),
    item(
        "std::bufio",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "Reader, Writer, Scanner with split_lines / split_words.",
    ),
    item(
        "std::net::url",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "Url, query_escape, query_unescape.",
    ),
    item(
        "std::slog",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "Logger, Field, TextHandler, JsonHandler with escape coverage.",
    ),
    item(
        "std::context",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "background, with_cancel, with_deadline, with_timeout.",
    ),
    item(
        "std::crypto::rand",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "fill, bytes.",
    ),
    item(
        "std::crypto::sha256",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "digest, hex.",
    ),
    item(
        "std::crypto::hmac",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "sha256_mac.",
    ),
    item(
        "std::crypto::subtle",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "constant_time_eq.",
    ),
    item(
        "std::sort",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "sort, sort_stable, binary_search.",
    ),
    item(
        "std::utf8",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "is_valid, rune_count.",
    ),
    item(
        "std::math::rand",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "Rng (SplitMix64).",
    ),
    item(
        "std::testing",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "Runner, check, check_eq, check_ok.",
    ),
    item(
        "std::runtime",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "max_procs, set_max_procs, num_cpus, caller, stack, set_finalizer.",
    ),
    item(
        "std::tls",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "rustls-backed; ServerConfig, ClientConfig.",
    ),
    item(
        "std::regex",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "compile, is_match, count, find, find_all, captures, replace, split.",
    ),
    item(
        "std::compress::gzip",
        Coverage::Full,
        Coverage::Full,
        Coverage::Full,
        "encode/decode + Level. Wired through builtins, MIR lower, and C ABI.",
    ),
];

const fn item(
    path: &'static str,
    interp: Coverage,
    compiled: Coverage,
    tested: Coverage,
    notes: &'static str,
) -> StdlibSupport {
    StdlibSupport {
        path,
        interp,
        compiled,
        tested,
        notes,
    }
}

/// Rewrites `docs_src/stdlib_coverage.md` from the complete manifest,
/// augmented with the legacy module-level evidence table.
fn regenerate_stdlib_coverage() -> Result<()> {
    let workspace_root = locate_workspace_root()?;
    let out_path = workspace_root.join("docs_src/stdlib_coverage.md");
    let page = render_stdlib_coverage_page(STDLIB_SUPPORT);
    fs::write(&out_path, page).with_context(|| format!("writing {}", out_path.display()))?;
    println!("xtask: wrote {}", out_path.display());
    Ok(())
}

fn render_stdlib_coverage_page(items: &[StdlibSupport]) -> String {
    let mut out = String::new();
    writeln!(out, "<!-- generated by `cargo xtask stdlib-coverage` -->").unwrap();
    writeln!(out, "# Stdlib coverage matrix").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Auto-generated. Do not hand-edit. Re-run `cargo xtask\n\
         stdlib-coverage` after changing the manifest or support evidence."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "The module table includes every manifest module. `module-only` means\n\
         legacy evidence exists for at least one item; it is not proof that\n\
         every declared item works. `partial` records a known partial surface,\n\
         and `none` means no evidence record exists. The item inventory is the\n\
         compatibility-audit queue and intentionally makes missing item-level\n\
         evidence visible.\n"
    )
    .unwrap();
    writeln!(
        out,
        "| Module | Lifecycle | Items | Interp evidence | Compiled evidence | Test evidence | Notes |"
    )
    .unwrap();
    writeln!(
        out,
        "|--------|-----------|------:|-----------------|-------------------|---------------|-------|"
    )
    .unwrap();
    for module in gossamer_std::manifest::ALL_MODULES {
        let status = gossamer_std::manifest::feature_status::lookup(module.path)
            .map_or("experimental", |entry| entry.status.tag());
        if let Some(entry) = items.iter().find(|entry| entry.path == module.path) {
            writeln!(
                out,
                "| `{}` | {} | {} | {} | {} | {} | {} |",
                module.path,
                status,
                module.items.len(),
                entry.interp.label(),
                entry.compiled.label(),
                entry.tested.label(),
                entry.notes,
            )
            .unwrap();
        } else {
            writeln!(
                out,
                "| `{}` | {} | {} | none | none | none | No module-level evidence record. |",
                module.path,
                status,
                module.items.len(),
            )
            .unwrap();
        }
    }
    render_declared_item_inventory(&mut out);
    writeln!(out).unwrap();
    writeln!(out, "## How to regenerate this page").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "```sh\ncargo xtask stdlib-coverage\n```").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "## How to interpret the columns").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "- `module-only` is legacy evidence that at least one item has the\n\
         corresponding implementation or test path. It must not be used as a\n\
         complete-module claim.\n\n\
         - `partial` records an explicitly incomplete implementation.\n\n\
         - `none` records missing module-level evidence, not proof that the\n\
         implementation is absent.\n\n\
         - Stable promotion requires item-level executable evidence keyed by\n\
         canonical item path; this page does not infer evidence from source-text\n\
         matches."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "## Cross-references").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "- [`stdlib.md`](stdlib.md) - module index with summaries.\n\
         - [`method_support.md`](method_support.md) - per-method\n\
           reference for shipped types."
    )
    .unwrap();
    out
}

fn render_declared_item_inventory(out: &mut String) {
    writeln!(out).unwrap();
    writeln!(out, "## Declared item inventory").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Every public manifest item appears below. An item whose row names a\n\
         program is exercised by it on every tier and every host in the CI\n\
         matrix. `not item-audited` is a deliberate non-claim: no executable\n\
         evidence is linked to that canonical item path yet."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Item | Kind | Lifecycle | Evidence |").unwrap();
    writeln!(out, "|------|------|-----------|----------|").unwrap();
    for record in gossamer_std::registry::item_records() {
        let evidence =
            gossamer_std::manifest::feature_status::item_evidence(&record.path, record.status);
        let cited = if evidence.positive_tests.is_empty() {
            "not item-audited".to_string()
        } else {
            evidence
                .positive_tests
                .iter()
                .map(|test| format!("`{test}`"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        writeln!(
            out,
            "| `{}` | {:?} | {} | {} |",
            record.path,
            record.kind,
            record.status.tag(),
            cited,
        )
        .unwrap();
    }
}

/// Start marker for the generated `ITEM_FIXTURES` table.
const ITEM_FIXTURES_BEGIN: &str = "// BEGIN generated by `cargo xtask item-fixtures`";
/// End marker for the generated `ITEM_FIXTURES` table.
const ITEM_FIXTURES_END: &str = "// END generated by `cargo xtask item-fixtures`";

/// Regenerates the `ITEM_FIXTURES` ledger from the fixture corpus.
///
/// The ledger says which program exercises a module and is registered in
/// the tier-parity suite, so the module carries that fixture's evidence
/// across every tier and host. Both halves are derivable: the SPECS list
/// names the registered fixtures, and a fixture's own `use std::...`
/// lines name the modules it exercises. Hand-maintaining it produced two
/// entries for 114 modules.
/// Items whose evidence is the fixture written for them, by name.
///
/// The scan behind the rest of the table reads a fixture's source for the
/// constructs it uses; these are the ones whose spelling is not distinctive
/// enough to match on, so the pairing is written down instead.
const PINNED_ITEM_FIXTURES: &[(&str, &str)] = &[
    (
        "lang::callback_shorthand",
        "feature-testing-examples/callback_shorthands.gos",
    ),
    (
        "lang::doctest",
        "feature-testing-examples/doctest_fences.gos",
    ),
    (
        "lang::keyword_arguments",
        "feature-testing-examples/keyword_and_default_arguments.gos",
    ),
    (
        "lang::unicode_identifiers",
        "feature-testing-examples/unicode_full.gos",
    ),
];

fn regenerate_item_fixtures(check: bool) -> Result<()> {
    let root = locate_workspace_root()?;
    let harness = fs::read_to_string(root.join("crates/gossamer-cli/tests/tier_parity/harness.rs"))
        .context("reading the tier-parity harness")?;

    // `spec("path")` names a registered fixture. A row carrying
    // `skip_all` opts out of the walk entirely and proves nothing.
    let mut registered: Vec<String> = Vec::new();
    for (index, _) in harness.match_indices("spec(\"") {
        let rest = &harness[index + "spec(\"".len()..];
        let Some(end) = rest.find('"') else { continue };
        let path = &rest[..end];
        let preceding = &harness[index.saturating_sub(400)..index];
        if preceding.contains("skip_all: Some(") {
            continue;
        }
        registered.push(path.to_string());
    }
    registered.sort();
    registered.dedup();

    let mut by_module: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // A construct no line scan can name uniquely earns its evidence from the
    // one fixture written to exercise it, named here rather than matched by a
    // pattern that would also match something else. Without these the
    // regenerated table drops them, and the item reads as unproven even though
    // a fixture proves it on every tier.
    for (item, fixture) in PINNED_ITEM_FIXTURES {
        by_module
            .entry((*item).to_string())
            .or_default()
            .push((*fixture).to_string());
    }
    for fixture in &registered {
        let Ok(source) = fs::read_to_string(root.join(fixture)) else {
            continue;
        };
        for module in stdlib_modules_used(&source) {
            by_module.entry(module).or_default().push(fixture.clone());
        }
        for feature in gossamer_std::manifest::feature_status::lang_features_used(&source) {
            by_module.entry(feature).or_default().push(fixture.clone());
        }
    }
    for fixtures in by_module.values_mut() {
        fixtures.sort();
        fixtures.dedup();
    }

    let mut table = String::new();
    table.push_str(ITEM_FIXTURES_BEGIN);
    table.push('\n');
    for (module, fixtures) in &by_module {
        table.push_str(&format!("    (\n        \"{module}\",\n        &[\n"));
        for fixture in fixtures {
            table.push_str(&format!("            \"{fixture}\",\n"));
        }
        table.push_str("        ],\n    ),\n");
    }
    table.push_str("    ");
    table.push_str(ITEM_FIXTURES_END);

    let target = root.join("crates/gossamer-std/src/manifest/feature_status.rs");
    let current = fs::read_to_string(&target).context("reading feature_status.rs")?;
    let begin = current
        .find(ITEM_FIXTURES_BEGIN)
        .context("ITEM_FIXTURES begin marker not found")?;
    let end = current
        .find(ITEM_FIXTURES_END)
        .context("ITEM_FIXTURES end marker not found")?
        + ITEM_FIXTURES_END.len();
    let rebuilt = format!(
        "{}{}{}",
        &current[..begin],
        table.trim_start_matches("    "),
        &current[end..]
    );

    // Compared after formatting, not before: `cargo fmt` rewraps the
    // generated table, so an unformatted comparison reports drift on a
    // table that is already current.
    let formatted = rustfmt(&rebuilt)?;
    if check {
        if formatted == current {
            println!(
                "xtask: ITEM_FIXTURES is in sync ({} modules)",
                by_module.len()
            );
            return Ok(());
        }
        anyhow::bail!("ITEM_FIXTURES is stale; run `cargo xtask item-fixtures`");
    }
    fs::write(&target, formatted).context("writing feature_status.rs")?;
    println!(
        "xtask: wrote ITEM_FIXTURES for {} modules from {} registered fixtures",
        by_module.len(),
        registered.len()
    );
    Ok(())
}

/// Formats Rust source through `rustfmt`, so generated output matches
/// what `cargo fmt` would leave behind.
fn rustfmt(source: &str) -> Result<String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning rustfmt")?;
    child
        .stdin
        .take()
        .context("rustfmt stdin")?
        .write_all(source.as_bytes())
        .context("writing to rustfmt")?;
    let out = child.wait_with_output().context("waiting for rustfmt")?;
    anyhow::ensure!(
        out.status.success(),
        "rustfmt failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).context("rustfmt output is not UTF-8")
}

/// Canonical stdlib module paths a Gossamer source imports.
fn stdlib_modules_used(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in source.lines() {
        let Some(rest) = line.trim().strip_prefix("use std::") else {
            continue;
        };
        let rest = rest.split("//").next().unwrap_or(rest).trim();
        let (prefix, leaves) = match rest.split_once('{') {
            Some((prefix, tail)) => (
                prefix.trim(),
                tail.trim_end_matches('}').split(',').collect::<Vec<_>>(),
            ),
            None => ("", vec![rest]),
        };
        for leaf in leaves {
            let leaf = leaf.split(" as ").next().unwrap_or(leaf).trim();
            if leaf.is_empty() {
                continue;
            }
            let path = format!("std::{prefix}{leaf}");
            // A leaf may name an item rather than a module
            // (`use std::sync::channel`); fall back to the module above.
            let parent = path
                .rsplit_once("::")
                .map_or_else(|| path.clone(), |(module, _)| module.to_string());
            for candidate in [path, parent] {
                if gossamer_std::registry::module(&candidate).is_some() {
                    if !found.contains(&candidate) {
                        found.push(candidate);
                    }
                    break;
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod diagnostic_catalogue_tests {
    use super::DIAGNOSTIC_CATALOGUE;

    /// The published page is a curated subset of the codes `gos explain`
    /// answers for, so it does not have to carry every code. It does have
    /// to carry only real ones: a row for a code nothing emits documents
    /// a diagnostic a reader can never see and can never look up.
    #[test]
    fn every_catalogue_row_names_a_code_the_compiler_can_emit() {
        let known: std::collections::BTreeSet<&str> =
            gossamer_diagnostics::code_registry::codes().collect();
        let stale: Vec<&str> = DIAGNOSTIC_CATALOGUE
            .iter()
            .map(|(code, ..)| *code)
            .filter(|code| !known.contains(code))
            .collect();
        assert!(
            stale.is_empty(),
            "DIAGNOSTIC_CATALOGUE rows with no registry entry: {stale:?}"
        );
    }

    /// Two rows for one code would render the anchor twice.
    #[test]
    fn catalogue_has_no_duplicate_codes() {
        let mut seen = std::collections::BTreeSet::new();
        for (code, ..) in DIAGNOSTIC_CATALOGUE {
            assert!(seen.insert(*code), "duplicate catalogue row for {code}");
        }
    }
}
