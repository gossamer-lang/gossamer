//! Top-level item parsing: functions, structs, enums, traits, impls,
//! type aliases, constants, statics, and modules.

#![forbid(unsafe_code)]

use gossamer_ast::{
    Attribute, Attrs, ConstDecl, EnumDecl, EnumVariant, Expr, ExprKind, FnDecl, FnParam, Generics,
    Ident, ImplDecl, ImplItem, Item, ItemKind, ModBody, ModDecl, Mutability, Receiver, StaticDecl,
    StructBody, StructDecl, StructField, TraitBound, TraitDecl, TraitItem, TupleField,
    TypeAliasDecl, TypePath, TypePathSegment, Visibility, WhereClause,
};
use gossamer_lex::{Keyword, Punct, TokenKind};

use crate::diagnostic::ParseError;
use crate::parser::Parser;

/// Type spelling for an initialiser whose type is known from its syntax
/// alone, used to name a concrete type in a missing-annotation diagnostic.
fn literal_type_name(value: &Expr) -> Option<&'static str> {
    use gossamer_ast::Literal;
    let inner = match &value.kind {
        ExprKind::Unary { operand, .. } => &operand.kind,
        other => other,
    };
    let ExprKind::Literal(literal) = inner else {
        return None;
    };
    Some(match literal {
        Literal::Int(_) => "i64",
        Literal::Float(_) => "f64",
        Literal::String(_) | Literal::RawString { .. } => "String",
        Literal::Char(_) => "char",
        Literal::Byte(_) => "u8",
        Literal::Bool(_) => "bool",
        Literal::ByteString(_) | Literal::RawByteString { .. } | Literal::Unit => return None,
    })
}

fn empty_enum_decl(
    name: Ident,
    generics: Generics,
    where_clause: WhereClause,
    repr: gossamer_ast::EnumRepr,
) -> EnumDecl {
    EnumDecl {
        name,
        generics,
        where_clause,
        variants: Vec::new(),
        repr,
    }
}

