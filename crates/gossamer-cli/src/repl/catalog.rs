//! The `%info` catalog: stdlib items, core methods, operators, and built-in traits.

use super::{
    BTreeMap, BUILTIN_MACROS, BuiltinMacro, CORE_METHODS, CORE_OPERATORS, CORE_TYPES,
    CoreMethodEntry, CoreOperatorHelp, CoreTypeHelp, HashSet, PRELUDE_BUILTINS, PreludeBuiltinHelp,
    Regex, StdItem, StdItemKind, StdModule, render_repl_declarations, repl_expr_mutates_binding,
    signature_takes_receiver,
};

pub(super) fn repl_info_matches(arg: &str, details: bool) -> std::result::Result<String, String> {
    let normalized = normalize_query(arg);
    if matches!(normalized, "std" | "std::") {
        return Ok(render_stdlib_dir());
    }
    if arg.is_empty() {
        return Ok(render_module_matches(
            gossamer_std::registry::modules(),
            details,
        ));
    }
    let namespace_query = info_search_query(arg);
    if matching_modules(&namespace_query).is_empty()
        && let Some(namespace) = canonical_stdlib_namespace(normalized)
    {
        let children = stdlib_namespace_children(&namespace);
        return Ok(render_stdlib_namespace_dir(&namespace, &children));
    }
    if let Some(pattern) = regex_argument(arg)? {
        let matches = render_catalog_matches(&pattern, details);
        return Ok(if matches == "no catalog matches" {
            format!("nothing found for `{arg}`")
        } else {
            matches
        });
    }

    let query = info_search_query(arg);
    let matches = render_catalog_query_matches(&query, details);
    if !matches.is_empty() {
        return Ok(matches);
    }
    // A module item is named through its module, except a trait: `Display`
    // and `Handler` are written bare in the `impl` header and the bound that
    // reach for them, so that is the spelling `%i` has to answer.
    if let Some(path) = stdlib_trait_path(arg) {
        let matches = render_catalog_query_matches(&info_search_query(&path), details);
        if !matches.is_empty() {
            return Ok(matches);
        }
    }
    Ok(format!("nothing found for `{arg}`"))
}

/// The canonical path of a stdlib trait named bare, or `None` when `name`
/// does not name one.
pub(super) fn stdlib_trait_path(name: &str) -> Option<String> {
    gossamer_std::registry::modules().iter().find_map(|module| {
        module
            .items
            .iter()
            .find(|item| {
                item.name == name && matches!(item.kind, gossamer_std::registry::StdItemKind::Trait)
            })
            .map(|item| format!("{}::{}", module.path, item.name))
    })
}

pub(super) fn stdlib_namespace_children(namespace: &str) -> Vec<StdModule> {
    let prefix = format!("{namespace}::");
    gossamer_std::registry::modules()
        .iter()
        .copied()
        .filter(|module| {
            module
                .path
                .strip_prefix(&prefix)
                .is_some_and(|path| !path.contains("::"))
        })
        .collect()
}

pub(super) fn canonical_stdlib_namespace(query: &str) -> Option<String> {
    let canonical = if query.starts_with("std::") {
        query.to_string()
    } else {
        format!("std::{query}")
    };
    (!stdlib_namespace_children(&canonical).is_empty()).then_some(canonical)
}

pub(super) fn render_repl_history(
    transcript: &[String],
    arg: &str,
) -> std::result::Result<Vec<String>, String> {
    let pattern = if arg.is_empty() {
        None
    } else {
        Some(compile_search_regex("history", arg)?)
    };
    Ok(transcript
        .iter()
        .filter(|entry| pattern.as_ref().is_none_or(|regex| regex.is_match(entry)))
        .cloned()
        .collect())
}

pub(super) fn compile_search_regex(
    command: &str,
    query: &str,
) -> std::result::Result<Regex, String> {
    Regex::new(query).map_err(|error| format!("invalid {command} regex `{query}`: {error}"))
}

pub(super) struct ListingOptions {
    pub(super) pattern: String,
    pub(super) details: bool,
}

pub(super) fn parse_listing_options(
    command: &str,
    arg: &str,
) -> std::result::Result<ListingOptions, String> {
    let mut details = false;
    let mut pattern = Vec::new();
    for word in arg.split_whitespace() {
        match word {
            "-d" | "--details" => details = true,
            "-a" | "--all" | "-p" | "--page" => {
                return Err(format!(
                    "%{command}: pagination options were removed; use a pattern to filter results"
                ));
            }
            _ => pattern.push(word),
        }
    }
    Ok(ListingOptions {
        pattern: pattern.join(" "),
        details,
    })
}

