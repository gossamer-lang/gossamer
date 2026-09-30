//! Parsing for Gossamer type expressions (SPEC §3).

#![forbid(unsafe_code)]

use gossamer_ast::{
    AssocBinding, FnTypeKind, GenericArg, Ident, Mutability, Type, TypeKind, TypePath,
    TypePathSegment,
};
use gossamer_lex::{Keyword, Punct, TokenKind};

use crate::diagnostic::ParseError;
use crate::parser::Parser;

impl Parser<'_> {
    /// Parses a single `Type` production.
    pub(crate) fn parse_type(&mut self) -> Type {
        let start_span = self.peek_span();
        if self.enter_recursion(start_span).is_err() {
            let id = self.alloc_id();
            if !self.at_eof() {
                self.bump();
            }
            return Type::new(id, start_span, TypeKind::Infer);
        }
        let kind = self.parse_type_kind();
        let end_span = self.last_span();
        let span = self.join(start_span, end_span);
        let id = self.alloc_id();
        self.leave_recursion();
        Type::new(id, span, kind)
    }

    fn parse_type_kind(&mut self) -> TypeKind {
        // `dyn Trait` and `impl Trait` report the declined feature and parse
        // the trait path in their place.
        let declined = if self.at_contextual_word("dyn")
            && matches!(self.peek_nth(1).kind, TokenKind::Ident)
        {
            Some(crate::declined::TRAIT_OBJECTS)
        } else if self.at_keyword(Keyword::Impl) {
            Some(crate::declined::IMPL_TRAIT_TYPES)
        } else {
            None
        };
        if let Some(declined) = declined {
            let span = self.peek_span();
            self.report_declined(declined, span);
            self.bump();
            return self.parse_type_kind();
        }
        if self.eat_punct(Punct::LParen) {
            return self.parse_tuple_or_unit_type();
        }
        if self.eat_punct(Punct::LBracket) {
            return self.parse_array_or_slice_type();
        }
        if self.at_punct(Punct::Amp) {
            return self.parse_ref_type();
        }
        if self.at_punct(Punct::Bang) {
            self.bump();
            return TypeKind::Never;
        }
        // One callable type. The lowercase spelling described a raw pointer
        // shape the language does not have, and `FnMut` / `FnOnce` name a
        // distinction it does not draw, so each reports with the `Fn` rewrite
        // and parses as `Fn` so the rest of the program still checks.
        if self.at_keyword(Keyword::Fn) {
            let span = self.peek_span();
            self.record(
                ParseError::CallableTypeSpelling {
                    written: "fn".to_string(),
                },
                span,
            );
            return self.parse_fn_type(FnTypeKind::ClosureFn);
        }
        if matches!(self.peek().kind, TokenKind::Ident) {
            let text = self.slice(self.peek_span());
            if text == "Fn" {
                self.bump();
                return self.parse_fn_type_after_keyword(FnTypeKind::ClosureFn);
            }
            if text == "FnMut" || text == "FnOnce" {
                let span = self.peek_span();
                self.record(
                    ParseError::CallableTypeSpelling {
                        written: text.to_string(),
                    },
                    span,
                );
                self.bump();
                return self.parse_fn_type_after_keyword(FnTypeKind::ClosureFn);
            }
            if text == "_" {
                self.bump();
                return TypeKind::Infer;
            }
        }
        TypeKind::Path(self.parse_type_path())
    }

    fn parse_tuple_or_unit_type(&mut self) -> TypeKind {
        if self.eat_punct(Punct::RParen) {
            return TypeKind::Unit;
        }
        let first = self.parse_type();
        if self.eat_punct(Punct::RParen) {
            return first.kind;
        }
        let mut elements = vec![first];
        while self.eat_list_separator() {
            if self.at_punct(Punct::RParen) {
                break;
            }
            elements.push(self.parse_type());
        }
        self.expect_punct(Punct::RParen, "to close tuple type");
        TypeKind::Tuple(elements)
    }

    fn parse_array_or_slice_type(&mut self) -> TypeKind {
        let element = self.parse_type();
        if self.eat_punct(Punct::Semi) {
            let length = self.parse_expr();
            self.expect_punct(Punct::RBracket, "to close array type");
            return TypeKind::Array {
                elem: Box::new(element),
                len: Box::new(length),
            };
        }
        self.expect_punct(Punct::RBracket, "to close slice type");
        TypeKind::Slice(Box::new(element))
    }

    fn parse_ref_type(&mut self) -> TypeKind {
        self.bump();
        if matches!(self.peek().kind, TokenKind::Label) {
            let span = self.peek_span();
            self.report_declined(crate::declined::LIFETIMES, span);
            self.bump();
        }
        let mutability = if self.eat_keyword(Keyword::Mut) {
            Mutability::Mutable
        } else {
            Mutability::Immutable
        };
        let inner = self.parse_type();
        TypeKind::Ref {
            mutability,
            inner: Box::new(inner),
        }
    }

    fn parse_fn_type(&mut self, kind: FnTypeKind) -> TypeKind {
        self.bump();
        self.parse_fn_type_after_keyword(kind)
    }

    fn parse_fn_type_after_keyword(&mut self, kind: FnTypeKind) -> TypeKind {
        let keyword = kind.as_str();
        if !self.eat_punct(Punct::LParen) {
            self.record(
                ParseError::unexpected_help(
                    format!("`(` after `{keyword}` in a function type"),
                    self.peek_text(),
                    format!(
                        "write the parameter types in parentheses, as in `{keyword}(i64) -> i64`"
                    ),
                ),
                self.peek_span(),
            );
            return self.parse_fn_type_unparenthesised(kind);
        }
        let mut params = Vec::new();
        while !self.at_punct(Punct::RParen) && !self.at_eof() {
            // A callable type's parameters follow the same rule a written
            // parameter list does: `T` or `&mut T`, with `[T]` the view.
            let amp = self.peek_span();
            let ty = self.parse_type();
            params.push(self.strip_shared_parameter_reference(ty, amp));
            if !self.eat_list_separator() {
                break;
            }
        }
        if !self.expect_punct(Punct::RParen, "to close the function type's parameter list") {
            self.recover_to_close(Punct::LParen, Punct::RParen);
        }
        let ret = if self.eat_punct(Punct::Arrow) {
            Some(Box::new(self.parse_type()))
        } else {
            None
        };
        TypeKind::Fn { kind, params, ret }
    }

    /// Reads the parameter and return types of a function type written
    /// without its parentheses, so the surrounding list still meets its
    /// own delimiters and one missing `(` costs one diagnostic.
    fn parse_fn_type_unparenthesised(&mut self, kind: FnTypeKind) -> TypeKind {
        let mut params = Vec::new();
        while !self.at_eof() && !self.at_close_angle() && !closes_type_position(self) {
            params.push(self.parse_type());
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        let ret = if self.eat_punct(Punct::Arrow) {
            Some(Box::new(self.parse_type()))
        } else {
            None
        };
        TypeKind::Fn { kind, params, ret }
    }

    /// Parses a `TypePath` - a path in type position (no turbofish `::<>`).
    pub(crate) fn parse_type_path(&mut self) -> TypePath {
        self.parse_type_path_inner(None)
    }

    /// Parses a `TypePath` in trait-bound position, where the argument list
    /// may carry `Name = Type` associated-type constraints alongside the
    /// ordinary type and const arguments.
    pub(crate) fn parse_type_path_with_bindings(&mut self) -> (TypePath, Vec<AssocBinding>) {
        let mut bindings = Vec::new();
        let path = self.parse_type_path_inner(Some(&mut bindings));
        (path, bindings)
    }

    fn parse_type_path_inner(&mut self, mut bindings: Option<&mut Vec<AssocBinding>>) -> TypePath {
        let first = self.parse_type_path_segment(bindings.as_deref_mut());
        let mut segments = vec![first];
        while self.eat_punct(Punct::ColonColon) {
            segments.push(self.parse_type_path_segment(bindings.as_deref_mut()));
        }
        TypePath { segments }
    }

    fn parse_type_path_segment(
        &mut self,
        bindings: Option<&mut Vec<AssocBinding>>,
    ) -> TypePathSegment {
        let name = self.parse_path_ident_text();
        // Primitive type names never carry generic arguments. Refusing
        // to consume `<` after a primitive disambiguates the ambiguity
        // exposed by `expr as i64 < width` - without this guard the
        // parser greedily opens a generic-argument list, fails when it
        // sees `width {`, and rejects perfectly valid comparison code
        // (and the formatter, which strips redundant parens, can
        // produce the same shape from `(expr as i64) < width`).
        // See `examples/list_dir.gos::pad_right` for the regression
        // that motivated this guard.
        let generics = if self.at_punct(Punct::Lt) && !is_non_generic_primitive(&name) {
            self.parse_type_generic_args(bindings)
        } else {
            Vec::new()
        };
        TypePathSegment::with_generics(name, generics)
    }

    fn parse_path_ident_text(&mut self) -> String {
        let token = self.peek();
        match token.kind {
            TokenKind::Ident => {
                self.bump();
                self.slice(token.span).to_string()
            }
            TokenKind::Keyword(Keyword::SelfUpper) => {
                self.bump();
                "Self".to_string()
            }
            TokenKind::Keyword(Keyword::SelfLower) => {
                self.bump();
                "self".to_string()
            }
            TokenKind::Keyword(Keyword::Super) => {
                self.bump();
                "super".to_string()
            }
            TokenKind::Keyword(Keyword::Crate) => {
                self.bump();
                "crate".to_string()
            }
            _ => {
                self.record(
                    ParseError::unexpected("a type name", self.peek_text()),
                    token.span,
                );
                gossamer_ast::ERROR_IDENT.to_string()
            }
        }
    }

    fn parse_type_generic_args(
        &mut self,
        mut bindings: Option<&mut Vec<AssocBinding>>,
    ) -> Vec<GenericArg> {
        if !self.eat_punct(Punct::Lt) {
            return Vec::new();
        }
        let mut args = Vec::new();
        while !self.at_close_angle() && !self.at_eof() {
            match self.parse_assoc_binding(bindings.is_some()) {
                Some(binding) => {
                    if let Some(sink) = bindings.as_deref_mut() {
                        sink.push(binding);
                    }
                }
                None => args.push(self.parse_generic_arg()),
            }
            if !self.eat_list_separator() {
                break;
            }
        }
        self.expect_close_angle("to close generic argument list");
        args
    }

    /// Reads a `Name = Type` associated-type constraint when one starts here
    /// and the surrounding argument list accepts constraints. Returns `None`
    /// without consuming anything otherwise, so the caller falls back to an
    /// ordinary generic argument.
    fn parse_assoc_binding(&mut self, accepts_bindings: bool) -> Option<AssocBinding> {
        if !accepts_bindings || !self.at_assoc_binding_start() {
            return None;
        }
        let name_span = self.peek_span();
        self.bump();
        let name = Ident::new(self.slice(name_span).to_string());
        self.bump();
        let ty = self.parse_type();
        Some(AssocBinding { name, ty })
    }

    /// `true` when the cursor sits on `Ident =`, the shape of an
    /// associated-type constraint inside a trait bound. `==` lexes as one
    /// token, so it never matches here.
    fn at_assoc_binding_start(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Ident)
            && matches!(self.peek_nth(1).kind, TokenKind::Punct(Punct::Eq))
    }

    /// Parses one generic argument (type or const expression).
    pub(crate) fn parse_generic_arg(&mut self) -> GenericArg {
        if is_const_arg_start(self) {
            return GenericArg::Const(self.parse_const_generic_arg());
        }
        GenericArg::Type(self.parse_type())
    }
}

