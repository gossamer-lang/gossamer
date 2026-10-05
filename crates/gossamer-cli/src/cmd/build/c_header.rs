//! The C header a library build writes beside its artifacts: a prototype for
//! every `#[export]` function and a definition of every `#[repr(C)]` struct
//! those prototypes reach.
//!
//! The checker has already accepted each exported signature, so every type
//! here is one the C ABI carries; the header spells it as C does.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use gossamer_ast::{
    Expr, ExprKind, FnDecl, FnParam, GenericArg, Item, ItemKind, Literal, ModBody, PatternKind,
    SourceFile, StructBody, Type, TypeKind,
};

/// One `#[export]` function, as the header and the link need it.
#[derive(Debug, Clone)]
pub(super) struct Export {
    /// The C symbol.
    pub(super) symbol: String,
    decl: FnDecl,
}

/// The `#[export]` functions of `sf`, in declaration order.
pub(super) fn exports(sf: &SourceFile) -> Vec<Export> {
    let mut found = Vec::new();
    collect_exports(&sf.items, &mut found);
    found
}

fn collect_exports(items: &[Item], found: &mut Vec<Export>) {
    for item in items {
        match &item.kind {
            ItemKind::Fn(decl) if decl.extern_abi.is_none() => {
                if let Some(symbol) = item.attrs.export_symbol(&decl.name.name) {
                    found.push(Export {
                        symbol,
                        decl: decl.clone(),
                    });
                }
            }
            ItemKind::Mod(module) => {
                if let ModBody::Inline(inner) = &module.body {
                    collect_exports(inner, found);
                }
            }
            _ => {}
        }
    }
}

/// Whether `sf` declares a top-level `fn main`.
pub(super) fn declares_main(sf: &SourceFile) -> bool {
    sf.items
        .iter()
        .any(|item| matches!(&item.kind, ItemKind::Fn(decl) if decl.name.name == "main"))
}

/// The declarations a header can name, by Gossamer name.
#[derive(Default)]
struct Declarations<'a> {
    structs: HashMap<&'a str, &'a gossamer_ast::StructDecl>,
    aliases: HashMap<&'a str, &'a Type>,
    consts: HashMap<&'a str, &'a Expr>,
}

impl<'a> Declarations<'a> {
    fn collect(&mut self, items: &'a [Item]) {
        for item in items {
            match &item.kind {
                ItemKind::Struct(decl) if item.attrs.lists_argument("repr", "C") => {
                    self.structs.insert(decl.name.name.as_str(), decl);
                }
                ItemKind::TypeAlias(decl) if decl.generics.params.is_empty() => {
                    self.aliases.insert(decl.name.name.as_str(), &decl.ty);
                }
                ItemKind::Const(decl) => {
                    self.consts.insert(decl.name.name.as_str(), &decl.value);
                }
                ItemKind::Mod(module) => {
                    if let ModBody::Inline(inner) = &module.body {
                        self.collect(inner);
                    }
                }
                _ => {}
            }
        }
    }
}

/// The header text for the library `name` exporting `exports`.
pub(super) fn render(sf: &SourceFile, exports: &[Export], name: &str) -> String {
    let mut decls = Declarations::default();
    decls.collect(&sf.items);
    let mut writer = Writer {
        decls,
        defined: HashSet::new(),
        types: String::new(),
        unions: 0,
    };
    let mut prototypes = String::new();
    for export in exports {
        let ret = match &export.decl.ret {
            Some(ret) => writer.c_type(ret),
            None => "void".to_string(),
        };
        let mut params = Vec::new();
        for (index, param) in export.decl.params.iter().enumerate() {
            let FnParam::Typed { pattern, ty, .. } = param else {
                continue;
            };
            let param_name = match &pattern.kind {
                PatternKind::Ident { name, .. } => name.name.clone(),
                _ => format!("arg{index}"),
            };
            params.push(format!("{} {param_name}", writer.c_type(ty)));
        }
        let params = if params.is_empty() {
            "void".to_string()
        } else {
            params.join(", ")
        };
        writeln!(prototypes, "{ret} {}({params});", export.symbol).unwrap();
    }
    writeln!(
        prototypes,
        "\n/* Runs what `runtime::at_exit` registered; call it before the process exits. */\nvoid {name}_shutdown(void);"
    )
    .unwrap();
    let guard: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let mut out = String::new();
    writeln!(
        out,
        "/* The C interface of the Gossamer library `{name}`, written by `gos build`. */"
    )
    .unwrap();
    writeln!(out, "#ifndef GOS_{guard}_H").unwrap();
    writeln!(out, "#define GOS_{guard}_H\n").unwrap();
    writeln!(out, "#include <stdbool.h>").unwrap();
    writeln!(out, "#include <stddef.h>").unwrap();
    writeln!(out, "#include <stdint.h>\n").unwrap();
    writeln!(out, "#ifdef __cplusplus\nextern \"C\" {{\n#endif\n").unwrap();
    if !writer.types.is_empty() {
        out.push_str(&writer.types);
        out.push('\n');
    }
    out.push_str(&prototypes);
    writeln!(out, "\n#ifdef __cplusplus\n}}\n#endif\n").unwrap();
    writeln!(out, "#endif").unwrap();
    out
}

struct Writer<'a> {
    decls: Declarations<'a>,
    /// Structs whose definition `types` already holds.
    defined: HashSet<String>,
    /// Struct and union definitions, each after the ones it names.
    types: String,
    unions: usize,
}

