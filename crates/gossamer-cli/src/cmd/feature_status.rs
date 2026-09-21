//! `gos feature-status` - prints every language / stdlib feature
//! with its lifecycle stage (stable, shipped, experimental, planned, removed)
//! plus optional per-tier test status read from a JSON sidecar.
//!
//! The lifecycle data is the single source of truth in
//! `gossamer_std::manifest::feature_status::FEATURE_STATUS` merged
//! with the implicit `Experimental` defaults from
//! `manifest::ALL_MODULES`. The per-tier test status comes from
//! `target/debug/.feature-status.json`, written by
//! `gos test --tier-parity --report=status`, falling back to the
//! evidence recorded for this release, which is compiled into the
//! binary so an installed `gos` with no repository behind it still
//! reports what the walk proved.
//!
//! `--check` enforces the CI gate: every `Stable` item must have a
//! doc page on disk (`docs_src/stdlib/<slug>.md` or
//! `docs_src/language/<slug>.md`) plus an all-tiers-pass record in
//! the JSON sidecar. `Shipped` and `Experimental` items need the
//! doc page; `Planned` and `Removed` items aren't gated.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use gossamer_std::manifest::{FeatureStatus, Status, feature_status};

/// Output format selector matching the CLI flag.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// ASCII pipe-separated table - default for human reading.
    #[default]
    Table,
    /// One JSON object per feature, easy to pipe into `jq`.
    Json,
    /// Markdown table - drop into docs pages or PR descriptions.
    Markdown,
}

impl OutputFormat {
    /// Parses the `--format` argument value. Returns `None` for an
    /// unrecognised tag so the dispatcher can surface a clear error.
    #[must_use]
    pub fn parse(tag: &str) -> Option<OutputFormat> {
        match tag {
            "table" => Some(OutputFormat::Table),
            "json" => Some(OutputFormat::Json),
            "markdown" | "md" => Some(OutputFormat::Markdown),
            _ => None,
        }
    }
}

/// Per-tier outcome for one feature, derived from the JSON sidecar.
/// `None` for any field means the sidecar didn't carry a value for
/// that tier; an explicit value is either `"pass"` or `"fail"`.
#[derive(Debug, Clone, Default)]
pub struct TierStatus {
    /// Bytecode VM (`gos`) tier outcome.
    pub vm: Option<String>,
    /// Cranelift JIT tier outcome.
    pub cranelift: Option<String>,
    /// LLVM release tier outcome.
    pub llvm: Option<String>,
}

impl TierStatus {
    /// Returns `true` when every present tier is `"pass"` and at
    /// least one tier was reported. Missing tiers and `"fail"`
    /// outcomes both return `false`.
    #[must_use]
    pub fn all_pass(&self) -> bool {
        let tiers = [&self.vm, &self.cranelift, &self.llvm];
        let mut any = false;
        for tier in tiers {
            match tier {
                Some(v) if v == "pass" => any = true,
                Some(_) => return false,
                None => return false,
            }
        }
        any
    }

    /// Renders the per-tier status as a compact `vm:pass cl:pass llvm:pass`
    /// string for the table view. Missing tiers print as `-`.
    #[must_use]
    pub fn render_compact(&self) -> String {
        let one = |tag: &str, value: &Option<String>| -> String {
            format!("{tag}:{}", value.as_deref().unwrap_or("-"))
        };
        format!(
            "{} {} {}",
            one("vm", &self.vm),
            one("cl", &self.cranelift),
            one("llvm", &self.llvm),
        )
    }
}

/// Options threaded into [`run`]. Plain struct so future flags can
/// be added without touching every call site.
#[derive(Debug, Clone, Default)]
pub struct FeatureStatusOpts {
    /// Output format selector.
    pub format: OutputFormat,
    /// CI gate mode - non-zero exit on policy violation.
    pub check: bool,
    /// Optional glob narrowing the displayed entries (`std::http::*`).
    pub filter: Option<String>,
    /// Optional status filter (`stable` / `shipped` / `experimental` / `planned` / `removed`).
    pub status: Option<Status>,
    /// Override for the JSON sidecar path (defaults to
    /// `target/debug/.feature-status.json`).
    pub sidecar: Option<PathBuf>,
    /// Override for the docs root used by `--check` (defaults to
    /// `docs_src/`).
    pub docs_root: Option<PathBuf>,
    /// Report one row per exported item rather than per module.
    pub items: bool,
}

