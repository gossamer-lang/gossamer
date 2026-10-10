//! Per-document state held by the LSP server.
//! A session is cheap: for each open document we keep the source
//! text, the source-map file id, and the outputs of the last full
//! front-end pipeline run. Every `didOpen` / `didChange` rebuilds
//! them - the front end is fast enough that incremental reuse is
//! not yet worth its complexity.

#![forbid(unsafe_code)]

use std::rc::Rc;

use gossamer_ast::SourceFile;
use gossamer_diagnostics::Diagnostic;
use gossamer_lex::{FileId, SourceMap, Span};
use gossamer_pkg::bundled_span::BundledSpan;
use gossamer_resolve::{Resolutions, resolve_source_file};
use gossamer_types::{TyCtxt, TypeTable};

use crate::navigation::DefinitionIndex;

/// Path-aware cursor context produced by [`DocumentAnalysis::cursor_context`].
/// Decomposes the source slice immediately to the left of the cursor into
/// the partial identifier the user is typing (`suffix`) plus the path
/// segments that preceded it (`qualifier`).
#[derive(Debug, Clone, Default)]
pub(crate) struct CursorContext<'a> {
    /// Identifier prefix immediately to the left of the cursor.
    /// Empty when the cursor sits in whitespace or on punctuation.
    pub suffix: &'a str,
    /// `::`-joined identifier segments preceding `suffix`. Empty when
    /// the cursor is on a bare prefix.
    pub qualifier: Vec<&'a str>,
    /// `true` when the cursor follows a `.` (receiver-method position).
    pub is_method_position: bool,
    /// `true` when the cursor is inside a `use ...` statement.
    pub is_use_context: bool,
}

impl<'a> CursorContext<'a> {
    /// Returns `qualifier` as a borrowed slice. Convenience for callers
    /// who want to forward the segments to a `&[&str]` API.
    #[must_use]
    pub(crate) fn qualifier_segments(&self) -> Vec<&'a str> {
        self.qualifier.clone()
    }
}

/// Analysis result for a single document.
///
/// Memory-shape note: the source text is held only inside `map`
/// (the `SourceMap` already owns one copy) and surfaced through
/// `source()` instead of being duplicated as a separate `String`
/// field. Top-level item names/spans are surfaced through
/// `top_level_span` / `index.def_iter()` instead of being mirrored
/// in a parallel `Vec<(Ident, Span)>`. Both changes shave per-file
/// LSP residency on workspaces with many open documents.
#[allow(
    dead_code,
    reason = "fields are reads from the LSP request handlers populated lazily as capabilities expand"
)]
pub(crate) struct UnitAnalysis {
    /// The unit's identity: its entry file's URI.
    pub(crate) name: String,
    /// Which file each region of the unit's source was read from.
    origins: Vec<BundledSpan>,
    pub(crate) file: FileId,
    pub(crate) map: SourceMap,
    pub(crate) sf: SourceFile,
    pub(crate) resolutions: Resolutions,
    pub(crate) types: TypeTable,
    pub(crate) tcx: TyCtxt,
    pub(crate) index: DefinitionIndex,
    /// Everything the front end said about the unit as a whole, before
    /// any one document's window narrows it.
    unit_diagnostics: Vec<Diagnostic>,
    /// Length of the assembled unit; the autoderive tail begins here.
    bundle_len: u32,
}

/// The text of every document the editor has open, by URI. A package's
/// unit is assembled from these wherever it names an open file, so unsaved
/// edits in several files of one package are analysed together.
pub(crate) type OpenBuffers = std::collections::BTreeMap<String, String>;

/// Analysis result for a single document: its window into the
/// [`UnitAnalysis`] of the package it belongs to, plus the diagnostics
/// that window earns.
///
/// Every file of a package shares one unit, so an editor with the whole
/// project open analyses it once and holds one copy of its AST and type
/// table.
#[allow(
    dead_code,
    reason = "fields are reads from the LSP request handlers populated lazily as capabilities expand"
)]
pub(crate) struct DocumentAnalysis {
    pub(crate) uri: String,
    pub(crate) unit: Rc<UnitAnalysis>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// First byte of the editor's own text within the unit's source.
    ///
    /// The unit is the whole package, so a module of a project sits
    /// inside the `mod` blocks its layout declares.
    /// `doc_start .. doc_start + user_len` is the window the editor's
    /// buffer occupies; everything the server sends back to the client
    /// must stay inside it.
    pub(crate) doc_start: u32,
    /// Byte length of the text the editor actually holds.
    pub(crate) user_len: u32,
}