pub(super) fn render_info(text: String, options: &ListingOptions) -> String {
    if options.details {
        text
    } else {
        text.split("\n\n")
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

pub(super) fn regex_argument(arg: &str) -> std::result::Result<Option<Regex>, String> {
    if !(arg.starts_with('/') && arg.ends_with('/') && arg.len() >= 2) {
        return Ok(None);
    }
    Regex::new(&arg[1..arg.len() - 1])
        .map(Some)
        .map_err(|e| format!("invalid regex `{arg}`: {e}"))
}

pub(super) fn render_catalog_matches(pattern: &Regex, details: bool) -> String {
    let mut entries = Vec::new();
    for builtin in BUILTIN_MACROS {
        if pattern.is_match(builtin.name)
            || pattern.is_match(builtin.signature)
            || pattern.is_match(builtin.doc)
        {
            let mut entry = String::new();
            push_catalog_match(
                &mut entry,
                builtin.name,
                "builtin",
                builtin.signature,
                builtin.doc,
                None,
                details,
            );
            entries.push(entry);
        }
    }
    for builtin in PRELUDE_BUILTINS {
        if pattern.is_match(builtin.name)
            || pattern.is_match(builtin.signature)
            || pattern.is_match(builtin.doc)
        {
            let mut entry = String::new();
            push_catalog_match(
                &mut entry,
                builtin.name,
                "builtin",
                builtin.signature,
                builtin.doc,
                None,
                details,
            );
            entries.push(entry);
        }
    }
    for owner in all_core_namespaces() {
        if pattern.is_match(&owner) {
            let mut entry = String::new();
            push_catalog_match(
                &mut entry,
                &owner,
                "type",
                "",
                core_namespace_description(&owner),
                Some("Builtin"),
                details,
            );
            entries.push(entry);
        }
    }
    for method in core_method_entries() {
        let path = format!("{}::{}", method.owner, method.name);
        if pattern.is_match(&path)
            || pattern.is_match(&method.signature)
            || pattern.is_match(&method.doc)
        {
            let mut entry = String::new();
            push_core_method_match(&mut entry, &method, details);
            entries.push(entry);
        }
    }
    for module in gossamer_std::registry::modules() {
        if module_matches_regex(pattern, module) {
            let mut entry = String::new();
            push_module_match(&mut entry, module, details);
            entries.push(entry);
        }
        for item in module.items {
            if item_matches_regex(pattern, module, item) {
                let mut entry = String::new();
                push_item_match(&mut entry, module, item, details);
                entries.push(entry);
            }
        }
    }
    render_catalog_entries(entries, "no catalog matches")
}

/// Appends every cataloged method filed under `owner`. Naming a type is a
/// request for the surface it owns, and its methods carry the type's own
/// spelling, so a bare-name query never reaches them on its own.
pub(super) fn push_owned_method_entries(entries: &mut Vec<String>, owner: &str, details: bool) {
    for method in core_method_entries()
        .into_iter()
        .filter(|method| method.owner == owner)
    {
        let mut entry = String::new();
        push_core_method_match(&mut entry, &method, details);
        entries.push(entry);
    }
}

pub(super) fn render_catalog_query_matches(query: &str, details: bool) -> String {
    let mut entries = Vec::new();
    for operator in matching_core_operators(query) {
        let mut entry = String::new();
        push_operator_match(&mut entry, operator, details);
        entries.push(entry);
    }
    for builtin in matching_builtin_macros(query) {
        let mut entry = String::new();
        push_catalog_match(
            &mut entry,
            builtin.name,
            "builtin",
            builtin.signature,
            builtin.doc,
            None,
            details,
        );
        entries.push(entry);
    }
    for builtin in matching_prelude_builtins(query) {
        let mut entry = String::new();
        push_catalog_match(
            &mut entry,
            builtin.name,
            "builtin",
            builtin.signature,
            builtin.doc,
            None,
            details,
        );
        entries.push(entry);
    }
    for entry in matching_builtin_traits(query) {
        let mut rendered = String::new();
        push_builtin_trait_match(&mut rendered, entry, details);
        entries.push(rendered);
    }
    for core_type in matching_core_types(query) {
        let mut entry = String::new();
        push_catalog_match(
            &mut entry,
            core_type.name,
            "type",
            core_type.signature,
            core_type.doc,
            Some("Builtin"),
            details,
        );
        entries.push(entry);
        push_owned_method_entries(&mut entries, core_type.name, details);
    }
    for owner in matching_core_namespaces(query) {
        let mut entry = String::new();
        push_catalog_match(
            &mut entry,
            &owner,
            "type",
            "",
            core_namespace_description(&owner),
            Some("Builtin"),
            details,
        );
        entries.push(entry);
        push_owned_method_entries(&mut entries, &owner, details);
    }
    for method in matching_core_methods(query) {
        let mut entry = String::new();
        push_core_method_match(&mut entry, &method, details);
        entries.push(entry);
    }
    for module in matching_modules(query) {
        let mut entry = String::new();
        push_module_match(&mut entry, &module, details);
        entries.push(entry);
        // A matching module name is a namespace query, so include its public
        // contents and the modules nested under it. A qualified item query
        // does not enter this branch and remains focused on the requested
        // symbol.
        for item in module.items {
            let mut entry = String::new();
            push_item_match(&mut entry, &module, item, details);
            entries.push(entry);
        }
        for child in stdlib_namespace_children(module.path) {
            let mut entry = String::new();
            push_module_match(&mut entry, &child, details);
            entries.push(entry);
        }
    }
    for (module, item) in matching_items(query) {
        let mut entry = String::new();
        push_item_match(&mut entry, &module, &item, details);
        entries.push(entry);
        // Naming a type through its module is the same request as naming it
        // bare: the surface it owns. Without this a qualified
        // `%i bytes::Buffer` answered with the type alone, reading as a type
        // with nothing callable on it.
        if matches!(item.kind, gossamer_std::registry::StdItemKind::Type) {
            push_owned_method_entries(&mut entries, item.name, details);
        }
    }
    render_catalog_entries(entries, "")
}

pub(super) fn render_module_matches(modules: &[StdModule], details: bool) -> String {
    let mut entries = Vec::new();
    for module in modules {
        let mut entry = String::new();
        push_module_match(&mut entry, module, details);
        entries.push(entry);
    }
    render_catalog_entries(entries, "")
}

pub(super) fn push_catalog_match(
    out: &mut String,
    path: &str,
    kind: &str,
    signature: &str,
    description: &str,
    defined_in: Option<&str>,
    details: bool,
) {
    out.push_str(path);
    if !signature.is_empty() {
        if let Some(suffix) = signature.strip_prefix(path) {
            out.push_str(suffix.trim_start());
        } else {
            out.push_str(signature);
        }
    }
    out.push_str(&format!(" [{}]\n", catalog_kind_label(kind)));
    // A type's line is otherwise just its name and tag, which answers
    // nothing about what the type is; a method's name and signature
    // already do, and a listing can match dozens of them.
    if !details && matches!(kind, "type" | "trait") && !description.is_empty() {
        out.push_str(&format!("    {description}\n"));
    }
    if details {
        out.push_str(&format!("    {description}\n"));
        let defined_in = defined_in
            .filter(|location| !location.is_empty())
            .unwrap_or("Builtin");
        push_catalog_origin(out, defined_in);
        out.push_str(&format!(
            "    Example: {}\n",
            catalog_example(path, kind, signature)
        ));
    }
}

pub(super) fn push_catalog_origin(out: &mut String, defined_in: &str) {
    if defined_in == "Builtin" {
        out.push_str("    Builtin\n");
    } else {
        out.push_str(&format!("    Defined in: {defined_in}\n"));
    }
}

pub(super) fn catalog_kind_label(kind: &str) -> &str {
    if kind == "assoc" {
        "associated function"
    } else {
        kind
    }
}

pub(super) fn catalog_example(path: &str, kind: &str, signature: &str) -> String {
    if let Some(core_type) = CORE_TYPES.iter().find(|entry| entry.name == path) {
        return core_type.example.to_string();
    }
    match path {
        "Map::from" | "HashMap::from" => {
            return "let empty: Map<String, i64> = Map::from([]); let map = {\"one\": 1, \"two\": 2}; let also = Map::from([(\"one\", 1), (\"two\", 2)])".to_string();
        }
        "BTreeMap::from" => {
            return "let map = BTreeMap::from([(\"one\", 1), (\"two\", 2)])".to_string();
        }
        "Set::from" | "HashSet::from" => {
            return "let set: Set<i64> = Set::from([1, 2, 2, 3])".to_string();
        }
        "BTreeSet::from" => {
            return "let set: BTreeSet<i64> = BTreeSet::from([1, 2, 2, 3])".to_string();
        }
        "Vec::from" => {
            return "let values = Vec::from([1, 2, 3])".to_string();
        }
        "Deque::from" | "VecDeque::from" => {
            return "let deque = Deque::from([1, 2, 3])".to_string();
        }
        "Queue::from" | "VecQueue::from" => {
            return "let queue = Queue::from([1, 2, 3])".to_string();
        }
        "Stack::from" | "VecStack::from" => {
            return "let stack = Stack::from([1, 2, 3])".to_string();
        }
        "BinaryHeap::from" | "MaxBinaryHeap::from" | "MaxHeap::from" => {
            return "let heap: MaxHeap<i64> = MaxHeap::from([1, 2, 3])".to_string();
        }
        "MinBinaryHeap::from" | "MinHeap::from" => {
            return "let heap: MinHeap<i64> = MinHeap::from([1, 2, 3])".to_string();
        }
        _ => {}
    }

    match kind {
        "module" => return format!("use {path}"),
        "type" => return format!("fn use_value(value: {path}) {{ }}"),
        "trait" => return format!("fn use_value<T: {path}>(value: T) {{ }}"),
        "const" => return format!("let value = {path}"),
        _ => {}
    }

    let args = signature_example_arguments(signature);
    if kind == "method" {
        let (owner, name) = path.rsplit_once("::").unwrap_or(("", path));
        return format!("{}.{}({args})", example_receiver(owner), name);
    }
    format!("{path}({args})")
}

pub(super) fn example_receiver(owner: &str) -> &'static str {
    match owner.rsplit("::").next().unwrap_or(owner) {
        "String" | "str" => "\"text\"",
        "Vec" | "Slice" | "Array" => "values",
        "Map" | "BTreeMap" => "map",
        "Set" | "BTreeSet" => "set",
        "Deque" => "deque",
        "Queue" => "queue",
        "Stack" => "stack",
        "MaxHeap" | "MinHeap" => "heap",
        "Simd" => "lanes",
        "Mask" => "mask",
        "Option" => "option",
        "Result" => "result",
        "Iterator" | "Range" => "iter",
        _ => "value",
    }
}

pub(super) fn core_namespace_description(owner: &str) -> &'static str {
    if owner == "sync::Map" {
        return "Concurrent string-to-string map shared across goroutines.";
    }
    match owner.rsplit("::").next().unwrap_or(owner) {
        "Array" => {
            "Fixed-size contiguous sequence, written `[1, 2, 3]` or `[0; 8]`. Owns its \
             elements; length is part of the type, so it cannot grow."
        }
        "Slice" => {
            "Borrowed contiguous view `&[T]` / `&mut [T]` over an array or Vec. Shares the \
             slice method surface; cannot resize."
        }
        "Vec" => {
            "Growable contiguous sequence, written `#[1, 2, 3]` or `#[0; 8]`. The default \
             owned sequence, and the only one with insert, remove, truncate, and capacity."
        }
        "Map" => {
            "Key-value map, written `{\"one\": 1}`. Any hashable value keys it - integers, \
             bool, char, String, tuples, arrays, structs, enums - and keys compare by value."
        }
        "Set" => {
            "Unique-value set, written `#{1, 2, 3}`, with full set algebra (union, \
             intersection, difference)."
        }
        "BTreeMap" => {
            "Ordered key-value map with String or i64 keys. A distinct type from `Map`: \
             neither converts to the other."
        }
        "BTreeSet" => {
            "Ordered unique-value set. Written `#{..}` where a `BTreeSet<T>` is expected."
        }
        "Deque" => "Double-ended queue: push and pop at either end. Build with `Deque::new()`.",
        "Queue" => {
            "FIFO-only queue. The idiomatic choice over `Vec` or `Deque` when the contract \
             is first-in-first-out. Build with `Queue::new()`."
        }
        "Stack" => {
            "LIFO-only stack. The idiomatic choice over `Vec` when the contract is \
             last-in-first-out. Build with `Stack::new()`."
        }
        "MaxHeap" => {
            "Max-priority heap: largest element first, without negating keys. Build with \
             `MaxHeap::new()`."
        }
        "MinHeap" => {
            "Min-priority heap: smallest element first, without wrapping values. Build with \
             `MinHeap::new()`."
        }
        "Iterator" => {
            "Lazy sequence cursor from `.iter()`. Adapters stay lazy; terminals end the \
             chain. Single-use - bind a fresh `.iter()` per pipeline."
        }
        "Range" => {
            "Bounded integer sequence `a..b` / `a..=b`, already an iterator, so \
             `(1..5).map(..)` reads straight through. Can be stored and consumed later."
        }
        "Option" => {
            "Optional value: `Some(v)` or `None`. `?` propagates it inside an Option-returning fn."
        }
        "Result" => {
            "Success or error value: `Ok(v)` or `Err(e)`. The fallibility type; `?` \
             propagates and converts errors through `From`."
        }
        "String" => {
            "UTF-8 text. Two index spaces: `len`, `s[i]`, and bare iteration count Unicode \
             scalars (so `s[i]` is a `char`); `byte_len`, `byte_at`, `as_bytes`, `bytes`, \
             and `substring` count UTF-8 bytes. Literals are already `String`."
        }
        "Simd" => {
            "Fixed-width lane vector `Simd<T, N>`: N lanes of `f32`, `f64`, or `i64` (2, 4, \
             or 8 lanes), or of `u8`, `i32`, or `u32` (2, 4, 8, or 16). `+`, `-`, and `*` \
             work lane by lane, `/` on float lanes, and `+%`, `-%`, `*%`, `<<`, `>>`, `&`, \
             `|`, and `^` on integer lanes, with the same bits on every tier. Build one with \
             `Simd::splat`, `Simd::from_array`, or `Simd::load`; a function may take one \
             over a const generic lane count."
        }
        "Mask" => {
            "Lane vector of `bool`, `Mask<N>`, answered by `lanes_eq`, `lanes_lt`, and \
             `lanes_le`. `select` picks each lane from one of two vectors by it; `&`, `|`, \
             and `^` combine two masks lane by lane."
        }
        "Buffer" => "Growable byte buffer for binary assembly.",
        "Tuple" => {
            "Fixed-length group of values whose element types may differ. Read positionally \
             (`t.0`), destructured, and compared in declaration order. Not iterable."
        }
        "Builder" => {
            "Incremental string builder. Preferred over repeated `+` on String, which \
             copies on every append."
        }
        _ => "Built-in type and method namespace.",
    }
}

