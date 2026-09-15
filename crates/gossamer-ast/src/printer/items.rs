//! Printing of items, use declarations, and supporting syntactic nodes.

#![forbid(unsafe_code)]

use crate::common::{Ident, Visibility};
use crate::items::{
    Attribute, Attrs, ConstDecl, EnumDecl, EnumVariant, FnDecl, FnParam, GenericParam, Generics,
    ImplDecl, ImplItem, Item, ItemKind, ModBody, ModDecl, Receiver, StaticDecl, StructBody,
    StructDecl, StructField, TraitBound, TraitDecl, TraitItem, TupleField, TypeAliasDecl,
    WhereClause,
};
use crate::source_file::{ModulePath, UseDecl, UseListEntry, UseTarget};

use super::Printer;

impl Printer {
    /// Renders a `use` declaration.
    pub fn print_use_decl(&mut self, decl: &UseDecl) {
        self.write("use ");
        self.write_use_target(&decl.target);
        if let Some(entries) = &decl.list {
            self.write("::{");
            for (index, entry) in entries.iter().enumerate() {
                if index > 0 {
                    self.write(", ");
                }
                self.write_use_entry(entry);
            }
            self.write("}");
        }
        if let Some(alias) = &decl.alias {
            self.write(" as ");
            self.write_ident(alias);
        }
    }

    fn write_use_target(&mut self, target: &UseTarget) {
        match target {
            UseTarget::Module(path) => self.write_module_path(path),
            UseTarget::Project { id, module } => {
                self.write("\"");
                self.write_escaped_str(id);
                self.write("\"");
                if let Some(module) = module {
                    self.write("::");
                    self.write_module_path(module);
                }
            }
        }
    }

    fn write_module_path(&mut self, path: &ModulePath) {
        for (index, segment) in path.segments.iter().enumerate() {
            if index > 0 {
                self.write("::");
            }
            self.write_ident(segment);
        }
    }

    fn write_use_entry(&mut self, entry: &UseListEntry) {
        for seg in &entry.prefix {
            self.write_ident(seg);
            self.write("::");
        }
        self.write_ident(&entry.name);
        if let Some(alias) = &entry.alias {
            self.write(" as ");
            self.write_ident(alias);
        }
    }

    /// Renders an item and its attributes.
    pub fn print_item(&mut self, item: &Item) {
        self.print_attrs(&item.attrs, false);
        self.write_visibility(item.visibility);
        self.print_item_kind(&item.kind);
    }

    fn print_item_kind(&mut self, kind: &ItemKind) {
        match kind {
            ItemKind::Fn(decl) => self.print_fn_decl(decl),
            ItemKind::Struct(decl) => self.print_struct_decl(decl),
            ItemKind::Enum(decl) => self.print_enum_decl(decl),
            ItemKind::Trait(decl) => self.print_trait_decl(decl),
            ItemKind::Impl(decl) => self.print_impl_decl(decl),
            ItemKind::TypeAlias(decl) => self.print_type_alias_decl(decl),
            ItemKind::Const(decl) => self.print_const_decl(decl),
            ItemKind::Static(decl) => self.print_static_decl(decl),
            ItemKind::Mod(decl) => self.print_mod_decl(decl),
            ItemKind::AttrItem(attr) => self.print_attribute(attr, true),
        }
    }

    pub(super) fn print_attrs(&mut self, attrs: &Attrs, is_inner: bool) {
        let list = if is_inner { &attrs.inner } else { &attrs.outer };
        for attr in list {
            self.print_attribute(attr, is_inner);
            self.newline();
        }
    }

    fn print_attribute(&mut self, attr: &Attribute, is_inner: bool) {
        self.write(if is_inner { "#![" } else { "#[" });
        self.print_path_expr(&attr.path);
        if let Some(tokens) = &attr.tokens {
            self.write("(");
            self.write(tokens);
            self.write(")");
        }
        self.write("]");
    }

    fn write_visibility(&mut self, visibility: Visibility) {
        if visibility.is_public() {
            self.write("pub ");
        }
    }

