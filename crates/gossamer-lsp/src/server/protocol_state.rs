// LSP request-dispatch loop.
// Reads JSON-RPC messages from the client, routes them by method,
// and writes replies back. The server covers the spec subset
// Gossamer's editor integrations need:
//
// - lifecycle: `initialize`, `initialized`, `shutdown`, `exit`
// - sync: `textDocument/didOpen`, `didChange`, `didClose`,
//   `publishDiagnostics`
// - navigation: `hover`, `definition`, `typeDefinition`,
//   `references`, `documentHighlight`, `prepareRename`, `rename`
// - completion + signature help: `completion`, `signatureHelp`
// - structure: `documentSymbol`, `workspace/symbol`,
//   `foldingRange`
// - decoration: `inlayHint`, `semanticTokens/full`
// - formatting: `formatting`


use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufReader, BufWriter, Read, Write};
use std::rc::Rc;

use gossamer_diagnostics::{Diagnostic as GossamerDiagnostic, Severity};
use gossamer_lex::Span;
use gossamer_resolve::{DefKind, Resolution};
use gossamer_std::json::Value;
use gossamer_types::{render_public_ty, render_ty};

use crate::inlay::{InlayHint, collect_inlays};
use crate::navigation::{BindingInfo, DefinitionInfo, Locate, attach_resolution, locate};
use crate::protocol::{
    REQUEST_CANCELLED, Transport, field, field_str, field_u32, notification, response_error,
    response_ok,
};
use crate::semantic_tokens::{TOKEN_MODIFIERS, TOKEN_TYPES, full_tokens};
use crate::session::{
    CursorContext, DocumentAnalysis, OpenBuffers, UnitCache, analyse_with, sibling_view,
};
use crate::stdlib_index::{MemberSpec, StdlibIndex};
use crate::symbols::{document_symbols, folding_ranges, workspace_symbols};
use crate::workspace_index::{
    SymbolBucket, SymbolKey, SymbolOccurrence, UseOccurrence, WorkspaceIndex, WorkspaceItem,
};