/// The `(name, type)` pairs a signature declares, its receiver excluded.
pub(super) fn signature_parameters(signature: &str) -> Vec<(&str, &str)> {
    let Some(open) = signature.find('(') else {
        return Vec::new();
    };
    let mut depth = 0usize;
    let mut close = None;
    for (offset, ch) in signature[open + 1..].char_indices() {
        match ch {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' if depth == 0 => {
                close = Some(open + 1 + offset);
                break;
            }
            ')' | ']' | '}' | '>' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    let Some(close) = close else {
        return Vec::new();
    };
    split_top_level_parameters(&signature[open + 1..close])
        .into_iter()
        .filter_map(|parameter| {
            let parameter = parameter.trim();
            let (name, ty) = parameter
                .split_once(':')
                .map_or((parameter, ""), |(name, ty)| (name, ty.trim()));
            let name = name.trim().trim_end_matches('?');
            (!matches!(name, "self" | "&self" | "&mut self") && !name.is_empty())
                .then_some((name, ty))
        })
        .collect()
}

/// The argument list of a call a reader could paste: one literal or closure
/// per parameter, shaped by the parameter's type and name.
pub(super) fn signature_example_arguments(signature: &str) -> String {
    signature_parameters(signature)
        .into_iter()
        .map(|(name, ty)| example_argument(name, ty))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A closure whose shape follows a callback type: its arity, and what it
/// answers.
pub(super) fn example_closure(ty: &str) -> String {
    let open = ty.find('(').map_or(0, |i| i + 1);
    let mut depth = 0usize;
    let mut close = ty.len();
    for (offset, ch) in ty[open..].char_indices() {
        match ch {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' if depth == 0 => {
                close = open + offset;
                break;
            }
            ')' | ']' | '}' | '>' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    let params: Vec<&str> = split_top_level_parameters(&ty[open..close])
        .into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let ret = ty[close..]
        .split_once("->")
        .map_or("", |(_, ret)| ret.trim());
    match params.as_slice() {
        [] => "|| 0".to_string(),
        [single] => {
            let (binder, value) = if single.starts_with('(') {
                ("(key, value)", "value")
            } else {
                ("value", "value")
            };
            let body = if ret == "bool" {
                format!("{value} > 0")
            } else if ret == "()" {
                format!("println(\"{{{value}}}\")")
            } else if ret.starts_with("Option<") {
                format!("Some({value})")
            } else if ret.starts_with("Vec<") {
                format!("#[{value}, {value}]")
            } else if ret == "String" {
                format!("format(\"{{{value}}}\")")
            } else {
                value.to_string()
            };
            format!("|{binder}| {body}")
        }
        [_, _] if ret == "bool" => "|left, right| left < right".to_string(),
        [_, _] if ret == "i64" => "|left, right| left - right".to_string(),
        _ => "|acc, value| acc + value".to_string(),
    }
}

/// A sample value for one parameter, chosen from its type and then its name.
pub(super) fn example_argument(name: &str, ty: &str) -> String {
    let ty = ty.split(" | ").next().unwrap_or(ty).trim();
    if ty.starts_with("Fn(") || ty.starts_with("fn(") {
        return example_closure(ty);
    }
    let is_generic = ty.len() == 1 && ty.starts_with(|c: char| c.is_ascii_uppercase());
    match ty {
        "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize" => {
            match name {
                "n" | "count" | "size" | "step" | "width" | "len" | "capacity" | "by" => "2",
                "index" | "i" | "at" | "position" | "start" | "from" => "0",
                _ => "1",
            }
            .to_string()
        }
        "f32" | "f64" => "1.5".to_string(),
        "bool" => "true".to_string(),
        "char" => "'a'".to_string(),
        "String" => match name {
            "sep" | "separator" | "delimiter" => "\", \"",
            "needle" | "prefix" | "suffix" | "pattern" | "pat" => "\"te\"",
            _ => "\"text\"",
        }
        .to_string(),
        _ if ty.starts_with("Vec<") || ty.starts_with('[') => "#[1, 2, 3]".to_string(),
        _ if ty.starts_with("Map<") => "{\"one\": 1}".to_string(),
        _ if ty.starts_with("Set<") => "#{1, 2, 3}".to_string(),
        _ if ty.starts_with("Option<") => "Some(1)".to_string(),
        _ if ty.starts_with('(') => "(1, 2)".to_string(),
        _ if is_generic => match name {
            "init" | "initial" | "default" | "acc" => "0",
            _ => "1",
        }
        .to_string(),
        _ => name.to_string(),
    }
}

pub(super) fn split_top_level_parameters(parameters: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (offset, ch) in parameters.char_indices() {
        match ch {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&parameters[start..offset]);
                start = offset + ch.len_utf8();
            }
            _ => {}
        }
    }
    if start < parameters.len() {
        parts.push(&parameters[start..]);
    }
    parts
}

pub(super) fn signature_suffix<'a>(signature: &'a str, name: &str) -> &'a str {
    signature
        .strip_prefix(&format!("fn {name}"))
        .or_else(|| signature.strip_prefix(name))
        .unwrap_or(signature)
        .trim_start()
}

pub(super) fn push_module_match(out: &mut String, module: &StdModule, details: bool) {
    push_catalog_match(
        out,
        module.path,
        "module",
        "",
        module.summary,
        Some(module.path),
        details,
    );
}

pub(super) fn push_item_match(out: &mut String, module: &StdModule, item: &StdItem, details: bool) {
    // A trait's surface is the method an `impl` supplies, which is what a
    // reader looking one up needs, exactly as a type's listing names its
    // methods.
    let signature = if matches!(item.kind, StdItemKind::Trait) {
        trait_required_signature(item.name).unwrap_or_default()
    } else {
        gossamer_types::stdlib_function_signature(module.path, item.name)
            .map(|signature| signature_suffix(signature, item.name).to_string())
            .unwrap_or_default()
    };
    push_catalog_match(
        out,
        &format!("{}::{}", module.path, item.call_name()),
        item_kind_label(item.kind),
        &signature,
        item.doc,
        Some(module.path),
        details,
    );
}

/// One built-in trait's listing. An implementable trait leads with the block
/// an `impl` writes; a trait the language supplies has no such block, so its
/// line carries the name alone and the detail says what to write instead.
pub(super) fn push_builtin_trait_match(
    out: &mut String,
    entry: &gossamer_types::BuiltinTrait,
    details: bool,
) {
    let signature = if entry.signature.is_empty() {
        String::new()
    } else {
        format!(" {}", entry.signature)
    };
    let description = if entry.instead.is_empty() {
        entry.doc.to_string()
    } else {
        format!("{} {}", entry.doc, entry.instead)
    };
    out.push_str(entry.name);
    out.push_str(&signature);
    out.push_str(" [trait]\n");
    if !details {
        out.push_str(&format!("    {description}\n"));
        return;
    }
    out.push_str(&format!("    {description}\n"));
    push_catalog_origin(out, "Builtin");
    out.push_str(&format!("    Example: {}\n", entry.example));
}

