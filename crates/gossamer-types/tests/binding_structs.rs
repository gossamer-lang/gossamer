//! A `#[derive(GosStruct)]` struct a binding answers crosses into the
//! program's struct of the same name; without one it is GT0122. Binding
//! modules register process-wide, so these cases run in a test binary of
//! their own.

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::{BindingType, ExternalItem, ExternalModule, resolve_source_file};
use gossamer_types::{TyCtxt, TypeError, typecheck_source_file};

fn register_shapes() {
    gossamer_resolve::set_external_modules(vec![ExternalModule {
        path: "shapes".to_string(),
        doc: String::new(),
        items: vec![ExternalItem {
            name: "make".to_string(),
            params: vec![],
            ret: BindingType::Opaque("Tagged".to_string()),
            doc: String::new(),
            json: 0,
        }],
    }]);
}

fn diagnostics(source: &str) -> Vec<TypeError> {
    let mut map = SourceMap::new();
    let file = map.add_file("binding-structs.gos".to_string(), source.to_string());
    let (parsed, parse_diagnostics) = parse_source_file(source, file);
    assert!(parse_diagnostics.is_empty(), "{parse_diagnostics:#?}");
    let (resolutions, _) = resolve_source_file(&parsed);
    let mut tcx = TyCtxt::new();
    let (_, diagnostics) = typecheck_source_file(&parsed, &resolutions, &mut tcx);
    diagnostics.into_iter().map(|d| d.error).collect()
}

#[test]
fn a_binding_struct_reads_as_the_program_struct_of_its_name_and_is_gt0122_without_one() {
    register_shapes();
    let declared = diagnostics(
        "use shapes\n\
         struct Tagged { label: String, weight: f64 }\n\
         fn main() {\n    let t = shapes::make()\n    let n: i64 = t.label.len()\n    println(f\"{n} {t.weight}\")\n}\n",
    );
    assert!(declared.is_empty(), "{declared:#?}");

    let undeclared = diagnostics(
        "use shapes\nfn main() {\n    let t = shapes::make()\n    println(f\"{t.label}\")\n}\n",
    );
    assert!(
        undeclared.iter().any(|e| matches!(
            e,
            TypeError::BindingStructUndeclared { binding, name }
                if binding == "shapes::make" && name == "Tagged"
        )),
        "{undeclared:#?}"
    );
}
