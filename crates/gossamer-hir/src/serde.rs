//! Typed serde: the bodies of `to_json::<T>`, `from_json::<T>`, and their
//! toml and yaml forms.
//!
//! The frontend declares a body-less stub `__gos_serde_<op>_<T>` for each type
//! a serde call names, so the calls are checked like any other. This pass
//! writes those stubs' bodies once the program is checked, from the checked
//! field types, and adds one encode and one decode function for every user
//! type the bodies reach. The bodies call the `__gos_serde_rd_*` and
//! `__gos_serde_wr_*` steps, which every tier implements over the same reader
//! (`gossamer_core::json::typed`) and writes the same bytes with.
//!
//! A decode reads through a sticky error: the first failure is kept, every
//! later read answers a zero value, and the walk checks once at the end. That
//! reports the failure the first missing or mistyped value in declaration
//! order causes, wrapped by the path of each nested decode it unwound, which
//! is what a walk that returned at its first failure would report.

use std::collections::HashMap;

use gossamer_ast::Ident;
use gossamer_ast::serde_attrs::{DefaultValue, FieldDefault, RenameRule, SerdeField};
use gossamer_lex::Span;
use gossamer_resolve::DefId;
use gossamer_types::{FloatTy, IntTy, Substs, Ty, TyCtxt, TyKind};

use crate::ids::HirIdGenerator;
use crate::tree::FnOrigin;
use crate::tree::{
    HirAdt, HirAdtKind, HirBinaryOp, HirBlock, HirBody, HirExpr, HirExprKind, HirFieldPat, HirFn,
    HirItem, HirItemKind, HirLiteral, HirMatchArm, HirParam, HirPat, HirPatKind, HirProgram,
    HirStmt, HirStmtKind,
};

mod binding_wire;
mod render_calls;

/// The runtime entry a serde step names, or `None` for a name that is not
/// one: `__gos_serde_rd_i64` is `gos_rt_serde_rd_i64`.
#[must_use]
pub fn serde_step_symbol(name: &str) -> Option<String> {
    let tail = name.strip_prefix("__gos_serde_")?;
    (tail.starts_with("rd_") || tail.starts_with("wr_")).then(|| format!("gos_rt_serde_{tail}"))
}

/// Whether `name` is a generated walk that writes a value: a renderer or an
/// encoder, in any mode, or a binding-wire encoder. Such a walk only reads the
/// value it is handed - it writes the writer, never the value, and keeps no
/// part of it - so a caller may hand it storage it still holds.
#[must_use]
pub fn is_serde_writing_walk(name: &str) -> bool {
    name.strip_prefix("__gos_serde_").is_some_and(|tail| {
        tail.starts_with("ren") || tail.starts_with("enc") || tail.starts_with("wenc_")
    })
}

/// How a named field reads and writes: under its own name, a `#[rename]`d
/// key, or its container's `#[rename_all]` spelling; left out under `#[skip]`;
/// and filled with its default when missing under `#[default]` or
/// `#[default(value)]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HirSerdeField {
    /// The key the field is written under, when it is not the field's name.
    pub key: Option<String>,
    /// `#[skip]`: never written, and read as its default.
    pub skip: bool,
    /// What a missing key reads as, when it is not a failure.
    pub default: Option<FieldDefault>,
}

impl HirSerdeField {
    /// The options the attributes of field `name` state, under the
    /// container's rename rule. `declared` names the function evaluating the
    /// field's own default, which a missing key reads when no `#[default]`
    /// says otherwise.
    #[must_use]
    pub fn of(
        attrs: &gossamer_ast::Attrs,
        name: &str,
        rule: Option<RenameRule>,
        declared: Option<String>,
    ) -> Self {
        let options = SerdeField::of(attrs);
        let key = options.key(name, rule);
        Self {
            key: (key != name).then_some(key),
            skip: options.skip,
            default: options.default.or(declared.map(FieldDefault::Declared)),
        }
    }
}

/// How an enum variant reads and writes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HirSerdeVariant {
    /// The name the variant is written under.
    pub key: String,
    /// A struct variant's field options, in declaration order.
    pub fields: Vec<HirSerdeField>,
}

/// How a struct or enum reads and writes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HirSerde {
    /// A named struct's field options, in declaration order.
    pub fields: Vec<HirSerdeField>,
    /// An enum's variant options, in declaration order.
    pub variants: Vec<HirSerdeVariant>,
    /// `#[tag("member")]`: the enum is internally tagged by that member.
    pub tag: Option<String>,
}

/// What a decode root reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Source {
    /// JSON text.
    Json,
    /// A `json::Value`.
    Value,
    /// YAML text.
    Yaml,
    /// TOML text.
    Toml,
}

impl Source {
    /// The step opening a reader over it.
    const fn open_step(self) -> &'static str {
        match self {
            Self::Json => "rd_new",
            Self::Value => "rd_new_value",
            Self::Yaml => "rd_new_yaml",
            Self::Toml => "rd_new_toml",
        }
    }
}

/// The operation a stub performs, by its name's prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    ToJson,
    FromJson,
    /// `json::from_value::<T>(value)`: a typed decode of a `json::Value`.
    FromValue,
    ToToml,
    FromToml,
    ToYaml,
    FromYaml,
}

impl Op {
    fn of_stub(name: &str) -> Option<Self> {
        // A stub declared in a module carries the module's path.
        let name = name.rsplit("::").next().unwrap_or(name);
        let tail = name.strip_prefix("__gos_serde_")?;
        [
            ("to_json_", Self::ToJson),
            ("from_json_", Self::FromJson),
            ("from_value_", Self::FromValue),
            ("to_toml_", Self::ToToml),
            ("from_toml_", Self::FromToml),
            ("to_yaml_", Self::ToYaml),
            ("from_yaml_", Self::FromYaml),
        ]
        .into_iter()
        .find_map(|(prefix, op)| tail.starts_with(prefix).then_some(op))
    }

    fn encodes(self) -> bool {
        matches!(self, Self::ToJson | Self::ToToml | Self::ToYaml)
    }
}

/// How a value of one type is read and written.
#[derive(Debug, Clone)]
enum Kind {
    I64,
    /// A narrower integer, read as an `i64` and cast.
    Narrow(&'static str),
    /// `u64` or `usize`, read as a `u64` and cast.
    Unsigned(&'static str),
    F64,
    F32,
    Bool,
    /// A `char`, written as a one-character string.
    Char,
    Str,
    Json,
    Seq(Ty),
    /// A fixed array, read from a JSON array of exactly its length.
    Array(Ty, usize),
    Opt(Ty),
    Tuple(Vec<Ty>),
    Map(Ty),
    Adt(DefId, Ty),
    /// A `Result`, which only the binding wire spells.
    Res(Ty, Ty),
}

/// A user type the bodies reach.
#[derive(Debug, Clone)]
struct AdtInfo {
    /// The type's qualified name: `geo::Point`.
    path: String,
    /// The same path as segments, for a variant constructor.
    segments: Vec<String>,
    adt: HirAdt,
    serde: HirSerde,
}

/// Writes the stubs' bodies and the functions they reach.
pub(crate) fn generate_serde_bodies(
    program: &mut HirProgram,
    tcx: &mut TyCtxt,
    ids: &mut HirIdGenerator,
) {
    let mut adts = HashMap::new();
    for item in &program.items {
        if let (HirItemKind::Adt(adt), Some(def)) = (&item.kind, item.def) {
            // A declaration inside a module already carries its qualified
            // name (`geo::Point`), the identity every literal of it uses.
            let segments: Vec<String> = adt.name.name.split("::").map(str::to_string).collect();
            adts.insert(
                def,
                AdtInfo {
                    path: adt.name.name.clone(),
                    segments,
                    adt: adt.clone(),
                    serde: adt.serde.clone(),
                },
            );
        }
    }
    let mut builder = Gen {
        tcx,
        ids,
        adts,
        encoders: HashMap::new(),
        renderers: HashMap::new(),
        decoders: HashMap::new(),
        wire_encoders: HashMap::new(),
        wire_decoders: HashMap::new(),
        roots: HashMap::new(),
        pending: Vec::new(),
        items: Vec::new(),
        temp: 0,
        span: Span::default(),
        rendering: false,
        mode: Mode::default(),
        wire: false,
    };
    for item in &mut program.items {
        let span = item.span;
        if let HirItemKind::Fn(f) = &mut item.kind
            && let Some(op) = Op::of_stub(&f.name.name)
            && let Some(body) = {
                builder.span = span;
                builder.stub_body(op, f, span)
            }
        {
            f.body = Some(body);
        }
    }
    for item in &mut program.items {
        builder.wire_binding_calls(item);
    }
    for item in &mut program.items {
        builder.render_json_calls(item);
    }
    builder.drain();
    program.items.append(&mut builder.items);
}

/// How a walk writes struct members: `sorted` orders them by key rather
/// than by declaration, and `omit_none` leaves out an `Option` member that is
/// `None`, as a format with no null (TOML) spells an absent value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
struct Mode {
    sorted: bool,
    omit_none: bool,
}

impl Mode {
    /// The infix naming the generated functions of this mode.
    fn infix(self) -> &'static str {
        match (self.sorted, self.omit_none) {
            (false, false) => "",
            (true, false) => "s",
            (false, true) => "n",
            (true, true) => "sn",
        }
    }
}

enum Pending {
    /// A type's encoder, written in the given walk mode.
    Encode(DefId, Ty, String, Mode),
    Render(DefId, Ty, String, Mode),
    Decode(DefId, Ty, String),
    WireEncode(DefId, Ty, String),
    WireDecode(DefId, Ty, String),
    /// A decode root: the target type, the stub's return type, the
    /// function's name, and what it reads.
    Root(Ty, Ty, String, Source),
}

struct Gen<'a> {
    tcx: &'a mut TyCtxt,
    ids: &'a mut HirIdGenerator,
    adts: HashMap<DefId, AdtInfo>,
    /// Every map of generated functions is keyed by the type a function
    /// serves, so each instantiation of a generic type has its own.
    encoders: HashMap<(Ty, Mode), String>,
    /// The functions writing a type the way `json::encode` does, per mode.
    renderers: HashMap<(Ty, Mode), String>,
    decoders: HashMap<Ty, String>,
    /// The functions writing and reading a type as the binding wire spells it.
    wire_encoders: HashMap<Ty, String>,
    wire_decoders: HashMap<Ty, String>,
    roots: HashMap<(Ty, Source), String>,
    pending: Vec<Pending>,
    items: Vec<HirItem>,
    temp: u32,
    /// Where the generated functions say they come from: the first stub.
    span: Span,
    /// Whether the walk being written renders as `json::encode` does: a
    /// struct's members in byte order of their names, every float with its
    /// fraction, and the shapes the typed surface does not take.
    rendering: bool,
    /// How the walk being written lays out struct members.
    mode: Mode,
    /// Whether the walk being written spells values as a binding item's text
    /// crosses (`gossamer_binding::wire_json`): `Option` as `[]` / `[v]`,
    /// `Result` as `[0, v]` / `[1, e]`, and a fixed array as a sequence.
    wire: bool,
}

/// The reader's local in a decode function, and the writer's in an encode.
const READER: &str = "__serde_r";
const WRITER: &str = "__serde_w";