impl std::ops::Deref for DocumentAnalysis {
    type Target = UnitAnalysis;

    fn deref(&self) -> &Self::Target {
        &self.unit
    }
}

/// The analysed unit of each package a session has documents from.
///
/// Keyed by the unit's own name (its entry file's URI) and holding the
/// source it was built from, so a document whose package assembles to
/// text already analysed reuses that analysis instead of repeating it.
#[derive(Default)]
pub(crate) struct UnitCache {
    entries: std::collections::HashMap<String, (String, Rc<UnitAnalysis>)>,
}

impl UnitCache {
    /// The analysis of `unit_source`, running the pipeline only when the
    /// cached one was built from different text.
    fn analysed(
        &mut self,
        name: &str,
        unit_source: String,
        origins: Vec<BundledSpan>,
    ) -> Rc<UnitAnalysis> {
        if let Some((cached_source, analysis)) = self.entries.get(name)
            && cached_source == &unit_source
        {
            return Rc::clone(analysis);
        }
        let analysis = Rc::new(analyse_unit(name, &unit_source, origins));
        self.entries
            .insert(name.to_string(), (unit_source, Rc::clone(&analysis)));
        analysis
    }

    /// How many analyses the cache holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drops the analyses no live document refers to.
    pub(crate) fn retain_live(&mut self) {
        self.entries
            .retain(|_, (_, analysis)| Rc::strong_count(analysis) > 1);
    }
}

/// Assembles the project compilation unit for a `file://` document,
/// using the editor's buffer for the open file and the on-disk text for
/// everything else, and reports the unit's name (its entry file's URI)
/// and where the buffer sits inside it.
///
/// The name is the unit's identity, so it is the entry's rather than the
/// open document's: it is what the comptime fold anchors relative reads
/// against, and what the manifest lookup walks up from. Returns `source`
/// unchanged for a document with no filesystem path or one that is not
/// inside a project.
#[cfg(not(target_arch = "wasm32"))]
fn bundle_project_unit(
    uri: &str,
    source: &str,
    open: &OpenBuffers,
) -> (String, String, u32, Vec<BundledSpan>) {
    let Some(path) = uri_to_path(uri) else {
        return (source.to_string(), uri.to_string(), 0, Vec::new());
    };
    let mut buffers: Vec<(std::path::PathBuf, String)> = open
        .iter()
        .filter(|(open_uri, _)| open_uri.as_str() != uri)
        .filter_map(|(open_uri, text)| Some((uri_to_path(open_uri)?, text.clone())))
        .collect();
    buffers.push((path.clone(), source.to_string()));
    let unit = gossamer_pkg::bundle::bundle_document_unit_with(&path, source, &buffers);
    let name = format!("file://{}", unit.entry.display());
    (unit.source, name, unit.window_start, unit.origins)
}

/// The foreign-function rule of the project the unit named `name` (its
/// entry's `file://` URI) belongs to, as the command line applies it.
#[cfg(not(target_arch = "wasm32"))]
fn unit_foreign_policy(name: &str) -> gossamer_types::ForeignPolicy {
    uri_to_path(name).map_or(gossamer_types::ForeignPolicy::Ungoverned, |path| {
        gossamer_types::ForeignPolicy::for_path(&path)
    })
}

/// The browser editor has no project on disk to govern it.
#[cfg(target_arch = "wasm32")]
fn unit_foreign_policy(_name: &str) -> gossamer_types::ForeignPolicy {
    gossamer_types::ForeignPolicy::Ungoverned
}

/// The wasm build has no filesystem to read sibling modules from, so a
/// document is its own compilation unit there.
#[cfg(target_arch = "wasm32")]
fn bundle_project_unit(
    uri: &str,
    source: &str,
    _open: &OpenBuffers,
) -> (String, String, u32, Vec<BundledSpan>) {
    (source.to_string(), uri.to_string(), 0, Vec::new())
}

