#[cfg(test)]
mod autoderive_tests {
    use gossamer_lex::SourceMap;

    use crate::{ParseError, SerdeTargetRefusal};

    /// Parses the way the driver does - `augment_source` first, so the
    /// synthesized `__gos_serde_*` functions are in the tree the reporter
    /// reads. Parsing raw source instead would leave that set empty and
    /// every serializable struct would look like one without a codec.
    fn autoderive_diags(source: &str) -> Vec<crate::ParseDiagnostic> {
        let augmented = super::augment_source(source);
        let mut map = SourceMap::new();
        let file = map.add_file("test.gos", augmented.clone());
        super::parse_with_autoderive(&augmented, file).1
    }

    /// The injected names a tree spells after the stdlib surface rewrite,
    /// read off the rewritten tree itself.
    fn names_after_rewrite(parsed: &gossamer_ast::SourceFile) -> std::collections::HashSet<String> {
        struct Names(std::collections::HashSet<String>);
        impl gossamer_ast::Visitor for Names {
            fn visit_path_expr(&mut self, path: &gossamer_ast::PathExpr) {
                for segment in &path.segments {
                    if segment.name.name.starts_with("__gos_") {
                        self.0.insert(segment.name.name.clone());
                    }
                }
                gossamer_ast::visitor::walk_path_expr(self, path);
            }
            fn visit_type_path(&mut self, path: &gossamer_ast::ty::TypePath) {
                for segment in &path.segments {
                    if segment.name.name.starts_with("__gos_") {
                        self.0.insert(segment.name.name.clone());
                    }
                }
                gossamer_ast::visitor::walk_type_path(self, path);
            }
        }
        let mut rewritten = parsed.clone();
        super::rewrite_stdlib_struct_surface(&mut rewritten);
        let mut names = Names(std::collections::HashSet::new());
        gossamer_ast::Visitor::visit_source_file(&mut names, &rewritten);
        names.0
    }

    #[test]
    fn implicit_derives_keep_same_named_declarations_separate() {
        let runtime = "struct Model { requests: Sender<i64> }\n";
        let value = "mod values { pub struct Model { value: i64 } }\n";
        for source in [format!("{runtime}{value}"), format!("{value}{runtime}")] {
            let augmented = super::augment_source(&source);
            let generated = &augmented[source.len()..];
            assert!(!generated.contains("impl Model {"), "{generated}");
            assert!(generated.contains("impl values::Model {"), "{generated}");
        }
    }

    #[test]
    fn implicit_derives_follow_qualified_field_types() {
        let source = "struct Model { requests: Sender<i64> }\nmod values { pub struct Model { value: i64 } }\nstruct Wrapper { value: values::Model }\n";
        let augmented = super::augment_source(source);
        let generated = &augmented[source.len()..];
        assert!(generated.contains("impl Wrapper {"), "{generated}");
        let start = generated.find("impl Wrapper {").expect("wrapper impl");
        assert!(generated[start..].contains("fn eq("), "{generated}");
    }

