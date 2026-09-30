//! The adapter and terminal methods of a program's own `impl Iterator`,
//! written in Gossamer.
//!
//! Every adapter is a generic struct that implements `Iterator` over the
//! iterator it wraps, so a chain stays lazy and composes with `for`, with
//! other adapters, and with built-in iterators. The structs are declared once
//! at the unit root; each iterator the program declares gets an inherent
//! `impl` of the methods it does not already define, written where the
//! iterator is declared so its names resolve as the program wrote them.

use std::collections::{BTreeMap, HashSet};

use gossamer_ast::{ImplDecl, ImplItem, Item, ItemKind, ModBody, NodeId, SourceFile, TypeKind};
use gossamer_lex::{Keyword, Lexer, Punct, SourceMap, TokenKind};

/// Name of the module an iterator's methods are emitted under when the
/// iterator is declared inside a module; [`splice_iterator_methods`] moves
/// them into that module.
pub(crate) const SPLICE_MODULE: &str = "__gos_iter_splice";

/// One adapter: its struct, its `Iterator` impl, and the item type it yields.
struct Adapter {
    /// Generic parameters of the `impl` blocks, bounds included.
    generics: &'static str,
    /// The struct as a type, with its parameters.
    self_ty: &'static str,
    /// What `next` answers inside `Option`.
    item: &'static str,
    /// Struct declaration.
    decl: &'static str,
    /// Body of `next`.
    next: &'static str,
}

const ADAPTERS: &[Adapter] = &[
    Adapter {
        generics: "<I: Iterator<Item = A>, A, B>",
        self_ty: "__GosMap<I, A, B>",
        item: "B",
        decl: "pub struct __GosMap<I, A, B> { inner: I, f: Fn(A) -> B }",
        next: "
        match self.inner.next() {
            Some(v) => Some((self.f)(v)),
            None => None,
        }",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, A>",
        self_ty: "__GosFilter<I, A>",
        item: "A",
        decl: "pub struct __GosFilter<I, A> { inner: I, f: Fn(A) -> bool }",
        next: "
        while let Some(v) = self.inner.next() {
            if (self.f)(v) {
                return Some(v)
            }
        }
        None",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, A, B>",
        self_ty: "__GosFilterMap<I, A, B>",
        item: "B",
        decl: "pub struct __GosFilterMap<I, A, B> { inner: I, f: Fn(A) -> Option<B> }",
        next: "
        while let Some(v) = self.inner.next() {
            if let Some(mapped) = (self.f)(v) {
                return Some(mapped)
            }
        }
        None",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, A, B>",
        self_ty: "__GosFlatMap<I, A, B>",
        item: "B",
        decl: "pub struct __GosFlatMap<I, A, B> { inner: I, f: Fn(A) -> Vec<B>, buf: Vec<B>, pos: i64 }",
        next: "
        loop {
            if self.pos < self.buf.len() {
                let v = self.buf[self.pos]
                self.pos += 1
                return Some(v)
            }
            match self.inner.next() {
                Some(v) => {
                    self.buf = (self.f)(v)
                    self.pos = 0
                }
                None => return None,
            }
        }",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, A>",
        self_ty: "__GosTake<I, A>",
        item: "A",
        decl: "pub struct __GosTake<I, A> { inner: I, left: i64 }",
        next: "
        if self.left <= 0 {
            return None
        }
        self.left -= 1
        self.inner.next()",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, A>",
        self_ty: "__GosSkip<I, A>",
        item: "A",
        decl: "pub struct __GosSkip<I, A> { inner: I, left: i64 }",
        next: "
        while self.left > 0 {
            self.left -= 1
            if self.inner.next().is_none() {
                return None
            }
        }
        self.inner.next()",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, A>",
        self_ty: "__GosStepBy<I, A>",
        item: "A",
        decl: "pub struct __GosStepBy<I, A> { inner: I, step: i64, first: bool }",
        next: "
        if self.first {
            self.first = false
            return self.inner.next()
        }
        let mut skipped = 1
        while skipped < self.step {
            if self.inner.next().is_none() {
                return None
            }
            skipped += 1
        }
        self.inner.next()",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, A>",
        self_ty: "__GosEnumerate<I, A>",
        item: "(i64, A)",
        decl: "pub struct __GosEnumerate<I, A> { inner: I, index: i64 }",
        next: "
        match self.inner.next() {
            Some(v) => {
                let at = self.index
                self.index += 1
                Some((at, v))
            }
            None => None,
        }",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, A>",
        self_ty: "__GosTakeWhile<I, A>",
        item: "A",
        decl: "pub struct __GosTakeWhile<I, A> { inner: I, f: Fn(A) -> bool, done: bool }",
        next: "
        if self.done {
            return None
        }
        match self.inner.next() {
            Some(v) => {
                if (self.f)(v) {
                    return Some(v)
                }
                self.done = true
                None
            }
            None => None,
        }",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, A>",
        self_ty: "__GosSkipWhile<I, A>",
        item: "A",
        decl: "pub struct __GosSkipWhile<I, A> { inner: I, f: Fn(A) -> bool, started: bool }",
        next: "
        if !self.started {
            self.started = true
            while let Some(v) = self.inner.next() {
                if !(self.f)(v) {
                    return Some(v)
                }
            }
            return None
        }
        self.inner.next()",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, J: Iterator<Item = A>, A>",
        self_ty: "__GosChain<I, J, A>",
        item: "A",
        decl: "pub struct __GosChain<I, J, A> { first: I, second: J, first_done: bool }",
        next: "
        if !self.first_done {
            match self.first.next() {
                Some(v) => return Some(v),
                None => self.first_done = true,
            }
        }
        self.second.next()",
    },
    Adapter {
        generics: "<I: Iterator<Item = A>, J: Iterator<Item = B>, A, B>",
        self_ty: "__GosZip<I, J, A, B>",
        item: "(A, B)",
        decl: "pub struct __GosZip<I, J, A, B> { first: I, second: J }",
        next: "
        let Some(a) = self.first.next() else {
            return None
        }
        let Some(b) = self.second.next() else {
            return None
        }
        Some((a, b))",
    },
];

