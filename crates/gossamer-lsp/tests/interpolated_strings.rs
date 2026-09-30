//! Editor features reach the bindings an `f"..."` string names: a capture is
//! a reference like any other, so references, rename, hover, and go-to
//! definition find it, and formatting keeps the string intact.

use gossamer_lsp::handle::{ServerHandle, position_params, rename_params};
use gossamer_std::json::{self, Value};

const URI: &str = "file:///interp.gos";

const SOURCE: &str = "fn main() {\n    let count = 3\n    let label = f\"count is {count}\"\n    let other = format(\"{count}\")\n    println(label + other)\n}\n";

fn server() -> ServerHandle {
    let mut server = ServerHandle::new();
    server.update(URI, SOURCE);
    server
}

/// `(line, character)` of every range in a `Location[]` or edit list.
fn starts(values: &[Value]) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = values
        .iter()
        .filter_map(|v| {
            let start = json::get(json::get(v, "range")?, "start")?;
            Some((
                json::as_i64(json::get(start, "line")?)?,
                json::as_i64(json::get(start, "character")?)?,
            ))
        })
        .collect();
    out.sort_unstable();
    out
}

#[test]
fn references_include_captures_in_interpolated_strings() {
    let response = server().references(&position_params(URI, 1, 9));
    let found = starts(json::as_array(&response).expect("locations"));
    assert!(
        found.contains(&(2, 28)),
        "f-string capture missing: {found:?}"
    );
    assert!(
        found.contains(&(3, 25)),
        "format capture missing: {found:?}"
    );
}

#[test]
fn rename_rewrites_the_capture_inside_an_interpolated_string() {
    let response = server().rename(&rename_params(URI, 1, 9, "total"));
    let edits = json::get(&response, "changes")
        .and_then(|c| json::get(c, URI))
        .and_then(json::as_array)
        .cloned()
        .unwrap_or_default();
    let found = starts(&edits);
    assert!(
        found.contains(&(2, 28)),
        "f-string capture not renamed: {found:?}"
    );
}

#[test]
fn hover_and_definition_answer_on_a_capture() {
    let server = server();
    let hover = server.hover(&position_params(URI, 2, 30));
    assert!(
        json::encode(&hover).contains("i64"),
        "hover on capture: {hover:?}"
    );
    let definition = server.definition(&position_params(URI, 2, 30));
    assert!(
        json::encode(&definition).contains("\"line\":1"),
        "definition from capture: {definition:?}"
    );
}

#[test]
fn formatting_keeps_an_interpolated_string() {
    let response = server().formatting(&position_params(URI, 0, 0));
    let text = json::encode(&response);
    assert!(
        !text.contains("f \\\"") && !text.contains("error"),
        "formatting split the f-string: {text}"
    );
}

#[test]
fn rename_skips_escaped_braces_and_reaches_a_dotted_capture_head() {
    let mut server = ServerHandle::new();
    let source = "struct P {\n    x: i64\n}\n\nfn main() {\n    let p = P { x: 1 }\n    println(f\"{{p}} {p.x}\")\n}\n";
    server.update(URI, source);
    let response = server.rename(&rename_params(URI, 5, 8, "point"));
    let edits = json::get(&response, "changes")
        .and_then(|c| json::get(c, URI))
        .and_then(json::as_array)
        .cloned()
        .unwrap_or_default();
    let found = starts(&edits);
    assert_eq!(found, vec![(5, 8), (6, 21)], "rename edits: {found:?}");
}

const EXPRESSION_SOURCE: &str = "struct User {\n    name: String\n}\n\nfn main() {\n    let count = 3\n    let user = User { name: \"Ada\" }\n    println(f\"{count + 1} {user.name.len()}\")\n}\n";

#[test]
fn editor_features_reach_names_inside_a_placeholder_expression() {
    let mut server = ServerHandle::new();
    server.update(URI, EXPRESSION_SOURCE);
    let references = server.references(&position_params(URI, 5, 8));
    let found = starts(json::as_array(&references).expect("locations"));
    assert!(
        found.contains(&(7, 15)),
        "capture in `count + 1`: {found:?}"
    );

    let response = server.rename(&rename_params(URI, 6, 8, "person"));
    let edits = json::get(&response, "changes")
        .and_then(|c| json::get(c, URI))
        .and_then(json::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        starts(&edits).contains(&(7, 27)),
        "rename inside `user.name.len()`: {:?}",
        starts(&edits)
    );

    let hover = server.hover(&position_params(URI, 7, 33));
    assert!(
        json::encode(&hover).contains("String"),
        "hover on a field inside a placeholder: {hover:?}"
    );
}

#[test]
fn member_completion_works_inside_a_placeholder() {
    let mut server = ServerHandle::new();
    let source = "struct User {\n    name: String\n}\n\nfn main() {\n    let user = User { name: \"Ada\" }\n    println(f\"{user.na}\")\n}\n";
    server.update(URI, source);
    let response = server.completion(&position_params(URI, 6, 21));
    assert!(
        json::encode(&response).contains("\"name\""),
        "member completion inside a placeholder: {response:?}"
    );
}