impl Parser<'_> {
    /// Parses a visibility annotation: nothing, `pub`, or `pub(package)`.
    ///
    /// Rust's other restriction forms (`pub(crate)`, `pub(super)`,
    /// `pub(in path)`) are reported against `pub(package)`, the one
    /// restricted spelling the language has.
    fn parse_visibility(&mut self) -> Visibility {
        if !self.eat_keyword(Keyword::Pub) {
            return Visibility::Inherited;
        }
        if !self.at_punct(Punct::LParen) {
            return Visibility::Public;
        }
        let start = self.peek_span();
        self.eat_punct(Punct::LParen);
        if self.at_keyword(Keyword::Package) {
            self.bump();
            if self.eat_punct(Punct::RParen) {
                return Visibility::Package;
            }
        }
        let written = self.visibility_restriction_text();
        let span = self.join(start, self.last_span());
        self.record(
            ParseError::UnsupportedVisibilityRestriction { written },
            span,
        );
        Visibility::Package
    }

    /// Consumes the tokens of a `pub(..)` restriction up to its closing
    /// parenthesis and returns them as written.
    fn visibility_restriction_text(&mut self) -> String {
        let mut written = String::new();
        let mut depth = 1usize;
        while !self.at_eof() {
            if self.at_punct(Punct::LParen) {
                depth += 1;
            } else if self.at_punct(Punct::RParen) {
                depth -= 1;
                self.bump();
                if depth == 0 {
                    break;
                }
                written.push(')');
                continue;
            }
            if !written.is_empty() {
                written.push(' ');
            }
            written.push_str(&self.token_source_text());
            self.bump();
        }
        written
    }

    /// The next token exactly as the source spells it.
    fn token_source_text(&self) -> String {
        match &self.peek().kind {
            TokenKind::Keyword(keyword) => keyword.as_str().to_string(),
            _ => self.peek_text(),
        }
    }

    /// Parses a single top-level item.
    pub(crate) fn parse_item(&mut self) -> Item {
        let start_span = self.peek_span();
        let attrs = self.parse_attrs();
        let visibility = self.parse_visibility();
        let synthesized = attrs.has_word("gos_synthesized");
        if synthesized {
            self.synthesized_depth = self.synthesized_depth.saturating_add(1);
        }
        let kind = self.parse_item_kind(visibility);
        if synthesized {
            self.synthesized_depth = self.synthesized_depth.saturating_sub(1);
        }
        let end_span = self.last_span();
        let span = self.join(start_span, end_span);
        let id = self.alloc_id();
        Item::new(id, span, attrs, visibility, kind)
    }

    fn parse_item_kind(&mut self, visibility: Visibility) -> ItemKind {
        if self.at_keyword(Keyword::Fn) || self.at_keyword(Keyword::Unsafe) || self.at_comptime_fn()
        {
            return ItemKind::Fn(self.parse_fn_decl(visibility));
        }
        if self.at_keyword(Keyword::Struct) {
            return ItemKind::Struct(self.parse_struct_decl());
        }
        if self.at_keyword(Keyword::Enum) {
            return ItemKind::Enum(self.parse_enum_decl(false));
        }
        // `packed enum Name { .. }` stores its discriminant in the fewest
        // bits the variants need. `packed` is contextual, never reserved.
        if self.at_packed_enum() {
            self.bump();
            return ItemKind::Enum(self.parse_enum_decl(true));
        }
        if self.at_keyword(Keyword::Trait) {
            return ItemKind::Trait(self.parse_trait_decl());
        }
        if self.at_keyword(Keyword::Impl) {
            return ItemKind::Impl(self.parse_impl_decl());
        }
        if self.at_keyword(Keyword::Type) {
            return ItemKind::TypeAlias(self.parse_type_alias_decl(false));
        }
        // `newtype Name = Repr` declares a distinct type over the
        // representation. `newtype` is a contextual word, never reserved:
        // it opens the form only when a name and an `=` follow it.
        if self.at_contextual_newtype() {
            return ItemKind::TypeAlias(self.parse_type_alias_decl(true));
        }
        if self.at_keyword(Keyword::Const) {
            return ItemKind::Const(self.parse_const_decl());
        }
        if self.at_keyword(Keyword::Static) {
            return ItemKind::Static(self.parse_static_decl());
        }
        if self.at_keyword(Keyword::Mod) {
            return ItemKind::Mod(self.parse_mod_decl());
        }
        // `extern "C" { ... }` and `unsafe extern "C" { ... }` - GP0016.
        // The keyword is recognised as an item start by the recovery
        // helper, so we must handle it here to avoid the infinite loop
        // where recovery returns without advancing past `extern`.
        if self.at_keyword(Keyword::Extern)
            || (self.at_keyword(Keyword::Unsafe)
                && matches!(self.peek_nth(1).kind, TokenKind::Keyword(Keyword::Extern)))
        {
            let span = self.peek_span();
            self.record(ParseError::ExternReserved, span);
            if self.eat_keyword(Keyword::Unsafe) {
                // consumed `unsafe` of `unsafe extern "C" ...`
            }
            self.bump(); // consume `extern`
            // optional ABI string: `"C"`, `"system"`, etc.
            if matches!(self.peek().kind, TokenKind::StringLit) {
                self.bump();
            }
            // skip braced body `{ ... }` when present
            if self.at_punct(Punct::LBrace) {
                self.bump(); // consume opening `{`
                let mut depth = 1u32;
                while !self.at_eof() && depth > 0 {
                    if self.at_punct(Punct::LBrace) {
                        depth += 1;
                    } else if self.at_punct(Punct::RBrace) {
                        depth -= 1;
                    }
                    self.bump();
                }
            }
            return ItemKind::Mod(ModDecl {
                name: Ident::new("<extern-error>"),
                body: ModBody::External,
            });
        }
        // A file's imports precede its items, so a `use` here is a
        // placement error rather than an unrecognised construct: naming
        // `use` among the accepted continuations would contradict the
        // rejection.
        if self.at_keyword(Keyword::Use) {
            let span = self.peek_span();
            self.record(
                ParseError::unexpected_help(
                    "an item",
                    self.peek_text(),
                    "a file's `use` declarations come before its first item; move this import \
                     above them",
                ),
                span,
            );
        } else {
            self.record(
                ParseError::unexpected(
                    "one of `fn`, `struct`, `enum`, `trait`, `impl`, `const`, `static`, `type`, \
                     `use`, or `mod`",
                    self.peek_text(),
                ),
                self.peek_span(),
            );
        }
        // Force progress past the bad token before re-syncing - otherwise
        // a token that is *itself* an item-start keyword (e.g. a stray
        // `use` after the first item) traps `recover_to_item_start` in a
        // no-op and the caller's progress check loops forever.
        if !self.at_eof() {
            self.bump();
        }
        self.recover_to_item_start();
        ItemKind::Mod(ModDecl {
            name: Ident::new("<error>"),
            body: ModBody::External,
        })
    }

    /// Parses the outer attribute list preceding an item.
    pub(crate) fn parse_attrs(&mut self) -> Attrs {
        let mut outer = Vec::new();
        while self.at_attribute_start() {
            if let Some(attribute) = self.parse_attribute() {
                outer.push(attribute);
            } else {
                break;
            }
        }
        Attrs {
            outer,
            inner: Vec::new(),
        }
    }

    /// Parses the leading run of file-level `#![..]` attributes. These
    /// belong to the file rather than to the item that happens to follow
    /// them, so they are consumed before any item is parsed.
    pub(crate) fn parse_file_attrs(&mut self) -> Attrs {
        let mut inner = Vec::new();
        while self.at_inner_attribute_start() {
            match self.parse_attribute() {
                Some(attribute) => inner.push(attribute),
                None => break,
            }
        }
        Attrs {
            outer: Vec::new(),
            inner,
        }
    }

    /// Whether the cursor is at `#![`, the file- and item-level inner form.
    pub(crate) fn at_inner_attribute_start(&self) -> bool {
        self.at_punct(Punct::Hash)
            && matches!(self.peek_nth(1).kind, TokenKind::Punct(Punct::Bang))
            && matches!(self.peek_nth(2).kind, TokenKind::Punct(Punct::LBracket))
    }

    /// Whether the cursor is at `#[` or `#![`, the only shapes that open an
    /// attribute. `#` also begins the `#[..]` Vec and `#{..}` Set literals,
    /// so the following token decides which construct this is.
    pub(crate) fn at_attribute_start(&self) -> bool {
        if !self.at_punct(Punct::Hash) {
            return false;
        }
        match self.peek_nth(1).kind {
            TokenKind::Punct(Punct::LBracket) => true,
            TokenKind::Punct(Punct::Bang) => {
                matches!(self.peek_nth(2).kind, TokenKind::Punct(Punct::LBracket))
            }
            _ => false,
        }
    }

    fn parse_attribute(&mut self) -> Option<Attribute> {
        if !self.eat_punct(Punct::Hash) {
            return None;
        }
        let _inner = self.eat_punct(Punct::Bang);
        if !self.eat_punct(Punct::LBracket) {
            self.record(ParseError::MalformedAttribute, self.peek_span());
            return None;
        }
        let path = self.parse_path_expr();
        let tokens = if self.at_punct(Punct::LParen) {
            self.bump();
            let body = self.collect_delimited_tokens_public(Punct::LParen, Punct::RParen);
            self.expect_punct(Punct::RParen, "to close attribute arguments");
            Some(body)
        } else if self.at_punct(Punct::Eq) {
            self.bump();
            let rest = self.collect_until_rbracket();
            Some(format!("= {rest}"))
        } else {
            None
        };
        self.expect_punct(Punct::RBracket, "to close attribute");
        Some(Attribute { path, tokens })
    }

    fn collect_delimited_tokens_public(&mut self, open: Punct, close: Punct) -> String {
        self.collect_delimited_tokens_in_attr(open, close)
    }

    fn collect_delimited_tokens_in_attr(&mut self, open: Punct, close: Punct) -> String {
        let mut depth = 1u32;
        let mut output = String::new();
        while !self.at_eof() {
            let token = self.peek();
            match token.kind {
                TokenKind::Punct(found) if found == open => {
                    depth += 1;
                    output.push_str(self.slice(token.span));
                    output.push(' ');
                    self.bump();
                }
                TokenKind::Punct(found) if found == close => {
                    depth -= 1;
                    if depth == 0 {
                        return output.trim_end().to_string();
                    }
                    output.push_str(self.slice(token.span));
                    output.push(' ');
                    self.bump();
                }
                _ => {
                    output.push_str(self.slice(token.span));
                    output.push(' ');
                    self.bump();
                }
            }
        }
        output.trim_end().to_string()
    }

    fn collect_until_rbracket(&mut self) -> String {
        let mut output = String::new();
        while !self.at_eof() && !self.at_punct(Punct::RBracket) {
            let token = self.peek();
            output.push_str(self.slice(token.span));
            output.push(' ');
            self.bump();
        }
        output.trim_end().to_string()
    }

    fn parse_fn_decl(&mut self, visibility: Visibility) -> FnDecl {
        let start = self.peek_span();
        let is_comptime = self.eat_contextual_word("comptime");
        let unsafe_span = self.peek_span();
        let is_unsafe = self.eat_keyword(Keyword::Unsafe);
        if is_unsafe {
            self.record(ParseError::UnsafeGrantsNothing, unsafe_span);
        }
        self.expect_keyword(Keyword::Fn, "to start function declaration");
        let name_span = self.peek_span();
        let name = self.parse_ident_required("function name");
        // A wrapper the autoderive stage splices in carries no source
        // position an author could edit, so its spelling is not reported.
        let synthesized_fn = name.name.starts_with("__gos_");
        if synthesized_fn {
            self.synthesized_depth = self.synthesized_depth.saturating_add(1);
        }
        // Both rendering contracts declare one method answering a `String`,
        // so both are written `fn fmt`; the `impl` header is what decides
        // which of the two channels a value reaches.
        if name.name == "to_string" && self.impl_trait_name.as_deref() == Some("Display") {
            self.record(ParseError::DisplayContractMethod, name_span);
        }
        let generics = self.parse_generics();
        self.expect_punct(Punct::LParen, "to open the parameter list");
        let params = self.parse_fn_params();
        if !self.expect_punct(Punct::RParen, "to close the parameter list") {
            self.recover_to_close(Punct::LParen, Punct::RParen);
        }
        let ret = if self.eat_punct(Punct::Arrow) {
            Some(self.parse_type())
        } else {
            None
        };
        let where_clause = self.parse_where_clause();
        let body = if self.at_punct(Punct::LBrace) {
            let open = self.peek_span();
            self.bump();
            let block = self.parse_block_body();
            let span = self.join(open, self.last_span());
            let id = self.alloc_id();
            Some(Box::new(Expr::new(id, span, ExprKind::Block(block))))
        } else {
            self.eat_punct(Punct::Semi);
            None
        };
        if synthesized_fn {
            self.synthesized_depth = self.synthesized_depth.saturating_sub(1);
        }
        FnDecl {
            attrs: Attrs::default(),
            span: start.join(self.last_span()),
            is_unsafe,
            is_comptime,
            visibility,
            name,
            generics,
            params,
            ret,
            where_clause,
            body,
        }
    }

    fn parse_fn_params(&mut self) -> Vec<FnParam> {
        let mut params = Vec::new();
        if self.at_receiver_start() {
            if let Some(receiver) = self.parse_receiver() {
                params.push(FnParam::Receiver(receiver));
                if !self.eat_list_separator() {
                    return params;
                }
            }
        }
        while !self.at_punct(Punct::RParen) && !self.at_eof() {
            let before_param = self.tokens.checkpoint();
            // `comptime x: T` marks the parameter; a parameter simply
            // named `comptime` is the same word followed by its `:`.
            let is_comptime =
                self.at_contextual_word("comptime") && !self.peek_nth_is_punct(1, Punct::Colon);
            if is_comptime {
                self.bump();
            }
            let pattern = self.parse_pattern_no_or();
            if !self.expect_punct(Punct::Colon, "after the parameter pattern") {
                break;
            }
            let amp = self.peek_span();
            let ty = self.parse_type();
            let ty = self.strip_shared_parameter_reference(ty, amp);
            let default = if self.eat_punct(Punct::Eq) {
                Some(Box::new(self.parse_expr_no_assign()))
            } else {
                None
            };
            params.push(FnParam::Typed {
                pattern,
                ty,
                is_comptime,
                default,
            });
            if self.tokens.checkpoint() == before_param {
                self.bump();
                continue;
            }
            if !self.eat_list_separator() {
                break;
            }
        }
        params
    }

    fn at_receiver_start(&self) -> bool {
        if self.at_keyword(Keyword::SelfLower) {
            return true;
        }
        if self.at_punct(Punct::Amp) {
            let after = self.peek_nth(1);
            if matches!(after.kind, TokenKind::Keyword(Keyword::SelfLower)) {
                return true;
            }
            if matches!(after.kind, TokenKind::Keyword(Keyword::Mut))
                && matches!(
                    self.peek_nth(2).kind,
                    TokenKind::Keyword(Keyword::SelfLower)
                )
            {
                return true;
            }
        }
        false
    }

    fn parse_receiver(&mut self) -> Option<Receiver> {
        if self.eat_keyword(Keyword::SelfLower) {
            return Some(Receiver::Owned);
        }
        if self.eat_punct(Punct::Amp) {
            let mutability = self.eat_keyword(Keyword::Mut);
            // `at_receiver_start` gates this path on a `self` following the
            // `&` or `&mut`, so the keyword is present.
            self.eat_keyword(Keyword::SelfLower);
            return Some(if mutability {
                Receiver::RefMut
            } else {
                Receiver::RefShared
            });
        }
        None
    }

    fn parse_struct_decl(&mut self) -> StructDecl {
        self.bump();
        let name = self.parse_ident_required("struct name");
        let generics = self.parse_generics();
        if self.at_keyword(Keyword::Where) {
            let where_clause = self.parse_where_clause();
            let body = self.parse_struct_body_terminated();
            return StructDecl {
                name,
                generics,
                where_clause,
                body,
            };
        }
        let body = self.parse_struct_body();
        let where_clause = self.parse_where_clause();
        StructDecl {
            name,
            generics,
            where_clause,
            body,
        }
    }

    fn parse_struct_body_terminated(&mut self) -> StructBody {
        self.parse_struct_body()
    }

    fn parse_struct_body(&mut self) -> StructBody {
        if self.eat_punct(Punct::LBrace) {
            let mut fields = Vec::new();
            // An item keyword ends the field list: the `}` is missing, and
            // reading the next item as a field would report every one of
            // its tokens instead of the one absent brace.
            while !self.at_punct(Punct::RBrace) && !self.at_eof() && !at_item_keyword(self) {
                let before_field = self.tokens.checkpoint();
                let attrs = self.parse_attrs();
                let visibility = self.parse_visibility();
                let name = self.parse_ident_required("field name");
                self.expect_punct(Punct::Colon, "after field name");
                let ty = self.parse_type();
                fields.push(StructField {
                    attrs,
                    visibility,
                    name,
                    ty,
                });
                if self.tokens.checkpoint() == before_field {
                    self.bump();
                    continue;
                }
                if !self.eat_list_separator() {
                    break;
                }
            }
            self.expect_punct(Punct::RBrace, "to close struct body");
            return StructBody::Named(fields);
        }
        if self.eat_punct(Punct::LParen) {
            let mut fields = Vec::new();
            while !self.at_punct(Punct::RParen) && !self.at_eof() {
                let before_field = self.tokens.checkpoint();
                let attrs = self.parse_attrs();
                let visibility = self.parse_visibility();
                let ty = self.parse_type();
                fields.push(TupleField {
                    attrs,
                    visibility,
                    ty,
                });
                if self.tokens.checkpoint() == before_field {
                    self.bump();
                    continue;
                }
                if !self.eat_list_separator() {
                    break;
                }
            }
            self.expect_punct(Punct::RParen, "to close tuple struct body");
            return StructBody::Tuple(fields);
        }
        if self.eat_punct(Punct::Semi) {
            return StructBody::Unit;
        }
        StructBody::Unit
    }

    fn parse_enum_decl(&mut self, packed: bool) -> EnumDecl {
        self.bump();
        let name = self.parse_ident_required("enum name");
        let generics = self.parse_generics();
        // `enum Name : uN { .. }` names the width the discriminant is stored
        // in. It sits before the where clause, where a reader looks for the
        // type's own shape rather than its bounds.
        let declared_bits = if self.at_punct(Punct::Colon) {
            self.bump();
            self.parse_enum_repr_width()
        } else {
            None
        };
        let repr = gossamer_ast::EnumRepr {
            packed,
            declared_bits,
        };
        let where_clause = self.parse_where_clause();
        if !self.expect_punct(Punct::LBrace, "to open enum body") {
            return empty_enum_decl(name, generics, where_clause, repr);
        }
        let mut variants = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            let before_variant = self.tokens.checkpoint();
            let attrs = self.parse_attrs();
            let variant_name = self.parse_ident_required("variant name");
            let body = if self.eat_punct(Punct::LBrace) {
                let mut fields = Vec::new();
                while !self.at_punct(Punct::RBrace) && !self.at_eof() {
                    let before_field = self.tokens.checkpoint();
                    let field_attrs = self.parse_attrs();
                    let visibility = self.parse_visibility();
                    let field_name = self.parse_ident_required("field name");
                    self.expect_punct(Punct::Colon, "after field name");
                    let ty = self.parse_type();
                    fields.push(StructField {
                        attrs: field_attrs,
                        visibility,
                        name: field_name,
                        ty,
                    });
                    if self.tokens.checkpoint() == before_field {
                        self.bump();
                        continue;
                    }
                    if !self.eat_list_separator() {
                        break;
                    }
                }
                self.expect_punct(Punct::RBrace, "to close variant body");
                StructBody::Named(fields)
            } else if self.eat_punct(Punct::LParen) {
                let mut fields = Vec::new();
                while !self.at_punct(Punct::RParen) && !self.at_eof() {
                    let before_field = self.tokens.checkpoint();
                    let field_attrs = self.parse_attrs();
                    let visibility = self.parse_visibility();
                    let ty = self.parse_type();
                    fields.push(TupleField {
                        attrs: field_attrs,
                        visibility,
                        ty,
                    });
                    if self.tokens.checkpoint() == before_field {
                        self.bump();
                        continue;
                    }
                    if !self.eat_list_separator() {
                        break;
                    }
                }
                self.expect_punct(Punct::RParen, "to close variant body");
                StructBody::Tuple(fields)
            } else {
                StructBody::Unit
            };
            let discriminant = if self.eat_punct(Punct::Eq) {
                Some(self.parse_expr_no_assign())
            } else {
                None
            };
            variants.push(EnumVariant {
                attrs,
                name: variant_name,
                body,
                discriminant,
            });
            if self.tokens.checkpoint() == before_variant {
                self.bump();
                continue;
            }
            if !self.eat_list_separator() {
                break;
            }
        }
        self.expect_punct(Punct::RBrace, "to close enum body");
        EnumDecl {
            name,
            generics,
            where_clause,
            variants,
            repr,
        }
    }

    /// Parses the `uN` of an `enum Name : uN` representation, in bits.
    ///
    /// Only an unsigned width names a discriminant store: a discriminant is
    /// a count, never a negative number.
    fn parse_enum_repr_width(&mut self) -> Option<u32> {
        let span = self.peek_span();
        let text = self.slice(span).to_string();
        let bits = text
            .strip_prefix('u')
            .and_then(|digits| digits.parse::<u32>().ok())
            .filter(|bits| (1..=64).contains(bits));
        if bits.is_none() {
            self.record(
                ParseError::EnumReprWidth {
                    written: text.clone(),
                },
                span,
            );
        }
        if matches!(self.peek().kind, TokenKind::Ident) {
            self.bump();
        }
        bits
    }

    /// Whether the cursor sits on the `packed` of `packed enum`.
    fn at_packed_enum(&self) -> bool {
        let token = self.peek();
        matches!(token.kind, TokenKind::Ident)
            && self.slice(token.span) == "packed"
            && matches!(self.peek_nth(1).kind, TokenKind::Keyword(Keyword::Enum))
    }

    fn parse_trait_decl(&mut self) -> TraitDecl {
        self.bump();
        let name = self.parse_ident_required("trait name");
        let generics = self.parse_generics();
        let supertraits = if self.eat_punct(Punct::Colon) {
            self.parse_trait_bound_list()
        } else {
            Vec::new()
        };
        let where_clause = self.parse_where_clause();
        if !self.expect_punct(Punct::LBrace, "to open trait body") {
            return TraitDecl {
                name,
                generics,
                supertraits,
                where_clause,
                items: Vec::new(),
            };
        }
        let mut items = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            let attrs = self.parse_attrs();
            if self.eat_keyword(Keyword::Type) {
                let name = self.parse_ident_required("associated type name");
                let bounds = if self.eat_punct(Punct::Colon) {
                    self.parse_trait_bound_list()
                } else {
                    Vec::new()
                };
                let default = if self.eat_punct(Punct::Eq) {
                    Some(self.parse_type())
                } else {
                    None
                };
                self.eat_punct(Punct::Semi);
                items.push(TraitItem::Type {
                    attrs,
                    name,
                    bounds,
                    default,
                });
                continue;
            }
            if self.eat_keyword(Keyword::Const) {
                let name = self.parse_ident_required("associated constant name");
                self.expect_punct(Punct::Colon, "after associated constant name");
                let ty = self.parse_type();
                let default = if self.eat_punct(Punct::Eq) {
                    Some(self.parse_expr())
                } else {
                    None
                };
                self.eat_punct(Punct::Semi);
                items.push(TraitItem::Const {
                    attrs,
                    name,
                    ty,
                    default,
                });
                continue;
            }
            let mut decl = self.parse_fn_decl(Visibility::Public);
            decl.attrs = attrs;
            items.push(TraitItem::Fn(decl));
        }
        self.expect_punct(Punct::RBrace, "to close trait body");
        TraitDecl {
            name,
            generics,
            supertraits,
            where_clause,
            items,
        }
    }

    fn parse_impl_decl(&mut self) -> ImplDecl {
        self.bump();
        let generics = self.parse_generics();
        let first_type = self.parse_type();
        let (trait_ref, self_ty) = if self.eat_keyword(Keyword::For) {
            let self_ty = self.parse_type();
            let bound = match first_type.kind {
                gossamer_ast::TypeKind::Path(path) => TraitBound::new(path),
                _ => TraitBound::new(TypePath {
                    segments: vec![TypePathSegment::new("<error>")],
                }),
            };
            (Some(bound), self_ty)
        } else {
            (None, first_type)
        };
        let where_clause = self.parse_where_clause();
        if !self.expect_punct(Punct::LBrace, "to open impl body") {
            return ImplDecl {
                generics,
                trait_ref,
                self_ty,
                where_clause,
                items: Vec::new(),
            };
        }
        let mut items = Vec::new();
        let outer_trait = self.impl_trait_name.replace(
            trait_ref
                .as_ref()
                .and_then(|bound| bound.path.segments.last())
                .map(|segment| segment.name.name.clone())
                .unwrap_or_default(),
        );
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            items.push(self.parse_impl_item());
        }
        self.impl_trait_name = outer_trait;
        self.expect_punct(Punct::RBrace, "to close impl body");
        ImplDecl {
            generics,
            trait_ref,
            self_ty,
            where_clause,
            items,
        }
    }

    fn parse_impl_item(&mut self) -> ImplItem {
        let attrs = self.parse_attrs();
        let visibility = self.parse_visibility();
        if self.eat_keyword(Keyword::Type) {
            let name = self.parse_ident_required("associated type name");
            self.expect_punct(Punct::Eq, "after the associated type name");
            let ty = self.parse_type();
            self.eat_punct(Punct::Semi);
            return ImplItem::Type { attrs, name, ty };
        }
        if self.eat_keyword(Keyword::Const) {
            let name = self.parse_ident_required("associated constant name");
            self.expect_punct(Punct::Colon, "after associated constant name");
            let ty = self.parse_type();
            self.expect_punct(Punct::Eq, "after the associated constant's type");
            let value = self.parse_expr();
            self.eat_punct(Punct::Semi);
            return ImplItem::Const {
                attrs,
                name,
                ty,
                value,
            };
        }
        let mut decl = self.parse_fn_decl(visibility);
        decl.attrs = attrs;
        ImplItem::Fn(decl)
    }

    /// Drops the `&` of a shared reference in parameter position, which
    /// names no choice a caller has: an argument is passed without copying
    /// and a callee cannot write to the caller's variable through it. A
    /// sequence view is written `[T]` and carries the shared reference
    /// internally, which is the one shape every pass below the parser
    /// reads a view through. `amp` is the span of the `&`, so the fix
    /// deletes exactly that.
    pub(crate) fn strip_shared_parameter_reference(
        &mut self,
        ty: gossamer_ast::Type,
        amp: gossamer_lex::Span,
    ) -> gossamer_ast::Type {
        use gossamer_ast::TypeKind;
        // `[T]` is the unsized view a parameter names directly. It carries
        // the shared reference internally, which is the one shape every
        // pass below the parser reads a sequence view through.
        if matches!(ty.kind, TypeKind::Slice(_)) {
            let id = ty.id;
            let span = ty.span;
            return gossamer_ast::Type::new(
                id,
                span,
                TypeKind::Ref {
                    mutability: Mutability::Immutable,
                    inner: Box::new(ty),
                },
            );
        }
        let TypeKind::Ref {
            mutability: Mutability::Immutable,
            inner,
        } = ty.kind
        else {
            return ty;
        };
        if !self.in_synthesized_item() {
            self.record(ParseError::SharedReferenceParameter, amp);
        }
        if matches!(inner.kind, TypeKind::Slice(_)) {
            return gossamer_ast::Type::new(
                ty.id,
                ty.span,
                TypeKind::Ref {
                    mutability: Mutability::Immutable,
                    inner,
                },
            );
        }
        *inner
    }

    /// Whether the cursor sits on the `comptime` of a `comptime fn`
    /// declaration. The word is contextual, so it opens an item only
    /// when a function declaration follows.
    fn at_comptime_fn(&self) -> bool {
        self.at_contextual_word("comptime")
            && matches!(
                self.peek_nth(1).kind,
                TokenKind::Keyword(Keyword::Fn | Keyword::Unsafe)
            )
    }

    /// Whether the cursor sits on the `newtype` of `newtype Name = Repr`.
    fn at_contextual_newtype(&self) -> bool {
        let token = self.peek();
        matches!(token.kind, TokenKind::Ident)
            && self.slice(token.span) == "newtype"
            && matches!(self.peek_nth(1).kind, TokenKind::Ident)
    }

    fn parse_type_alias_decl(&mut self, newtype: bool) -> TypeAliasDecl {
        self.bump();
        let name = self.parse_ident_required("type alias name");
        let generics = self.parse_generics();
        self.expect_punct(Punct::Eq, "after the alias name");
        // `type X = new T` was the old opaque spelling. `new` reads as
        // allocation to a reader arriving from any other language, and the
        // concept has a standard name, so `newtype X = T` replaces it.
        let mut nominal = newtype;
        if self.at_contextual_new_before_type() {
            let span = self.peek_span();
            self.bump();
            self.record(ParseError::OpaqueAliasSpelling, span);
            nominal = true;
        }
        let ty = self.parse_type();
        self.eat_punct(Punct::Semi);
        TypeAliasDecl {
            name,
            generics,
            ty,
            nominal,
        }
    }

    /// Whether the cursor sits on the `new` of `type X = new T`.
    ///
    /// Requires a following token that can begin a type, so a transparent
    /// alias of a type actually named `new` keeps its meaning.
    fn at_contextual_new_before_type(&self) -> bool {
        let token = self.peek();
        if !matches!(token.kind, TokenKind::Ident) || self.slice(token.span) != "new" {
            return false;
        }
        matches!(
            self.peek_nth(1).kind,
            TokenKind::Ident
                | TokenKind::Keyword(Keyword::Fn | Keyword::SelfUpper)
                | TokenKind::Punct(Punct::Amp | Punct::Bang | Punct::LBracket | Punct::LParen)
        )
    }

    fn parse_const_decl(&mut self) -> ConstDecl {
        self.bump();
        let name_span = self.peek_span();
        let name = self.parse_ident_required("constant name");
        if let Some((ty, value)) = self.recover_missing_item_type("constant", &name, name_span) {
            return ConstDecl { name, ty, value };
        }
        self.expect_punct(Punct::Colon, "after constant name");
        let ty = self.parse_type();
        self.expect_punct(Punct::Eq, "before the constant's value");
        let value = self.parse_expr();
        self.eat_punct(Punct::Semi);
        ConstDecl { name, ty, value }
    }

    /// Recovers from `const NAME = value` / `static NAME = value`, where the
    /// mandatory type annotation is absent.
    ///
    /// Returns `None` when the annotation is present, leaving the cursor
    /// untouched for the normal path. Otherwise consumes through the
    /// initialiser and reports one diagnostic naming the type to write,
    /// taken from the initialiser when it is a literal. The recovered type is
    /// `Infer` so later passes see no name to resolve.
    fn recover_missing_item_type(
        &mut self,
        kind: &'static str,
        name: &Ident,
        name_span: gossamer_lex::Span,
    ) -> Option<(gossamer_ast::Type, Expr)> {
        if !self.at_punct(Punct::Eq) {
            return None;
        }
        self.bump();
        let value = self.parse_expr();
        self.eat_punct(Punct::Semi);
        // A name the parser had to invent was already reported; naming it
        // back to the user, or offering it as an edit, would put a spelling
        // into their source that never appeared in it.
        if !name.is_error() {
            self.record(
                ParseError::MissingItemType {
                    kind,
                    name: name.name.clone(),
                    inferred: literal_type_name(&value),
                },
                name_span,
            );
        }
        let id = self.alloc_id();
        let ty = gossamer_ast::Type::new(id, name_span, gossamer_ast::TypeKind::Infer);
        Some((ty, value))
    }

    fn parse_static_decl(&mut self) -> StaticDecl {
        self.bump();
        let mutability = if self.eat_keyword(Keyword::Mut) {
            Mutability::Mutable
        } else {
            Mutability::Immutable
        };
        let name_span = self.peek_span();
        let name = self.parse_ident_required("static name");
        if let Some((ty, value)) = self.recover_missing_item_type("static", &name, name_span) {
            return StaticDecl {
                mutability,
                name,
                ty,
                value,
            };
        }
        self.expect_punct(Punct::Colon, "after static name");
        let ty = self.parse_type();
        self.expect_punct(Punct::Eq, "before the static's initial value");
        let value = self.parse_expr();
        self.eat_punct(Punct::Semi);
        StaticDecl {
            mutability,
            name,
            ty,
            value,
        }
    }

    fn parse_mod_decl(&mut self) -> ModDecl {
        self.bump();
        let name = self.parse_ident_required("module name");
        // A statement ends at its newline here, so the module a file declares
        // is `mod name`. The trailing `;` is the one place the old grammar
        // asked for a terminator, and it reports with the rewrite that drops
        // it rather than parsing as a statement separator would.
        if self.at_punct(Punct::Semi) {
            let span = self.peek_span();
            self.bump();
            self.record(ParseError::ModDeclSemicolon, span);
            return ModDecl {
                name,
                body: ModBody::External,
            };
        }
        if !self.at_punct(Punct::LBrace) {
            return ModDecl {
                name,
                body: ModBody::External,
            };
        }
        self.bump();
        self.mod_stack.push(name.name.clone());
        let mut items = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            let before = self.checkpoint_public();
            if self.at_keyword(Keyword::Use) {
                // Hoist the `use` decl into the side channel so
                // [`parse_source_file`] adds it to the source file's
                // top-level imports. Without this, `use
                // std::encoding::json` inside `mod chat { ... }`
                // (the auto-bundled sibling shape) silently
                // disappears and `json::Value` references inside
                // the module fail to resolve.
                let use_decl = self.parse_use_decl();
                self.hoisted_uses.push(use_decl);
                continue;
            }
            if !crate::recovery::is_item_start(self) {
                // A module body holds items only; a bare statement here is
                // the misplaced-top-level-code case (a bundled sibling or
                // library module is not the entry file's implicit `fn main`).
                self.record(ParseError::StatementOutsideEntry, self.peek_span());
                self.bump();
                self.recover_to_item_start();
                continue;
            }
            items.push(self.parse_item());
            if self.checkpoint_public() == before {
                self.bump();
            }
        }
        self.expect_punct(Punct::RBrace, "to close inline module");
        self.mod_stack.pop();
        ModDecl {
            name,
            body: ModBody::Inline(items),
        }
    }

    fn parse_ident_required(&mut self, context: &str) -> Ident {
        let span = self.peek_span();
        if matches!(self.peek().kind, TokenKind::Ident) {
            self.bump();
            return Ident::new(self.slice(span));
        }
        self.record(ParseError::unexpected(context, self.peek_text()), span);
        Ident::new("<error>")
    }
}

/// `true` on a keyword that can only introduce an item. A struct field
/// may be prefixed with `pub`, so visibility alone does not qualify.
fn at_item_keyword(parser: &Parser<'_>) -> bool {
    matches!(
        parser.peek().kind,
        TokenKind::Keyword(
            Keyword::Fn
                | Keyword::Struct
                | Keyword::Enum
                | Keyword::Trait
                | Keyword::Impl
                | Keyword::Type
                | Keyword::Const
                | Keyword::Static
                | Keyword::Mod
                | Keyword::Use
        )
    )
}
