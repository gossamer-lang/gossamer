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

    /// Every refusal reports in the user's own vocabulary. A synthesized
    /// `__gos_serde_*` name reaching the user means one of these is missing.
    #[test]
    fn serde_target_outside_the_synthesizer_names_its_shape() {
        let cases = [
            (
                "enum E { A(i64), B }\nfn main() { let _ = to_json::<E>(E::A(1)); }",
                "E",
                SerdeTargetRefusal::Enum,
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
        let src = "enum Color { Red, Green }\n\
                   struct Paint { name: String, shade: Color }\n\
                   fn main() { let _ = to_json::<Paint>(Paint { name: \"w\", shade: Color::Red }); }";
        let errs = serde_field_errors(src);
        assert_eq!(errs, vec![("shade".to_string(), "Color".to_string())]);
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
        assert!(!super::source_may_need_ast_synthesis(src));
        assert_eq!(super::augment_source(src), src);
    }

    #[test]
    fn prescan_detects_real_type_declarations() {
        assert!(super::source_may_need_ast_synthesis(
            "struct Point { x: i64 }\nfn main() {}\n"
        ));
        assert!(super::source_may_need_ast_synthesis(
            "enum Color { Red }\nfn main() {}\n"
        ));
    }

    #[test]
    fn a_literal_regex_needs_no_synthesized_code() {
        let src = "use std::regex\nfn main() { let _ = regex::compile(\"^[a]+$\") }\n";
        assert_eq!(super::augment_source(src), src);
    }
}
