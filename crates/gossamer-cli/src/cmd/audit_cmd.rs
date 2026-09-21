//! `gos audit` - reports advisories that this project can actually
//! reach.
//!
//! Integrity was already covered: `project.lock` pins a sha256 and
//! `gos fetch` verifies an Ed25519 signature before unpacking. What was
//! missing is whether anything resolved is *known bad*.
//!
//! The report is filtered by reachability, which is the property that
//! decides whether a security tool gets used or switched off. An
//! advisory naming an item this project never references is not
//! actionable, and a list of those trains a reader to skip the output.
//! The frontend already knows every path a project mentions, so the
//! filter is a set intersection rather than a new analysis.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use gossamer_pkg::advisory::{Advisory, parse_feed};

use crate::paths::{collect_lint_targets, read_source};

/// Where a project's advisory feed is read from when the registry is
/// not reachable or not configured. Keeps `gos audit` useful offline
/// and gives a test somewhere to plant one.
const LOCAL_FEED: &str = "advisories.json";

/// Entry point for `gos audit`.
pub(crate) fn dispatch(path: Option<PathBuf>, all: bool, format: &str) -> Result<()> {
    let root = match path {
        Some(p) => p,
        None => crate::paths::default_test_root()?,
    };
    let project_root = root
        .ancestors()
        .find(|dir| dir.join("project.toml").is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("gos audit: no project.toml above {}", root.display()))?;

    let Some(advisories) = load_advisories(&project_root)? else {
        outln!(
            "audit: no advisory feed - none at {}, and no `[trusted-publishers]` key to \
             verify a registry feed against",
            project_root.join(LOCAL_FEED).display()
        );
        return Ok(());
    };

    let lockfile = gossamer_pkg::Lockfile::load(&project_root)
        .map_err(|e| anyhow!("reading project.lock: {e}"))?
        .ok_or_else(|| anyhow!("gos audit: no project.lock; run `gos fetch` first"))?;

    let referenced = referenced_paths(&project_root)?;
    let mut hits: Vec<(&Advisory, String)> = Vec::new();
    let mut suppressed = 0usize;
    for entry in &lockfile.entries {
        let id = entry.resolved.id.to_string();
        // Only a registry pin carries a version an advisory range can be
        // compared against. A git or path dependency is pinned by
        // revision, which no published range describes.
        let gossamer_pkg::ResolvedSource::Registry(version) = &entry.resolved.pin else {
            continue;
        };
        for advisory in &advisories {
            if advisory.package != id || !advisory.affects_version(version) {
                continue;
            }
            if advisory.is_reachable(&referenced) || all {
                hits.push((advisory, format!("{id}@{version}")));
            } else {
                suppressed += 1;
            }
        }
    }

    if format == "json" {
        out!("{}", render_json(&hits));
    } else {
        for (advisory, package) in &hits {
            outln!(
                "advisory[{id}]: {summary}\n  package: {package}\n  severity: {severity}\n  \
                 fixed in: {fixed}",
                id = advisory.id,
                summary = advisory.summary,
                severity = advisory.severity,
                fixed = advisory.fixed_in.as_ref().map_or_else(
                    || "no fixed version published".to_string(),
                    ToString::to_string
                ),
            );
        }
    }
    if suppressed > 0 {
        outln!(
            "audit: {suppressed} advisory(ies) affect a resolved version but name no item this \
             project references; `--all` lists them"
        );
    }
    if hits.is_empty() {
        outln!(
            "audit: no reachable advisories ({} checked)",
            advisories.len()
        );
        return Ok(());
    }
    Err(anyhow!("{} reachable advisory(ies)", hits.len()))
}

