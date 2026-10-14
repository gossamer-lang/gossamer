//! `cargo xtask schema`: the frontend blob's schema, traced from the serde
//! types the cache encodes, so code written in another language - the
//! Gossamer parser benchmarks - generates its structures and encoder from
//! data instead of by hand.

use std::fs;

use std::collections::BTreeSet;
use std::fmt::Write as _;

use anyhow::{Context, Result, anyhow};
use gossamer_driver::CachedFrontend;
use serde_reflection::{
    ContainerFormat, Format, Named, Registry, Samples, Tracer, TracerConfig, VariantFormat,
};

/// Where the schema lives, relative to the workspace root.
const SCHEMA_PATH: &str = "benchmarks/frontend_parser/schema/blob.json";

/// Where the generated AST and encoder of the parser benchmarks live.
const AST_PATHS: [&str; 2] = [
    "benchmarks/frontend_parser/idiomatic/src/ast.gos",
    "benchmarks/frontend_parser/flat/src/ast.gos",
];

/// Writes the schema, or with `check` fails when the written one is stale.
///
/// # Errors
///
/// A type the tracer cannot read, or a stale or unwritable schema file.
pub(crate) fn export(check: bool) -> Result<()> {
    let root = super::locate_workspace_root()?;
    let registry = trace()?;
    let ast = render_gossamer_ast(&registry, "SourceFile")?;
    let outputs = [
        (SCHEMA_PATH, render_schema(&registry)?),
        (AST_PATHS[0], ast.clone()),
        (AST_PATHS[1], ast),
    ];
    for (relative, rendered) in outputs {
        let path = root.join(relative);
        if check {
            let written = fs::read_to_string(&path).unwrap_or_default();
            if written != rendered {
                return Err(anyhow!("{relative} is stale; run `cargo xtask schema`"));
            }
            println!("xtask: {relative} is in sync");
            continue;
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&path, rendered).with_context(|| format!("writing {}", path.display()))?;
        println!("xtask: wrote {relative}");
    }
    Ok(())
}

/// Traces every variant of each enum the blob holds.
fn trace_enums(tracer: &mut Tracer, samples: &Samples) -> Result<()> {
    macro_rules! each {
        ($($ty:ty),* $(,)?) => {
            $(
                tracer
                    .trace_type::<$ty>(samples)
                    .map_err(|e| anyhow!("tracing {}: {e}", stringify!($ty)))?;
            )*
        };
    }
    each!(
        gossamer_ast::ArrayExpr,
        gossamer_ast::AssignOp,
        gossamer_ast::BinaryOp,
        gossamer_ast::BlockKind,
        gossamer_ast::FieldSelector,
        gossamer_ast::FnTypeKind,
        gossamer_ast::ImplItem,
        gossamer_ast::ModBody,
        gossamer_ast::Mutability,
        gossamer_ast::RangeKind,
        gossamer_ast::SelectOp,
        gossamer_ast::StructBody,
        gossamer_ast::expr::StructExprSyntax,
        gossamer_ast::TraitItem,
        gossamer_ast::UnaryOp,
        gossamer_resolve::FloatWidth,
        gossamer_resolve::IntWidth,
        gossamer_resolve::PrimitiveTy,
        gossamer_types::ArrayLen,
        gossamer_types::ClosureKind,
        gossamer_types::FloatTy,
        gossamer_types::IntTy,
        gossamer_types::Mutbl,
        gossamer_ast::ExprKind,
        gossamer_ast::FnParam,
        gossamer_ast::GenericArg,
        gossamer_ast::GenericParam,
        gossamer_ast::ItemKind,
        gossamer_ast::Literal,
        gossamer_ast::PatternKind,
        gossamer_ast::Receiver,
        gossamer_ast::StmtKind,
        gossamer_ast::TypeKind,
        gossamer_ast::UseTarget,
        gossamer_ast::Visibility,
        gossamer_diagnostics::Severity,
        gossamer_resolve::DefKind,
        gossamer_resolve::Resolution,
        gossamer_types::ConstGenericArg,
        gossamer_types::GenericArg,
        gossamer_types::TyKind,
    );
    Ok(())
}

