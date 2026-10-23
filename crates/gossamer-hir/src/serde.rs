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

/// How a named struct field reads and writes: under its own name or a
/// `#[rename]`d key, left out under `#[skip]`, and filled with its type's zero
/// value when missing under `#[default]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HirSerdeField {
    /// The key the field is written under, when `#[rename]` sets one.
    pub key: Option<String>,
    /// `#[skip]`: never written, and read as its type's zero value.
    pub skip: bool,
    /// `#[default]`: a missing key reads as its type's zero value.
    pub default: bool,
}

impl HirSerdeField {
    /// The options a field's attributes state.
    #[must_use]
    pub fn of(attrs: &gossamer_ast::Attrs) -> Self {
        let mut options = Self::default();
        for attr in &attrs.outer {
            let Some(name) = attr.path.segments.last().map(|s| s.name.name.as_str()) else {
                continue;
            };
            match name {
                "rename" => {
                    options.key = attr
                        .tokens
                        .as_deref()
                        .map(|text| text.trim().trim_matches('"').to_string())
                        .filter(|key| !key.is_empty());
                }
                "skip" => options.skip = true,
                "default" => options.default = true,
                _ => {}
            }
        }
        options
    }
}

/// The operation a stub performs, by its name's prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    ToJson,
    FromJson,
    ToToml,
    FromToml,
    ToYaml,
    FromYaml,
}