pub(super) fn push_core_method_match(out: &mut String, method: &CoreMethodEntry, details: bool) {
    let signature = signature_suffix(&method.signature, &method.name);
    push_catalog_match(
        out,
        &format!("{}::{}", method.owner, method.name),
        method.kind,
        signature,
        &method.doc,
        Some("Builtin"),
        details,
    );
}

pub(super) fn render_catalog_entries(mut entries: Vec<String>, empty: &str) -> String {
    if entries.is_empty() {
        return empty.to_string();
    }
    entries.sort_unstable();
    entries.dedup();
    entries.join("\n").trim_end().to_string()
}

pub(super) fn render_stdlib_dir() -> String {
    let mut entries = Vec::new();
    for namespace in stdlib_namespaces() {
        let mut entry = String::new();
        push_catalog_entry(
            &mut entry,
            &namespace,
            "module",
            "Standard-library namespace.",
        );
        entries.push(entry);
    }
    for module in gossamer_std::registry::modules() {
        let mut entry = String::new();
        push_catalog_entry(&mut entry, module.path, "module", module.summary);
        entries.push(entry);
    }
    entries.sort_unstable();
    entries.concat().trim_end().to_string()
}

pub(super) fn stdlib_namespaces() -> Vec<String> {
    let modules = gossamer_std::registry::modules();
    let mut namespaces = modules
        .iter()
        .filter_map(|module| module.path.rsplit_once("::").map(|(parent, _)| parent))
        .filter(|parent| *parent != "std")
        .filter(|parent| !modules.iter().any(|module| module.path == *parent))
        .map(str::to_string)
        .collect::<Vec<_>>();
    namespaces.sort_unstable();
    namespaces.dedup();
    namespaces
}

pub(super) fn render_stdlib_namespace_dir(namespace: &str, modules: &[StdModule]) -> String {
    let mut entries = Vec::with_capacity(modules.len() + 1);
    let mut namespace_entry = String::new();
    push_catalog_entry(
        &mut namespace_entry,
        namespace,
        "module",
        "Standard-library namespace.",
    );
    entries.push(namespace_entry);
    for module in modules {
        let mut entry = String::new();
        push_catalog_entry(&mut entry, module.path, "module", module.summary);
        entries.push(entry);
    }
    entries.sort_unstable();
    entries.concat().trim_end().to_string()
}

pub(super) fn push_catalog_entry(out: &mut String, path: &str, kind: &str, description: &str) {
    out.push_str(&format!(
        "{path} [{kind}]\n  {description}\n  Example: {}\n\n",
        catalog_example(path, kind, "")
    ));
}

/// The core method catalog, derived once. Its inputs - the static table,
/// the stdlib manifest, and the interpreter's registered builtins - are
/// fixed for the life of the process.
pub(super) fn core_method_entries() -> Vec<CoreMethodEntry> {
    static CATALOG: std::sync::OnceLock<Vec<CoreMethodEntry>> = std::sync::OnceLock::new();
    CATALOG.get_or_init(build_core_method_entries).clone()
}

pub(super) fn build_core_method_entries() -> Vec<CoreMethodEntry> {
    let mut entries = BTreeMap::<(String, String), CoreMethodEntry>::new();
    for method in CORE_METHODS {
        insert_core_method_entry(
            &mut entries,
            CoreMethodEntry {
                owner: method.owner.to_string(),
                name: method.name.to_string(),
                kind: method.kind,
                signature: method.signature.to_string(),
                doc: method.doc.to_string(),
            },
        );
    }
    add_data_last_std_methods(&mut entries, "Option", "std::option");
    add_data_last_std_methods(&mut entries, "Result", "std::result");
    add_data_last_std_methods(&mut entries, "Vec", "std::iter");
    add_data_last_std_methods(&mut entries, "Iterator", "std::iter");
    // A range answers exactly the iterator surface, so `%info Range` lists
    // the same methods rather than reporting an empty namespace.
    add_data_last_std_methods(&mut entries, "Range", "std::iter");
    for registered in gossamer_interp::registered_names() {
        if let Some((owner, name)) = registered_core_method_path(registered) {
            // A runtime registration is not evidence the checker accepts the
            // call: the name is global, so every receiver's builtin lands in
            // one table. Discovery follows what the checker resolves.
            if !gossamer_types::core_type_accepts_method(&owner, &name) {
                continue;
            }
            let named_kind = if runtime_assoc_name(&name) {
                "assoc"
            } else {
                "method"
            };
            // A runtime registration is authoritative evidence that the
            // option exists. Keep it visible even while its richer checker
            // signature metadata is being filled in. An empty signature is
            // truthful and renders as a method name, unlike the old `...`
            // placeholder that pretended to know an argument contract.
            let signature =
                runtime_core_method_signature(&owner, &name, named_kind).unwrap_or_default();
            // A stated contract is the authority on whether the call takes a
            // receiver, so a constructor is listed under its type rather than
            // offered on a value of it.
            let kind = match signature.split_once('(') {
                Some((_, params)) => {
                    if signature_takes_receiver(params) {
                        "method"
                    } else {
                        "assoc"
                    }
                }
                None => named_kind,
            };
            let doc = runtime_core_method_doc(&owner, &name)
                .map_or_else(|| format!("Built-in {kind} on {owner}."), str::to_string);
            insert_core_method_entry(
                &mut entries,
                CoreMethodEntry {
                    owner: owner.clone(),
                    name: name.clone(),
                    kind,
                    signature,
                    doc,
                },
            );
        }
    }
    // Arrays and slices inherit only the canonical slice surface, not every
    // non-resizing Vec convenience or eager iterator combinator.
    let shared_sequence_methods: Vec<CoreMethodEntry> = entries
        .values()
        .filter(|method| {
            method.owner == "Vec"
                && method.kind == "method"
                && gossamer_types::is_slice_sequence_method(&method.name)
        })
        .cloned()
        .collect();
    for method in shared_sequence_methods {
        for owner in ["Array", "Slice"] {
            let mut derived = method.clone();
            derived.owner = owner.to_string();
            derived.signature = sequence_owner_signature(&derived.signature, owner);
            insert_core_method_entry(&mut entries, derived);
        }
    }
    // `to_vec` copies a borrowed or fixed-length sequence into an owned
    // one, so it belongs to these two owners rather than being inherited
    // from `Vec`, which is already the owned form.
    for owner in ["Array", "Slice"] {
        let receiver = if owner == "Array" { "&[T; N]" } else { "&[T]" };
        insert_core_method_entry(
            &mut entries,
            CoreMethodEntry {
                owner: owner.to_string(),
                name: "to_vec".to_string(),
                kind: "method",
                signature: format!("fn to_vec<T>(self: {receiver}) -> Vec<T>"),
                doc: "Copies the elements into an owned vector.".to_string(),
            },
        );
    }
    insert_core_method_entry(
        &mut entries,
        CoreMethodEntry {
            owner: "Array".to_string(),
            name: "clone".to_string(),
            kind: "method",
            signature: "fn clone<T, const N: i64>(self: [T; N]) -> [T; N]".to_string(),
            doc: "Returns a fixed-size copy of the array.".to_string(),
        },
    );
    fill_collection_sequence_signatures(&mut entries);
    catalog_under_receiver_types(&mut entries);
    entries.into_values().collect()
}

/// Also files a method under the type its receiver names. One runtime
/// namespace can serve several types - `Channel` carries `send`, `recv`,
/// and `join`, whose receivers are a `Sender`, a `Receiver`, and a
/// `JoinHandle` - and a binding of one of those types reaches its methods
/// through its own name.
pub(super) fn catalog_under_receiver_types(
    entries: &mut BTreeMap<(String, String), CoreMethodEntry>,
) {
    let rehomed: Vec<CoreMethodEntry> = entries
        .values()
        .filter(|entry| entry.kind == "method")
        .filter_map(|entry| {
            let owner = receiver_type_name(&entry.signature)?;
            // The receiver is written as the checker resolves it, module path
            // and all: `flag::Set` is a type of its own, not the collection
            // the last segment also names.
            (short_type_name(owner) != short_type_name(&entry.owner)
                && gossamer_types::core_type_accepts_method(owner, &entry.name))
            .then(|| CoreMethodEntry {
                owner: owner.to_string(),
                ..entry.clone()
            })
        })
        .collect();
    for entry in rehomed {
        insert_core_method_entry(entries, entry);
    }
}