impl Gen<'_> {
    fn stub_body(&mut self, op: Op, f: &HirFn, span: Span) -> Option<HirBody> {
        let ret = f.ret?;
        let ty = if op.encodes() {
            f.params.first()?.ty
        } else {
            self.result_ok_ty(ret)?
        };
        let param = binding_name(&f.params.first()?.pattern)?;
        if !op.encodes() {
            let source = match op {
                Op::FromValue => Source::Value,
                Op::FromYaml => Source::Yaml,
                Op::FromToml => Source::Toml,
                _ => Source::Json,
            };
            let root = self.root(ty, ret, source);
            let source_ty = if source == Source::Value {
                self.tcx.json_value_ty()
            } else {
                self.tcx.string_ty()
            };
            let input = self.local(&param, source_ty, span);
            let tail = self.call_named(&root, vec![input], ret, span);
            return Some(self.body(Vec::new(), tail, ret, span));
        }
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let mut out = Emit::default();
        let writer = self.step("wr_new", Vec::new(), i64_ty, span);
        out.stmt(self.let_stmt(WRITER, false, i64_ty, writer, span));
        // `sort:`, the stub's second parameter: members by key when true.
        if let Some(sort) = f.params.get(1).and_then(|p| binding_name(&p.pattern)) {
            let bool_ty = self.tcx.bool_ty();
            let mut walks = Vec::new();
            for sorted in [true, false] {
                let mut walk = Emit::default();
                let mode = Mode {
                    sorted,
                    omit_none: op == Op::ToToml,
                };
                let outer = std::mem::replace(&mut self.mode, mode);
                let value = self.local(&param, ty, span);
                self.encode(&mut walk, value, ty, span);
                self.mode = outer;
                let stmts = self.finish(walk, span);
                walks.push(self.block(stmts, None, unit, span));
            }
            let declared = walks.pop()?;
            let sorted = walks.pop()?;
            let cond = self.local(&sort, bool_ty, span);
            let chosen = self.if_else(cond, sorted, declared, unit, span);
            out.stmt(self.expr_stmt(chosen, span));
        } else {
            let mode = Mode {
                sorted: false,
                omit_none: op == Op::ToToml,
            };
            let outer = std::mem::replace(&mut self.mode, mode);
            let value = self.local(&param, ty, span);
            self.encode(&mut out, value, ty, span);
            self.mode = outer;
        }
        let stmts = self.finish(out, span);
        let w = self.local(WRITER, i64_ty, span);
        let answer = match op {
            Op::ToToml => self.step("wr_finish_toml", vec![w], ret, span),
            Op::ToYaml => self.step("wr_finish_yaml", vec![w], ret, span),
            _ => {
                let string_ty = self.tcx.string_ty();
                let text = self.step("wr_finish", vec![w], string_ty, span);
                self.ctor("Ok", vec![text], ret, span)
            }
        };
        Some(self.body(stmts, answer, ret, span))
    }

    /// The decode root for `ty` over `source`: opens a reader, decodes, and
    /// answers the value or the failure.
    fn root(&mut self, ty: Ty, ret: Ty, source: Source) -> String {
        if let Some(name) = self.roots.get(&(ty, source)) {
            return name.clone();
        }
        let name = format!("__gos_serde_root_{}", self.roots.len());
        self.roots.insert((ty, source), name.clone());
        self.pending
            .push(Pending::Root(ty, ret, name.clone(), source));
        name
    }

    /// What tells the functions of one instantiation of a generic type from
    /// another's: nothing for a type with no arguments.
    fn instance_suffix(&self, ty: Ty) -> String {
        match self.tcx.kind_of(ty) {
            TyKind::Adt { substs, .. } if !substs.is_empty() => format!("_i{}", ty.as_u32()),
            _ => String::new(),
        }
    }

    /// The field types of struct `def` as instantiation `ty` holds them.
    fn struct_fields_of(&mut self, def: DefId, ty: Ty) -> Option<Vec<Ty>> {
        let declared = self.tcx.struct_field_tys(def)?.to_vec();
        let args = self.type_args(ty);
        Some(
            declared
                .into_iter()
                .map(|f| gossamer_types::instantiate::subst_param_ty(self.tcx, f, &args))
                .collect(),
        )
    }

    /// The payload types of each variant of enum `def` as instantiation `ty`
    /// holds them.
    fn variant_fields_of(&mut self, def: DefId, ty: Ty) -> Option<Vec<Vec<Ty>>> {
        let declared = self.tcx.enum_variant_tys(def)?.to_vec();
        let args = self.type_args(ty);
        Some(
            declared
                .into_iter()
                .map(|tys| {
                    tys.into_iter()
                        .map(|t| gossamer_types::instantiate::subst_param_ty(self.tcx, t, &args))
                        .collect()
                })
                .collect(),
        )
    }

    /// The type arguments of instantiation `ty`, by parameter position.
    fn type_args(&self, ty: Ty) -> Vec<Option<Ty>> {
        match self.tcx.kind_of(ty) {
            TyKind::Adt { substs, .. } => substs
                .as_slice()
                .iter()
                .map(|arg| match arg {
                    gossamer_types::GenericArg::Type(t) => Some(*t),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    fn encoder(&mut self, def: DefId, ty: Ty) -> String {
        if self.wire {
            if let Some(name) = self.wire_encoders.get(&ty) {
                return name.clone();
            }
            let name = format!("__gos_serde_wenc_{}{}", def.local, self.instance_suffix(ty));
            self.wire_encoders.insert(ty, name.clone());
            self.pending
                .push(Pending::WireEncode(def, ty, name.clone()));
            return name;
        }
        let key = (ty, self.mode);
        if let Some(name) = self.encoders.get(&key) {
            return name.clone();
        }
        let name = format!(
            "__gos_serde_enc{}_{}{}",
            self.mode.infix(),
            def.local,
            self.instance_suffix(ty)
        );
        self.encoders.insert(key, name.clone());
        self.pending
            .push(Pending::Encode(def, ty, name.clone(), self.mode));
        name
    }

    fn renderer(&mut self, def: DefId, ty: Ty) -> String {
        let key = (ty, self.mode);
        if let Some(name) = self.renderers.get(&key) {
            return name.clone();
        }
        let name = format!(
            "__gos_serde_ren{}_{}{}",
            self.mode.infix(),
            def.local,
            self.instance_suffix(ty)
        );
        self.renderers.insert(key, name.clone());
        self.pending
            .push(Pending::Render(def, ty, name.clone(), self.mode));
        name
    }

    fn decoder(&mut self, def: DefId, ty: Ty) -> String {
        if self.wire {
            if let Some(name) = self.wire_decoders.get(&ty) {
                return name.clone();
            }
            let name = format!("__gos_serde_wdec_{}{}", def.local, self.instance_suffix(ty));
            self.wire_decoders.insert(ty, name.clone());
            self.pending
                .push(Pending::WireDecode(def, ty, name.clone()));
            return name;
        }
        if let Some(name) = self.decoders.get(&ty) {
            return name.clone();
        }
        let name = format!("__gos_serde_dec_{}{}", def.local, self.instance_suffix(ty));
        self.decoders.insert(ty, name.clone());
        self.pending.push(Pending::Decode(def, ty, name.clone()));
        name
    }

    /// Generates every function requested so far, and the ones those request.
    fn drain(&mut self) {
        while let Some(next) = self.pending.pop() {
            let item = match next {
                Pending::Encode(def, ty, name, mode) => {
                    let outer = std::mem::replace(&mut self.mode, mode);
                    let item = self.encode_fn(def, ty, &name);
                    self.mode = outer;
                    item
                }
                Pending::Render(def, ty, name, mode) => {
                    let outer = std::mem::replace(&mut self.rendering, true);
                    let outer_mode = std::mem::replace(&mut self.mode, mode);
                    let item = self.encode_fn(def, ty, &name);
                    self.rendering = outer;
                    self.mode = outer_mode;
                    item
                }
                Pending::Decode(def, ty, name) => self.decode_fn(def, ty, &name),
                Pending::WireEncode(def, ty, name) => {
                    let outer = std::mem::replace(&mut self.wire, true);
                    let item = self.encode_fn(def, ty, &name);
                    self.wire = outer;
                    item
                }
                Pending::WireDecode(def, ty, name) => {
                    let outer = std::mem::replace(&mut self.wire, true);
                    let item = self.decode_fn(def, ty, &name);
                    self.wire = outer;
                    item
                }
                Pending::Root(ty, ret, name, source) => Some(self.root_fn(ty, ret, &name, source)),
            };
            if let Some(item) = item {
                self.items.push(item);
            }
        }
    }

    fn root_fn(&mut self, ty: Ty, ret: Ty, name: &str, source: Source) -> HirItem {
        let span = self.adt_span();
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let source_ty = if source == Source::Value {
            self.tcx.json_value_ty()
        } else {
            self.tcx.string_ty()
        };
        let bool_ty = self.tcx.bool_ty();
        let unit = self.tcx.unit();
        let mut out = Emit::default();
        let source_value = self.local("__serde_source", source_ty, span);
        let reader = self.step(source.open_step(), vec![source_value], i64_ty, span);
        out.stmt(self.let_stmt(READER, false, i64_ty, reader, span));
        let value = self.decode(&mut out, ty, span);
        out.stmt(self.let_stmt("__serde_value", false, ty, value, span));
        let stmts = self.finish(out, span);
        let r = self.local(READER, i64_ty, span);
        let failed = self.step("rd_failed", vec![r], bool_ty, span);
        let err_ty = self.result_err_ty(ret).unwrap_or(unit);
        let then = {
            let r = self.local(READER, i64_ty, span);
            let error = self.step("rd_error", vec![r], err_ty, span);
            let bind = self.let_stmt("__serde_error", false, err_ty, error, span);
            let r = self.local(READER, i64_ty, span);
            let free = self.step("rd_free", vec![r], unit, span);
            let free = self.expr_stmt(free, span);
            let error = self.local("__serde_error", err_ty, span);
            let failure = self.ctor("Err", vec![error], ret, span);
            self.block(vec![bind, free], Some(failure), ret, span)
        };
        let otherwise = {
            let r = self.local(READER, i64_ty, span);
            let free = self.step("rd_free", vec![r], unit, span);
            let free = self.expr_stmt(free, span);
            let value = self.local("__serde_value", ty, span);
            let success = self.ctor("Ok", vec![value], ret, span);
            self.block(vec![free], Some(success), ret, span)
        };
        let tail = self.if_else(failed, then, otherwise, ret, span);
        let params = vec![self.param("__serde_source", source_ty, span)];
        self.function(name, params, ret, stmts, tail, span)
    }

    fn encode_fn(&mut self, def: DefId, ty: Ty, name: &str) -> Option<HirItem> {
        let info = self.adts.get(&def)?.clone();
        let span = self.adt_span();
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let mut out = Emit::default();
        match &info.adt.kind {
            HirAdtKind::Struct(fields) => {
                let field_tys = self.struct_fields_of(def, ty)?;
                self.encode_struct(
                    &mut out,
                    &info,
                    fields,
                    &field_tys,
                    "__serde_value",
                    ty,
                    false,
                    span,
                );
            }
            HirAdtKind::Enum(variants) => {
                let variant_tys = self.variant_fields_of(def, ty)?;
                let mut arms = Vec::new();
                for (index, (variant, tys)) in variants.iter().zip(&variant_tys).enumerate() {
                    let options = info.serde.variants.get(index).cloned().unwrap_or_default();
                    let tag = info.serde.tag.clone();
                    arms.push(self.encode_variant_arm(
                        variant,
                        &options,
                        tag.as_deref(),
                        tys,
                        ty,
                        span,
                    ));
                }
                let scrutinee = self.local("__serde_value", ty, span);
                let matched = self.expr(
                    unit,
                    span,
                    HirExprKind::Match {
                        scrutinee: Box::new(scrutinee),
                        arms,
                    },
                );
                out.stmt(self.expr_stmt(matched, span));
            }
        }
        let stmts = self.finish(out, span);
        let params = vec![
            self.param(WRITER, i64_ty, span),
            self.param("__serde_value", ty, span),
        ];
        let tail = self.unit_lit(span);
        Some(self.function(name, params, unit, stmts, tail, span))
    }

    /// A struct as an object of its written fields, in declaration order (by
    /// key when the walk renders sorted). `value` names the local holding it.
    /// With `members_only` the members follow one already written and no
    /// braces are written, as a tagged variant spells a struct payload.
    #[allow(clippy::too_many_arguments, reason = "one struct's parts, spelled out")]
    fn encode_struct(
        &mut self,
        out: &mut Emit,
        info: &AdtInfo,
        fields: &[Ident],
        field_tys: &[Ty],
        value: &str,
        ty: Ty,
        members_only: bool,
        span: Span,
    ) {
        // Every field is written under its key unless `#[skip]` leaves it out,
        // so a decode of the text reads the value back.
        let mut written: Vec<(usize, String)> = (0..fields.len().min(field_tys.len()))
            .filter_map(|index| {
                let options = info.serde.fields.get(index).cloned().unwrap_or_default();
                (!options.skip).then(|| {
                    (
                        index,
                        options.key.unwrap_or_else(|| fields[index].name.clone()),
                    )
                })
            })
            .collect();
        if self.mode.sorted {
            written.sort_by(|(_, a), (_, b)| a.as_bytes().cmp(b.as_bytes()));
        }
        let members = written
            .into_iter()
            .map(|(index, key)| {
                let holder = self.local(value, ty, span);
                let read = self.field(holder, &fields[index].name, field_tys[index], span);
                (key, read, field_tys[index])
            })
            .collect();
        self.encode_members(out, members, !members_only, members_only, span);
    }

    /// Writes object members. `braces` writes the object's `{` and `}`;
    /// `after_member` says a member is already written before the first. A
    /// member holding `Option<Option<T>>` is left out when it is `None`, so a
    /// decode reads it back as missing, and `Some(None)` writes `null`.
    fn encode_members(
        &mut self,
        out: &mut Emit,
        members: Vec<(String, HirExpr, Ty)>,
        braces: bool,
        after_member: bool,
        span: Span,
    ) {
        let unit = self.tcx.unit();
        // A member that may be left out makes every comma depend on what was
        // written before it, unless another member always precedes it.
        let dynamic = !after_member && members.iter().any(|(_, _, t)| self.may_omit(*t));
        if braces {
            out.raw("{");
        }
        for (n, (key, value, ty)) in members.into_iter().enumerate() {
            let text = format!("{}:", serde_json_string(&key));
            if self.may_omit(ty)
                && let Some(Kind::Opt(inner)) = self.kind(ty)
            {
                let payload = self.fresh("x");
                let mut some = Emit::default();
                self.write_key(&mut some, &text, span);
                let payload_value = self.local(&payload, inner, span);
                self.encode(&mut some, payload_value, inner, span);
                let some_stmts = self.finish(some, span);
                let some_body = self.block(some_stmts, None, unit, span);
                let none_body = self.block(Vec::new(), None, unit, span);
                let matched =
                    self.match_option(value, &payload, inner, some_body, none_body, unit, span);
                out.stmt(self.expr_stmt(matched, span));
                continue;
            }
            if dynamic {
                self.write_key(out, &text, span);
            } else {
                if n > 0 || after_member {
                    out.raw(",");
                }
                out.raw(&text);
            }
            self.encode(out, value, ty, span);
        }
        if braces {
            out.raw("}");
        }
    }

    /// `wr_key(w, text)`.
    fn write_key(&mut self, out: &mut Emit, text: &str, span: Span) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let w = self.local(WRITER, i64_ty, span);
        let text = self.string_lit(text, span);
        let step = self.step("wr_key", vec![w, text], unit, span);
        out.stmt(self.expr_stmt(step, span));
    }

    /// Whether a member of type `ty` is left out when it is `None`: an
    /// `Option<Option<T>>`, whose missing and null members read apart, or
    /// any `Option` in a walk that omits them.
    fn may_omit(&mut self, ty: Ty) -> bool {
        self.is_double_option(ty)
            || (self.mode.omit_none && matches!(self.kind(ty), Some(Kind::Opt(_))))
    }

    /// Whether `ty` is `Option<Option<T>>`, whose missing and null members
    /// read apart.
    fn is_double_option(&mut self, ty: Ty) -> bool {
        match self.kind(ty) {
            Some(Kind::Opt(inner)) => matches!(self.kind(inner), Some(Kind::Opt(_))),
            _ => false,
        }
    }

    /// The arm writing one variant. Externally tagged, a unit variant is its
    /// name and any other an object whose one key is its name. Under
    /// `#[tag]`, every variant is one object holding the tag member and the
    /// variant's fields (or its struct payload's).
    fn encode_variant_arm(
        &mut self,
        variant: &crate::tree::HirEnumVariant,
        options: &HirSerdeVariant,
        tag_member: Option<&str>,
        tys: &[Ty],
        ty: Ty,
        span: Span,
    ) -> HirMatchArm {
        let unit = self.tcx.unit();
        let name = if options.key.is_empty() {
            variant.name.name.clone()
        } else {
            options.key.clone()
        };
        let tag = serde_json_string(&name);
        let mut arm = Emit::default();
        let binds: Vec<String> = (0..tys.len()).map(|i| format!("__serde_p{i}")).collect();
        if let Some(member) = tag_member {
            arm.raw(&format!("{{{}:{tag}", serde_json_string(member)));
        }
        if let Some(names) = &variant.struct_fields {
            if tag_member.is_none() {
                arm.raw(&format!("{{{tag}:"));
            }
            self.encode_variant_fields(
                &mut arm,
                names,
                options,
                tag_member.is_some(),
                tys,
                &binds,
                span,
            );
            arm.raw("}");
        } else {
            self.encode_variant_payload(&mut arm, &tag, tag_member, tys, &binds, span);
        }
        let pattern = self.variant_pattern(variant, tys, &binds, ty, span);
        let stmts = self.finish(arm, span);
        let body = self.block(stmts, None, unit, span);
        HirMatchArm {
            pattern,
            guard: None,
            body,
        }
    }

    /// A struct variant's fields as object members, after the tag member
    /// when `tagged`, in their own braces otherwise.
    #[allow(
        clippy::too_many_arguments,
        reason = "one variant's parts, spelled out"
    )]
    fn encode_variant_fields(
        &mut self,
        arm: &mut Emit,
        names: &[Ident],
        options: &HirSerdeVariant,
        tagged: bool,
        tys: &[Ty],
        binds: &[String],
        span: Span,
    ) {
        let mut order: Vec<usize> = (0..names.len().min(tys.len()))
            .filter(|&i| !options.fields.get(i).is_some_and(|o| o.skip))
            .collect();
        let key_of = |i: usize| {
            options
                .fields
                .get(i)
                .and_then(|o| o.key.clone())
                .unwrap_or_else(|| names[i].name.clone())
        };
        if self.mode.sorted {
            order.sort_by(|&a, &b| key_of(a).as_bytes().cmp(key_of(b).as_bytes()));
        }
        let members = order
            .into_iter()
            .map(|i| (key_of(i), self.local(&binds[i], tys[i], span), tys[i]))
            .collect();
        self.encode_members(arm, members, !tagged, tagged, span);
    }

    /// A unit or tuple variant's payload: externally tagged, its name or a
    /// one-key object; under `#[tag]`, a one-field struct payload's members
    /// beside the tag, and the object's close.
    fn encode_variant_payload(
        &mut self,
        arm: &mut Emit,
        tag: &str,
        tag_member: Option<&str>,
        tys: &[Ty],
        binds: &[String],
        span: Span,
    ) {
        match (tag_member, tys) {
            (Some(_), []) => arm.raw("}"),
            (Some(_), [payload]) => {
                if let Some(Kind::Adt(def, adt_ty)) = self.kind(*payload)
                    && let Some(HirAdtKind::Struct(fields)) =
                        self.adts.get(&def).map(|i| i.adt.kind.clone())
                    && let Some(info) = self.adts.get(&def).cloned()
                    && let Some(field_tys) = self.struct_fields_of(def, adt_ty)
                {
                    self.encode_struct(
                        arm, &info, &fields, &field_tys, &binds[0], adt_ty, true, span,
                    );
                }
                arm.raw("}");
            }
            (Some(_), _) => arm.raw("}"),
            (None, []) => arm.raw(tag),
            (None, [payload]) => {
                arm.raw(&format!("{{{tag}:"));
                let value = self.local(&binds[0], *payload, span);
                self.encode(arm, value, *payload, span);
                arm.raw("}");
            }
            (None, _) => {
                arm.raw(&format!("{{{tag}:["));
                for (i, payload) in tys.iter().enumerate() {
                    if i > 0 {
                        arm.raw(",");
                    }
                    let value = self.local(&binds[i], *payload, span);
                    self.encode(arm, value, *payload, span);
                }
                arm.raw("]}");
            }
        }
    }

    /// The pattern matching `variant` with each payload bound to `binds`.
    fn variant_pattern(
        &mut self,
        variant: &crate::tree::HirEnumVariant,
        tys: &[Ty],
        binds: &[String],
        ty: Ty,
        span: Span,
    ) -> HirPat {
        let kind = if let Some(names) = &variant.struct_fields {
            let fields = names
                .iter()
                .zip(tys)
                .zip(binds)
                .map(|((field, field_ty), bind)| HirFieldPat {
                    name: field.clone(),
                    pattern: Some(self.binding(bind, *field_ty, span)),
                })
                .collect();
            HirPatKind::Struct {
                name: variant.name.clone(),
                fields,
                rest: false,
            }
        } else {
            let fields = tys
                .iter()
                .zip(binds)
                .map(|(t, b)| self.binding(b, *t, span))
                .collect();
            HirPatKind::Variant {
                name: variant.name.clone(),
                fields,
            }
        };
        HirPat {
            id: self.ids.next(),
            span,
            ty,
            kind,
        }
    }

    /// Appends the steps that write `value`, of type `ty`.
    fn encode(&mut self, out: &mut Emit, value: HirExpr, ty: Ty, span: Span) {
        if self.rendering && self.encode_rendered_only(out, &value, ty, span) {
            return;
        }
        let Some(kind) = self.kind(ty) else {
            out.raw("null");
            return;
        };
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        match kind {
            Kind::I64 => self.write_step(out, "wr_i64", value, span),
            Kind::Narrow(..) => {
                let wide = self.cast(value, i64_ty, span);
                self.write_step(out, "wr_i64", wide, span);
            }
            Kind::Unsigned(..) => {
                let u64_ty = self.tcx.int_ty(IntTy::U64);
                let wide = self.cast(value, u64_ty, span);
                self.write_step(out, "wr_u64", wide, span);
            }
            // A float keeps its fraction (`1.0`), as `serde_json` writes one
            // and a `json::Value` holds one; the binding wire writes the
            // shortest text its reader takes.
            Kind::F64 => {
                let step = if self.wire { "wr_f64" } else { "wr_f64_tree" };
                self.write_step(out, step, value, span);
            }
            Kind::F32 => {
                let f64_ty = self.tcx.float_ty(FloatTy::F64);
                let wide = self.cast(value, f64_ty, span);
                let step = if self.wire { "wr_f32" } else { "wr_f32_tree" };
                self.write_step(out, step, wide, span);
            }
            Kind::Bool => self.write_step(out, "wr_bool", value, span),
            Kind::Char => self.write_step(out, "wr_char", value, span),
            Kind::Str => self.write_step(out, "wr_str", value, span),
            Kind::Json => self.write_step(out, "wr_json", value, span),
            Kind::Adt(def, adt_ty) => {
                let encoder = if self.rendering {
                    self.renderer(def, adt_ty)
                } else {
                    self.encoder(def, adt_ty)
                };
                let w = self.local(WRITER, i64_ty, span);
                let call = self.call_named(&encoder, vec![w, value], unit, span);
                out.stmt(self.expr_stmt(call, span));
            }
            Kind::Seq(elem) | Kind::Array(elem, _) => self.encode_seq(out, value, ty, elem, span),
            Kind::Opt(inner) => {
                let payload = self.fresh("x");
                let mut some = Emit::default();
                if self.wire {
                    some.raw("[");
                }
                let payload_value = self.local(&payload, inner, span);
                self.encode(&mut some, payload_value, inner, span);
                if self.wire {
                    some.raw("]");
                }
                let some_stmts = self.finish(some, span);
                let some_body = self.block(some_stmts, None, unit, span);
                let mut none = Emit::default();
                none.raw(if self.wire { "[]" } else { "null" });
                let none_stmts = self.finish(none, span);
                let none_body = self.block(none_stmts, None, unit, span);
                let matched =
                    self.match_option(value, &payload, inner, some_body, none_body, unit, span);
                out.stmt(self.expr_stmt(matched, span));
            }
            Kind::Tuple(elems) => self.encode_tuple(out, value, ty, &elems, span),
            Kind::Map(value_ty) => self.encode_map(out, value, ty, value_ty, span),
            Kind::Res(ok_ty, err_ty) => {
                let (ok_name, on_ok) = self.encode_wire_arm("[0,", ok_ty, span);
                let (err_name, on_err) = self.encode_wire_arm("[1,", err_ty, span);
                let matched = self.match_result(
                    value, &ok_name, ok_ty, on_ok, &err_name, err_ty, on_err, unit, span,
                );
                out.stmt(self.expr_stmt(matched, span));
            }
        }
    }

    /// `value` as the walk reads it: a place as itself, read where it lies
    /// since the walk only reads it, and anything else computed once into a
    /// binding named after `prefix`.
    fn hold(
        &mut self,
        out: &mut Emit,
        prefix: &str,
        ty: Ty,
        value: HirExpr,
        span: Span,
    ) -> HirExpr {
        if is_plain_place(&value) {
            return value;
        }
        let name = self.fresh(prefix);
        out.stmt(self.let_stmt(&name, false, ty, value, span));
        self.local(&name, ty, span)
    }

    /// A tuple as an array of its elements.
    fn encode_tuple(&mut self, out: &mut Emit, value: HirExpr, ty: Ty, elems: &[Ty], span: Span) {
        let tuple = self.hold(out, "tuple", ty, value, span);
        out.raw("[");
        for (i, elem) in elems.iter().enumerate() {
            if i > 0 {
                out.raw(",");
            }
            let tuple_value = tuple.clone();
            let item = self.expr(
                *elem,
                span,
                HirExprKind::TupleIndex {
                    receiver: Box::new(tuple_value),
                    index: u32::try_from(i).unwrap_or(u32::MAX),
                },
            );
            self.encode(out, item, *elem, span);
        }
        out.raw("]");
    }

    /// A sequence as an array of its elements.
    fn encode_seq(&mut self, out: &mut Emit, value: HirExpr, ty: Ty, elem: Ty, span: Span) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let seq = self.hold(out, "seq", ty, value, span);
        let count = self.fresh("n");
        let receiver = seq.clone();
        let len = self.method(receiver, "len", Vec::new(), i64_ty, span);
        out.stmt(self.let_stmt(&count, false, i64_ty, len, span));
        out.raw("[");
        let index = self.fresh("i");
        let zero = self.int_lit(0, i64_ty, span);
        out.stmt(self.let_stmt(&index, true, i64_ty, zero, span));
        let mut body = Emit::default();
        let w = self.local(WRITER, i64_ty, span);
        let at = self.local(&index, i64_ty, span);
        let separator = self.step("wr_sep", vec![w, at], unit, span);
        body.stmt(self.expr_stmt(separator, span));
        let receiver = seq.clone();
        let at = self.local(&index, i64_ty, span);
        let item = self.index(receiver, at, elem, span);
        self.encode(&mut body, item, elem, span);
        body.stmt(self.increment(&index, span));
        let body = self.finish(body, span);
        out.stmt(self.while_below(&index, &count, body, span));
        out.raw("]");
    }

    /// A `String`-keyed map as an object, its keys in byte order so the text
    /// is the same on every tier. Every tier answers such a map's `keys()` in
    /// that order already, so the walk takes them as they come.
    fn encode_map(&mut self, out: &mut Emit, value: HirExpr, ty: Ty, value_ty: Ty, span: Span) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let string_ty = self.tcx.string_ty();
        let names_ty = self.tcx.intern(TyKind::Vec(string_ty));
        let map = self.hold(out, "map", ty, value, span);
        let names = self.fresh("keys");
        let receiver = map.clone();
        let all = self.method(receiver, "keys", Vec::new(), names_ty, span);
        out.stmt(self.let_stmt(&names, false, names_ty, all, span));
        let count = self.fresh("n");
        let receiver = self.local(&names, names_ty, span);
        let len = self.method(receiver, "len", Vec::new(), i64_ty, span);
        out.stmt(self.let_stmt(&count, false, i64_ty, len, span));
        out.raw("{");
        let index = self.fresh("i");
        let zero = self.int_lit(0, i64_ty, span);
        out.stmt(self.let_stmt(&index, true, i64_ty, zero, span));
        let mut body = Emit::default();
        let key = self.fresh("key");
        let receiver = self.local(&names, names_ty, span);
        let at = self.local(&index, i64_ty, span);
        let name = self.index(receiver, at, string_ty, span);
        body.stmt(self.let_stmt(&key, false, string_ty, name, span));
        let w = self.local(WRITER, i64_ty, span);
        let at = self.local(&index, i64_ty, span);
        let separator = self.step("wr_sep", vec![w, at], unit, span);
        body.stmt(self.expr_stmt(separator, span));
        let name = self.local(&key, string_ty, span);
        self.write_step(&mut body, "wr_str", name, span);
        body.raw(":");
        // A map read by key answers an `Option`; every key came from the map,
        // so the `None` arm never runs.
        let receiver = map.clone();
        let name = self.local(&key, string_ty, span);
        let option_ty = self.option_ty(value_ty);
        let entry = self.method(receiver, "get", vec![name], option_ty, span);
        let payload = self.fresh("x");
        let mut some = Emit::default();
        let payload_value = self.local(&payload, value_ty, span);
        self.encode(&mut some, payload_value, value_ty, span);
        let some_stmts = self.finish(some, span);
        let some_body = self.block(some_stmts, None, unit, span);
        let none_body = self.block(Vec::new(), None, unit, span);
        let matched =
            self.match_option(entry, &payload, value_ty, some_body, none_body, unit, span);
        body.stmt(self.expr_stmt(matched, span));
        body.stmt(self.increment(&index, span));
        let body = self.finish(body, span);
        out.stmt(self.while_below(&index, &count, body, span));
        out.raw("}");
    }

    fn decode_fn(&mut self, def: DefId, ty: Ty, name: &str) -> Option<HirItem> {
        let info = self.adts.get(&def)?.clone();
        let span = self.adt_span();
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let mut out = Emit::default();
        let tail = match &info.adt.kind {
            HirAdtKind::Struct(fields) => {
                let field_tys = self.struct_fields_of(def, ty)?;
                self.decode_struct(&mut out, &info, fields, &field_tys, ty, span)
            }
            HirAdtKind::Enum(variants) => {
                let variant_tys = self.variant_fields_of(def, ty)?;
                self.decode_enum(&mut out, def, &info, variants, &variant_tys, ty, span)
            }
        };
        let stmts = self.finish(out, span);
        let params = vec![self.param(READER, i64_ty, span)];
        Some(self.function(name, params, ty, stmts, tail, span))
    }

    /// A struct read field by field in declaration order. A tuple struct's
    /// fields are its positions, and it reports them as elements.
    fn decode_struct(
        &mut self,
        out: &mut Emit,
        info: &AdtInfo,
        fields: &[Ident],
        field_tys: &[Ty],
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let options: Vec<HirSerdeField> = (0..fields.len())
            .map(|i| info.serde.fields.get(i).cloned().unwrap_or_default())
            .collect();
        let tuple_struct = fields.first().is_some_and(|f| f.name == "0");
        let values = self.read_named(out, fields, field_tys, &options, tuple_struct, info, span);
        self.struct_lit(&info.path, values, ty, span)
    }

    /// Begins the current object and reads each named field from it, under
    /// its key, skipped fields as their default.
    #[allow(clippy::too_many_arguments, reason = "one record's parts, spelled out")]
    fn read_named(
        &mut self,
        out: &mut Emit,
        fields: &[Ident],
        field_tys: &[Ty],
        options: &[HirSerdeField],
        positional: bool,
        info: &AdtInfo,
        span: Span,
    ) -> Vec<(String, HirExpr)> {
        let key_of = |i: usize| {
            options
                .get(i)
                .and_then(|o| o.key.clone())
                .unwrap_or_else(|| fields[i].name.clone())
        };
        let keys: Vec<String> = (0..fields.len())
            .filter(|&i| !options.get(i).is_some_and(|o| o.skip))
            .map(key_of)
            .collect();
        self.begin_struct(out, &keys, span);
        let mut values = Vec::new();
        let mut read_index = 0i64;
        for (index, (field, field_ty)) in fields.iter().zip(field_tys).enumerate() {
            let local = self.fresh("f");
            let field_options = options.get(index).cloned().unwrap_or_default();
            let value = if field_options.skip {
                self.field_default(field_options.default.as_ref(), *field_ty, info, span)
            } else {
                let key = key_of(index);
                let missing = if positional {
                    format!("missing element `{key}`")
                } else {
                    format!("missing field `{key}`")
                };
                let absent = match field_options.default {
                    Some(default) => Absent::Default(default),
                    None => Absent::Fail(missing),
                };
                let value = self.read_field(read_index, *field_ty, absent, info, span);
                read_index += 1;
                value
            };
            out.stmt(self.let_stmt(&local, false, *field_ty, value, span));
            values.push((field.name.clone(), self.local(&local, *field_ty, span)));
        }
        values
    }

    /// What a missing or skipped field reads as: its type's zero value (a
    /// user type's `default()`), or the value `#[default(value)]` names.
    fn field_default(
        &mut self,
        default: Option<&FieldDefault>,
        ty: Ty,
        info: &AdtInfo,
        span: Span,
    ) -> HirExpr {
        match default {
            Some(FieldDefault::Value(value)) => self.literal_default(value, ty, span),
            Some(FieldDefault::Declared(function)) => {
                let mut path = info.segments.clone();
                path.pop();
                path.push(function.clone());
                self.call_segments(&path, Vec::new(), ty, span)
            }
            Some(FieldDefault::Zero) | None => self.default_value(ty, info, span),
        }
    }

    /// The value `#[default(value)]` spells, as a `ty`.
    fn literal_default(&mut self, value: &DefaultValue, ty: Ty, span: Span) -> HirExpr {
        let kind = self.kind(ty);
        if let Some(Kind::Opt(inner)) = kind {
            if *value == DefaultValue::None {
                return self.none(ty, span);
            }
            let inner_value = self.literal_default(value, inner, span);
            return self.ctor("Some", vec![inner_value], ty, span);
        }
        let target = self.peel(ty);
        let number = |this: &mut Self, text: &str, float: bool| {
            let (negative, digits) = match text.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, text),
            };
            let literal = if float {
                let spelled = if digits.contains(['.', 'e', 'E']) {
                    digits.to_string()
                } else {
                    format!("{digits}.0")
                };
                HirLiteral::Float(spelled)
            } else {
                HirLiteral::Int(digits.to_string())
            };
            let positive = this.expr(target, span, HirExprKind::Literal(literal));
            if negative {
                this.expr(
                    target,
                    span,
                    HirExprKind::Unary {
                        op: crate::tree::HirUnaryOp::Neg,
                        operand: Box::new(positive),
                    },
                )
            } else {
                positive
            }
        };
        match value {
            DefaultValue::Int(text) => {
                let float = matches!(kind, Some(Kind::F64 | Kind::F32));
                number(self, text, float)
            }
            DefaultValue::Float(text) => number(self, text, true),
            DefaultValue::Bool(b) => {
                self.expr(target, span, HirExprKind::Literal(HirLiteral::Bool(*b)))
            }
            DefaultValue::Str(text) => self.string_lit(text, span),
            DefaultValue::Char(c) => {
                self.expr(target, span, HirExprKind::Literal(HirLiteral::Char(*c)))
            }
            DefaultValue::None => self.zero_value(ty, span),
            DefaultValue::Variant(name) => match kind {
                Some(Kind::Adt(def, _)) => {
                    let mut segments = self
                        .adts
                        .get(&def)
                        .map(|adt| adt.segments.clone())
                        .unwrap_or_default();
                    segments.push(name.clone());
                    self.path_segments(&segments, ty, span)
                }
                _ => self.zero_value(ty, span),
            },
        }
    }

    /// An enum: the variant the reader selects, then its payload.
    #[allow(clippy::too_many_arguments, reason = "one enum's parts, spelled out")]
    fn decode_enum(
        &mut self,
        out: &mut Emit,
        def: DefId,
        info: &AdtInfo,
        variants: &[crate::tree::HirEnumVariant],
        variant_tys: &[Vec<Ty>],
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let keys: Vec<String> = variants
            .iter()
            .enumerate()
            .map(|(i, v)| {
                info.serde
                    .variants
                    .get(i)
                    .map(|o| o.key.clone())
                    .filter(|k| !k.is_empty())
                    .unwrap_or_else(|| v.name.name.clone())
            })
            .collect();
        let table = encode_variants(
            variants
                .iter()
                .zip(variant_tys)
                .zip(&keys)
                .map(|((v, tys), key)| (tys.is_empty() && v.struct_fields.is_none(), key.as_str())),
        );
        let fallback = self.fallback_variant(def);
        let r = self.local(READER, i64_ty, span);
        let table = self.string_lit(&table, span);
        let bare = self.string_lit(&info.adt.name.name, span);
        let fallback_lit = self.int_lit(i64::try_from(fallback).unwrap_or(0), i64_ty, span);
        let tagged = info.serde.tag.clone();
        let chosen = match &tagged {
            Some(member) => {
                let member = self.string_lit(member, span);
                self.step(
                    "rd_tagged",
                    vec![r, table, member, bare, fallback_lit],
                    i64_ty,
                    span,
                )
            }
            None => self.step("rd_enum", vec![r, table, bare, fallback_lit], i64_ty, span),
        };
        out.stmt(self.let_stmt("__serde_variant", false, i64_ty, chosen, span));
        let mut arms = Vec::new();
        let last = variants.len().saturating_sub(1);
        for (index, (variant, tys)) in variants.iter().zip(variant_tys).enumerate() {
            let options = info.serde.variants.get(index).cloned().unwrap_or_default();
            let body =
                self.decode_variant(info, variant, &options, tagged.is_some(), tys, ty, span);
            let kind = if index == last {
                HirPatKind::Wildcard
            } else {
                HirPatKind::Literal(HirLiteral::Int(index.to_string()))
            };
            let pattern = HirPat {
                id: self.ids.next(),
                span,
                ty: i64_ty,
                kind,
            };
            arms.push(HirMatchArm {
                pattern,
                guard: None,
                body,
            });
        }
        let scrutinee = self.local("__serde_variant", i64_ty, span);
        self.expr(
            ty,
            span,
            HirExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
        )
    }

    /// The payload of one variant. Externally tagged, the reader stands on
    /// the payload and steps out after it; under `#[tag]` the variant's fields
    /// (or its struct payload's) sit in the object the reader stands on.
    #[allow(
        clippy::too_many_arguments,
        reason = "one variant's parts, spelled out"
    )]
    fn decode_variant(
        &mut self,
        info: &AdtInfo,
        variant: &crate::tree::HirEnumVariant,
        options: &HirSerdeVariant,
        tagged: bool,
        tys: &[Ty],
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let vname = variant.name.name.clone();
        let mut segments = info.segments.clone();
        segments.push(vname.clone());
        let mut out = Emit::default();
        let value = match (&variant.struct_fields, tys.len()) {
            (None, 0) => self.path_segments(&segments, ty, span),
            (None, 1) => {
                // Read before stepping out: the payload is the value stood on.
                let payload = self.decode(&mut out, tys[0], span);
                out.stmt(self.let_stmt("__serde_p0", false, tys[0], payload, span));
                if !tagged {
                    out.stmt(self.leave_stmt(span));
                }
                let payload = self.local("__serde_p0", tys[0], span);
                self.call_segments(&segments, vec![payload], ty, span)
            }
            (None, _) if tagged => {
                let r = self.reader(span);
                let message = self.string_lit(
                    &format!("variant `{vname}` has no form inside a tagged object"),
                    span,
                );
                let fail = self.step("rd_fail", vec![r, message], unit, span);
                out.stmt(self.expr_stmt(fail, span));
                let args = tys.iter().map(|t| self.zero_value(*t, span)).collect();
                self.call_segments(&segments, args, ty, span)
            }
            (None, _) => {
                let mut args = Vec::new();
                for (i, payload_ty) in tys.iter().enumerate() {
                    let r = self.local(READER, i64_ty, span);
                    let at = self.int_lit(i64::try_from(i).unwrap_or(0), i64_ty, span);
                    let step = self.step("rd_at", vec![r, at], unit, span);
                    out.stmt(self.expr_stmt(step, span));
                    let value = self.decode(&mut out, *payload_ty, span);
                    let local = format!("__serde_p{i}");
                    out.stmt(self.let_stmt(&local, false, *payload_ty, value, span));
                    out.stmt(self.leave_stmt(span));
                    args.push(self.local(&local, *payload_ty, span));
                }
                out.stmt(self.leave_stmt(span));
                self.call_segments(&segments, args, ty, span)
            }
            (Some(names), _) => {
                let values =
                    self.read_named(&mut out, names, tys, &options.fields, false, info, span);
                if !tagged {
                    out.stmt(self.leave_stmt(span));
                }
                self.struct_lit(&vname, values, ty, span)
            }
        };
        let stmts = self.finish(out, span);
        self.block(stmts, Some(value), ty, span)
    }

    /// `rd_struct(r, keys)`.
    fn begin_struct(&mut self, out: &mut Emit, keys: &[String], span: Span) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let r = self.local(READER, i64_ty, span);
        let encoded = self.string_lit(&encode_names(keys.iter().map(String::as_str)), span);
        let step = self.step("rd_struct", vec![r, encoded], unit, span);
        out.stmt(self.expr_stmt(step, span));
    }

    /// The value of struct field `index`: decoded when the document has it,
    /// and otherwise `None` for an `Option`, the default under `#[default]`,
    /// or a recorded failure.
    fn read_field(
        &mut self,
        index: i64,
        ty: Ty,
        absent: Absent,
        info: &AdtInfo,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let unit = self.tcx.unit();
        let r = self.local(READER, i64_ty, span);
        let at = self.int_lit(index, i64_ty, span);
        let present = self.step("rd_field", vec![r, at], bool_ty, span);
        let mut found = Emit::default();
        let value = self.decode(&mut found, ty, span);
        let local = self.fresh("v");
        found.stmt(self.let_stmt(&local, false, ty, value, span));
        found.stmt(self.leave_stmt(span));
        let found_stmts = self.finish(found, span);
        let found_value = self.local(&local, ty, span);
        let then = self.block(found_stmts, Some(found_value), ty, span);
        let otherwise = match absent {
            Absent::Default(default) => self.field_default(Some(&default), ty, info, span),
            Absent::Fail(_) if matches!(self.kind(ty), Some(Kind::Opt(_))) => self.none(ty, span),
            Absent::Fail(message) => {
                let r = self.local(READER, i64_ty, span);
                let message = self.string_lit(&message, span);
                let fail = self.step("rd_fail", vec![r, message], unit, span);
                let fail = self.expr_stmt(fail, span);
                let zero = self.zero_value(ty, span);
                self.block(vec![fail], Some(zero), ty, span)
            }
        };
        self.if_else(present, then, otherwise, ty, span)
    }

    /// Appends the steps that read a `ty` at the current value and answers
    /// the expression holding it.
    fn decode(&mut self, out: &mut Emit, ty: Ty, span: Span) -> HirExpr {
        let Some(kind) = self.kind(ty) else {
            return self.zero_value(ty, span);
        };
        match kind {
            Kind::Adt(def, adt_ty) => self.decode_adt(out, def, adt_ty, ty, span),
            Kind::Seq(elem) => self.decode_seq(out, ty, elem, span),
            Kind::Array(elem, len) => self.decode_array(out, ty, elem, len, span),
            Kind::Opt(inner) if self.wire => self.decode_wire_option(out, ty, inner, span),
            Kind::Res(ok_ty, err_ty) => self.decode_wire_result(out, ty, ok_ty, err_ty, span),
            Kind::Opt(inner) => {
                let mut some = Emit::default();
                let value = self.decode(&mut some, inner, span);
                let wrapped = self.ctor("Some", vec![value], ty, span);
                let some_stmts = self.finish(some, span);
                let some_block = self.block(some_stmts, Some(wrapped), ty, span);
                // `Option<Option<T>>`: a present `null` is `Some(None)`, which
                // the inner `Option` reads; only a missing member is `None`.
                if matches!(self.kind(inner), Some(Kind::Opt(_))) {
                    return some_block;
                }
                let bool_ty = self.tcx.bool_ty();
                let r = self.reader(span);
                let is_null = self.step("rd_is_null", vec![r], bool_ty, span);
                let none = self.none(ty, span);
                self.if_else(is_null, none, some_block, ty, span)
            }
            Kind::Tuple(elems) => self.decode_tuple(out, ty, &elems, span),
            Kind::Map(value_ty) => self.decode_map(out, ty, value_ty, span),
            scalar => self.decode_scalar(&scalar, ty, span),
        }
    }

    /// A scalar, read by the step for its kind and cast to its width.
    fn decode_scalar(&mut self, kind: &Kind, ty: Ty, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let f64_ty = self.tcx.float_ty(FloatTy::F64);
        let r = self.reader(span);
        let read = match kind {
            Kind::Narrow(spelled) => {
                let (min, max) = int_bounds(spelled);
                let expected = self.string_lit(spelled, span);
                let min = self.int_lit(min, i64_ty, span);
                let max = self.int_lit(max, i64_ty, span);
                self.step("rd_int", vec![r, expected, min, max], i64_ty, span)
            }
            Kind::Json => return self.step("rd_json", vec![r], ty, span),
            Kind::Adt(..)
            | Kind::Seq(_)
            | Kind::Array(..)
            | Kind::Opt(_)
            | Kind::Tuple(_)
            | Kind::Map(_)
            | Kind::Res(..) => {
                return self.zero_value(ty, span);
            }
            Kind::I64
            | Kind::Unsigned(_)
            | Kind::F64
            | Kind::F32
            | Kind::Bool
            | Kind::Char
            | Kind::Str => {
                let (step, spelled, read_ty) = match kind {
                    Kind::Unsigned(spelled) => ("rd_u64", *spelled, self.tcx.int_ty(IntTy::U64)),
                    Kind::F64 => ("rd_f64", "f64", f64_ty),
                    Kind::F32 => ("rd_f64", "f32", f64_ty),
                    Kind::Bool => ("rd_bool", "bool", self.tcx.bool_ty()),
                    Kind::Char => ("rd_char", "char", self.tcx.char_ty()),
                    Kind::Str => ("rd_str", "string", self.tcx.string_ty()),
                    _ => ("rd_i64", "i64", i64_ty),
                };
                let expected = self.string_lit(spelled, span);
                self.step(step, vec![r, expected], read_ty, span)
            }
        };
        // A newtype field is its representation here.
        let target = self.peel(ty);
        self.cast(read, target, span)
    }

    /// A user type, decoded by its own function.
    fn decode_adt(
        &mut self,
        out: &mut Emit,
        def: DefId,
        adt_ty: Ty,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let callee = self.decoder(def, adt_ty);
        let value = self.fresh("v");
        let r = self.reader(span);
        let read = self.call_named(&callee, vec![r], ty, span);
        out.stmt(self.let_stmt(&value, false, ty, read, span));
        self.local(&value, ty, span)
    }

    /// A sequence, element by element.
    fn decode_seq(&mut self, out: &mut Emit, ty: Ty, elem: Ty, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let count = self.fresh("n");
        let r = self.reader(span);
        let expected = self.string_lit("array", span);
        let begun = self.step("rd_array", vec![r, expected], i64_ty, span);
        out.stmt(self.let_stmt(&count, false, i64_ty, begun, span));
        let seq = self.fresh("seq");
        let capacity = self.local(&count, i64_ty, span);
        let fresh = self.call_path(&["Vec", "with_capacity"], vec![capacity], ty, span);
        out.stmt(self.let_stmt(&seq, true, ty, fresh, span));
        let index = self.fresh("i");
        let zero = self.int_lit(0, i64_ty, span);
        out.stmt(self.let_stmt(&index, true, i64_ty, zero, span));
        let mut body = Emit::default();
        let item = self.decode_elem(&mut body, &index, elem, span);
        let unit = self.tcx.unit();
        let receiver = self.local(&seq, ty, span);
        let push = self.method(receiver, "push", vec![item], unit, span);
        body.stmt(self.expr_stmt(push, span));
        body.stmt(self.increment(&index, span));
        let body = self.finish(body, span);
        out.stmt(self.while_below(&index, &count, body, span));
        self.local(&seq, ty, span)
    }

    /// Element `index` (a local) of the array begun: stepped into, read, and
    /// stepped out of, answering the local holding it.
    fn decode_elem(&mut self, body: &mut Emit, index: &str, elem: Ty, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let r = self.reader(span);
        let at = self.local(index, i64_ty, span);
        let enter = self.step("rd_elem", vec![r, at], unit, span);
        body.stmt(self.expr_stmt(enter, span));
        let item = self.decode(body, elem, span);
        let item_local = self.fresh("x");
        body.stmt(self.let_stmt(&item_local, false, elem, item, span));
        body.stmt(self.leave_stmt(span));
        self.local(&item_local, elem, span)
    }

    /// A fixed array, from a JSON array of exactly its length. An element
    /// type whose zero value is plain fills a repeat literal in a loop; one
    /// that builds a user type is read element by element into a list.
    fn decode_array(
        &mut self,
        out: &mut Emit,
        ty: Ty,
        elem: Ty,
        len: usize,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let r = self.reader(span);
        let count = self.int_lit(i64::try_from(len).unwrap_or(i64::MAX), i64_ty, span);
        let not_array = self.string_lit("array", span);
        let wrong_len = self.string_lit(&format!("{len}-element array"), span);
        let begin = self.step("rd_tuple", vec![r, count, not_array, wrong_len], unit, span);
        out.stmt(self.expr_stmt(begin, span));
        if self.reaches_adt_directly(elem) {
            let mut items = Vec::with_capacity(len);
            for i in 0..len {
                let index = self.fresh("i");
                let at = self.int_lit(i64::try_from(i).unwrap_or(0), i64_ty, span);
                out.stmt(self.let_stmt(&index, false, i64_ty, at, span));
                items.push(self.decode_elem(out, &index, elem, span));
            }
            return self.expr(
                ty,
                span,
                HirExprKind::Array(crate::tree::HirArrayExpr::List(items)),
            );
        }
        let array = self.fresh("array");
        let zero = self.zero_value(elem, span);
        let count = self.int_lit(i64::try_from(len).unwrap_or(i64::MAX), i64_ty, span);
        let filled = self.expr(
            ty,
            span,
            HirExprKind::Array(crate::tree::HirArrayExpr::Repeat {
                value: Box::new(zero),
                count: Box::new(count),
            }),
        );
        out.stmt(self.let_stmt(&array, true, ty, filled, span));
        let index = self.fresh("i");
        let start = self.int_lit(0, i64_ty, span);
        out.stmt(self.let_stmt(&index, true, i64_ty, start, span));
        let bound = self.fresh("n");
        let total = self.int_lit(i64::try_from(len).unwrap_or(i64::MAX), i64_ty, span);
        out.stmt(self.let_stmt(&bound, false, i64_ty, total, span));
        let mut body = Emit::default();
        let item = self.decode_elem(&mut body, &index, elem, span);
        let base = self.local(&array, ty, span);
        let at = self.local(&index, i64_ty, span);
        let place = self.index(base, at, elem, span);
        let store = self.expr(
            unit,
            span,
            HirExprKind::Assign {
                place: Box::new(place),
                value: Box::new(item),
            },
        );
        body.stmt(self.expr_stmt(store, span));
        body.stmt(self.increment(&index, span));
        let body = self.finish(body, span);
        out.stmt(self.while_below(&index, &bound, body, span));
        self.local(&array, ty, span)
    }

    /// A tuple, from an array of exactly its length.
    fn decode_tuple(&mut self, out: &mut Emit, ty: Ty, elems: &[Ty], span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let r = self.reader(span);
        let len = self.int_lit(i64::try_from(elems.len()).unwrap_or(0), i64_ty, span);
        let not_array = self.string_lit("array", span);
        let wrong_len = self.string_lit(&format!("{}-element array", elems.len()), span);
        let begin = self.step("rd_tuple", vec![r, len, not_array, wrong_len], unit, span);
        out.stmt(self.expr_stmt(begin, span));
        let mut items = Vec::new();
        for (i, elem) in elems.iter().enumerate() {
            let r = self.reader(span);
            let at = self.int_lit(i64::try_from(i).unwrap_or(0), i64_ty, span);
            let enter = self.step("rd_elem", vec![r, at], unit, span);
            out.stmt(self.expr_stmt(enter, span));
            let value = self.decode(out, *elem, span);
            let local = self.fresh("e");
            out.stmt(self.let_stmt(&local, false, *elem, value, span));
            out.stmt(self.leave_stmt(span));
            items.push(self.local(&local, *elem, span));
        }
        self.expr(ty, span, HirExprKind::Tuple(items))
    }

    /// A `String`-keyed map, entry by entry in key order.
    fn decode_map(&mut self, out: &mut Emit, ty: Ty, value_ty: Ty, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let string_ty = self.tcx.string_ty();
        let count = self.fresh("n");
        let r = self.reader(span);
        let expected = self.string_lit("object", span);
        let begun = self.step("rd_map", vec![r, expected], i64_ty, span);
        out.stmt(self.let_stmt(&count, false, i64_ty, begun, span));
        let map = self.fresh("map");
        let fresh = self.call_path(&["Map", "new"], Vec::new(), ty, span);
        out.stmt(self.let_stmt(&map, true, ty, fresh, span));
        let index = self.fresh("i");
        let zero = self.int_lit(0, i64_ty, span);
        out.stmt(self.let_stmt(&index, true, i64_ty, zero, span));
        let mut body = Emit::default();
        let key = self.fresh("key");
        let r = self.reader(span);
        let at = self.local(&index, i64_ty, span);
        let name = self.step("rd_map_key", vec![r, at], string_ty, span);
        body.stmt(self.let_stmt(&key, false, string_ty, name, span));
        let r = self.reader(span);
        let at = self.local(&index, i64_ty, span);
        let enter = self.step("rd_map_value", vec![r, at], unit, span);
        body.stmt(self.expr_stmt(enter, span));
        let entry = self.decode(&mut body, value_ty, span);
        let entry_local = self.fresh("x");
        body.stmt(self.let_stmt(&entry_local, false, value_ty, entry, span));
        body.stmt(self.leave_stmt(span));
        let receiver = self.local(&map, ty, span);
        let name = self.local(&key, string_ty, span);
        let entry_value = self.local(&entry_local, value_ty, span);
        let option_ty = self.option_ty(value_ty);
        let insert = self.method(receiver, "insert", vec![name, entry_value], option_ty, span);
        body.stmt(self.expr_stmt(insert, span));
        body.stmt(self.increment(&index, span));
        let body = self.finish(body, span);
        out.stmt(self.while_below(&index, &count, body, span));
        self.local(&map, ty, span)
    }

    /// `[value; len]`.
    fn repeat(&mut self, value: HirExpr, len: usize, ty: Ty, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let count = self.int_lit(i64::try_from(len).unwrap_or(i64::MAX), i64_ty, span);
        self.expr(
            ty,
            span,
            HirExprKind::Array(crate::tree::HirArrayExpr::Repeat {
                value: Box::new(value),
                count: Box::new(count),
            }),
        )
    }

    fn reader(&mut self, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        self.local(READER, i64_ty, span)
    }

    /// A value of `ty` for a read that has already failed: zeros, empties,
    /// `None`, and for a user type its decode, which reads zeros once failed.
    fn zero_value(&mut self, ty: Ty, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        match self.kind(ty) {
            Some(Kind::Adt(def, adt_ty)) => {
                let decoder = self.decoder(def, adt_ty);
                let r = self.local(READER, i64_ty, span);
                self.call_named(&decoder, vec![r], ty, span)
            }
            Some(Kind::Tuple(elems)) => {
                let items = elems.iter().map(|e| self.zero_value(*e, span)).collect();
                self.expr(ty, span, HirExprKind::Tuple(items))
            }
            Some(Kind::Array(elem, len)) => {
                let zero = self.zero_value(elem, span);
                self.repeat(zero, len, ty, span)
            }
            Some(kind) => self.scalar_zero(&kind, ty, span),
            None => self.unit_lit(span),
        }
    }

    /// The value `#[default]` and `#[skip]` fill a field with: zeros and
    /// empties, and a user type's own `default()`.
    fn default_value(&mut self, ty: Ty, info: &AdtInfo, span: Span) -> HirExpr {
        match self.kind(ty) {
            Some(Kind::Adt(def, _)) => {
                let segments = self
                    .adts
                    .get(&def)
                    .map_or_else(|| info.segments.clone(), |adt| adt.segments.clone());
                let mut path = segments;
                path.push("default".to_string());
                self.call_segments(&path, Vec::new(), ty, span)
            }
            Some(Kind::Tuple(elems)) => {
                let items = elems
                    .iter()
                    .map(|e| self.default_value(*e, info, span))
                    .collect();
                self.expr(ty, span, HirExprKind::Tuple(items))
            }
            Some(Kind::Array(elem, len)) => {
                let value = self.default_value(elem, info, span);
                self.repeat(value, len, ty, span)
            }
            Some(kind) => self.scalar_zero(&kind, ty, span),
            None => self.unit_lit(span),
        }
    }

    fn scalar_zero(&mut self, kind: &Kind, ty: Ty, span: Span) -> HirExpr {
        match kind {
            Kind::I64 | Kind::Narrow(..) | Kind::Unsigned(..) => self.int_lit(0, ty, span),
            Kind::F64 | Kind::F32 => self.expr(
                ty,
                span,
                HirExprKind::Literal(HirLiteral::Float("0.0".to_string())),
            ),
            Kind::Bool => self.expr(ty, span, HirExprKind::Literal(HirLiteral::Bool(false))),
            Kind::Char => self.expr(ty, span, HirExprKind::Literal(HirLiteral::Char('\0'))),
            Kind::Str => self.string_lit("", span),
            Kind::Json => self.path_segments(
                &["json".to_string(), "Value".to_string(), "Null".to_string()],
                ty,
                span,
            ),
            Kind::Seq(_) => {
                let i64_ty = self.tcx.int_ty(IntTy::I64);
                let zero = self.int_lit(0, i64_ty, span);
                self.call_path(&["Vec", "with_capacity"], vec![zero], ty, span)
            }
            Kind::Opt(_) => self.none(ty, span),
            Kind::Map(_) => self.call_path(&["Map", "new"], Vec::new(), ty, span),
            Kind::Res(ok_ty, _) => {
                let ok = self.zero_value(*ok_ty, span);
                self.ctor("Ok", vec![ok], ty, span)
            }
            Kind::Tuple(_) | Kind::Adt(..) | Kind::Array(..) => self.unit_lit(span),
        }
    }

    /// The variant a failed decode answers: one that does not reach back
    /// into its own enum, so the zero value it builds is finite.
    fn fallback_variant(&self, def: DefId) -> usize {
        let Some(info) = self.adts.get(&def) else {
            return 0;
        };
        let Some(tys) = self.tcx.enum_variant_tys(def) else {
            return 0;
        };
        if let Some(unit) = info
            .adt
            .kind
            .variants()
            .iter()
            .zip(tys)
            .position(|(v, t)| t.is_empty() && v.struct_fields.is_none())
        {
            return unit;
        }
        tys.iter()
            .position(|payload| payload.iter().all(|t| !self.reaches_adt_directly(*t)))
            .unwrap_or(0)
    }

    /// Whether a zero value of `ty` builds a user type: a sequence, map, or
    /// `Option` zero is empty and builds none.
    fn reaches_adt_directly(&self, ty: Ty) -> bool {
        match self.tcx.kind_of(self.tcx.peel_nominal(ty)) {
            TyKind::Adt { def, .. } => def.local < u32::MAX - 1,
            TyKind::Tuple(elems) => elems.iter().any(|e| self.reaches_adt_directly(*e)),
            _ => false,
        }
    }

    fn kind(&mut self, ty: Ty) -> Option<Kind> {
        let peeled = self.peel(ty);
        Some(match self.tcx.kind_of(peeled).clone() {
            TyKind::Int(IntTy::I64) => Kind::I64,
            TyKind::Int(int @ (IntTy::U64 | IntTy::Usize)) => Kind::Unsigned(int_name(int)),
            TyKind::Int(int) => Kind::Narrow(int_name(int)),
            TyKind::Float(FloatTy::F32) => Kind::F32,
            TyKind::Float(_) => Kind::F64,
            TyKind::Bool => Kind::Bool,
            TyKind::Char => Kind::Char,
            TyKind::String => Kind::Str,
            TyKind::JsonValue => Kind::Json,
            TyKind::Vec(elem) | TyKind::Slice(elem) => Kind::Seq(elem),
            TyKind::Tuple(elems) => Kind::Tuple(elems),
            TyKind::HashMap { value, .. } => Kind::Map(value),
            TyKind::Adt { def, substs } if def.local == u32::MAX - 1 => {
                Kind::Opt(*substs.types().first()?)
            }
            TyKind::Adt { def, substs } if self.wire && def.local == u32::MAX => {
                let types = substs.types();
                Kind::Res(*types.first()?, *types.get(1)?)
            }
            TyKind::Array { elem, .. } if self.wire => Kind::Seq(elem),
            TyKind::Array {
                elem,
                len: gossamer_types::ArrayLen::Concrete(len),
            } => Kind::Array(elem, len),
            TyKind::Adt { def, .. } if self.adts.contains_key(&def) => Kind::Adt(def, peeled),
            _ => return None,
        })
    }

    fn peel(&self, mut ty: Ty) -> Ty {
        loop {
            let next = match self.tcx.kind_of(ty) {
                TyKind::Ref { inner, .. } => *inner,
                _ => self.tcx.peel_nominal(ty),
            };
            if next == ty {
                return ty;
            }
            ty = next;
        }
    }

    fn result_ok_ty(&self, ret: Ty) -> Option<Ty> {
        match self.tcx.kind_of(ret) {
            TyKind::Adt { def, substs } if def.local == u32::MAX => substs.types().first().copied(),
            _ => None,
        }
    }

    fn result_err_ty(&self, ret: Ty) -> Option<Ty> {
        match self.tcx.kind_of(ret) {
            TyKind::Adt { def, substs } if def.local == u32::MAX => substs.types().get(1).copied(),
            _ => None,
        }
    }

    fn option_ty(&mut self, payload: Ty) -> Ty {
        let def = DefId::local(u32::MAX - 1);
        self.tcx.register_def_name(def, "Option");
        self.tcx.intern(TyKind::Adt {
            def,
            substs: Substs::from_types([payload]),
        })
    }

    fn adt_span(&self) -> Span {
        self.span
    }

    // ---- building blocks -------------------------------------------------

    fn fresh(&mut self, role: &str) -> String {
        self.temp += 1;
        format!("__serde_{role}{}", self.temp)
    }

    fn expr(&mut self, ty: Ty, span: Span, kind: HirExprKind) -> HirExpr {
        HirExpr {
            id: self.ids.next(),
            span,
            ty,
            kind,
        }
    }

    fn path_segments(&mut self, segments: &[String], ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Path {
                segments: segments.iter().map(Ident::new).collect(),
                def: None,
            },
        )
    }

    fn local(&mut self, name: &str, ty: Ty, span: Span) -> HirExpr {
        self.path_segments(&[name.to_string()], ty, span)
    }

    fn int_lit(&mut self, value: i64, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Literal(HirLiteral::Int(value.to_string())),
        )
    }

    fn string_lit(&mut self, text: &str, span: Span) -> HirExpr {
        let string = self.tcx.string_ty();
        self.expr(
            string,
            span,
            HirExprKind::Literal(HirLiteral::String(text.to_string())),
        )
    }

    fn unit_lit(&mut self, span: Span) -> HirExpr {
        let unit = self.tcx.unit();
        self.expr(unit, span, HirExprKind::Literal(HirLiteral::Unit))
    }

    fn field(&mut self, receiver: HirExpr, name: &str, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Field {
                receiver: Box::new(receiver),
                name: Ident::new(name),
            },
        )
    }

    fn index(&mut self, base: HirExpr, index: HirExpr, ty: Ty, span: Span) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::Index {
                base: Box::new(base),
                index: Box::new(index),
            },
        )
    }

    fn method(
        &mut self,
        receiver: HirExpr,
        name: &str,
        args: Vec<HirExpr>,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::MethodCall {
                receiver: Box::new(receiver),
                name: Ident::new(name),
                args,
                owner: None,
            },
        )
    }

    fn cast(&mut self, value: HirExpr, ty: Ty, span: Span) -> HirExpr {
        if value.ty == ty {
            return value;
        }
        self.expr(
            ty,
            span,
            HirExprKind::Cast {
                value: Box::new(value),
                ty,
            },
        )
    }

    fn call_named(&mut self, name: &str, args: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        self.call_segments(&[name.to_string()], args, ty, span)
    }

    fn call_path(&mut self, segments: &[&str], args: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        let segments: Vec<String> = segments.iter().map(|s| (*s).to_string()).collect();
        self.call_segments(&segments, args, ty, span)
    }

    fn call_segments(
        &mut self,
        segments: &[String],
        args: Vec<HirExpr>,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let unit = self.tcx.unit();
        let callee = self.path_segments(segments, unit, span);
        self.expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        )
    }

    /// A call to the serde step `name`: `rd_i64` is `__gos_serde_rd_i64`.
    fn step(&mut self, name: &str, args: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        self.call_named(&format!("__gos_serde_{name}"), args, ty, span)
    }

    fn write_step(&mut self, out: &mut Emit, name: &str, value: HirExpr, span: Span) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let w = self.local(WRITER, i64_ty, span);
        let call = self.step(name, vec![w, value], unit, span);
        out.stmt(self.expr_stmt(call, span));
    }

    fn leave_stmt(&mut self, span: Span) -> HirStmt {
        let leave = self.leave(span);
        self.expr_stmt(leave, span)
    }

    fn leave(&mut self, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let r = self.local(READER, i64_ty, span);
        self.step("rd_leave", vec![r], unit, span)
    }

    /// A constructor call of `Ok`, `Err`, or `Some`.
    fn ctor(&mut self, name: &str, args: Vec<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        let callee = self.path_segments(&[name.to_string()], ty, span);
        self.expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        )
    }

    fn none(&mut self, ty: Ty, span: Span) -> HirExpr {
        self.path_segments(&["None".to_string()], ty, span)
    }

    fn struct_lit(
        &mut self,
        name: &str,
        fields: Vec<(String, HirExpr)>,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let unit = self.tcx.unit();
        let mut args = vec![self.string_lit(name, span)];
        for (field, value) in fields {
            args.push(self.string_lit(&field, span));
            args.push(value);
        }
        let callee = self.path_segments(&["__struct".to_string()], unit, span);
        self.expr(
            ty,
            span,
            HirExprKind::Call {
                callee: Box::new(callee),
                args,
            },
        )
    }

    fn if_else(
        &mut self,
        condition: HirExpr,
        then: HirExpr,
        otherwise: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        self.expr(
            ty,
            span,
            HirExprKind::If {
                condition: Box::new(condition),
                then_branch: Box::new(then),
                else_branch: Some(Box::new(otherwise)),
            },
        )
    }

    fn binding(&mut self, name: &str, ty: Ty, span: Span) -> HirPat {
        HirPat {
            id: self.ids.next(),
            span,
            ty,
            kind: HirPatKind::Binding {
                name: Ident::new(name),
                mutable: false,
            },
        }
    }

    #[allow(clippy::too_many_arguments, reason = "one match, spelled out")]
    fn match_option(
        &mut self,
        scrutinee: HirExpr,
        bound: &str,
        payload: Ty,
        some_body: HirExpr,
        none_body: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let scrutinee_ty = scrutinee.ty;
        let binding = self.binding(bound, payload, span);
        let some_pat = HirPat {
            id: self.ids.next(),
            span,
            ty: scrutinee_ty,
            kind: HirPatKind::Variant {
                name: Ident::new("Some"),
                fields: vec![binding],
            },
        };
        let none_pat = HirPat {
            id: self.ids.next(),
            span,
            ty: scrutinee_ty,
            kind: HirPatKind::Variant {
                name: Ident::new("None"),
                fields: Vec::new(),
            },
        };
        self.expr(
            ty,
            span,
            HirExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms: vec![
                    HirMatchArm {
                        pattern: some_pat,
                        guard: None,
                        body: some_body,
                    },
                    HirMatchArm {
                        pattern: none_pat,
                        guard: None,
                        body: none_body,
                    },
                ],
            },
        )
    }

    /// `match result { Ok(ok) => on_ok, Err(err) => on_err }`.
    #[allow(clippy::too_many_arguments, reason = "one match, spelled out")]
    fn match_result(
        &mut self,
        scrutinee: HirExpr,
        ok: &str,
        ok_ty: Ty,
        on_ok: HirExpr,
        err: &str,
        err_ty: Ty,
        on_err: HirExpr,
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let scrutinee_ty = scrutinee.ty;
        let ok_binding = self.binding(ok, ok_ty, span);
        let err_binding = self.binding(err, err_ty, span);
        let ok_pat = HirPat {
            id: self.ids.next(),
            span,
            ty: scrutinee_ty,
            kind: HirPatKind::Variant {
                name: Ident::new("Ok"),
                fields: vec![ok_binding],
            },
        };
        let err_pat = HirPat {
            id: self.ids.next(),
            span,
            ty: scrutinee_ty,
            kind: HirPatKind::Variant {
                name: Ident::new("Err"),
                fields: vec![err_binding],
            },
        };
        self.expr(
            ty,
            span,
            HirExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms: vec![
                    HirMatchArm {
                        pattern: ok_pat,
                        guard: None,
                        body: on_ok,
                    },
                    HirMatchArm {
                        pattern: err_pat,
                        guard: None,
                        body: on_err,
                    },
                ],
            },
        )
    }

    fn block(&mut self, stmts: Vec<HirStmt>, tail: Option<HirExpr>, ty: Ty, span: Span) -> HirExpr {
        let block = HirBlock {
            id: self.ids.next(),
            span,
            stmts,
            tail: tail.map(Box::new),
            ty,
            is_comptime: false,
        };
        self.expr(ty, span, HirExprKind::Block(block))
    }

    fn let_stmt(
        &mut self,
        name: &str,
        mutable: bool,
        ty: Ty,
        init: HirExpr,
        span: Span,
    ) -> HirStmt {
        let pattern = HirPat {
            id: self.ids.next(),
            span,
            ty,
            kind: HirPatKind::Binding {
                name: Ident::new(name),
                mutable,
            },
        };
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Let {
                pattern,
                ty,
                init: Some(init),
            },
        }
    }

    fn expr_stmt(&mut self, expr: HirExpr, span: Span) -> HirStmt {
        HirStmt {
            id: self.ids.next(),
            span,
            kind: HirStmtKind::Expr {
                expr,
                has_semi: true,
            },
        }
    }

    /// `index = index + 1`.
    fn increment(&mut self, index: &str, span: Span) -> HirStmt {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let current = self.local(index, i64_ty, span);
        let one = self.int_lit(1, i64_ty, span);
        let next = self.expr(
            i64_ty,
            span,
            HirExprKind::Binary {
                op: HirBinaryOp::Add,
                lhs: Box::new(current),
                rhs: Box::new(one),
            },
        );
        let place = self.local(index, i64_ty, span);
        let assign = self.expr(
            unit,
            span,
            HirExprKind::Assign {
                place: Box::new(place),
                value: Box::new(next),
            },
        );
        self.expr_stmt(assign, span)
    }

    /// `while index < count { body }`.
    fn while_below(&mut self, index: &str, count: &str, body: Vec<HirStmt>, span: Span) -> HirStmt {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let bool_ty = self.tcx.bool_ty();
        let unit = self.tcx.unit();
        let lhs = self.local(index, i64_ty, span);
        let rhs = self.local(count, i64_ty, span);
        let condition = self.expr(
            bool_ty,
            span,
            HirExprKind::Binary {
                op: HirBinaryOp::Lt,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        );
        let body = self.block(body, None, unit, span);
        let looped = self.expr(
            unit,
            span,
            HirExprKind::While {
                condition: Box::new(condition),
                body: Box::new(body),
                label: None,
            },
        );
        self.expr_stmt(looped, span)
    }

    fn param(&mut self, name: &str, ty: Ty, span: Span) -> HirParam {
        HirParam {
            pattern: self.binding(name, ty, span),
            ty,
            is_comptime: false,
        }
    }

    fn body(&mut self, stmts: Vec<HirStmt>, tail: HirExpr, ty: Ty, span: Span) -> HirBody {
        HirBody {
            block: HirBlock {
                id: self.ids.next(),
                span,
                stmts,
                tail: Some(Box::new(tail)),
                ty,
                is_comptime: false,
            },
        }
    }

    fn function(
        &mut self,
        name: &str,
        params: Vec<HirParam>,
        ret: Ty,
        stmts: Vec<HirStmt>,
        tail: HirExpr,
        span: Span,
    ) -> HirItem {
        let body = self.body(stmts, tail, ret, span);
        HirItem {
            id: self.ids.next(),
            span,
            def: None,
            module_path: Vec::new(),
            kind: HirItemKind::Fn(HirFn {
                name: Ident::new(name),
                params,
                ret: Some(ret),
                body: Some(body),
                is_unsafe: false,
                is_comptime: false,
                has_self: false,
                origin: FnOrigin::Declared,
                foreign_link: None,
                foreign_symbol: None,
                export_symbol: None,
            }),
        }
    }

    /// The statements an [`Emit`] gathered, each run of raw text written by
    /// one `wr_raw` step.
    fn finish(&mut self, emit: Emit, span: Span) -> Vec<HirStmt> {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let mut stmts = Vec::with_capacity(emit.pieces.len());
        for piece in emit.pieces {
            match piece {
                Piece::Stmt(stmt) => stmts.push(stmt),
                Piece::Raw(text) => {
                    let w = self.local(WRITER, i64_ty, span);
                    let text = self.string_lit(&text, span);
                    let call = self.step("wr_raw", vec![w, text], unit, span);
                    stmts.push(self.expr_stmt(call, span));
                }
            }
        }
        stmts
    }
}