/// Entry point for the `gos feature-status` subcommand.
pub fn run(opts: FeatureStatusOpts) -> Result<()> {
    let entries = if opts.items {
        collect_item_entries(&opts)
    } else {
        collect_entries(&opts)
    };
    let tiers = load_tier_status(opts.sidecar.as_deref())?;
    let docs_root = opts.docs_root.clone().unwrap_or_else(default_docs_root);

    if opts.check {
        return check_mode(&entries, &tiers, &docs_root);
    }

    let rows: Vec<Row> = entries
        .iter()
        .map(|e| Row {
            entry: *e,
            // An item inherits its module's record: the walk observes a
            // module, which is the granularity a fixture's imports name.
            tiers: tier_record(&tiers, e.path).cloned().unwrap_or_default(),
            doc: doc_page_for(e.path, &docs_root),
        })
        .collect();

    match opts.format {
        OutputFormat::Table => print_table(&rows, !tiers.is_empty()),
        OutputFormat::Json => print_json(&rows, !tiers.is_empty()),
        OutputFormat::Markdown => print_markdown(&rows, !tiers.is_empty()),
    }
    Ok(())
}

/// Resolves the registry + applied filters into the final ordered
/// list of features to display.
fn collect_entries(opts: &FeatureStatusOpts) -> Vec<FeatureStatus> {
    let mut out: Vec<FeatureStatus> = feature_status::all_entries()
        .into_iter()
        // Filtered on the status the table reports, not the one that was
        // authored: selecting `experimental` must not return rows the
        // same command renders as `unproven`.
        .filter(|e| match opts.status {
            Some(s) => derived_status_of(e) == s,
            None => true,
        })
        .filter(|e| match opts.filter.as_deref() {
            Some(pattern) => glob_match(pattern, e.path),
            None => true,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(b.path));
    out
}

/// One row per exported stdlib item, rather than one per module.
///
/// A module is the wrong unit for the question an item-level reader is
/// asking. `std::strings` is one row whether `trim` has been there since
/// the beginning or `split_once` landed last week; the evidence behind
/// the two is identical only because it is recorded per module. Each
/// item inherits its module's evidence and carries its own doc line, so
/// the answer is at least addressed to the thing being asked about.
fn collect_item_entries(opts: &FeatureStatusOpts) -> Vec<FeatureStatus> {
    let mut out: Vec<FeatureStatus> = gossamer_std::registry::item_records()
        .into_iter()
        .map(|record| FeatureStatus {
            // `item_records` owns its paths; the table holds `&'static`
            // ones, and the registry is itself static data.
            path: Box::leak(record.path.into_boxed_str()),
            status: record.status,
            doc: record.doc,
        })
        .filter(|e| match opts.status {
            Some(s) => derived_status_of(e) == s,
            None => true,
        })
        .filter(|e| match opts.filter.as_deref() {
            Some(pattern) => glob_match(pattern, e.path),
            None => true,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(b.path));
    out
}

/// Status reported for `entry`, reduced to what evidence supports.
fn derived_status_of(entry: &FeatureStatus) -> Status {
    feature_status::derived_status(entry.path, entry.status)
}

/// Reads the JSON sidecar produced by
/// `gos test --tier-parity --report=status`. Missing file silently
/// returns an empty map so the caller can render `(no test data)`.
/// Malformed JSON is a hard error.
pub fn load_tier_status(path: Option<&Path>) -> Result<BTreeMap<String, TierStatus>> {
    let path = path.map_or_else(default_sidecar_path, Path::to_path_buf);
    if !path.exists() {
        return release_evidence();
    }
    let bytes = fs::read(&path)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    parse_sidecar(&text).map_err(|e| anyhow!("parsing {}: {e}", path.display()))
}

/// The tier-parity evidence compiled into this binary - what an installed
/// `gos` with no repository behind it reports.
pub fn release_evidence() -> Result<BTreeMap<String, TierStatus>> {
    parse_sidecar(RELEASE_EVIDENCE)
        .map_err(|e| anyhow!("parsing the recorded release evidence: {e}"))
}

/// Tier-parity evidence recorded for this release by
/// `gos test --tier-parity --report status`, used when no locally
/// generated sidecar is present. Regenerated by the CI walk; a stale
/// copy fails that job.
const RELEASE_EVIDENCE: &str = include_str!("../../../../resources/feature-status.json");

/// Renders the sidecar JSON shape produced by the test harness.
/// Exposed so the test harness can call it directly to keep one
/// canonical format.
#[must_use]
pub fn render_sidecar(records: &[(String, TierStatus)]) -> String {
    let mut out = String::with_capacity(records.len() * 96);
    out.push_str("[\n");
    for (i, (name, status)) in records.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str("  {\"name\":");
        out.push_str(&json_string(name));
        out.push_str(",\"tiers\":{");
        let mut wrote = false;
        for (label, value) in [
            ("vm", &status.vm),
            ("cranelift", &status.cranelift),
            ("llvm", &status.llvm),
        ] {
            if let Some(value) = value {
                if wrote {
                    out.push(',');
                }
                out.push_str(&format!("\"{label}\":{}", json_string(value)));
                wrote = true;
            }
        }
        out.push_str("}}");
    }
    out.push_str("\n]\n");
    out
}

/// Default sidecar path - `target/debug/.feature-status.json` from
/// the workspace root, falling back to the current directory when
/// `CARGO_MANIFEST_DIR` isn't set.
fn default_sidecar_path() -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || workspace_root_or_cwd().map_or_else(|| PathBuf::from("target"), |r| r.join("target")),
        PathBuf::from,
    );
    base.join("debug").join(".feature-status.json")
}