/// The type a signature's `self` parameter is written as, module path and
/// all, or `None` when the signature states no receiver.
pub(super) fn receiver_type_name(signature: &str) -> Option<&str> {
    let params = signature.split_once('(')?.1;
    if !signature_takes_receiver(params) {
        return None;
    }
    let ty = params.split_once("self: ")?.1;
    let ty = ty.trim_start_matches("&mut ").trim_start_matches('&');
    // A sequence receiver is written as a bracketed shape rather than a
    // named type, and belongs to the owner the entry already carries.
    let name = ty.split(['<', ',', ')', ' ', '[']).next().unwrap_or(ty);
    (!name.is_empty()).then_some(name)
}

/// A type name without its module path: `time::Duration` is `Duration`.
pub(super) fn short_type_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Names a map's elements are pairs of, so the `Vec` sequence surface it
/// inherits reads over `(K, V)` rather than over a single element type.
pub(super) const MAP_SEQUENCE_OWNERS: &[&str] = &["Map", "BTreeMap"];

/// Names a set's elements are single values of, so the inherited surface
/// keeps `Vec`'s element type and only its receiver changes.
pub(super) const SET_SEQUENCE_OWNERS: &[&str] = &["Set", "BTreeSet"];

/// Fills the signature of every map/set sequence method from the `Vec`
/// entry of the same name. The traversal surface is the same one `Vec`
/// carries - only the receiver and the element type differ - so deriving
/// it keeps one table authoritative instead of a second copy that drifts.
pub(super) fn fill_collection_sequence_signatures(
    entries: &mut BTreeMap<(String, String), CoreMethodEntry>,
) {
    let vec_signatures: BTreeMap<String, String> = entries
        .values()
        .filter(|method| method.owner == "Vec" && method.kind == "method")
        .map(|method| (method.name.clone(), method.signature.clone()))
        .collect();
    let owners: Vec<(String, String)> = entries
        .keys()
        .filter(|(owner, _)| {
            MAP_SEQUENCE_OWNERS.contains(&owner.as_str())
                || SET_SEQUENCE_OWNERS.contains(&owner.as_str())
        })
        .cloned()
        .collect();
    for key in owners {
        let (owner, name) = (key.0.clone(), key.1.clone());
        let is_map = MAP_SEQUENCE_OWNERS.contains(&owner.as_str());
        let Some(entry) = entries.get_mut(&key) else {
            continue;
        };
        if !entry.signature.is_empty() {
            continue;
        }
        let Some(vec_signature) = vec_signatures.get(&name) else {
            continue;
        };
        entry.signature = if is_map {
            map_sequence_signature(vec_signature, &owner)
        } else {
            set_sequence_signature(vec_signature, &owner)
        };
    }
}

/// The `Vec` signature rewritten for a set receiver: same element type,
/// same results, a set in the receiver's place.
pub(super) fn set_sequence_signature(signature: &str, owner: &str) -> String {
    signature
        .replace("self: &mut Vec<", &format!("self: &mut {owner}<"))
        .replace("self: Vec<", &format!("self: {owner}<"))
}

/// The `Vec` signature rewritten for a map receiver: the element type `T`
/// becomes the `(K, V)` pair, and `Vec`'s own key generic moves out of the
/// way of the map's `K`.
pub(super) fn map_sequence_signature(signature: &str, owner: &str) -> String {
    // `Vec`'s own key generic moves aside first so the map's `K` is free.
    let renamed = replace_generic(signature, "K", "B");
    let Some(params_start) = renamed.find('(') else {
        return renamed;
    };
    let (head, body) = renamed.split_at(params_start);
    // The declaration list names the two parameters a map is written with;
    // every use of the element type is the pair they form.
    let head = replace_generic(head, "T", "K, V");
    let body = replace_generic(body, "T", "(K, V)")
        .replace(
            "self: &mut Vec<(K, V)>",
            &format!("self: &mut {owner}<K, V>"),
        )
        .replace("self: Vec<(K, V)>", &format!("self: {owner}<K, V>"));
    format!("{head}{body}")
}