impl Writer<'_> {
    /// The C spelling of `ty` in a declaration, defining any struct it
    /// names first.
    fn c_type(&mut self, ty: &Type) -> String {
        match &ty.kind {
            TypeKind::Unit | TypeKind::Never => "void".to_string(),
            TypeKind::Path(path) => {
                let Some(last) = path.segments.last() else {
                    return "void".to_string();
                };
                let name = last.name.name.as_str();
                let name = name.strip_prefix("__gos_ffi_").unwrap_or(name);
                let first_type_arg = || {
                    last.generics.iter().find_map(|arg| match arg {
                        GenericArg::Type(inner) => Some(inner),
                        GenericArg::Const(_) => None,
                    })
                };
                match name {
                    "Ptr" => match first_type_arg() {
                        Some(inner) => format!("{} *", self.c_type(inner)),
                        None => "void *".to_string(),
                    },
                    "Option" => match first_type_arg() {
                        Some(inner) => self.c_type(inner),
                        None => "void *".to_string(),
                    },
                    "Union" => match first_type_arg() {
                        Some(members) => self.union_type(members),
                        None => "void".to_string(),
                    },
                    other => self.named_type(other),
                }
            }
            TypeKind::Tuple(_)
            | TypeKind::Infer
            | TypeKind::Array { .. }
            | TypeKind::Slice(_)
            | TypeKind::Ref { .. }
            | TypeKind::Fn { .. } => "void *".to_string(),
        }
    }

    fn named_type(&mut self, name: &str) -> String {
        let scalar = match name {
            "i8" | "c_schar" => "int8_t",
            "i16" | "c_short" => "int16_t",
            "i32" => "int32_t",
            "i64" => "int64_t",
            "u8" | "c_uchar" => "uint8_t",
            "u16" | "c_ushort" => "uint16_t",
            "u32" => "uint32_t",
            "u64" => "uint64_t",
            "isize" => "intptr_t",
            "usize" => "uintptr_t",
            "f32" | "c_float" => "float",
            "f64" | "c_double" => "double",
            "bool" => "bool",
            "char" => "uint32_t",
            "c_char" => "char",
            "c_int" => "int",
            "c_uint" => "unsigned int",
            "c_long" => "long",
            "c_ulong" => "unsigned long",
            "c_longlong" => "long long",
            "c_ulonglong" => "unsigned long long",
            "size_t" => "size_t",
            "ssize_t" => "ptrdiff_t",
            "c_void" => "void",
            _ => "",
        };
        if !scalar.is_empty() {
            return scalar.to_string();
        }
        if let Some(target) = self.decls.aliases.get(name).copied() {
            return self.c_type(target);
        }
        if self.decls.structs.contains_key(name) {
            self.define_struct(name);
            return name.to_string();
        }
        "void".to_string()
    }

    /// Writes `typedef struct name { .. } name;` once, after the structs its
    /// fields name.
    fn define_struct(&mut self, name: &str) {
        if !self.defined.insert(name.to_string()) {
            return;
        }
        let Some(decl) = self.decls.structs.get(name).copied() else {
            return;
        };
        let fields: Vec<(String, &Type)> = match &decl.body {
            StructBody::Named(fields) => fields
                .iter()
                .map(|field| (field.name.name.clone(), &field.ty))
                .collect(),
            StructBody::Tuple(fields) => fields
                .iter()
                .enumerate()
                .map(|(index, field)| (format!("f{index}"), &field.ty))
                .collect(),
            StructBody::Unit => Vec::new(),
        };
        let mut body = String::new();
        for (field, ty) in fields {
            let line = self.field(&field, ty);
            writeln!(body, "    {line};").unwrap();
        }
        writeln!(self.types, "typedef struct {name} {{\n{body}}} {name};\n").unwrap();
    }

    /// One member declaration: `T name`, or `T name[N]` for an array.
    fn field(&mut self, field: &str, ty: &Type) -> String {
        if let TypeKind::Array { elem, len } = &ty.kind {
            let elem_ty = self.c_type(elem);
            let len = self.array_len(len).map_or(String::new(), |n| n.to_string());
            return format!("{elem_ty} {field}[{len}]");
        }
        format!("{} {field}", self.c_type(ty))
    }

    /// `typedef union { .. } <name>;` for the member tuple `members`.
    fn union_type(&mut self, members: &Type) -> String {
        let list: Vec<&Type> = match &members.kind {
            TypeKind::Tuple(items) => items.iter().collect(),
            _ => vec![members],
        };
        let mut body = String::new();
        for (index, member) in list.into_iter().enumerate() {
            let line = self.field(&format!("m{index}"), member);
            writeln!(body, "    {line};").unwrap();
        }
        let name = format!("gos_union{}", self.unions);
        self.unions += 1;
        writeln!(self.types, "typedef union {{\n{body}}} {name};\n").unwrap();
        name
    }

    /// The value of an array length: an integer literal or a constant
    /// holding one.
    fn array_len(&self, len: &Expr) -> Option<u64> {
        match &len.kind {
            ExprKind::Literal(Literal::Int(text)) => {
                let digits: String = text
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '_')
                    .filter(|c| *c != '_')
                    .collect();
                digits.parse().ok()
            }
            ExprKind::Path(path) => {
                let name = path.segments.last()?.name.name.as_str();
                let value = self.decls.consts.get(name).copied()?;
                self.array_len(value)
            }
            _ => None,
        }
    }
}