/// Decodes a `file://` URI into a filesystem path, undoing the percent
/// escapes an editor applies to spaces and other reserved characters.
#[cfg(not(target_arch = "wasm32"))]
fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` (empty authority) is the only form editors send for
    // a local file; anything with a host is not a path we can read.
    let rest = rest.strip_prefix('/').map(|r| format!("/{r}"))?;
    let bytes = rest.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(std::path::PathBuf::from(String::from_utf8(out).ok()?))
}

/// Runs the full pipeline over `source` in a scratch cache. Callers
/// serving an editor pass their own through [`analyse_with`], so the
/// documents of one package share the analysis of that package.
#[cfg(test)]
pub(crate) fn analyse(uri: &str, source: &str) -> DocumentAnalysis {
    analyse_with(uri, source, &mut UnitCache::default(), &OpenBuffers::new())
}

/// Analyses `source` as the document at `uri`: assembles the package it
/// belongs to, reuses `cache`'s analysis of that package when the
/// assembled text is unchanged, and narrows the result to the window the
/// editor's buffer occupies.
pub(crate) fn analyse_with(
    uri: &str,
    source: &str,
    cache: &mut UnitCache,
    open: &OpenBuffers,
) -> DocumentAnalysis {
    // The source map strips a leading byte-order mark so spans and
    // diagnostic columns share one basis with the rest of the toolchain.
    // Measure the editor's text after the same strip, or every length
    // derived here describes three bytes the stored source does not have.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let (bundled, unit_name, doc_start, origins) = bundle_project_unit(uri, source, open);
    let unit = cache.analysed(&unit_name, bundled, origins);
    view_in_unit(uri, source, unit, doc_start)
}

/// The document at `uri`, holding `source`, as the already-analysed `unit`
/// sees it, when `unit` still holds that text: the view an edit to another
/// file of the same package leaves this one with.
pub(crate) fn sibling_view(
    uri: &str,
    source: &str,
    unit: &Rc<UnitAnalysis>,
) -> Option<DocumentAnalysis> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let doc_start = unit_window(uri, source, unit)?;
    Some(view_in_unit(uri, source, Rc::clone(unit), doc_start))
}

#[cfg(not(target_arch = "wasm32"))]
fn unit_window(uri: &str, source: &str, unit: &UnitAnalysis) -> Option<u32> {
    let path = uri_to_path(uri)?;
    gossamer_pkg::bundle::document_window(&unit.origins, &path, source.len()).map(|w| w.0)
}

#[cfg(target_arch = "wasm32")]
fn unit_window(_uri: &str, _source: &str, _unit: &UnitAnalysis) -> Option<u32> {
    None
}

/// Narrows `unit`'s findings to the window `doc_start ..` that the
/// document's `source` occupies, and adds the document's own lint findings.
fn view_in_unit(
    uri: &str,
    source: &str,
    unit: Rc<UnitAnalysis>,
    doc_start: u32,
) -> DocumentAnalysis {
    let user_len = u32::try_from(source.len()).unwrap_or(u32::MAX);
    let mut diagnostics = unit.unit_diagnostics.clone();
    // A diagnostic anchored in the synthesized autoderive tail still
    // describes a defect in the user's own declarations, so it is moved
    // onto the construct that caused the synthesis rather than dropped:
    // dropping it lets the editor call a file clean that `gos check`
    // rejects. One anchored in another file of the unit is that file's.
    crate::synthesis::reanchor_out_of_buffer(
        &mut diagnostics,
        &unit.sf,
        unit.map.source(unit.file),
        unit.file,
        doc_start,
        user_len,
        unit.bundle_len,
    );
    // Editors should see the same default lint findings as `gos lint`.
    // Parse the user source without the synthesized autoderive tail so
    // lint spans and fixes can never point outside the editor buffer.
    let (lint_sf, lint_parse_diags) = gossamer_parse::parse_source_file(source, unit.file);
    if lint_parse_diags.is_empty() {
        let mut registry = gossamer_lint::Registry::with_defaults();
        gossamer_lint::apply_attributes(&lint_sf.attrs, &mut registry);
        let mut lint_diagnostics = gossamer_lint::run(&lint_sf, source, &registry);
        let lint_fixes = gossamer_lint::fixes(&lint_sf, &registry, source);
        attach_lint_fixes(&mut lint_diagnostics, lint_fixes);
        // The lint pass reads the editor's buffer on its own, so its spans
        // are offsets into that buffer; every other diagnostic here is an
        // offset into the assembled unit.
        for diag in &mut lint_diagnostics {
            crate::synthesis::shift_spans(diag, doc_start);
        }
        diagnostics.extend(lint_diagnostics);
    }

    DocumentAnalysis {
        uri: uri.to_string(),
        unit,
        diagnostics,
        doc_start,
        user_len,
    }
}

/// Runs the full front end over one assembled compilation unit.
fn analyse_unit(name: &str, bundled: &str, origins: Vec<BundledSpan>) -> UnitAnalysis {
    // Mirror the driver pipeline: the parse-time autoderive step
    // synthesizes the serde free functions (`from_json::<T>` and
    // friends), `#[derive]` impls, and stdlib struct wrappers. The
    // synthesized text is APPENDED, so every user-code span is
    // unchanged; without this step the LSP reports `from_json` (and
    // every other synthesized name) as unresolved while `gos check`
    // accepts the file.
    let augmented = gossamer_parse::autoderive::augment_source(bundled);
    let bundle_len = u32::try_from(bundled.len()).unwrap_or(u32::MAX);
    let mut map = SourceMap::new();
    let file = map.add_file(name.to_string(), augmented.clone());
    let (mut sf, parse_diags) = gossamer_parse::autoderive::parse_with_autoderive(&augmented, file);
    let (resolutions, resolve_diags) = resolve_source_file(&sf);
    // A program that does not parse is not the program the later passes
    // see: `autoderive::augment_source` declines to synthesize from a
    // recovered tree, so the derived `fmt` / `to_string` / serde surface a
    // clean parse would carry is absent, and every pass below would report
    // its absence against a line the user wrote correctly. The parse
    // diagnostics are the actionable report, exactly as on the command
    // line; the passes still run so navigation keeps a type table.
    let parse_failed = !parse_diags.is_empty();
    let mut fatal: Vec<Diagnostic> = parse_diags
        .iter()
        .map(gossamer_parse::ParseDiagnostic::to_diagnostic)
        .collect();
    if !parse_failed {
        fatal.extend(unit_foreign_policy(name).diagnostics_per_declaration(
            &sf,
            gossamer_parse::autoderive::program_source_end(&augmented),
        ));
    }
    // The editor runs the command-line gate's own checks, so a file reads
    // clean here exactly when `gos check` accepts it.
    let mut tcx = TyCtxt::new();
    let earlier_fatal = !fatal.is_empty();
    let checks = gossamer_types::check_resolved_unit(
        &mut sf,
        &resolutions,
        &resolve_diags,
        parse_failed,
        earlier_fatal,
        &mut tcx,
        &mut (),
    );
    fatal.extend(checks.fatal);
    let types = checks.table;
    // The comptime fold lowers the program, so it runs only once every
    // earlier phase has accepted it - exactly the order `gos check` uses.
    // Advisory findings do not hold it back.
    if fatal.is_empty()
        && let Some(diag) = crate::comptime::fold_diagnostic(
            name,
            &augmented,
            &sf,
            &resolutions,
            &types,
            &mut tcx,
            file,
        )
    {
        fatal.push(diag);
    }
    let mut diagnostics = fatal;
    diagnostics.extend(checks.advisory);

    let index = DefinitionIndex::build(&sf, &augmented, &resolutions);

    UnitAnalysis {
        name: name.to_string(),
        origins,
        file,
        map,
        sf,
        resolutions,
        types,
        tcx,
        index,
        unit_diagnostics: diagnostics,
        bundle_len,
    }
}