/// Default docs root - `docs_src/` next to the workspace root,
/// falling back to a `docs_src` directory beside the cwd.
fn default_docs_root() -> PathBuf {
    workspace_root_or_cwd().map_or_else(|| PathBuf::from("docs_src"), |r| r.join("docs_src"))
}

fn workspace_root_or_cwd() -> Option<PathBuf> {
    // Walk up from the cwd looking for a Cargo workspace marker.
    let mut cur = std::env::current_dir().ok()?;
    loop {
        if cur.join("Cargo.toml").exists() && cur.join("crates").is_dir() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// Returns the doc page path for `feature_path`, or `None` when no
/// file is on disk under `docs_root`. Stdlib items map to
/// `docs_root/stdlib/<slug>.md`, language items to
/// `docs_root/language/<slug>.md`.
fn doc_page_for(feature_path: &str, docs_root: &Path) -> Option<PathBuf> {
    let (subdir, slug) = if let Some(rest) = feature_path.strip_prefix("std::") {
        ("stdlib", rest.replace("::", "_"))
    } else if let Some(rest) = feature_path.strip_prefix("lang::") {
        ("language", rest.replace("::", "_"))
    } else {
        ("misc", feature_path.replace("::", "_"))
    };
    let candidate = docs_root.join(subdir).join(format!("{slug}.md"));
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// One assembled row ready for rendering.
struct Row {
    entry: FeatureStatus,
    tiers: TierStatus,
    doc: Option<PathBuf>,
}

impl Row {
    /// Status to report: the authored label reduced by what evidence
    /// supports. Reading `entry.status` directly would print the claim
    /// rather than what is known.
    fn status(&self) -> Status {
        feature_status::derived_status(self.entry.path, self.entry.status)
    }
}

fn print_table(rows: &[Row], has_tiers: bool) {
    let header_tier = "Tier-Parity";
    let name_w = rows
        .iter()
        .map(|r| r.entry.path.len())
        .max()
        .unwrap_or(0)
        .max("Name".len());
    let status_w = "experimental".len();
    let tier_w = "vm:pass cl:pass llvm:pass".len().max(header_tier.len());
    let doc_w = "(no doc)".len();
    outln!(
        "{:name_w$} | {:status_w$} | {:tier_w$} | Doc",
        "Name",
        "Status",
        header_tier
    );
    outln!(
        "{} | {} | {} | {}",
        "-".repeat(name_w),
        "-".repeat(status_w),
        "-".repeat(tier_w),
        "-".repeat(doc_w),
    );
    for row in rows {
        let tier_cell = if has_tiers {
            row.tiers.render_compact()
        } else {
            "(no test data)".to_string()
        };
        let doc_cell = match &row.doc {
            Some(p) => p
                .file_name()
                .map_or("(no doc)".to_string(), |s| s.to_string_lossy().into_owned()),
            None => "(no doc)".to_string(),
        };
        outln!(
            "{:name_w$} | {:status_w$} | {:tier_w$} | {}",
            row.entry.path,
            row.status().tag(),
            tier_cell,
            doc_cell,
        );
    }
}

fn print_json(rows: &[Row], has_tiers: bool) {
    let mut out = String::from("[\n");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        out.push_str("  {\"name\":");
        out.push_str(&json_string(row.entry.path));
        out.push_str(",\"status\":");
        out.push_str(&json_string(row.status().tag()));
        out.push_str(",\"doc\":");
        out.push_str(&row.doc.as_ref().map_or_else(
            || "null".to_string(),
            |p| json_string(&p.display().to_string()),
        ));
        if has_tiers {
            out.push_str(",\"tiers\":{");
            let mut wrote = false;
            for (label, value) in [
                ("vm", &row.tiers.vm),
                ("cranelift", &row.tiers.cranelift),
                ("llvm", &row.tiers.llvm),
            ] {
                if let Some(value) = value {
                    if wrote {
                        out.push(',');
                    }
                    out.push_str(&format!("\"{label}\":{}", json_string(value)));
                    wrote = true;
                }
            }
            out.push('}');
        }
        out.push_str(",\"doc_description\":");
        out.push_str(&json_string(row.entry.doc));
        let evidence = feature_status::item_evidence(row.entry.path, row.status());
        out.push_str(",\"evidence\":{");
        out.push_str("\"status\":");
        out.push_str(&json_string(evidence.status.tag()));
        out.push_str(",\"supported_tiers\":");
        json_string_array(
            &mut out,
            evidence.supported_tiers.iter().map(|tier| tier.tag()),
        );
        out.push_str(",\"supported_targets\":");
        json_string_array(&mut out, evidence.supported_targets.iter().copied());
        out.push_str(",\"doc_path\":");
        out.push_str(
            &evidence
                .doc_path
                .map_or_else(|| "null".to_string(), |path| json_string(&path)),
        );
        out.push_str(",\"positive_tests\":");
        json_string_array(&mut out, evidence.positive_tests.iter().map(String::as_str));
        out.push_str(",\"negative_tests\":");
        json_string_array(&mut out, evidence.negative_tests.iter().map(String::as_str));
        out.push_str(",\"known_limits\":");
        json_string_array(&mut out, evidence.known_limits.iter().map(String::as_str));
        out.push('}');
        out.push('}');
    }
    out.push_str("\n]\n");
    outln!("{out}");
}

fn print_markdown(rows: &[Row], has_tiers: bool) {
    outln!("| Name | Status | Tier-Parity | Doc |");
    outln!("|---|---|---|---|");
    for row in rows {
        let tier_cell = if has_tiers {
            row.tiers.render_compact()
        } else {
            "(no test data)".to_string()
        };
        let doc_cell = match &row.doc {
            Some(p) => format!(
                "`{}`",
                p.file_name()
                    .map_or("(no doc)".to_string(), |s| s.to_string_lossy().into_owned()),
            ),
            None => "(no doc)".to_string(),
        };
        outln!(
            "| `{}` | {} | {} | {} |",
            row.entry.path,
            row.status().tag(),
            tier_cell,
            doc_cell,
        );
    }
}

/// CI gate. Per-feature failures are collected then reported together
/// so one run surfaces the full punch list.
/// Tier record for `path`, falling back to the module it belongs to.
///
/// The walk records a module - that is the granularity a fixture's
/// imports name - while the lifecycle table also carries sub-feature
/// labels like `std::http::redirect_policy`. A label inherits its
/// module's evidence, exactly as its fixtures do.
fn tier_record<'a>(tiers: &'a BTreeMap<String, TierStatus>, path: &str) -> Option<&'a TierStatus> {
    if let Some(found) = tiers.get(path) {
        return Some(found);
    }
    let mut cursor = path;
    while let Some((parent, _)) = cursor.rsplit_once("::") {
        if let Some(found) = tiers.get(parent) {
            return Some(found);
        }
        cursor = parent;
    }
    None
}

