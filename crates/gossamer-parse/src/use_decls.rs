//! Parses `use` declarations (SPEC §6.6).

#![forbid(unsafe_code)]

use gossamer_ast::{Ident, ModulePath, UseDecl, UseListEntry, UseTarget};
use gossamer_lex::{Keyword, Punct, TokenKind};

use crate::diagnostic::ParseError;
use crate::parser::Parser;

impl Parser<'_> {
    /// Parses a `use` declaration at the `use` keyword. A grouped
    /// `use { a::B, c::{d, e} }` answers one declaration per entry, each
    /// numbered with the group it was written in.
    pub(crate) fn parse_use_decls(&mut self) -> Vec<UseDecl> {
        let start_span = self.peek_span();
        self.bump();
        if !self.at_punct(Punct::LBrace) {
            let decl = self.parse_use_body(start_span);
            self.reject_trailing_semicolon();
            return vec![decl];
        }
        let group = self.use_groups;
        self.use_groups += 1;
        let mut decls = Vec::new();
        self.parse_use_group(group, &mut decls);
        self.reject_trailing_semicolon();
        decls
    }

    /// Parses a brace group of `use` entries at its `{`, each nested group
    /// adding its entries to the same `group`.
    fn parse_use_group(&mut self, group: u32, decls: &mut Vec<UseDecl>) {
        self.bump();
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            if self.at_punct(Punct::LBrace) {
                self.parse_use_group(group, decls);
            } else {
                let mut decl = self.parse_use_body(self.peek_span());
                decl.group = Some(group);
                decls.push(decl);
            }
            if !self.eat_list_separator() {
                break;
            }
        }
        self.expect_punct(Punct::RBrace, "to close `use` group");
    }

    /// Parses what follows `use` in a single declaration, or one entry of
    /// a group, starting at `start_span`.
    fn parse_use_body(&mut self, start_span: gossamer_lex::Span) -> UseDecl {
        let target = if matches!(self.peek().kind, TokenKind::StringLit) {
            self.parse_project_use_target()
        } else {
            self.parse_module_use_target()
        };
        let alias = if self.eat_keyword(Keyword::As) {
            Some(self.parse_use_ident())
        } else {
            None
        };
        let mut list_follows = false;
        if self.at_punct(Punct::ColonColon)
            && matches!(self.peek_nth(1).kind, TokenKind::Punct(Punct::LBrace))
        {
            self.bump();
            list_follows = true;
        }
        let glob = self.at_punct(Punct::ColonColon)
            && matches!(self.peek_nth(1).kind, TokenKind::Punct(Punct::Star));
        if glob {
            self.bump();
            self.bump();
        }
        // A `{` on a later line without `::` opens the next entry of an
        // enclosing group, not this path's list.
        let list = if self.at_punct(Punct::LBrace) && (list_follows || !self.newline_before_peek())
        {
            Some(self.parse_use_list())
        } else {
            None
        };
        let end_span = self.last_span();
        let span = self.join(start_span, end_span);
        let id = self.alloc_id();
        UseDecl {
            id,
            span,
            target,
            alias,
            list,
            module: self.mod_stack.clone(),
            cfg: None,
            glob,
            group: None,
        }
    }

    /// Whether the cursor is at attributes followed by `use`.
    pub(crate) fn at_attributed_use(&self) -> bool {
        let mut index = 0;
        let mut saw_attribute = false;
        loop {
            let at_hash = matches!(self.peek_nth(index).kind, TokenKind::Punct(Punct::Hash));
            if !at_hash || !self.peek_nth_is_punct(index + 1, Punct::LBracket) {
                break;
            }
            saw_attribute = true;
            index += 2;
            let mut depth = 1;
            while depth > 0 {
                match self.peek_nth(index).kind {
                    TokenKind::Punct(Punct::LBracket) => depth += 1,
                    TokenKind::Punct(Punct::RBracket) => depth -= 1,
                    TokenKind::Eof => return false,
                    _ => {}
                }
                index += 1;
            }
        }
        saw_attribute && matches!(self.peek_nth(index).kind, TokenKind::Keyword(Keyword::Use))
    }

    /// `#[cfg(..)] use ..`: the declarations with the conjunction of its
    /// `cfg` attributes. Other attributes on an import mean nothing and are
    /// dropped.
    pub(crate) fn parse_attributed_use(&mut self) -> Vec<UseDecl> {
        let attrs = self.parse_attrs();
        let conditions: Vec<String> = attrs
            .outer
            .iter()
            .filter(|attr| attr.is_named("cfg"))
            .filter_map(|attr| attr.tokens.clone())
            .collect();
        let cfg = match conditions.as_slice() {
            [] => None,
            [one] => Some(one.clone()),
            many => Some(format!("all({})", many.join(", "))),
        };
        let mut decls = self.parse_use_decls();
        for decl in &mut decls {
            decl.cfg.clone_from(&cfg);
        }
        decls
    }

    fn parse_project_use_target(&mut self) -> UseTarget {
        let lit_span = self.peek_span();
        self.bump();
        let raw = self.slice(lit_span);
        let project_id = raw
            .strip_prefix('"')
            .and_then(|text| text.strip_suffix('"'))
            .unwrap_or(raw)
            .to_string();
        let module = if self.eat_punct(Punct::ColonColon) {
            Some(self.parse_module_path())
        } else {
            None
        };
        UseTarget::Project {
            id: project_id,
            module,
        }
    }

    fn parse_module_use_target(&mut self) -> UseTarget {
        let path = self.parse_module_path();
        self.reject_hyphenated_use_path(&path);
        UseTarget::Module(path)
    }

    /// Reports a `use` path written with the hyphens a package name may carry
    /// (`use pgsql-gos`). A `-` is subtraction, never part of an identifier,
    /// so the path stops at the first one and the rest would parse as an
    /// expression. The segments are consumed here so that misreading does not
    /// produce a second, unrelated diagnostic.
    fn reject_hyphenated_use_path(&mut self, path: &ModulePath) {
        if !self.at_punct(Punct::Minus)
            || self.newline_before_peek()
            || !matches!(self.peek_nth(1).kind, TokenKind::Ident)
        {
            return;
        }
        let start = self.peek_span();
        let mut written = path
            .segments
            .iter()
            .map(|segment| segment.name.as_str())
            .collect::<Vec<_>>()
            .join("::");
        while self.at_punct(Punct::Minus)
            && !self.newline_before_peek()
            && matches!(self.peek_nth(1).kind, TokenKind::Ident)
        {
            self.bump();
            let span = self.peek_span();
            self.bump();
            written.push('-');
            written.push_str(self.slice(span));
        }
        let end = self.last_span();
        let module = written.replace('-', "_");
        self.record(
            ParseError::HyphenInUsePath { written, module },
            self.join(start, end),
        );
    }

    fn parse_module_path(&mut self) -> ModulePath {
        let mut segments = Vec::new();
        segments.push(self.parse_use_ident());
        while self.at_punct(Punct::ColonColon) {
            let checkpoint = self.tokens.checkpoint();
            self.bump();
            if self.at_punct(Punct::LBrace) || self.at_punct(Punct::Star) {
                self.tokens.rewind(checkpoint);
                break;
            }
            segments.push(self.parse_use_ident());
        }
        ModulePath { segments }
    }

    fn parse_use_ident(&mut self) -> Ident {
        let span = self.peek_span();
        match self.peek().kind {
            TokenKind::Ident => {
                self.bump();
                Ident::new(self.slice(span))
            }
            TokenKind::Keyword(Keyword::Crate) => {
                self.bump();
                Ident::new("crate")
            }
            TokenKind::Keyword(Keyword::Super) => {
                self.bump();
                Ident::new("super")
            }
            TokenKind::Keyword(Keyword::SelfLower) => {
                self.bump();
                Ident::new("self")
            }
            _ => {
                self.record(ParseError::MalformedUse, span);
                Ident::new("<error>")
            }
        }
    }

    fn parse_use_list(&mut self) -> Vec<UseListEntry> {
        self.bump();
        let mut entries = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            self.parse_use_list_entry(&[], &mut entries);
            if !self.eat_list_separator() {
                break;
            }
        }
        self.expect_punct(Punct::RBrace, "to close `use` list");
        entries
    }

    /// Parses one brace-list entry, which may be a multi-segment path
    /// (`encoding::json`) and may itself open a nested brace group
    /// (`encoding::{json, yaml}`). `outer` carries the segments contributed
    /// by any enclosing group so a nested entry records its full prefix.
    fn parse_use_list_entry(&mut self, outer: &[Ident], entries: &mut Vec<UseListEntry>) {
        let mut prefix: Vec<Ident> = outer.to_vec();
        let mut name = self.parse_use_ident();
        while self.at_punct(Punct::ColonColon) {
            self.bump();
            // `a::{b, c}` - a nested group: recurse with `a` folded into
            // the prefix so each inner entry keeps the full path.
            if self.at_punct(Punct::LBrace) {
                let mut nested_prefix = prefix.clone();
                nested_prefix.push(name);
                self.bump();
                while !self.at_punct(Punct::RBrace) && !self.at_eof() {
                    self.parse_use_list_entry(&nested_prefix, entries);
                    if !self.eat_list_separator() {
                        break;
                    }
                }
                self.expect_punct(Punct::RBrace, "to close `use` list");
                return;
            }
            prefix.push(name);
            name = self.parse_use_ident();
        }
        let alias = if self.eat_keyword(Keyword::As) {
            Some(self.parse_use_ident())
        } else {
            None
        };
        entries.push(UseListEntry {
            prefix,
            name,
            alias,
        });
    }
}