    fn print_fn_decl(&mut self, decl: &FnDecl) {
        if decl.is_unsafe {
            self.write("unsafe ");
        }
        self.write("fn ");
        self.write_ident(&decl.name);
        self.print_generics(&decl.generics);
        self.write("(");
        for (index, param) in decl.params.iter().enumerate() {
            if index > 0 {
                self.write(", ");
            }
            self.print_fn_param(param);
        }
        self.write(")");
        if let Some(ret) = &decl.ret {
            self.write(" -> ");
            self.print_type(ret);
        }
        self.print_where_clause(&decl.where_clause);
        match &decl.body {
            Some(body) => {
                self.write(" ");
                self.print_expr(body);
            }
            None => self.write(";"),
        }
    }

    fn print_fn_param(&mut self, param: &FnParam) {
        match param {
            FnParam::Receiver(Receiver::Owned) => self.write("self"),
            FnParam::Receiver(Receiver::RefShared) => self.write("&self"),
            FnParam::Receiver(Receiver::RefMut) => self.write("&mut self"),
            FnParam::Typed { pattern, ty, .. } => {
                self.print_pattern(pattern);
                self.write(": ");
                self.print_type(ty);
            }
        }
    }

    /// Prints a generic parameter list, or nothing when there is none.
    pub fn print_generics(&mut self, generics: &Generics) {
        if generics.is_empty() {
            return;
        }
        self.write("<");
        for (index, param) in generics.params.iter().enumerate() {
            if index > 0 {
                self.write(", ");
            }
            self.print_generic_param(param);
        }
        self.write(">");
    }

    fn print_generic_param(&mut self, param: &GenericParam) {
        match param {
            GenericParam::Lifetime { name } => {
                self.write("'");
                self.write(name);
            }
            GenericParam::Type {
                name,
                bounds,
                default,
            } => {
                self.write_ident(name);
                if !bounds.is_empty() {
                    self.write(": ");
                    self.print_trait_bounds(bounds);
                }
                if let Some(ty) = default {
                    self.write(" = ");
                    self.print_type(ty);
                }
            }
            GenericParam::Const { name, ty, default } => {
                self.write("const ");
                self.write_ident(name);
                self.write(": ");
                self.print_type(ty);
                if let Some(expr) = default {
                    self.write(" = ");
                    self.print_expr(expr);
                }
            }
        }
    }

    fn print_trait_bounds(&mut self, bounds: &[TraitBound]) {
        for (index, bound) in bounds.iter().enumerate() {
            if index > 0 {
                self.write(" + ");
            }
            self.print_trait_bound(bound);
        }
    }

    /// Renders one bound, folding its associated-type constraints back into
    /// the trait's argument list (`Iterator<Item = i64>`).
    fn print_trait_bound(&mut self, bound: &TraitBound) {
        if bound.bindings.is_empty() {
            self.print_type_path(&bound.path);
            return;
        }
        for (index, segment) in bound.path.segments.iter().enumerate() {
            if index > 0 {
                self.write("::");
            }
            self.write_ident(&segment.name);
            if index + 1 < bound.path.segments.len() {
                continue;
            }
            self.write("<");
            for arg in &segment.generics {
                self.print_generic_arg(arg);
                self.write(", ");
            }
            for (index, binding) in bound.bindings.iter().enumerate() {
                if index > 0 {
                    self.write(", ");
                }
                self.write_ident(&binding.name);
                self.write(" = ");
                self.print_type(&binding.ty);
            }
            self.write(">");
        }
    }

    fn print_where_clause(&mut self, clause: &WhereClause) {
        if clause.is_empty() {
            return;
        }
        self.write(" where ");
        for (index, predicate) in clause.predicates.iter().enumerate() {
            if index > 0 {
                self.write(", ");
            }
            self.print_type(&predicate.bounded);
            self.write(": ");
            self.print_trait_bounds(&predicate.bounds);
        }
    }

