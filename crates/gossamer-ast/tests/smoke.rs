//! Smoke tests that build small ASTs by hand and check `Display` output.

#![forbid(unsafe_code)]

mod common;

use common::{
    block_expr, call_expr, expr_stmt, fn_item, literal_string, path_expr, source_file,
    use_decl_module,
};

use gossamer_ast::{
    Attrs, ConstDecl, EnumDecl, EnumVariant, Generics, Ident, Item, ItemKind, NodeId, StructBody,
    StructDecl, StructField, Type, TypeKind, TypePath, Visibility, WhereClause,
};
use gossamer_lex::{FileId, SourceMap, Span};

fn span() -> Span {
    let mut map = SourceMap::new();
    let file: FileId = map.add_file("smoke", "");
    Span::new(file, 0, 0)
}

fn public_item(kind: ItemKind) -> Item {
    Item::new(
        NodeId::DUMMY,
        span(),
        Attrs::default(),
        Visibility::Public,
        kind,
    )
}

fn make_i32_type() -> Type {
    Type::new(
        NodeId::DUMMY,
        span(),
        TypeKind::Path(TypePath::single("i32")),
    )
}

#[test]
fn hello_world_display_contains_fn_main_and_println_call() {
    let println_path = path_expr(&["fmt", "println"]);
    let call = call_expr(println_path, vec![literal_string("hello, world")]);
    let body = block_expr(vec![expr_stmt(call, false)], None);
    let item = fn_item("main", body);
    let source = source_file(vec![use_decl_module(&["fmt"])], vec![item]);

    let rendered = source.to_string();
    assert!(
        rendered.contains("use fmt\n"),
        "rendered output missing use: {rendered}"
    );
    assert!(
        rendered.contains("fn main() {"),
        "missing fn main: {rendered}"
    );
    assert!(
        rendered.contains("fmt::println(\"hello, world\")"),
        "missing call: {rendered}"
    );
}

#[test]
fn struct_decl_renders_named_fields_with_trailing_commas() {
    let fields = vec![
        StructField {
            attrs: Attrs::default(),
            visibility: Visibility::Inherited,
            name: Ident::new("path"),
            ty: Type::new(
                NodeId::DUMMY,
                span(),
                TypeKind::Path(TypePath::single("String")),
            ),
            default: None,
        },
        StructField {
            attrs: Attrs::default(),
            visibility: Visibility::Inherited,
            name: Ident::new("lines"),
            ty: Type::new(
                NodeId::DUMMY,
                span(),
                TypeKind::Path(TypePath::single("u64")),
            ),
            default: None,
        },
    ];
    let decl = StructDecl {
        name: Ident::new("FileCount"),
        generics: Generics::default(),
        where_clause: WhereClause::default(),
        body: StructBody::Named(fields),
        traits: Vec::new(),
        items: Vec::new(),
    };
    let item = public_item(ItemKind::Struct(decl));
    let source = source_file(vec![], vec![item]);
    let rendered = source.to_string();

    assert!(rendered.contains("pub struct FileCount {"));
    // Fields render with their colons column-aligned: `path` is
    // padded to match `lines` (the longest name).
    assert!(
        rendered.contains("    path : String,\n"),
        "rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("    lines: u64,\n"),
        "rendered:\n{rendered}"
    );
}

#[test]
fn struct_decl_aligns_three_field_colons_in_column() {
    use gossamer_ast::{
        Generics, Ident, ItemKind, NodeId, StructBody, StructDecl, StructField, Type, TypeKind,
        TypePath, Visibility, WhereClause,
    };
    let names = [("x", "i64"), ("item", "String"), ("y", "i64")];
    let fields: Vec<StructField> = names
        .iter()
        .map(|(n, t)| StructField {
            attrs: gossamer_ast::Attrs::default(),
            visibility: Visibility::Inherited,
            name: Ident::new(*n),
            ty: Type::new(NodeId::DUMMY, span(), TypeKind::Path(TypePath::single(*t))),
            default: None,
        })
        .collect();
    let decl = StructDecl {
        name: Ident::new("Row"),
        generics: Generics::default(),
        where_clause: WhereClause::default(),
        body: StructBody::Named(fields),
        traits: Vec::new(),
        items: Vec::new(),
    };
    let item = public_item(ItemKind::Struct(decl));
    let source = source_file(vec![], vec![item]);
    let rendered = source.to_string();
    assert!(
        rendered.contains("    x   : i64,\n"),
        "rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("    item: String,\n"),
        "rendered:\n{rendered}"
    );
    assert!(
        rendered.contains("    y   : i64,\n"),
        "rendered:\n{rendered}"
    );
}

#[test]
fn use_decl_with_brace_list_renders_correctly() {
    use gossamer_ast::UseListEntry;

    let decl = common::use_decl_module_with_list(
        &["std", "channel"],
        vec![
            UseListEntry::simple("channel"),
            UseListEntry::simple("Sender"),
        ],
    );
    let source = source_file(vec![decl], vec![]);
    let rendered = source.to_string();
    assert!(
        rendered.contains("use std::channel::{channel, Sender}"),
        "rendered: {rendered}"
    );
}

#[test]
fn const_decl_renders_name_type_and_value() {
    let value = gossamer_ast::Expr::new(
        NodeId::DUMMY,
        span(),
        gossamer_ast::ExprKind::Literal(gossamer_ast::Literal::Int("42".into())),
    );
    let decl = ConstDecl {
        name: Ident::new("ANSWER"),
        ty: make_i32_type(),
        value,
    };
    let item = public_item(ItemKind::Const(decl));
    let rendered = source_file(vec![], vec![item]).to_string();
    assert!(
        rendered.contains("pub const ANSWER: i32 = 42;"),
        "rendered: {rendered}"
    );
}

#[test]
fn enum_decl_with_unit_variants_renders_block() {
    let decl = EnumDecl {
        name: Ident::new("Direction"),
        generics: Generics::default(),
        where_clause: WhereClause::default(),
        repr: gossamer_ast::EnumRepr::default(),
        traits: Vec::new(),
        items: Vec::new(),
        variants: vec![
            EnumVariant {
                attrs: Attrs::default(),
                name: Ident::new("North"),
                body: StructBody::Unit,
                discriminant: None,
            },
            EnumVariant {
                attrs: Attrs::default(),
                name: Ident::new("South"),
                body: StructBody::Unit,
                discriminant: None,
            },
        ],
    };
    let item = public_item(ItemKind::Enum(decl));
    let rendered = source_file(vec![], vec![item]).to_string();
    assert!(rendered.contains("pub enum Direction {"));
    assert!(rendered.contains("    North,\n"));
    assert!(rendered.contains("    South,\n"));
}

#[test]
fn empty_fn_renders_with_empty_block() {
    let body = block_expr(vec![], None);
    let item = fn_item("nothing", body);
    let rendered = source_file(vec![], vec![item]).to_string();
    assert!(rendered.contains("fn nothing() {}"));
}
