//! Hover, definition, and references over the two forms a type's
//! declaration takes: defaulted fields, and methods written in its body.

mod common;

use common::{hover_text, server_with};
use gossamer_lsp::handle::position_params;
use gossamer_std::json::Value;

const SOURCE: &str = "trait Bench {\n    fn checksum(&self) -> u32\n}\n\nstruct Sieve: Bench {\n    limit: i64\n    result: u32 = 0\n\n    fn checksum(&self) -> u32 { self.result }\n    fn doubled(&self) -> i64 { self.limit * 2 }\n}\n\nfn main() {\n    let s = Sieve { limit: 3 }\n    println(s.checksum())\n    println(s.doubled())\n    println(s.result)\n}\n";

fn location_lines(response: &Value) -> Vec<i64> {
    let items: Vec<&Value> = match response {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![response],
        _ => Vec::new(),
    };
    items
        .into_iter()
        .filter_map(|item| {
            let range = common::field(item, "range");
            let start = common::field(range, "start");
            common::field_i64(start, "line")
        })
        .collect()
}

#[test]
fn hover_on_an_in_body_method_call_shows_its_signature() {
    let server = server_with("file:///b.gos", SOURCE);
    // `doubled` in `s.doubled()` on line 15.
    let response = server.hover(&position_params("file:///b.gos", 15, 15));
    let text = hover_text(&response);
    assert!(text.contains("doubled"), "hover: {text:?}");
    assert!(text.contains("i64"), "hover: {text:?}");
}

#[test]
fn definition_of_an_in_body_method_is_its_line_in_the_type_body() {
    let server = server_with("file:///b.gos", SOURCE);
    let response = server.definition(&position_params("file:///b.gos", 14, 15));
    assert_eq!(
        location_lines(&response),
        vec![8],
        "definition: {response:?}"
    );
}

#[test]
fn references_of_a_defaulted_field_include_its_reads() {
    let server = server_with("file:///b.gos", SOURCE);
    // `result` in its declaration on line 6.
    let response = server.references(&position_params("file:///b.gos", 6, 5));
    let lines = location_lines(&response);
    assert!(lines.contains(&8), "references: {lines:?}");
    assert!(lines.contains(&16), "references: {lines:?}");
}

#[test]
fn hover_on_a_defaulted_field_read_shows_its_type() {
    let server = server_with("file:///b.gos", SOURCE);
    let response = server.hover(&position_params("file:///b.gos", 16, 15));
    let text = hover_text(&response);
    assert!(text.contains("u32"), "hover: {text:?}");
}

const STANDALONE: &str = "trait Bench {\n    fn checksum(&self) -> u32\n}\n\nstruct Sieve {\n    limit: i64\n    result: u32\n}\nimpl Bench for Sieve {\n    fn checksum(&self) -> u32 { self.result }\n}\nimpl Sieve {\n    fn doubled(&self) -> i64 { self.limit * 2 }\n}\n\nfn main() {\n    let s = Sieve { limit: 3, result: 0 }\n    println(s.checksum())\n    println(s.doubled())\n    println(s.result)\n}\n";

#[test]
fn a_standalone_impl_navigates_the_same_way() {
    let server = server_with("file:///s.gos", STANDALONE);
    let hover = hover_text(&server.hover(&position_params("file:///s.gos", 18, 15)));
    assert!(
        hover.contains("fn doubled(&self) -> i64"),
        "hover: {hover:?}"
    );
    let definition = server.definition(&position_params("file:///s.gos", 17, 15));
    assert_eq!(
        location_lines(&definition),
        vec![9],
        "definition: {definition:?}"
    );
    let references = location_lines(&server.references(&position_params("file:///s.gos", 6, 5)));
    assert_eq!(references, vec![6, 9, 16, 19], "references: {references:?}");
}
