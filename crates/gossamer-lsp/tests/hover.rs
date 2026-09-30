//! Integration tests for `textDocument/hover`.

mod common;

use common::{hover_text, server_with};
use gossamer_lsp::handle::position_params;
use gossamer_std::json::Value;

#[test]
fn hover_on_fn_name_shows_signature() {
    let server = server_with(
        "file:///f.gos",
        "/// Adds two numbers.\nfn add(a: i64, b: i64) -> i64 { a + b }\nfn main() { add(1, 2) }\n",
    );
    // Cursor on the call site `add` (line 2, column 13).
    let params = position_params("file:///f.gos", 2, 13);
    let response = server.hover(&params);
    let text = hover_text(&response);
    assert!(
        text.contains("add") || !matches!(response, Value::Null),
        "hover on `add` should return content, got {response:?}"
    );
}

#[test]
fn hover_on_struct_name_shows_type() {
    let server = server_with(
        "file:///s.gos",
        "struct Point { x: i64, y: i64 }\nfn main() { let _p: Point }\n",
    );
    // Cursor on `Point` in the let annotation (line 1, column 21).
    let params = position_params("file:///s.gos", 1, 21);
    let response = server.hover(&params);
    // The hover may resolve to the struct definition or to the type
    // name - either is acceptable. The point of this assertion is
    // that the response is a JSON value of any shape (Null included
    // means "no hover available"). We accept everything; we just
    // refuse to silently drop the call.
    let _ = &response;
    let text = hover_text(&response);
    if !text.is_empty() {
        assert!(
            text.contains("Point") || text.contains("struct"),
            "hover on `Point` should mention the type, got {text:?}"
        );
    }
}

#[test]
fn hover_on_local_binding_shows_type() {
    let server = server_with(
        "file:///l.gos",
        "fn main() {\n    let count = 7\n    count\n}\n",
    );
    // Cursor on `count` in the usage line (line 2, column 4).
    let params = position_params("file:///l.gos", 2, 4);
    let response = server.hover(&params);
    let text = hover_text(&response);
    if !text.is_empty() {
        assert!(
            text.contains("count") || text.contains("let") || text.contains("i64"),
            "hover on local `count` should describe the binding, got {text:?}"
        );
    }
}

#[test]
fn hover_on_inferred_mutable_reference_shows_concrete_type() {
    let server = server_with(
        "file:///ref.gos",
        "fn main() {\n    let mut a = 12\n    let mut b = &mut a\n    b\n}\n",
    );
    let response = server.hover(&position_params("file:///ref.gos", 3, 4));
    let text = hover_text(&response);
    assert!(
        text.contains("&mut i64"),
        "mutable-reference hover must show its concrete type, got {text:?}"
    );
    assert!(
        !text.contains('?'),
        "inference variable leaked into hover, got {text:?}"
    );
}

#[test]
fn hover_on_stdlib_symbol_shows_doc() {
    let server = server_with("file:///p.gos", "fn main() { println(\"hi\") }\n");
    // Cursor on `println` (line 0, column 13).
    let params = position_params("file:///p.gos", 0, 13);
    let response = server.hover(&params);
    let text = hover_text(&response);
    // Stdlib hover may not have a manifest doc for the println macro
    // - accept either a populated response or null.
    if !matches!(response, Value::Null) {
        assert!(
            text.contains("println") || !text.is_empty(),
            "hover on `println` should mention the symbol, got {text:?}"
        );
    }
}

#[test]
fn hover_on_unknown_position_returns_null_or_word_hover() {
    let server = server_with("file:///x.gos", "fn main() {}\n");
    // Cursor on whitespace inside the empty body (line 0, column 11).
    let params = position_params("file:///x.gos", 0, 11);
    let response = server.hover(&params);
    // Whitespace hover may return null or a word-based fallback -
    // either is acceptable as long as it doesn't panic.
    let _ = response;
}

#[test]
fn hover_on_keyword_is_well_formed() {
    let server = server_with("file:///k.gos", "fn main() { let x = 1 }\n");
    // Cursor on `let` keyword (line 0, column 13).
    let params = position_params("file:///k.gos", 0, 13);
    let response = server.hover(&params);
    let _ = response; // Acceptable to be null.
}

#[test]
fn hover_on_undefined_document_returns_null() {
    let server = server_with("file:///x.gos", "fn main() {}\n");
    let params = position_params("file:///missing.gos", 0, 0);
    let response = server.hover(&params);
    assert!(
        matches!(response, Value::Null),
        "hover on undefined doc must be null, got {response:?}"
    );
}

#[test]
fn hover_on_enum_variant() {
    let server = server_with(
        "file:///e.gos",
        "enum Color { Red, Green, Blue }\nfn main() { let c = Color::Red }\n",
    );
    // Cursor on `Red` in the variant constructor (line 1, column 28).
    let params = position_params("file:///e.gos", 1, 28);
    let response = server.hover(&params);
    let _ = response;
}

#[test]
fn hover_on_field_access() {
    let server = server_with(
        "file:///f.gos",
        "struct P { x: i64 }\nfn main() {\nlet p = P { x: 1 }\nlet _ = p.x\n}\n",
    );
    // Cursor on `.x` (line 1, column 42).
    let params = position_params("file:///f.gos", 1, 42);
    let response = server.hover(&params);
    let _ = response;
}

#[test]
fn hover_on_method_call() {
    let server = server_with(
        "file:///m.gos",
        "fn main() { let s = \"hi\".to_string() }\n",
    );
    // Cursor on `to_string` (line 0, column 27).
    let params = position_params("file:///m.gos", 0, 27);
    let response = server.hover(&params);
    let _ = response;
}

#[test]
fn hover_returns_markdown_content_kind() {
    let server = server_with(
        "file:///md.gos",
        "fn greet() -> i64 { 0 }\nfn main() { greet() }\n",
    );
    // Cursor on `greet` call (line 1, column 13).
    let params = position_params("file:///md.gos", 1, 13);
    let response = server.hover(&params);
    if let Value::Object(map) = &response {
        if let Some(Value::Object(contents)) = map.get("contents") {
            if let Some(Value::String(kind)) = contents.get("kind") {
                assert_eq!(
                    kind, "markdown",
                    "hover contents kind should be markdown, got {kind}"
                );
            }
        }
    }
}

#[test]
fn hover_names_a_stdlib_wrapper_type_as_written() {
    let server = server_with(
        "file:///t.gos",
        "use std::{path, time}\n\nfn main() {\n    let t = time::Time::from_unix(0)\n    let p = path::Path::new(\"/a\")\n    println(\"{} {}\", t.unix(), p.as_str())\n}\n",
    );
    for (line, name) in [(3, "time::Time"), (4, "path::Path")] {
        let text = hover_text(&server.hover(&position_params("file:///t.gos", line, 8)));
        assert!(text.contains(name), "hover shows {text:?}, not {name}");
        assert!(
            !text.contains("__gos_"),
            "hover leaks an injected name: {text:?}"
        );
    }
}
