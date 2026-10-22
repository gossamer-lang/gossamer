//! Phase 0 ergonomic regression - the `name: <ident>` form and doc-comment
//! capture.

use gossamer_binding::register_module;

register_module!(
    name: ergo,
    doc: "Phase-0 ergonomic test module.",

    /// Uppercase the input.
    /// Multi-line docs collapse via newline-separation.
    fn shout(s: String) -> String {
        s.to_uppercase()
    }

    /// Returns the sum of every element.
    fn sum(xs: Vec<i64>) -> i64 {
        xs.iter().sum()
    }

    // Item with no doc-comment must still register with empty doc.
    fn no_doc() -> i64 {
        0
    }
);

#[test]
fn new_form_registers_under_path_derived_from_name() {
    let modules = gossamer_binding::modules();
    let ergo = modules
        .iter()
        .find(|m| m.path == "ergo")
        .expect("`ergo` module not registered");
    assert_eq!(ergo.items.len(), 3);
    let names: Vec<&str> = ergo.items.iter().map(|i| i.name).collect();
    assert!(names.contains(&"shout"));
    assert!(names.contains(&"sum"));
    assert!(names.contains(&"no_doc"));
}

#[test]
fn doc_comments_flow_through_to_itemfn() {
    let modules = gossamer_binding::modules();
    let ergo = modules
        .iter()
        .find(|m| m.path == "ergo")
        .expect("`ergo` module");
    let shout = ergo.items.iter().find(|i| i.name == "shout").unwrap();
    assert!(
        shout.doc.contains("Uppercase"),
        "expected doc to flow through, got: {:?}",
        shout.doc
    );
    let no_doc = ergo.items.iter().find(|i| i.name == "no_doc").unwrap();
    assert_eq!(no_doc.doc, "");
}

#[test]
fn each_module_keeps_a_force_link_hook() {
    // A crate that once called its module's `force_link()` from a linker
    // hook still compiles, and the hook references the module.
    __gos_ergo::force_link();
    assert!(gossamer_binding::module("ergo").is_some());
}
