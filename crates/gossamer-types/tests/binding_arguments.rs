//! An argument to a binding function takes the value its binding type spells,
//! as an argument to a program's own function does (a sequence parameter takes
//! an array, a `Vec`, or a view), and the call names every parameter. Binding modules register process-wide, so these cases run in a
//! test binary of their own.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::{BindingType, ExternalItem, ExternalModule, resolve_source_file};
use gossamer_types::{TyCtxt, TypeError, typecheck_source_file};

fn register_menus() {
    gossamer_resolve::set_external_modules(vec![ExternalModule {
        path: "menus".to_string(),
        doc: String::new(),
        items: vec![
            ExternalItem {
                name: "set_bar".to_string(),
                params: vec![BindingType::String],
                ret: BindingType::Unit,
                doc: String::new(),
                json: 0,
            },
            ExternalItem {
                name: "total".to_string(),
                params: vec![
                    BindingType::Vec(Box::new(BindingType::I64)),
                    BindingType::Bool,
                ],
                ret: BindingType::I64,
                doc: String::new(),
                json: 0,
            },
        ],
    }]);
}

fn diagnostics(source: &str) -> Vec<TypeError> {
    let mut map = SourceMap::new();
    let file = map.add_file("binding-arguments.gos".to_string(), source.to_string());
    let (parsed, parse_diagnostics) = parse_source_file(source, file);
    assert!(parse_diagnostics.is_empty(), "{parse_diagnostics:#?}");
    let (resolutions, _) = resolve_source_file(&parsed);
    let mut tcx = TyCtxt::new();
    let (_, diagnostics) = typecheck_source_file(&parsed, &resolutions, &mut tcx);
    diagnostics.into_iter().map(|d| d.error).collect()
}

fn mismatch(errors: &[TypeError], expected: &str, found: &str) -> bool {
    errors.iter().any(|e| {
        matches!(
            e,
            TypeError::TypeMismatch { expected: want, found: got }
                if want == expected && got == found
        )
    })
}

#[test]
fn binding_arguments_are_checked_against_their_binding_types() {
    register_menus();
    let fitting = diagnostics(
        "use menus\nuse std::errors\n\
         fn text() -> Result<String, errors::Error> { Ok(\"[1]\") }\n\
         fn main() -> Result<(), errors::Error> {\n    menus::set_bar(text()?)\n    \
         let n: i64 = menus::total(#[1, 2, 3], true)\n    \
         let m: i64 = menus::total([4, 5], false)\n    println(f\"{n} {m}\")\n    Ok(())\n}\n",
    );
    assert!(fitting.is_empty(), "{fitting:#?}");

    let integer = diagnostics("use menus\nfn main() {\n    menus::set_bar(42)\n}\n");
    assert!(mismatch(&integer, "String", "i64"), "{integer:#?}");

    let unhandled = diagnostics(
        "use menus\nuse std::errors\n\
         fn text() -> Result<String, errors::Error> { Ok(\"[1]\") }\n\
         fn main() {\n    menus::set_bar(text())\n}\n",
    );
    assert!(
        mismatch(&unhandled, "String", "Result<String, errors::Error>"),
        "{unhandled:#?}"
    );

    let wrong_elements =
        diagnostics("use menus\nfn main() {\n    let _ = menus::total(#[\"a\"], false)\n}\n");
    assert!(!wrong_elements.is_empty(), "{wrong_elements:#?}");

    let short = diagnostics("use menus\nfn main() {\n    let _ = menus::total(#[1])\n}\n");
    assert!(
        short.iter().any(|e| matches!(
            e,
            TypeError::CallArityMismatch { callee, expected: 2, found: 1 }
                if callee == "menus::total"
        )),
        "{short:#?}"
    );
}