fn check_mode(
    entries: &[FeatureStatus],
    tiers: &BTreeMap<String, TierStatus>,
    docs_root: &Path,
) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();
    for entry in entries {
        match entry.status {
            Status::Stable => {
                if doc_page_for(entry.path, docs_root).is_none() {
                    failures.push(format!(
                        "{}: stable item missing doc page under {}",
                        entry.path,
                        docs_root.display(),
                    ));
                }
                match tier_record(tiers, entry.path) {
                    Some(t) if t.all_pass() => {}
                    Some(_) => failures.push(format!(
                        "{}: stable item failed at least one tier in test sidecar",
                        entry.path,
                    )),
                    None => failures.push(format!(
                        "{}: stable item missing tier-parity test (no sidecar entry)",
                        entry.path,
                    )),
                }
            }
            // A declined feature still carries a doc page: the page is
            // where the decision and its alternative are written down.
            // `Unproven` is a documented surface too - it is only the
            // evidence that is missing, not the description.
            Status::Shipped | Status::Experimental | Status::Declined | Status::Unproven => {
                if doc_page_for(entry.path, docs_root).is_none() {
                    failures.push(format!(
                        "{}: {} item missing doc page under {}",
                        entry.path,
                        entry.status.tag(),
                        docs_root.display(),
                    ));
                }
            }
            Status::Planned | Status::Removed => {}
        }
        // An item may not claim a tier no fixture ran it on. This is the
        // gate that keeps the ledger from drifting back into restating
        // its own labels: evidence has to come from a program that ran.
        let evidence = feature_status::item_evidence(entry.path, entry.status);
        if !evidence.supported_tiers.is_empty() && evidence.positive_tests.is_empty() {
            failures.push(format!(
                "{}: claims tiers {:?} with no fixture behind them",
                entry.path, evidence.supported_tiers,
            ));
        }
        // A surface reported as settled must have a fixture that proves
        // it on every tier. `Unproven` is exempt - it is the label for
        // exactly this absence.
        let derived = feature_status::derived_status(entry.path, entry.status);
        if matches!(derived, Status::Stable | Status::Shipped) {
            match tier_record(tiers, entry.path) {
                Some(t) if t.all_pass() => {}
                Some(_) => failures.push(format!(
                    "{}: reported {} but failed at least one tier",
                    entry.path,
                    derived.tag(),
                )),
                None => failures.push(format!(
                    "{}: reported {} with no tier-parity record",
                    entry.path,
                    derived.tag(),
                )),
            }
        }
    }
    if failures.is_empty() {
        outln!("feature-status: ok ({} items checked)", entries.len());
        Ok(())
    } else {
        for line in &failures {
            eprintln!("feature-status: {line}");
        }
        Err(anyhow!(
            "feature-status check failed ({} item(s))",
            failures.len(),
        ))
    }
}