impl Op {
    fn of_stub(name: &str) -> Option<Self> {
        let tail = name.strip_prefix("__gos_serde_")?;
        [
            ("to_json_", Self::ToJson),
            ("from_json_", Self::FromJson),
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
    serde: Vec<HirSerdeField>,
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

enum Pending {
    Encode(DefId, Ty, String),
    Render(DefId, Ty, String),
    Decode(DefId, Ty, String),
    WireEncode(DefId, Ty, String),
    WireDecode(DefId, Ty, String),
    Root(DefId, Ty, Ty, String),
}

struct Gen<'a> {
    tcx: &'a mut TyCtxt,
    ids: &'a mut HirIdGenerator,
    adts: HashMap<DefId, AdtInfo>,
    encoders: HashMap<DefId, String>,
    /// The functions writing a type the way `json::encode` does.
    renderers: HashMap<DefId, String>,
    decoders: HashMap<DefId, String>,
    /// The functions writing and reading a type as the binding wire spells it.
    wire_encoders: HashMap<DefId, String>,
    wire_decoders: HashMap<DefId, String>,
    roots: HashMap<DefId, String>,
    pending: Vec<Pending>,
    items: Vec<HirItem>,
    temp: u32,
    /// Where the generated functions say they come from: the first stub.
    span: Span,
    /// Whether the walk being written renders as `json::encode` does: a
    /// struct's members in byte order of their names, every float with its
    /// fraction, and the shapes the typed surface does not take.
    rendering: bool,
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
        let (def, ty) = if op.encodes() {
            let ty = f.params.first()?.ty;
            (self.adt_of(ty)?, ty)
        } else {
            let ty = self.result_ok_ty(ret)?;
            (self.adt_of(ty)?, ty)
        };
        let param = binding_name(&f.params.first()?.pattern)?;
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let string_ty = self.tcx.string_ty();
        let mut stmts = Vec::new();
        let tail = if op.encodes() {
            let writer = self.step("wr_new", Vec::new(), i64_ty, span);
            stmts.push(self.let_stmt(WRITER, false, i64_ty, writer, span));
            let encoder = self.encoder(def, ty);
            let w = self.local(WRITER, i64_ty, span);
            let value = self.local(&param, ty, span);
            let unit = self.tcx.unit();
            let call = self.call_named(&encoder, vec![w, value], unit, span);
            stmts.push(self.expr_stmt(call, span));
            let w = self.local(WRITER, i64_ty, span);
            let text = self.step("wr_finish", vec![w], string_ty, span);
            match op {
                Op::ToToml => self.call_path(&["toml", "from_json"], vec![text], ret, span),
                Op::ToYaml => self.call_path(&["yaml", "from_json"], vec![text], ret, span),
                _ => self.ctor("Ok", vec![text], ret, span),
            }
        } else {
            let root = self.root(def, ty, ret);
            let text = self.local(&param, string_ty, span);
            match op {
                Op::FromToml | Op::FromYaml => {
                    let module = if op == Op::FromToml { "toml" } else { "yaml" };
                    let converted_ty = self.result_ty(string_ty, self.result_err_ty(ret)?);
                    let converted =
                        self.call_path(&[module, "to_json"], vec![text], converted_ty, span);
                    let json = self.fresh("json");
                    let json_value = self.local(&json, string_ty, span);
                    let decoded = self.call_named(&root, vec![json_value], ret, span);
                    let error = self.fresh("err");
                    let err_ty = self.result_err_ty(ret)?;
                    let error_value = self.local(&error, err_ty, span);
                    let failed = self.ctor("Err", vec![error_value], ret, span);
                    self.match_result(
                        converted, &json, string_ty, decoded, &error, err_ty, failed, ret, span,
                    )
                }
                _ => self.call_named(&root, vec![text], ret, span),
            }
        };
        Some(self.body(stmts, tail, ret, span))
    }

    /// The decode root for `ty`: opens a reader over the text, decodes, and
    /// answers the value or the failure.
    fn root(&mut self, def: DefId, ty: Ty, ret: Ty) -> String {
        if let Some(name) = self.roots.get(&def) {
            return name.clone();
        }
        let name = format!("__gos_serde_root_{}", def.local);
        self.roots.insert(def, name.clone());
        self.pending.push(Pending::Root(def, ty, ret, name.clone()));
        name
    }

    fn encoder(&mut self, def: DefId, ty: Ty) -> String {
        if self.wire {
            if let Some(name) = self.wire_encoders.get(&def) {
                return name.clone();
            }
            let name = format!("__gos_serde_wenc_{}", def.local);
            self.wire_encoders.insert(def, name.clone());
            self.pending
                .push(Pending::WireEncode(def, ty, name.clone()));
            return name;
        }
        if let Some(name) = self.encoders.get(&def) {
            return name.clone();
        }
        let name = format!("__gos_serde_enc_{}", def.local);
        self.encoders.insert(def, name.clone());
        self.pending.push(Pending::Encode(def, ty, name.clone()));
        name
    }

    fn renderer(&mut self, def: DefId, ty: Ty) -> String {
        if let Some(name) = self.renderers.get(&def) {
            return name.clone();
        }
        let name = format!("__gos_serde_ren_{}", def.local);
        self.renderers.insert(def, name.clone());
        self.pending.push(Pending::Render(def, ty, name.clone()));
        name
    }

    fn decoder(&mut self, def: DefId, ty: Ty) -> String {
        if self.wire {
            if let Some(name) = self.wire_decoders.get(&def) {
                return name.clone();
            }
            let name = format!("__gos_serde_wdec_{}", def.local);
            self.wire_decoders.insert(def, name.clone());
            self.pending
                .push(Pending::WireDecode(def, ty, name.clone()));
            return name;
        }
        if let Some(name) = self.decoders.get(&def) {
            return name.clone();
        }
        let name = format!("__gos_serde_dec_{}", def.local);
        self.decoders.insert(def, name.clone());
        self.pending.push(Pending::Decode(def, ty, name.clone()));
        name
    }

    /// Generates every function requested so far, and the ones those request.
    fn drain(&mut self) {
        while let Some(next) = self.pending.pop() {
            let item = match next {
                Pending::Encode(def, ty, name) => self.encode_fn(def, ty, &name),
                Pending::Render(def, ty, name) => {
                    let outer = std::mem::replace(&mut self.rendering, true);
                    let item = self.encode_fn(def, ty, &name);
                    self.rendering = outer;
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
                Pending::Root(def, ty, ret, name) => Some(self.root_fn(def, ty, ret, &name)),
            };
            if let Some(item) = item {
                self.items.push(item);
            }
        }
    }

    fn root_fn(&mut self, def: DefId, ty: Ty, ret: Ty, name: &str) -> HirItem {
        let span = self.adt_span();
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let string_ty = self.tcx.string_ty();
        let bool_ty = self.tcx.bool_ty();
        let unit = self.tcx.unit();
        let mut stmts = Vec::new();
        let text = self.local("__serde_text", string_ty, span);
        let reader = self.step("rd_new", vec![text], i64_ty, span);
        stmts.push(self.let_stmt(READER, false, i64_ty, reader, span));
        let decoder = self.decoder(def, ty);
        let r = self.local(READER, i64_ty, span);
        let value = self.call_named(&decoder, vec![r], ty, span);
        stmts.push(self.let_stmt("__serde_value", false, ty, value, span));
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
        let params = vec![self.param("__serde_text", string_ty, span)];
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
                let field_tys = self.tcx.struct_field_tys(def)?.to_vec();
                self.encode_struct(&mut out, &info, fields, &field_tys, ty, span);
            }
            HirAdtKind::Enum(variants) => {
                let variant_tys = self.tcx.enum_variant_tys(def)?.to_vec();
                let mut arms = Vec::new();
                for (variant, tys) in variants.iter().zip(&variant_tys) {
                    arms.push(self.encode_variant_arm(variant, tys, ty, span));
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

    /// A struct as an object of its written fields, in declaration order.
    fn encode_struct(
        &mut self,
        out: &mut Emit,
        info: &AdtInfo,
        fields: &[Ident],
        field_tys: &[Ty],
        ty: Ty,
        span: Span,
    ) {
        out.raw("{");
        let mut first = true;
        // A rendering keeps every field under its own name, in byte order of
        // the names, as a `json::Value` object holds its members.
        let mut order: Vec<usize> = (0..fields.len().min(field_tys.len())).collect();
        if self.rendering {
            order.sort_by(|&a, &b| fields[a].name.as_bytes().cmp(fields[b].name.as_bytes()));
        }
        for index in order {
            let (field, field_ty) = (&fields[index], &field_tys[index]);
            let options = if self.rendering {
                HirSerdeField::default()
            } else {
                info.serde.get(index).cloned().unwrap_or_default()
            };
            if options.skip {
                continue;
            }
            let key = options.key.unwrap_or_else(|| field.name.clone());
            if !first {
                out.raw(",");
            }
            first = false;
            out.raw(&format!("{}:", serde_json_string(&key)));
            let value = self.local("__serde_value", ty, span);
            let read = self.field(value, &field.name, *field_ty, span);
            self.encode(out, read, *field_ty, span);
        }
        out.raw("}");
    }

    /// The arm writing one variant: a unit variant as its name, any other as
    /// an object whose one key is its name.
    fn encode_variant_arm(
        &mut self,
        variant: &crate::tree::HirEnumVariant,
        tys: &[Ty],
        ty: Ty,
        span: Span,
    ) -> HirMatchArm {
        let unit = self.tcx.unit();
        let tag = serde_json_string(&variant.name.name);
        let mut arm = Emit::default();
        let binds: Vec<String> = (0..tys.len()).map(|i| format!("__serde_p{i}")).collect();
        let kind = if let Some(names) = &variant.struct_fields {
            arm.raw(&format!("{{{tag}:{{"));
            let mut order: Vec<usize> = (0..names.len().min(tys.len())).collect();
            if self.rendering {
                order.sort_by(|&a, &b| names[a].name.as_bytes().cmp(names[b].name.as_bytes()));
            }
            for (n, i) in order.into_iter().enumerate() {
                if n > 0 {
                    arm.raw(",");
                }
                arm.raw(&format!("{}:", serde_json_string(&names[i].name)));
                let value = self.local(&binds[i], tys[i], span);
                self.encode(&mut arm, value, tys[i], span);
            }
            arm.raw("}}");
            let fields = names
                .iter()
                .zip(tys)
                .zip(&binds)
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
            match tys {
                [] => arm.raw(&tag),
                [payload] => {
                    arm.raw(&format!("{{{tag}:"));
                    let value = self.local(&binds[0], *payload, span);
                    self.encode(&mut arm, value, *payload, span);
                    arm.raw("}");
                }
                _ => {
                    arm.raw(&format!("{{{tag}:["));
                    for (i, payload) in tys.iter().enumerate() {
                        if i > 0 {
                            arm.raw(",");
                        }
                        let value = self.local(&binds[i], *payload, span);
                        self.encode(&mut arm, value, *payload, span);
                    }
                    arm.raw("]}");
                }
            }
            let fields = tys
                .iter()
                .zip(&binds)
                .map(|(t, b)| self.binding(b, *t, span))
                .collect();
            HirPatKind::Variant {
                name: variant.name.clone(),
                fields,
            }
        };
        let pattern = HirPat {
            id: self.ids.next(),
            span,
            ty,
            kind,
        };
        let stmts = self.finish(arm, span);
        let body = self.block(stmts, None, unit, span);
        HirMatchArm {
            pattern,
            guard: None,
            body,
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
            Kind::F64 => {
                let step = if self.rendering {
                    "wr_f64_tree"
                } else {
                    "wr_f64"
                };
                self.write_step(out, step, value, span);
            }
            Kind::F32 => {
                let f64_ty = self.tcx.float_ty(FloatTy::F64);
                let wide = self.cast(value, f64_ty, span);
                let step = if self.rendering {
                    "wr_f32_tree"
                } else {
                    "wr_f32"
                };
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
            Kind::Seq(elem) => self.encode_seq(out, value, ty, elem, span),
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

    /// A tuple as an array of its elements.
    fn encode_tuple(&mut self, out: &mut Emit, value: HirExpr, ty: Ty, elems: &[Ty], span: Span) {
        let tuple = self.fresh("tuple");
        out.stmt(self.let_stmt(&tuple, false, ty, value, span));
        out.raw("[");
        for (i, elem) in elems.iter().enumerate() {
            if i > 0 {
                out.raw(",");
            }
            let tuple_value = self.local(&tuple, ty, span);
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
        let seq = self.fresh("seq");
        out.stmt(self.let_stmt(&seq, false, ty, value, span));
        let count = self.fresh("n");
        let receiver = self.local(&seq, ty, span);
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
        let receiver = self.local(&seq, ty, span);
        let at = self.local(&index, i64_ty, span);
        let item = self.index(receiver, at, elem, span);
        self.encode(&mut body, item, elem, span);
        body.stmt(self.increment(&index, span));
        let body = self.finish(body, span);
        out.stmt(self.while_below(&index, &count, body, span));
        out.raw("]");
    }

    /// A `String`-keyed map as an object, its keys sorted so the text is the
    /// same on every tier.
    fn encode_map(&mut self, out: &mut Emit, value: HirExpr, ty: Ty, value_ty: Ty, span: Span) {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let string_ty = self.tcx.string_ty();
        let names_ty = self.tcx.intern(TyKind::Vec(string_ty));
        let map = self.fresh("map");
        out.stmt(self.let_stmt(&map, false, ty, value, span));
        let names = self.fresh("keys");
        let receiver = self.local(&map, ty, span);
        let all = self.method(receiver, "keys", Vec::new(), names_ty, span);
        out.stmt(self.let_stmt(&names, true, names_ty, all, span));
        let receiver = self.local(&names, names_ty, span);
        let sorted = self.method(receiver, "sort", Vec::new(), unit, span);
        out.stmt(self.expr_stmt(sorted, span));
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
        let receiver = self.local(&map, ty, span);
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
                let field_tys = self.tcx.struct_field_tys(def)?.to_vec();
                self.decode_struct(&mut out, &info, fields, &field_tys, ty, span)
            }
            HirAdtKind::Enum(variants) => {
                let variant_tys = self.tcx.enum_variant_tys(def)?.to_vec();
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
            .map(|i| info.serde.get(i).cloned().unwrap_or_default())
            .collect();
        let keys: Vec<String> = fields
            .iter()
            .zip(&options)
            .filter(|(_, o)| !o.skip)
            .map(|(f, o)| o.key.clone().unwrap_or_else(|| f.name.clone()))
            .collect();
        let tuple_struct = fields.first().is_some_and(|f| f.name == "0");
        self.begin_struct(out, &keys, span);
        let mut values = Vec::new();
        let mut read_index = 0i64;
        for (index, (field, field_ty)) in fields.iter().zip(field_tys).enumerate() {
            let local = format!("__serde_f{index}");
            let field_options = &options[index];
            let value = if field_options.skip {
                self.default_value(*field_ty, info, span)
            } else {
                let key = field_options
                    .key
                    .clone()
                    .unwrap_or_else(|| field.name.clone());
                let (path, missing) = if tuple_struct {
                    (
                        format!("element `{key}`"),
                        format!("missing element `{key}`"),
                    )
                } else {
                    (format!("field `{key}`"), format!("missing field `{key}`"))
                };
                let absent = if field_options.default {
                    Absent::Default
                } else {
                    Absent::Fail(missing)
                };
                let value = self.read_field(read_index, *field_ty, &path, absent, info, span);
                read_index += 1;
                value
            };
            out.stmt(self.let_stmt(&local, false, *field_ty, value, span));
            values.push((field.name.clone(), self.local(&local, *field_ty, span)));
        }
        self.struct_lit(&info.path, values, ty, span)
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
        let table = encode_variants(variants.iter().zip(variant_tys).map(|(v, tys)| {
            (
                tys.is_empty() && v.struct_fields.is_none(),
                v.name.name.as_str(),
            )
        }));
        let fallback = self.fallback_variant(def);
        let r = self.local(READER, i64_ty, span);
        let table = self.string_lit(&table, span);
        let bare = self.string_lit(&info.adt.name.name, span);
        let fallback_lit = self.int_lit(i64::try_from(fallback).unwrap_or(0), i64_ty, span);
        let chosen = self.step("rd_enum", vec![r, table, bare, fallback_lit], i64_ty, span);
        out.stmt(self.let_stmt("__serde_variant", false, i64_ty, chosen, span));
        let mut arms = Vec::new();
        let last = variants.len().saturating_sub(1);
        for (index, (variant, tys)) in variants.iter().zip(variant_tys).enumerate() {
            let body = self.decode_variant(info, variant, tys, ty, span);
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

    fn decode_variant(
        &mut self,
        info: &AdtInfo,
        variant: &crate::tree::HirEnumVariant,
        tys: &[Ty],
        ty: Ty,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let vname = variant.name.name.clone();
        let mut segments = info.segments.clone();
        segments.push(vname.clone());
        let path = format!("variant `{vname}`");
        let mut out = Emit::default();
        let value = match (&variant.struct_fields, tys.len()) {
            (None, 0) => self.path_segments(&segments, ty, span),
            (None, 1) => {
                // Read before stepping out: the payload is the value stood on.
                let payload = self.decode(&mut out, tys[0], &path, span);
                out.stmt(self.let_stmt("__serde_p0", false, tys[0], payload, span));
                out.stmt(self.leave_stmt(span));
                let payload = self.local("__serde_p0", tys[0], span);
                self.call_segments(&segments, vec![payload], ty, span)
            }
            (None, _) => {
                let mut args = Vec::new();
                for (i, payload_ty) in tys.iter().enumerate() {
                    let r = self.local(READER, i64_ty, span);
                    let at = self.int_lit(i64::try_from(i).unwrap_or(0), i64_ty, span);
                    let step = self.step("rd_at", vec![r, at], unit, span);
                    out.stmt(self.expr_stmt(step, span));
                    let value = self.decode(&mut out, *payload_ty, &path, span);
                    let local = format!("__serde_p{i}");
                    out.stmt(self.let_stmt(&local, false, *payload_ty, value, span));
                    out.stmt(self.leave_stmt(span));
                    args.push(self.local(&local, *payload_ty, span));
                }
                out.stmt(self.leave_stmt(span));
                self.call_segments(&segments, args, ty, span)
            }
            (Some(names), _) => {
                let keys: Vec<String> = names.iter().map(|n| n.name.clone()).collect();
                self.begin_struct(&mut out, &keys, span);
                let mut values = Vec::new();
                for (i, (field, field_ty)) in names.iter().zip(tys).enumerate() {
                    let field_path = format!("variant `{vname}` field `{}`", field.name);
                    let missing = format!("missing field `{}` of variant `{vname}`", field.name);
                    let value = self.read_field(
                        i64::try_from(i).unwrap_or(0),
                        *field_ty,
                        &field_path,
                        Absent::Fail(missing),
                        info,
                        span,
                    );
                    let local = format!("__serde_p{i}");
                    out.stmt(self.let_stmt(&local, false, *field_ty, value, span));
                    values.push((field.name.clone(), self.local(&local, *field_ty, span)));
                }
                out.stmt(self.leave_stmt(span));
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
    /// and otherwise `None` for an `Option`, the zero value under
    /// `#[default]`, or a recorded failure.
    fn read_field(
        &mut self,
        index: i64,
        ty: Ty,
        path: &str,
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
        let value = self.decode(&mut found, ty, path, span);
        let local = self.fresh("v");
        found.stmt(self.let_stmt(&local, false, ty, value, span));
        found.stmt(self.leave_stmt(span));
        let found_stmts = self.finish(found, span);
        let found_value = self.local(&local, ty, span);
        let then = self.block(found_stmts, Some(found_value), ty, span);
        let otherwise =
            if matches!(self.kind(ty), Some(Kind::Opt(_))) && !matches!(absent, Absent::Default) {
                self.none(ty, span)
            } else {
                match absent {
                    Absent::Default => self.default_value(ty, info, span),
                    Absent::Fail(message) => {
                        let r = self.local(READER, i64_ty, span);
                        let message = self.string_lit(&message, span);
                        let fail = self.step("rd_fail", vec![r, message], unit, span);
                        let fail = self.expr_stmt(fail, span);
                        let zero = self.zero_value(ty, span);
                        self.block(vec![fail], Some(zero), ty, span)
                    }
                }
            };
        self.if_else(present, then, otherwise, ty, span)
    }

    /// Appends the steps that read a `ty` at the current value and answers
    /// the expression holding it. `path` names the value in a failure.
    fn decode(&mut self, out: &mut Emit, ty: Ty, path: &str, span: Span) -> HirExpr {
        let Some(kind) = self.kind(ty) else {
            return self.zero_value(ty, span);
        };
        match kind {
            Kind::Adt(def, adt_ty) => self.decode_adt(out, def, adt_ty, ty, path, span),
            Kind::Seq(elem) => self.decode_seq(out, ty, elem, path, span),
            Kind::Opt(inner) if self.wire => self.decode_wire_option(out, ty, inner, path, span),
            Kind::Res(ok_ty, err_ty) => self.decode_wire_result(out, ty, ok_ty, err_ty, path, span),
            Kind::Opt(inner) => {
                let bool_ty = self.tcx.bool_ty();
                let r = self.reader(span);
                let is_null = self.step("rd_is_null", vec![r], bool_ty, span);
                let none = self.none(ty, span);
                let mut some = Emit::default();
                let value = self.decode(&mut some, inner, path, span);
                let wrapped = self.ctor("Some", vec![value], ty, span);
                let some_stmts = self.finish(some, span);
                let some_block = self.block(some_stmts, Some(wrapped), ty, span);
                self.if_else(is_null, none, some_block, ty, span)
            }
            Kind::Tuple(elems) => self.decode_tuple(out, ty, &elems, path, span),
            Kind::Map(value_ty) => self.decode_map(out, ty, value_ty, path, span),
            scalar => self.decode_scalar(&scalar, ty, path, span),
        }
    }

    /// A scalar, read by the step for its kind and cast to its width.
    fn decode_scalar(&mut self, kind: &Kind, ty: Ty, path: &str, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let f64_ty = self.tcx.float_ty(FloatTy::F64);
        let (step, spelled, read_ty) = match kind {
            Kind::I64 => ("rd_i64", "i64", i64_ty),
            Kind::Narrow(spelled) => ("rd_i64", *spelled, i64_ty),
            Kind::Unsigned(spelled) => ("rd_u64", *spelled, self.tcx.int_ty(IntTy::U64)),
            Kind::F64 => ("rd_f64", "f64", f64_ty),
            Kind::F32 => ("rd_f64", "f32", f64_ty),
            Kind::Bool => ("rd_bool", "bool", self.tcx.bool_ty()),
            Kind::Char => ("rd_char", "char", self.tcx.char_ty()),
            Kind::Str => ("rd_str", "string", self.tcx.string_ty()),
            Kind::Json => {
                let r = self.reader(span);
                return self.step("rd_json", vec![r], ty, span);
            }
            Kind::Adt(..)
            | Kind::Seq(_)
            | Kind::Opt(_)
            | Kind::Tuple(_)
            | Kind::Map(_)
            | Kind::Res(..) => {
                return self.zero_value(ty, span);
            }
        };
        let r = self.reader(span);
        let message = self.string_lit(&format!("{path}: expected {spelled}"), span);
        let read = self.step(step, vec![r, message], read_ty, span);
        // A newtype field is its representation here.
        let target = self.peel(ty);
        self.cast(read, target, span)
    }

    /// A user type, decoded by its own function and wrapped by `path` when it
    /// fails.
    fn decode_adt(
        &mut self,
        out: &mut Emit,
        def: DefId,
        adt_ty: Ty,
        ty: Ty,
        path: &str,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let callee = self.decoder(def, adt_ty);
        let mark = self.fresh("mark");
        let r = self.reader(span);
        let taken = self.step("rd_mark", vec![r], i64_ty, span);
        out.stmt(self.let_stmt(&mark, false, i64_ty, taken, span));
        let value = self.fresh("v");
        let r = self.reader(span);
        let read = self.call_named(&callee, vec![r], ty, span);
        out.stmt(self.let_stmt(&value, false, ty, read, span));
        let r = self.reader(span);
        let mark_value = self.local(&mark, i64_ty, span);
        let context = self.string_lit(path, span);
        let wrap = self.step("rd_wrap", vec![r, mark_value, context], unit, span);
        out.stmt(self.expr_stmt(wrap, span));
        self.local(&value, ty, span)
    }

    /// A sequence, element by element.
    fn decode_seq(&mut self, out: &mut Emit, ty: Ty, elem: Ty, path: &str, span: Span) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let count = self.fresh("n");
        let r = self.reader(span);
        let message = self.string_lit(&format!("{path}: expected array"), span);
        let begun = self.step("rd_array", vec![r, message], i64_ty, span);
        out.stmt(self.let_stmt(&count, false, i64_ty, begun, span));
        let seq = self.fresh("seq");
        let capacity = self.local(&count, i64_ty, span);
        let fresh = self.call_path(&["Vec", "with_capacity"], vec![capacity], ty, span);
        out.stmt(self.let_stmt(&seq, true, ty, fresh, span));
        let index = self.fresh("i");
        let zero = self.int_lit(0, i64_ty, span);
        out.stmt(self.let_stmt(&index, true, i64_ty, zero, span));
        let mut body = Emit::default();
        let r = self.reader(span);
        let at = self.local(&index, i64_ty, span);
        let enter = self.step("rd_elem", vec![r, at], unit, span);
        body.stmt(self.expr_stmt(enter, span));
        let item = self.decode(&mut body, elem, &format!("{path}[index]"), span);
        let item_local = self.fresh("x");
        body.stmt(self.let_stmt(&item_local, false, elem, item, span));
        body.stmt(self.leave_stmt(span));
        let receiver = self.local(&seq, ty, span);
        let item_value = self.local(&item_local, elem, span);
        let push = self.method(receiver, "push", vec![item_value], unit, span);
        body.stmt(self.expr_stmt(push, span));
        body.stmt(self.increment(&index, span));
        let body = self.finish(body, span);
        out.stmt(self.while_below(&index, &count, body, span));
        self.local(&seq, ty, span)
    }

    /// A tuple, from an array of exactly its length.
    fn decode_tuple(
        &mut self,
        out: &mut Emit,
        ty: Ty,
        elems: &[Ty],
        path: &str,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let r = self.reader(span);
        let len = self.int_lit(i64::try_from(elems.len()).unwrap_or(0), i64_ty, span);
        let not_array = self.string_lit(&format!("{path}: expected array"), span);
        let wrong_len = self.string_lit(
            &format!("{path}: expected {}-element array", elems.len()),
            span,
        );
        let begin = self.step("rd_tuple", vec![r, len, not_array, wrong_len], unit, span);
        out.stmt(self.expr_stmt(begin, span));
        let mut items = Vec::new();
        for (i, elem) in elems.iter().enumerate() {
            let r = self.reader(span);
            let at = self.int_lit(i64::try_from(i).unwrap_or(0), i64_ty, span);
            let enter = self.step("rd_elem", vec![r, at], unit, span);
            out.stmt(self.expr_stmt(enter, span));
            let value = self.decode(out, *elem, &format!("{path}.{i}"), span);
            let local = self.fresh("e");
            out.stmt(self.let_stmt(&local, false, *elem, value, span));
            out.stmt(self.leave_stmt(span));
            items.push(self.local(&local, *elem, span));
        }
        self.expr(ty, span, HirExprKind::Tuple(items))
    }

    /// A `String`-keyed map, entry by entry in key order.
    fn decode_map(
        &mut self,
        out: &mut Emit,
        ty: Ty,
        value_ty: Ty,
        path: &str,
        span: Span,
    ) -> HirExpr {
        let i64_ty = self.tcx.int_ty(IntTy::I64);
        let unit = self.tcx.unit();
        let string_ty = self.tcx.string_ty();
        let count = self.fresh("n");
        let r = self.reader(span);
        let message = self.string_lit(&format!("{path}: expected object"), span);
        let begun = self.step("rd_map", vec![r, message], i64_ty, span);
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
        let entry = self.decode(&mut body, value_ty, &format!("{path}[key]"), span);
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
            Kind::Tuple(_) | Kind::Adt(..) => self.unit_lit(span),
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

    fn adt_of(&self, ty: Ty) -> Option<DefId> {
        match self.tcx.kind_of(self.peel(ty)) {
            TyKind::Adt { def, .. } if self.adts.contains_key(def) => Some(*def),
            _ => None,
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

    fn result_ty(&mut self, ok: Ty, err: Ty) -> Ty {
        let def = DefId::local(u32::MAX);
        self.tcx.register_def_name(def, "Result");
        self.tcx.intern(TyKind::Adt {
            def,
            substs: Substs::from_types([ok, err]),
        })
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
    /// The field type's zero value, under `#[default]`.
    Default,
    /// A failure with this message.
    Fail(String),
}

fn binding_name(pattern: &HirPat) -> Option<String> {
    match &pattern.kind {
        HirPatKind::Binding { name, .. } => Some(name.name.clone()),
        _ => None,
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