/// Attaches `gos lint --fix` edits to their nearest same-lint
/// diagnostic so the generic LSP suggestion path can expose them.
fn attach_lint_fixes(diagnostics: &mut [Diagnostic], fixes: Vec<gossamer_lint::Fix>) {
    use gossamer_diagnostics::{Location, Suggestion};

    for fix in fixes {
        let lint_note = format!("lint: {}", fix.lint_id);
        let best = diagnostics
            .iter()
            .enumerate()
            .filter(|(_, diag)| diag.notes.iter().any(|note| note == &lint_note))
            .min_by_key(|(_, diag)| {
                let start = diag
                    .labels
                    .iter()
                    .find(|label| label.primary)
                    .or_else(|| diag.labels.first())
                    .map_or(0, |label| label.location.span.start);
                start.abs_diff(fix.span.start)
            })
            .map(|(index, _)| index);
        let Some(index) = best else {
            continue;
        };
        let title = fix.lint_id.replace('_', " ");
        diagnostics[index].suggestions.push(Suggestion::replacement(
            Location::new(fix.span.file, fix.span),
            format!("Fix {title}"),
            fix.replacement,
        ));
    }
}

impl DocumentAnalysis {
    /// Returns the document's source text. The text lives inside the
    /// embedded `SourceMap`, so this is a borrow into existing
    /// storage - no extra `String` clone per document.
    #[must_use]
    pub(crate) fn source(&self) -> &str {
        self.map.source(self.file)
    }