/// Replaces every whole-word occurrence of the type parameter `from`.
pub(super) fn replace_generic(signature: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(signature.len());
    let mut rest = signature;
    while let Some(idx) = rest.find(from) {
        let (head, tail) = rest.split_at(idx);
        let after = &tail[from.len()..];
        let before_ok = head
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let after_ok = after
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        out.push_str(head);
        if before_ok && after_ok {
            out.push_str(to);
        } else {
            out.push_str(from);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The same contract read over a fixed array or a slice: only the receiver
/// changes, and it keeps whatever element type the `Vec` form carries.
pub(super) fn sequence_owner_signature(signature: &str, owner: &str) -> String {
    const RECEIVER: &str = "self: ";
    let Some(start) = signature.find(RECEIVER) else {
        return signature.to_string();
    };
    let rest = &signature[start + RECEIVER.len()..];
    let (mutability, ty) = rest.strip_prefix("&mut ").map_or_else(
        || ("&", rest.strip_prefix('&').unwrap_or(rest)),
        |ty| ("&mut ", ty),
    );
    let Some((element, tail)) = vec_element_and_tail(ty) else {
        return signature.to_string();
    };
    let length = if owner == "Array" { "; N" } else { "" };
    format!(
        "{}{RECEIVER}{mutability}[{element}{length}]{tail}",
        &signature[..start]
    )
}

/// Splits `Vec<E>rest` into its element type and what follows it, keeping a
/// nested `Vec<Vec<T>>` element whole.
pub(super) fn vec_element_and_tail(ty: &str) -> Option<(&str, &str)> {
    let body = ty.strip_prefix("Vec<")?;
    let mut depth = 1usize;
    for (offset, ch) in body.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&body[..offset], &body[offset + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// Exposes a data-last standard module as receiver methods without maintaining
/// a second signature or documentation table. Constructors whose final
/// parameter is not the collection value are excluded automatically.
pub(super) fn add_data_last_std_methods(
    entries: &mut BTreeMap<(String, String), CoreMethodEntry>,
    owner: &str,
    module_path: &str,
) {
    let Some(module) = gossamer_std::registry::modules()
        .iter()
        .find(|module| module.path == module_path)
    else {
        return;
    };
    for item in module.items {
        if item.kind != StdItemKind::Function {
            continue;
        }
        if matches!(owner, "Iterator" | "Range")
            && !gossamer_types::iterator_receiver_accepts_method(item.name)
        {
            continue;
        }
        // Discovery follows what the checker resolves on the owner.
        if !matches!(owner, "Iterator" | "Range")
            && !gossamer_types::core_type_accepts_method(owner, item.name)
        {
            continue;
        }
        let Some(signature) = data_first_method_signature(owner, module_path, item.name) else {
            continue;
        };
        insert_core_method_entry(
            entries,
            CoreMethodEntry {
                owner: owner.to_string(),
                name: item.name.to_string(),
                kind: "method",
                signature,
                doc: if matches!(owner, "Iterator" | "Range") {
                    iterator_method_doc(item.name)
                        .unwrap_or(item.doc)
                        .to_string()
                } else {
                    item.doc.to_string()
                },
            },
        );
    }
}

/// The method spelling of a std free function: every one takes its data
/// first, so the leading parameter is the receiver and the rest follow it.
pub(super) fn data_first_method_signature(
    owner: &str,
    module_path: &str,
    name: &str,
) -> Option<String> {
    let row = gossamer_types::stdlib_function_signature(module_path, name)?;
    let shape = gossamer_types::stdlib_function_shape(module_path, name)?;
    let (receiver, leading) = shape.params.split_first()?;
    let receiver_matches = match owner {
        "Option" => receiver.ty.starts_with("Option<"),
        "Result" => receiver.ty.starts_with("Result<"),
        "Vec" => receiver.ty.starts_with("Vec<"),
        "Iterator" | "Range" => receiver.ty.starts_with("Vec<"),
        _ => false,
    };
    if !receiver_matches {
        return None;
    }
    let name_start = row.find(name)?;
    let open = row[name_start..].find('(')? + name_start;
    let generics = row.get(name_start + name.len()..open)?;
    if matches!(owner, "Iterator" | "Range") {
        if name == "chain" {
            return Some(
                "fn chain<T>(self: Iterator<T>, other: Iterator<T>) -> Iterator<T>".to_string(),
            );
        }
        if name == "zip" {
            return Some(
                "fn zip<A, B>(self: Iterator<A>, other: Iterator<B>) -> Iterator<(A, B)>"
                    .to_string(),
            );
        }
    }
    let receiver_ty = match owner {
        "Iterator" => receiver.ty.replacen("Vec<", "Iterator<", 1),
        // A range is written `0..n`, so its receiver reads as the range
        // itself rather than the element container.
        "Range" => "Range".to_string(),
        _ => receiver.ty.to_string(),
    };
    let mut params = vec![format!("self: {receiver_ty}")];
    params.extend(
        leading
            .iter()
            .map(|param| format!("{}: {}", param.name, param.ty)),
    );
    // A `Range` receiver answers the same iterator surface as an
    // `Iterator`, and the eager catalog shape these rows are rendered from
    // states the materialised return. A lazy adapter hands back another
    // iterator on both receivers, so the rendered return follows the
    // classification the type checker applies rather than a list kept here.
    let return_ty = if matches!(owner, "Iterator" | "Range")
        && gossamer_types::iterator_adapter_is_lazy(name)
    {
        shape.return_ty.replacen("Vec<", "Iterator<", 1)
    } else {
        shape.return_ty.to_string()
    };
    Some(format!(
        "fn {name}{generics}({}) -> {}",
        params.join(", "),
        return_ty
    ))
}

pub(super) fn iterator_method_doc(name: &str) -> Option<&'static str> {
    match name {
        "take" => Some("Returns a lazy iterator over at most the first n values."),
        "skip" => Some("Returns a lazy iterator that skips the first n values."),
        "step_by" => Some("Returns a lazy iterator yielding every step-th value."),
        "enumerate" => Some("Returns a lazy iterator of index and value pairs."),
        "chain" => Some("Returns a lazy iterator followed by another iterator."),
        "zip" => Some("Returns a lazy iterator pairing values from two iterators."),
        "map" => Some("Returns a lazy iterator that applies a function to each value."),
        "filter" => Some("Returns a lazy iterator containing values accepted by a predicate."),
        "rev" => Some("Returns a lazy iterator in reverse order."),
        _ => None,
    }
}

pub(super) fn runtime_core_method_signature(owner: &str, name: &str, kind: &str) -> Option<String> {
    // These runtime-backed handle constructors are registered by the
    // interpreter rather than the stdlib function catalog. Keep their public
    // contracts here so `%info` never fabricates an ellipsis signature.
    if let Some(signature) = match (owner, name) {
        // The cursor pull a `for` desugars to, and the one iterator method
        // that is not a `std::iter` free function.
        ("Iterator", "next") => Some("fn next<T>(self: &mut Iterator<T>) -> Option<T>"),
        ("AtomicBool", "new") => Some("fn new(value: bool) -> AtomicBool"),
        ("AtomicI32", "new") => Some("fn new(value: i32) -> AtomicI32"),
        ("AtomicI64", "new") => Some("fn new(value: i64) -> AtomicI64"),
        ("AtomicU64", "new") => Some("fn new(value: u64) -> AtomicU64"),
        ("Barrier", "new") => Some("fn new(parties: i64) -> Barrier"),
        ("Mutex", "new") => Some("fn new() -> Mutex"),
        ("RwLock", "new") => Some("fn new(value: i64) -> RwLock"),
        ("Once", "new") => Some("fn new() -> Once"),
        ("WaitGroup", "new") => Some("fn new() -> WaitGroup"),
        ("sync::Map", "new") => Some("fn new() -> sync::Map"),
        ("sync::Map", "insert") => {
            Some("fn insert(self: sync::Map, key: String, value: String) -> ()")
        }
        ("sync::Map", "get") => Some("fn get(self: sync::Map, key: String) -> Option<String>"),
        ("sync::Map", "remove") => Some("fn remove(self: sync::Map, key: String) -> ()"),
        ("sync::Map", "len") => Some("fn len(self: sync::Map) -> i64"),
        ("sync::Map", "contains_key") => {
            Some("fn contains_key(self: sync::Map, key: String) -> bool")
        }
        ("sync::Map", "keys") => Some("fn keys(self: sync::Map) -> Vec<String>"),
        ("Errors" | "validate::Errors", "new") => Some("fn new() -> validate::Errors"),
        ("FieldError" | "validate::FieldError", "new") => {
            Some("fn new(path: String, message: String, code: String) -> validate::FieldError")
        }
        ("http::Client", "new") => Some("fn new() -> http::Client"),
        ("http::Client", "builder") => Some("fn builder() -> http::ClientBuilder"),
        _ => None,
    } {
        return Some(signature.to_string());
    }
    if let Some(signature) = crate::repl_handles::handle_signature(owner, name) {
        return Some(signature.to_string());
    }
    if owner == "String" && kind == "method" {
        if let Some(shape) = gossamer_types::stdlib_function_shape("std::strings", name) {
            let mut params = vec!["self: String".to_string()];
            params.extend(
                shape
                    .params
                    .iter()
                    .skip(1)
                    .map(|param| format!("{}: {}", param.name, param.ty)),
            );
            return Some(format!(
                "fn {name}({}) -> {}",
                params.join(", "),
                shape.return_ty
            ));
        }
        if let Some(signature) = match name {
            "byte_at" => Some("fn byte_at(self: String, index: i64) -> i64"),
            "byte_len" => Some("fn byte_len(self: String) -> i64"),
            "substring" => Some("fn substring(self: String, start: i64, end: i64) -> String"),
            _ => None,
        } {
            return Some(signature.to_string());
        }
    }
    None
}

#[allow(
    clippy::too_many_lines,
    reason = "flat metadata table keeps REPL core-method docs auditable"
)]
pub(super) fn runtime_core_method_doc(owner: &str, name: &str) -> Option<&'static str> {
    if owner == "Iterator" && name == "next" {
        return Some(
            "Advances the iterator and returns its next value, or None when it is exhausted.",
        );
    }
    if owner == "sync::Map" {
        return match name {
            "new" => Some("Creates an empty concurrent string map."),
            "insert" => Some("Associates a string key with a string value."),
            "get" => Some("Returns the value for a key, or None when absent."),
            "remove" => Some("Removes a key and its value when present."),
            "len" => Some("Returns the number of entries."),
            "contains_key" => Some("Reports whether the map contains a key."),
            "keys" => Some("Returns a snapshot of the current keys."),
            _ => None,
        };
    }
    match (owner, name) {
        ("String", "byte_at") => Some("Returns the byte at an index, or -1 when out of range."),
        ("String", "byte_len") => Some("Returns the byte length of the string."),
        ("String", "bytes") => Some("Returns the UTF-8 bytes of the string."),
        ("String", "center") => Some("Pads both sides to the requested display width."),
        ("String", "chars") => Some(
            "Returns a cursor over the string's Unicode scalar values; `collect` materialises it.",
        ),
        ("String", "contains") => Some("Returns whether the string contains a substring."),
        ("String", "contains_any") => Some("Returns whether any character in the set appears."),
        ("String", "count") => Some("Counts non-overlapping substring occurrences."),
        ("String", "ends_with") => Some("Returns whether the string ends with a suffix."),
        ("String", "equal_fold") => Some("Compares strings with Unicode case folding."),
        ("String", "find") => Some("Returns the first byte index of a match."),
        ("String", "find_any") => Some("Returns the first byte index of any character in a set."),
        ("String", "index_rune") => Some("Returns the first byte index of a character."),
        ("String", "lines") => Some("Splits the string into lines."),
        ("String", "pad_left") => Some("Left-pads to the requested display width."),
        ("String", "pad_right") => Some("Right-pads to the requested display width."),
        ("String", "repeat") => Some("Repeats the string count times."),
        ("String", "replace") => Some("Replaces every occurrence of one pattern with another."),
        ("String", "replacen") => Some("Replaces at most n occurrences of a pattern."),
        ("String", "rfind") => Some("Returns the last byte index of a match."),
        ("String", "rfind_any") => Some("Returns the last byte index of any character in a set."),
        ("String", "rsplit_once") => Some("Splits once at the last matching separator."),
        ("String", "slice") => Some("Returns a checked byte-range slice."),
        ("String", "split") => Some("Splits on every matching separator."),
        ("String", "split_once") => Some("Splits once at the first matching separator."),
        ("String", "split_whitespace") => Some("Splits on runs of whitespace."),
        ("String", "splitn") => Some("Splits into at most n parts."),
        ("String", "starts_with") => Some("Returns whether the string starts with a prefix."),
        ("String", "strip_prefix") => Some("Removes a prefix when present."),
        ("String", "strip_suffix") => Some("Removes a suffix when present."),
        ("String", "substring") => Some("Returns a clamped character-range substring."),
        ("String", "to_bool") => Some("Parses exactly true or false to Option<bool>."),
        ("String", "to_f64") => Some("Parses the full string to Option<f64>."),
        ("String", "to_i64") => Some("Parses the full string to Option<i64>."),
        ("String", "to_lowercase") => Some("Lowercases every character."),
        ("String", "to_title") => Some("Title-cases the first letter of each word."),
        ("String", "to_uppercase") => Some("Uppercases every character."),
        ("String", "trim") => Some("Removes leading and trailing whitespace."),
        ("String", "trim_end") => Some("Removes trailing whitespace."),
        ("String", "trim_end_matches") => Some("Removes trailing characters from a set."),
        ("String", "trim_matches") => Some("Removes characters from a set at both ends."),
        ("String", "trim_start") => Some("Removes leading whitespace."),
        ("String", "trim_start_matches") => Some("Removes leading characters from a set."),
        ("Vec", "chain") => Some("Concatenates this sequence with another sequence."),
        ("Vec", "chunks") => Some("Groups values into fixed-size chunks."),
        ("Vec", "count") => Some("Counts values, or values accepted by a predicate."),
        ("Vec", "dedup") => Some("Removes adjacent duplicate values."),
        ("Vec", "enumerate") => Some("Pairs each value with its index."),
        ("Vec", "flatten") => Some("Flattens one level of nested vectors."),
        ("Vec", "for_each") => Some("Runs a closure for each value."),
        ("Vec", "max_by_key") => Some("Returns the maximum value by derived key."),
        ("Vec", "min_by_key") => Some("Returns the minimum value by derived key."),
        ("Vec", "pairwise") => Some("Returns adjacent value pairs."),
        ("Vec", "skip") => Some("Drops the first n values."),
        ("Vec", "step_by") => Some("Returns every nth value."),
        ("Vec", "take") => Some("Returns the first n values."),
        ("Vec", "windows") => Some("Returns overlapping fixed-size windows."),
        ("Vec", "zip") => Some("Pairs values with another sequence."),
        ("Map", "clear") => Some("Removes all entries."),
        ("Map", "inc") => Some("Increments an i64 counter value."),
        ("Map", "inc_at") => Some("Increments counters from a substring key range."),
        ("Map", "inc_batch") => Some("Increments counters for a batch of keys."),
        ("Set", "clear") => Some("Removes all values from the set."),
        ("Set", "is_disjoint") => Some("Returns true when two sets share no values."),
        ("Set", "is_empty") => Some("Returns true when the set has no values."),
        ("Set", "is_subset") => Some("Returns true when every value is in the other set."),
        ("Set", "is_superset") => Some("Returns true when the other set is a subset."),
        ("Set", "iter") => Some("Returns the set values as a vector."),
        ("Set", "len") => Some("Returns the number of values."),
        ("Set", "to_vec") => Some("Returns the set values as a vector."),
        ("Deque", "push_back") => Some("Appends a value to the back."),
        ("Deque", "push_front") => Some("Appends a value to the front."),
        ("Deque", "pop_front") => Some("Removes and returns the front value when present."),
        ("Deque", "pop_back") => Some("Removes and returns the back value when present."),
        ("Deque", "peek_front") => Some("Returns the front value without removing it."),
        ("Deque", "peek_back") => Some("Returns the back value without removing it."),
        ("Deque", "clear") => Some("Removes all values."),
        ("Deque", "is_empty") => Some("Returns true when the deque has no values."),
        ("Deque", "len") => Some("Returns the number of values."),
        ("Queue", "push") => Some("Appends a value to the back of the queue."),
        ("Queue", "pop") => Some("Removes and returns the front value when present."),
        ("Queue", "peek") => Some("Returns the front value without removing it."),
        ("Queue", "clear") => Some("Removes all values."),
        ("Queue", "is_empty") => Some("Returns true when the queue has no values."),
        ("Queue", "len") => Some("Returns the number of values."),
        ("Stack", "push") => Some("Appends a value to the top of the stack."),
        ("Stack", "pop") => Some("Removes and returns the top value when present."),
        ("Stack", "peek") => Some("Returns the top value without removing it."),
        ("Stack", "clear") => Some("Removes all values."),
        ("Stack", "is_empty") => Some("Returns true when the stack has no values."),
        ("Stack", "len") => Some("Returns the number of values."),
        ("Option", "filter") => Some("Keeps Some only when a predicate accepts it."),
        ("Option", "flatten") => Some("Flattens a nested Option."),
        ("Option", "iter") => Some("Returns a zero-or-one element vector."),
        ("Option", "or") => Some("Returns the receiver when Some, otherwise a fallback."),
        ("Option", "or_else") => Some("Calls a fallback closure only when None."),
        ("Option", "unwrap_or") => Some("Returns the payload or a fallback value."),
        ("Option", "unwrap_or_else") => Some("Calls a fallback closure only when None."),
        ("Option", "zip") => Some("Combines two Options when both are Some."),
        ("Result", "and_then") => Some("Chains an Ok value through a Result-returning closure."),
        ("Result", "err") => Some("Converts the Err payload to Option."),
        ("Result", "ok") => Some("Converts the Ok payload to Option."),
        ("Result", "or_else") => Some("Calls a fallback closure only when Err."),
        ("Result", "unwrap_or") => Some("Returns the Ok payload or a fallback value."),
        ("Result", "unwrap_or_else") => Some("Calls a fallback closure only when Err."),
        _ => None,
    }
}

pub(super) fn insert_core_method_entry(
    entries: &mut BTreeMap<(String, String), CoreMethodEntry>,
    entry: CoreMethodEntry,
) {
    entries
        .entry((entry.owner.clone(), entry.name.clone()))
        .or_insert(entry);
}

pub(super) fn registered_core_method_path(path: &str) -> Option<(String, String)> {
    let (owner, name) = path.rsplit_once("::")?;
    if name.starts_with("__") || owner == "Type" {
        return None;
    }
    let owner = canonical_runtime_owner(owner)?;
    Some((owner, name.to_string()))
}

pub(super) fn canonical_runtime_owner(owner: &str) -> Option<String> {
    let owner = owner.strip_prefix("collections::").unwrap_or(owner);
    let owner = match owner {
        "option" => "Option",
        "result" => "Result",
        // Runtime spellings of collections the language names once. A
        // rejected alias (GR0006) must not surface as a type of its own.
        "HashMap" => "Map",
        "HashSet" => "Set",
        "VecDeque" => "Deque",
        "BinaryHeap" => "MaxHeap",
        "bytes::Buffer" => "Buffer",
        "bytes::Builder" => "Builder",
        other => other,
    };
    if matches!(
        owner,
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize"
    ) {
        return Some(owner.to_string());
    }
    let last = owner.rsplit("::").next().unwrap_or(owner);
    if last.chars().next().is_some_and(char::is_uppercase) {
        Some(owner.to_string())
    } else {
        None
    }
}

pub(super) fn runtime_assoc_name(name: &str) -> bool {
    matches!(
        name,
        "new"
            | "with_capacity"
            | "from"
            | "from_utf8"
            | "background"
            | "with_cancel"
            | "with_timeout"
            | "bind"
            | "connect"
            | "open"
            | "create"
            | "default"
            | "builder"
            | "object"
            | "Object"
            | "Array"
            | "String"
            | "Int"
            | "Float"
            | "Bool"
            | "Null"
    )
}

pub(super) fn all_core_namespaces() -> Vec<String> {
    let mut owners = core_method_entries()
        .into_iter()
        .map(|method| method.owner)
        .collect::<Vec<_>>();
    // A range is iterator state with its own spelling, so it names a
    // namespace even though every method it answers is an iterator method.
    owners.push("Range".to_string());
    owners.sort_unstable();
    owners.dedup();
    owners
}

pub(super) fn matching_core_namespaces(query: &str) -> Vec<String> {
    all_core_namespaces()
        .into_iter()
        .filter(|owner| core_namespace_matches(owner, query))
        // A type with a `CORE_TYPES` row is rendered from that row, which
        // carries its spelling and example; the namespace list holds the
        // same name only because the type owns methods.
        .filter(|owner| !CORE_TYPES.iter().any(|core| core.name == owner))
        .collect()
}

pub(super) fn matching_modules(query: &str) -> Vec<StdModule> {
    gossamer_std::registry::modules()
        .iter()
        .copied()
        .filter(|module| module_query_matches(module, query))
        .collect()
}

pub(super) fn matching_items(query: &str) -> Vec<(StdModule, StdItem)> {
    let mut out = Vec::new();
    for module in gossamer_std::registry::modules() {
        for item in module.items {
            if item_query_matches(module, item, query) {
                out.push((*module, *item));
            }
        }
    }
    out
}

pub(super) fn matching_core_methods(query: &str) -> Vec<CoreMethodEntry> {
    core_method_entries()
        .into_iter()
        .filter(|method| core_method_query_matches(method, query))
        .collect()
}

pub(super) fn matching_builtin_macros(query: &str) -> Vec<&'static BuiltinMacro> {
    BUILTIN_MACROS
        .iter()
        .filter(|builtin| symbol_query_matches(builtin.name, query))
        .collect()
}