/// Runs the server over the supplied reader/writer streams. Returns
/// `Ok(())` when the client sends `exit` after `shutdown`.
#[allow(
    clippy::too_many_lines,
    reason = "flat-shape dispatch / lowering - splitting hides the per-arm intent"
)]
fn run<R: Read, W: Write>(reader: R, writer: W) -> std::io::Result<()> {
    let mut transport = Transport::new(BufReader::new(reader), BufWriter::new(writer));
    let mut state = ServerState::new();
    // Edited text not yet analysed. Edits that arrive together are applied
    // as text and analysed once, when no more input is waiting or before
    // anything else is answered, so a burst of keystrokes costs one
    // analysis rather than one per keystroke.
    let mut edited: BTreeMap<String, String> = BTreeMap::new();
    // Messages already read and not yet handled. Everything that has
    // arrived is read before the next one is handled, so a cancellation
    // reaches the request it names while that request is still waiting.
    let mut queued: VecDeque<Value> = VecDeque::new();

    loop {
        if queued.is_empty() {
            if !edited.is_empty() && !transport.has_buffered_input() {
                flush_edits(&mut state, &mut edited, &mut transport)?;
            }
            let Some(message) = transport.read_message()? else {
                return Ok(());
            };
            queue_message(message, &mut queued, &mut transport)?;
        }
        while transport.has_buffered_input() {
            let Some(message) = transport.read_message()? else {
                break;
            };
            queue_message(message, &mut queued, &mut transport)?;
        }
        let Some(message) = queued.pop_front() else {
            continue;
        };
        let Some(method) = field_str(&message, "method") else {
            continue;
        };
        let id = field(&message, "id").clone();
        let params = field(&message, "params").clone();
        if method == "textDocument/didChange" {
            if let Some(uri) = field_str(field(&params, "textDocument"), "uri")
                && let Some(text) =
                    state.edited_text(uri, field(&params, "contentChanges"), &edited)
            {
                edited.insert(uri.to_string(), text);
            }
            continue;
        }
        flush_edits(&mut state, &mut edited, &mut transport)?;

        match method {
            "initialize" => {
                state.discover_workspace_roots(&params);
                transport.write_message(&response_ok(id, initialize_result()))?;
            }
            "initialized" => {}
            "textDocument/didOpen" => {
                if let Some((uri, text)) = extract_did_open(&params) {
                    for affected in state.update(&uri, &text) {
                        for notif in state.publish_diagnostics(&affected) {
                            transport.write_message(&notif)?;
                        }
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = field_str(field(&params, "textDocument"), "uri") {
                    state.close(uri);
                    transport.write_message(&empty_diagnostics_notification(uri))?;
                }
            }
            "textDocument/hover" => {
                let result = state.hover(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/definition" => {
                let result = state.definition(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/typeDefinition" => {
                let result = state.type_definition(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/completion" => {
                let result = state.completion(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/references" => {
                let result = state.references(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/documentHighlight" => {
                let result = state.document_highlight(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/prepareRename" => {
                let result = state.prepare_rename(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/rename" => {
                let result = state.rename(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/inlayHint" => {
                let result = state.inlay_hints(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/documentSymbol" => {
                let result = state.document_symbols(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "workspace/symbol" => {
                let result = state.workspace_symbols(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/foldingRange" => {
                let result = state.folding_ranges(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/signatureHelp" => {
                let result = state.signature_help(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/formatting" => {
                let result = state.formatting(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/codeAction" => {
                let result = state.code_actions(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "textDocument/semanticTokens/full" => {
                let result = state.semantic_tokens(&params);
                transport.write_message(&response_ok(id, result))?;
            }
            "shutdown" => {
                transport.write_message(&response_ok(id, Value::Null))?;
            }
            "exit" => return Ok(()),
            _ => {
                if !matches!(id, Value::Null) {
                    transport.write_message(&response_ok(id, Value::Null))?;
                }
            }
        }
    }
}

/// Convenience wrapper that runs the server over the process's
/// stdio streams.
pub fn run_stdio() -> std::io::Result<()> {
    run(std::io::stdin(), std::io::stdout())
}

fn initialize_result() -> Value {
    let mut caps = BTreeMap::new();
    let mut sync = BTreeMap::new();
    sync.insert("openClose".to_string(), Value::Bool(true));
    sync.insert("change".to_string(), Value::Number(1.0));
    caps.insert("textDocumentSync".to_string(), Value::Object(sync));
    caps.insert("hoverProvider".to_string(), Value::Bool(true));
    caps.insert("definitionProvider".to_string(), Value::Bool(true));
    caps.insert("typeDefinitionProvider".to_string(), Value::Bool(true));
    caps.insert("referencesProvider".to_string(), Value::Bool(true));
    caps.insert("documentHighlightProvider".to_string(), Value::Bool(true));
    caps.insert("inlayHintProvider".to_string(), Value::Bool(true));
    caps.insert("documentSymbolProvider".to_string(), Value::Bool(true));
    caps.insert("workspaceSymbolProvider".to_string(), Value::Bool(true));
    caps.insert("foldingRangeProvider".to_string(), Value::Bool(true));
    caps.insert("documentFormattingProvider".to_string(), Value::Bool(true));
    let mut code_action = BTreeMap::new();
    code_action.insert(
        "codeActionKinds".to_string(),
        Value::Array(vec![
            Value::String("quickfix".to_string()),
            Value::String("source.fixAll.gossamer".to_string()),
        ]),
    );
    caps.insert("codeActionProvider".to_string(), Value::Object(code_action));
    let mut rename = BTreeMap::new();
    rename.insert("prepareProvider".to_string(), Value::Bool(true));
    caps.insert("renameProvider".to_string(), Value::Object(rename));
    let mut completion = BTreeMap::new();
    completion.insert(
        "triggerCharacters".to_string(),
        Value::Array(vec![
            Value::String(".".to_string()),
            Value::String(":".to_string()),
        ]),
    );
    let mut completion_item_caps = BTreeMap::new();
    completion_item_caps.insert("snippetSupport".to_string(), Value::Bool(true));
    completion.insert(
        "completionItem".to_string(),
        Value::Object(completion_item_caps),
    );
    caps.insert("completionProvider".to_string(), Value::Object(completion));
    let mut sig = BTreeMap::new();
    sig.insert(
        "triggerCharacters".to_string(),
        Value::Array(vec![
            Value::String("(".to_string()),
            Value::String(",".to_string()),
        ]),
    );
    caps.insert("signatureHelpProvider".to_string(), Value::Object(sig));
    caps.insert(
        "semanticTokensProvider".to_string(),
        semantic_tokens_capability(),
    );
    let mut info = BTreeMap::new();
    info.insert("name".to_string(), Value::String("gos-lsp".to_string()));
    info.insert(
        "version".to_string(),
        Value::String(env!("CARGO_PKG_VERSION").to_string()),
    );
    let mut root = BTreeMap::new();
    root.insert("capabilities".to_string(), Value::Object(caps));
    root.insert("serverInfo".to_string(), Value::Object(info));
    Value::Object(root)
}

fn semantic_tokens_capability() -> Value {
    let mut legend = BTreeMap::new();
    legend.insert(
        "tokenTypes".to_string(),
        Value::Array(
            TOKEN_TYPES
                .iter()
                .map(|t| Value::String((*t).to_string()))
                .collect(),
        ),
    );
    legend.insert(
        "tokenModifiers".to_string(),
        Value::Array(
            TOKEN_MODIFIERS
                .iter()
                .map(|t| Value::String((*t).to_string()))
                .collect(),
        ),
    );
    let mut cap = BTreeMap::new();
    cap.insert("legend".to_string(), Value::Object(legend));
    cap.insert("full".to_string(), Value::Bool(true));
    cap.insert("range".to_string(), Value::Bool(false));
    Value::Object(cap)
}

/// Converts a `file://...` URI into a filesystem path. Returns
/// `None` for non-`file://` schemes (e.g. `inmemory://`) and for
/// URIs that don't decode cleanly.
fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let hex = bytes.get(index + 1..index + 3)?;
        let high = (hex[0] as char).to_digit(16)?;
        let low = (hex[1] as char).to_digit(16)?;
        let byte = u8::try_from((high << 4) | low).ok()?;
        if byte == 0 {
            return None;
        }
        decoded.push(byte);
        index += 3;
    }
    String::from_utf8(decoded).ok()
}

fn extract_did_open(params: &Value) -> Option<(String, String)> {
    let doc = field(params, "textDocument");
    let uri = field_str(doc, "uri")?.to_string();
    let text = field_str(doc, "text")?.to_string();
    Some((uri, text))
}

fn text_range_to_offsets(source: &str, range: &Value) -> Option<(usize, usize)> {
    let start = field(range, "start");
    let end = field(range, "end");
    let start = text_position_to_offset(
        source,
        field_u32(start, "line")?,
        field_u32(start, "character").unwrap_or(0),
    )?;
    let end = text_position_to_offset(
        source,
        field_u32(end, "line")?,
        field_u32(end, "character").unwrap_or(0),
    )?;
    (start <= end).then_some((start, end))
}

fn text_position_to_offset(source: &str, line: u32, column: u32) -> Option<usize> {
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
            return Some(line_start + byte);
        }
        utf16_column += ch.len_utf16() as u32;
        if utf16_column > column {
            return None;
        }
    }
    (utf16_column == column).then_some(line_start + line_text.len())
}

/// Queues `message`, or, for a `$/cancelRequest`, answers the queued request
/// it names with `RequestCancelled` and drops it, so a request the client no
/// longer wants costs no analysis. A cancellation for a request already
/// answered, or never sent, has nothing left to do.
fn queue_message<R: Read, W: Write>(
    message: Value,
    queued: &mut VecDeque<Value>,
    transport: &mut Transport<BufReader<R>, BufWriter<W>>,
) -> std::io::Result<()> {
    if field_str(&message, "method") != Some("$/cancelRequest") {
        queued.push_back(message);
        return Ok(());
    }
    let target = field(field(&message, "params"), "id");
    let Some(at) = queued.iter().position(|waiting| {
        field_str(waiting, "method").is_some() && same_request_id(field(waiting, "id"), target)
    }) else {
        return Ok(());
    };
    if let Some(cancelled) = queued.remove(at) {
        let id = field(&cancelled, "id").clone();
        transport.write_message(&response_error(id, REQUEST_CANCELLED, "request cancelled"))?;
    }
    Ok(())
}

/// Whether two JSON-RPC ids name the same request. An id is an integer or a
/// string, and a parsed integer may arrive in either of the parser's number
/// forms.
fn same_request_id(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::String(x), Value::String(y)) => x == y,
        _ => match (gossamer_std::json::as_i64(a), gossamer_std::json::as_i64(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        },
    }
}

/// Analyses every edited document and publishes the diagnostics of each
/// document the analyses changed.
fn flush_edits<R: Read, W: Write>(
    state: &mut ServerState,
    edited: &mut BTreeMap<String, String>,
    transport: &mut Transport<BufReader<R>, BufWriter<W>>,
) -> std::io::Result<()> {
    let mut affected: Vec<String> = Vec::new();
    for (uri, text) in std::mem::take(edited) {
        for doc in state.update(&uri, &text) {
            if !affected.contains(&doc) {
                affected.push(doc);
            }
        }
    }
    for uri in affected {
        for notif in state.publish_diagnostics(&uri) {
            transport.write_message(&notif)?;
        }
    }
    Ok(())
}

struct ServerState {
    documents: HashMap<String, DocumentAnalysis>,
    /// Analysed compilation units, shared by every open document of the
    /// package they assemble.
    units: UnitCache,
    stdlib: StdlibIndex,
    workspace: WorkspaceIndex,
}

impl ServerState {
    fn new() -> Self {
        Self {
            documents: HashMap::new(),
            units: UnitCache::default(),
            stdlib: StdlibIndex::build(),
            workspace: WorkspaceIndex::default(),
        }
    }

    /// Re-analyses `uri` with `text`, then every other document of the
    /// package it belongs to, so each one sees the package as the editor
    /// now holds it. Answers the documents whose analysis changed.
    fn update(&mut self, uri: &str, text: &str) -> Vec<String> {
        let mut open: OpenBuffers = self
            .documents
            .iter()
            .map(|(doc_uri, doc)| (doc_uri.clone(), doc.user_source().to_string()))
            .collect();
        open.insert(uri.to_string(), text.to_string());
        let analysis = analyse_with(uri, text, &mut self.units, &open);
        let unit_name = analysis.unit.name.clone();
        self.workspace.update(uri, &analysis);
        self.documents.insert(uri.to_string(), analysis);
        let mut affected = vec![uri.to_string()];
        let siblings: Vec<String> = self
            .documents
            .iter()
            .filter(|(doc_uri, doc)| doc_uri.as_str() != uri && doc.unit.name == unit_name)
            .map(|(doc_uri, _)| doc_uri.clone())
            .collect();
        let unit = Rc::clone(&self.documents[uri].unit);
        for sibling in siblings {
            let Some(sibling_text) = open.get(&sibling) else {
                continue;
            };
            let analysis = sibling_view(&sibling, sibling_text, &unit)
                .unwrap_or_else(|| analyse_with(&sibling, sibling_text, &mut self.units, &open));
            self.workspace.update(&sibling, &analysis);
            self.documents.insert(sibling.clone(), analysis);
            affected.push(sibling);
        }
        self.units.retain_live();
        affected
    }

    /// `uri`'s text with `changes` applied, starting from the edited text
    /// still awaiting analysis when there is some.
    fn edited_text(
        &self,
        uri: &str,
        changes: &Value,
        edited: &BTreeMap<String, String>,
    ) -> Option<String> {
        let Value::Array(items) = changes else {
            return None;
        };
        if items.is_empty() {
            return None;
        }
        let mut text = edited.get(uri).cloned().unwrap_or_else(|| {
            self.documents
                .get(uri)
                .map_or_else(String::new, |doc| doc.user_source().to_string())
        });
        for change in items {
            let Some(change_text) = field_str(change, "text") else {
                continue;
            };
            let range = field(change, "range");
            if matches!(range, Value::Object(_)) {
                let Some((start, end)) = text_range_to_offsets(&text, range) else {
                    continue;
                };
                text.replace_range(start..end, change_text);
            } else {
                text.clear();
                text.push_str(change_text);
            }
        }
        Some(text)
    }

    /// Applies one `didChange` and analyses the result at once.
    #[cfg(test)]
    fn apply_did_change(&mut self, uri: &str, changes: &Value) -> Vec<String> {
        let Some(text) = self.edited_text(uri, changes, &BTreeMap::new()) else {
            return Vec::new();
        };
        self.update(uri, &text)
    }

    fn close(&mut self, uri: &str) {
        self.documents.remove(uri);
        self.workspace.remove(uri);
        self.units.retain_live();
    }

    /// Walks the workspace root advertised by `initialize` for
    /// `.gos` files and seeds the analysis cache + workspace index
    /// with each one. Honours a simple `.gitignore` (only the
    /// `target/`, `.git/`, and dotfile excludes the typical Rust
    /// project ships); caps the discovery at 1000 files to keep
    /// large monorepos responsive.
    fn discover_workspace_roots(&mut self, params: &Value) {
        let mut roots: Vec<String> = Vec::new();
        if let Some(uri) = field_str(params, "rootUri") {
            if let Some(path) = file_uri_to_path(uri) {
                roots.push(path);
            }
        }
        if let Value::Array(folders) = field(params, "workspaceFolders") {
            for folder in folders {
                if let Some(uri) = field_str(folder, "uri") {
                    if let Some(path) = file_uri_to_path(uri) {
                        roots.push(path);
                    }
                }
            }
        }
        let mut budget = 1000usize;
        for root in roots {
            self.scan_workspace_path(&root, &mut budget);
        }
    }

    /// Recursive helper for [`Self::discover_workspace_roots`].
    fn scan_workspace_path(&mut self, path: &str, budget: &mut usize) {
        if *budget == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return;
            }
            let entry_path = entry.path();
            let Some(name) = entry_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // Skip the usual junk drawers without parsing a full
            // .gitignore. Editors typically already exclude these.
            if matches!(name, "target" | ".git" | "node_modules") || name.starts_with('.') {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                if let Some(p) = entry_path.to_str() {
                    self.scan_workspace_path(p, budget);
                }
                continue;
            }
            if !std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("gos"))
            {
                continue;
            }
            let Some(path_str) = entry_path.to_str() else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&entry_path) else {
                continue;
            };
            let uri = format!("file://{path_str}");
            if self.documents.contains_key(&uri) {
                continue;
            }
            let _ = self.update(&uri, &text);
            *budget -= 1;
        }
    }

    /// Returns the `CodeAction[]` for `textDocument/codeAction`.
    ///
    /// The request carries a `range` and (optionally) a list of
    /// diagnostics. We iterate the document's diagnostics and
    /// surface every `Suggestion` whose label overlaps the
    /// request range as a `quickfix` action with a
    /// `WorkspaceEdit` applying the suggestion's replacement.
    ///
    /// Diagnostics without a `Suggestion` produce no action.
    /// Resolver / parser / typechecker fix-its (GP0006, GP0016,
    /// GR0001 import-this-name, etc.) all live as
    /// `Suggestion` payloads on the diagnostic; once the
    /// suggestion is computed by the front end, this method is
    /// the only thing that decides whether the client sees it.
    fn code_actions(&self, params: &Value) -> Value {
        let Some(uri) = field_str(field(params, "textDocument"), "uri") else {
            return Value::Array(Vec::new());
        };
        let Some(doc) = self.documents.get(uri) else {
            return Value::Array(Vec::new());
        };
        let req_range = field(params, "range");
        let (req_start, req_end) = lsp_range_to_offsets(doc, req_range);
        let mut actions: Vec<Value> = Vec::new();
        let wants_quickfix = code_action_kind_requested(params, "quickfix");
        let wants_fix_all = code_action_kind_requested(params, "source.fixAll.gossamer");
        if !wants_quickfix && !wants_fix_all {
            return Value::Array(actions);
        }
        for diag in &doc.diagnostics {
            if !wants_quickfix || !code_action_diagnostic_requested(params, doc, diag) {
                continue;
            }
            // Skip when the diagnostic's primary label does not
            // overlap the request range. Clients filter by
            // their cursor position; we honour that filter so a
            // codeAction request at line 10 does not show fix-its
            // for diagnostics at line 200.
            let diag_span = diag
                .labels
                .iter()
                .find(|l| l.primary)
                .or_else(|| diag.labels.first())
                .map(|l| l.location.span);
            if let Some(span) = diag_span {
                if !offset_ranges_overlap(
                    req_start,
                    req_end,
                    span.start as usize,
                    span.end as usize,
                ) {
                    continue;
                }
            }
            for suggestion in &diag.suggestions {
                actions.push(suggestion_to_code_action(doc, uri, diag, suggestion));
            }
            if diag.code.as_str() == "GR0001" {
                actions.extend(self.auto_import_code_actions(doc, uri, diag));
            }
        }
        if wants_fix_all {
            if let Some(action) = fix_all_code_action(doc, uri) {
                actions.push(action);
            }
        }
        Value::Array(actions)
    }

    /// Offers exact stdlib imports for an unresolved bare name. This
    /// complements completion-time auto-imports when the user has
    /// already finished typing the unknown identifier.
    fn auto_import_code_actions(
        &self,
        doc: &DocumentAnalysis,
        uri: &str,
        diag: &GossamerDiagnostic,
    ) -> Vec<Value> {
        let Some(span) = diag
            .labels
            .iter()
            .find(|label| label.primary)
            .or_else(|| diag.labels.first())
            .map(|label| label.location.span)
        else {
            return Vec::new();
        };
        let source = doc.source();
        let start = span.start as usize;
        let end = span.end as usize;
        let Some(name) = source.get(start..end) else {
            return Vec::new();
        };
        if name.is_empty() {
            return Vec::new();
        }
        let existing = collect_existing_imports(doc);
        if let Some((head, _)) = name.split_once("::") {
            let Some(path) = self.stdlib.canonical_module_for_leaf(head) else {
                return Vec::new();
            };
            if existing.iter().any(|import| import == path) {
                return Vec::new();
            }
            return vec![import_to_code_action(doc, uri, diag, path)];
        }
        self.stdlib
            .fuzzy_paths_for(name)
            .into_iter()
            .filter(|path| !existing.contains(path))
            .map(|path| import_to_code_action(doc, uri, diag, &path))
            .collect()
    }

    fn publish_diagnostics(&self, uri: &str) -> Vec<Value> {
        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let items: Vec<Value> = doc
            .diagnostics
            .iter()
            .map(|d| diagnostic_to_lsp(doc, d))
            .collect();
        let mut params = BTreeMap::new();
        params.insert("uri".to_string(), Value::String(uri.to_string()));
        params.insert("diagnostics".to_string(), Value::Array(items));
        vec![notification(
            "textDocument/publishDiagnostics",
            Value::Object(params),
        )]
    }

    fn hover(&self, params: &Value) -> Value {
        let Some((doc, offset)) = self.locate(params) else {
            return Value::Null;
        };
        let Some(loc) = self.cursor(doc, offset) else {
            // Fallback: word-based hover when we couldn't locate a
            // semantic node (e.g. the cursor is in whitespace inside
            // a partially-parseable file).
            return word_hover(doc, offset);
        };
        let body = render_hover(doc, &loc);
        if body.is_empty() {
            // A built-in trait has no declaration to index, so the header and
            // the bound that name it fall to the word-based rendering.
            return word_hover(doc, offset);
        }
        let mut contents = BTreeMap::new();
        contents.insert("kind".to_string(), Value::String("markdown".to_string()));
        contents.insert("value".to_string(), Value::String(body));
        let mut hover = BTreeMap::new();
        hover.insert("contents".to_string(), Value::Object(contents));
        Value::Object(hover)
    }

    fn definition(&self, params: &Value) -> Value {
        let Some((doc, offset)) = self.locate(params) else {
            return Value::Null;
        };
        let Some(loc) = self.cursor(doc, offset) else {
            return self.definition_by_name(doc, offset);
        };
        match &loc {
            Locate::PathExpr {
                resolution: Some(Resolution::Local(node)),
                ..
            }
            | Locate::TypePath {
                resolution: Some(Resolution::Local(node)),
                ..
            } => doc
                .index
                .local(*node)
                .map_or(Value::Null, |info| location(doc, info.name_span)),
            Locate::PathExpr {
                resolution: Some(Resolution::Def { def, .. }),
                ..
            }
            | Locate::TypePath {
                resolution: Some(Resolution::Def { def, .. }),
                ..
            } => doc
                .index
                .def(*def)
                .map_or(Value::Null, |info| location(doc, info.name_span)),
            Locate::Binding { name_span, .. } => location(doc, *name_span),
            _ => self.definition_by_name(doc, offset),
        }
    }

    fn type_definition(&self, params: &Value) -> Value {
        let Some((doc, offset)) = self.locate(params) else {
            return Value::Null;
        };
        let Some(loc) = self.cursor(doc, offset) else {
            return Value::Null;
        };
        // For locals and field accesses, look up the inferred type's
        // node in the type table → if it's an Adt resolved via the
        // resolver, jump to that struct/enum. For path expressions
        // already pointing at a type, behave like `definition`.
        match &loc {
            Locate::TypePath {
                resolution: Some(Resolution::Def { def, .. }),
                ..
            } => doc
                .index
                .def(*def)
                .map_or(Value::Null, |info| location(doc, info.name_span)),
            Locate::PathExpr {
                resolution: Some(resolution),
                ..
            }
            | Locate::TypePath {
                resolution: Some(resolution),
                ..
            } => self.locate_type_definition(doc, *resolution),
            Locate::Binding { pattern_id, .. } => {
                let Some(ty) = doc.types.get(*pattern_id) else {
                    return Value::Null;
                };
                self.locate_type_in_index(doc, &render_ty(&doc.tcx, ty))
            }
            Locate::Field { .. } | Locate::PathExpr { .. } | Locate::TypePath { .. } => Value::Null,
        }
    }

    /// Re-routes a `Resolution` carrying a value (function / const) onto
    /// the type definition of the value's static type. Functions go to
    /// their return type's definition; constants to the const type.
    fn locate_type_definition(&self, doc: &DocumentAnalysis, resolution: Resolution) -> Value {
        let Resolution::Def { def, .. } = resolution else {
            return Value::Null;
        };
        let Some(info) = doc.index.def(def) else {
            return Value::Null;
        };
        // Hover signature contains the rendered return type at the end
        // (`-> Foo`). Pull the last identifier word out and look it up.
        if let Some(arrow) = info.signature.rfind("->") {
            let ret = info.signature[arrow + 2..].trim();
            let target = self.locate_type_in_index(doc, ret);
            if !matches!(target, Value::Null) {
                return target;
            }
        }
        Value::Null
    }

    fn locate_type_in_index(&self, doc: &DocumentAnalysis, name: &str) -> Value {
        let head = name
            .trim_start_matches(['&', '*', ' '])
            .trim_end_matches([',', ';', ' '])
            .split(['<', '[', '(', ' '])
            .next()
            .unwrap_or("");
        if head.is_empty() {
            return Value::Null;
        }
        for (_, info) in doc.index_pairs() {
            if info.name == head
                && matches!(
                    info.kind,
                    DefKind::Struct | DefKind::Enum | DefKind::Trait | DefKind::TypeAlias
                )
            {
                return location(doc, info.name_span);
            }
        }
        Value::Null
    }

    fn definition_by_name(&self, doc: &DocumentAnalysis, offset: u32) -> Value {
        let Some(word) = doc.word_at(offset) else {
            return Value::Null;
        };
        let Some(span) = doc.top_level_span(word) else {
            return Value::Null;
        };
        location(doc, span)
    }

    fn completion(&self, params: &Value) -> Value {
        let Some((doc, offset)) = self.locate(params) else {
            return Value::Array(Vec::new());
        };
        let cursor = doc.cursor_context(offset);
        let prefix = cursor.suffix;
        let mut items: Vec<Value> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Use-context: surface module / item members of the qualifier.
        if cursor.is_use_context {
            if cursor.qualifier.is_empty() {
                for spec in self.stdlib.root_modules() {
                    if spec.name.starts_with(prefix) && seen.insert(spec.name.clone()) {
                        items.push(member_to_completion(&spec));
                    }
                }
            } else if let Some(members) = self.stdlib.members_of(&cursor.qualifier_segments()) {
                for spec in &members {
                    if spec.name.starts_with(prefix) && seen.insert(spec.name.clone()) {
                        items.push(member_to_completion(spec));
                    }
                }
            }
            return Value::Array(items);
        }

        // Receiver-method completion (`expr.p|`).
        if cursor.is_method_position {
            self.method_completions(doc, offset, prefix, &mut items, &mut seen);
            return Value::Array(items);
        }

        // Module / type-qualified path completion (`os::p|`, `Vec::n|`).
        if !cursor.qualifier.is_empty() {
            let qualifier = cursor.qualifier_segments();
            if let Some(members) = self.stdlib.members_of(&qualifier) {
                let imported = stdlib_qualifier_is_imported(doc, &qualifier);
                let import_path = (!imported && qualifier.len() == 1)
                    .then(|| self.stdlib.canonical_module_for_leaf(qualifier[0]))
                    .flatten();
                if imported || import_path.is_some() {
                    for spec in members {
                        if spec.name.starts_with(prefix) && seen.insert(spec.name.clone()) {
                            let item = member_to_completion(&spec);
                            items.push(import_path.map_or(item.clone(), |path| {
                                completion_with_module_import(doc, item, path)
                            }));
                        }
                    }
                }
            }
            // Type-qualified user types (e.g. `MyEnum::V`).
            self.type_qualified_completions(doc, &cursor, prefix, &mut items, &mut seen);
            // No fall-through to bare prefix when the user already
            // typed `::` - that would surface unrelated names.
            return Value::Array(items);
        }

        // Bare prefix path: top-level items, locals, keywords, builtins.
        // The DefinitionIndex already records every top-level item with
        // its `name` and `name_span`, so iterate it directly instead of
        // keeping a parallel `top_level: Vec<(Ident, Span)>` cache.
        for (_, info) in doc.index.def_iter() {
            if info.name.starts_with(prefix) && seen.insert(info.name.clone()) {
                items.push(completion_item_for(doc, &info.name, prefix));
            }
        }
        // Locals in scope: we don't track scopes at hover-time, so just
        // surface every binding seen in the file. Editors rank short
        // prefixes before stale names.
        for (_, binding) in doc.binding_pairs() {
            if binding.name.starts_with(prefix) && seen.insert(binding.name.clone()) {
                items.push(completion_item_local(&binding.name));
            }
        }
        for name in KEYWORDS {
            if name.starts_with(prefix) && seen.insert((*name).to_string()) {
                items.push(completion_item(name, 14));
            }
        }
        for name in BUILTIN_COMPLETIONS {
            if name.starts_with(prefix) && seen.insert((*name).to_string()) {
                items.push(completion_function_item_with_snippet(name));
            }
        }
        // Workspace-wide top-level items (other open files).
        if !prefix.is_empty() {
            for item in self.workspace.by_prefix(prefix, &doc.uri) {
                if seen.insert(item.name.clone()) {
                    items.push(workspace_completion_item(&item));
                }
            }
        }
        // Auto-import suggestions for unqualified names that don't
        // resolve in the current file.
        if !prefix.is_empty() {
            self.auto_import_completions(doc, prefix, &mut items, &mut seen);
        }
        Value::Array(items)
    }

    /// Fills `items` with method-call completions when the cursor is in
    /// `receiver.suffix|` position. Best-effort: walks the receiver's
    /// resolved type back to a set of impl/trait methods either declared
    /// on the receiver in this file or known to be built-in.
    fn method_completions(
        &self,
        doc: &DocumentAnalysis,
        offset: u32,
        prefix: &str,
        items: &mut Vec<Value>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        let receiver_kind = receiver_descriptor(doc, offset);
        for method in builtin_methods_for(&receiver_kind) {
            if method.name.starts_with(prefix) && seen.insert(method.name.to_string()) {
                items.push(method_completion_item(method));
            }
        }
        // Walk every impl block in this file looking for methods whose
        // receiver type spelling matches, and the struct's own fields.
        if let Some(receiver_type) = receiver_kind.type_name() {
            for field in user_fields_for(doc, receiver_type) {
                if field.name.starts_with(prefix) && seen.insert(field.name.clone()) {
                    items.push(user_field_completion_item(&field));
                }
            }
            for method in user_methods_for(doc, receiver_type) {
                if method.name.starts_with(prefix) && seen.insert(method.name.clone()) {
                    items.push(user_method_completion_item(&method));
                }
            }
        }
        // Last-ditch fallback: when we can't resolve the receiver type,
        // surface every known builtin method whose name matches the
        // prefix. Keeps `vec.p` useful even mid-edit when the receiver
        // expression doesn't typecheck.
        if items.is_empty() && !prefix.is_empty() {
            for method in ALL_BUILTIN_METHODS {
                if method.name.starts_with(prefix) && seen.insert(method.name.to_string()) {
                    items.push(method_completion_item(method));
                }
            }
        }
    }

    /// Type-qualified completions. Looks up the qualifier's last segment
    /// against in-file enums (variants) and impl blocks (associated fns).
    fn type_qualified_completions(
        &self,
        doc: &DocumentAnalysis,
        cursor: &CursorContext<'_>,
        prefix: &str,
        items: &mut Vec<Value>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        let Some(last) = cursor.qualifier.last().copied() else {
            return;
        };
        for assoc in user_associated_items(doc, last) {
            if assoc.name.starts_with(prefix) && seen.insert(assoc.name.clone()) {
                items.push(user_method_completion_item(&assoc));
            }
        }
    }

    /// Suggests `use` imports for unqualified names that don't already
    /// resolve in scope. Each completion item carries
    /// `additionalTextEdits` inserting the matching `use` statement.
    fn auto_import_completions(
        &self,
        doc: &DocumentAnalysis,
        prefix: &str,
        items: &mut Vec<Value>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        let already_imported = collect_existing_imports(doc);
        for path in self.stdlib.fuzzy_paths_for_prefix(prefix) {
            // The user already typed an exact-name match: only suggest
            // when the bare-name space doesn't already cover this name.
            let leaf = path.rsplit("::").next().unwrap_or("");
            if leaf.is_empty() || !leaf.starts_with(prefix) {
                continue;
            }
            if already_imported.iter().any(|imp| imp == &path) {
                continue;
            }
            if !seen.insert(format!("{leaf}::__import__::{path}")) {
                continue;
            }
            items.push(import_completion_item(doc, leaf, &path));
        }
    }

    fn locate<'s>(&'s self, params: &Value) -> Option<(&'s DocumentAnalysis, u32)> {
        let uri = field_str(field(params, "textDocument"), "uri")?;
        let doc = self.documents.get(uri)?;
        let position = field(params, "position");
        let line = field_u32(position, "line")?;
        let column = field_u32(position, "character")?;
        let offset = doc.position_to_offset(line, column)?;
        Some((doc, offset))
    }

    fn cursor(&self, doc: &DocumentAnalysis, offset: u32) -> Option<Locate> {
        let mut loc = locate(&doc.sf, doc.source(), offset)?;
        attach_resolution(&mut loc, &doc.resolutions);
        Some(loc)
    }

    fn references(&self, params: &Value) -> Value {
        let Some((doc, offset)) = self.locate(params) else {
            return Value::Array(Vec::new());
        };
        Value::Array(self.workspace_reference_locations(doc, offset))
    }

    fn document_highlight(&self, params: &Value) -> Value {
        let Some((doc, offset)) = self.locate(params) else {
            return Value::Array(Vec::new());
        };
        let spans = self.references_spans(doc, offset);
        let highlights: Vec<Value> = spans
            .into_iter()
            .map(|span| {
                let mut entry = BTreeMap::new();
                entry.insert("range".to_string(), span_to_range(doc, span));
                // Kind 1 = Text per LSP. Read/write tagging would
                // require dataflow we don't track yet.
                entry.insert("kind".to_string(), Value::Number(1.0));
                Value::Object(entry)
            })
            .collect();
        Value::Array(highlights)
    }

    /// Resolves a cursor position to a workspace-wide [`SymbolKey`] when
    /// possible. Local bindings, unresolved identifiers, and
    /// out-of-vocabulary names return `None`.
    pub(crate) fn workspace_key_at(
        &self,
        doc: &DocumentAnalysis,
        offset: u32,
    ) -> Option<SymbolKey> {
        if let Some(loc) = self.cursor(doc, offset) {
            if let Some(key) = symbol_key_for_locate(doc, &loc) {
                return Some(key);
            }
        }
        // Cursor sits on a declaration name (the locator's visit
        // surface doesn't traverse fn / struct / enum names because
        // they're not refutable patterns). Use the word at the
        // cursor + the DefinitionIndex to bridge.
        //
        // The DefinitionIndex's `name_span` is unreliable for items
        // that start with `pub` (it stretches from the item span
        // start, missing the visibility prefix), so the
        // word-equality check is the source of truth here.
        let word = doc.word_at(offset)?;
        for (_, info) in doc.index.def_iter() {
            if info.name != word {
                continue;
            }
            let bucket = match info.kind {
                DefKind::Fn
                | DefKind::Struct
                | DefKind::Enum
                | DefKind::Trait
                | DefKind::Const
                | DefKind::Static
                | DefKind::TypeAlias => SymbolBucket::Item,
                DefKind::Variant => SymbolBucket::Variant,
                DefKind::Mod | DefKind::TypeParam => continue,
            };
            return Some(SymbolKey {
                bucket,
                name: info.name.clone(),
            });
        }
        None
    }

    /// Returns every cross-file occurrence of the symbol under the
    /// cursor, as a `Vec<Value>` of LSP `Location` objects (each
    /// carrying its own `uri`). Falls back to the document-local
    /// text-fallback only when the cursor sits inside a string
    /// literal or doctest fence.
    fn workspace_reference_locations(&self, doc: &DocumentAnalysis, offset: u32) -> Vec<Value> {
        if let Some(key) = self.workspace_key_at(doc, offset) {
            let by_uri = self.workspace.occurrences_of(&key);
            if !by_uri.is_empty() {
                return cross_file_locations(self, by_uri);
            }
        }
        // Single-file semantic refs (locals, fields/methods without
        // resolved receivers).
        let spans = self.references_spans(doc, offset);
        if !spans.is_empty() {
            return spans.into_iter().map(|s| location(doc, s)).collect();
        }
        // Semantic resolution failed everywhere. Only honour the
        // text-based whole-word fallback when the cursor sits inside
        // a string literal or fenced doctest.
        if cursor_in_string_or_doctest(doc.source(), offset) {
            if let Some(word) = doc.word_at(offset) {
                return doc
                    .find_references(word)
                    .into_iter()
                    .map(|s| location(doc, s))
                    .collect();
            }
        }
        Vec::new()
    }

    fn references_spans(&self, doc: &DocumentAnalysis, offset: u32) -> Vec<Span> {
        let Some(loc) = self.cursor(doc, offset) else {
            return Vec::new();
        };
        let target = match &loc {
            Locate::PathExpr {
                resolution: Some(resolution),
                ..
            }
            | Locate::TypePath {
                resolution: Some(resolution),
                ..
            } => Some(*resolution),
            Locate::Binding { pattern_id, .. } => Some(Resolution::Local(*pattern_id)),
            _ => None,
        };
        let Some(target) = target else {
            return Vec::new();
        };
        let mut spans: Vec<Span> = Vec::new();
        if let Resolution::Local(node) = target {
            if let Some(info) = doc.index.local(node) {
                spans.push(info.name_span);
            }
        } else if let Resolution::Def { def, .. } = target {
            if let Some(info) = doc.index.def(def) {
                spans.push(info.name_span);
            }
        }
        for occurrence in doc.index.occurrences() {
            if occurrence.resolution == Some(target) {
                spans.push(occurrence.span);
            }
        }
        spans.sort_by_key(|s| (s.start, s.end));
        spans.dedup_by_key(|s| (s.start, s.end));
        spans
    }

    fn prepare_rename(&self, params: &Value) -> Value {
        let Some((doc, offset)) = self.locate(params) else {
            return Value::Null;
        };
        if let Some(loc) = self.cursor(doc, offset) {
            let span = locate_span(&loc);
            let name = locate_name(&loc);
            let mut result = BTreeMap::new();
            result.insert("range".to_string(), span_to_range(doc, span));
            result.insert("placeholder".to_string(), Value::String(name));
            return Value::Object(result);
        }
        let Some(word) = doc.word_at(offset) else {
            return Value::Null;
        };
        let bytes = doc.source().as_bytes();
        let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let mut start = offset as usize;
        while start > 0 && is_word(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = offset as usize;
        while end < bytes.len() && is_word(bytes[end]) {
            end += 1;
        }
        let span = Span::new(doc.file, start as u32, end as u32);
        let mut result = BTreeMap::new();
        result.insert("range".to_string(), span_to_range(doc, span));
        result.insert("placeholder".to_string(), Value::String(word.to_string()));
        Value::Object(result)
    }

    fn rename(&self, params: &Value) -> Value {
        let Some((doc, offset)) = self.locate(params) else {
            return Value::Null;
        };
        let Some(new_name) = field_str(params, "newName") else {
            return Value::Null;
        };
        if !is_valid_identifier(new_name) {
            return Value::Null;
        }
        self.build_rename_edit(doc, offset, new_name)
            .unwrap_or(Value::Null)
    }

    /// Computes the `WorkspaceEdit` for renaming the symbol under
    /// `offset` to `new_name`. Returns `None` when no semantic target
    /// is available at that position.
    pub(crate) fn build_rename_edit(
        &self,
        doc: &DocumentAnalysis,
        offset: u32,
        new_name: &str,
    ) -> Option<Value> {
        let mut changes: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        if let Some(key) = self.workspace_key_at(doc, offset) {
            // Workspace-wide: every doc carrying an occurrence of this
            // symbol contributes one edit per span.
            for (uri, occurrences) in self.workspace.occurrences_of(&key) {
                let Some(target_doc) = self.documents.get(&uri) else {
                    continue;
                };
                let edits: Vec<Value> = occurrences
                    .into_iter()
                    .map(|occ: SymbolOccurrence| build_text_edit(target_doc, occ.span, new_name))
                    .collect();
                if !edits.is_empty() {
                    changes.entry(uri.clone()).or_default().extend(edits);
                }
            }
            // `use util::foo` re-exports - rewrite only the matching
            // leaf identifier in every importing file. Only items
            // (top-level fn/struct/...) participate; fields, methods,
            // and variants aren't imported via `use`.
            if matches!(key.bucket, SymbolBucket::Item) {
                for (uri, use_occs) in self.workspace.use_occurrences_of(key.leaf()) {
                    let Some(target_doc) = self.documents.get(&uri) else {
                        continue;
                    };
                    let leaf_edits: Vec<Value> = use_occs
                        .into_iter()
                        .map(|UseOccurrence { span, .. }| {
                            build_text_edit(target_doc, span, new_name)
                        })
                        .collect();
                    if !leaf_edits.is_empty() {
                        changes.entry(uri.clone()).or_default().extend(leaf_edits);
                    }
                }
            }
        }
        // File-local fallback for locals and other symbols that don't
        // enter the workspace index. The file-local spans are always
        // safe to rewrite even when no workspace key resolved.
        let local_spans = self.references_spans(doc, offset);
        if !local_spans.is_empty() {
            let edits: Vec<Value> = local_spans
                .into_iter()
                .map(|span| build_text_edit(doc, span, new_name))
                .collect();
            let existing = changes.entry(doc.uri.clone()).or_default();
            for edit in edits {
                if !existing.iter().any(|e| edits_overlap(e, &edit)) {
                    existing.push(edit);
                }
            }
        }
        if changes.is_empty() {
            return None;
        }
        let changes_obj: BTreeMap<String, Value> = changes
            .into_iter()
            .map(|(uri, edits)| (uri, Value::Array(edits)))
            .collect();
        let mut workspace_edit = BTreeMap::new();
        workspace_edit.insert("changes".to_string(), Value::Object(changes_obj));
        Some(Value::Object(workspace_edit))
    }

    fn inlay_hints(&self, params: &Value) -> Value {
        let Some(uri) = field_str(field(params, "textDocument"), "uri") else {
            return Value::Array(Vec::new());
        };
        let Some(doc) = self.documents.get(uri) else {
            return Value::Array(Vec::new());
        };
        let range = field(params, "range");
        let byte_range = if matches!(range, Value::Object(_)) {
            let start = field(range, "start");
            let end = field(range, "end");
            let start_offset = field_u32(start, "line").and_then(|line| {
                let column = field_u32(start, "character").unwrap_or(0);
                doc.position_to_offset(line, column)
            });
            let end_offset = field_u32(end, "line").and_then(|line| {
                let column = field_u32(end, "character").unwrap_or(0);
                doc.position_to_offset(line, column)
            });
            match (start_offset, end_offset) {
                (Some(a), Some(b)) if a <= b => Some((a, b)),
                _ => None,
            }
        } else {
            None
        };
        let hints = collect_inlays(doc, byte_range);
        Value::Array(hints.into_iter().map(inlay_to_lsp).collect())
    }

    fn document_symbols(&self, params: &Value) -> Value {
        let Some(uri) = field_str(field(params, "textDocument"), "uri") else {
            return Value::Array(Vec::new());
        };
        let Some(doc) = self.documents.get(uri) else {
            return Value::Array(Vec::new());
        };
        document_symbols(doc)
    }

    fn workspace_symbols(&self, params: &Value) -> Value {
        let query = field_str(params, "query").unwrap_or("");
        let docs: Vec<&DocumentAnalysis> = self.documents.values().collect();
        workspace_symbols(&docs, query)
    }

    fn folding_ranges(&self, params: &Value) -> Value {
        let Some(uri) = field_str(field(params, "textDocument"), "uri") else {
            return Value::Array(Vec::new());
        };
        let Some(doc) = self.documents.get(uri) else {
            return Value::Array(Vec::new());
        };
        folding_ranges(doc)
    }

    fn signature_help(&self, params: &Value) -> Value {
        let Some((doc, offset)) = self.locate(params) else {
            return Value::Null;
        };
        let Some((callee_name, active_param)) = enclosing_call(doc.source(), offset) else {
            return Value::Null;
        };
        for (_, info) in doc.index_pairs() {
            if info.name == callee_name && matches!(info.kind, DefKind::Fn) {
                return signature_help_for(info, active_param);
            }
        }
        Value::Null
    }

    fn formatting(&self, params: &Value) -> Value {
        let Some(uri) = field_str(field(params, "textDocument"), "uri") else {
            return Value::Array(Vec::new());
        };
        let Some(doc) = self.documents.get(uri) else {
            return Value::Array(Vec::new());
        };
        // The stored source is the AUGMENTED program (user text +
        // synthesized autoderive tail). Format ONLY the user prefix,
        // through the same token-stream formatter `gos fmt` uses -
        // rendering `doc.sf` would print the synthesized items and
        // the parse-time desugars straight into the editor's buffer.
        let user_source = doc.user_source();
        let Ok(formatted) = gossamer_parse::format_source(user_source, doc.file) else {
            // Unparseable, or failed the formatter's token-equivalence
            // self-check: leave the buffer untouched.
            return Value::Array(Vec::new());
        };
        let formatted = if formatted.ends_with('\n') {
            formatted
        } else {
            format!("{formatted}\n")
        };
        if formatted == user_source {
            return Value::Array(Vec::new());
        }
        // Replace the user text exactly: the range end is the end of
        // the editor's buffer, never a position inside the synthesized
        // tail the client has no lines for.
        let (end_line, end_col) = doc.offset_to_position(doc.user_len);
        let mut start = BTreeMap::new();
        start.insert("line".to_string(), Value::Number(0.0));
        start.insert("character".to_string(), Value::Number(0.0));
        let mut end = BTreeMap::new();
        end.insert("line".to_string(), Value::Number(f64::from(end_line)));
        end.insert("character".to_string(), Value::Number(f64::from(end_col)));
        let mut range = BTreeMap::new();
        range.insert("start".to_string(), Value::Object(start));
        range.insert("end".to_string(), Value::Object(end));
        let mut edit = BTreeMap::new();
        edit.insert("range".to_string(), Value::Object(range));
        edit.insert("newText".to_string(), Value::String(formatted));
        Value::Array(vec![Value::Object(edit)])
    }

    fn semantic_tokens(&self, params: &Value) -> Value {
        let Some(uri) = field_str(field(params, "textDocument"), "uri") else {
            return empty_semantic_tokens();
        };
        let Some(doc) = self.documents.get(uri) else {
            return empty_semantic_tokens();
        };
        let data = full_tokens(doc);
        let mut out = BTreeMap::new();
        out.insert(
            "data".to_string(),
            Value::Array(
                data.into_iter()
                    .map(|n| Value::Number(f64::from(n)))
                    .collect(),
            ),
        );
        Value::Object(out)
    }
}