/// Returns `true` when the upcoming generic argument is a const expression
/// (integer literal, bool keyword, or a parenthesised expression).
fn is_const_arg_start(parser: &Parser<'_>) -> bool {
    matches!(
        parser.peek().kind,
        TokenKind::IntLit
            | TokenKind::FloatLit
            | TokenKind::Keyword(Keyword::True | Keyword::False)
    ) || parser.at_punct(Punct::LBrace)
}

/// Returns `true` for built-in scalar / primitive type names that
/// can never carry generic arguments. Used by
/// [`Parser::parse_type_path_segment`] to skip a `<…>` lookahead
/// after a primitive - required so `expr as i64 < width` parses as
/// the comparison the user wrote rather than as an opening generic
/// argument list. The list is intentionally narrow; user-defined
/// types and stdlib types like `Vec` / `Result` / `Option` are not
/// here so their `<…>` arguments still parse.
fn is_non_generic_primitive(name: &str) -> bool {
    matches!(
        name,
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "bool"
            | "char"
            | "str"
    )
}

/// Returns `true` when the cursor sits on a token that can only follow a
/// type, never begin one.
fn closes_type_position(parser: &Parser<'_>) -> bool {
    parser.at_punct(Punct::RParen)
        || parser.at_punct(Punct::RBracket)
        || parser.at_punct(Punct::RBrace)
        || parser.at_punct(Punct::LBrace)
        || parser.at_punct(Punct::Comma)
        || parser.at_punct(Punct::Semi)
        || parser.at_punct(Punct::Arrow)
        || parser.at_punct(Punct::Eq)
}