/// The traced registry of every container the blob reaches.
fn trace() -> Result<Registry> {
    // A diagnostic code reads back only as one the registry knows, so the
    // tracer's stand-in for a string is a real code.
    let code = gossamer_diagnostics::code_registry::codes()
        .next()
        .ok_or_else(|| anyhow!("the diagnostic code registry is empty"))?;
    let config = TracerConfig::default()
        .default_borrowed_str_value(code)
        .default_string_value(code.to_string());
    let mut tracer = Tracer::new(config);
    let samples = Samples::new();
    tracer
        .trace_type::<CachedFrontend>(&samples)
        .map_err(|e| anyhow!("tracing the blob: {e}"))?;
    // One pass over the root meets one variant of each enum; tracing an enum
    // on its own walks every variant. An enum missing here is named by the
    // error `registry` answers.
    trace_enums(&mut tracer, &samples)?;
    tracer
        .registry()
        .map_err(|e| anyhow!("completing the schema: {e}"))
}

/// The schema as JSON: the root container, then every container the blob
/// reaches, by name. postcard encodes exactly this shape: fields in order,
/// variants by index, sequences and maps with a varint length.
fn render_schema(registry: &Registry) -> Result<String> {
    let schema = serde_json::json!({
        "root": "CachedFrontend",
        "encoding": "postcard",
        "containers": registry,
    });
    let mut text = serde_json::to_string_pretty(&schema)?;
    text.push('\n');
    Ok(text)
}