    /// Returns exactly the text the editor holds: the stored source
    /// minus the synthesized autoderive tail. Everything sent back to
    /// the client (formatting edits above all) must be computed
    /// against this prefix, or edits reference positions past the
    /// client's buffer.
    #[must_use]
    pub(crate) fn user_source(&self) -> &str {
        let source = self.map.source(self.file);
        let start = (self.doc_start as usize).min(source.len());
        let end = start
            .saturating_add(self.user_len as usize)
            .min(source.len());
        &source[start..end]
    }

    /// The unit offset a document-relative offset names.
    #[must_use]
    pub(crate) fn unit_offset(&self, relative: u32) -> u32 {
        self.doc_start.saturating_add(relative)
    }

    /// Whether `offset` lies inside the editor's own buffer.
    #[must_use]
    pub(crate) fn in_document(&self, offset: u32) -> bool {
        offset >= self.doc_start && offset <= self.doc_start.saturating_add(self.user_len)
    }

    /// Whether `span` lies inside the editor's own buffer.
    #[must_use]
    pub(crate) fn span_in_document(&self, span: Span) -> bool {
        self.in_document(span.start) && self.in_document(span.end)
    }

    /// Every item the editor's buffer declares, reached through whatever
    /// `mod` wrappers the package layout inlines the file under. A
    /// wrapper is not itself a declaration of this file, so the walk
    /// descends through it rather than reporting it.
    #[must_use]
    pub(crate) fn document_items(&self) -> Vec<&gossamer_ast::Item> {
        let mut out = Vec::new();
        self.collect_document_items(&self.sf.items, &mut out);
        out
    }