/// Statements under construction, in order, with adjacent raw text merged
/// into one `wr_raw` step.
#[derive(Default)]
struct Emit {
    pieces: Vec<Piece>,
}

enum Piece {
    Raw(String),
    Stmt(HirStmt),
}

impl Emit {
    fn raw(&mut self, text: &str) {
        if let Some(Piece::Raw(last)) = self.pieces.last_mut() {
            last.push_str(text);
        } else {
            self.pieces.push(Piece::Raw(text.to_string()));
        }
    }

    fn stmt(&mut self, stmt: HirStmt) {
        self.pieces.push(Piece::Stmt(stmt));
    }
}

/// What a missing struct key reads as.
enum Absent {
    /// The field's default, under `#[default]` or `#[default(value)]`.
    Default(FieldDefault),
    /// A failure with this message.
    Fail(String),
}

fn binding_name(pattern: &HirPat) -> Option<String> {
    match &pattern.kind {
        HirPatKind::Binding { name, .. } => Some(name.name.clone()),
        _ => None,
    }
}

/// The range a narrow integer type holds, by its spelling.
fn int_bounds(spelled: &str) -> (i64, i64) {
    match spelled {
        "i8" => (i8::MIN.into(), i8::MAX.into()),
        "i16" => (i16::MIN.into(), i16::MAX.into()),
        "i32" => (i32::MIN.into(), i32::MAX.into()),
        "u8" => (0, u8::MAX.into()),
        "u16" => (0, u16::MAX.into()),
        "u32" => (0, u32::MAX.into()),
        _ => (i64::MIN, i64::MAX),
    }
}