    fn print_struct_decl(&mut self, decl: &StructDecl) {
        self.write("struct ");
        self.write_ident(&decl.name);
        self.print_generics(&decl.generics);
        match &decl.body {
            StructBody::Named(fields) => {
                self.print_where_clause(&decl.where_clause);
                self.write(" {");
                self.newline();
                self.indent_in();
                let name_width = fields
                    .iter()
                    .filter(|f| f.attrs.is_empty())
                    .map(|f| f.name.name.chars().count())
                    .max()
                    .unwrap_or(0);
                for field in fields {
                    self.print_struct_field(field, name_width);
                    self.write(",");
                    self.newline();
                }
                self.indent_out();
                self.write("}");
            }
            StructBody::Tuple(fields) => {
                self.write("(");
                for (index, field) in fields.iter().enumerate() {
                    if index > 0 {
                        self.write(", ");
                    }
                    self.print_tuple_field(field);
                }
                self.write(")");
                self.print_where_clause(&decl.where_clause);
                self.write(";");
            }
            StructBody::Unit => {
                self.print_where_clause(&decl.where_clause);
            }
        }
    }

    fn print_struct_field(&mut self, field: &StructField, align_to: usize) {
        self.print_attrs(&field.attrs, false);
        self.write_visibility(field.visibility);
        self.write_ident(&field.name);
        let padding = align_to.saturating_sub(field.name.name.chars().count());
        if padding > 0 && field.attrs.is_empty() {
            self.write(&" ".repeat(padding));
        }
        self.write(": ");
        self.print_type(&field.ty);
    }

    fn print_tuple_field(&mut self, field: &TupleField) {
        self.print_attrs(&field.attrs, false);
        self.write_visibility(field.visibility);
        self.print_type(&field.ty);
    }

    fn print_enum_decl(&mut self, decl: &EnumDecl) {
        if decl.repr.packed {
            self.write("packed ");
        }
        self.write("enum ");
        self.write_ident(&decl.name);
        self.print_generics(&decl.generics);
        if let Some(bits) = decl.repr.declared_bits {
            self.write(&format!(": u{bits}"));
        }
        self.print_where_clause(&decl.where_clause);
        self.write(" {");
        self.newline();
        self.indent_in();
        for variant in &decl.variants {
            self.print_enum_variant(variant);
            self.write(",");
            self.newline();
        }
        self.indent_out();
        self.write("}");
    }

    fn print_enum_variant(&mut self, variant: &EnumVariant) {
        self.print_attrs(&variant.attrs, false);
        self.write_ident(&variant.name);
        match &variant.body {
            StructBody::Unit => {}
            StructBody::Tuple(fields) => {
                self.write("(");
                for (index, field) in fields.iter().enumerate() {
                    if index > 0 {
                        self.write(", ");
                    }
                    self.print_tuple_field(field);
                }
                self.write(")");
            }
            StructBody::Named(fields) => {
                self.write(" {");
                self.newline();
                self.indent_in();
                let name_width = fields
                    .iter()
                    .filter(|f| f.attrs.is_empty())
                    .map(|f| f.name.name.chars().count())
                    .max()
                    .unwrap_or(0);
                for field in fields {
                    self.print_struct_field(field, name_width);
                    self.write(",");
                    self.newline();
                }
                self.indent_out();
                self.write("}");
            }
        }
        if let Some(disc) = &variant.discriminant {
            self.write(" = ");
            self.print_expr(disc);
        }
    }

    fn print_trait_decl(&mut self, decl: &TraitDecl) {
        self.write("trait ");
        self.write_ident(&decl.name);
        self.print_generics(&decl.generics);
        if !decl.supertraits.is_empty() {
            self.write(": ");
            self.print_trait_bounds(&decl.supertraits);
        }
        self.print_where_clause(&decl.where_clause);
        self.write(" {");
        self.newline();
        self.indent_in();
        for item in &decl.items {
            self.print_trait_item(item);
            self.newline();
        }
        self.indent_out();
        self.write("}");
    }