/// Each method an iterator gains, by name. `S` names the iterator type, `T`
/// its item, and `P::` the unit root the adapter structs live at.
const METHODS: &[(&str, &str)] = &[
    (
        "map",
        "pub fn map<__B>(self, f: Fn(T) -> __B) -> P::__GosMap<S, T, __B> {
        P::__GosMap { inner: self, f: f }
    }",
    ),
    (
        "filter",
        "pub fn filter(self, f: Fn(T) -> bool) -> P::__GosFilter<S, T> {
        P::__GosFilter { inner: self, f: f }
    }",
    ),
    (
        "filter_map",
        "pub fn filter_map<__B>(self, f: Fn(T) -> Option<__B>) -> P::__GosFilterMap<S, T, __B> {
        P::__GosFilterMap { inner: self, f: f }
    }",
    ),
    (
        "flat_map",
        "pub fn flat_map<__B>(self, f: Fn(T) -> Vec<__B>) -> P::__GosFlatMap<S, T, __B> {
        P::__GosFlatMap { inner: self, f: f, buf: #[], pos: 0 }
    }",
    ),
    (
        "take",
        "pub fn take(self, n: i64) -> P::__GosTake<S, T> {
        P::__GosTake { inner: self, left: n }
    }",
    ),
    (
        "skip",
        "pub fn skip(self, n: i64) -> P::__GosSkip<S, T> {
        P::__GosSkip { inner: self, left: n }
    }",
    ),
    (
        "step_by",
        "pub fn step_by(self, step: i64) -> P::__GosStepBy<S, T> {
        if step <= 0 {
            panic(\"step_by: the step must be positive\")
        }
        P::__GosStepBy { inner: self, step: step, first: true }
    }",
    ),
    (
        "enumerate",
        "pub fn enumerate(self) -> P::__GosEnumerate<S, T> {
        P::__GosEnumerate { inner: self, index: 0 }
    }",
    ),
    (
        "take_while",
        "pub fn take_while(self, f: Fn(T) -> bool) -> P::__GosTakeWhile<S, T> {
        P::__GosTakeWhile { inner: self, f: f, done: false }
    }",
    ),
    (
        "skip_while",
        "pub fn skip_while(self, f: Fn(T) -> bool) -> P::__GosSkipWhile<S, T> {
        P::__GosSkipWhile { inner: self, f: f, started: false }
    }",
    ),
    (
        "chain",
        "pub fn chain<__J: Iterator<Item = T>>(self, other: __J) -> P::__GosChain<S, __J, T> {
        P::__GosChain { first: self, second: other, first_done: false }
    }",
    ),
    (
        "zip",
        "pub fn zip<__J: Iterator<Item = __B>, __B>(self, other: __J) -> P::__GosZip<S, __J, T, __B> {
        P::__GosZip { first: self, second: other }
    }",
    ),
    (
        "collect",
        "pub fn collect(self) -> Vec<T> {
        let mut it = self
        let mut out = #[]
        while let Some(v) = it.next() {
            out.push(v)
        }
        out
    }",
    ),
    (
        "count",
        "pub fn count(self) -> i64 {
        let mut it = self
        let mut n = 0
        while let Some(_) = it.next() {
            n += 1
        }
        n
    }",
    ),
    (
        "sum",
        "pub fn sum(self) -> T {
        self.collect().sum()
    }",
    ),
    (
        "product",
        "pub fn product(self) -> T {
        self.collect().product()
    }",
    ),
    (
        "min",
        "pub fn min(self) -> Option<T> {
        self.collect().min()
    }",
    ),
    (
        "max",
        "pub fn max(self) -> Option<T> {
        self.collect().max()
    }",
    ),
    (
        "min_by_key",
        "pub fn min_by_key<__B>(self, f: Fn(T) -> __B) -> Option<T> {
        self.collect().min_by_key(f)
    }",
    ),
    (
        "max_by_key",
        "pub fn max_by_key<__B>(self, f: Fn(T) -> __B) -> Option<T> {
        self.collect().max_by_key(f)
    }",
    ),
    (
        "join",
        "pub fn join(self, sep: String) -> String {
        self.collect().join(sep)
    }",
    ),
    (
        "fold",
        "pub fn fold<__B>(self, init: __B, f: Fn(__B, T) -> __B) -> __B {
        let mut it = self
        let mut acc = init
        while let Some(v) = it.next() {
            acc = f(acc, v)
        }
        acc
    }",
    ),
    (
        "reduce",
        "pub fn reduce(self, f: Fn(T, T) -> T) -> Option<T> {
        let mut it = self
        let Some(first) = it.next() else {
            return None
        }
        let mut acc = first
        while let Some(v) = it.next() {
            acc = f(acc, v)
        }
        Some(acc)
    }",
    ),
    (
        "any",
        "pub fn any(self, f: Fn(T) -> bool) -> bool {
        let mut it = self
        while let Some(v) = it.next() {
            if f(v) {
                return true
            }
        }
        false
    }",
    ),
    (
        "all",
        "pub fn all(self, f: Fn(T) -> bool) -> bool {
        let mut it = self
        while let Some(v) = it.next() {
            if !f(v) {
                return false
            }
        }
        true
    }",
    ),
    (
        "find",
        "pub fn find(self, f: Fn(T) -> bool) -> Option<T> {
        let mut it = self
        while let Some(v) = it.next() {
            if f(v) {
                return Some(v)
            }
        }
        None
    }",
    ),
    (
        "find_map",
        "pub fn find_map<__B>(self, f: Fn(T) -> Option<__B>) -> Option<__B> {
        let mut it = self
        while let Some(v) = it.next() {
            if let Some(found) = f(v) {
                return Some(found)
            }
        }
        None
    }",
    ),
    (
        "position",
        "pub fn position(self, f: Fn(T) -> bool) -> Option<i64> {
        let mut it = self
        let mut at = 0
        while let Some(v) = it.next() {
            if f(v) {
                return Some(at)
            }
            at += 1
        }
        None
    }",
    ),
    (
        "for_each",
        "pub fn for_each(self, f: Fn(T)) {
        let mut it = self
        while let Some(v) = it.next() {
            f(v)
        }
    }",
    ),
    (
        "last",
        "pub fn last(self) -> Option<T> {
        let mut it = self
        let mut last = None
        while let Some(v) = it.next() {
            last = Some(v)
        }
        last
    }",
    ),
    (
        "nth",
        "pub fn nth(self, n: i64) -> Option<T> {
        let mut it = self
        let mut left = n
        while let Some(v) = it.next() {
            if left == 0 {
                return Some(v)
            }
            left -= 1
        }
        None
    }",
    ),
];