    #[test]
    fn wrapper_names_match_the_rewritten_tree_on_every_fixture() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut with_names = 0;
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
                let Some(parsed) = super::clean_parse(&source) else {
                    continue;
                };
                let expected = names_after_rewrite(&parsed);
                assert_eq!(
                    super::stdlib_wrapper_names(&parsed),
                    expected,
                    "{}",
                    path.display()
                );
                if !expected.is_empty() {
                    with_names += 1;
                }
            }
        }
        assert!(with_names > 10, "only {with_names} fixtures name a wrapper");
    }

    fn serde_field_errors(source: &str) -> Vec<(String, String)> {
        let diags = autoderive_diags(source);
        diags
            .into_iter()
            .filter_map(|d| match d.error {
                ParseError::SerdeUnserializableField {
                    field, field_ty, ..
                } => Some((field, field_ty)),
                _ => None,
            })
            .collect()
    }

    fn serde_target_refusals(source: &str) -> Vec<(String, SerdeTargetRefusal)> {
        autoderive_diags(source)
            .into_iter()
            .filter_map(|d| match d.error {
                ParseError::SerdeUnsupportedTarget { ty, reason, .. } => Some((ty, reason)),
                _ => None,
            })
            .collect()
    }

    /// A parse error elsewhere leaves the codecs unsynthesized, which says
    /// nothing about the serde target: only the parse error is reported.
    #[test]
    fn a_parse_error_elsewhere_reports_no_serde_refusal() {
        let source = "use std::regex\nstruct P { x: i64 }\n\
                      fn main() {\n    let p = \"a\"\n    let _ = regex::compile(p)\n    \
                      let _ = from_json::<P>(\"{}\")\n}\n";
        assert!(serde_target_refusals(source).is_empty());
    }

    /// Every refusal reports in the user's own vocabulary. A synthesized
    /// `__gos_serde_*` name reaching the user means one of these is missing.
    #[test]
    fn serde_target_outside_the_synthesizer_names_its_shape() {
        let cases = [
            (
                "enum E<T> { A(T), B }\nfn main() { let _ = to_json::<E<i64>>(E::A(1)); }",
                "E",
                SerdeTargetRefusal::Generic,
            ),
            (
                "struct W<T> { v: T }\nfn main() { let _ = to_json::<W<i64>>(W { v: 1 }); }",
                "W",
                SerdeTargetRefusal::Generic,
            ),
            (
                "fn main() { let _ = to_json::<Nope>(1); }",
                "Nope",
                SerdeTargetRefusal::NotAStruct,
            ),
        ];
        for (src, ty, reason) in cases {
            assert_eq!(
                serde_target_refusals(src),
                vec![(ty.to_string(), reason)],
                "refusal for {ty}"
            );
        }
    }

    /// A struct the synthesizer accepts is reached through an alias too, so
    /// the alias spelling is serialized rather than refused.
    #[test]
    fn serde_target_through_an_alias_resolves_to_the_struct() {
        let src = "struct Point { x: i64 }\n                   type P = Point\n                   fn main() { let _ = to_json::<P>(Point { x: 1 }); }";
        assert!(serde_target_refusals(src).is_empty());
        assert!(serde_field_errors(src).is_empty());
    }

    /// A callable field names its own type in the report rather than the
    /// renderer's catch-all.
    #[test]
    fn unserializable_callable_field_reports_its_written_type() {
        let src = "struct Holder { cb: Fn(i64) -> i64, n: i64 }\n                   fn main() { let _ = to_json::<Holder>(Holder { cb: |x: i64| x, n: 1 }); }";
        assert_eq!(
            serde_field_errors(src),
            vec![("cb".to_string(), "Fn(i64) -> i64".to_string())]
        );
    }

    #[test]
    fn unserializable_field_used_in_serde_is_reported() {
        let src = "enum Tint<T> { Of(T) }\n\
                   struct Paint { name: String, shade: Tint<i64> }\n\
                   fn main() { let _ = to_json::<Paint>(Paint { name: \"w\", shade: Tint::Of(1) }); }";
        let errs = serde_field_errors(src);
        assert_eq!(errs, vec![("shade".to_string(), "Tint<i64>".to_string())]);
    }

    #[test]
    fn unserializable_field_never_serialized_is_silent() {
        let src = "enum Color { Red, Green }\n\
                   struct Paint { name: String, shade: Color }\n\
                   fn main() { let p = Paint { name: \"w\", shade: Color::Red }; let _ = p.name; }";
        assert!(serde_field_errors(src).is_empty());
    }

    #[test]
    fn fully_serializable_struct_is_silent() {
        let src = "struct Inner { n: i64 }\n\
                   struct Outer { id: i64, tags: Vec<String>, inner: Inner }\n\
                   fn main() { let _ = to_json::<Outer>(Outer { id: 1, tags: Vec::from([\"a\"]), inner: Inner { n: 2 } }); }";
        assert!(serde_field_errors(src).is_empty());
    }

    #[test]
    fn prescan_ignores_type_keywords_in_comments_and_strings() {
        let src = "fn main() {\n\
                   \tlet _ = \"struct NotAType\"\n\
                   \t// enum AlsoNotAType { A }\n\
                   }\n";
        assert!(!super::source_may_need_ast_synthesis(&super::SourceTokens::new(src)));
        assert_eq!(
            super::augment_source(src),
            format!("{src}{}", super::GENERATED_SECTION_MARKER)
        );
    }

    #[test]
    fn prescan_detects_real_type_declarations() {
        for src in [
            "struct Point { x: i64 }\nfn main() {}\n",
            "enum Color { Red }\nfn main() {}\n",
            "#[derive(Debug)]\nfn main() {}\n",
        ] {
            assert!(super::source_may_need_ast_synthesis(&super::SourceTokens::new(src)));
            // Once the scans have lexed the source, the prescan reads their
            // tokens and answers the same.
            let lexed = super::SourceTokens::new(src);
            assert!(!lexed.all().is_empty());
            assert!(super::source_may_need_ast_synthesis(&lexed));
        }
        let plain = super::SourceTokens::new("fn main() { let _ = 1 }\n");
        assert!(!plain.all().is_empty());
        assert!(!super::source_may_need_ast_synthesis(&plain));
    }

    #[test]
    fn a_literal_regex_needs_no_synthesized_code() {
        let src = "use std::regex\nfn main() { let _ = regex::compile(\"^[a]+$\") }\n";
        assert_eq!(
            super::augment_source(src),
            format!("{src}{}", super::GENERATED_SECTION_MARKER)
        );
    }

    #[test]
    fn the_program_ends_at_the_last_marker_the_toolchain_appends() {
        // A program that writes the marker line itself still ends where the
        // toolchain's own marker begins, after everything the program wrote.
        let src = format!(
            "fn main() {{}}\n{}unsafe extern \"C\" {{\n    fn abs(x: i32) -> i32\n}}\n",
            super::GENERATED_SECTION_MARKER
        );
        let augmented = super::augment_source(&src);
        assert!(super::program_source_end(&augmented) >= src.len());
    }
}
