//! `gos bindgen --c header.h`: Gossamer declarations for a C header.
//!
//! The header is read by clang (`-Xclang -ast-dump=json`, with `-dM` for
//! its macros), so layouts and types are the target's own; no library is
//! linked into the toolchain for it. The output is committed source - an
//! `unsafe extern "C"` block, `#[repr(C)]` structs, `ffi::Union` aliases,
//! opaque `type`s, and constants - so a build never needs clang. Run for
//! several `--target`s, a declaration that differs between them is written
//! once per target under its `#[cfg]`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

/// What `gos bindgen --c` was asked for.
#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    /// The header to read.
    pub(crate) header: PathBuf,
    /// Targets to read it for; the host when empty.
    pub(crate) targets: Vec<String>,
    /// Glob patterns (`*` wildcards) naming the declarations to keep;
    /// without one, those in the header's own directory tree are kept.
    pub(crate) allow: Vec<String>,
    /// Further include directories.
    pub(crate) include: Vec<PathBuf>,
    /// Preprocessor definitions, `NAME` or `NAME=VALUE`.
    pub(crate) define: Vec<String>,
    /// The library the `#[link]` attribute names.
    pub(crate) link: Option<String>,
    /// Where to write the declarations; standard output when `None`.
    pub(crate) output: Option<PathBuf>,
}

/// Runs `gos bindgen --c`.
pub(crate) fn run(options: &Options) -> Result<()> {
    let clang = clang()?;
    let targets: Vec<Option<String>> = if options.targets.is_empty() {
        vec![None]
    } else {
        options.targets.iter().cloned().map(Some).collect()
    };
    let mut per_target = Vec::new();
    for target in &targets {
        let ast = clang_ast(&clang, options, target.as_deref())?;
        let macros = clang_macros(&clang, options, target.as_deref())?;
        let mut model = Model::new(options);
        model.read(&ast)?;
        model.select();
        per_target.push((target.clone(), model.render(&macros)));
    }
    let text = merge(&per_target, options);
    match &options.output {
        Some(path) => {
            std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        }
        None => print!("{text}"),
    }
    Ok(())
}

fn clang() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("GOS_CLANG") {
        return Ok(PathBuf::from(path));
    }
    gossamer_codegen_llvm::llvm_toolchain_status()
        .into_iter()
        .find(|status| status.tool == "clang")
        .and_then(|status| status.resolved.ok())
        .map(|(path, _)| path)
        .or_else(|| which::which("clang").ok())
        .ok_or_else(|| {
            anyhow!("bindgen --c reads headers with clang; none was found (set GOS_CLANG)")
        })
}

fn clang_command(clang: &Path, options: &Options, target: Option<&str>) -> Command {
    let mut cmd = Command::new(clang);
    cmd.arg("-x").arg("c").arg("-fsyntax-only");
    if let Some(target) = target {
        cmd.arg(format!("--target={target}"));
    }
    for dir in &options.include {
        cmd.arg(format!("-I{}", dir.display()));
    }
    for define in &options.define {
        cmd.arg(format!("-D{define}"));
    }
    cmd
}

fn clang_ast(clang: &Path, options: &Options, target: Option<&str>) -> Result<Value> {
    let out = clang_command(clang, options, target)
        .arg("-Xclang")
        .arg("-ast-dump=json")
        .arg(&options.header)
        .output()
        .with_context(|| format!("running {}", clang.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "clang could not read {}:\n{}",
            options.header.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    serde_json::from_slice(&out.stdout).context("reading clang's AST")
}

/// The object-like macros the header's own directory tree defines, with
/// their replacement text, read from `-E -dD`, whose line markers say
/// which file each definition is in.
fn clang_macros(
    clang: &Path,
    options: &Options,
    target: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let out = clang_command(clang, options, target)
        .arg("-E")
        .arg("-dD")
        .arg(&options.header)
        .output()
        .with_context(|| format!("running {}", clang.display()))?;
    let root = header_root(&options.header);
    let base = options
        .header
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut local = false;
    let mut macros = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(marker) = line.strip_prefix("# ") {
            // `# <line> "<file>" <flags>`.
            if let Some(file) = marker.split('"').nth(1) {
                local = !file.starts_with('<') && {
                    let path = Path::new(file);
                    let path = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        base.join(path)
                    };
                    let path = std::path::absolute(&path).unwrap_or(path);
                    std::fs::canonicalize(&path)
                        .unwrap_or(path)
                        .starts_with(&root)
                };
            }
            continue;
        }
        if !local {
            continue;
        }
        let Some(rest) = line.strip_prefix("#define ") else {
            continue;
        };
        let (name, value) = rest.split_once(' ').unwrap_or((rest, ""));
        if !name.contains('(') {
            macros.push((name.to_string(), value.trim().to_string()));
        }
    }
    Ok(macros)
}

