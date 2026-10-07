#[cfg(test)]
mod tests {
    use super::*;

    fn position(line: u32, character: u32) -> Value {
        let mut p = BTreeMap::new();
        p.insert("line".to_string(), Value::Number(f64::from(line)));
        p.insert("character".to_string(), Value::Number(f64::from(character)));
        Value::Object(p)
    }

    fn locate_params(uri: &str, line: u32, character: u32) -> Value {
        let mut text_doc = BTreeMap::new();
        text_doc.insert("uri".to_string(), Value::String(uri.to_string()));
        let mut params = BTreeMap::new();
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        params.insert("position".to_string(), position(line, character));
        Value::Object(params)
    }

    fn document_params_value(uri: &str) -> Value {
        let mut text_doc = BTreeMap::new();
        text_doc.insert("uri".to_string(), Value::String(uri.to_string()));
        let mut params = BTreeMap::new();
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        Value::Object(params)
    }

    fn apply_substr_change(
        state: &mut ServerState,
        uri: &str,
        current: &mut String,
        needle: &str,
        replacement: &str,
    ) {
        let start = current
            .find(needle)
            .unwrap_or_else(|| panic!("missing edit needle `{needle}` in:\n{current}"));
        apply_offset_change(state, uri, current, start, start + needle.len(), replacement);
    }

    fn apply_last_substr_change(
        state: &mut ServerState,
        uri: &str,
        current: &mut String,
        needle: &str,
        replacement: &str,
    ) {
        let start = current
            .rfind(needle)
            .unwrap_or_else(|| panic!("missing edit needle `{needle}` in:\n{current}"));
        apply_offset_change(state, uri, current, start, start + needle.len(), replacement);
    }

    fn apply_offset_change(
        state: &mut ServerState,
        uri: &str,
        current: &mut String,
        start: usize,
        end: usize,
        replacement: &str,
    ) {
        let range = range_from_offsets(current, start, end);
        let mut change = BTreeMap::new();
        change.insert("range".to_string(), range);
        change.insert("text".to_string(), Value::String(replacement.to_string()));
        state.apply_did_change(uri, &Value::Array(vec![Value::Object(change)]));
        current.replace_range(start..end, replacement);
    }

    fn range_from_offsets(source: &str, start: usize, end: usize) -> Value {
        let (start_line, start_char) = position_from_offset(source, start);
        let (end_line, end_char) = position_from_offset(source, end);
        let mut range = BTreeMap::new();
        range.insert("start".to_string(), position(start_line, start_char));
        range.insert("end".to_string(), position(end_line, end_char));
        Value::Object(range)
    }

    fn position_from_offset(source: &str, offset: usize) -> (u32, u32) {
        let prefix = &source[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
        let character = prefix[line_start..].encode_utf16().count() as u32;
        (line, character)
    }

    fn extract_labels(response: &Value) -> Vec<String> {
        let Value::Array(items) = response else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| {
                let Value::Object(fields) = item else {
                    return None;
                };
                let Value::String(label) = fields.get("label")? else {
                    return None;
                };
                Some(label.clone())
            })
            .collect()
    }

    #[test]
    fn initialize_result_advertises_full_capability_set() {
        let v = initialize_result();
        let Value::Object(top) = v else {
            panic!("not object")
        };
        let Value::Object(caps) = top.get("capabilities").unwrap() else {
            panic!("no caps");
        };
        for key in [
            "completionProvider",
            "hoverProvider",
            "definitionProvider",
            "typeDefinitionProvider",
            "referencesProvider",
            "documentHighlightProvider",
            "renameProvider",
            "inlayHintProvider",
            "documentSymbolProvider",
            "workspaceSymbolProvider",
            "foldingRangeProvider",
            "documentFormattingProvider",
            "signatureHelpProvider",
            "semanticTokensProvider",
        ] {
            assert!(caps.contains_key(key), "missing capability: {key}");
        }
        let Value::Object(code_actions) = caps.get("codeActionProvider").unwrap() else {
            panic!("codeActionProvider must be an object");
        };
        let Some(Value::Array(kinds)) = code_actions.get("codeActionKinds") else {
            panic!("missing code action kinds");
        };
        assert!(kinds.contains(&Value::String("quickfix".to_string())));
        assert!(kinds.contains(&Value::String("source.fixAll.gossamer".to_string())));
    }

    #[test]
    fn file_uri_percent_decodes_workspace_paths() {
        assert_eq!(
            file_uri_to_path("file:///tmp/a%20project/%CF%80.gos").as_deref(),
            Some("/tmp/a project/π.gos")
        );
        assert_eq!(file_uri_to_path("file:///tmp/bad%2"), None);
        assert_eq!(file_uri_to_path("https://example.com/a.gos"), None);
    }

    #[test]
    fn did_close_notification_clears_diagnostics() {
        let notification = empty_diagnostics_notification("file:///closed.gos");
        assert_eq!(
            field_str(&notification, "method"),
            Some("textDocument/publishDiagnostics")
        );
        let params = field(&notification, "params");
        assert_eq!(field_str(params, "uri"), Some("file:///closed.gos"));
        assert!(matches!(field(params, "diagnostics"), Value::Array(items) if items.is_empty()));
    }

    fn inlay_params(uri: &str) -> Value {
        let mut text_doc = BTreeMap::new();
        text_doc.insert("uri".to_string(), Value::String(uri.to_string()));
        let mut params = BTreeMap::new();
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        Value::Object(params)
    }