/// An operator's `%info` entry: the form it is written in, then, with details,
/// what it does and an example.
pub(super) fn push_operator_match(out: &mut String, operator: &CoreOperatorHelp, details: bool) {
    out.push_str(&format!("{} [operator]\n", operator.signature));
    if details {
        out.push_str(&format!("    {}\n", operator.doc));
        push_catalog_origin(out, "Builtin");
        out.push_str(&format!("    Example: {}\n", operator.example));
    }
}

/// The operator a query names. An operator is answered only for its exact
/// spelling: `*` is itself part of the operators, so no wildcard reading of
/// the query can say which one was meant.
pub(super) fn matching_core_operators(query: &str) -> Vec<&'static CoreOperatorHelp> {
    CORE_OPERATORS
        .iter()
        .filter(|operator| operator.spelling == query)
        .collect()
}

pub(super) fn matching_core_types(query: &str) -> Vec<&'static CoreTypeHelp> {
    CORE_TYPES
        .iter()
        .filter(|core_type| symbol_query_matches(core_type.name, query))
        .collect()
}

/// The built-in traits a query names. A trait the standard library declares
/// is reached through its module's manifest entry instead, so the catalog's
/// bare-name rendering covers only the ones the language supplies itself.
pub(super) fn matching_builtin_traits(query: &str) -> Vec<&'static gossamer_types::BuiltinTrait> {
    gossamer_types::BUILTIN_TRAITS
        .iter()
        .filter(|entry| entry.module.is_none() && symbol_query_matches(entry.name, query))
        .collect()
}