/// A C type, as the declarations need it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CType {
    Void,
    /// A scalar, by its Gossamer spelling.
    Scalar(&'static str),
    /// A pointer, and whether what it points at is `const`.
    Pointer(Box<CType>, bool),
    Array(Box<CType>, u64),
    Function {
        ret: Box<CType>,
        params: Vec<CType>,
    },
    /// A struct, union, or typedef name this output declares.
    Named(String),
    /// A type Gossamer cannot spell (`long double`, `__int128`, a vector).
    Unsupported(String),
}

/// Where a type appears, which decides how a pointer and a function
/// pointer are spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Field,
    Param,
    Return,
}

#[derive(Debug, Clone)]
struct Field {
    name: String,
    ty: CType,
}

#[derive(Debug, Clone)]
enum Decl {
    Record {
        union: bool,
        fields: Vec<Field>,
        bitfields: bool,
    },
    Opaque,
    Alias(CType),
    Enum(Vec<(String, i64)>),
    Function {
        params: Vec<Field>,
        ret: CType,
        variadic: bool,
    },
    Static(CType),
}

/// The declarations of one target's reading of the header.
struct Model {
    root: PathBuf,
    allow: Vec<String>,
    /// Declarations in header order, by name.
    order: Vec<String>,
    decls: BTreeMap<String, Decl>,
    /// Whether each was declared in the header's own tree.
    local: BTreeMap<String, bool>,
    /// Typedef names and what they stand for, for spelling types.
    typedefs: BTreeMap<String, String>,
    selected: BTreeSet<String>,
    anonymous: usize,
    /// An unnamed record just read, which a following typedef names.
    unnamed_record: Option<Value>,
}

impl Model {
    fn new(options: &Options) -> Self {
        Self {
            root: header_root(&options.header),
            allow: options.allow.clone(),
            order: Vec::new(),
            decls: BTreeMap::new(),
            local: BTreeMap::new(),
            typedefs: BTreeMap::new(),
            selected: BTreeSet::new(),
            anonymous: 0,
            unnamed_record: None,
        }
    }

    fn insert(&mut self, name: &str, decl: Decl, local: bool) {
        if !self.decls.contains_key(name) {
            self.order.push(name.to_string());
        }
        // A complete definition replaces an earlier forward declaration.
        let keep_existing = matches!(
            (self.decls.get(name), &decl),
            (Some(Decl::Record { .. }), Decl::Opaque)
        );
        if !keep_existing {
            self.decls.insert(name.to_string(), decl);
            self.local.insert(name.to_string(), local);
        }
    }

    fn read(&mut self, ast: &Value) -> Result<()> {
        let Some(nodes) = ast.get("inner").and_then(Value::as_array) else {
            return Err(anyhow!("clang's AST has no declarations"));
        };
        let mut file = String::new();
        for node in nodes {
            if let Some(found) = node_file(node) {
                file = found;
            }
            let local = self.is_local(&file);
            self.read_node(node, local);
        }
        Ok(())
    }

    fn is_local(&self, file: &str) -> bool {
        if file.is_empty() {
            return false;
        }
        let path = std::fs::canonicalize(file).unwrap_or_else(|_| PathBuf::from(file));
        path.starts_with(&self.root)
    }

    fn read_node(&mut self, node: &Value, local: bool) {
        let kind = node.get("kind").and_then(Value::as_str).unwrap_or_default();
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if node.get("isImplicit").and_then(Value::as_bool) == Some(true) {
            return;
        }
        match kind {
            "RecordDecl" => {
                if name.is_empty() {
                    self.unnamed_record = Some(node.clone());
                    return;
                }
                self.unnamed_record = None;
                let decl = self.record(node, &name);
                self.insert(&name, decl, local);
            }
            "TypedefDecl" => self.read_typedef(node, name, local),
            "EnumDecl" => self.read_enum(node, name, local),
            "FunctionDecl" => self.read_function(node, &name, local),
            "VarDecl" if node.get("storageClass").and_then(Value::as_str) == Some("extern") => {
                let ty = self.ctype(&qual_type(node));
                self.insert(&name, Decl::Static(ty), local);
            }
            _ => {}
        }
    }