    fn inlay_hint_lines(response: &Value) -> Vec<u32> {
        let Value::Array(items) = response else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| {
                let Value::Object(fields) = item else {
                    return None;
                };
                let Value::Object(pos) = fields.get("position")? else {
                    return None;
                };
                let Value::Number(line) = pos.get("line")? else {
                    return None;
                };
                Some(*line as u32)
            })
            .collect()
    }

    /// Writes a package whose `src/engine/bind.gos` reaches a sibling
    /// `src/codec` directory module through `crate::`, and answers its root.
    fn nested_module_project(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("gos-lsp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src").join("codec")).unwrap();
        std::fs::create_dir_all(root.join("src").join("engine")).unwrap();
        std::fs::write(
            root.join("project.toml"),
            "[project]\nid = \"example.com/db\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src").join("main.gos"), "fn main() { }\n").unwrap();
        std::fs::write(
            root.join("src").join("codec").join("mod.gos"),
            "pub fn tag() -> i64 { 7 }\n",
        )
        .unwrap();
        std::fs::write(root.join("src").join("engine").join("mod.gos"), "\n").unwrap();
        root
    }

    fn file_uri(path: &std::path::Path) -> String {
        format!("file://{}", path.display())
    }

    fn diagnostic_messages(state: &ServerState, uri: &str) -> Vec<String> {
        let doc = state.documents.get(uri).expect("document");
        doc.diagnostics
            .iter()
            .map(|diag| diag.title.clone())
            .collect()
    }

    #[test]
    fn a_nested_module_resolves_crate_paths_through_its_package() {
        let root = nested_module_project("crate-path");
        let bind = root.join("src").join("engine").join("bind.gos");
        let source = "use crate::codec\n\npub fn one() -> i64 { codec::tag() }\n";
        std::fs::write(&bind, source).unwrap();
        let uri = file_uri(&bind);
        let mut state = ServerState::new();
        state.update(&uri, source);
        assert_eq!(
            diagnostic_messages(&state, &uri),
            Vec::<String>::new(),
            "a module of the package must resolve `crate::` against that package"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_nested_modules_diagnostic_is_reported_at_its_own_line() {
        let root = nested_module_project("window");
        let bind = root.join("src").join("engine").join("bind.gos");
        let source = "use crate::codec\n\npub fn one() -> i64 { codec::missing() }\n";
        std::fs::write(&bind, source).unwrap();
        let uri = file_uri(&bind);
        let mut state = ServerState::new();
        state.update(&uri, source);
        let doc = state.documents.get(&uri).expect("document");
        let diag = doc.diagnostics.first().expect("a diagnostic");
        let span = diag
            .labels
            .iter()
            .find(|label| label.primary)
            .or_else(|| diag.labels.first())
            .expect("a label")
            .location
            .span;
        let (line, _) = doc.offset_to_position(span.start);
        assert_eq!(line, 2, "the diagnostic must land on the line the user wrote");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_nested_modules_outline_holds_only_its_own_items() {
        let root = nested_module_project("outline");
        let bind = root.join("src").join("engine").join("bind.gos");
        let source = "pub fn one() -> i64 { 1 }\n";
        std::fs::write(&bind, source).unwrap();
        let uri = file_uri(&bind);
        let mut state = ServerState::new();
        state.update(&uri, source);
        let doc = state.documents.get(&uri).expect("document");
        let names: Vec<String> = doc
            .document_items()
            .iter()
            .filter_map(|item| match &item.kind {
                gossamer_ast::ItemKind::Fn(decl) => Some(decl.name.name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["one".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn frame(body: &str) -> String {
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    #[test]
    fn a_burst_of_edits_is_analysed_once() {
        let uri = "file:///burst.gos";
        let mut input = frame(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{uri}","languageId":"gossamer","version":1,"text":"fn main() {{ }}\n"}}}}}}"#
        ));
        for (version, body) in ["fn main() { let a = 1 }", "fn main() { let ab = 1 }", "fn main() { let x: i64 = \\\"s\\\" }"].iter().enumerate() {
            input.push_str(&frame(&format!(
                r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{uri}","version":{}}},"contentChanges":[{{"text":"{body}\n"}}]}}}}"#,
                version + 2
            )));
        }
        let mut output: Vec<u8> = Vec::new();
        run(input.as_bytes(), &mut output).expect("serve the burst");
        let output = String::from_utf8(output).expect("utf-8 output");
        let publishes: Vec<&str> = output
            .split("Content-Length:")
            .filter(|frame| frame.contains("publishDiagnostics"))
            .collect();
        assert_eq!(publishes.len(), 2, "one for the open, one for the burst: {output}");
        assert!(publishes[1].contains("GT0001"), "the burst's last text was analysed: {output}");
    }

    fn codes(state: &ServerState, uri: &str) -> Vec<String> {
        state.documents[uri]
            .diagnostics
            .iter()
            .map(|diag| diag.code.as_str().to_string())
            .filter(|code| !code.starts_with("GL"))
            .collect()
    }

    #[test]
    fn unsaved_buffers_of_one_package_are_analysed_together() {
        let root = nested_module_project("unsaved");
        let helper = root.join("src").join("helper.gos");
        let main = root.join("src").join("main.gos");
        std::fs::write(&helper, "pub fn one() -> i64 { 1 }\n").unwrap();
        std::fs::write(&main, "fn main() { println(\"{}\", helper::one()) }\n").unwrap();
        let (helper_uri, main_uri) = (file_uri(&helper), file_uri(&main));
        let mut state = ServerState::new();
        state.update(&helper_uri, "pub fn two() -> i64 { 2 }\n");
        let affected = state.update(&main_uri, "fn main() { println(\"{}\", helper::two()) }\n");
        assert!(codes(&state, &main_uri).is_empty(), "{:?}", codes(&state, &main_uri));
        assert!(affected.contains(&helper_uri), "the helper's view must be refreshed too");

        // Editing only the helper back refreshes the dependent document,
        // which now names a function the package no longer has.
        let affected = state.update(&helper_uri, "pub fn one() -> i64 { 1 }\n");
        assert!(affected.contains(&main_uri));
        assert_eq!(codes(&state, &main_uri), ["GR0001"]);

        // Alternating edits leave every document on one analysis of the
        // package, not one snapshot per document.
        state.update(&main_uri, "fn main() { println(\"{}\", helper::one()) }\n");
        assert!(codes(&state, &main_uri).is_empty());
        assert!(std::rc::Rc::ptr_eq(
            &state.documents[&main_uri].unit,
            &state.documents[&helper_uri].unit
        ));
        state.units.retain_live();
        assert_eq!(state.units.len(), 1, "a stale snapshot of the package survived");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_document_of_one_package_shares_its_analysis() {
        let root = nested_module_project("shared");
        let bind = root.join("src").join("engine").join("bind.gos");
        std::fs::write(&bind, "pub fn one() -> i64 { 1 }\n").unwrap();
        let main = root.join("src").join("main.gos");
        let mut state = ServerState::new();
        state.update(&file_uri(&main), &std::fs::read_to_string(&main).unwrap());
        state.update(&file_uri(&bind), &std::fs::read_to_string(&bind).unwrap());
        let a = state.documents.get(&file_uri(&main)).expect("main");
        let b = state.documents.get(&file_uri(&bind)).expect("bind");
        assert!(
            std::rc::Rc::ptr_eq(&a.unit, &b.unit),
            "two files of one package analysed the same unit twice"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_comptime_read_anchors_at_the_packages_entry() {
        // The unit is the package, so a relative read inside a comptime
        // region resolves against the entry's directory whichever file of
        // the package the editor has open.
        let root = nested_module_project("comptime-anchor");
        std::fs::write(root.join("src").join("asset.txt"), "embedded\n").unwrap();
        std::fs::write(
            root.join("src").join("main.gos"),
            "use std::fs\n\nfn main() {\n    println(\"{}\", comptime { fs::read_to_string(\"asset.txt\").unwrap() })\n}\n",
        )
        .unwrap();
        let bind = root.join("src").join("engine").join("bind.gos");
        let source = "pub fn one() -> i64 { 1 }\n";
        std::fs::write(&bind, source).unwrap();
        let main = root.join("src").join("main.gos");
        let main_uri = file_uri(&main);
        // The nested file is opened first, so it is the document that
        // triggers the package's analysis. A unit is named and cached by
        // its entry, so the fold reads `src/` whichever document assembled
        // it - anchoring on the open document instead would look for the
        // asset in `src/engine/` and report a failure the entry did not
        // cause.
        let mut state = ServerState::new();
        state.update(&file_uri(&bind), source);
        state.update(&main_uri, &std::fs::read_to_string(&main).unwrap());
        assert_eq!(
            diagnostic_messages(&state, &main_uri),
            Vec::<String>::new(),
            "the fold must read the entry's own directory"
        );
        assert!(
            std::rc::Rc::ptr_eq(
                &state.documents.get(&main_uri).expect("main").unit,
                &state.documents.get(&file_uri(&bind)).expect("bind").unit,
            ),
            "the entry and its module must share the unit the fold anchored"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn inlay_hints_emits_inferred_let_type() {
        let mut state = ServerState::new();
        state.update("file:///inlay.gos", "fn main() {\n    let n = 42\n}\n");
        let response = state.inlay_hints(&inlay_params("file:///inlay.gos"));
        let labels = extract_labels(&response);
        assert!(
            labels.iter().any(|l| l == ": i64"),
            "expected `: i64` hint; got {labels:?}"
        );
    }

    #[test]
    fn inlay_hint_top_level_let_position_is_on_correct_line() {
        // An entry file's top-level `let` becomes part of the synthesized
        // `fn main`; the inlay hint anchor must stay on the source line
        // where the binding appears, not collapse to line 0.
        let mut state = ServerState::new();
        // Full hanoi.gos content: let n is on line 12 (0-indexed).
        let src = concat!(
            "use std::{env, strconv}\n",
            "\n",
            "fn hanoi(n: i64, src: String, dst: String, aux: String) {\n",
            "    if n == 1 {\n",
            "        println(\"Move disk 1 from {src} to {dst}\")\n",
            "    } else {\n",
            "        hanoi(n - 1, src, aux, dst)\n",
            "        println(\"Move disk {n} from {src} to {dst}\")\n",
            "        hanoi(n - 1, aux, dst, src)\n",
            "    }\n",
            "}\n",
            "\n",
            "let n = strconv::parse_i64(env::args().first().unwrap_or(\"3\")).unwrap_or(3)\n",
            "hanoi(n, \"A\", \"C\", \"B\")\n",
        );
        state.update("file:///hanoi.gos", src);
        let response = state.inlay_hints(&inlay_params("file:///hanoi.gos"));
        let lines = inlay_hint_lines(&response);
        // `let n` is on line 12 (0-indexed); the hint must not be on line 0.
        assert!(
            !lines.contains(&0),
            "inlay hint must not be on line 0 (wrong position); got lines {lines:?}"
        );
        if !lines.is_empty() {
            assert!(
                lines.contains(&12),
                "expected inlay hint on line 12; got lines {lines:?}"
            );
        }
    }

    #[test]
    fn references_returns_every_whole_word_occurrence() {
        let mut state = ServerState::new();
        state.update(
            "file:///r.gos",
            "fn greet() { greet() }\nfn other() { greet() }\n",
        );
        let response = state.references(&locate_params("file:///r.gos", 0, 4));
        let Value::Array(items) = response else {
            panic!("not array");
        };
        assert!(!items.is_empty(), "expected at least one reference");
    }

    #[test]
    fn prepare_rename_returns_span_and_placeholder() {
        let mut state = ServerState::new();
        state.update("file:///p.gos", "fn greet() { }\n");
        let response = state.prepare_rename(&locate_params("file:///p.gos", 0, 4));
        let Value::Object(fields) = response else {
            panic!("not object");
        };
        let Value::String(placeholder) = fields.get("placeholder").unwrap() else {
            panic!("no placeholder");
        };
        assert_eq!(placeholder, "greet");
        assert!(fields.contains_key("range"));
    }

    #[test]
    fn rename_rejects_invalid_identifier_input() {
        let mut state = ServerState::new();
        state.update("file:///bad.gos", "fn greet() { }\n");
        let mut params = locate_params("file:///bad.gos", 0, 4);
        if let Value::Object(fields) = &mut params {
            fields.insert(
                "newName".to_string(),
                Value::String("not valid!".to_string()),
            );
        }
        let response = state.rename(&params);
        assert!(
            matches!(response, Value::Null),
            "expected null for invalid ident"
        );
    }

    #[test]
    fn completion_surfaces_top_level_functions_matching_prefix() {
        let mut state = ServerState::new();
        state.update(
            "file:///c.gos",
            "fn greet() { }\nfn greeter() { }\nfn main() { gr }\n",
        );
        let response = state.completion(&locate_params("file:///c.gos", 2, 13));
        let labels = extract_labels(&response);
        assert!(labels.iter().any(|l| l == "greet"), "labels: {labels:?}");
        assert!(labels.iter().any(|l| l == "greeter"), "labels: {labels:?}");
    }

    #[test]
    fn definition_finds_top_level_function_span() {
        let mut state = ServerState::new();
        state.update(
            "file:///d.gos",
            "fn helper() -> i64 { 1i64 }\nfn main() { helper() }\n",
        );
        let response = state.definition(&locate_params("file:///d.gos", 1, 13));
        let Value::Object(fields) = response else {
            panic!("expected Location object");
        };
        assert!(fields.contains_key("uri"));
        assert!(fields.contains_key("range"));
    }

    /// An editor's incremental changes race the server's view of the buffer,
    /// so a range can arrive inverted, past the end, or mid-character. None of
    /// those may bring the server down.
    #[test]
    fn incremental_changes_survive_out_of_range_edits() {
        let uri = "file:///edits.gos";
        let ranges = [
            (0, 0, 0, 0),
            (0, 0, 0, 5_000),
            (5_000, 0, 5_000, 3),
            (2, 9, 0, 0),
            (0, 3, 0, 1),
            (u32::MAX, u32::MAX, u32::MAX, u32::MAX),
            (1, 2, 1, 2),
        ];
        for (start_line, start_char, end_line, end_char) in ranges {
            let mut state = ServerState::new();
            state.update(uri, "fn café() { let x = 1 }\nstruct S { f: i64 }\n");
            let mut change = BTreeMap::new();
            change.insert(
                "range".to_string(),
                super::handle::range_value(start_line, start_char, end_line, end_char),
            );
            change.insert("text".to_string(), Value::String("ü".to_string()));
            state.apply_did_change(uri, &Value::Array(vec![Value::Object(change)]));
            // The document must still answer requests after a rejected edit.
            let _ = state.document_symbols(&document_params_value(uri));
            let _ = state.hover(&locate_params(uri, 0, 4));
            let _ = state.semantic_tokens(&document_params_value(uri));
        }
    }

    /// Every request an editor can send lands on a document the editor may
    /// have already changed, so a position or a recorded span can point past
    /// the text or into the middle of a character. None of that may bring the
    /// server down: a request it cannot answer answers with nothing.
    #[test]
    fn every_request_survives_adversarial_documents_and_positions() {
        let sources = [
            // Multi-byte characters, so a byte offset can land mid-character.
            "fn café() { let naïve = \"héllo wörld\"\n let e = naïve }\n",
            // Combining marks and astral-plane characters.
            "fn a() { let s = \"e\\u{301}x\" }\nstruct S { f: i64 }\n",
            "fn b() { let emoji = \"..\" }\n",
            // Truncated and malformed programs.
            "fn a&(",
            "struct \nenum|+ name\n type< \n}\n",
            "fn main() { let t = (1\n",
            "impl",
            "use ",
            "",
            "\n\n\n",
            // A lone carriage return and a stray NUL-adjacent control byte.
            "fn a() {\r\n let x = 1\r\n}\r\n",
            "\u{feff}fn bom() { }\n",
            // A byte-order mark in front of a program that does not parse.
            "\u{feff}fn a&(",
            // Deeply nested delimiters, and a very long single line.
            "fn main() { let x = ((((((((((((((((1)))))))))))))))) }\n",
            "fn main() { let s = \"................................................\" }\n",
            // Non-ASCII identifiers, which the language accepts.
            "fn ünïcödé() { let ß = 1\n let _ = ß }\n",
            // A trailing lone surrogate-ish escape and an unterminated string.
            "fn a() { let s = \"unterminated\n",
            "fn a() { /* unterminated comment\n",
        ];
        let positions = [
            (0, 0),
            (0, 3),
            (1, 1),
            (0, 10_000),
            (10_000, 0),
            (u32::MAX, u32::MAX),
            (2, 7),
        ];
        for source in sources {
            let uri = "file:///adversarial.gos";
            let mut state = ServerState::new();
            state.update(uri, source);
            // Whole-document requests.
            let _ = state.document_symbols(&document_params_value(uri));
            let _ = state.semantic_tokens(&document_params_value(uri));
            let _ = state.inlay_hints(&document_params_value(uri));
            let _ = state.folding_ranges(&document_params_value(uri));
            let _ = state.formatting(&document_params_value(uri));
            let _ = state.workspace_symbols(&super::handle::workspace_symbol_params("a"));
            let _ = state.workspace_symbols(&super::handle::workspace_symbol_params("café"));
            for (line, character) in positions {
                let at = locate_params(uri, line, character);
                let _ = state.hover(&at);
                let _ = state.definition(&at);
                let _ = state.references(&at);
                let _ = state.completion(&at);
                let _ = state.signature_help(&at);
                let _ = state.document_highlight(&at);
                let _ = state.rename(&super::handle::rename_params(uri, line, character, "renamed"));
                let _ = state.code_actions(&super::handle::code_action_params(
                    uri,
                    super::handle::range_value(line, character, line, character.saturating_add(3)),
                    vec![],
                ));
            }
        }
    }

    #[test]
    fn document_symbol_emits_top_level_items() {
        let mut state = ServerState::new();
        state.update(
            "file:///s.gos",
            "fn helper() { }\nstruct Point { x: i64, y: i64 }\n",
        );
        let mut params = BTreeMap::new();
        let mut text_doc = BTreeMap::new();
        text_doc.insert(
            "uri".to_string(),
            Value::String("file:///s.gos".to_string()),
        );
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        let response = state.document_symbols(&Value::Object(params));
        let Value::Array(items) = response else {
            panic!("not array");
        };
        let names: Vec<String> = items
            .iter()
            .filter_map(|item| match item {
                Value::Object(fields) => match fields.get("name") {
                    Some(Value::String(s)) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(names.contains(&"helper".to_string()), "names: {names:?}");
        assert!(names.contains(&"Point".to_string()), "names: {names:?}");
    }

    #[test]
    fn code_action_surfaces_did_you_mean_quickfix() {
        // GR0001 (unresolved name) attaches a Suggestion when
        // the resolver finds a candidate within edit distance 2.
        // textDocument/codeAction must surface that Suggestion
        // as a quickfix with a WorkspaceEdit replacing the
        // misspelled identifier with the candidate.
        let mut state = ServerState::new();
        // `helpre` is unresolved; `helper` is in scope; edit
        // distance is 1 so the resolver emits a suggestion.
        let source = "fn helper() { }\nfn main() { helpre() }\n";
        state.update("file:///x.gos", source);

        let mut text_doc = BTreeMap::new();
        text_doc.insert(
            "uri".to_string(),
            Value::String("file:///x.gos".to_string()),
        );

        // Range covering the misspelled call on line 1.
        let mut start = BTreeMap::new();
        start.insert("line".to_string(), Value::Number(1.0));
        start.insert("character".to_string(), Value::Number(12.0));
        let mut end = BTreeMap::new();
        end.insert("line".to_string(), Value::Number(1.0));
        end.insert("character".to_string(), Value::Number(20.0));
        let mut range = BTreeMap::new();
        range.insert("start".to_string(), Value::Object(start));
        range.insert("end".to_string(), Value::Object(end));

        let mut params = BTreeMap::new();
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        params.insert("range".to_string(), Value::Object(range));

        let response = state.code_actions(&Value::Object(params));
        let Value::Array(items) = response else {
            panic!("codeAction must return an Array, got {response:?}");
        };
        assert!(
            !items.is_empty(),
            "expected at least one quickfix for `helpre` → `helper`",
        );
        let first = &items[0];
        let Value::Object(action) = first else {
            panic!("action must be object");
        };
        assert!(matches!(action.get("kind"), Some(Value::String(s)) if s == "quickfix"));
        // Title carries the resolver's suggestion text.
        let Some(Value::String(title)) = action.get("title") else {
            panic!("action.title must be string");
        };
        assert!(
            title.contains("helper"),
            "title should name the candidate: {title}",
        );
        // The edit must include a `changes` map keyed by uri.
        let Some(Value::Object(edit)) = action.get("edit") else {
            panic!("edit must be object");
        };
        let Some(Value::Object(changes)) = edit.get("changes") else {
            panic!("edit.changes must be object");
        };
        let Some(Value::Array(edits)) = changes.get("file:///x.gos") else {
            panic!("changes must contain edits for the uri");
        };
        let Value::Object(first_edit) = &edits[0] else {
            panic!("edit must be object");
        };
        let Some(Value::String(new_text)) = first_edit.get("newText") else {
            panic!("edit.newText must be string");
        };
        assert_eq!(new_text, "helper");
        // The action must link back to the originating GR0001
        // diagnostic.
        let Some(Value::Array(diagnostics)) = action.get("diagnostics") else {
            panic!("action.diagnostics must be array");
        };
        let Value::Object(diag0) = &diagnostics[0] else {
            panic!("diagnostic must be object");
        };
        assert!(matches!(diag0.get("code"), Some(Value::String(s)) if s == "GR0001"));
    }

    #[test]
    fn code_action_returns_empty_when_no_suggestion_attached() {
        // A clean source has no diagnostics and therefore no
        // codeAction. The handler must return an empty array
        // (not Null) so clients can lift it directly.
        let mut state = ServerState::new();
        state.update("file:///ok.gos", "fn main() { println(\"hi\") }\n");

        let mut text_doc = BTreeMap::new();
        text_doc.insert(
            "uri".to_string(),
            Value::String("file:///ok.gos".to_string()),
        );
        let mut params = BTreeMap::new();
        params.insert("textDocument".to_string(), Value::Object(text_doc));

        let response = state.code_actions(&Value::Object(params));
        let Value::Array(items) = response else {
            panic!("codeAction must return Array, got {response:?}");
        };
        assert!(items.is_empty(), "expected no actions on clean source");
    }

    #[test]
    fn folding_ranges_include_each_top_level_item() {
        let mut state = ServerState::new();
        state.update(
            "file:///fr.gos",
            "fn one() {\n    let x = 1\n}\n\nfn two() {\n    let y = 2\n}\n",
        );
        let mut params = BTreeMap::new();
        let mut text_doc = BTreeMap::new();
        text_doc.insert(
            "uri".to_string(),
            Value::String("file:///fr.gos".to_string()),
        );
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        let response = state.folding_ranges(&Value::Object(params));
        let Value::Array(items) = response else {
            panic!("not array");
        };
        assert!(items.len() >= 2, "expected at least two folding ranges");
    }

    #[test]
    fn formatting_returns_no_edits_when_already_formatted() {
        let mut state = ServerState::new();
        state.update("file:///fmt.gos", "fn main() {\n    let x = 1\n}\n");
        let mut params = BTreeMap::new();
        let mut text_doc = BTreeMap::new();
        text_doc.insert(
            "uri".to_string(),
            Value::String("file:///fmt.gos".to_string()),
        );
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        // Whatever the formatter emits should be fine - we just need
        // the call to complete cleanly.
        let _ = state.formatting(&Value::Object(params));
    }

    #[test]
    fn formatting_never_leaks_the_synthesized_autoderive_tail() {
        // A struct makes the analysis pipeline append synthesized serde
        // functions to the stored source. A formatting edit must cover
        // and return ONLY the user text: the synthesized names must not
        // appear in `newText`, and the edit's end position must not
        // reach past the editor's buffer (the client would clamp it and
        // splice the expansion over all but the final line).
        let src = "struct Account { balance: i64, txns: i64 }\n\nfn main() {\n    let a = Account { balance: 1, txns: 2 }\n    println(\"{a.balance}\")\n}\n";
        let mut state = ServerState::new();
        state.update("file:///fmt_tail.gos", src);
        let mut params = BTreeMap::new();
        let mut text_doc = BTreeMap::new();
        text_doc.insert(
            "uri".to_string(),
            Value::String("file:///fmt_tail.gos".to_string()),
        );
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        let response = state.formatting(&Value::Object(params));
        let Value::Array(edits) = response else {
            panic!("formatting must return an edit array");
        };
        let user_lines = src.lines().count() as f64;
        for edit in &edits {
            let Value::Object(edit) = edit else {
                panic!("edit must be an object");
            };
            if let Some(Value::String(new_text)) = edit.get("newText") {
                assert!(
                    !new_text.contains("__gos_serde") && !new_text.contains("__concat"),
                    "formatting leaked synthesized items into the buffer:\n{new_text}"
                );
            }
            let end_line = edit
                .get("range")
                .and_then(|r| match r {
                    Value::Object(r) => r.get("end"),
                    _ => None,
                })
                .and_then(|e| match e {
                    Value::Object(e) => e.get("line"),
                    _ => None,
                })
                .and_then(|l| match l {
                    Value::Number(n) => Some(*n),
                    _ => None,
                })
                .expect("edit carries an end line");
            assert!(
                end_line <= user_lines,
                "edit end line {end_line} reaches past the {user_lines}-line editor buffer"
            );
        }
    }

    #[test]
    fn ranged_did_change_preserves_top_level_for_after_main_wrapper_removal() {
        const PREVIOUS: &str = r#"enum Expr { Lit(i64), Add(Expr, Expr), Sub(Expr, Expr), Mul(Expr, Expr), Div(Expr, Expr) }

struct Cur { s: Vec<char>, i: i64 }

fn run(src: String) -> i64 {
    0
}

fn main() {
    for src in ["2 + 3 * 4", "(2 + 3) * 4", "10 - 2 - 3", "2 * 3 + 4 * 5", "100 / 5 / 2"] {
        println("{} = {}", src, run(src))
    }
}
"#;
        const EXPECTED: &str = r#"enum Expr { Lit(i64), Add(Expr, Expr), Sub(Expr, Expr), Mul(Expr, Expr), Div(Expr, Expr) }

struct Cur { s: Vec<char>, i: i64 }

fn run(src: String) -> i64 {
    0
}

for src in ["2 + 3 * 4", "(2 + 3) * 4", "10 - 2 - 3", "2 * 3 + 4 * 5", "100 / 5 / 2"] {
    println("{} = {}", src, run(src))
}
"#;

        let uri = "file:///top-level-for.gos";
        let mut state = ServerState::new();
        state.update(uri, PREVIOUS);
        let mut current = PREVIOUS.to_string();
        apply_substr_change(&mut state, uri, &mut current, "fn main() {\n", "");
        apply_substr_change(&mut state, uri, &mut current, "    for src in ", "for src in ");
        apply_substr_change(&mut state, uri, &mut current, "        println", "    println");
        apply_last_substr_change(&mut state, uri, &mut current, "    }\n}\n", "}\n");

        let stored = state.documents.get(uri).expect("document").user_source();
        assert_eq!(stored, EXPECTED);

        let response = state.formatting(&document_params_value(uri));
        if let Value::Array(edits) = response {
            for edit in edits {
                if let Some(new_text) = field_str(&edit, "newText") {
                    assert!(
                        !new_text.ends_with("}\n}\n"),
                        "formatter edit reintroduced an extra closing brace:\n{new_text}"
                    );
                }
            }
        } else {
            panic!("formatting must return an edit array");
        }
    }

    #[test]
    fn signature_help_finds_the_called_function() {
        let mut state = ServerState::new();
        state.update(
            "file:///sh.gos",
            "fn add(x: i64, y: i64) -> i64 { x + y }\nfn main() { add(1,) }\n",
        );
        // Cursor sits right after the `,` inside `add(1, )`.
        let response = state.signature_help(&locate_params("file:///sh.gos", 1, 18));
        if let Value::Object(fields) = response {
            assert!(fields.contains_key("signatures"));
        }
    }

    fn complete_at(state: &mut ServerState, src_with_cursor: &str, uri: &str) -> Vec<String> {
        let cursor = src_with_cursor
            .find('|')
            .expect("cursor marker `|` missing");
        let cleaned: String =
            src_with_cursor[..cursor].to_string() + &src_with_cursor[cursor + 1..];
        state.update(uri, &cleaned);
        let doc = state.documents.get(uri).expect("document just added");
        let (line, col) = doc.offset_to_position(u32::try_from(cursor).unwrap());
        let response = state.completion(&locate_params(uri, line, col));
        extract_labels(&response)
    }

    fn complete_full(state: &mut ServerState, src_with_cursor: &str, uri: &str) -> Value {
        let cursor = src_with_cursor
            .find('|')
            .expect("cursor marker `|` missing");
        let cleaned: String =
            src_with_cursor[..cursor].to_string() + &src_with_cursor[cursor + 1..];
        state.update(uri, &cleaned);
        let doc = state.documents.get(uri).expect("document just added");
        let (line, col) = doc.offset_to_position(u32::try_from(cursor).unwrap());
        state.completion(&locate_params(uri, line, col))
    }

    #[test]
    fn module_qualified_completion_returns_module_members() {
        let mut state = ServerState::new();
        let labels = complete_at(
            &mut state,
            "use std::os\nfn main() { os::e| }\n",
            "file:///os.gos",
        );
        // `os::e|` should suggest the `exec` submodule.
        assert!(
            labels.iter().any(|l| l == "exec"),
            "expected `exec` submodule in {labels:?}"
        );
    }

    #[test]
    fn unimported_module_completion_adds_import_edit() {
        let mut state = ServerState::new();
        let response = complete_full(&mut state, "fn main() { env::a| }\n", "file:///env.gos");
        let Value::Array(items) = response else {
            panic!("expected completion array");
        };
        let args = items.into_iter().find(|item| {
            matches!(
                item,
                Value::Object(fields)
                    if fields.get("label") == Some(&Value::String("args".to_string()))
            )
        });
        let Some(Value::Object(fields)) = args else {
            panic!("expected `env::args` completion");
        };
        assert!(
            matches!(fields.get("additionalTextEdits"), Some(Value::Array(edits)) if !edits.is_empty()),
            "expected completion to insert `use std::env`"
        );
    }

    #[test]
    fn grouped_module_import_does_not_add_duplicate_import_edit() {
        let mut state = ServerState::new();
        let response = complete_full(
            &mut state,
            "use std::{env, fs}\nfn main() { env::a| }\n",
            "file:///grouped.gos",
        );
        let Value::Array(items) = response else {
            panic!("expected completion array");
        };
        let args = items.into_iter().find(|item| {
            matches!(
                item,
                Value::Object(fields)
                    if fields.get("label") == Some(&Value::String("args".to_string()))
            )
        });
        let Some(Value::Object(fields)) = args else {
            panic!("expected `env::args` completion");
        };
        assert!(
            !fields.contains_key("additionalTextEdits"),
            "grouped import must suppress a duplicate import edit"
        );
    }

    #[test]
    fn nested_module_qualifier_resolves() {
        let mut state = ServerState::new();
        let labels = complete_at(
            &mut state,
            "use std::os\nfn main() { os::e| }\n",
            "file:///os2.gos",
        );
        // std::os::exec is a known submodule.
        assert!(
            labels.iter().any(|l| l == "exec"),
            "expected `exec` in labels {labels:?}"
        );
    }

    #[test]
    fn unknown_qualifier_returns_no_member_match() {
        let mut state = ServerState::new();
        let labels = complete_at(&mut state, "fn main() { xyzzy::p| }\n", "file:///x.gos");
        // Unknown qualifier short-circuits - should produce no matches.
        assert!(
            labels.iter().all(|l| l != "println"),
            "did not expect bare-prefix items in qualifier completion: {labels:?}"
        );
    }

    #[test]
    fn use_statement_completion_lists_modules() {
        let mut state = ServerState::new();
        let labels = complete_at(&mut state, "use std::|\n", "file:///use.gos");
        assert!(
            labels.iter().any(|l| l == "fmt"),
            "expected `fmt` in {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "os"),
            "expected `os` in {labels:?}"
        );
    }

    #[test]
    fn vec_dot_completes_to_vec_methods() {
        let mut state = ServerState::new();
        let labels = complete_at(
            &mut state,
            "fn main() {\n    let mut v: Vec<i64> = Vec::new()\n    v.p|\n}\n",
            "file:///vec.gos",
        );
        assert!(
            labels.iter().any(|l| l == "push"),
            "expected `push` in {labels:?}"
        );
    }

    #[test]
    fn string_method_completion_includes_to_uppercase() {
        let mut state = ServerState::new();
        let labels = complete_at(&mut state, "fn main() { \"hi\".to_u| }\n", "file:///s.gos");
        assert!(
            labels.iter().any(|l| l == "to_uppercase"),
            "expected `to_uppercase` in {labels:?}"
        );
    }

    #[test]
    fn user_type_qualified_completion_returns_associated_fns() {
        let mut state = ServerState::new();
        let src = r"struct Foo {}
impl Foo {
    fn new() -> Foo { Foo {} }
    fn make_default() -> Foo { Foo {} }
}
fn main() { Foo::n| }
";
        let labels = complete_at(&mut state, src, "file:///foo.gos");
        assert!(
            labels.iter().any(|l| l == "new"),
            "expected `new` in {labels:?}"
        );
    }

    #[test]
    fn enum_qualified_completion_returns_variants() {
        let mut state = ServerState::new();
        let src = r"enum Color { Red, Green, Blue }
fn main() { Color::R| }
";
        let labels = complete_at(&mut state, src, "file:///enum.gos");
        assert!(
            labels.iter().any(|l| l == "Red"),
            "expected `Red` in {labels:?}"
        );
    }

    #[test]
    fn auto_import_suggestion_includes_use_edit() {
        let mut state = ServerState::new();
        let response = complete_full(&mut state, "fn main() { format| }\n", "file:///fmt_use.gos");
        let Value::Array(items) = response else {
            panic!("expected array response");
        };
        let mut found = false;
        for item in items {
            let Value::Object(fields) = item else {
                continue;
            };
            let Some(Value::String(label)) = fields.get("label") else {
                continue;
            };
            if label != "format" {
                continue;
            }
            if let Some(Value::Array(edits)) = fields.get("additionalTextEdits") {
                if !edits.is_empty() {
                    found = true;
                    break;
                }
            }
        }
        assert!(
            found,
            "expected at least one `format` completion with additionalTextEdits"
        );
    }

    #[test]
    fn function_completion_carries_snippet_insert_text() {
        let mut state = ServerState::new();
        let response = complete_full(&mut state, "fn main() { printl| }\n", "file:///snippet.gos");
        let Value::Array(items) = response else {
            panic!("expected array");
        };
        let mut found_snippet = false;
        for item in items {
            let Value::Object(fields) = item else {
                continue;
            };
            let Some(Value::String(label)) = fields.get("label") else {
                continue;
            };
            if label == "println"
                && matches!(fields.get("insertTextFormat"), Some(Value::Number(n)) if (*n - 2.0).abs() < 0.5)
            {
                found_snippet = true;
                break;
            }
        }
        assert!(
            found_snippet,
            "expected a snippet-format `println` completion"
        );
    }

    #[test]
    fn module_member_completion_carries_documentation() {
        let mut state = ServerState::new();
        let response = complete_full(
            &mut state,
            "use std::env\nfn main() { env::a| }\n",
            "file:///doc.gos",
        );
        let Value::Array(items) = response else {
            panic!("expected array");
        };
        let mut found_doc = false;
        for item in items {
            let Value::Object(fields) = item else {
                continue;
            };
            let Some(Value::String(label)) = fields.get("label") else {
                continue;
            };
            if label == "args" {
                if let Some(Value::Object(docs)) = fields.get("documentation") {
                    if let Some(Value::String(value)) = docs.get("value") {
                        if !value.is_empty() {
                            found_doc = true;
                        }
                    }
                }
            }
        }
        assert!(
            found_doc,
            "expected `env::args` completion to carry documentation"
        );
    }

    #[test]
    fn workspace_completion_surfaces_symbol_from_other_file() {
        let mut state = ServerState::new();
        state.update("file:///util.gos", "fn shared_helper() -> i64 { 1 }\n");
        let labels = complete_at(&mut state, "fn main() { shared_h| }\n", "file:///main.gos");
        assert!(
            labels.iter().any(|l| l == "shared_helper"),
            "expected `shared_helper` from util.gos in {labels:?}"
        );
    }

    #[test]
    fn workspace_completion_drops_renamed_symbol_after_didchange() {
        let mut state = ServerState::new();
        state.update("file:///lib.gos", "fn old_thing() { }\n");
        state.update("file:///lib.gos", "fn new_thing() { }\n");
        let labels = complete_at(&mut state, "fn main() { old_t| }\n", "file:///main.gos");
        assert!(
            !labels.iter().any(|l| l == "old_thing"),
            "expected `old_thing` to be gone after rename; got {labels:?}"
        );
    }

    #[test]
    fn semantic_tokens_returns_data_array_for_known_doc() {
        let mut state = ServerState::new();
        state.update("file:///t.gos", "fn helper() { }\n");
        let mut params = BTreeMap::new();
        let mut text_doc = BTreeMap::new();
        text_doc.insert(
            "uri".to_string(),
            Value::String("file:///t.gos".to_string()),
        );
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        let response = state.semantic_tokens(&Value::Object(params));
        let Value::Object(fields) = response else {
            panic!("not object");
        };
        let Value::Array(data) = fields.get("data").unwrap() else {
            panic!("data not array");
        };
        assert!(!data.is_empty(), "expected at least one semantic token");
    }
}

/// In-memory request surface mirroring the JSON-RPC loop, for embedders
/// that drive the analysis engine without a transport: the `gos mcp`
/// server's navigation tools and this crate's integration tests.
pub mod handle {
    use std::collections::BTreeMap;

    use gossamer_std::json::Value;

    use super::ServerState;

    /// Thin wrapper around the crate-private `ServerState` mirroring
    /// the request surface used by the JSON-RPC loop.
    pub struct ServerHandle {
        state: ServerState,
    }

    impl Default for ServerHandle {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ServerHandle {
        /// Constructs a handle around a fresh server state.
        #[must_use]
        pub fn new() -> Self {
            Self {
                state: ServerState::new(),
            }
        }

        /// Mirrors `textDocument/didOpen` / `didChange`.
        pub fn update(&mut self, uri: &str, text: &str) {
            self.state.update(uri, text);
        }

        /// Mirrors `textDocument/didClose`.
        pub fn close(&mut self, uri: &str) {
            self.state.close(uri);
        }

        /// Indexes every `.gos` file under `root` for workspace-wide
        /// symbol queries, mirroring the `initialize` workspace-roots
        /// scan (same 1000-file budget).
        pub fn scan_workspace(&mut self, root: &str) {
            let mut budget = 1000usize;
            self.state.scan_workspace_path(root, &mut budget);
        }

        /// Mirrors `textDocument/references`.
        #[must_use]
        pub fn references(&self, params: &Value) -> Value {
            self.state.references(params)
        }

        /// Mirrors `textDocument/rename`.
        #[must_use]
        pub fn rename(&self, params: &Value) -> Value {
            self.state.rename(params)
        }

        /// Mirrors `textDocument/prepareRename`.
        #[must_use]
        pub fn prepare_rename(&self, params: &Value) -> Value {
            self.state.prepare_rename(params)
        }

        /// Mirrors `textDocument/completion`.
        #[must_use]
        pub fn completion(&self, params: &Value) -> Value {
            self.state.completion(params)
        }

        /// Mirrors `textDocument/hover`.
        #[must_use]
        pub fn hover(&self, params: &Value) -> Value {
            self.state.hover(params)
        }

        /// Mirrors `textDocument/definition`.
        #[must_use]
        pub fn definition(&self, params: &Value) -> Value {
            self.state.definition(params)
        }

        /// Mirrors `textDocument/typeDefinition`.
        #[must_use]
        pub fn type_definition(&self, params: &Value) -> Value {
            self.state.type_definition(params)
        }

        /// Mirrors `textDocument/documentSymbol`.
        #[must_use]
        pub fn document_symbols(&self, params: &Value) -> Value {
            self.state.document_symbols(params)
        }

        /// Mirrors `workspace/symbol`.
        #[must_use]
        pub fn workspace_symbols(&self, params: &Value) -> Value {
            self.state.workspace_symbols(params)
        }

        /// Mirrors `textDocument/codeAction`.
        #[must_use]
        pub fn code_actions(&self, params: &Value) -> Value {
            self.state.code_actions(params)
        }

        /// Mirrors `textDocument/formatting`.
        #[must_use]
        pub fn formatting(&self, params: &Value) -> Value {
            self.state.formatting(params)
        }

        /// Mirrors `textDocument/semanticTokens/full`.
        #[must_use]
        pub fn semantic_tokens(&self, params: &Value) -> Value {
            self.state.semantic_tokens(params)
        }

        /// Mirrors `textDocument/inlayHint`.
        #[must_use]
        pub fn inlay_hints(&self, params: &Value) -> Value {
            self.state.inlay_hints(params)
        }

        /// Mirrors `textDocument/signatureHelp`.
        #[must_use]
        pub fn signature_help(&self, params: &Value) -> Value {
            self.state.signature_help(params)
        }

        /// Mirrors `textDocument/foldingRange`.
        #[must_use]
        pub fn folding_ranges(&self, params: &Value) -> Value {
            self.state.folding_ranges(params)
        }

        /// Mirrors `textDocument/documentHighlight`.
        #[must_use]
        pub fn document_highlight(&self, params: &Value) -> Value {
            self.state.document_highlight(params)
        }

        /// Diagnostics published for `uri` after the last `update`.
        #[must_use]
        pub fn publish_diagnostics(&self, uri: &str) -> Vec<Value> {
            self.state.publish_diagnostics(uri)
        }
    }

    /// Builds the JSON-RPC params payload for a document-only
    /// request (no position).
    #[must_use]
    pub fn document_params(uri: &str) -> Value {
        let mut text_doc = BTreeMap::new();
        text_doc.insert("uri".to_string(), Value::String(uri.to_string()));
        let mut params = BTreeMap::new();
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        Value::Object(params)
    }

    /// Builds a `Range` JSON value spanning the given lines/columns.
    #[must_use]
    pub fn range_value(start_line: u32, start_char: u32, end_line: u32, end_char: u32) -> Value {
        let mut start = BTreeMap::new();
        start.insert("line".to_string(), Value::Number(f64::from(start_line)));
        start.insert(
            "character".to_string(),
            Value::Number(f64::from(start_char)),
        );
        let mut end = BTreeMap::new();
        end.insert("line".to_string(), Value::Number(f64::from(end_line)));
        end.insert("character".to_string(), Value::Number(f64::from(end_char)));
        let mut range = BTreeMap::new();
        range.insert("start".to_string(), Value::Object(start));
        range.insert("end".to_string(), Value::Object(end));
        Value::Object(range)
    }

    /// Builds the JSON-RPC params payload for `textDocument/codeAction`.
    /// `diagnostics` is the optional diagnostic context the editor
    /// surfaces for the range - pass `vec![]` when the test has none.
    #[must_use]
    pub fn code_action_params(uri: &str, range: Value, diagnostics: Vec<Value>) -> Value {
        let mut text_doc = BTreeMap::new();
        text_doc.insert("uri".to_string(), Value::String(uri.to_string()));
        let mut context = BTreeMap::new();
        context.insert("diagnostics".to_string(), Value::Array(diagnostics));
        let mut params = BTreeMap::new();
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        params.insert("range".to_string(), range);
        params.insert("context".to_string(), Value::Object(context));
        Value::Object(params)
    }

    /// Builds the JSON-RPC params payload for `workspace/symbol`.
    #[must_use]
    pub fn workspace_symbol_params(query: &str) -> Value {
        let mut params = BTreeMap::new();
        params.insert("query".to_string(), Value::String(query.to_string()));
        Value::Object(params)
    }

    /// Builds the JSON-RPC params payload for a position-aware
    /// request (`textDocument/{references, rename, ...}`).
    #[must_use]
    pub fn position_params(uri: &str, line: u32, character: u32) -> Value {
        let mut text_doc = BTreeMap::new();
        text_doc.insert("uri".to_string(), Value::String(uri.to_string()));
        let mut pos = BTreeMap::new();
        pos.insert("line".to_string(), Value::Number(f64::from(line)));
        pos.insert("character".to_string(), Value::Number(f64::from(character)));
        let mut params = BTreeMap::new();
        params.insert("textDocument".to_string(), Value::Object(text_doc));
        params.insert("position".to_string(), Value::Object(pos));
        Value::Object(params)
    }

    /// Convenience wrapper that adds a `newName` field to a
    /// position-shaped params payload.
    #[must_use]
    pub fn rename_params(uri: &str, line: u32, character: u32, new_name: &str) -> Value {
        let mut params = position_params(uri, line, character);
        if let Value::Object(map) = &mut params {
            map.insert("newName".to_string(), Value::String(new_name.to_string()));
        }
        params
    }
}