pub(super) fn matching_prelude_builtins(query: &str) -> Vec<&'static PreludeBuiltinHelp> {
    PRELUDE_BUILTINS
        .iter()
        .filter(|builtin| symbol_query_matches(builtin.name, query))
        .collect()
}

pub(super) fn module_query_matches(module: &StdModule, query: &str) -> bool {
    module_aliases(module.path)
        .iter()
        .any(|alias| symbol_query_matches(alias, query))
}

pub(super) fn core_namespace_matches(owner: &str, query: &str) -> bool {
    let (text, shape) = split_symbol_query(query);
    if shape != QueryShape::Exact {
        return shape_matches(owner, text, shape);
    }
    owner == text || owner.eq_ignore_ascii_case(text) || owner == canonical_collection_owner(text)
}

pub(super) fn item_query_matches(module: &StdModule, item: &StdItem, query: &str) -> bool {
    // A macro is written bare, so its calling form is a spelling of its own.
    if item.kind == StdItemKind::Builtin && symbol_query_matches(&item.call_name(), query) {
        return true;
    }
    // Every other item is named through the module that declares it, so its
    // spellings are the qualified ones.
    let names = [item.name.to_string(), item.call_name()];
    module_aliases(module.path).iter().any(|alias| {
        names
            .iter()
            .any(|name| symbol_query_matches(&format!("{alias}::{name}"), query))
    })
}

pub(super) fn core_method_query_matches(method: &CoreMethodEntry, query: &str) -> bool {
    let (text, shape) = split_symbol_query(query);
    let spellings = [
        method.name.clone(),
        format!("{}::{}", method.owner, method.name),
        core_lower_path(method),
    ];
    if spellings
        .iter()
        .any(|spelling| shape_matches(spelling, text, shape))
    {
        return true;
    }
    shape == QueryShape::Exact
        && text.rsplit_once("::").is_some_and(|(owner, name)| {
            name == method.name && canonical_collection_owner(owner) == method.owner
        })
}

pub(super) fn core_lower_path(method: &CoreMethodEntry) -> String {
    format!("{}::{}", method.owner.to_ascii_lowercase(), method.name)
}

pub(super) fn canonical_collection_owner(owner: &str) -> &str {
    owner.strip_prefix("collections::").unwrap_or(owner)
}

pub(super) fn info_search_query(arg: &str) -> String {
    normalize_query(arg).to_string()
}

/// How a `%info` / `%explain` argument matches a candidate spelling. A bare
/// argument names one symbol; a leading or trailing `*` widens it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum QueryShape {
    Exact,
    Prefix,
    Suffix,
    Substring,
}

pub(super) fn symbol_query_matches(candidate: &str, query: &str) -> bool {
    let (text, shape) = split_symbol_query(query);
    shape_matches(candidate, text, shape)
}

pub(super) fn shape_matches(candidate: &str, text: &str, shape: QueryShape) -> bool {
    match shape {
        QueryShape::Exact => candidate == text,
        QueryShape::Prefix => candidate.starts_with(text),
        QueryShape::Suffix => candidate.ends_with(text),
        QueryShape::Substring => candidate.contains(text),
    }
}

pub(super) fn split_symbol_query(query: &str) -> (&str, QueryShape) {
    let leading = query.starts_with('*');
    // A lone `*` is one wildcard, not both ends of an empty pattern.
    let trailing = query.len() > 1 && query.ends_with('*');
    let shape = match (leading, trailing) {
        (true, true) => QueryShape::Substring,
        (true, false) => QueryShape::Suffix,
        (false, true) => QueryShape::Prefix,
        (false, false) => QueryShape::Exact,
    };
    (query.trim_matches('*'), shape)
}

pub(super) fn module_matches_regex(pattern: &Regex, module: &StdModule) -> bool {
    pattern.is_match(module.path) || pattern.is_match(module.summary)
}

pub(super) fn item_matches_regex(pattern: &Regex, module: &StdModule, item: &StdItem) -> bool {
    pattern.is_match(&format!("{}::{}", module.path, item.name))
        || pattern.is_match(item.name)
        || pattern.is_match(item.doc)
}

pub(super) fn module_aliases(path: &'static str) -> Vec<&'static str> {
    let mut aliases = vec![path];
    if let Some(stripped) = path.strip_prefix("std::") {
        aliases.push(stripped);
    }
    if let Some(last) = path.rsplit("::").next()
        && !aliases.contains(&last)
    {
        aliases.push(last);
    }
    aliases
}

pub(super) fn normalize_query(arg: &str) -> &str {
    arg.trim_matches('`').trim()
}

/// The method an `impl` of a stdlib trait must supply, rendered as the
/// listing's signature suffix. `None` for a trait with no required method.
pub(super) fn trait_required_signature(name: &str) -> Option<String> {
    let entry = gossamer_types::builtin_trait(name)?;
    (!entry.signature.is_empty()).then(|| format!(" {}", entry.signature))
}

pub(super) fn item_kind_label(kind: StdItemKind) -> &'static str {
    match kind {
        StdItemKind::Function => "fn",
        StdItemKind::Type => "type",
        StdItemKind::Trait => "trait",
        StdItemKind::Builtin => "builtin",
        StdItemKind::Const => "const",
    }
}

/// True for assignment, built-in mutating collection calls, or loop forms that
/// can mutate through a mutable reference.
/// Type checking remains authoritative for receiver mutability and whether
/// the method exists. This parser-only classification only controls replay.
pub(super) fn input_mutates_binding(input: &str, user_mutating_methods: &HashSet<String>) -> bool {
    use gossamer_ast::{ExprKind, ItemKind, StmtKind};

    // End the input before the synthetic closing brace so a trailing line
    // comment cannot consume it.
    let source = format!("fn __irepl_classify() {{ {input}\n}}\n");
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file("irepl-classify".to_string(), source.clone());
    let (sf, diags) = gossamer_parse::parse_source_file(&source, file);
    if !diags.is_empty() {
        return false;
    }
    let Some(item) = sf.items.first() else {
        return false;
    };
    let ItemKind::Fn(decl) = &item.kind else {
        return false;
    };
    let Some(body) = &decl.body else {
        return false;
    };
    let ExprKind::Block(block) = &body.kind else {
        return false;
    };
    let target = block.tail.as_deref().or_else(|| match block.stmts.last() {
        Some(stmt) => match &stmt.kind {
            StmtKind::Expr { expr, .. } => Some(expr.as_ref()),
            _ => None,
        },
        None => None,
    });

    target.is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
}

pub(super) fn collect_repl_mut_self_method_names(declarations: &[String]) -> HashSet<String> {
    use gossamer_ast::{FnParam, ImplItem, ItemKind, Receiver};

    let source = render_repl_declarations(declarations);
    if source.trim().is_empty() {
        return HashSet::new();
    }
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file("irepl-mut-self-methods".to_string(), source.clone());
    let (sf, diags) = gossamer_parse::parse_source_file(&source, file);
    if !diags.is_empty() {
        return HashSet::new();
    }
    let mut names = HashSet::new();
    for item in sf.items {
        let ItemKind::Impl(decl) = item.kind else {
            continue;
        };
        for item in decl.items {
            let ImplItem::Fn(method) = item else {
                continue;
            };
            if matches!(
                method.params.first(),
                Some(FnParam::Receiver(Receiver::RefMut))
            ) {
                names.insert(method.name.name);
            }
        }
    }
    names
}