    /// A `typedef`: an alias, or the name of the record it introduces.
    fn read_typedef(&mut self, node: &Value, name: String, local: bool) {
        let qual = qual_type(node);
        self.typedefs.insert(name.clone(), qual.clone());
        // `typedef struct { .. } name;` names the unnamed record
        // read just before it.
        if (qual.contains("(unnamed") || qual.contains("(anonymous"))
            && let Some(record) = self.unnamed_record.take()
        {
            let decl = self.record(&record, &name);
            self.insert(&name, decl, local);
            return;
        }
        // `typedef struct name name;` adds nothing to the struct;
        // clang also spells an unnamed record that a typedef names
        // after the typedef.
        let tag = qual
            .strip_prefix("struct ")
            .or_else(|| qual.strip_prefix("union "));
        if tag == Some(name.as_str()) {
            if let Some(record) = self.unnamed_record.take() {
                let decl = self.record(&record, &name);
                self.insert(&name, decl, local);
                return;
            }
            if !self.decls.contains_key(&name) {
                self.insert(&name, Decl::Opaque, local);
            }
            return;
        }
        let ty = self.ctype(&qual);
        self.insert(&name, Decl::Alias(ty), local);
    }

    /// An `enum`: its constants, numbered as C numbers them.
    fn read_enum(&mut self, node: &Value, name: String, local: bool) {
        let mut next = 0i64;
        let mut values = Vec::new();
        for constant in children(node) {
            if constant.get("kind").and_then(Value::as_str) != Some("EnumConstantDecl") {
                continue;
            }
            let constant_name = constant
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(value) = constant_value(constant) {
                next = value;
            }
            values.push((constant_name.to_string(), next));
            next = next.wrapping_add(1);
        }
        let key = if name.is_empty() {
            self.anonymous += 1;
            format!("__anonymous_enum_{}", self.anonymous)
        } else {
            name
        };
        self.insert(&key, Decl::Enum(values), local);
    }

