//! The frontend parses only what autoderive appended to a program, taking
//! the program's own items from the tree autoderive already parsed. The tree
//! that splice answers must be the one a parse of the whole augmented text
//! answers, spans and node ids included.

use std::path::Path;

use gossamer_lex::SourceMap;
use gossamer_parse::autoderive::{
    augment_source_with_program, parse_with_autoderive, parse_with_autoderive_from,
};

/// A rendering of `sf` that two equal trees share, with the call-site label
/// table in a fixed order.
fn rendered(mut sf: gossamer_ast::SourceFile) -> String {
    let mut named: Vec<_> = std::mem::take(&mut sf.named_args).into_iter().collect();
    named.sort_by_key(|(id, _)| id.as_u32());
    format!("{sf:?}\n{named:?}")
}

/// Whether `source` reached the splice, which a program that does not
/// parse cleanly does not.
fn assert_splice_matches(label: &str, source: &str) -> bool {
    let (augmented, program) = augment_source_with_program(source);
    let Some(program) = program else {
        return false;
    };
    let mut map = SourceMap::new();
    let file = map.add_file(label, augmented.clone());
    let (whole, whole_diags) = parse_with_autoderive(&augmented, file);
    let (spliced, spliced_diags) = parse_with_autoderive_from(&augmented, file, Some(program));
    assert_eq!(
        whole_diags.len(),
        spliced_diags.len(),
        "{label}: the spliced parse reported different diagnostics"
    );
    assert!(
        rendered(whole) == rendered(spliced),
        "{label}: the spliced tree differs from the whole parse"
    );
    true
}

#[test]
fn a_spliced_parse_matches_the_whole_parse_on_every_fixture() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut spliced = 0;
    for dir in ["feature-testing-examples", "examples"] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "gos") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            if assert_splice_matches(&path.display().to_string(), &source) {
                spliced += 1;
            }
        }
    }
    assert!(spliced > 100, "only {spliced} fixtures reached the splice");
}

#[test]
fn a_spliced_parse_matches_on_the_shapes_at_the_program_boundary() {
    let shapes = [
        (
            "no trailing newline",
            "#[derive(Debug, PartialEq)]\nstruct P { x: i64 }\nfn main() { println(P { x: 1 }) }",
        ),
        (
            "byte order mark",
            "\u{feff}#[derive(Debug)]\nstruct P { x: i64 }\nfn main() { println(P { x: 1 }) }\n",
        ),
        (
            "top-level statements",
            "#[derive(Debug)]\nstruct P { x: i64 }\nlet p = P { x: 1 }\nprintln(p)",
        ),
        (
            "inline module with a hoisted use",
            "mod inner {\n    use std::strings\n    #[derive(Debug)]\n    pub struct Q { s: String }\n}\nfn main() { let n = 2.pow(3)\n println(f\"{n:+}\") }\n",
        ),
        (
            "trailing comment",
            "#[derive(Debug)]\nstruct P { x: i64 }\nfn main() {}\n// the end",
        ),
    ];
    for (label, source) in shapes {
        assert!(
            assert_splice_matches(label, source),
            "{label} did not parse cleanly"
        );
    }
}

#[test]
fn a_program_from_another_file_is_parsed_whole() {
    let source = "#[derive(Debug)]\nstruct P { x: i64 }\nfn main() {}\n";
    let (augmented, program) = augment_source_with_program(source);
    let mut map = SourceMap::new();
    let _ = map.add_file("first", String::new());
    let second = map.add_file("second", augmented.clone());
    let (whole, _) = parse_with_autoderive(&augmented, second);
    let (from, _) = parse_with_autoderive_from(&augmented, second, program);
    assert_eq!(rendered(whole), rendered(from));
}