    fn collect_document_items<'a>(
        &self,
        items: &'a [gossamer_ast::Item],
        out: &mut Vec<&'a gossamer_ast::Item>,
    ) {
        let doc_end = self.doc_start.saturating_add(self.user_len);
        for item in items {
            if self.span_in_document(item.span) {
                out.push(item);
                continue;
            }
            if item.span.end < self.doc_start || item.span.start > doc_end {
                continue;
            }
            if let gossamer_ast::ItemKind::Mod(decl) = &item.kind
                && let gossamer_ast::ModBody::Inline(nested) = &decl.body
            {
                self.collect_document_items(nested, out);
            }
        }
    }

    /// Looks up the source span of the top-level item declaring
    /// `name`. Replaces the previous parallel `top_level: Vec<(Ident,
    /// Span)>` cache by reading from the existing
    /// [`DefinitionIndex`].
    #[must_use]
    pub(crate) fn top_level_span(&self, name: &str) -> Option<Span> {
        // The index records each definition's `name_span` (the
        // identifier itself) but not the whole-item span. For
        // go-to-def of top-level names the identifier span is the
        // editor-friendly target - the previous cache returned the
        // whole item span, but no caller actually needed that
        // wider range; navigation handlers only consume the
        // identifier position to centre the editor view.
        for (_, info) in self.index.def_iter() {
            if info.name == name {
                return Some(info.name_span);
            }
        }
        None
    }

    /// Translates a 0-based LSP position (whose character is a UTF-16
    /// code-unit offset) into a UTF-8 byte offset.
    #[must_use]
    pub(crate) fn position_to_offset(&self, line: u32, column: u32) -> Option<u32> {
        let source = self.user_source();
        let mut line_start = 0usize;
        for _ in 0..line {
            let newline = source[line_start..].find('\n')?;
            line_start += newline + 1;
        }
        let remainder = &source[line_start..];
        let mut line_text = remainder
            .split_once('\n')
            .map_or(remainder, |(text, _)| text);
        if let Some(without_cr) = line_text.strip_suffix('\r') {
            line_text = without_cr;
        }

        let mut utf16_column = 0u32;
        for (byte, ch) in line_text.char_indices() {
            if utf16_column == column {
                return u32::try_from(line_start + byte)
                    .ok()
                    .map(|offset| self.unit_offset(offset));
            }
            utf16_column += ch.len_utf16() as u32;
            if utf16_column > column {
                return None;
            }
        }
        (utf16_column == column)
            .then(|| u32::try_from(line_start + line_text.len()).ok())
            .flatten()
            .map(|offset| self.unit_offset(offset))
    }

    /// Translates a UTF-8 byte offset back into an LSP 0-based
    /// position using UTF-16 code units for the character field.
    #[must_use]
    pub(crate) fn offset_to_position(&self, offset: u32) -> (u32, u32) {
        let source = self.user_source();
        let relative = offset.saturating_sub(self.doc_start);
        let mut cap = std::cmp::min(relative as usize, source.len());
        while cap > 0 && !source.is_char_boundary(cap) {
            cap -= 1;
        }
        let prefix = &source[..cap];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
        let column = prefix[line_start..].encode_utf16().count() as u32;
        (line, column)
    }

    /// Returns the identifier covering `offset`, if any. Used by
    /// hover and go-to-def to map a cursor onto a symbol.
    #[must_use]
    pub(crate) fn word_at(&self, offset: u32) -> Option<&str> {
        let source = self.user_source();
        let offset = offset.checked_sub(self.doc_start)? as usize;
        if offset > source.len() || !source.is_char_boundary(offset) {
            return None;
        }
        let is_word = |ch: char| ch == '_' || unicode_ident::is_xid_continue(ch);
        let mut start = offset;
        for (index, ch) in source[..offset].char_indices().rev() {
            if !is_word(ch) {
                break;
            }
            start = index;
        }
        let mut end = offset;
        for (relative, ch) in source[offset..].char_indices() {
            if !is_word(ch) {
                break;
            }
            end = offset + relative + ch.len_utf8();
        }
        if start == end {
            return None;
        }
        Some(&source[start..end])
    }

    /// Path-aware cursor context. Walks left from `offset` over the
    /// source bytes and decomposes the construct under the cursor into
    /// `(qualifier, suffix)` plus a couple of position flags. This is
    /// the input every modern completion path consumes.
    #[must_use]
    pub(crate) fn cursor_context(&self, offset: u32) -> CursorContext<'_> {
        let source = self.user_source();
        let bytes = source.as_bytes();
        let mut end = (offset.saturating_sub(self.doc_start) as usize).min(bytes.len());
        while end > 0 && !source.is_char_boundary(end) {
            end -= 1;
        }
        let is_word = |ch: char| ch == '_' || unicode_ident::is_xid_continue(ch);
        // Walk left across the suffix word (the partial identifier the
        // cursor is currently typing).
        let mut start = end;
        while let Some((index, ch)) = source[..start].char_indices().next_back() {
            if !is_word(ch) {
                break;
            }
            start = index;
        }
        let suffix_start = start;
        let suffix_end = end;
        // Pre-suffix marker for detecting `.` (method position) or `::`
        // (path qualifier).
        let mut qualifier: Vec<&str> = Vec::new();
        let mut is_method_position = false;
        let mut scan = start;
        // Detect `::` immediately preceding the suffix.
        while scan >= 2 && bytes[scan - 1] == b':' && bytes[scan - 2] == b':' {
            scan -= 2;
            // Walk left over a word.
            let seg_end = scan;
            while let Some((index, ch)) = source[..scan].char_indices().next_back() {
                if !is_word(ch) {
                    break;
                }
                scan = index;
            }
            let seg_start = scan;
            if seg_start == seg_end {
                break;
            }
            if let Ok(seg) = std::str::from_utf8(&bytes[seg_start..seg_end]) {
                qualifier.push(seg);
            } else {
                break;
            }
        }
        qualifier.reverse();
        // Method position: a single `.` immediately before the suffix
        // (or the qualifier head if any).
        let dot_pos = if qualifier.is_empty() { start } else { scan };
        if dot_pos > 0 && bytes[dot_pos - 1] == b'.' {
            // Make sure it's not a `..` (range op) - if so leave it alone.
            if !(dot_pos >= 2 && bytes[dot_pos - 2] == b'.') {
                is_method_position = true;
            }
        }
        // Use-statement detection: scan backwards across the line
        // (skipping word/`::` chars + whitespace) and look for a leading
        // `use` keyword at the start of the current statement.
        let is_use_context = is_inside_use_statement(bytes, suffix_start);
        end = suffix_end;
        let suffix = std::str::from_utf8(&bytes[suffix_start..end]).unwrap_or("");
        CursorContext {
            suffix,
            qualifier,
            is_method_position,
            is_use_context,
        }
    }

    /// Returns every byte-range occurrence of `name` in the document,
    /// matched as a whole word. This is the legacy text-based
    /// fallback used when no resolution is available; semantic
    /// callers should prefer `find_semantic_references`.
    #[must_use]
    pub(crate) fn find_references(&self, name: &str) -> Vec<Span> {
        if name.is_empty() {
            return Vec::new();
        }
        let source = self.user_source();
        let is_word = |ch: char| ch == '_' || unicode_ident::is_xid_continue(ch);
        let mut out = Vec::new();
        for (cursor, _) in source.match_indices(name) {
            let end = cursor + name.len();
            let before_ok = source[..cursor]
                .chars()
                .next_back()
                .is_none_or(|ch| !is_word(ch));
            let after_ok = source[end..].chars().next().is_none_or(|ch| !is_word(ch));
            if before_ok && after_ok {
                out.push(Span::new(self.file, cursor as u32, end as u32));
            }
        }
        out
    }
}