const fn int_name(int: IntTy) -> &'static str {
    match int {
        IntTy::I8 => "i8",
        IntTy::I16 => "i16",
        IntTy::I32 => "i32",
        IntTy::I64 => "i64",
        IntTy::I128 => "i128",
        IntTy::Isize => "isize",
        IntTy::U8 => "u8",
        IntTy::U16 => "u16",
        IntTy::U32 => "u32",
        IntTy::U64 => "u64",
        IntTy::U128 => "u128",
        IntTy::Usize => "usize",
    }
}

/// `text` as a JSON string, quotes included, escaping only what JSON
/// requires: a key the program spells is written as it is.
fn serde_json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The key table `rd_struct` reads: each key as its byte length, `:`, and
/// its bytes.
fn encode_names<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for name in names {
        out.push_str(&name.len().to_string());
        out.push(':');
        out.push_str(name);
    }
    out
}

/// The variant table `rd_enum` reads: `u` or `p` for a unit variant or one
/// with a payload, then the name as [`encode_names`] spells one.
fn encode_variants<'a>(variants: impl IntoIterator<Item = (bool, &'a str)>) -> String {
    let mut out = String::new();
    for (unit, name) in variants {
        out.push(if unit { 'u' } else { 'p' });
        out.push_str(&name.len().to_string());
        out.push(':');
        out.push_str(name);
    }
    out
}

/// Whether `expr` names storage without computing anything: a binding, or a
/// field of one, read any number of times to the same value.
pub(super) fn is_plain_place(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Path { def: None, .. } => true,
        HirExprKind::Field { receiver, .. } | HirExprKind::TupleIndex { receiver, .. } => {
            is_plain_place(receiver)
        }
        _ => false,
    }
}