/// Every container `root` reaches, by name.
fn reachable(registry: &Registry, root: &str) -> BTreeSet<String> {
    fn format(f: &Format, registry: &Registry, seen: &mut BTreeSet<String>) {
        match f {
            Format::TypeName(name) => container(name, registry, seen),
            Format::Option(inner) | Format::Seq(inner) => format(inner, registry, seen),
            Format::Map { key, value } => {
                format(key, registry, seen);
                format(value, registry, seen);
            }
            Format::Tuple(items) => items.iter().for_each(|i| format(i, registry, seen)),
            Format::TupleArray { content, .. } => format(content, registry, seen),
            _ => {}
        }
    }
    fn container(name: &str, registry: &Registry, seen: &mut BTreeSet<String>) {
        if !seen.insert(name.to_string()) {
            return;
        }
        match registry.get(name) {
            Some(ContainerFormat::NewTypeStruct(inner)) => format(inner, registry, seen),
            Some(ContainerFormat::TupleStruct(items)) => {
                for item in items {
                    format(item, registry, seen);
                }
            }
            Some(ContainerFormat::Struct(fields)) => {
                for field in fields {
                    format(&field.value, registry, seen);
                }
            }
            Some(ContainerFormat::Enum(variants)) => {
                for variant in variants.values() {
                    match &variant.value {
                        VariantFormat::NewType(inner) => format(inner, registry, seen),
                        VariantFormat::Tuple(items) => {
                            for item in items {
                                format(item, registry, seen);
                            }
                        }
                        VariantFormat::Struct(fields) => {
                            for field in fields {
                                format(&field.value, registry, seen);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let mut seen = BTreeSet::new();
    container(root, registry, &mut seen);
    seen
}

/// The Gossamer type a field of format `f` has.
fn gossamer_type(f: &Format) -> Result<String> {
    Ok(match f {
        Format::TypeName(name) => name.clone(),
        Format::Unit => "()".to_string(),
        Format::Bool => "bool".to_string(),
        Format::U8 => "u8".to_string(),
        Format::U16 => "u16".to_string(),
        Format::U32 => "u32".to_string(),
        Format::U64 => "u64".to_string(),
        Format::I8 => "i8".to_string(),
        Format::I16 => "i16".to_string(),
        Format::I32 => "i32".to_string(),
        Format::I64 => "i64".to_string(),
        Format::F64 => "f64".to_string(),
        Format::Char => "char".to_string(),
        Format::Str => "String".to_string(),
        Format::Option(inner) => format!("Option<{}>", gossamer_type(inner)?),
        Format::Seq(inner) => format!("Vec<{}>", gossamer_type(inner)?),
        Format::Map { key, value } => {
            format!("Map<{}, {}>", gossamer_type(key)?, gossamer_type(value)?)
        }
        Format::Tuple(items) => format!(
            "({})",
            items
                .iter()
                .map(gossamer_type)
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        ),
        other => return Err(anyhow!("no Gossamer type for blob format {other:?}")),
    })
}

/// The Gossamer name of variant `variant` of enum `owner`. A module's enum
/// variants share its namespace with its types, so a variant spelled like a
/// type the module declares takes the enum's stem as a prefix
/// (`ExprKind::Block` is `ExprKind::ExprBlock`). postcard writes a variant
/// as its index, so the name never reaches the blob.
fn variant_name(owner: &str, variant: &str, types: &BTreeSet<String>) -> String {
    if types.contains(variant) {
        format!("{}{variant}", owner.strip_suffix("Kind").unwrap_or(owner))
    } else {
        variant.to_string()
    }
}

/// `snake_case` of an `UpperCamelCase` name.
fn snake(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Gossamer statements writing the value `value` of format `f` to `out`,
/// indented by `indent`; `fresh` numbers the names nested values bind.
fn encode_stmts(f: &Format, value: &str, indent: &str, fresh: &mut usize) -> Result<String> {
    let mut next = || {
        *fresh += 1;
        format!("e{fresh}")
    };
    Ok(match f {
        Format::TypeName(name) => format!("{indent}encode_{}(out, {value})\n", snake(name)),
        Format::Unit => String::new(),
        Format::Bool => format!("{indent}out.push(if {value} {{ 1 }} else {{ 0 }})\n"),
        Format::U8 => format!("{indent}out.push({value})\n"),
        Format::U16 | Format::U32 | Format::U64 => {
            format!("{indent}put_varint(out, {value} as u64)\n")
        }
        Format::I8 | Format::I16 | Format::I32 | Format::I64 => {
            format!("{indent}put_zigzag(out, {value} as i64)\n")
        }
        Format::Char => format!("{indent}put_str(out, {value}.to_string())\n"),
        Format::Str => format!("{indent}put_str(out, {value})\n"),
        Format::Option(inner) => {
            let x = next();
            let body = encode_stmts(inner, &x, &format!("{indent}        "), fresh)?;
            format!(
                "{indent}match {value} {{\n{indent}    Some({x}) => {{\n{indent}        out.push(1)\n{body}{indent}    }}\n{indent}    None => out.push(0)\n{indent}}}\n"
            )
        }
        Format::Seq(inner) => {
            let x = next();
            let body = encode_stmts(inner, &x, &format!("{indent}    "), fresh)?;
            format!(
                "{indent}put_varint(out, {value}.len() as u64)\n{indent}for {x} in {value} {{\n{body}{indent}}}\n"
            )
        }
        Format::Map { key, value: v } => {
            // A map is written in key order, as `gossamer_ast::ordered`
            // writes the Rust one.
            let keys = next();
            let k = next();
            let body_k = encode_stmts(key, &k, &format!("{indent}    "), fresh)?;
            let body_v =
                encode_stmts(v, &format!("{value}[{k}]"), &format!("{indent}    "), fresh)?;
            format!(
                "{indent}let mut {keys} = {value}.keys()\n{indent}{keys}.sort()\n{indent}put_varint(out, {keys}.len() as u64)\n{indent}for {k} in {keys} {{\n{body_k}{body_v}{indent}}}\n"
            )
        }
        Format::Tuple(items) => {
            let names: Vec<String> = items.iter().map(|_| next()).collect();
            let mut out = format!("{indent}let {} = {value}\n", names.join(", "));
            for (item, name) in items.iter().zip(&names) {
                out.push_str(&encode_stmts(item, name, indent, fresh)?);
            }
            out
        }
        other => return Err(anyhow!("no encoder for blob format {other:?}")),
    })
}

/// The Gossamer declarations and postcard encoder of every container
/// `root` reaches: one type per container, named and shaped as the Rust
/// one, and one `encode_<name>(out, value)` writing it as the Rust frontend
/// writes it.
/// Appends the Gossamer declaration and encoder of the enum `name`.
fn render_enum(
    text: &mut String,
    name: &str,
    variants: &std::collections::BTreeMap<u32, Named<VariantFormat>>,
    names: &BTreeSet<String>,
    fresh: &mut usize,
) -> Result<()> {
    writeln!(text, "pub enum {name} {{")?;
    for variant in variants.values() {
        let v = &variant_name(name, &variant.name, names);
        match &variant.value {
            VariantFormat::Unit => writeln!(text, "    {v},")?,
            VariantFormat::NewType(inner) => {
                writeln!(text, "    {v}({}),", gossamer_type(inner)?)?;
            }
            VariantFormat::Tuple(items) => {
                let tys = items
                    .iter()
                    .map(gossamer_type)
                    .collect::<Result<Vec<_>>>()?;
                writeln!(text, "    {v}({}),", tys.join(", "))?;
            }
            VariantFormat::Struct(fields) => {
                let tys = fields
                    .iter()
                    .map(|f| Ok(format!("{}: {}", f.name, gossamer_type(&f.value)?)))
                    .collect::<Result<Vec<_>>>()?;
                writeln!(text, "    {v} {{ {} }},", tys.join(", "))?;
            }
            VariantFormat::Variable(_) => {
                return Err(anyhow!("`{name}::{v}` has no traced shape"));
            }
        }
    }
    text.push_str("}\n\n");
    writeln!(
        text,
        "pub fn encode_{}(out: &mut Vec<u8>, v: {name}) {{",
        snake(name)
    )?;
    text.push_str("    match v {\n");
    for (index, variant) in variants {
        let v = &variant_name(name, &variant.name, names);
        match &variant.value {
            VariantFormat::Unit => {
                writeln!(text, "        {name}::{v} => put_varint(out, {index}),")?;
            }
            VariantFormat::NewType(inner) => {
                writeln!(text, "        {name}::{v}(f0) => {{")?;
                writeln!(text, "            put_varint(out, {index})")?;
                text.push_str(&encode_stmts(inner, "f0", "            ", fresh)?);
                text.push_str("        }\n");
            }
            VariantFormat::Tuple(items) => {
                let binds: Vec<String> = (0..items.len()).map(|i| format!("f{i}")).collect();
                writeln!(text, "        {name}::{v}({}) => {{", binds.join(", "))?;
                writeln!(text, "            put_varint(out, {index})")?;
                for (item, bind) in items.iter().zip(&binds) {
                    text.push_str(&encode_stmts(item, bind, "            ", fresh)?);
                }
                text.push_str("        }\n");
            }
            VariantFormat::Struct(fields) => {
                let binds: Vec<String> = fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| format!("{}: f{i}", f.name))
                    .collect();
                writeln!(text, "        {name}::{v} {{ {} }} => {{", binds.join(", "))?;
                writeln!(text, "            put_varint(out, {index})")?;
                for (i, f) in fields.iter().enumerate() {
                    text.push_str(&encode_stmts(
                        &f.value,
                        &format!("f{i}"),
                        "            ",
                        fresh,
                    )?);
                }
                text.push_str("        }\n");
            }
            VariantFormat::Variable(_) => {
                return Err(anyhow!("`{name}::{v}` has no traced shape"));
            }
        }
    }
    text.push_str("    }\n}\n");
    Ok(())
}

fn render_gossamer_ast(registry: &Registry, root: &str) -> Result<String> {
    let names = reachable(registry, root);
    let mut text = String::from(
        "// Generated by `cargo xtask schema` from the serde types the frontend blob\n\
         // encodes; change those, not this file. Each type is named and shaped as\n\
         // its Rust counterpart, and `encode_<name>` writes one as postcard does:\n\
         // fields in order, a variant as its index, a length before every\n\
         // sequence or map, a map in key order.\n\n\
         /// Appends `v` as an unsigned LEB128 varint.\n\
         pub fn put_varint(out: &mut Vec<u8>, v: u64) {\n    let mut rest = v\n    while rest >= 128 {\n        out.push((rest & 127 | 128) as u8)\n        rest = rest >> 7\n    }\n    out.push(rest as u8)\n}\n\n\
         /// Appends `v` zigzag-encoded as a varint.\n\
         pub fn put_zigzag(out: &mut Vec<u8>, v: i64) {\n    put_varint(out, ((v << 1) ^ (v >> 63)) as u64)\n}\n\n\
         /// Appends `s` as its byte length and its bytes.\n\
         pub fn put_str(out: &mut Vec<u8>, s: String) {\n    put_varint(out, s.byte_len() as u64)\n    out.extend_from_slice(s.as_bytes())\n}\n",
    );
    for name in &names {
        let container = registry
            .get(name)
            .ok_or_else(|| anyhow!("the schema names `{name}` but does not define it"))?;
        text.push('\n');
        let mut fresh = 0usize;
        match container {
            ContainerFormat::UnitStruct => {
                writeln!(text, "pub struct {name}\n")?;
                writeln!(
                    text,
                    "pub fn encode_{}(out: &mut Vec<u8>, v: {name}) {{\n}}",
                    snake(name)
                )?;
            }
            ContainerFormat::NewTypeStruct(inner) => {
                writeln!(text, "pub struct {name}(pub {})\n", gossamer_type(inner)?)?;
                writeln!(
                    text,
                    "pub fn encode_{}(out: &mut Vec<u8>, v: {name}) {{",
                    snake(name)
                )?;
                text.push_str(&encode_stmts(inner, "v.0", "    ", &mut fresh)?);
                text.push_str("}\n");
            }
            ContainerFormat::TupleStruct(items) => {
                let tys = items
                    .iter()
                    .map(gossamer_type)
                    .collect::<Result<Vec<_>>>()?;
                let fields: Vec<String> = tys.iter().map(|t| format!("pub {t}")).collect();
                writeln!(text, "pub struct {name}({})\n", fields.join(", "))?;
                writeln!(
                    text,
                    "pub fn encode_{}(out: &mut Vec<u8>, v: {name}) {{",
                    snake(name)
                )?;
                for (i, item) in items.iter().enumerate() {
                    text.push_str(&encode_stmts(item, &format!("v.{i}"), "    ", &mut fresh)?);
                }
                text.push_str("}\n");
            }
            ContainerFormat::Struct(fields) => {
                writeln!(text, "pub struct {name} {{")?;
                for Named { name: field, value } in fields {
                    writeln!(text, "    pub {field}: {}", gossamer_type(value)?)?;
                }
                text.push_str("}\n\n");
                writeln!(
                    text,
                    "pub fn encode_{}(out: &mut Vec<u8>, v: {name}) {{",
                    snake(name)
                )?;
                for Named { name: field, value } in fields {
                    text.push_str(&encode_stmts(
                        value,
                        &format!("v.{field}"),
                        "    ",
                        &mut fresh,
                    )?);
                }
                text.push_str("}\n");
            }
            ContainerFormat::Enum(variants) => {
                render_enum(&mut text, name, variants, &names, &mut fresh)?;
            }
        }
    }
    Ok(text)
}