    /// A function with external linkage.
    fn read_function(&mut self, node: &Value, name: &str, local: bool) {
        if node.get("storageClass").and_then(Value::as_str) == Some("static")
            || node.get("inline").and_then(Value::as_bool) == Some(true)
        {
            return;
        }
        let signature = qual_type(node);
        let ret = signature
            .split_once('(')
            .map(|(ret, _)| ret.trim().to_string())
            .unwrap_or_default();
        let ret = self.ctype(&ret);
        let params = children(node)
            .filter(|param| param.get("kind").and_then(Value::as_str) == Some("ParmVarDecl"))
            .enumerate()
            .map(|(index, param)| Field {
                name: param
                    .get("name")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("arg{index}"), str::to_string),
                ty: self.ctype(&qual_type(param)),
            })
            .collect();
        let variadic = node.get("variadic").and_then(Value::as_bool) == Some(true);
        self.insert(
            name,
            Decl::Function {
                params,
                ret,
                variadic,
            },
            local,
        );
    }

    /// A struct or union's declaration; nested anonymous records become
    /// records of their own named `<owner>_<field>`.
    fn record(&mut self, node: &Value, owner: &str) -> Decl {
        if node.get("completeDefinition").and_then(Value::as_bool) != Some(true) {
            return Decl::Opaque;
        }
        let union = node.get("tagUsed").and_then(Value::as_str) == Some("union");
        let mut fields = Vec::new();
        let mut bitfields = false;
        let mut pending: Option<String> = None;
        for child in children(node) {
            match child.get("kind").and_then(Value::as_str) {
                Some("RecordDecl") => {
                    let inner_name = child
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if inner_name.is_empty() {
                        self.anonymous += 1;
                        let generated = format!("{owner}_anon{}", self.anonymous);
                        let decl = self.record(child, &generated);
                        self.insert(&generated, decl, true);
                        pending = Some(generated);
                    } else {
                        let decl = self.record(child, inner_name);
                        self.insert(inner_name, decl, true);
                    }
                }
                Some("FieldDecl") => {
                    if child.get("isBitfield").and_then(Value::as_bool) == Some(true) {
                        bitfields = true;
                    }
                    let qual = qual_type(child);
                    let anonymous = qual.contains("(unnamed") || qual.contains("(anonymous");
                    let ty = match (anonymous, pending.take()) {
                        (true, Some(generated)) => CType::Named(generated),
                        _ => self.ctype(&qual),
                    };
                    let name = child
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .map_or_else(|| format!("anon{}", fields.len()), str::to_string);
                    fields.push(Field { name, ty });
                }
                _ => {}
            }
        }
        // A C11 anonymous member: a nested record with no field naming it.
        if let Some(generated) = pending {
            fields.push(Field {
                name: format!("anon{}", fields.len()),
                ty: CType::Named(generated),
            });
        }
        Decl::Record {
            union,
            fields,
            bitfields,
        }
    }

    /// The C type `qual` spells.
    fn ctype(&self, qual: &str) -> CType {
        let raw = qual.trim();
        // `T *`, `const T *`, `T *const`: a pointer whose pointee is `const`
        // when that word qualifies the text before the last `*`.
        let unqualified_tail =
            raw.trim_end_matches(|c: char| c.is_alphabetic() || c == '_' || c.is_whitespace());
        if unqualified_tail.ends_with('*') && !raw.contains("(*)") {
            let pointee = &unqualified_tail[..unqualified_tail.len() - 1];
            let constant = pointee
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|word| word == "const");
            return CType::Pointer(Box::new(self.ctype(pointee)), constant);
        }
        let text = strip_qualifiers(qual);
        let text = text.trim();
        // `T (*)[N]`: a pointer to an array.
        if let Some(open) = text.find("(*)[") {
            let elem = self.ctype(&format!("{}{}", &text[..open].trim(), &text[open + 3..]));
            return CType::Pointer(Box::new(elem), false);
        }
        // `R (*)(A, B)`: a function pointer.
        if let Some(open) = text.find("(*)") {
            let ret = self.ctype(&text[..open]);
            let params_text = text[open + 3..].trim();
            let params_text = params_text
                .strip_prefix('(')
                .and_then(|rest| rest.strip_suffix(')'))
                .unwrap_or("");
            let params = split_params(params_text)
                .into_iter()
                .filter(|param| param != "void" && !param.is_empty())
                .map(|param| self.ctype(&param))
                .collect();
            return CType::Pointer(
                Box::new(CType::Function {
                    ret: Box::new(ret),
                    params,
                }),
                false,
            );
        }
        if let Some(stripped) = text.strip_suffix('*') {
            return CType::Pointer(Box::new(self.ctype(stripped)), false);
        }
        if let Some(open) = text.rfind('[')
            && text.ends_with(']')
        {
            let elem = self.ctype(&text[..open]);
            let len = text[open + 1..text.len() - 1].trim().parse().unwrap_or(0);
            return CType::Array(Box::new(elem), len);
        }
        for prefix in ["struct ", "union "] {
            if let Some(tag) = text.strip_prefix(prefix) {
                return CType::Named(tag.trim().to_string());
            }
        }
        if text.starts_with("enum ") {
            return CType::Scalar("ffi::c_int");
        }
        if let Some(scalar) = scalar(text) {
            return scalar;
        }
        if let Some(definition) = self.typedefs.get(text) {
            // A function pointer is spelled where it is used.
            let normal = strip_qualifiers(definition);
            if normal.contains("(*)(") && normal != text {
                return self.ctype(&normal);
            }
            return CType::Named(text.to_string());
        }
        CType::Unsupported(text.to_string())
    }

    /// Keeps the declarations the header's tree declares, or those
    /// `--allow` names, and every type they reach.
    fn select(&mut self) {
        let wanted: Vec<String> = self
            .order
            .iter()
            .filter(|name| {
                if self.allow.is_empty() {
                    self.local.get(*name).copied().unwrap_or(false)
                } else {
                    self.allow.iter().any(|pattern| glob(pattern, name))
                }
            })
            .cloned()
            .collect();
        let mut work = wanted;
        while let Some(name) = work.pop() {
            if !self.selected.insert(name.clone()) {
                continue;
            }
            let mut reached = Vec::new();
            match self.decls.get(&name) {
                Some(Decl::Record { fields, .. }) => {
                    for field in fields {
                        named_in(&field.ty, &mut reached);
                    }
                }
                Some(Decl::Alias(ty) | Decl::Static(ty)) => named_in(ty, &mut reached),
                Some(Decl::Function { params, ret, .. }) => {
                    named_in(ret, &mut reached);
                    for param in params {
                        named_in(&param.ty, &mut reached);
                    }
                }
                _ => {}
            }
            work.extend(
                reached
                    .into_iter()
                    .filter(|name| self.decls.contains_key(name)),
            );
        }
    }

    /// Each selected declaration's text, by name, in header order.
    fn render(&self, macros: &[(String, String)]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for name in &self.order {
            if !self.selected.contains(name) {
                continue;
            }
            let Some(decl) = self.decls.get(name) else {
                continue;
            };
            if let Some(text) = self.render_decl(name, decl) {
                out.push((name.clone(), text));
            }
        }
        for (name, value) in macros {
            let allowed = if self.allow.is_empty() {
                true
            } else {
                self.allow.iter().any(|pattern| glob(pattern, name))
            };
            if allowed && let Some(text) = constant(name, value) {
                out.push((format!("#define {name}"), text));
            }
        }
        out
    }

    fn render_decl(&self, name: &str, decl: &Decl) -> Option<String> {
        Some(match decl {
            Decl::Record {
                union: true,
                fields,
                ..
            } => {
                let mut members = Vec::new();
                for field in fields {
                    members.push(Self::spell(&field.ty, Place::Field)?);
                }
                let tuple = if members.len() == 1 {
                    format!("({},)", members[0])
                } else {
                    format!("({})", members.join(", "))
                };
                format!("pub type {name} = ffi::Union<{tuple}>\n")
            }
            Decl::Record {
                bitfields: true, ..
            } => format!(
                "    // `{name}` holds bit-fields, which have no portable layout.\n    pub type {name}\n"
            ),
            Decl::Record { fields, .. } => {
                let mut text = format!("#[repr(C)]\npub struct {name} {{\n");
                for field in fields {
                    let ty = Self::spell(&field.ty, Place::Field)?;
                    writeln!(text, "    pub {}: {ty}", identifier(&field.name)).ok()?;
                }
                text.push_str("}\n");
                text
            }
            Decl::Opaque => format!("    pub type {name}\n"),
            Decl::Alias(ty) => {
                // A function pointer is spelled where it is used.
                if matches!(ty, CType::Pointer(inner, _) if matches!(**inner, CType::Function { .. }))
                {
                    return None;
                }
                format!("pub type {name} = {}\n", Self::spell(ty, Place::Field)?)
            }
            Decl::Enum(values) => {
                let mut text = String::new();
                if !name.starts_with("__anonymous_enum_") {
                    writeln!(text, "pub type {name} = ffi::c_int").ok()?;
                }
                for (constant, value) in values {
                    writeln!(text, "pub const {constant}: ffi::c_int = {value}").ok()?;
                }
                text
            }
            Decl::Function { variadic: true, .. } => format!(
                "    // `{name}` takes a variable argument list; call it through a fixed-arity C shim\n    // in `[native]` sources.\n"
            ),
            Decl::Function { params, ret, .. } => {
                let mut list = Vec::new();
                for param in params {
                    let ty = Self::spell(&param.ty, Place::Param);
                    let Some(ty) = ty else {
                        return Some(format!(
                            "    // `{name}` takes a `{}`, which Gossamer cannot pass to C.\n",
                            describe(&param.ty)
                        ));
                    };
                    list.push(format!("{}: {ty}", identifier(&param.name)));
                }
                let ret = match ret {
                    CType::Void => String::new(),
                    other => match Self::spell(other, Place::Return) {
                        Some(ty) => format!(" -> {ty}"),
                        None => {
                            return Some(format!(
                                "    // `{name}` answers a `{}`, which Gossamer cannot take from C.\n",
                                describe(other)
                            ));
                        }
                    },
                };
                format!("    pub fn {name}({}){ret}\n", list.join(", "))
            }
            Decl::Static(ty) => {
                format!(
                    "    pub static {name}: {}\n",
                    Self::spell(ty, Place::Field)?
                )
            }
        })
    }

    /// The Gossamer spelling of `ty` at `place`, or `None` when it has none.
    fn spell(ty: &CType, place: Place) -> Option<String> {
        Some(match ty {
            CType::Void => "()".to_string(),
            CType::Scalar(name) => (*name).to_string(),
            CType::Named(name) => name.clone(),
            CType::Array(elem, len) => match place {
                Place::Field => format!("[{}; {len}]", Self::spell(elem, Place::Field)?),
                Place::Param | Place::Return => {
                    let pointer = CType::Pointer(elem.clone(), false);
                    return Self::spell(&pointer, place);
                }
            },
            // A pointer to scalars is a buffer: a parameter crosses as a
            // slice, which C reads (`const`) or writes back.
            CType::Pointer(inner, constant)
                if place == Place::Param && matches!(&**inner, CType::Scalar(_)) =>
            {
                let elem = match &**inner {
                    CType::Scalar("ffi::c_char" | "ffi::c_uchar" | "ffi::c_schar") => "u8",
                    CType::Scalar(name) => name,
                    _ => return None,
                };
                if *constant {
                    format!("[{elem}]")
                } else {
                    format!("&mut [{elem}]")
                }
            }
            CType::Pointer(inner, _) => match (&**inner, place) {
                (CType::Function { ret, params }, Place::Param) => {
                    let mut list = Vec::new();
                    for param in params {
                        list.push(Self::spell(param, Place::Return)?);
                    }
                    let ret = match &**ret {
                        CType::Void => String::new(),
                        other => format!(" -> {}", Self::spell(other, Place::Return)?),
                    };
                    format!("Fn({}){ret}", list.join(", "))
                }
                (CType::Function { .. }, Place::Field) => "ffi::Ptr<ffi::c_void>".to_string(),
                (CType::Function { .. }, Place::Return) => {
                    "Option<ffi::Ptr<ffi::c_void>>".to_string()
                }
                (pointee, _) => {
                    let pointee = match pointee {
                        CType::Void => "ffi::c_void".to_string(),
                        // A pointer to a pointer is a word in memory.
                        CType::Pointer(..) => "u64".to_string(),
                        other => Self::spell(other, Place::Field)?,
                    };
                    match place {
                        Place::Field => format!("ffi::Ptr<{pointee}>"),
                        Place::Param | Place::Return => format!("Option<ffi::Ptr<{pointee}>>"),
                    }
                }
            },
            CType::Function { .. } | CType::Unsupported(_) => return None,
        })
    }
}