/// Minimal glob matcher: supports `*` for any number of characters
/// and `?` for one. Adequate for the `std::http::*` / `lang::*`
/// shapes the CLI accepts.
fn glob_match(pattern: &str, candidate: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let cnd: Vec<char> = candidate.chars().collect();
    // Iterative implementation with a star-back pointer; O(n*m)
    // worst case but constant memory.
    let (mut i, mut j) = (0, 0);
    let (mut star, mut match_) = (None, 0);
    while i < cnd.len() {
        if j < pat.len() && (pat[j] == '?' || pat[j] == cnd[i]) {
            i += 1;
            j += 1;
        } else if j < pat.len() && pat[j] == '*' {
            star = Some(j);
            match_ = i;
            j += 1;
        } else if let Some(s) = star {
            j = s + 1;
            match_ += 1;
            i = match_;
        } else {
            return false;
        }
    }
    while j < pat.len() && pat[j] == '*' {
        j += 1;
    }
    j == pat.len()
}

/// Parses the sidecar JSON. The expected shape is a top-level array
/// of `{"name": "...", "tiers": {"vm": "pass", ...}}` objects.
fn parse_sidecar(text: &str) -> Result<BTreeMap<String, TierStatus>, String> {
    let mut out = BTreeMap::new();
    // Hand-rolled JSON parser tailored to the closed shape. Avoids
    // pulling serde into the CLI crate just for this sidecar.
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();
    if bytes.first() != Some(&b'[') || bytes.last() != Some(&b']') {
        return Err("sidecar must be a top-level JSON array".into());
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    for record in split_top_objects(inner) {
        let (name, tiers) = parse_record(&record)?;
        out.insert(name, tiers);
    }
    Ok(out)
}

fn split_top_objects(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0_i32;
    let mut start = None::<usize>;
    let mut in_string = false;
    let mut escape = false;
    for (i, ch) in input.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start.take() {
                        out.push(input[s..=i].to_string());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_record(record: &str) -> Result<(String, TierStatus), String> {
    let name = json_extract_string(record, "name").ok_or("record missing `name`")?;
    let tiers_obj = json_extract_object(record, "tiers").unwrap_or_default();
    let vm = json_extract_string(&tiers_obj, "vm");
    let cranelift = json_extract_string(&tiers_obj, "cranelift");
    let llvm = json_extract_string(&tiers_obj, "llvm");
    Ok((
        name,
        TierStatus {
            vm,
            cranelift,
            llvm,
        },
    ))
}

fn json_extract_string(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let key_pos = find_json_key(input, &needle)?;
    let rest = &input[key_pos + needle.len()..];
    let colon = rest.find(':')?;
    let after_colon = rest[colon + 1..].trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut escape = false;
    for ch in after_colon[1..].chars() {
        if escape {
            out.push(ch);
            escape = false;
            continue;
        }
        if ch == '\\' {
            escape = true;
            continue;
        }
        if ch == '"' {
            return Some(out);
        }
        out.push(ch);
    }
    None
}

fn json_extract_object(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let key_pos = find_json_key(input, &needle)?;
    let rest = &input[key_pos + needle.len()..];
    let colon = rest.find(':')?;
    let after_colon = rest[colon + 1..].trim_start();
    if !after_colon.starts_with('{') {
        return None;
    }
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, ch) in after_colon.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(after_colon[..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Locates `needle` (already quoted, e.g. `"name"`) at a position
/// that is genuinely a JSON object key. The naive `str::find` walk
/// would match the literal text inside a string value (`"tiers"`
/// appearing as part of a value), so we filter to occurrences
/// outside any string context.
fn find_json_key(input: &str, needle: &str) -> Option<usize> {
    let needle_bytes = needle.as_bytes();
    let bytes = input.as_bytes();
    let mut in_string = false;
    let mut escape = false;
    let mut i = 0;
    while i < bytes.len() {
        let ch = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if in_string {
            match ch {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if ch == b'"' {
            if bytes[i..].starts_with(needle_bytes) {
                return Some(i);
            }
            in_string = true;
            i += 1;
            continue;
        }
        i += 1;
    }
    None
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Writes a JSON string array without introducing a serialization dependency
/// for the small, closed feature-status output format.
fn json_string_array<'a>(out: &mut String, values: impl IntoIterator<Item = &'a str>) {
    out.push('[');
    for (index, value) in values.into_iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&json_string(value));
    }
    out.push(']');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_format_round_trips() {
        for tag in ["table", "json", "markdown", "md"] {
            assert!(OutputFormat::parse(tag).is_some(), "missing {tag}");
        }
        assert!(OutputFormat::parse("yaml").is_none());
    }

    #[test]
    fn glob_match_handles_star() {
        assert!(glob_match("std::http::*", "std::http::router"));
        assert!(!glob_match("std::http::*", "std::net::tcp"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("std::*::router", "std::http::router"));
    }

    #[test]
    fn render_compact_shows_missing_tiers_as_dash() {
        let t = TierStatus {
            vm: Some("pass".into()),
            cranelift: None,
            llvm: Some("fail".into()),
        };
        assert_eq!(t.render_compact(), "vm:pass cl:- llvm:fail");
    }

    #[test]
    fn all_pass_requires_every_tier() {
        let pass = TierStatus {
            vm: Some("pass".into()),
            cranelift: Some("pass".into()),
            llvm: Some("pass".into()),
        };
        assert!(pass.all_pass());
        let partial = TierStatus {
            vm: Some("pass".into()),
            cranelift: None,
            llvm: Some("pass".into()),
        };
        assert!(!partial.all_pass());
        let failed = TierStatus {
            vm: Some("pass".into()),
            cranelift: Some("fail".into()),
            llvm: Some("pass".into()),
        };
        assert!(!failed.all_pass());
    }

    #[test]
    fn sidecar_round_trips() {
        let records = vec![
            (
                "examples/foo.gos".to_string(),
                TierStatus {
                    vm: Some("pass".into()),
                    cranelift: Some("pass".into()),
                    llvm: Some("pass".into()),
                },
            ),
            (
                "feature-testing-examples/bar.gos".to_string(),
                TierStatus {
                    vm: Some("pass".into()),
                    cranelift: Some("fail".into()),
                    llvm: None,
                },
            ),
        ];
        let text = render_sidecar(&records);
        let parsed = parse_sidecar(&text).expect("parse own output");
        assert_eq!(parsed.len(), 2);
        let foo = parsed.get("examples/foo.gos").unwrap();
        assert!(foo.all_pass());
        let bar = parsed.get("feature-testing-examples/bar.gos").unwrap();
        assert_eq!(bar.cranelift.as_deref(), Some("fail"));
    }

    /// The JSON row carries the same evidence the table does: the status
    /// the ledger derives, and tiers only a fixture can put there.
    #[test]
    fn json_output_includes_item_evidence_fields() {
        // A language construct earns its tiers from the fixtures that contain
        // it, the way a module earns them from the fixtures that import it.
        let derived = feature_status::derived_status("lang::if", Status::Shipped);
        let evidence = feature_status::item_evidence("lang::if", derived);
        let mut output = String::new();
        output.push_str(&json_string(evidence.status.tag()));
        json_string_array(
            &mut output,
            evidence.supported_tiers.iter().map(|tier| tier.tag()),
        );
        assert_eq!(output, "\"shipped\"[\"vm\",\"cranelift\",\"llvm\"]");
        assert_eq!(
            evidence.doc_path.as_deref(),
            Some("docs_src/language/if.md")
        );
        assert!(!evidence.positive_tests.is_empty());

        // A construct no line scan can name uniquely earns its tiers from the
        // one fixture written to exercise it, pinned by name rather than by a
        // pattern that would also match something else.
        let pinned = feature_status::item_evidence(
            "lang::callback_shorthand",
            feature_status::derived_status("lang::callback_shorthand", Status::Shipped),
        );
        let mut pinned_out = String::new();
        pinned_out.push_str(&json_string(pinned.status.tag()));
        json_string_array(
            &mut pinned_out,
            pinned.supported_tiers.iter().map(|tier| tier.tag()),
        );
        assert_eq!(pinned_out, "\"shipped\"[\"vm\",\"cranelift\",\"llvm\"]");
        assert!(!pinned.positive_tests.is_empty());

        // A surface no fixture exercises still claims nothing.
        let unproven = feature_status::item_evidence(
            "std::compress::zstd",
            feature_status::derived_status("std::compress::zstd", Status::Shipped),
        );
        let mut bare = String::new();
        bare.push_str(&json_string(unproven.status.tag()));
        json_string_array(
            &mut bare,
            unproven.supported_tiers.iter().map(|tier| tier.tag()),
        );
        assert_eq!(bare, "\"unproven\"[]");
        assert!(!unproven.known_limits.is_empty());

        let covered = feature_status::item_evidence("std::fs::read_to_string", Status::Shipped);
        let mut tiers = String::new();
        json_string_array(&mut tiers, covered.supported_tiers.iter().map(|t| t.tag()));
        assert_eq!(tiers, "[\"vm\",\"cranelift\",\"llvm\"]");
        assert!(!covered.positive_tests.is_empty());
    }

    #[test]
    fn check_mode_passes_when_shipped_have_tests_and_docs() {
        let tmp = tempdir();
        let docs = tmp.join("docs_src");
        fs::create_dir_all(docs.join("language")).unwrap();
        fs::create_dir_all(docs.join("stdlib")).unwrap();
        let entry = FeatureStatus {
            path: "lang::if",
            status: Status::Stable,
            doc: "Conditional expression.",
        };
        fs::write(docs.join("language/if.md"), "Status: shipped\n").unwrap();
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "lang::if".to_string(),
            TierStatus {
                vm: Some("pass".into()),
                cranelift: Some("pass".into()),
                llvm: Some("pass".into()),
            },
        );
        check_mode(&[entry], &tiers, &docs).expect("ok");
    }

    #[test]
    fn check_mode_fails_when_shipped_missing_doc() {
        let tmp = tempdir();
        let docs = tmp.join("docs_src");
        fs::create_dir_all(docs.join("language")).unwrap();
        let entry = FeatureStatus {
            path: "lang::zzz_undocumented",
            status: Status::Stable,
            doc: "",
        };
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "lang::zzz_undocumented".to_string(),
            TierStatus {
                vm: Some("pass".into()),
                cranelift: Some("pass".into()),
                llvm: Some("pass".into()),
            },
        );
        let err = check_mode(&[entry], &tiers, &docs).unwrap_err();
        assert!(err.to_string().contains("feature-status check failed"));
    }

    #[test]
    fn check_mode_fails_when_shipped_missing_test() {
        let tmp = tempdir();
        let docs = tmp.join("docs_src");
        fs::create_dir_all(docs.join("language")).unwrap();
        fs::write(docs.join("language/match.md"), "Status: shipped\n").unwrap();
        let entry = FeatureStatus {
            path: "lang::match",
            status: Status::Stable,
            doc: "",
        };
        let tiers = BTreeMap::new();
        let err = check_mode(&[entry], &tiers, &docs).unwrap_err();
        assert!(err.to_string().contains("feature-status check failed"));
    }

    #[test]
    fn check_mode_skips_planned_items() {
        let tmp = tempdir();
        let docs = tmp.join("docs_src");
        fs::create_dir_all(docs.join("language")).unwrap();
        let entry = FeatureStatus {
            path: "lang::async_await",
            status: Status::Planned,
            doc: "",
        };
        let tiers = BTreeMap::new();
        check_mode(&[entry], &tiers, &docs).expect("planned items skipped");
    }

    #[test]
    fn collect_entries_filters_by_status() {
        let opts = FeatureStatusOpts {
            status: Some(Status::Experimental),
            ..FeatureStatusOpts::default()
        };
        let entries = collect_entries(&opts);
        assert!(entries.iter().all(|e| e.status == Status::Experimental));
        assert!(!entries.is_empty(), "registry has experimental items");
    }

    #[test]
    fn collect_entries_filters_by_glob() {
        let opts = FeatureStatusOpts {
            filter: Some("std::http::*".into()),
            ..FeatureStatusOpts::default()
        };
        let entries = collect_entries(&opts);
        assert!(!entries.is_empty(), "http module surface");
        assert!(entries.iter().all(|e| e.path.starts_with("std::http::")));
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gos-feature-status-test-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }
}