    fn print_trait_item(&mut self, item: &TraitItem) {
        match item {
            TraitItem::Fn(decl) => self.print_fn_decl(decl),
            TraitItem::Type {
                attrs,
                name,
                bounds,
                default,
            } => {
                self.print_attrs(attrs, false);
                self.write("type ");
                self.write_ident(name);
                if !bounds.is_empty() {
                    self.write(": ");
                    self.print_trait_bounds(bounds);
                }
                if let Some(ty) = default {
                    self.write(" = ");
                    self.print_type(ty);
                }
                self.write(";");
            }
            TraitItem::Const {
                attrs,
                name,
                ty,
                default,
            } => {
                self.print_attrs(attrs, false);
                self.write("const ");
                self.write_ident(name);
                self.write(": ");
                self.print_type(ty);
                if let Some(expr) = default {
                    self.write(" = ");
                    self.print_expr(expr);
                }
                self.write(";");
            }
        }
    }

    fn print_impl_decl(&mut self, decl: &ImplDecl) {
        self.write("impl");
        if !decl.generics.is_empty() {
            self.print_generics(&decl.generics);
        }
        self.write(" ");
        if let Some(trait_ref) = &decl.trait_ref {
            self.print_type_path(&trait_ref.path);
            self.write(" for ");
        }
        self.print_type(&decl.self_ty);
        self.print_where_clause(&decl.where_clause);
        self.write(" {");
        self.newline();
        self.indent_in();
        for item in &decl.items {
            self.print_impl_item(item);
            self.newline();
        }
        self.indent_out();
        self.write("}");
    }

    fn print_impl_item(&mut self, item: &ImplItem) {
        match item {
            ImplItem::Fn(decl) => self.print_fn_decl(decl),
            ImplItem::Type { attrs, name, ty } => {
                self.print_attrs(attrs, false);
                self.write("type ");
                self.write_ident(name);
                self.write(" = ");
                self.print_type(ty);
                self.write(";");
            }
            ImplItem::Const {
                attrs,
                name,
                ty,
                value,
            } => {
                self.print_attrs(attrs, false);
                self.write("const ");
                self.write_ident(name);
                self.write(": ");
                self.print_type(ty);
                self.write(" = ");
                self.print_expr(value);
                self.write(";");
            }
        }
    }

    fn print_type_alias_decl(&mut self, decl: &TypeAliasDecl) {
        self.write("type ");
        self.write_ident(&decl.name);
        self.print_generics(&decl.generics);
        self.write(" = ");
        // Dropping `new` would rewrite an opaque alias into a transparent
        // one, changing the program's types rather than its layout.
        if decl.nominal {
            self.write("new ");
        }
        self.print_type(&decl.ty);
        self.write(";");
    }

    fn print_const_decl(&mut self, decl: &ConstDecl) {
        self.write("const ");
        self.write_ident(&decl.name);
        self.write(": ");
        self.print_type(&decl.ty);
        self.write(" = ");
        self.print_expr(&decl.value);
        self.write(";");
    }

    fn print_static_decl(&mut self, decl: &StaticDecl) {
        self.write("static ");
        if decl.mutability.is_mutable() {
            self.write("mut ");
        }
        self.write_ident(&decl.name);
        self.write(": ");
        self.print_type(&decl.ty);
        self.write(" = ");
        self.print_expr(&decl.value);
        self.write(";");
    }

    fn print_mod_decl(&mut self, decl: &ModDecl) {
        self.write("mod ");
        self.write_ident(&decl.name);
        match &decl.body {
            ModBody::External => self.write(";"),
            ModBody::Inline(items) => {
                self.write(" {");
                self.newline();
                self.indent_in();
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        self.newline();
                    }
                    self.print_item(item);
                    self.newline();
                }
                self.indent_out();
                self.write("}");
            }
        }
    }

    pub(super) fn write_ident(&mut self, ident: &Ident) {
        self.write(&ident.name);
    }

    pub(super) fn write_escaped_str(&mut self, value: &str) {
        let escaped = escape_str(value);
        self.write(&escaped);
    }
}

pub(super) fn escape_str(value: &str) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            other if (other as u32) < 0x20 => {
                let code = other as u32;
                let _ = write!(out, "\\u{{{code:x}}}");
            }
            other => out.push(other),
        }
    }
    out
}