/// True when the byte at `pos` in `bytes` sits inside a `use ...`
/// statement. Walks left across the current statement (stopping at the
/// nearest `;`, `{`, or `}`) and checks whether the first non-whitespace
/// run is the keyword `use`.
fn is_inside_use_statement(bytes: &[u8], pos: usize) -> bool {
    let cap = pos.min(bytes.len());
    let mut idx = cap;
    while idx > 0 {
        match bytes[idx - 1] {
            b';' | b'{' | b'}' => break,
            _ => idx -= 1,
        }
    }
    while idx < cap && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    let needle = b"use";
    if idx + needle.len() > cap {
        return false;
    }
    if &bytes[idx..idx + needle.len()] != needle {
        return false;
    }
    let after = idx + needle.len();
    after < bytes.len()
        && (bytes[after].is_ascii_whitespace() || bytes[after] == b':' || bytes[after] == b'{')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_at(source: &str) -> CursorContext<'_> {
        let cursor = source.find('|').expect("expected | cursor marker");
        // We can't construct a DocumentAnalysis without running the
        // parser, so call cursor_context against a synthetic doc that
        // shares only the source/file. Build via `analyse` for fidelity.
        // Static lifetime-erased buffer keeps the borrow valid.
        let cleaned: String = source[..cursor].to_string() + &source[cursor + 1..];
        let doc = Box::leak(Box::new(analyse("file:///t.gos", &cleaned)));
        let offset = u32::try_from(cursor).expect("cursor offset");
        doc.cursor_context(offset)
    }

    #[test]
    fn cursor_context_extracts_qualifier() {
        let ctx = ctx_at("fn main() { os::path::p| }\n");
        assert_eq!(ctx.suffix, "p");
        assert_eq!(ctx.qualifier, vec!["os", "path"]);
        assert!(!ctx.is_method_position);
        assert!(!ctx.is_use_context);
    }

    #[test]
    fn cursor_context_handles_method_position() {
        let ctx = ctx_at("fn main() { let v = vec![1]; v.p| }\n");
        assert_eq!(ctx.suffix, "p");
        assert!(ctx.qualifier.is_empty());
        assert!(ctx.is_method_position);
    }

    #[test]
    fn cursor_context_detects_use_statement() {
        let ctx = ctx_at("use std::os::|\n");
        assert_eq!(ctx.suffix, "");
        assert_eq!(ctx.qualifier, vec!["std", "os"]);
        assert!(ctx.is_use_context);
    }

    #[test]
    fn cursor_context_bare_prefix_returns_no_qualifier() {
        let ctx = ctx_at("fn main() { gr| }\n");
        assert_eq!(ctx.suffix, "gr");
        assert!(ctx.qualifier.is_empty());
        assert!(!ctx.is_method_position);
        assert!(!ctx.is_use_context);
    }

    #[test]
    fn lsp_positions_use_utf16_code_units() {
        let doc = analyse("file:///unicode.gos", "fn main() { \"😀\"; café() }\n");
        let cafe_offset = doc.user_source().find("café").unwrap() as u32;
        let (line, column) = doc.offset_to_position(cafe_offset);
        assert_eq!(line, 0);
        assert_eq!(column, 18, "emoji must occupy two UTF-16 code units");
        assert_eq!(doc.position_to_offset(line, column), Some(cafe_offset));
        assert_eq!(
            doc.position_to_offset(0, 14),
            None,
            "a position inside the emoji surrogate pair is invalid"
        );
    }

    #[test]
    fn unicode_words_and_completion_prefixes_remain_whole() {
        let doc = analyse(
            "file:///unicode.gos",
            "fn café() {}\nfn main() { café() }\n",
        );
        let call = doc.user_source().rfind("café").unwrap() as u32;
        assert_eq!(doc.word_at(call + 3), Some("café"));
        assert_eq!(
            doc.cursor_context(call + "café".len() as u32).suffix,
            "café"
        );
        assert_eq!(doc.find_references("café").len(), 2);
    }

    #[test]
    fn top_level_statements_produce_no_spurious_diagnostics() {
        // An entry file with bare top-level statements is implicitly `fn main`;
        // the LSP must analyse it cleanly, not report it as malformed items.
        let doc = analyse(
            "file:///t.gos",
            "println(\"hi\")\nlet x = 1\nprintln(\"{}\", x)\n",
        );
        assert!(
            doc.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            doc.diagnostics
        );
    }

    #[test]
    fn top_level_stdlib_paths_mark_grouped_imports_used() {
        let doc = analyse(
            "file:///imports.gos",
            "use std::{env, fs}\n\
             let root = env::args().first()\n\
             let exists = fs::exists(\".\")\n\
             println(\"{} {:?}\", exists, root)\n",
        );
        assert!(
            doc.diagnostics
                .iter()
                .all(|diag| diag.code.as_str() != "GL0002"),
            "used grouped imports were reported unused: {:?}",
            doc.diagnostics
        );
    }

    #[test]
    fn top_level_question_operator_is_accepted() {
        let src = "use std::errors\n\
                   fn f() -> Result<i64, errors::Error> { Ok(1) }\n\
                   let n = f()?\n\
                   println(\"{}\", n)\n";
        let doc = analyse("file:///t.gos", src);
        assert!(
            doc.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            doc.diagnostics
        );
    }

    #[test]
    fn bare_mutable_argument_reports_explicit_reference_diagnostic() {
        let doc = analyse(
            "file:///mut-arg.gos",
            "fn change(value: &mut i64) { *value = 0 }\n\
             fn main() { let mut value = 1\n change(value) }\n",
        );
        assert!(
            doc.diagnostics
                .iter()
                .any(|diag| diag.code.as_str() == "GT0046"),
            "expected explicit mutable-reference diagnostic: {:?}",
            doc.diagnostics
        );
    }

    #[test]
    fn a_parse_error_suppresses_the_later_phases_report() {
        // Synthesis declines to run on a recovered tree, so the struct's
        // derived `fmt` is absent and the checker would report the call on
        // line 2 as an unknown method. The command line reports the parse
        // error alone; the editor must agree.
        let doc = analyse(
            "file:///parse-gate.gos",
            "struct Point(i64)\n             println(\"{}\", Point(1).fmt())\n             println({}, Point(1))\n",
        );
        let codes: Vec<&str> = doc
            .diagnostics
            .iter()
            .map(|diag| diag.code.as_str())
            .collect();
        assert_eq!(
            codes,
            vec!["GP0024"],
            "only the parse error is actionable: {:?}",
            doc.diagnostics
        );
    }

    #[test]
    fn defaulted_and_named_arguments_type_check() {
        let doc = analyse(
            "file:///named-args.gos",
            "fn volume(width: i64, height: i64 = 2) -> i64 { width * height }\n             println(\"{}\", volume(2))\n             println(\"{}\", volume(width: 2, height: 3))\n",
        );
        assert!(
            doc.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            doc.diagnostics
        );
    }

    #[test]
    fn mixing_top_level_statements_with_explicit_main_is_reported() {
        let doc = analyse("file:///t.gos", "println(\"hi\")\nfn main() { }\n");
        assert!(
            !doc.diagnostics.is_empty(),
            "expected a conflict diagnostic for mixed entry forms"
        );
    }
}