/// The advisory feed for this project, local first, then the registry.
///
/// A local file is what a test or an air-gapped build uses. A registry
/// feed is verified against a key the *project* pins, never one the
/// registry supplies: a feed whoever serves it can rewrite could hide an
/// advisory as easily as invent one. With no pinned key there is no
/// remote feed, rather than an unverified one.
fn load_advisories(project_root: &Path) -> Result<Option<Vec<Advisory>>> {
    let feed_path = project_root.join(LOCAL_FEED);
    if feed_path.is_file() {
        let text = std::fs::read_to_string(&feed_path)?;
        return parse_feed(&text).map(Some).map_err(|e| anyhow!("{e}"));
    }
    let Ok(text) = std::fs::read_to_string(project_root.join("project.toml")) else {
        return Ok(None);
    };
    let Ok(manifest) = gossamer_pkg::Manifest::parse(&text) else {
        return Ok(None);
    };
    let Some((_, key_hex)) = manifest.trusted_publishers.iter().next() else {
        return Ok(None);
    };
    let key = gossamer_pkg::signing::VerifyingKey::from_hex(key_hex)
        .map_err(|e| anyhow!("[trusted-publishers] key: {e}"))?;
    let transport = gossamer_pkg::transport::HttpTransport;
    gossamer_pkg::advisory::fetch_verified_feed(
        &transport,
        gossamer_pkg::fetch::DEFAULT_REGISTRY_URL,
        &key,
    )
    .map(Some)
    .map_err(|e| anyhow!("{e}"))
}

/// Every qualified path the project's sources mention.
///
/// Deliberately syntactic: an advisory names published item paths, and
/// a path a project never writes is one it cannot call. Over-approximate
/// rather than under - a path mentioned in dead code still counts, since
/// reporting an advisory that turns out to be unreachable is a smaller
/// failure than hiding one that is not.
fn referenced_paths(project_root: &Path) -> Result<BTreeSet<String>> {
    let src = project_root.join("src");
    let files = if src.is_dir() {
        collect_lint_targets(&src)?
    } else {
        collect_lint_targets(&project_root.to_path_buf())?
    };
    let mut out = BTreeSet::new();
    for file in files {
        let Ok(source) = read_source(&file) else {
            continue;
        };
        let mut map = gossamer_lex::SourceMap::new();
        let id = map.add_file(file.to_string_lossy().into_owned(), source.clone());
        let (sf, _) = gossamer_parse::parse_source_file(&source, id);
        collect_paths(&sf, &mut out);
    }
    Ok(out)
}

/// Collects `a::b::c` spellings from every path expression and import.
fn collect_paths(sf: &gossamer_ast::SourceFile, out: &mut BTreeSet<String>) {
    use gossamer_ast::visitor::Visitor;

    struct Scan<'a> {
        out: &'a mut BTreeSet<String>,
    }
    impl Visitor for Scan<'_> {
        fn visit_expr(&mut self, expr: &gossamer_ast::Expr) {
            if let gossamer_ast::ExprKind::Path(path) = &expr.kind
                && path.segments.len() > 1
            {
                let joined: Vec<&str> =
                    path.segments.iter().map(|s| s.name.name.as_str()).collect();
                self.out.insert(joined.join("::"));
            }
            gossamer_ast::visitor::walk_expr(self, expr);
        }
    }
    let mut scan = Scan { out };
    scan.visit_source_file(sf);

    for decl in &sf.uses {
        if let gossamer_ast::UseTarget::Module(path) = &decl.target {
            let base: Vec<&str> = path.segments.iter().map(|s| s.name.as_str()).collect();
            match &decl.list {
                Some(list) => {
                    for entry in list {
                        let mut full = base.clone();
                        full.push(entry.name.name.as_str());
                        out.insert(full.join("::"));
                    }
                }
                None => {
                    out.insert(base.join("::"));
                }
            }
        }
    }
}

/// Renders hits in the shared diagnostic JSON shape, so an MCP `check`
/// consumer needs no second parser.
fn render_json(hits: &[(&Advisory, String)]) -> String {
    let mut out = String::new();
    for (advisory, package) in hits {
        out.push_str(&format!(
            "{{\"schema\":1,\"code\":\"{id}\",\"severity\":\"error\",\"title\":{title},\
             \"labels\":[],\"notes\":[{package}],\"helps\":[{fixed}],\"suggestions\":[]}}\n",
            id = advisory.id,
            title = json_string(&advisory.summary),
            package = json_string(&format!("affects {package}")),
            fixed = json_string(&advisory.fixed_in.as_ref().map_or_else(
                || "no fixed version published".to_string(),
                |v| format!("fixed in {v}"),
            )),
        ));
    }
    out
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