/// One `impl Iterator for X` the program declares.
struct UserIterator {
    /// `::`-joined path of the declaring module; empty at the unit root.
    module: String,
    generics: String,
    self_ty: String,
    where_clause: String,
    item: String,
    /// Methods the type already has, which the injected surface leaves alone.
    defined: HashSet<String>,
}

/// Source for the adapter structs and every declared iterator's methods, or
/// an empty string when the program declares no iterator.
pub(crate) fn synthesize_iterator_adapters(parsed: &SourceFile, source: &str) -> String {
    let flat = super::flatten_items_with_modules(&parsed.items);
    // A program that declares its own `trait Iterator` names that trait in
    // every `impl Iterator`, and the adapters implement the built-in one.
    if flat.iter().any(
        |(_, item)| matches!(&item.kind, ItemKind::Trait(decl) if decl.name.name == "Iterator"),
    ) {
        return String::new();
    }
    let iterators: Vec<UserIterator> = flat
        .iter()
        .filter_map(|(module, item)| user_iterator(module, item, &flat, source))
        .collect();
    if iterators.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for adapter in ADAPTERS {
        out.push_str(adapter.decl);
        out.push('\n');
        out.push_str(&format!(
            "impl{} Iterator for {} {{\n    type Item = {}\n    fn next(&mut self) -> Option<{}> {{{}\n    }}\n}}\n",
            adapter.generics, adapter.self_ty, adapter.item, adapter.item, adapter.next
        ));
        out.push_str(&methods_impl(
            adapter.generics,
            adapter.self_ty,
            "",
            adapter.item,
            &HashSet::new(),
            "crate",
        ));
    }
    let mut nested: BTreeMap<String, String> = BTreeMap::new();
    for iterator in &iterators {
        let block = methods_impl(
            &iterator.generics,
            &iterator.self_ty,
            &iterator.where_clause,
            &iterator.item,
            &iterator.defined,
            "crate",
        );
        if iterator.module.is_empty() {
            out.push_str(&block);
        } else {
            nested
                .entry(iterator.module.clone())
                .or_default()
                .push_str(&block);
        }
    }
    if !nested.is_empty() {
        out.push_str(&format!("mod {SPLICE_MODULE} {{\n"));
        for (module, block) in &nested {
            let segments: Vec<&str> = module.split("::").collect();
            for segment in &segments {
                out.push_str(&format!("mod {segment} {{\n"));
            }
            out.push_str(block);
            for _ in &segments {
                out.push_str("}\n");
            }
        }
        out.push_str("}\n");
    }
    out
}