/// The directory tree whose declarations a header's output keeps: the
/// header's own directory.
fn header_root(header: &Path) -> PathBuf {
    let absolute = std::path::absolute(header).unwrap_or_else(|_| header.to_path_buf());
    let dir = absolute.parent().map(Path::to_path_buf).unwrap_or(absolute);
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// The file a declaration's location names, when it names one.
fn node_file(node: &Value) -> Option<String> {
    let loc = node.get("loc")?;
    loc.get("file")
        .or_else(|| loc.get("spellingLoc").and_then(|l| l.get("file")))
        .or_else(|| loc.get("expansionLoc").and_then(|l| l.get("file")))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn children(node: &Value) -> impl Iterator<Item = &Value> {
    node.get("inner")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn qual_type(node: &Value) -> String {
    node.get("type")
        .and_then(|ty| ty.get("qualType"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn constant_value(node: &Value) -> Option<i64> {
    children(node).find_map(|inner| {
        inner
            .get("value")
            .and_then(Value::as_str)
            .and_then(|value| value.parse().ok())
            .or_else(|| constant_value(inner))
    })
}

/// `text` without type qualifiers, packed: one space between two words,
/// none around punctuation (`char**`, `int(*)(void*,int)`).
fn strip_qualifiers(text: &str) -> String {
    const QUALIFIERS: &[&str] = &[
        "const",
        "volatile",
        "restrict",
        "__restrict",
        "_Nonnull",
        "_Nullable",
        "_Null_unspecified",
    ];
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let mut last_was_word = false;
    let flush = |word: &mut String, out: &mut String, last_was_word: &mut bool| {
        if word.is_empty() {
            return;
        }
        if !QUALIFIERS.contains(&word.as_str()) {
            if *last_was_word {
                out.push(' ');
            }
            out.push_str(word);
            *last_was_word = true;
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out, &mut last_was_word);
            if !c.is_whitespace() {
                out.push(c);
                last_was_word = false;
            }
        }
    }
    flush(&mut word, &mut out, &mut last_was_word);
    out
}

fn split_params(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(current.trim().to_string());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

fn scalar(text: &str) -> Option<CType> {
    Some(match text {
        "void" => CType::Void,
        "char" => CType::Scalar("ffi::c_char"),
        "signed char" => CType::Scalar("ffi::c_schar"),
        "unsigned char" => CType::Scalar("ffi::c_uchar"),
        "short" | "short int" | "signed short" => CType::Scalar("ffi::c_short"),
        "unsigned short" | "unsigned short int" => CType::Scalar("ffi::c_ushort"),
        "int" | "signed" | "signed int" => CType::Scalar("ffi::c_int"),
        "unsigned" | "unsigned int" => CType::Scalar("ffi::c_uint"),
        "long" | "long int" | "signed long" => CType::Scalar("ffi::c_long"),
        "unsigned long" | "unsigned long int" => CType::Scalar("ffi::c_ulong"),
        "long long" | "long long int" | "signed long long" => CType::Scalar("ffi::c_longlong"),
        "unsigned long long" | "unsigned long long int" => CType::Scalar("ffi::c_ulonglong"),
        "float" => CType::Scalar("f32"),
        "double" => CType::Scalar("f64"),
        "_Bool" | "bool" => CType::Scalar("bool"),
        "int8_t" => CType::Scalar("i8"),
        "int16_t" => CType::Scalar("i16"),
        "int32_t" => CType::Scalar("i32"),
        "int64_t" => CType::Scalar("i64"),
        "uint8_t" => CType::Scalar("u8"),
        "uint16_t" => CType::Scalar("u16"),
        "uint32_t" => CType::Scalar("u32"),
        "uint64_t" => CType::Scalar("u64"),
        "intptr_t" | "ptrdiff_t" => CType::Scalar("isize"),
        "uintptr_t" => CType::Scalar("usize"),
        "size_t" => CType::Scalar("ffi::size_t"),
        "ssize_t" => CType::Scalar("ffi::ssize_t"),
        "long double" | "__int128" | "unsigned __int128" | "_Float16" | "__fp16" => {
            CType::Unsupported(text.to_string())
        }
        _ => return None,
    })
}

fn named_in(ty: &CType, out: &mut Vec<String>) {
    match ty {
        CType::Named(name) => out.push(name.clone()),
        CType::Pointer(inner, _) | CType::Array(inner, _) => named_in(inner, out),
        CType::Function { ret, params } => {
            named_in(ret, out);
            for param in params {
                named_in(param, out);
            }
        }
        _ => {}
    }
}

fn describe(ty: &CType) -> String {
    match ty {
        CType::Unsupported(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

/// `name` as a Gossamer identifier: a keyword gains a trailing `_`.
fn identifier(name: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "as", "break", "const", "continue", "defer", "else", "enum", "false", "fn", "for", "if",
        "impl", "in", "let", "loop", "match", "mod", "mut", "pub", "return", "self", "static",
        "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "spawn",
        "select", "cohort", "arena", "comptime", "extern", "move", "ref",
    ];
    if KEYWORDS.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

/// `const NAME: T = value` for a macro whose replacement is one literal.
fn constant(name: &str, value: &str) -> Option<String> {
    let mut text = value.trim();
    while let Some(inner) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
        text = inner.trim();
    }
    if text.starts_with('"') && text.ends_with('"') && text.len() >= 2 {
        return Some(format!("pub const {name}: String = {text}\n"));
    }
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest.trim()),
        None => ("", text),
    };
    let lower = digits.to_ascii_lowercase();
    let unsigned = lower.trim_end_matches(['l']).ends_with('u') || lower.starts_with('u');
    let core = lower.trim_end_matches(['u', 'l']);
    let numeric = if let Some(hex) = core.strip_prefix("0x") {
        !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit())
    } else {
        !core.is_empty() && core.chars().all(|c| c.is_ascii_digit())
    };
    if numeric {
        let ty = if unsigned && sign.is_empty() {
            "u64"
        } else {
            "i64"
        };
        return Some(format!("pub const {name}: {ty} = {sign}{core}\n"));
    }
    let float = core.trim_end_matches('f');
    if float.contains('.') && float.parse::<f64>().is_ok() {
        return Some(format!("pub const {name}: f64 = {sign}{float}\n"));
    }
    None
}

/// Whether `name` matches `pattern`, where `*` matches any run.
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    for (index, part) in parts.iter().enumerate() {
        if index == 0 {
            let Some(after) = rest.strip_prefix(part) else {
                return false;
            };
            rest = after;
        } else if index == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            let Some(at) = rest.find(part) else {
                return false;
            };
            rest = &rest[at + part.len()..];
        }
    }
    true
}

/// The `#[cfg]` that selects `triple`: its OS, architecture, and C
/// library.
fn target_cfg(triple: &str) -> String {
    let arch = triple.split('-').next().unwrap_or_default();
    let arch = match arch {
        "arm64" => "aarch64",
        a if a.starts_with("armv7") => "arm",
        a if a == "i686" || a == "i586" => "x86",
        a => a,
    };
    let os = if triple.contains("windows") {
        "windows"
    } else if triple.contains("apple") || triple.contains("darwin") {
        "macos"
    } else if triple.contains("linux") {
        "linux"
    } else {
        "unknown"
    };
    let env = if triple.ends_with("musl") {
        "musl"
    } else if triple.ends_with("gnu") {
        "gnu"
    } else if triple.ends_with("msvc") {
        "msvc"
    } else {
        ""
    };
    let mut parts = vec![
        format!("target_os = \"{os}\""),
        format!("target_arch = \"{arch}\""),
    ];
    if !env.is_empty() {
        parts.push(format!("target_env = \"{env}\""));
    }
    format!("#[cfg(all({}))]", parts.join(", "))
}

/// One target's rendered declarations, by name, under its triple (`None`
/// for the host).
type TargetItems = (Option<String>, Vec<(String, String)>);

/// The output: declarations every target agrees on once, and the rest once
/// per target under its `#[cfg]`, with the functions and statics gathered
/// into `unsafe extern "C"` blocks.
fn merge(per_target: &[TargetItems], options: &Options) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut texts: BTreeMap<String, Vec<(Option<String>, String)>> = BTreeMap::new();
    for (target, items) in per_target {
        for (name, text) in items {
            if !texts.contains_key(name) {
                names.push(name.clone());
            }
            texts
                .entry(name.clone())
                .or_default()
                .push((target.clone(), text.clone()));
        }
    }
    let link = options
        .link
        .as_ref()
        .map(|library| format!("#[link(name = \"{library}\")]\n"))
        .unwrap_or_default();
    let header = options.header.file_name().map_or_else(
        || options.header.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut types = String::new();
    let mut externs = String::new();
    let mut consts = String::new();
    for name in &names {
        let variants = &texts[name];
        let agreed = variants.len() == per_target.len()
            && variants.iter().all(|(_, text)| *text == variants[0].1);
        let entries: Vec<(Option<String>, &String)> = if agreed {
            vec![(None, &variants[0].1)]
        } else {
            variants
                .iter()
                .map(|(target, text)| (target.as_deref().map(target_cfg), text))
                .collect()
        };
        for (cfg, text) in entries {
            // Functions, statics, and opaque types sit in the extern block.
            let is_extern = text.starts_with("    ");
            let sink = if is_extern {
                &mut externs
            } else if name.starts_with("#define ") {
                &mut consts
            } else {
                &mut types
            };
            if let Some(cfg) = cfg {
                let indent = if is_extern { "    " } else { "" };
                sink.push_str(&format!("{indent}{cfg}\n"));
            }
            sink.push_str(text);
            // A definition spanning lines stands apart from the next.
            if !is_extern && text.trim_end().contains('\n') {
                sink.push('\n');
            }
        }
    }
    let mut out = format!(
        "// Gossamer declarations for `{header}`, written by `gos bindgen --c`.\n// Regenerate them rather than editing by hand. A parameter pointing at\n// scalars crosses as a slice (`[T]` for `const T *`, `&mut [T]` otherwise,\n// `u8` for `char`); other pointers are `Option<ffi::Ptr<T>>`.\n\nuse std::ffi\n\n"
    );
    out.push_str(&consts);
    if !consts.is_empty() && !consts.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&types);
    if !types.is_empty() && !types.ends_with("\n\n") {
        out.push('\n');
    }
    if !externs.is_empty() {
        out.push_str(&format!("{link}unsafe extern \"C\" {{\n{externs}}}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_macros_become_constants() {
        assert_eq!(
            constant("A", "3").as_deref(),
            Some("pub const A: i64 = 3\n")
        );
        assert_eq!(
            constant("B", "(0x10u)").as_deref(),
            Some("pub const B: u64 = 0x10\n")
        );
        assert_eq!(
            constant("C", "(-1)").as_deref(),
            Some("pub const C: i64 = -1\n")
        );
        assert_eq!(
            constant("D", "\"tee\"").as_deref(),
            Some("pub const D: String = \"tee\"\n")
        );
        assert_eq!(
            constant("E", "1.5f").as_deref(),
            Some("pub const E: f64 = 1.5\n")
        );
        assert_eq!(constant("F", "SOME_OTHER"), None);
    }

    #[test]
    fn globs_match_prefixes_and_infixes() {
        assert!(glob("git_*", "git_repository_open"));
        assert!(glob("*_open", "git_repository_open"));
        assert!(glob("git_*_open", "git_repository_open"));
        assert!(!glob("git_*", "svn_open"));
        assert!(glob("exact", "exact"));
    }

    #[test]
    fn qualifiers_and_pointers_normalise() {
        assert_eq!(strip_qualifiers("const char *"), "char*");
        assert_eq!(strip_qualifiers("char *const *"), "char**");
        assert_eq!(strip_qualifiers("unsigned long long"), "unsigned long long");
        assert_eq!(
            strip_qualifiers("int (*)(const void *, int)"),
            "int(*)(void*,int)"
        );
    }
}