/// The inherent `impl` giving `self_ty` every adapter and terminal method it
/// does not define itself.
fn methods_impl(
    generics: &str,
    self_ty: &str,
    where_clause: &str,
    item: &str,
    defined: &HashSet<String>,
    root: &str,
) -> String {
    let mut out = format!("impl{generics} {self_ty} {where_clause} {{\n");
    for (name, template) in METHODS {
        if defined.contains(*name) {
            continue;
        }
        out.push_str("    ");
        out.push_str(&instantiate(template, self_ty, item, root));
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

/// A method template with `S`, `T`, and `P` replaced. Each is a whole token
/// in the templates, so an identifier-boundary scan finds exactly them.
fn instantiate(template: &str, self_ty: &str, item: &str, root: &str) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_alphanumeric() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &template[start..i];
            let in_string = template[..start].matches('"').count() % 2 == 1;
            match word {
                "S" if !in_string => out.push_str(self_ty),
                "T" if !in_string => out.push_str(item),
                "P" if !in_string => out.push_str(root),
                _ => out.push_str(word),
            }
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    out
}

/// The iterator `item` declares, when it is an `impl Iterator for X` whose
/// item type is written.
fn user_iterator(
    module: &str,
    item: &Item,
    flat: &[(String, &Item)],
    source: &str,
) -> Option<UserIterator> {
    let ItemKind::Impl(decl) = &item.kind else {
        return None;
    };
    if decl.trait_ref.as_ref()?.trait_name()? != "Iterator" {
        return None;
    }
    let head = type_head(&decl.self_ty)?;
    let item_ty = decl
        .items
        .iter()
        .find_map(|impl_item| match impl_item {
            ImplItem::Type { name, ty, .. } if name.name == "Item" => Some(slice(source, ty.span)),
            _ => None,
        })
        .or_else(|| next_payload(decl, source))?;
    let (generics, where_clause) = header_text(item, decl, source)?;
    let defined = flat
        .iter()
        .filter(|(other_module, _)| other_module == module)
        .filter_map(|(_, other)| match &other.kind {
            ItemKind::Impl(other_decl) if type_head(&other_decl.self_ty) == Some(head) => {
                Some(other_decl)
            }
            _ => None,
        })
        .flat_map(|other_decl| {
            other_decl
                .items
                .iter()
                .filter_map(|impl_item| match impl_item {
                    ImplItem::Fn(f) => Some(f.name.name.clone()),
                    _ => None,
                })
        })
        .collect();
    Some(UserIterator {
        module: module.to_string(),
        generics,
        self_ty: slice(source, decl.self_ty.span),
        where_clause,
        item: item_ty,
        defined,
    })
}

/// The `T` of a `fn next(&mut self) -> Option<T>` in `decl`.
fn next_payload(decl: &ImplDecl, source: &str) -> Option<String> {
    let ret = decl.items.iter().find_map(|impl_item| match impl_item {
        ImplItem::Fn(f) if f.name.name == "next" => f.ret.as_ref(),
        _ => None,
    })?;
    let TypeKind::Path(path) = &ret.kind else {
        return None;
    };
    let last = path.segments.last()?;
    if last.name.name != "Option" {
        return None;
    }
    match last.generics.as_slice() {
        [gossamer_ast::GenericArg::Type(inner)] => Some(slice(source, inner.span)),
        _ => None,
    }
}

/// The written `<..>` parameter list after `impl` and the `where` clause
/// between the self type and the body, as source text.
fn header_text(item: &Item, decl: &ImplDecl, source: &str) -> Option<(String, String)> {
    let start = item.span.start as usize;
    let self_start = decl.self_ty.span.start as usize;
    let self_end = decl.self_ty.span.end as usize;
    let generics = if decl.generics.is_empty() {
        String::new()
    } else {
        let head = source.get(start..self_start)?;
        let open = head.find('<')?;
        let close = matching_angle(&head[open..])?;
        head[open..open + close].to_string()
    };
    let tail = source.get(self_end..)?;
    let brace = tail.find('{')?;
    Some((generics, tail[..brace].trim().to_string()))
}

/// Length of the `<..>` list `text` opens, through its closing `>`.
fn matching_angle(text: &str) -> Option<usize> {
    let mut map = SourceMap::new();
    let file = map.add_file("<iterator-header>", String::new());
    let mut lexer = Lexer::new(text, file);
    let mut depth = 0i32;
    loop {
        let token = lexer.next_token();
        match token.kind {
            TokenKind::Punct(Punct::Lt) => depth += 1,
            TokenKind::Punct(Punct::Gt) => depth -= 1,
            TokenKind::Punct(Punct::ShiftR) => depth -= 2,
            TokenKind::Keyword(Keyword::For) | TokenKind::Eof => return None,
            _ => {}
        }
        if depth <= 0 {
            return Some(token.span.end as usize);
        }
    }
}

fn type_head(ty: &gossamer_ast::Type) -> Option<&str> {
    let TypeKind::Path(path) = &ty.kind else {
        return None;
    };
    path.segments.last().map(|s| s.name.name.as_str())
}

fn slice(source: &str, span: gossamer_lex::Span) -> String {
    source
        .get(span.start as usize..span.end as usize)
        .unwrap_or_default()
        .to_string()
}

/// Moves the methods emitted under [`SPLICE_MODULE`] into the modules that
/// declare their iterators, and drops the carrier module.
pub(crate) fn splice_iterator_methods(sf: &mut SourceFile) {
    let Some(at) = sf.items.iter().position(
        |item| matches!(&item.kind, ItemKind::Mod(decl) if decl.name.name == SPLICE_MODULE),
    ) else {
        return;
    };
    let carrier = sf.items.remove(at);
    let ItemKind::Mod(decl) = carrier.kind else {
        return;
    };
    let ModBody::Inline(items) = decl.body else {
        return;
    };
    splice_into(&mut sf.items, items);
}

fn splice_into(target: &mut Vec<Item>, carried: Vec<Item>) {
    for item in carried {
        match item.kind {
            ItemKind::Mod(decl) => {
                let ModBody::Inline(inner) = decl.body else {
                    continue;
                };
                let Some(destination) = target.iter_mut().find_map(|t| match &mut t.kind {
                    ItemKind::Mod(target_decl) if target_decl.name.name == decl.name.name => {
                        match &mut target_decl.body {
                            ModBody::Inline(items) => Some(items),
                            ModBody::External => None,
                        }
                    }
                    _ => None,
                }) else {
                    continue;
                };
                splice_into(destination, inner);
            }
            _ => target.push(item),
        }
    }
}

/// Name of the function `collect` into a `Result<Vec<T>, E>` becomes.
const COLLECT_RESULT: &str = "__gos_collect_result";
/// Name of the function `collect` into an `Option<Vec<T>>` becomes.
const COLLECT_OPTION: &str = "__gos_collect_option";

/// The two collectors a fallible `collect` reaches. Each stops pulling at the
/// first `Err` or `None`, so the stages behind it run no further.
const COLLECT_HELPERS: &str = "
fn __gos_collect_result<I: Iterator<Item = Result<T, E>>, T, E>(mut it: I) -> Result<Vec<T>, E> {
    let mut out = #[]
    while let Some(item) = it.next() {
        match item {
            Ok(v) => out.push(v),
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}
fn __gos_collect_option<I: Iterator<Item = Option<T>>, T>(mut it: I) -> Option<Vec<T>> {
    let mut out = #[]
    while let Some(item) = it.next() {
        match item {
            Some(v) => out.push(v),
            None => return None,
        }
    }
    Some(out)
}
";

/// Source for the fallible collectors, when the program may reach one.
pub(crate) fn synthesize_collect_helpers(source: &str) -> String {
    if source.contains("collect") && (source.contains("Result") || source.contains("Option")) {
        COLLECT_HELPERS.to_string()
    } else {
        String::new()
    }
}

/// Which fallible collection a type names.
#[derive(Clone, Copy)]
enum Fallible {
    Result,
    Option,
}

impl Fallible {
    fn collector(self) -> &'static str {
        match self {
            Self::Result => COLLECT_RESULT,
            Self::Option => COLLECT_OPTION,
        }
    }
}

/// `Result<..>` or `Option<..>`, when `ty` names one; with `needs_vec`, only
/// when its first argument is a `Vec`.
fn fallible_shape(ty: &gossamer_ast::Type, needs_vec: bool) -> Option<Fallible> {
    let TypeKind::Path(path) = &ty.kind else {
        return None;
    };
    let last = path.segments.last()?;
    let shape = match last.name.name.as_str() {
        "Result" => Fallible::Result,
        "Option" => Fallible::Option,
        _ => return None,
    };
    let Some(gossamer_ast::GenericArg::Type(first)) = last.generics.first() else {
        return None;
    };
    if needs_vec && type_head(first) != Some("Vec") {
        return None;
    }
    Some(shape)
}

/// Replaces `recv.collect()` with `collector(recv)`.
fn to_collector(expr: &mut gossamer_ast::expr::Expr, shape: Fallible) {
    use gossamer_ast::expr::{Expr, ExprKind};

    let ExprKind::MethodCall {
        receiver,
        name,
        args,
        ..
    } = &expr.kind
    else {
        return;
    };
    if name.name != "collect" || !args.is_empty() {
        return;
    }
    let receiver = (**receiver).clone();
    let callee = Expr {
        id: NodeId::DUMMY,
        span: expr.span,
        kind: ExprKind::Path(gossamer_ast::PathExpr {
            segments: vec![gossamer_ast::PathSegment::new(shape.collector())],
        }),
    };
    expr.kind = ExprKind::Call {
        callee: Box::new(callee),
        args: vec![receiver],
    };
}

/// Applies [`to_collector`] to every expression `expr` answers as its
/// value: itself, a block's tail, and each branch of an `if` or `match`.
fn rewrite_value(expr: &mut gossamer_ast::expr::Expr, shape: Fallible) {
    use gossamer_ast::expr::ExprKind;

    match &mut expr.kind {
        ExprKind::Block(block) => {
            if let Some(tail) = &mut block.tail {
                rewrite_value(tail, shape);
            }
        }
        ExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            rewrite_value(then_branch, shape);
            if let Some(other) = else_branch {
                rewrite_value(other, shape);
            }
        }
        ExprKind::Match { arms, .. } => {
            for arm in arms {
                rewrite_value(&mut arm.body, shape);
            }
        }
        _ => to_collector(expr, shape),
    }
}

/// Rewrites a `collect()` whose context names `Result<Vec<T>, E>` or
/// `Option<Vec<T>>` into the collector for it: a turbofish on the call, the
/// type of the `let` it initialises, or the return type of the function whose
/// value it is.
pub(crate) fn rewrite_fallible_collect(sf: &mut SourceFile) {
    use gossamer_ast::VisitorMut;
    use gossamer_ast::expr::{Expr, ExprKind};
    use gossamer_ast::stmt::StmtKind;
    use gossamer_ast::visitor::{walk_expr_mut, walk_item_mut, walk_stmt_mut};

    struct Rewriter {
        /// The fallible shape each enclosing function returns; a closure
        /// answers for itself, so it enters `None`.
        returns: Vec<Option<Fallible>>,
    }

    impl VisitorMut for Rewriter {
        fn visit_item(&mut self, item: &mut Item) {
            let shape = match &item.kind {
                ItemKind::Fn(decl) => decl.ret.as_ref().and_then(|ty| fallible_shape(ty, true)),
                _ => None,
            };
            if let ItemKind::Fn(decl) = &mut item.kind
                && let (Some(shape), Some(body)) = (shape, &mut decl.body)
            {
                rewrite_value(body, shape);
            }
            self.returns.push(shape);
            walk_item_mut(self, item);
            self.returns.pop();
        }

        fn visit_stmt(&mut self, stmt: &mut gossamer_ast::Stmt) {
            if let StmtKind::Let {
                ty: Some(ty),
                init: Some(init),
                ..
            } = &mut stmt.kind
                && let Some(shape) = fallible_shape(ty, true)
            {
                rewrite_value(init, shape);
            }
            walk_stmt_mut(self, stmt);
        }

        fn visit_expr(&mut self, expr: &mut Expr) {
            if let ExprKind::Closure { .. } = expr.kind {
                self.returns.push(None);
                walk_expr_mut(self, expr);
                self.returns.pop();
                return;
            }
            if let ExprKind::Return(Some(value)) = &mut expr.kind
                && let Some(Some(shape)) = self.returns.last()
            {
                rewrite_value(value, *shape);
            }
            if let ExprKind::MethodCall { name, generics, .. } = &mut expr.kind
                && name.name == "collect"
                && let [gossamer_ast::GenericArg::Type(ty)] = generics.as_slice()
                && let Some(shape) = fallible_shape(ty, false)
            {
                generics.clear();
                to_collector(expr, shape);
            }
            walk_expr_mut(self, expr);
        }
    }

    Rewriter {
        returns: Vec::new(),
    }
    .visit_source_file(sf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instantiate_replaces_only_whole_placeholder_words() {
        let out = instantiate(
            "pub fn take(self, n: i64) -> P::__GosTake<S, T> { \"S T\" }",
            "Stack<U>",
            "U",
            "crate",
        );
        assert_eq!(
            out,
            "pub fn take(self, n: i64) -> crate::__GosTake<Stack<U>, U> { \"S T\" }"
        );
    }

    #[test]
    fn matching_angle_skips_arrows_inside_bounds() {
        let text = "<I: Iterator<Item = A>, F: Fn(A) -> B, A, B> Iterator for X";
        let end = matching_angle(text);
        assert_eq!(
            end.map(|n| &text[..n]),
            Some("<I: Iterator<Item = A>, F: Fn(A) -> B, A, B>")
        );
    }
}
