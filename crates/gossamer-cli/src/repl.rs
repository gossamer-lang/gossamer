//! Interactive REPL.
//!
//! Kept in its own module so `main.rs` stays under the 2000-line
//! hard limit defined in `GUIDELINES.md`.

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;

use anyhow::{Result, anyhow};
use gossamer_parse::builtin_macros::{BUILTIN_MACROS, BuiltinMacro};
use gossamer_std::registry::{StdItem, StdItemKind, StdModule};
use regex::Regex;

use crate::paths::repl_history_path;

const REPL_HELP_TEXT: &str = "\
REPL commands

  %help
    Show this list of REPL commands.
  %info (%i) [name] [-d|--details]
    Show a language, standard-library, or session symbol by name; a type
    reports its fields, the traits implemented for it, and its methods, and a
    trait reports its methods and implementors. Use -d for documentation.
  %explain (%e) NAME [-d|--details]
    Inspect a persistent `let` binding or declaration, including its fields and
    implemented traits; use -d for methods and capability.
  %bindings (%b) [pattern]
    Show persistent `let` bindings. Patterns filter binding names.
  %drop NAME
    End a persistent binding's lexical lifetime and remove it, or remove the
    declaration that introduced NAME so the name can be declared again.
  %declarations (%d) [pattern]
    Show persistent declarations. Patterns filter declaration names.
  %history (%h) [regex]
    Search inputs from this and previous sessions.
  %clear-history
    Delete all saved inputs and clear up/down history.
  %reset (%r)
    Clear persistent bindings and declarations.
  %quit (%q)
    Exit the REPL.

Expressions print their value. Declarations and `let` bindings persist.

%info and %explain name one symbol exactly; `*` widens that to a prefix
(`Set*`), a suffix (`*Set`), or a substring (`*Set*`). %info also accepts
/regex/. %bindings, %declarations, and %history search substrings or /regex/.

Up/down cycles history.";

const REPL_FALLBACK_COLUMNS: usize = 80;

fn repl_output_width() -> usize {
    crate::style::terminal_width(REPL_FALLBACK_COLUMNS, 24)
}

// A heading line is a symbol's signature: code the reader copies, which a
// reflow at whitespace would split inside a type list. Only the indented
// detail lines are prose.
fn wrap_repl_output(text: &str) -> String {
    let width = repl_output_width();
    text.lines()
        .flat_map(|line| {
            if line.starts_with(char::is_whitespace) {
                wrap_repl_line(line, width)
            } else {
                vec![line.to_string()]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrap_repl_line(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width {
        return vec![line.to_string()];
    }
    let indent_len = line.chars().take_while(|ch| ch.is_whitespace()).count();
    let indent = " ".repeat(indent_len.min(width.saturating_sub(1)));
    let continuation_len = indent_len.min(width.saturating_sub(1));
    let continuation = " ".repeat(continuation_len);
    let mut lines = Vec::new();
    let mut current = indent;
    for word in line.split_whitespace() {
        let separator = usize::from(!current.trim().is_empty());
        if current.chars().count() + separator + word.chars().count() > width
            && !current.trim().is_empty()
        {
            lines.push(std::mem::take(&mut current));
            current.push_str(&continuation);
        }
        if !current.trim().is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn print_repl_output(text: &str) {
    let wrapped = wrap_repl_output(text);
    for line in wrapped.lines() {
        println!("{}", style_repl_output_line(line));
    }
}

fn style_repl_output_line(line: &str) -> String {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return String::new();
    }
    if !line.starts_with(char::is_whitespace) {
        return crate::style::repl_meta_heading(line);
    }
    if trimmed.starts_with('%') {
        return crate::style::repl_meta_accent(line);
    }
    crate::style::repl_meta_detail(line)
}

fn print_repl_error(message: &str) {
    eprintln!("{}", crate::style::repl_error(message));
}

struct PreludeBuiltinHelp {
    name: &'static str,
    signature: &'static str,
    doc: &'static str,
}

struct CoreMethodHelp {
    owner: &'static str,
    name: &'static str,
    kind: &'static str,
    signature: &'static str,
    doc: &'static str,
}

/// A built-in type whose surface is syntax rather than methods, so it
/// has no entry in [`CORE_METHODS`] to be discovered through.
struct CoreTypeHelp {
    name: &'static str,
    signature: &'static str,
    doc: &'static str,
    example: &'static str,
}

#[derive(Clone, Debug)]
struct CoreMethodEntry {
    owner: String,
    name: String,
    kind: &'static str,
    signature: String,
    doc: String,
}

/// What a binding's name reaches through a dot: the methods its type and
/// capability can call, the fields its session-declared type has, and the
/// positions a tuple is read by. `%explain` prints this surface and Tab
/// completes it, so what discovery lists is what completion offers.
#[derive(Clone, Debug, Default)]
pub(crate) struct BindingSurface {
    owner: Option<String>,
    type_name: String,
    fixed_array: bool,
    can_mutate: bool,
    tuple_arity: usize,
}

impl BindingSurface {
    /// A surface owned by one type, with a writable receiver. Test-only:
    /// the REPL builds every surface from a binding it has type-checked.
    #[cfg(test)]
    pub(crate) fn owned_by(owner: &str) -> Self {
        Self {
            owner: Some(owner.to_string()),
            type_name: owner.to_string(),
            fixed_array: owner == "Array",
            can_mutate: true,
            tuple_arity: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_tuple_arity(&mut self, arity: usize) {
        self.tuple_arity = arity;
    }

    fn of(var: &ReplBindingVar, ty: &ReplValueType) -> Self {
        Self {
            owner: ty.method_owner.clone(),
            type_name: base_type_name(&ty.rendered).to_string(),
            fixed_array: ty.fixed_array,
            can_mutate: binding_can_mutate(var, ty),
            tuple_arity: ty.tuple_elements.len(),
        }
    }
}

/// Every member `binding.` reaches, sorted and de-duplicated. The session's
/// own declarations are read on each call, so an `impl` block added after
/// the binding is offered the moment it is accepted.
pub(crate) fn binding_member_names(
    surface: &BindingSurface,
    declarations: &[String],
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(ref owner) = surface.owner {
        names.extend(
            core_methods_for(owner, surface.fixed_array, surface.can_mutate)
                .into_iter()
                .map(|method| method.name),
        );
    }
    for position in 0..surface.tuple_arity {
        names.push(position.to_string());
    }
    let index = session_index(declarations);
    if let Some(fields) = index.fields.get(&surface.type_name) {
        names.extend(fields.iter().map(|(field, _)| field.clone()));
    }
    if let Some(methods) = index.methods.get(&surface.type_name) {
        names.extend(
            methods
                .iter()
                .filter_map(|(signature, _)| receiver_method_name(signature)),
        );
    }
    names.sort();
    names.dedup();
    names
}

/// Every member a type's own name reaches, whichever spelling of the type
/// is written: the methods and associated functions `%info` reports for it,
/// plus what the session's own `impl` blocks add.
pub(crate) fn qualified_member_names(owner: &str, declarations: &[String]) -> Vec<String> {
    let canonical = canonical_collection_owner(owner);
    let short = canonical.rsplit("::").next().unwrap_or(canonical);
    let mut names: Vec<String> = core_method_entries()
        .into_iter()
        .filter(|entry| entry.owner == canonical || entry.owner == short)
        .map(|entry| entry.name)
        .collect();
    let index = session_index(declarations);
    if let Some(methods) = index.methods.get(canonical) {
        names.extend(methods.iter().filter_map(|(signature, _)| {
            signature.split_once('(').map(|(name, _)| name.to_string())
        }));
    }
    names.sort();
    names.dedup();
    names
}

/// The name of a session-declared function when it takes a receiver, so an
/// associated function is not offered as a method of a value.
fn receiver_method_name(signature: &str) -> Option<String> {
    let (name, rest) = signature.split_once('(')?;
    signature_takes_receiver(rest).then(|| name.to_string())
}

/// Whether a parameter list, written without its opening parenthesis,
/// begins with a `self` receiver.
fn signature_takes_receiver(params: &str) -> bool {
    let params = params.trim_start();
    let params = params
        .strip_prefix("&mut ")
        .or_else(|| params.strip_prefix('&'))
        .unwrap_or(params);
    let Some(rest) = params.strip_prefix("self") else {
        return false;
    };
    rest.starts_with([',', ')', ':'])
}

/// The core methods a receiver of this shape can call: no resizing method
/// on a fixed-length sequence, and no in-place mutation without writable
/// access to the binding.
fn core_methods_for(owner: &str, fixed_array: bool, can_mutate: bool) -> Vec<CoreMethodEntry> {
    let owner = canonical_collection_owner(owner);
    core_method_entries()
        .into_iter()
        .filter(|method| {
            method.kind == "method"
                && method.owner == owner
                && (!fixed_array || !gossamer_types::is_vec_only_sequence_method(&method.name))
                && (can_mutate || !gossamer_types::is_mutating_method_name(&method.name))
        })
        .collect()
}

// These prelude functions are runtime builtins rather than stdlib-manifest
// exports. Every parser-recognized macro is sourced from BUILTIN_MACROS below.
const PRELUDE_BUILTINS: &[PreludeBuiltinHelp] = &[
    PreludeBuiltinHelp {
        name: "assert",
        signature: "assert(condition: bool, message: String)",
        doc: "Panics when condition is false. The message may be omitted.",
    },
    PreludeBuiltinHelp {
        name: "assert_eq",
        signature: "assert_eq(left, right, message: String)",
        doc: "Panics when left and right are not equal. The message may be omitted.",
    },
];

// Types the language provides through syntax alone. They own no methods,
// so `%info` would otherwise have nothing to report for them.
const CORE_TYPES: &[CoreTypeHelp] = &[CoreTypeHelp {
    name: "Tuple",
    // Rendered like the other `[type]` entries, whose one-line form is the
    // bare name; the spelling `(A, B, ...)` leads the doc instead.
    signature: "",
    doc: "`(A, B, ...)` - a fixed-length group of values whose element types may differ. \
          Written `(a, b, c)`, with `()` for the empty tuple and a trailing \
          comma for the one-element form `(a,)`. Elements are read and \
          assigned positionally (`t.0`, `t.1`, chained as `t.0.1`), bound by \
          destructuring (`let a, b = pair`), and compared field by field in \
          declaration order.",
    example: "let t = (1, \"two\", 3.0); println(\"{} {}\", t.0, t.1); let n, s, f = t",
}];

// Core receiver and associated methods are runtime builtins, not stdlib module
// exports. Keep them visible to REPL discovery so working calls such as
// `"123".parse()` are not hidden from `%help` and `%info`.
const CORE_METHODS: &[CoreMethodHelp] = &[
    CoreMethodHelp {
        owner: "Tuple",
        name: "len",
        kind: "method",
        signature: "fn len(self: Tuple) -> i64",
        doc: "Element count, fixed by the tuple's type and folded at compile time.",
    },
    CoreMethodHelp {
        owner: "Tuple",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty(self: Tuple) -> bool",
        doc: "True only for the empty tuple `()`.",
    },
    CoreMethodHelp {
        owner: "Tuple",
        name: "get",
        kind: "method",
        signature: "fn get(self: Tuple, index: i64) -> Option<T>",
        doc: "Element at a runtime index; prefer `t.0` when the position is known.",
    },
    CoreMethodHelp {
        owner: "Tuple",
        name: "clone",
        kind: "method",
        signature: "fn clone(self: Tuple) -> Tuple",
        doc: "Copies the tuple, sharing each element's storage.",
    },
    CoreMethodHelp {
        owner: "Tuple",
        name: "to_string",
        kind: "method",
        signature: "fn to_string(self: Tuple) -> String",
        doc: "Renders as `(a, b, ...)`, the same text `{}` and `{:?}` produce.",
    },
    CoreMethodHelp {
        owner: "Tuple",
        name: "into",
        kind: "method",
        signature: "fn into<B>(self: Tuple) -> B",
        doc: "Converts to the type the use site fixes, through that type's \
              `From` impl. The target never comes from the receiver, so a \
              bare `(1, 2).into()` has nothing to convert to.",
    },
    CoreMethodHelp {
        owner: "Tuple",
        name: "try_into",
        kind: "method",
        signature: "fn try_into<B, E>(self: Tuple) -> Result<B, E>",
        doc: "The fallible form, answering `Result<B, E>` through the \
              target's `TryFrom` impl. The target is fixed by the use site \
              the same way `into` is.",
    },
    CoreMethodHelp {
        owner: "Buffer",
        name: "new",
        kind: "assoc",
        signature: "fn new() -> bytes::Buffer",
        doc: "Creates an empty byte buffer.",
    },
    CoreMethodHelp {
        owner: "Buffer",
        name: "with_capacity",
        kind: "assoc",
        signature: "fn with_capacity(capacity: i64) -> bytes::Buffer",
        doc: "Creates an empty byte buffer with capacity reserved.",
    },
    CoreMethodHelp {
        owner: "Buffer",
        name: "push",
        kind: "method",
        signature: "fn push(&mut self, byte: u8) -> ()",
        doc: "Appends one byte.",
    },
    CoreMethodHelp {
        owner: "Buffer",
        name: "write_str",
        kind: "method",
        signature: "fn write_str(&mut self, text: String) -> ()",
        doc: "Appends a string's UTF-8 bytes.",
    },
    CoreMethodHelp {
        owner: "Buffer",
        name: "clear",
        kind: "method",
        signature: "fn clear(&mut self) -> ()",
        doc: "Clears the buffer in place.",
    },
    CoreMethodHelp {
        owner: "Buffer",
        name: "len",
        kind: "method",
        signature: "fn len(&self) -> i64",
        doc: "Returns the number of buffered bytes.",
    },
    CoreMethodHelp {
        owner: "Buffer",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty(&self) -> bool",
        doc: "Returns true when the buffer has no bytes.",
    },
    CoreMethodHelp {
        owner: "Buffer",
        name: "to_string",
        kind: "method",
        signature: "fn to_string(&self) -> String",
        doc: "Decodes the buffered bytes with lossy UTF-8 replacement.",
    },
    CoreMethodHelp {
        owner: "Builder",
        name: "new",
        kind: "assoc",
        signature: "fn new() -> bytes::Builder",
        doc: "Creates an empty string builder.",
    },
    CoreMethodHelp {
        owner: "Builder",
        name: "with_capacity",
        kind: "assoc",
        signature: "fn with_capacity(capacity: i64) -> bytes::Builder",
        doc: "Creates an empty string builder with capacity reserved.",
    },
    CoreMethodHelp {
        owner: "Builder",
        name: "write",
        kind: "method",
        signature: "fn write(&mut self, text: String) -> ()",
        doc: "Appends text.",
    },
    CoreMethodHelp {
        owner: "Builder",
        name: "write_char",
        kind: "method",
        signature: "fn write_char(&mut self, ch: char) -> ()",
        doc: "Appends one Unicode scalar.",
    },
    CoreMethodHelp {
        owner: "Builder",
        name: "len",
        kind: "method",
        signature: "fn len(&self) -> i64",
        doc: "Returns the accumulated byte length.",
    },
    CoreMethodHelp {
        owner: "Builder",
        name: "as_str",
        kind: "method",
        signature: "fn as_str(&self) -> String",
        doc: "Returns the accumulated text.",
    },
    CoreMethodHelp {
        owner: "Builder",
        name: "build",
        kind: "method",
        signature: "fn build(&self) -> String",
        doc: "Returns the accumulated text.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "new",
        kind: "assoc",
        signature: "fn new() -> String",
        doc: "Creates an empty owned string.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "with_capacity",
        kind: "assoc",
        signature: "fn with_capacity(capacity: i64) -> String",
        doc: "Creates an empty string with capacity reserved.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "from",
        kind: "assoc",
        signature: "fn from<T: Display>(value: T) -> String",
        doc: "Converts a Display value into a string.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "from_utf8",
        kind: "assoc",
        signature: "fn from_utf8(bytes: Vec<u8>) -> Result<String, errors::Error>",
        doc: "Decodes UTF-8 bytes into a string.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "parse",
        kind: "method",
        signature: "fn parse<T>(self: String) -> Result<T, errors::Error>",
        doc: "Parses the string into the expected result type.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "len",
        kind: "method",
        signature: "fn len(self: String) -> i64",
        doc: "Returns the byte length of the string.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty(self: String) -> bool",
        doc: "Returns true when the string has zero bytes.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "clear",
        kind: "method",
        signature: "fn clear(self: &mut String) -> ()",
        doc: "Clears the string in place.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "truncate",
        kind: "method",
        signature: "fn truncate(self: &mut String, len: i64) -> ()",
        doc: "Truncates the string at a valid UTF-8 boundary.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "push",
        kind: "method",
        signature: "fn push(self: &mut String, ch: char) -> ()",
        doc: "Appends a Unicode scalar.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "push_char",
        kind: "method",
        signature: "fn push_char(self: &mut String, ch: char) -> ()",
        doc: "Appends a Unicode scalar.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "push_byte",
        kind: "method",
        signature: "fn push_byte(self: &mut String, byte: i64) -> ()",
        doc: "Appends the byte as a Unicode scalar.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "push_str",
        kind: "method",
        signature: "fn push_str(self: &mut String, text: String) -> ()",
        doc: "Appends string contents.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "push_utf8",
        kind: "method",
        signature: "fn push_utf8(self: &mut String, buf: Vec<u8>, start: i64, end: i64) -> bool",
        doc: "Appends the [start, end) byte window of buf when it is valid UTF-8.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "clone",
        kind: "method",
        signature: "fn clone(self: String) -> String",
        doc: "Returns a copy of the string.",
    },
    CoreMethodHelp {
        owner: "String",
        name: "as_bytes",
        kind: "method",
        signature: "fn as_bytes(self: String) -> Vec<u8>",
        doc: "Returns the UTF-8 bytes of the string.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "new",
        kind: "assoc",
        signature: "fn new<T>() -> Vec<T>",
        doc: "Creates an empty vector.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "from",
        kind: "assoc",
        signature: "fn from<T, const N: usize>(values: [T; N]) -> Vec<T>",
        doc: "Creates a growable vector by moving values from a fixed-size array.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "with_capacity",
        kind: "assoc",
        signature: "fn with_capacity<T>(capacity: i64) -> Vec<T>",
        doc: "Creates an empty vector with capacity reserved.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "clone",
        kind: "method",
        signature: "fn clone<T>(self: Vec<T>) -> Vec<T>",
        doc: "Returns a copy of the vector.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "push",
        kind: "method",
        signature: "fn push<T>(self: &mut Vec<T>, value: T) -> ()",
        doc: "Appends a value to the end of the vector.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "pop",
        kind: "method",
        signature: "fn pop<T>(self: &mut Vec<T>) -> Option<T>",
        doc: "Removes and returns the last value when present.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "insert",
        kind: "method",
        signature: "fn insert<T>(self: &mut Vec<T>, index: i64, value: T) -> Result<(), errors::Error>",
        doc: "Inserts a value at an index.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "remove",
        kind: "method",
        signature: "fn remove<T>(self: &mut Vec<T>, index: i64) -> Result<T, errors::Error>",
        doc: "Removes and returns the value at an index.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "clear",
        kind: "method",
        signature: "fn clear<T>(self: &mut Vec<T>) -> ()",
        doc: "Removes all values.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "extend",
        kind: "method",
        signature: "fn extend<T>(self: &mut Vec<T>, values: Vec<T>) -> ()",
        doc: "Appends all values from another vector.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "extend_from_slice",
        kind: "method",
        signature: "fn extend_from_slice<T>(self: &mut Vec<T>, values: Vec<T>) -> ()",
        doc: "Appends all values from another vector.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "truncate",
        kind: "method",
        signature: "fn truncate<T>(self: &mut Vec<T>, len: i64) -> ()",
        doc: "Shortens the vector to at most len values.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "reserve",
        kind: "method",
        signature: "fn reserve<T>(self: &mut Vec<T>, capacity: i64) -> ()",
        doc: "Ensures at least the requested total capacity.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "reserve_exact",
        kind: "method",
        signature: "fn reserve_exact<T>(self: &mut Vec<T>, capacity: i64) -> ()",
        doc: "Ensures at least the requested total capacity without extra growth.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "len",
        kind: "method",
        signature: "fn len<T>(self: Vec<T>) -> i64",
        doc: "Returns the number of values.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "capacity",
        kind: "method",
        signature: "fn capacity<T>(self: Vec<T>) -> i64",
        doc: "Returns the current vector capacity.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty<T>(self: Vec<T>) -> bool",
        doc: "Returns true when the vector has no values.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "slice",
        kind: "method",
        signature: "fn slice<T>(self: Vec<T>, start: i64, end: i64) -> Result<Vec<T>, errors::Error>",
        doc: "Returns a checked sub-slice copy.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "first",
        kind: "method",
        signature: "fn first<T>(self: Vec<T>) -> Option<T>",
        doc: "Returns the first value when present.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "last",
        kind: "method",
        signature: "fn last<T>(self: Vec<T>) -> Option<T>",
        doc: "Returns the last value when present.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "get",
        kind: "method",
        signature: "fn get<T>(self: Vec<T>, index: i64) -> Option<T>",
        doc: "Returns the value at an index when present.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "contains",
        kind: "method",
        signature: "fn contains<T>(self: Vec<T>, value: T) -> bool",
        doc: "Returns true when the vector contains the value.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "index_of",
        kind: "method",
        signature: "fn index_of<T>(self: Vec<T>, value: T) -> Option<i64>",
        doc: "Returns the first matching index when present.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "count_of",
        kind: "method",
        signature: "fn count_of<T>(self: Vec<T>, value: T) -> i64",
        doc: "Counts values equal to the argument.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "sort",
        kind: "method",
        signature: "fn sort<T>(self: &mut Vec<T>) -> ()",
        doc: "Sorts the vector in place.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "sort_by",
        kind: "method",
        signature: "fn sort_by<T>(self: &mut Vec<T>, cmp: fn(T, T) -> i64) -> ()",
        doc: "Sorts the vector in place with a comparator.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "sort_by_key",
        kind: "method",
        signature: "fn sort_by_key<T, K>(self: &mut Vec<T>, f: fn(T) -> K) -> ()",
        doc: "Sorts the vector in place by a derived key.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "reverse",
        kind: "method",
        signature: "fn reverse<T>(self: &mut Vec<T>) -> ()",
        doc: "Reverses the vector in place.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "binary_search",
        kind: "method",
        signature: "fn binary_search<T>(self: Vec<T>, needle: T) -> Result<i64, i64>",
        doc: "Index of a matching element in an already-ascending sequence; \
              `Err` carries the position an insert would keep sorted.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "copy_from_slice",
        kind: "method",
        signature: "fn copy_from_slice<T>(self: &mut Vec<T>, source: Vec<T>) -> ()",
        doc: "Overwrites every element with the matching one of `source`; \
              the lengths must match.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "copy_within",
        kind: "method",
        signature: "fn copy_within<T>(self: &mut Vec<T>, src: i64, dest: i64, len: i64) -> ()",
        doc: "Moves `len` elements from `src` to `dest` inside one Vec, \
              correct when the ranges overlap.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "resize",
        kind: "method",
        signature: "fn resize<T>(self: &mut Vec<T>, new_len: i64, value: T) -> ()",
        doc: "Shrinks by dropping the tail, or grows by appending copies of `value`.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "fill",
        kind: "method",
        signature: "fn fill<T>(self: &mut Vec<T>, value: T) -> ()",
        doc: "Clones a value into every existing element without resizing.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "iter",
        kind: "method",
        signature: "fn iter<T>(self: Vec<T>) -> Iterator<T>",
        doc: "Returns an iterator over the vector values.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "rev",
        kind: "method",
        signature: "fn rev<T>(self: Vec<T>) -> Vec<T>",
        doc: "Returns a reversed vector.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "dedup",
        kind: "method",
        signature: "fn dedup<T>(self: Vec<T>) -> Vec<T>",
        doc: "Removes adjacent duplicate values.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "take",
        kind: "method",
        signature: "fn take<T>(self: Vec<T>, n: i64) -> Vec<T>",
        doc: "Returns the first n values.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "skip",
        kind: "method",
        signature: "fn skip<T>(self: Vec<T>, n: i64) -> Vec<T>",
        doc: "Drops the first n values.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "take_while",
        kind: "method",
        signature: "fn take_while<T>(self: Vec<T>, f: fn(T) -> bool) -> Vec<T>",
        doc: "Returns the leading run of values accepted by a predicate.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "skip_while",
        kind: "method",
        signature: "fn skip_while<T>(self: Vec<T>, f: fn(T) -> bool) -> Vec<T>",
        doc: "Drops the leading run of values accepted by a predicate.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "step_by",
        kind: "method",
        signature: "fn step_by<T>(self: Vec<T>, step: i64) -> Vec<T>",
        doc: "Returns every nth value.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "chain",
        kind: "method",
        signature: "fn chain<T>(self: Vec<T>, other: Vec<T>) -> Vec<T>",
        doc: "Concatenates this sequence with another sequence.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "zip",
        kind: "method",
        signature: "fn zip<T, U>(self: Vec<T>, other: Vec<U>) -> Vec<(T, U)>",
        doc: "Pairs values with another sequence.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "windows",
        kind: "method",
        signature: "fn windows<T>(self: Vec<T>, size: i64) -> Vec<Vec<T>>",
        doc: "Returns overlapping fixed-size windows.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "chunks",
        kind: "method",
        signature: "fn chunks<T>(self: Vec<T>, size: i64) -> Vec<Vec<T>>",
        doc: "Groups values into fixed-size chunks.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "pairwise",
        kind: "method",
        signature: "fn pairwise<T>(self: Vec<T>) -> Vec<(T, T)>",
        doc: "Returns adjacent value pairs.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "flatten",
        kind: "method",
        signature: "fn flatten<T>(self: Vec<Vec<T>>) -> Vec<T>",
        doc: "Flattens one level of nested vectors.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "swap",
        kind: "method",
        signature: "fn swap<T>(self: &mut Vec<T>, a: i64, b: i64)",
        doc: "Swaps two vector positions; an index outside [0, len) panics.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "join",
        kind: "method",
        signature: "fn join<T>(self: Vec<T>, sep: String) -> String",
        doc: "Joins displayable values with a separator.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "map",
        kind: "method",
        signature: "fn map<T, U>(self: Vec<T>, f: fn(T) -> U) -> Vec<U>",
        doc: "Maps every value through a closure.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "filter",
        kind: "method",
        signature: "fn filter<T>(self: Vec<T>, f: fn(T) -> bool) -> Vec<T>",
        doc: "Keeps values accepted by a predicate.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "fold",
        kind: "method",
        signature: "fn fold<T, A>(self: Vec<T>, init: A, f: fn(A, T) -> A) -> A",
        doc: "Reduces values with an accumulator.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "for_each",
        kind: "method",
        signature: "fn for_each<T>(self: Vec<T>, f: fn(T) -> ()) -> ()",
        doc: "Runs a closure for each value.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "any",
        kind: "method",
        signature: "fn any<T>(self: Vec<T>, f: fn(T) -> bool) -> bool",
        doc: "Returns true if any value matches.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "all",
        kind: "method",
        signature: "fn all<T>(self: Vec<T>, f: fn(T) -> bool) -> bool",
        doc: "Returns true if all values match.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "find",
        kind: "method",
        signature: "fn find<T>(self: Vec<T>, f: fn(T) -> bool) -> Option<T>",
        doc: "Returns the first matching value.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "position",
        kind: "method",
        signature: "fn position<T>(self: Vec<T>, f: fn(T) -> bool) -> Option<i64>",
        doc: "Returns the first matching index.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "count",
        kind: "method",
        signature: "fn count<T>(self: Vec<T>) -> i64",
        doc: "Counts values.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "enumerate",
        kind: "method",
        signature: "fn enumerate<T>(self: Vec<T>) -> Vec<(i64, T)>",
        doc: "Pairs each value with its index.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "sum",
        kind: "method",
        signature: "fn sum<T>(self: Vec<T>) -> T",
        doc: "Sums numeric values.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "product",
        kind: "method",
        signature: "fn product<T>(self: Vec<T>) -> T",
        doc: "Multiplies every element, answering the element type's one for an empty sequence.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "inc",
        kind: "method",
        signature: "fn inc<K>(self: &mut Map<K, i64>, key: K, by: i64 = 1) -> i64",
        doc: "Adds to the counter at a key, starting from zero, and answers the new count.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "inc_at",
        kind: "method",
        signature: "fn inc_at(self: &mut Map<String, i64>, text: String, start: i64, len: i64, \
                    by: i64 = 1) -> i64",
        doc: "Adds to the counter keyed by a byte range of `text`, and answers the new count.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "inc_batch",
        kind: "method",
        signature: "fn inc_batch<K>(self: &mut Map<K, i64>, keys: Vec<K>, by: i64 = 1) \
                    -> Map<K, i64>",
        doc: "Adds to the counter at every key in one pass, and answers the map.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "min",
        kind: "method",
        signature: "fn min<T>(self: Vec<T>) -> Option<T>",
        doc: "Returns the minimum value when present.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "max",
        kind: "method",
        signature: "fn max<T>(self: Vec<T>) -> Option<T>",
        doc: "Returns the maximum value when present.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "min_by_key",
        kind: "method",
        signature: "fn min_by_key<T, K>(self: Vec<T>, f: fn(T) -> K) -> Option<T>",
        doc: "Returns the minimum value by derived key.",
    },
    CoreMethodHelp {
        owner: "Vec",
        name: "max_by_key",
        kind: "method",
        signature: "fn max_by_key<T, K>(self: Vec<T>, f: fn(T) -> K) -> Option<T>",
        doc: "Returns the maximum value by derived key.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "new",
        kind: "assoc",
        signature: "fn new<K, V>() -> Map<K, V>",
        doc: "Creates an empty hash map.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "with_capacity",
        kind: "assoc",
        signature: "fn with_capacity<K, V>(capacity: i64) -> Map<K, V>",
        doc: "Creates an empty hash map with capacity reserved.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "from",
        kind: "assoc",
        signature: "fn from<K, V, const N: usize>(entries: [(K, V); N]) -> Map<K, V>",
        doc: "Creates a hash map from a fixed array of key-value tuples.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "insert",
        kind: "method",
        signature: "fn insert<K, V>(self: &mut Map<K, V>, key: K, value: V) -> Option<V>",
        doc: "Inserts a pair and returns the previous value when present.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "get",
        kind: "method",
        signature: "fn get<K, V>(self: Map<K, V>, key: K) -> Option<V>",
        doc: "Returns the value for a key when present.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "get_or",
        kind: "method",
        signature: "fn get_or<K, V>(self: Map<K, V>, key: K, default: V) -> V",
        doc: "Returns the value for a key or a default.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "or_insert",
        kind: "method",
        signature: "fn or_insert<K, V>(self: &mut Map<K, V>, key: K, default: V) -> V",
        doc: "Returns the existing value or inserts a default.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "remove",
        kind: "method",
        signature: "fn remove<K, V>(self: &mut Map<K, V>, key: K) -> Option<V>",
        doc: "Removes a key and returns its previous value when present.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "pop",
        kind: "method",
        signature: "fn pop<K, V>(self: &mut Map<K, V>, key: K) -> Option<V>",
        doc: "Removes and returns the value for a key when present.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "contains_key",
        kind: "method",
        signature: "fn contains_key<K, V>(self: Map<K, V>, key: K) -> bool",
        doc: "Returns true when the map contains a key.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "contains",
        kind: "method",
        signature: "fn contains<K, V>(self: Map<K, V>, key: K) -> bool",
        doc: "Alias for contains_key.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "len",
        kind: "method",
        signature: "fn len<K, V>(self: Map<K, V>) -> i64",
        doc: "Returns the number of entries.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty<K, V>(self: Map<K, V>) -> bool",
        doc: "Returns true when the map has no entries.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "keys",
        kind: "method",
        signature: "fn keys<K, V>(self: Map<K, V>) -> Vec<K>",
        doc: "Returns all keys.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "values",
        kind: "method",
        signature: "fn values<K, V>(self: Map<K, V>) -> Vec<V>",
        doc: "Returns all values.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "iter",
        kind: "method",
        signature: "fn iter<K, V>(self: Map<K, V>) -> Iterator<(K, V)>",
        doc: "Returns key-value pairs.",
    },
    CoreMethodHelp {
        owner: "Map",
        name: "clear",
        kind: "method",
        signature: "fn clear<K, V>(self: &mut Map<K, V>) -> ()",
        doc: "Removes all entries.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "new",
        kind: "assoc",
        signature: "fn new<K, V>() -> BTreeMap<K, V>",
        doc: "Creates an empty ordered map.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "from",
        kind: "assoc",
        signature: "fn from<K, V, const N: usize>(entries: [(K, V); N]) -> BTreeMap<K, V>",
        doc: "Creates an ordered map from a fixed array of key-value tuples.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "insert",
        kind: "method",
        signature: "fn insert<K, V>(self: &mut BTreeMap<K, V>, key: K, value: V) -> Option<V>",
        doc: "Inserts a pair and returns the previous value when present.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "get",
        kind: "method",
        signature: "fn get<K, V>(self: BTreeMap<K, V>, key: K) -> Option<V>",
        doc: "Returns the value for a key when present.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "get_or",
        kind: "method",
        signature: "fn get_or<K, V>(self: BTreeMap<K, V>, key: K, default: V) -> V",
        doc: "Returns the value for a key or a default.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "or_insert",
        kind: "method",
        signature: "fn or_insert<K, V>(self: &mut BTreeMap<K, V>, key: K, default: V) -> V",
        doc: "Returns the existing value or inserts a default.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "remove",
        kind: "method",
        signature: "fn remove<K, V>(self: &mut BTreeMap<K, V>, key: K) -> Option<V>",
        doc: "Removes a key and returns its previous value when present.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "pop",
        kind: "method",
        signature: "fn pop<K, V>(self: &mut BTreeMap<K, V>, key: K) -> Option<V>",
        doc: "Removes and returns the value for a key when present.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "contains_key",
        kind: "method",
        signature: "fn contains_key<K, V>(self: BTreeMap<K, V>, key: K) -> bool",
        doc: "Returns true when the ordered map contains a key.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "contains",
        kind: "method",
        signature: "fn contains<K, V>(self: BTreeMap<K, V>, key: K) -> bool",
        doc: "Alias for contains_key.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "len",
        kind: "method",
        signature: "fn len<K, V>(self: BTreeMap<K, V>) -> i64",
        doc: "Returns the number of entries.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty<K, V>(self: BTreeMap<K, V>) -> bool",
        doc: "Returns true when the ordered map has no entries.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "keys",
        kind: "method",
        signature: "fn keys<K, V>(self: BTreeMap<K, V>) -> Vec<K>",
        doc: "Returns ordered keys.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "values",
        kind: "method",
        signature: "fn values<K, V>(self: BTreeMap<K, V>) -> Vec<V>",
        doc: "Returns values in key order.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "iter",
        kind: "method",
        signature: "fn iter<K, V>(self: BTreeMap<K, V>) -> Iterator<(K, V)>",
        doc: "Returns key-value pairs in key order.",
    },
    CoreMethodHelp {
        owner: "BTreeMap",
        name: "clear",
        kind: "method",
        signature: "fn clear<K, V>(self: &mut BTreeMap<K, V>) -> ()",
        doc: "Removes all entries.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "new",
        kind: "assoc",
        signature: "fn new<T>() -> Set<T>",
        doc: "Creates an empty hash set.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "from",
        kind: "assoc",
        signature: "fn from<T, const N: usize>(values: [T; N]) -> Set<T>",
        doc: "Creates a hash set from a collection, removing duplicate values.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "insert",
        kind: "method",
        signature: "fn insert<T>(self: &mut Set<T>, value: T) -> bool",
        doc: "Adds a value to the set.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "remove",
        kind: "method",
        signature: "fn remove<T>(self: &mut Set<T>, value: T) -> bool",
        doc: "Removes a value from the set.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "contains",
        kind: "method",
        signature: "fn contains<T>(self: Set<T>, value: T) -> bool",
        doc: "Returns true when the set contains a value.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "union",
        kind: "method",
        signature: "fn union<T>(self: Set<T>, other: Set<T>) -> Set<T>",
        doc: "Returns the union of two sets.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "intersection",
        kind: "method",
        signature: "fn intersection<T>(self: Set<T>, other: Set<T>) -> Set<T>",
        doc: "Returns the intersection of two sets.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "difference",
        kind: "method",
        signature: "fn difference<T>(self: Set<T>, other: Set<T>) -> Set<T>",
        doc: "Returns values present only in the receiver.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "symmetric_difference",
        kind: "method",
        signature: "fn symmetric_difference<T>(self: Set<T>, other: Set<T>) -> Set<T>",
        doc: "Returns values present in exactly one set.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "len",
        kind: "method",
        signature: "fn len<T>(self: Set<T>) -> i64",
        doc: "Returns the number of values.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty<T>(self: Set<T>) -> bool",
        doc: "Returns true when the set has no values.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "clear",
        kind: "method",
        signature: "fn clear<T>(self: &mut Set<T>) -> ()",
        doc: "Removes every value.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "iter",
        kind: "method",
        signature: "fn iter<T>(self: Set<T>) -> Iterator<T>",
        doc: "Returns a deterministic snapshot suitable for iterator methods.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "to_vec",
        kind: "method",
        signature: "fn to_vec<T>(self: Set<T>) -> Vec<T>",
        doc: "Returns the values in deterministic order.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "is_subset",
        kind: "method",
        signature: "fn is_subset<T>(self: Set<T>, other: Set<T>) -> bool",
        doc: "Returns true when every value is present in the other set.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "is_superset",
        kind: "method",
        signature: "fn is_superset<T>(self: Set<T>, other: Set<T>) -> bool",
        doc: "Returns true when the set contains every value from the other set.",
    },
    CoreMethodHelp {
        owner: "Set",
        name: "is_disjoint",
        kind: "method",
        signature: "fn is_disjoint<T>(self: Set<T>, other: Set<T>) -> bool",
        doc: "Returns true when the sets have no values in common.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "new",
        kind: "assoc",
        signature: "fn new<T>() -> BTreeSet<T>",
        doc: "Creates an empty ordered set.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "from",
        kind: "assoc",
        signature: "fn from<T, const N: usize>(values: [T; N]) -> BTreeSet<T>",
        doc: "Creates an ordered set from a collection, removing duplicate values.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "insert",
        kind: "method",
        signature: "fn insert<T>(self: &mut BTreeSet<T>, value: T) -> bool",
        doc: "Adds a value to the set.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "remove",
        kind: "method",
        signature: "fn remove<T>(self: &mut BTreeSet<T>, value: T) -> bool",
        doc: "Removes a value from the set.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "contains",
        kind: "method",
        signature: "fn contains<T>(self: BTreeSet<T>, value: T) -> bool",
        doc: "Returns true when the set contains a value.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "union",
        kind: "method",
        signature: "fn union<T>(self: BTreeSet<T>, other: BTreeSet<T>) -> BTreeSet<T>",
        doc: "Returns the union of two sets.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "intersection",
        kind: "method",
        signature: "fn intersection<T>(self: BTreeSet<T>, other: BTreeSet<T>) -> BTreeSet<T>",
        doc: "Returns the intersection of two sets.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "difference",
        kind: "method",
        signature: "fn difference<T>(self: BTreeSet<T>, other: BTreeSet<T>) -> BTreeSet<T>",
        doc: "Returns values present only in the receiver.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "symmetric_difference",
        kind: "method",
        signature: "fn symmetric_difference<T>(self: BTreeSet<T>, other: BTreeSet<T>) -> BTreeSet<T>",
        doc: "Returns values present in exactly one set.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "len",
        kind: "method",
        signature: "fn len<T>(self: BTreeSet<T>) -> i64",
        doc: "Returns the number of values.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty<T>(self: BTreeSet<T>) -> bool",
        doc: "Returns true when the set has no values.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "clear",
        kind: "method",
        signature: "fn clear<T>(self: &mut BTreeSet<T>) -> ()",
        doc: "Removes every value.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "iter",
        kind: "method",
        signature: "fn iter<T>(self: BTreeSet<T>) -> Iterator<T>",
        doc: "Returns a deterministic snapshot suitable for iterator methods.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "to_vec",
        kind: "method",
        signature: "fn to_vec<T>(self: BTreeSet<T>) -> Vec<T>",
        doc: "Returns the values in deterministic order.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "is_subset",
        kind: "method",
        signature: "fn is_subset<T>(self: BTreeSet<T>, other: BTreeSet<T>) -> bool",
        doc: "Returns true when every value is present in the other set.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "is_superset",
        kind: "method",
        signature: "fn is_superset<T>(self: BTreeSet<T>, other: BTreeSet<T>) -> bool",
        doc: "Returns true when the set contains every value from the other set.",
    },
    CoreMethodHelp {
        owner: "BTreeSet",
        name: "is_disjoint",
        kind: "method",
        signature: "fn is_disjoint<T>(self: BTreeSet<T>, other: BTreeSet<T>) -> bool",
        doc: "Returns true when the sets have no values in common.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "new",
        kind: "assoc",
        signature: "fn new() -> Deque<i64>",
        doc: "Creates an empty double-ended queue.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "from",
        kind: "assoc",
        signature: "fn from<const N: usize>(values: [i64; N]) -> Deque<i64>",
        doc: "Creates a deque from values in front-to-back order.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "push_back",
        kind: "method",
        signature: "fn push_back(self: &mut Deque<i64>, value: i64) -> ()",
        doc: "Appends a value to the back.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "push_front",
        kind: "method",
        signature: "fn push_front(self: &mut Deque<i64>, value: i64) -> ()",
        doc: "Appends a value to the front.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "pop_front",
        kind: "method",
        signature: "fn pop_front(self: &mut Deque<i64>) -> Option<i64>",
        doc: "Removes and returns the front value when present.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "pop_back",
        kind: "method",
        signature: "fn pop_back(self: &mut Deque<i64>) -> Option<i64>",
        doc: "Removes and returns the back value when present.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "peek_front",
        kind: "method",
        signature: "fn peek_front(self: Deque<i64>) -> Option<i64>",
        doc: "Returns the front value without removing it.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "peek_back",
        kind: "method",
        signature: "fn peek_back(self: Deque<i64>) -> Option<i64>",
        doc: "Returns the back value without removing it.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "len",
        kind: "method",
        signature: "fn len(self: Deque<i64>) -> i64",
        doc: "Returns the number of values.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty(self: Deque<i64>) -> bool",
        doc: "Returns true when the deque has no values.",
    },
    CoreMethodHelp {
        owner: "Deque",
        name: "clear",
        kind: "method",
        signature: "fn clear(self: &mut Deque<i64>) -> ()",
        doc: "Removes all values.",
    },
    CoreMethodHelp {
        owner: "Queue",
        name: "new",
        kind: "assoc",
        signature: "fn new() -> Queue<i64>",
        doc: "Creates an empty FIFO queue.",
    },
    CoreMethodHelp {
        owner: "Queue",
        name: "from",
        kind: "assoc",
        signature: "fn from<const N: usize>(values: [i64; N]) -> Queue<i64>",
        doc: "Creates a FIFO queue from values in front-to-back order. The literal spelling is <[a, b].",
    },
    CoreMethodHelp {
        owner: "Queue",
        name: "push",
        kind: "method",
        signature: "fn push(self: &mut Queue<i64>, value: i64) -> ()",
        doc: "Appends a value to the back of the queue.",
    },
    CoreMethodHelp {
        owner: "Queue",
        name: "pop",
        kind: "method",
        signature: "fn pop(self: &mut Queue<i64>) -> Option<i64>",
        doc: "Removes and returns the front value when present.",
    },
    CoreMethodHelp {
        owner: "Queue",
        name: "peek",
        kind: "method",
        signature: "fn peek(self: Queue<i64>) -> Option<i64>",
        doc: "Returns the front value without removing it.",
    },
    CoreMethodHelp {
        owner: "Queue",
        name: "len",
        kind: "method",
        signature: "fn len(self: Queue<i64>) -> i64",
        doc: "Returns the number of values.",
    },
    CoreMethodHelp {
        owner: "Queue",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty(self: Queue<i64>) -> bool",
        doc: "Returns true when the queue has no values.",
    },
    CoreMethodHelp {
        owner: "Queue",
        name: "clear",
        kind: "method",
        signature: "fn clear(self: &mut Queue<i64>) -> ()",
        doc: "Removes all values.",
    },
    CoreMethodHelp {
        owner: "Stack",
        name: "new",
        kind: "assoc",
        signature: "fn new() -> Stack<i64>",
        doc: "Creates an empty LIFO stack.",
    },
    CoreMethodHelp {
        owner: "Stack",
        name: "from",
        kind: "assoc",
        signature: "fn from<const N: usize>(values: [i64; N]) -> Stack<i64>",
        doc: "Creates a LIFO stack from values in bottom-to-top order. The literal spelling is [a, b]>.",
    },
    CoreMethodHelp {
        owner: "Stack",
        name: "push",
        kind: "method",
        signature: "fn push(self: &mut Stack<i64>, value: i64) -> ()",
        doc: "Appends a value to the top of the stack.",
    },
    CoreMethodHelp {
        owner: "Stack",
        name: "pop",
        kind: "method",
        signature: "fn pop(self: &mut Stack<i64>) -> Option<i64>",
        doc: "Removes and returns the top value when present.",
    },
    CoreMethodHelp {
        owner: "Stack",
        name: "peek",
        kind: "method",
        signature: "fn peek(self: Stack<i64>) -> Option<i64>",
        doc: "Returns the top value without removing it.",
    },
    CoreMethodHelp {
        owner: "Stack",
        name: "len",
        kind: "method",
        signature: "fn len(self: Stack<i64>) -> i64",
        doc: "Returns the number of values.",
    },
    CoreMethodHelp {
        owner: "Stack",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty(self: Stack<i64>) -> bool",
        doc: "Returns true when the stack has no values.",
    },
    CoreMethodHelp {
        owner: "Stack",
        name: "clear",
        kind: "method",
        signature: "fn clear(self: &mut Stack<i64>) -> ()",
        doc: "Removes all values.",
    },
    CoreMethodHelp {
        owner: "MaxHeap",
        name: "new",
        kind: "assoc",
        signature: "fn new<T>() -> MaxHeap<T>",
        doc: "Creates an empty max heap.",
    },
    CoreMethodHelp {
        owner: "MaxHeap",
        name: "from",
        kind: "assoc",
        signature: "fn from<T, const N: usize>(values: [T; N]) -> MaxHeap<T>",
        doc: "Creates a max heap. The literal spelling is ^[a, b].",
    },
    CoreMethodHelp {
        owner: "MaxHeap",
        name: "push",
        kind: "method",
        signature: "fn push<T>(self: &mut MaxHeap<T>, value: T) -> ()",
        doc: "Pushes a value onto the max heap.",
    },
    CoreMethodHelp {
        owner: "MaxHeap",
        name: "pop",
        kind: "method",
        signature: "fn pop<T>(self: &mut MaxHeap<T>) -> Option<T>",
        doc: "Removes and returns the largest value when present.",
    },
    CoreMethodHelp {
        owner: "MaxHeap",
        name: "peek",
        kind: "method",
        signature: "fn peek<T>(self: MaxHeap<T>) -> Option<T>",
        doc: "Returns the largest value without removing it.",
    },
    CoreMethodHelp {
        owner: "MaxHeap",
        name: "len",
        kind: "method",
        signature: "fn len<T>(self: MaxHeap<T>) -> i64",
        doc: "Returns the number of values.",
    },
    CoreMethodHelp {
        owner: "MaxHeap",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty<T>(self: MaxHeap<T>) -> bool",
        doc: "Returns true when the heap has no values.",
    },
    CoreMethodHelp {
        owner: "MaxHeap",
        name: "clear",
        kind: "method",
        signature: "fn clear<T>(self: &mut MaxHeap<T>) -> ()",
        doc: "Removes all values.",
    },
    CoreMethodHelp {
        owner: "MinHeap",
        name: "new",
        kind: "assoc",
        signature: "fn new<T>() -> MinHeap<T>",
        doc: "Creates an empty min heap.",
    },
    CoreMethodHelp {
        owner: "MinHeap",
        name: "from",
        kind: "assoc",
        signature: "fn from<T, const N: usize>(values: [T; N]) -> MinHeap<T>",
        doc: "Creates a min heap. The literal spelling is _[a, b].",
    },
    CoreMethodHelp {
        owner: "MinHeap",
        name: "push",
        kind: "method",
        signature: "fn push<T>(self: &mut MinHeap<T>, value: T) -> ()",
        doc: "Pushes a value onto the min heap.",
    },
    CoreMethodHelp {
        owner: "MinHeap",
        name: "pop",
        kind: "method",
        signature: "fn pop<T>(self: &mut MinHeap<T>) -> Option<T>",
        doc: "Removes and returns the smallest value when present.",
    },
    CoreMethodHelp {
        owner: "MinHeap",
        name: "peek",
        kind: "method",
        signature: "fn peek<T>(self: MinHeap<T>) -> Option<T>",
        doc: "Returns the smallest value without removing it.",
    },
    CoreMethodHelp {
        owner: "MinHeap",
        name: "len",
        kind: "method",
        signature: "fn len<T>(self: MinHeap<T>) -> i64",
        doc: "Returns the number of values.",
    },
    CoreMethodHelp {
        owner: "MinHeap",
        name: "is_empty",
        kind: "method",
        signature: "fn is_empty<T>(self: MinHeap<T>) -> bool",
        doc: "Returns true when the heap has no values.",
    },
    CoreMethodHelp {
        owner: "MinHeap",
        name: "clear",
        kind: "method",
        signature: "fn clear<T>(self: &mut MinHeap<T>) -> ()",
        doc: "Removes all values.",
    },
    CoreMethodHelp {
        owner: "Option",
        name: "unwrap",
        kind: "method",
        signature: "fn unwrap<T>(self: Option<T>) -> T",
        doc: "Returns the payload, panicking when the value is None.",
    },
    CoreMethodHelp {
        owner: "Option",
        name: "expect",
        kind: "method",
        signature: "fn expect<T>(self: Option<T>, message: String) -> T",
        doc: "Returns the payload, panicking with the message when the value is None.",
    },
    CoreMethodHelp {
        owner: "Option",
        name: "ok_or",
        kind: "method",
        signature: "fn ok_or<T, E>(self: Option<T>, err: E) -> Result<T, E>",
        doc: "Converts Some to Ok, or None to Err with the provided error.",
    },
    CoreMethodHelp {
        owner: "Option",
        name: "ok_or_else",
        kind: "method",
        signature: "fn ok_or_else<T, E>(self: Option<T>, err: fn() -> E) -> Result<T, E>",
        doc: "Converts Some to Ok, or None to Err from a fallback closure.",
    },
    CoreMethodHelp {
        owner: "Result",
        name: "map",
        kind: "method",
        signature: "fn map<T, U, E>(self: Result<T, E>, f: fn(T) -> U) -> Result<U, E>",
        doc: "Maps Ok through a closure and leaves Err unchanged.",
    },
    CoreMethodHelp {
        owner: "Result",
        name: "map_err",
        kind: "method",
        signature: "fn map_err<T, E, F>(self: Result<T, E>, f: fn(E) -> F) -> Result<T, F>",
        doc: "Maps Err through a closure and leaves Ok unchanged.",
    },
    CoreMethodHelp {
        owner: "Result",
        name: "is_ok",
        kind: "method",
        signature: "fn is_ok<T, E>(self: Result<T, E>) -> bool",
        doc: "Returns true for Ok.",
    },
    CoreMethodHelp {
        owner: "Result",
        name: "is_err",
        kind: "method",
        signature: "fn is_err<T, E>(self: Result<T, E>) -> bool",
        doc: "Returns true for Err.",
    },
    CoreMethodHelp {
        owner: "Result",
        name: "unwrap",
        kind: "method",
        signature: "fn unwrap<T, E>(self: Result<T, E>) -> T",
        doc: "Returns the Ok payload, panicking when the value is Err.",
    },
    CoreMethodHelp {
        owner: "Result",
        name: "expect",
        kind: "method",
        signature: "fn expect<T, E>(self: Result<T, E>, message: String) -> T",
        doc: "Returns the Ok payload, panicking with the message when the value is Err.",
    },
];

#[allow(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "REPL loop bundles input, completion, history, and graceful-exit handling"
)]
pub(crate) fn cmd_repl(verbose: bool) -> Result<()> {
    use rustyline::error::ReadlineError;
    use rustyline::history::FileHistory;
    use rustyline::{ColorMode, CompletionType, Config, EditMode, Editor, EventHandler, KeyEvent};

    use crate::repl_helper::{GosReplHelper, ReplEnterHandler};

    println!(
        "gos {version} REPL [{arch}-{os}]\n\
         %help for commands · Enter continues until braces close · Ctrl-D or %q exits",
        version = env!("CARGO_PKG_VERSION"),
        arch = std::env::consts::ARCH,
        os = std::env::consts::OS,
    );

    let mut transcript: Vec<String> = Vec::new();
    let mut declarations: Vec<String> = Vec::new();
    let mut lets: Vec<String> = Vec::new();
    let mut bindings: Vec<ReplBinding> = Vec::new();
    let mut input_no = 1u32;

    let config = Config::builder()
        .edit_mode(EditMode::Emacs)
        .color_mode(ColorMode::Enabled)
        .completion_type(CompletionType::List)
        .auto_add_history(false)
        .build();
    let mut editor: Editor<GosReplHelper, FileHistory> =
        Editor::with_config(config).map_err(|e| anyhow!("repl init: {e}"))?;
    editor.set_helper(Some(GosReplHelper::new()));
    editor.bind_sequence(
        KeyEvent::from('\r'),
        EventHandler::Conditional(Box::new(ReplEnterHandler)),
    );
    let history_path = repl_history_path();
    if let Some(path) = &history_path {
        let _ = editor.load_history(path);
    }
    transcript.extend(editor.history().iter().cloned());

    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    if tty {
        crate::style::force_enable();
    }
    loop {
        let prompt = if tty {
            "\x1b[32m>>>\x1b[0m ".to_string()
        } else {
            ">>> ".to_string()
        };
        let line = match editor.readline(&prompt) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => {
                eprintln!("KeyboardInterrupt");
                continue;
            }
            Err(ReadlineError::Eof) => {
                if let Some(path) = &history_path {
                    let _ = editor.save_history(path);
                }
                gossamer_std::exec::terminate_live_children();
                println!();
                return Ok(());
            }
            Err(err) => {
                print_repl_error(&format!("repl: {err}"));
                gossamer_std::exec::terminate_live_children();
                return Ok(());
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // History searches must run against earlier inputs only. Recording
        // `%history pattern` first would make it match its own pattern.
        if let Some(rest) = trimmed.strip_prefix('%') {
            let (command, arg) = split_meta_command(rest.trim());
            if matches!(command, "history" | "h") {
                match render_repl_history(&transcript, arg) {
                    Ok(entries) => {
                        for entry in entries {
                            println!("{}", crate::style::repl_meta_accent(&entry));
                        }
                    }
                    Err(message) => print_repl_error(&message),
                }
                let _ = editor.add_history_entry(trimmed);
                transcript.push(trimmed.to_string());
                continue;
            }
            // Clearing is deliberately handled before recording the current
            // meta-command. It clears both the in-memory editor navigation
            // and the persistent transcript, so `%h` immediately after it is
            // empty and a later REPL session cannot resurrect old entries.
            if command == "clear-history" {
                if !arg.is_empty() {
                    print_repl_error("usage: %clear-history");
                    continue;
                }
                if let Err(err) = editor.clear_history() {
                    print_repl_error(&format!("clear history: {err}"));
                    continue;
                }
                transcript.clear();
                if let Some(path) = &history_path
                    && let Err(err) = std::fs::remove_file(path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    print_repl_error(&format!("clear history: {err}"));
                    continue;
                }
                println!("history cleared");
                continue;
            }
        }
        let _ = editor.add_history_entry(trimmed);
        transcript.push(trimmed.to_string());

        // Meta-commands first.
        if let Some(rest) = trimmed.strip_prefix('%') {
            let rest = rest.trim();
            let (command, arg) = split_meta_command(rest);
            match command {
                "quit" | "q" => {
                    if let Some(path) = &history_path {
                        let _ = editor.save_history(path);
                    }
                    // A program run here may have started children of its own;
                    // the session that started them is what ends them.
                    gossamer_std::exec::terminate_live_children();
                    return Ok(());
                }
                "bindings" | "b" => {
                    let options = match parse_listing_options("bindings", arg) {
                        Ok(options) => options,
                        Err(message) => {
                            print_repl_error(&message);
                            continue;
                        }
                    };
                    if bindings.is_empty() {
                        println!(
                            "{}",
                            crate::style::repl_meta_detail("    no `let` bindings yet")
                        );
                    } else {
                        let pattern = if options.pattern.is_empty() {
                            None
                        } else {
                            match compile_search_regex("bindings", &options.pattern) {
                                Ok(pattern) => Some(pattern),
                                Err(message) => {
                                    print_repl_error(&message);
                                    continue;
                                }
                            }
                        };
                        let entries = render_repl_bindings(&declarations, &lets, &bindings);
                        let matches = entries
                            .iter()
                            .filter(|entry| {
                                pattern.as_ref().is_none_or(|re| re.is_match(&entry.name))
                            })
                            .collect::<Vec<_>>();
                        if matches.is_empty() {
                            println!(
                                "{}",
                                crate::style::repl_meta_detail(&format!(
                                    "    no bindings match `{}`",
                                    options.pattern
                                ))
                            );
                            continue;
                        }
                        for entry in matches {
                            println!("{}", crate::style::repl_meta_heading(&entry.line));
                        }
                    }
                    continue;
                }
                "drop" => {
                    let mut words = arg.split_whitespace();
                    let Some(name) = words.next() else {
                        print_repl_error("usage: %drop NAME");
                        continue;
                    };
                    if words.next().is_some() {
                        print_repl_error("usage: %drop NAME");
                        continue;
                    }
                    let Some(drop_plan) = prepare_repl_drop(&lets, &bindings, name) else {
                        // A name the session declares rather than binds ends the
                        // whole declaration that introduced it. Redeclaring a name
                        // is rejected as a duplicate definition, so ending the
                        // declaration is what makes a name reusable.
                        match prepare_repl_declaration_drop(&declarations, name) {
                            Some(plan) => {
                                let probe_body = format!("{}()\n", render_repl_setup(&lets));
                                let entry = format!("__irepl_drop_{input_no}");
                                let probe = format!(
                                    "{}\nfn {entry}() {{\n    {probe_body}}}\n",
                                    render_repl_declarations(&plan.declarations),
                                );
                                match rebuild_session(&plan.declarations)
                                    .and_then(|()| build_and_call(&probe, &entry).map(|_| ()))
                                {
                                    Ok(()) => {
                                        declarations = plan.declarations;
                                        if let Some(helper) = editor.helper_mut() {
                                            for dropped in &plan.dropped_names {
                                                helper.forget_binding(dropped);
                                            }
                                            helper.set_declarations(&declarations);
                                        }
                                        let dropped =
                                            render_dropped_declaration_names(&plan.dropped_names);
                                        println!(
                                            "{}",
                                            crate::style::repl_meta_accent(&format!(
                                                "dropped {dropped}"
                                            ))
                                        );
                                    }
                                    Err(message) => print_repl_error(&format!(
                                        "cannot drop `{name}`: the rest of the session still depends on it:\n{message}"
                                    )),
                                }
                            }
                            None => print_repl_error(&format!(
                                "no persistent binding or declaration named `{name}`"
                            )),
                        }
                        continue;
                    };
                    let probe_body = format!("{}()\n", render_repl_setup(&drop_plan.lets));
                    let entry = format!("__irepl_drop_{input_no}");
                    let probe = format!(
                        "{}\nfn {entry}() {{\n    {probe_body}}}\n",
                        render_repl_declarations(&declarations),
                    );
                    match build_and_call(&probe, &entry) {
                        Ok(_) => {
                            lets = drop_plan.lets;
                            bindings = drop_plan.bindings;
                            if let Some(helper) = editor.helper_mut() {
                                for dropped in &drop_plan.dropped_names {
                                    helper.forget_binding(dropped);
                                }
                            }
                            let dropped = render_dropped_binding_names(&drop_plan.dropped_names);
                            println!(
                                "{}",
                                crate::style::repl_meta_accent(&format!("dropped {dropped}"))
                            );
                        }
                        Err(message) => print_repl_error(&format!(
                            "cannot drop `{name}`: remaining REPL bindings could not be replayed after ending it:\n{message}"
                        )),
                    }
                    continue;
                }
                "declarations" | "decls" | "d" => {
                    let options = match parse_listing_options("declarations", arg) {
                        Ok(options) => options,
                        Err(message) => {
                            print_repl_error(&message);
                            continue;
                        }
                    };
                    if declarations.is_empty() {
                        println!(
                            "{}",
                            crate::style::repl_meta_detail("    no declarations yet")
                        );
                    } else {
                        let pattern = if options.pattern.is_empty() {
                            None
                        } else {
                            match compile_search_regex("declarations", &options.pattern) {
                                Ok(pattern) => Some(pattern),
                                Err(message) => {
                                    print_repl_error(&message);
                                    continue;
                                }
                            }
                        };
                        let matches = declarations
                            .iter()
                            .filter(|declaration| {
                                pattern.as_ref().is_none_or(|re| {
                                    declaration_names(declaration)
                                        .into_iter()
                                        .any(|name| re.is_match(&name))
                                })
                            })
                            .collect::<Vec<_>>();
                        if matches.is_empty() {
                            println!(
                                "{}",
                                crate::style::repl_meta_detail(&format!(
                                    "    no declarations match `{}`",
                                    options.pattern
                                ))
                            );
                            continue;
                        }
                        for line in matches {
                            println!("{}", crate::style::repl_meta_heading(line));
                        }
                    }
                    continue;
                }
                "reset" | "r" => {
                    declarations.clear();
                    lets.clear();
                    bindings.clear();
                    if let Some(helper) = editor.helper_mut() {
                        helper.reset_session();
                    }
                    println!("{}", crate::style::repl_meta_accent("session cleared"));
                    continue;
                }
                "help" => {
                    if arg.is_empty() {
                        print_repl_output(REPL_HELP_TEXT);
                    } else {
                        print_repl_error("usage: %help");
                    }
                    continue;
                }
                "info" | "i" => {
                    let options = match parse_listing_options("info", arg) {
                        Ok(options) => options,
                        Err(message) => {
                            print_repl_error(&message);
                            continue;
                        }
                    };
                    let session = session_index(&declarations);
                    let session_text = repl_session_info(&session, &options.pattern);
                    let catalog = if options.details {
                        repl_info(&options.pattern)
                    } else {
                        repl_info_listing(&options.pattern)
                    }
                    .map(|text| render_info(text, &options));
                    // A session `impl` on a catalog type adds to what the
                    // catalog knows rather than standing in for it.
                    let result = match (session_text, catalog) {
                        (Some(session), Ok(catalog)) if !is_nothing_found(&catalog) => {
                            Ok(splice_session_into_catalog(&session, &catalog))
                        }
                        (Some(session), _) => Ok(session),
                        (None, catalog) => catalog,
                    };
                    match result {
                        Ok(text) => print_repl_output(&text),
                        Err(msg) => print_repl_error(&msg),
                    }
                    continue;
                }
                "explain" | "e" => {
                    let options = match parse_listing_options("explain", arg) {
                        Ok(options) => options,
                        Err(message) => {
                            print_repl_error(&message);
                            continue;
                        }
                    };
                    if options.pattern.is_empty() {
                        print_repl_error("usage: %explain NAME [-d|--details]");
                        continue;
                    }
                    let result = if options.details {
                        repl_binding_info(&declarations, &lets, &bindings, &options.pattern)
                    } else {
                        repl_binding_listing(&declarations, &lets, &bindings, &options.pattern)
                    };
                    match result {
                        Some(Ok(text)) => print_repl_output(&text),
                        Some(Err(msg)) => print_repl_error(&msg),
                        None => {
                            match repl_session_info(&session_index(&declarations), &options.pattern)
                                .or_else(|| repl_declaration_info(&declarations, &options.pattern))
                            {
                                Some(text) => print_repl_output(&text),
                                None => print_repl_error(&format!(
                                    "no persistent binding or declaration named `{}`",
                                    options.pattern
                                )),
                            }
                        }
                    }
                    continue;
                }
                _ => {
                    print_repl_error(&format!("unknown meta-command: %{rest}"));
                    continue;
                }
            }
        }

        let is_declaration = input_is_declaration(trimmed);

        if is_declaration {
            declarations.push(trimmed.to_string());
            match rebuild_session(&declarations) {
                Ok(()) => {
                    if let Some(helper) = editor.helper_mut() {
                        helper.set_declarations(&declarations);
                    }
                    if verbose {
                        println!("    added {} declarations", declarations.len());
                    }
                }
                Err(msg) => {
                    declarations.pop();
                    print_repl_error(&msg);
                }
            }
            input_no += 1;
            continue;
        }

        if trimmed.starts_with("let ") {
            let candidate = trimmed.to_string();
            let mut new_binding = match repl_binding_from_let_source(&candidate) {
                Ok(binding) => binding,
                Err(msg) => {
                    print_repl_error(&msg);
                    input_no += 1;
                    continue;
                }
            };
            let probe_body = format!("{}{candidate}\n    ()\n", render_repl_setup(&lets));
            let probe = format!(
                "{}\nfn __irepl_{n}() {{\n    {body}}}\n",
                render_repl_declarations(&declarations),
                n = input_no,
                body = probe_body,
            );
            match build_and_call(&probe, &format!("__irepl_{input_no}")) {
                Ok(_) => {
                    new_binding.source_index = lets.len();
                    let completion_vars = new_binding.vars.clone();
                    update_repl_bindings(&mut bindings, new_binding);
                    lets.push(candidate.clone());
                    let completion_surfaces = completion_vars
                        .iter()
                        .map(|var| {
                            let surface = infer_repl_binding_type(&declarations, &lets, &var.name)
                                .ok()
                                .map(|ty| BindingSurface::of(var, &ty));
                            (var.name.clone(), surface)
                        })
                        .collect::<Vec<_>>();
                    if let Some(helper) = editor.helper_mut() {
                        for (name, surface) in completion_surfaces {
                            helper.set_binding_surface(&name, surface);
                        }
                    }
                    if verbose {
                        println!("    binding added ({} total)", bindings.len());
                    }
                }
                Err(msg) => {
                    print_repl_error(&msg);
                }
            }
            input_no += 1;
            continue;
        }

        // Assignments and collection mutation calls must be replayed with the
        // preceding bindings so their effects survive into later inputs.
        let user_mutating_methods = collect_repl_mut_self_method_names(&declarations);
        if input_mutates_binding(trimmed, &user_mutating_methods) {
            let probe_body = format!("{}{trimmed}", render_repl_setup(&lets));
            let probe = format!(
                "{}\nfn __irepl_{n}() {{\n    {body}}}\n",
                render_repl_declarations(&declarations),
                n = input_no,
                body = probe_body,
            );
            match build_and_call_with_type(&probe, &format!("__irepl_{input_no}")) {
                Ok((value, ty)) => {
                    if matches!(value, gossamer_interp::Value::Unit) {
                        lets.push(trimmed.to_string());
                    } else {
                        print_repl_result(&value, &ty);
                        // The call is a tail expression in the probe so its
                        // Result can be displayed. On later inputs it becomes
                        // a statement, where an unused Result is rightly a
                        // type error. Replay it with an explicit discard so
                        // its receiver mutation persists without poisoning
                        // every subsequent binding and `%b` inspection.
                        lets.push(format!("let _ = {trimmed}"));
                    }
                }
                Err(msg) => print_repl_error(&msg),
            }
            input_no += 1;
            continue;
        }

        let let_body = render_repl_setup(&lets);
        let program_source = format!(
            "{}\nfn __irepl_{n}() {{ {lets}{expr}\n}}\n",
            render_repl_declarations(&declarations),
            n = input_no,
            lets = let_body,
            expr = trimmed,
        );
        match build_and_call_with_type(&program_source, &format!("__irepl_{input_no}")) {
            Ok((value, ty)) => {
                if !matches!(value, gossamer_interp::Value::Unit) {
                    print_repl_result(&value, &ty);
                }
            }
            Err(msg) => {
                print_repl_error(&msg);
            }
        }
        input_no += 1;
    }
}

/// REPL results use source-like representation, while explicit `print` and
/// `println` retain `Display` formatting. This keeps a bare string distinct
/// from an identifier and applies recursively to aggregate values.
fn render_repl_value(value: &gossamer_interp::Value) -> String {
    value.repr()
}

struct ReplValueType {
    rendered: String,
    /// How a value of this type renders, when the value alone cannot
    /// say: a `Vec` and a fixed array share one runtime representation,
    /// and a `u64` shares a slot with an `i64`. This is the descriptor
    /// the bytecode compiler builds for a `println!` of the same value,
    /// so the REPL and a program show one value the same way.
    render_desc: Option<String>,
    references: Vec<gossamer_types::Mutbl>,
    method_owner: Option<String>,
    fixed_array: bool,
    /// Rendered element types when the binding is a tuple. A tuple owns
    /// no methods, so its positional elements are the surface `%explain`
    /// and `%info` have to report.
    tuple_elements: Vec<String>,
}

impl ReplValueType {
    fn from_ty(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> Self {
        let rendered = gossamer_types::render_public_ty(tcx, ty);
        let mut references = Vec::new();
        let mut current = ty;
        while let Some(gossamer_types::TyKind::Ref { mutability, inner }) = tcx.kind(current) {
            references.push(*mutability);
            current = *inner;
        }
        let (method_owner, fixed_array) = match tcx.kind(current) {
            Some(gossamer_types::TyKind::Array { .. }) => (Some("Array".to_string()), true),
            Some(gossamer_types::TyKind::Slice(_)) => (Some("Slice".to_string()), true),
            Some(gossamer_types::TyKind::Vec(_)) => (Some("Vec".to_string()), false),
            Some(gossamer_types::TyKind::String) => (Some("String".to_string()), false),
            Some(gossamer_types::TyKind::HashMap { .. }) => (Some("Map".to_string()), false),
            Some(gossamer_types::TyKind::Iterator(_)) => (Some("Iterator".to_string()), false),
            Some(gossamer_types::TyKind::Sender(_)) => (Some("Sender".to_string()), false),
            Some(gossamer_types::TyKind::Receiver(_)) => (Some("Receiver".to_string()), false),
            Some(gossamer_types::TyKind::JoinHandle(_)) => (Some("JoinHandle".to_string()), false),
            Some(gossamer_types::TyKind::Duration) => (Some("Duration".to_string()), false),
            Some(gossamer_types::TyKind::Instant) => (Some("Instant".to_string()), false),
            Some(gossamer_types::TyKind::Tuple(_)) => (Some("Tuple".to_string()), false),
            Some(gossamer_types::TyKind::Adt { def, .. }) => {
                (tcx.def_name(*def).map(str::to_string), false)
            }
            _ => (None, false),
        };
        let tuple_elements = match tcx.kind(current) {
            Some(gossamer_types::TyKind::Tuple(elems)) => elems
                .clone()
                .iter()
                .map(|elem| gossamer_types::render_public_ty(tcx, *elem))
                .collect(),
            _ => Vec::new(),
        };
        Self {
            rendered,
            render_desc: gossamer_interp::value::repl_render_descriptor(tcx, ty),
            references,
            method_owner,
            fixed_array,
            tuple_elements,
        }
    }

    fn unknown() -> Self {
        Self {
            rendered: "<unknown>".to_string(),
            render_desc: None,
            references: Vec::new(),
            method_owner: None,
            fixed_array: false,
            tuple_elements: Vec::new(),
        }
    }
}

fn render_repl_binding_value(value: &gossamer_interp::Value, ty: &ReplValueType) -> String {
    // An iterator is a cursor over a sequence it has not walked, so it shows
    // as one whatever the state behind it is: printing the elements would
    // claim a walk the binding has not made, and reading them is what `%b`
    // must not do to a single-use value.
    if ty.rendered.starts_with("Iterator<") {
        return "<iterator>".to_string();
    }
    // Rendered through the binding's own type, so a `Vec` shows in its
    // own spelling at every depth and a fixed array keeps the bare
    // brackets - the two share one runtime representation, so the value
    // alone cannot say which it is. This is the descriptor the bytecode
    // compiler builds for a `println!` of the same value.
    let mut rendered = match &ty.render_desc {
        Some(desc) => gossamer_interp::value::uint_leaves(value, desc.as_bytes()).repr(),
        None => render_repl_value(value),
    };
    for mutability in ty.references.iter().rev() {
        rendered = format!("{}{rendered}", mutability.prefix());
    }
    rendered
}

/// Prints an expression's value in the spelling its type is written in, the
/// same one `%bindings` shows for a binding of that type.
fn print_repl_result(value: &gossamer_interp::Value, ty: &ReplValueType) {
    println!("{}", render_repl_binding_value(value, ty));
    std::io::stdout()
        .flush()
        .expect("flush REPL expression result");
}

/// The session's declarations as one source block, imports first.
///
/// A file's `use` declarations precede its items, and the prompt accepts
/// them in whatever order the session reached them, so the block the REPL
/// assembles puts them back in the order the grammar states.
fn render_repl_declarations(declarations: &[String]) -> String {
    let (imports, items): (Vec<&String>, Vec<&String>) = declarations
        .iter()
        .partition(|declaration| declaration.trim_start().starts_with("use "));
    imports
        .into_iter()
        .chain(items)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_repl_setup(lets: &[String]) -> String {
    if lets.is_empty() {
        String::new()
    } else {
        let replay = lets
            .iter()
            .map(|line| suppress_replayed_prints(line))
            .collect::<Vec<_>>()
            .join("\n    ");
        format!("{replay}\n    ")
    }
}

fn suppress_replayed_prints(input: &str) -> String {
    input
        .replace("println(", "format(")
        .replace("eprintln(", "format(")
        .replace("print(", "format(")
        .replace("eprint(", "format(")
        .replace("println(", "__repl_discard(")
        .replace("eprintln(", "__repl_discard(")
        .replace("print(", "__repl_discard(")
        .replace("eprint(", "__repl_discard(")
}

#[derive(Clone)]
struct ReplBinding {
    vars: Vec<ReplBindingVar>,
    source_index: usize,
}

#[derive(Clone)]
struct ReplBindingVar {
    name: String,
    mutable: bool,
}

struct ReplDropPlan {
    lets: Vec<String>,
    bindings: Vec<ReplBinding>,
    dropped_names: Vec<String>,
}

fn update_repl_bindings(bindings: &mut Vec<ReplBinding>, new_binding: ReplBinding) {
    if !new_binding.vars.is_empty() {
        for binding in bindings.iter_mut() {
            binding.vars.retain(|var| {
                !new_binding
                    .vars
                    .iter()
                    .any(|new_var| new_var.name == var.name)
            });
        }
        bindings.retain(|binding| !binding.vars.is_empty());
    }
    bindings.push(new_binding);
}

fn prepare_repl_drop(
    lets: &[String],
    bindings: &[ReplBinding],
    name: &str,
) -> Option<ReplDropPlan> {
    let target_index = bindings
        .iter()
        .find(|binding| binding.vars.iter().any(|var| var.name == name))?
        .source_index;

    let mut dropped_names = vec![name.to_string()];
    let mut dropped_set = HashSet::from([name.to_string()]);
    let mut later_bindings = bindings
        .iter()
        .filter(|binding| binding.source_index > target_index)
        .collect::<Vec<_>>();
    later_bindings.sort_by_key(|binding| binding.source_index);
    for binding in later_bindings {
        let source = lets
            .get(binding.source_index)
            .map_or("", std::string::String::as_str);
        if dropped_set
            .iter()
            .any(|dropped| source_mentions_binding(source, dropped))
        {
            for var in &binding.vars {
                if dropped_set.insert(var.name.clone()) {
                    dropped_names.push(var.name.clone());
                }
            }
        }
    }

    let surviving_vars = bindings
        .iter()
        .filter(|binding| binding.source_index >= target_index)
        .flat_map(|binding| binding.vars.iter())
        .filter(|var| !dropped_set.contains(&var.name))
        .cloned()
        .collect::<Vec<_>>();
    let scoped_source = lets[target_index..].join("\n    ");
    let replacement = match surviving_vars.as_slice() {
        [] => format!("let _ = {{\n    {scoped_source}\n    ()\n}}"),
        [var] => format!(
            "let {mutability}{name} = {{\n    {scoped_source}\n    {name}\n}}",
            mutability = if var.mutable { "mut " } else { "" },
            name = var.name,
        ),
        vars => {
            let pattern = vars
                .iter()
                .map(|var| {
                    if var.mutable {
                        format!("mut {}", var.name)
                    } else {
                        var.name.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            let values = vars
                .iter()
                .map(|var| var.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("let ({pattern}) = {{\n    {scoped_source}\n    ({values})\n}}")
        }
    };

    let mut new_lets = lets[..target_index].to_vec();
    new_lets.push(replacement);
    let mut new_bindings = bindings.to_vec();
    for binding in &mut new_bindings {
        binding.vars.retain(|var| !dropped_set.contains(&var.name));
        if binding.source_index >= target_index {
            binding.source_index = target_index;
        }
    }
    new_bindings.retain(|binding| !binding.vars.is_empty());
    Some(ReplDropPlan {
        lets: new_lets,
        bindings: new_bindings,
        dropped_names,
    })
}

fn source_mentions_binding(source: &str, name: &str) -> bool {
    source.match_indices(name).any(|(start, _)| {
        let before = source[..start].chars().next_back();
        let after = source[start + name.len()..].chars().next();
        !before.is_some_and(is_ident_continue) && !after.is_some_and(is_ident_continue)
    })
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

struct ReplDeclarationDropPlan {
    declarations: Vec<String>,
    dropped_names: Vec<String>,
}

/// Removes the declaration introducing `name`, reporting every name that goes.
///
/// One entry can introduce several names - an enum and its variants - and the
/// entry is the unit the user typed, so it ends whole. Entries that name a
/// departing item go with it: an `impl Point` written separately from `Point`
/// cannot outlive it, so the set closes over mentions until it stops growing.
fn prepare_repl_declaration_drop(
    declarations: &[String],
    name: &str,
) -> Option<ReplDeclarationDropPlan> {
    let target = declarations
        .iter()
        .position(|declaration| declaration_declares_name(declaration, name))?;

    let mut removed = HashSet::from([target]);
    let mut dropped_names = declaration_names(&declarations[target]);
    if let Some(position) = dropped_names.iter().position(|other| other == name) {
        dropped_names.swap(0, position);
    }
    let mut dropped_set = dropped_names.iter().cloned().collect::<HashSet<_>>();
    while let Some((index, declaration)) = declarations
        .iter()
        .enumerate()
        .filter(|(index, _)| !removed.contains(index))
        .find(|(_, declaration)| {
            dropped_set
                .iter()
                .any(|dropped| source_mentions_binding(declaration, dropped))
        })
    {
        removed.insert(index);
        for declared in declaration_names(declaration) {
            if dropped_set.insert(declared.clone()) {
                dropped_names.push(declared);
            }
        }
    }

    let declarations = declarations
        .iter()
        .enumerate()
        .filter(|(index, _)| !removed.contains(index))
        .map(|(_, declaration)| declaration.clone())
        .collect();
    Some(ReplDeclarationDropPlan {
        declarations,
        dropped_names,
    })
}

fn render_dropped_declaration_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [name] => format!("`{name}`"),
        [first, rest @ ..] => {
            let also = rest
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("`{first}` and dependent {also}")
        }
    }
}

fn render_dropped_binding_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [name] => format!("`{name}`"),
        [first, rest @ ..] => {
            let also = rest
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("`{first}` and dependent {also}")
        }
    }
}

struct RenderedReplBinding {
    name: String,
    line: String,
}

fn render_repl_bindings(
    declarations: &[String],
    lets: &[String],
    bindings: &[ReplBinding],
) -> Vec<RenderedReplBinding> {
    let let_body = render_repl_setup(lets);
    let mut lines = Vec::new();
    for binding in bindings {
        for var in &binding.vars {
            let entry = format!("__irepl_binding_{}", lines.len());
            let source = format!(
                "{}\nfn {entry}() {{ {lets}{name} }}\n",
                render_repl_declarations(declarations),
                lets = let_body,
                name = var.name,
            );
            let (value, ty) = match build_and_call_with_type_for_inspection(&source, &entry) {
                Ok((value, ty)) => (render_repl_binding_value(&value, &ty), ty.rendered),
                Err(msg) => (
                    format!("<error: {}>", msg.lines().next().unwrap_or("unknown")),
                    "<unknown>".to_string(),
                ),
            };
            let prefix = if var.mutable { "mut " } else { "" };
            lines.push(RenderedReplBinding {
                name: var.name.clone(),
                line: format!("{prefix}{}: {ty} = {value}", var.name),
            });
        }
    }
    lines
}

/// Every session binding whose name the `%explain` query matches, latest
/// declaration of each name first.
fn matching_repl_bindings<'a>(bindings: &'a [ReplBinding], query: &str) -> Vec<&'a ReplBindingVar> {
    let mut out: Vec<&ReplBindingVar> = Vec::new();
    for var in bindings.iter().flat_map(|binding| &binding.vars) {
        if !symbol_query_matches(&var.name, query) {
            continue;
        }
        match out.iter().position(|seen| seen.name == var.name) {
            Some(index) => out[index] = var,
            None => out.push(var),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Joins one rendering per matched binding, failing on the first that cannot
/// be resolved.
fn render_matched_repl_bindings(
    bindings: &[ReplBinding],
    query: &str,
    mut render: impl FnMut(&ReplBindingVar) -> std::result::Result<String, String>,
) -> Option<std::result::Result<String, String>> {
    let matches = matching_repl_bindings(bindings, query);
    if matches.is_empty() {
        return None;
    }
    let mut sections = Vec::new();
    for var in matches {
        match render(var) {
            Ok(text) => sections.push(text),
            Err(message) => return Some(Err(message)),
        }
    }
    Some(Ok(sections.join("\n")))
}

fn repl_binding_info(
    declarations: &[String],
    lets: &[String],
    bindings: &[ReplBinding],
    query: &str,
) -> Option<std::result::Result<String, String>> {
    render_matched_repl_bindings(bindings, query, |var| {
        repl_binding_info_for(declarations, lets, var)
    })
}

fn repl_binding_info_for(
    declarations: &[String],
    lets: &[String],
    var: &ReplBindingVar,
) -> std::result::Result<String, String> {
    let name = var.name.as_str();
    resolve_repl_binding(declarations, lets, name).map(|(_, ty)| {
        let can_mutate = binding_can_mutate(var, &ty);
        let capability = match (var.mutable, ty.references.as_slice(), can_mutate) {
            (_, [], true) => "mutable binding",
            (_, [], false) => "immutable binding",
            (_, _, true) => "mutable referent",
            (_, _, false) => "shared referent",
        };
        let mut out = format!("{} [binding]\n  type: {}\n  capability: {capability}\n", var.name, ty.rendered);
        if !ty.tuple_elements.is_empty() {
            out.push_str("  method surface: tuple (positional elements and the methods below)\n");
            for (index, elem) in ty.tuple_elements.iter().enumerate() {
                out.push_str(&format!("  {}.{index}: {elem} [element]\n", var.name));
            }
            let mutability = if can_mutate {
                format!("; assign with {}.0 = ...", var.name)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "  Example: let ({}) = {}{mutability}\n",
                (0..ty.tuple_elements.len())
                    .map(|i| format!("e{i}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                var.name
            ));
        }
        if let Some(session) = render_session_type(
            &session_index(declarations),
            base_type_name(&ty.rendered),
            Some(&var.name),
        ) {
            out.push_str(&session);
            out.push('\n');
        }
        let Some(ref owner) = ty.method_owner else {
            if index_has_facts(declarations, base_type_name(&ty.rendered)) {
                return out.trim_end().to_string();
            }
            out.push_str(&format!(
                "\nNo cataloged methods for this binding's type.\nExample: let copy = {}",
                var.name
            ));
            return out;
        };
        if ty.fixed_array {
            out.push_str(&format!(
                "  method surface: fixed array (array and slice methods only; mutable methods require writable access)\n  Example: let first = {}[0]\n",
                var.name
            ));
        }
        let methods = available_repl_binding_methods(&ty, owner, can_mutate);
        let mut found = false;
        for method in methods {
            found = true;
            let signature = signature_suffix(&method.signature, &method.name);
            out.push_str(&format!(
                "{}.{}{signature} [method]\n    {}\n    Builtin\n    Example: {}.{}({})\n",
                var.name,
                method.name,
                method.doc,
                var.name,
                method.name,
                signature_example_arguments(signature)
            ));
        }
        if !found {
            out.push_str(&format!(
                "\nNo methods are available with this binding's capability.\nExample: let copy = {}",
                var.name
            ));
        }
        out.trim_end().to_string()
    })
}

fn repl_binding_listing(
    declarations: &[String],
    lets: &[String],
    bindings: &[ReplBinding],
    query: &str,
) -> Option<std::result::Result<String, String>> {
    render_matched_repl_bindings(bindings, query, |var| {
        repl_binding_listing_for(declarations, lets, var)
    })
}

fn repl_binding_listing_for(
    declarations: &[String],
    lets: &[String],
    var: &ReplBindingVar,
) -> std::result::Result<String, String> {
    let name = var.name.as_str();
    resolve_repl_binding(declarations, lets, name).map(|(_value, ty)| {
        let prefix = if var.mutable { "mut " } else { "" };
        let mut out = format!("{prefix}{name}: {} [binding]\n", ty.rendered);
        for (index, elem) in ty.tuple_elements.iter().enumerate() {
            out.push_str(&format!("{name}.{index}: {elem} [element]\n"));
        }
        if let Some(session) = render_session_type(
            &session_index(declarations),
            base_type_name(&ty.rendered),
            Some(name),
        ) {
            out.push_str(&session);
            out.push('\n');
        }
        let Some(ref owner) = ty.method_owner else {
            return out.trim_end().to_string();
        };
        let can_mutate = binding_can_mutate(var, &ty);
        for method in available_repl_binding_methods(&ty, owner, can_mutate) {
            let signature = signature_suffix(&method.signature, &method.name);
            out.push_str(&format!("{name}.{}{signature} [method]\n", method.name));
        }
        out.trim_end().to_string()
    })
}

fn repl_declaration_info(declarations: &[String], query: &str) -> Option<String> {
    let mut sections = Vec::new();
    let mut seen = Vec::new();
    for declaration in declarations.iter().rev() {
        for name in declaration_matching_names(declaration, query) {
            if seen.contains(&name) {
                continue;
            }
            sections.push(format!("{name} [declaration]\n  {declaration}"));
            seen.push(name);
        }
    }
    (!sections.is_empty()).then(|| sections.join("\n"))
}

fn declaration_declares_name(declaration: &str, name: &str) -> bool {
    declaration_names(declaration).contains(&name.to_string())
}

fn declaration_matching_names(declaration: &str, query: &str) -> Vec<String> {
    declaration_names(declaration)
        .into_iter()
        .filter(|name| symbol_query_matches(name, query))
        .collect()
}

fn declaration_names(declaration: &str) -> Vec<String> {
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file(
        "irepl-declaration-explain".to_string(),
        declaration.to_string(),
    );
    let (sf, diags) = gossamer_parse::parse_source_file(declaration, file);
    if diags.is_empty() {
        collect_source_file_names(&sf)
            .into_iter()
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    }
}

fn binding_can_mutate(var: &ReplBindingVar, ty: &ReplValueType) -> bool {
    if ty.references.is_empty() {
        var.mutable
    } else {
        ty.references
            .iter()
            .all(|mutability| *mutability == gossamer_types::Mutbl::Mut)
    }
}

fn available_repl_binding_methods(
    ty: &ReplValueType,
    owner: &str,
    can_mutate: bool,
) -> Vec<CoreMethodEntry> {
    core_methods_for(owner, ty.fixed_array, can_mutate)
}

fn resolve_repl_binding(
    declarations: &[String],
    lets: &[String],
    name: &str,
) -> std::result::Result<(gossamer_interp::Value, ReplValueType), String> {
    let let_body = render_repl_setup(lets);
    let entry = "__irepl_binding_info";
    let source = format!(
        "{}\nfn {entry}() {{ {lets}{name} }}\n",
        render_repl_declarations(declarations),
        lets = let_body,
    );
    build_and_call_with_type_for_inspection(&source, entry)
}

fn infer_repl_binding_type(
    declarations: &[String],
    lets: &[String],
    name: &str,
) -> std::result::Result<ReplValueType, String> {
    let let_body = render_repl_setup(lets);
    let entry = "__irepl_binding_type";
    let source = format!(
        "{}\nfn {entry}() {{ {lets}{name} }}\n",
        render_repl_declarations(declarations),
        lets = let_body,
    );
    infer_repl_tail_type(&source)
}

fn repl_binding_from_let_source(input: &str) -> std::result::Result<ReplBinding, String> {
    use gossamer_ast::{ExprKind, ItemKind, StmtKind};

    // End the input before the synthetic closing brace so a trailing line
    // comment cannot consume it.
    let source = format!("fn __irepl_binding_names() {{ {input}\n}}\n");
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file("<repl>".to_string(), source.clone());
    let (sf, diags) = gossamer_parse::parse_source_file(&source, file);
    if !diags.is_empty() {
        return Err(format_parse_diags(&diags, &map));
    }
    let Some(item) = sf.items.first() else {
        return Err(repl_let_shape_error());
    };
    let ItemKind::Fn(decl) = &item.kind else {
        return Err(repl_let_shape_error());
    };
    let Some(body) = &decl.body else {
        return Err(repl_let_shape_error());
    };
    let ExprKind::Block(block) = &body.kind else {
        return Err(repl_let_shape_error());
    };
    if block.stmts.is_empty() {
        return Err(repl_let_shape_error());
    }
    let mut vars = Vec::new();
    let mut saw_let = false;
    for stmt in &block.stmts {
        let StmtKind::Let { pattern, init, .. } = &stmt.kind else {
            continue;
        };
        saw_let = true;
        if init.is_none() {
            return Err(repl_let_initializer_error());
        }
        collect_repl_pattern_bindings(pattern, &mut vars);
    }
    if !saw_let {
        return Err(repl_let_shape_error());
    }
    Ok(ReplBinding {
        vars,
        source_index: 0,
    })
}

fn repl_let_shape_error() -> String {
    "1 REPL input error:\n  malformed `let` input: expected one or more `let PAT = EXPR` statements"
        .to_string()
}

fn repl_let_initializer_error() -> String {
    "1 REPL input error:\n  malformed `let` input: missing `=` initializer; write `let PAT = EXPR`"
        .to_string()
}

fn collect_repl_pattern_bindings(pattern: &gossamer_ast::Pattern, out: &mut Vec<ReplBindingVar>) {
    use gossamer_ast::PatternKind;

    match &pattern.kind {
        PatternKind::Ident {
            mutability,
            name,
            subpattern,
        } => {
            out.push(ReplBindingVar {
                name: name.name.clone(),
                mutable: mutability.is_mutable(),
            });
            if let Some(subpattern) = subpattern {
                collect_repl_pattern_bindings(subpattern, out);
            }
        }
        PatternKind::Tuple(patterns) => {
            for pattern in patterns {
                collect_repl_pattern_bindings(pattern, out);
            }
        }
        PatternKind::Or(patterns) => {
            if let Some(pattern) = patterns.first() {
                collect_repl_pattern_bindings(pattern, out);
            }
        }
        PatternKind::Slice {
            prefix,
            rest,
            suffix,
        } => {
            for pattern in prefix {
                collect_repl_pattern_bindings(pattern, out);
            }
            if let Some(rest) = rest {
                collect_repl_pattern_bindings(rest, out);
            }
            for pattern in suffix {
                collect_repl_pattern_bindings(pattern, out);
            }
        }
        PatternKind::Struct { fields, .. } => {
            for field in fields {
                match &field.pattern {
                    Some(pattern) => collect_repl_pattern_bindings(pattern, out),
                    None => out.push(ReplBindingVar {
                        name: field.name.name.clone(),
                        mutable: false,
                    }),
                }
            }
        }
        PatternKind::TupleStruct { elems, .. } => {
            for pattern in elems {
                collect_repl_pattern_bindings(pattern, out);
            }
        }
        PatternKind::Ref { inner, .. } => collect_repl_pattern_bindings(inner, out),
        _ => {}
    }
}

fn split_meta_command(input: &str) -> (&str, &str) {
    input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(command, arg)| (command, arg.trim()))
}

fn input_is_declaration(input: &str) -> bool {
    let input = strip_leading_outer_attributes(input);
    let input = input
        .strip_prefix("pub ")
        .or_else(|| input.strip_prefix("pub(crate) "))
        .unwrap_or(input);
    input.starts_with("fn ")
        || input.starts_with("struct ")
        || input.starts_with("enum ")
        || input.starts_with("impl ")
        || input.starts_with("trait ")
        || input.starts_with("use ")
        || input.starts_with("const ")
        || input.starts_with("static ")
        || input.starts_with("type ")
}

/// Removes complete outer attributes from the beginning of a REPL input.
///
/// The REPL classifies declarations before rebuilding the accumulated source.
/// An attributed item still starts with `#`, so without this step it is
/// mistaken for an expression and wrapped in the synthetic REPL function.
fn strip_leading_outer_attributes(mut input: &str) -> &str {
    loop {
        input = input.trim_start();
        if !input.starts_with("#[") {
            return input;
        }

        let mut depth = 0usize;
        let mut quote = None;
        let mut escaped = false;
        let mut end = None;

        for (offset, ch) in input.char_indices() {
            if let Some(delimiter) = quote {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == delimiter {
                    quote = None;
                }
                continue;
            }

            match ch {
                '"' | '\'' => quote = Some(ch),
                '[' => depth += 1,
                ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(offset + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
        }

        let Some(end) = end else {
            return input;
        };
        input = &input[end..];
    }
}

fn repl_info(arg: &str) -> std::result::Result<String, String> {
    // The catalog listing is the canonical module rendering. Omitting module
    // help here prevents `%i gzip` from printing the same module twice while
    // retaining matching items, methods, and types from the search.
    repl_info_matches(arg, true)
}

fn repl_info_listing(arg: &str) -> std::result::Result<String, String> {
    repl_info_matches(arg, false)
}

fn repl_info_matches(arg: &str, details: bool) -> std::result::Result<String, String> {
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
fn stdlib_trait_path(name: &str) -> Option<String> {
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

fn stdlib_namespace_children(namespace: &str) -> Vec<StdModule> {
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

fn canonical_stdlib_namespace(query: &str) -> Option<String> {
    let canonical = if query.starts_with("std::") {
        query.to_string()
    } else {
        format!("std::{query}")
    };
    (!stdlib_namespace_children(&canonical).is_empty()).then_some(canonical)
}

fn render_repl_history(
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

fn compile_search_regex(command: &str, query: &str) -> std::result::Result<Regex, String> {
    Regex::new(query).map_err(|error| format!("invalid {command} regex `{query}`: {error}"))
}

struct ListingOptions {
    pattern: String,
    details: bool,
}

fn parse_listing_options(command: &str, arg: &str) -> std::result::Result<ListingOptions, String> {
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

fn render_info(text: String, options: &ListingOptions) -> String {
    if options.details {
        text
    } else {
        text.split("\n\n")
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn regex_argument(arg: &str) -> std::result::Result<Option<Regex>, String> {
    if !(arg.starts_with('/') && arg.ends_with('/') && arg.len() >= 2) {
        return Ok(None);
    }
    Regex::new(&arg[1..arg.len() - 1])
        .map(Some)
        .map_err(|e| format!("invalid regex `{arg}`: {e}"))
}

fn render_catalog_matches(pattern: &Regex, details: bool) -> String {
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
fn push_owned_method_entries(entries: &mut Vec<String>, owner: &str, details: bool) {
    for method in core_method_entries()
        .into_iter()
        .filter(|method| method.owner == owner)
    {
        let mut entry = String::new();
        push_core_method_match(&mut entry, &method, details);
        entries.push(entry);
    }
}

fn render_catalog_query_matches(query: &str, details: bool) -> String {
    let mut entries = Vec::new();
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
        // contents. A qualified item query does not enter this branch and
        // remains focused on the requested symbol.
        for item in module.items {
            let mut entry = String::new();
            push_item_match(&mut entry, &module, item, details);
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

fn render_module_matches(modules: &[StdModule], details: bool) -> String {
    let mut entries = Vec::new();
    for module in modules {
        let mut entry = String::new();
        push_module_match(&mut entry, module, details);
        entries.push(entry);
    }
    render_catalog_entries(entries, "")
}

fn push_catalog_match(
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

fn push_catalog_origin(out: &mut String, defined_in: &str) {
    if defined_in == "Builtin" {
        out.push_str("    Builtin\n");
    } else {
        out.push_str(&format!("    Defined in: {defined_in}\n"));
    }
}

fn catalog_kind_label(kind: &str) -> &str {
    if kind == "assoc" {
        "associated function"
    } else {
        kind
    }
}

fn catalog_example(path: &str, kind: &str, signature: &str) -> String {
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

fn example_receiver(owner: &str) -> &'static str {
    match owner.rsplit("::").next().unwrap_or(owner) {
        "String" | "str" => "\"text\"",
        "Vec" | "Slice" | "Array" => "values",
        "Map" | "BTreeMap" => "map",
        "Set" | "BTreeSet" => "set",
        "Deque" => "deque",
        "Queue" => "queue",
        "Stack" => "stack",
        "MaxHeap" | "MinHeap" => "heap",
        "Option" => "option",
        "Result" => "result",
        "Iterator" | "Range" => "iter",
        _ => "value",
    }
}

fn core_namespace_description(owner: &str) -> &'static str {
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
fn signature_parameters(signature: &str) -> Vec<(&str, &str)> {
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
fn signature_example_arguments(signature: &str) -> String {
    signature_parameters(signature)
        .into_iter()
        .map(|(name, ty)| example_argument(name, ty))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A closure whose shape follows a callback type: its arity, and what it
/// answers.
fn example_closure(ty: &str) -> String {
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
fn example_argument(name: &str, ty: &str) -> String {
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

fn split_top_level_parameters(parameters: &str) -> Vec<&str> {
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

fn signature_suffix<'a>(signature: &'a str, name: &str) -> &'a str {
    signature
        .strip_prefix(&format!("fn {name}"))
        .or_else(|| signature.strip_prefix(name))
        .unwrap_or(signature)
        .trim_start()
}

fn push_module_match(out: &mut String, module: &StdModule, details: bool) {
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

fn push_item_match(out: &mut String, module: &StdModule, item: &StdItem, details: bool) {
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
fn push_builtin_trait_match(out: &mut String, entry: &gossamer_types::BuiltinTrait, details: bool) {
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

fn push_core_method_match(out: &mut String, method: &CoreMethodEntry, details: bool) {
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

fn render_catalog_entries(mut entries: Vec<String>, empty: &str) -> String {
    if entries.is_empty() {
        return empty.to_string();
    }
    entries.sort_unstable();
    entries.dedup();
    entries.join("\n").trim_end().to_string()
}

fn render_stdlib_dir() -> String {
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

fn stdlib_namespaces() -> Vec<String> {
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

fn render_stdlib_namespace_dir(namespace: &str, modules: &[StdModule]) -> String {
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

fn push_catalog_entry(out: &mut String, path: &str, kind: &str, description: &str) {
    out.push_str(&format!(
        "{path} [{kind}]\n  {description}\n  Example: {}\n\n",
        catalog_example(path, kind, "")
    ));
}

/// The core method catalog, derived once. Its inputs - the static table,
/// the stdlib manifest, and the interpreter's registered builtins - are
/// fixed for the life of the process.
fn core_method_entries() -> Vec<CoreMethodEntry> {
    static CATALOG: std::sync::OnceLock<Vec<CoreMethodEntry>> = std::sync::OnceLock::new();
    CATALOG.get_or_init(build_core_method_entries).clone()
}

fn build_core_method_entries() -> Vec<CoreMethodEntry> {
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
fn catalog_under_receiver_types(entries: &mut BTreeMap<(String, String), CoreMethodEntry>) {
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
fn receiver_type_name(signature: &str) -> Option<&str> {
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
fn short_type_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Names a map's elements are pairs of, so the `Vec` sequence surface it
/// inherits reads over `(K, V)` rather than over a single element type.
const MAP_SEQUENCE_OWNERS: &[&str] = &["Map", "BTreeMap"];

/// Names a set's elements are single values of, so the inherited surface
/// keeps `Vec`'s element type and only its receiver changes.
const SET_SEQUENCE_OWNERS: &[&str] = &["Set", "BTreeSet"];

/// Fills the signature of every map/set sequence method from the `Vec`
/// entry of the same name. The traversal surface is the same one `Vec`
/// carries - only the receiver and the element type differ - so deriving
/// it keeps one table authoritative instead of a second copy that drifts.
fn fill_collection_sequence_signatures(entries: &mut BTreeMap<(String, String), CoreMethodEntry>) {
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
fn set_sequence_signature(signature: &str, owner: &str) -> String {
    signature
        .replace("self: &mut Vec<", &format!("self: &mut {owner}<"))
        .replace("self: Vec<", &format!("self: {owner}<"))
}

/// The `Vec` signature rewritten for a map receiver: the element type `T`
/// becomes the `(K, V)` pair, and `Vec`'s own key generic moves out of the
/// way of the map's `K`.
fn map_sequence_signature(signature: &str, owner: &str) -> String {
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
fn replace_generic(signature: &str, from: &str, to: &str) -> String {
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
fn sequence_owner_signature(signature: &str, owner: &str) -> String {
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
fn vec_element_and_tail(ty: &str) -> Option<(&str, &str)> {
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
fn add_data_last_std_methods(
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
fn data_first_method_signature(owner: &str, module_path: &str, name: &str) -> Option<String> {
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

fn iterator_method_doc(name: &str) -> Option<&'static str> {
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

fn runtime_core_method_signature(owner: &str, name: &str, kind: &str) -> Option<String> {
    // These runtime-backed handle constructors are registered by the
    // interpreter rather than the stdlib function catalog. Keep their public
    // contracts here so `%info` never fabricates an ellipsis signature.
    if matches!(
        owner,
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize"
    ) && matches!(name, "wrapping_add" | "wrapping_mul")
    {
        return Some(format!("fn {name}(self: {owner}, rhs: {owner}) -> {owner}"));
    }
    if let Some(signature) = match (owner, name) {
        // The cursor pull a `for` desugars to, and the one iterator method
        // that is not a `std::iter` free function.
        ("Iterator", "next") => Some("fn next<T>(self: &mut Iterator<T>) -> Option<T>"),
        ("AtomicBool", "new") => Some("fn new(value: bool) -> AtomicBool"),
        ("AtomicI32", "new") => Some("fn new(value: i64) -> AtomicI32"),
        ("AtomicI64", "new") => Some("fn new(value: i64) -> AtomicI64"),
        ("AtomicU64", "new") => Some("fn new(value: i64) -> AtomicU64"),
        ("Barrier", "new") => Some("fn new(parties: i64) -> Barrier"),
        ("Mutex", "new") => Some("fn new<T>(value: T) -> Mutex<T>"),
        ("RwLock", "new") => Some("fn new<T>(value: T) -> RwLock<T>"),
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
fn runtime_core_method_doc(owner: &str, name: &str) -> Option<&'static str> {
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
    if matches!(
        owner,
        "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize"
    ) {
        return match name {
            "wrapping_add" => {
                Some("Adds with two's-complement wrapping at this integer type's width.")
            }
            "wrapping_mul" => {
                Some("Multiplies with two's-complement wrapping at this integer type's width.")
            }
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

fn insert_core_method_entry(
    entries: &mut BTreeMap<(String, String), CoreMethodEntry>,
    entry: CoreMethodEntry,
) {
    entries
        .entry((entry.owner.clone(), entry.name.clone()))
        .or_insert(entry);
}

fn registered_core_method_path(path: &str) -> Option<(String, String)> {
    let (owner, name) = path.rsplit_once("::")?;
    if name.starts_with("__") || owner == "Type" {
        return None;
    }
    let owner = canonical_runtime_owner(owner)?;
    Some((owner, name.to_string()))
}

fn canonical_runtime_owner(owner: &str) -> Option<String> {
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

fn runtime_assoc_name(name: &str) -> bool {
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

fn all_core_namespaces() -> Vec<String> {
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

fn matching_core_namespaces(query: &str) -> Vec<String> {
    all_core_namespaces()
        .into_iter()
        .filter(|owner| core_namespace_matches(owner, query))
        // A type with a `CORE_TYPES` row is rendered from that row, which
        // carries its spelling and example; the namespace list holds the
        // same name only because the type owns methods.
        .filter(|owner| !CORE_TYPES.iter().any(|core| core.name == owner))
        .collect()
}

fn matching_modules(query: &str) -> Vec<StdModule> {
    gossamer_std::registry::modules()
        .iter()
        .copied()
        .filter(|module| module_query_matches(module, query))
        .collect()
}

fn matching_items(query: &str) -> Vec<(StdModule, StdItem)> {
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

fn matching_core_methods(query: &str) -> Vec<CoreMethodEntry> {
    core_method_entries()
        .into_iter()
        .filter(|method| core_method_query_matches(method, query))
        .collect()
}

fn matching_builtin_macros(query: &str) -> Vec<&'static BuiltinMacro> {
    BUILTIN_MACROS
        .iter()
        .filter(|builtin| symbol_query_matches(builtin.name, query))
        .collect()
}

fn matching_core_types(query: &str) -> Vec<&'static CoreTypeHelp> {
    CORE_TYPES
        .iter()
        .filter(|core_type| symbol_query_matches(core_type.name, query))
        .collect()
}

/// The built-in traits a query names. A trait the standard library declares
/// is reached through its module's manifest entry instead, so the catalog's
/// bare-name rendering covers only the ones the language supplies itself.
fn matching_builtin_traits(query: &str) -> Vec<&'static gossamer_types::BuiltinTrait> {
    gossamer_types::BUILTIN_TRAITS
        .iter()
        .filter(|entry| entry.module.is_none() && symbol_query_matches(entry.name, query))
        .collect()
}

fn matching_prelude_builtins(query: &str) -> Vec<&'static PreludeBuiltinHelp> {
    PRELUDE_BUILTINS
        .iter()
        .filter(|builtin| symbol_query_matches(builtin.name, query))
        .collect()
}

fn module_query_matches(module: &StdModule, query: &str) -> bool {
    module_aliases(module.path)
        .iter()
        .any(|alias| symbol_query_matches(alias, query))
}

fn core_namespace_matches(owner: &str, query: &str) -> bool {
    let (text, shape) = split_symbol_query(query);
    if shape != QueryShape::Exact {
        return shape_matches(owner, text, shape);
    }
    owner == text || owner.eq_ignore_ascii_case(text) || owner == canonical_collection_owner(text)
}

fn item_query_matches(module: &StdModule, item: &StdItem, query: &str) -> bool {
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

fn core_method_query_matches(method: &CoreMethodEntry, query: &str) -> bool {
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

fn core_lower_path(method: &CoreMethodEntry) -> String {
    format!("{}::{}", method.owner.to_ascii_lowercase(), method.name)
}

fn canonical_collection_owner(owner: &str) -> &str {
    owner.strip_prefix("collections::").unwrap_or(owner)
}

fn info_search_query(arg: &str) -> String {
    normalize_query(arg).to_string()
}

/// How a `%info` / `%explain` argument matches a candidate spelling. A bare
/// argument names one symbol; a leading or trailing `*` widens it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryShape {
    Exact,
    Prefix,
    Suffix,
    Substring,
}

fn symbol_query_matches(candidate: &str, query: &str) -> bool {
    let (text, shape) = split_symbol_query(query);
    shape_matches(candidate, text, shape)
}

fn shape_matches(candidate: &str, text: &str, shape: QueryShape) -> bool {
    match shape {
        QueryShape::Exact => candidate == text,
        QueryShape::Prefix => candidate.starts_with(text),
        QueryShape::Suffix => candidate.ends_with(text),
        QueryShape::Substring => candidate.contains(text),
    }
}

fn split_symbol_query(query: &str) -> (&str, QueryShape) {
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

fn module_matches_regex(pattern: &Regex, module: &StdModule) -> bool {
    pattern.is_match(module.path) || pattern.is_match(module.summary)
}

fn item_matches_regex(pattern: &Regex, module: &StdModule, item: &StdItem) -> bool {
    pattern.is_match(&format!("{}::{}", module.path, item.name))
        || pattern.is_match(item.name)
        || pattern.is_match(item.doc)
}

fn module_aliases(path: &'static str) -> Vec<&'static str> {
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

fn normalize_query(arg: &str) -> &str {
    arg.trim_matches('`').trim()
}

/// The method an `impl` of a stdlib trait must supply, rendered as the
/// listing's signature suffix. `None` for a trait with no required method.
fn trait_required_signature(name: &str) -> Option<String> {
    let entry = gossamer_types::builtin_trait(name)?;
    (!entry.signature.is_empty()).then(|| format!(" {}", entry.signature))
}

fn item_kind_label(kind: StdItemKind) -> &'static str {
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
fn input_mutates_binding(input: &str, user_mutating_methods: &HashSet<String>) -> bool {
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

fn collect_repl_mut_self_method_names(declarations: &[String]) -> HashSet<String> {
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

fn repl_stmt_mutates_binding(
    stmt: &gossamer_ast::Stmt,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    use gossamer_ast::StmtKind;

    match &stmt.kind {
        StmtKind::Let { init, .. } => init
            .as_deref()
            .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods)),
        StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => {
            repl_expr_mutates_binding(expr, user_mutating_methods)
        }
        StmtKind::Item(_) => false,
    }
}

fn repl_select_op_contains_ref_mut(op: &gossamer_ast::expr::SelectOp) -> bool {
    use gossamer_ast::expr::SelectOp;

    match op {
        SelectOp::Recv { channel, .. } => repl_expr_contains_ref_mut(channel),
        SelectOp::Send { channel, value } => {
            repl_expr_contains_ref_mut(channel) || repl_expr_contains_ref_mut(value)
        }
        SelectOp::Default => false,
    }
}

fn repl_stmt_contains_ref_mut(stmt: &gossamer_ast::Stmt) -> bool {
    use gossamer_ast::StmtKind;

    match &stmt.kind {
        StmtKind::Let { init, .. } => init.as_deref().is_some_and(repl_expr_contains_ref_mut),
        StmtKind::Expr { expr, .. } | StmtKind::Defer(expr) => repl_expr_contains_ref_mut(expr),
        StmtKind::Item(_) => false,
    }
}

fn repl_expr_contains_ref_mut(expr: &gossamer_ast::Expr) -> bool {
    use gossamer_ast::ExprKind;
    use gossamer_ast::common::UnaryOp;

    match &expr.kind {
        ExprKind::Unary {
            op: UnaryOp::RefMut,
            ..
        } => true,
        ExprKind::Call { callee, args } => {
            repl_expr_contains_ref_mut(callee) || args.iter().any(repl_expr_contains_ref_mut)
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            repl_expr_contains_ref_mut(receiver) || args.iter().any(repl_expr_contains_ref_mut)
        }
        ExprKind::FieldAccess { receiver, .. } => repl_expr_contains_ref_mut(receiver),
        ExprKind::Index { base, index } => {
            repl_expr_contains_ref_mut(base) || repl_expr_contains_ref_mut(index)
        }
        ExprKind::Unary { operand, .. } => repl_expr_contains_ref_mut(operand),
        ExprKind::Binary { lhs, rhs, .. } => {
            repl_expr_contains_ref_mut(lhs) || repl_expr_contains_ref_mut(rhs)
        }
        ExprKind::Assign { place, value, .. } => {
            repl_expr_contains_ref_mut(place) || repl_expr_contains_ref_mut(value)
        }
        ExprKind::Cast { value, .. } | ExprKind::Try(value) => repl_expr_contains_ref_mut(value),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            repl_expr_contains_ref_mut(condition)
                || repl_expr_contains_ref_mut(then_branch)
                || else_branch
                    .as_deref()
                    .is_some_and(repl_expr_contains_ref_mut)
        }
        ExprKind::Match { scrutinee, arms } => {
            repl_expr_contains_ref_mut(scrutinee)
                || arms.iter().any(|arm| {
                    arm.guard.as_ref().is_some_and(repl_expr_contains_ref_mut)
                        || repl_expr_contains_ref_mut(&arm.body)
                })
        }
        ExprKind::Loop { body, .. } => repl_expr_contains_ref_mut(body),
        ExprKind::While {
            condition, body, ..
        } => repl_expr_contains_ref_mut(condition) || repl_expr_contains_ref_mut(body),
        ExprKind::For { iter, body, .. } => {
            repl_expr_contains_ref_mut(iter) || repl_expr_contains_ref_mut(body)
        }
        ExprKind::Block(block) | ExprKind::Unsafe(block) => {
            block.stmts.iter().any(repl_stmt_contains_ref_mut)
                || block
                    .tail
                    .as_deref()
                    .is_some_and(repl_expr_contains_ref_mut)
        }
        ExprKind::Closure { body, .. } => repl_expr_contains_ref_mut(body),
        ExprKind::Return(value) => value.as_deref().is_some_and(repl_expr_contains_ref_mut),
        ExprKind::Break { value, .. } => value.as_deref().is_some_and(repl_expr_contains_ref_mut),
        ExprKind::Tuple(elems) | ExprKind::MapLiteral(elems) | ExprKind::SetLiteral(elems) => {
            elems.iter().any(repl_expr_contains_ref_mut)
        }
        ExprKind::Struct { fields, base, .. } => {
            fields
                .iter()
                .any(|field| field.value.as_ref().is_some_and(repl_expr_contains_ref_mut))
                || base.as_deref().is_some_and(repl_expr_contains_ref_mut)
        }
        ExprKind::Array(array) | ExprKind::FixedArray(array) => {
            repl_array_expr_contains_ref_mut(array)
        }
        ExprKind::Range { start, end, .. } => {
            start.as_deref().is_some_and(repl_expr_contains_ref_mut)
                || end.as_deref().is_some_and(repl_expr_contains_ref_mut)
        }
        ExprKind::Select(arms) => arms.iter().any(|arm| {
            repl_select_op_contains_ref_mut(&arm.op) || repl_expr_contains_ref_mut(&arm.body)
        }),
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::Continue { .. } | ExprKind::Error => {
            false
        }
    }
}

fn repl_array_expr_contains_ref_mut(array: &gossamer_ast::expr::ArrayExpr) -> bool {
    match array {
        gossamer_ast::expr::ArrayExpr::List(elems) => elems.iter().any(repl_expr_contains_ref_mut),
        gossamer_ast::expr::ArrayExpr::Repeat { value, count } => {
            repl_expr_contains_ref_mut(value) || repl_expr_contains_ref_mut(count)
        }
    }
}

fn repl_expr_mutates_binding(
    expr: &gossamer_ast::Expr,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    use gossamer_ast::ExprKind;

    match &expr.kind {
        ExprKind::Assign { .. } => true,
        ExprKind::MethodCall {
            receiver,
            name,
            args,
            ..
        } => {
            gossamer_types::is_mutating_method_name(&name.name)
                || user_mutating_methods.contains(&name.name)
                || repl_expr_mutates_binding(receiver, user_mutating_methods)
                || args
                    .iter()
                    .any(|arg| repl_expr_mutates_binding(arg, user_mutating_methods))
        }
        ExprKind::Call { callee, args } => {
            repl_callee_is_mutating_name(callee)
                || repl_expr_mutates_binding(callee, user_mutating_methods)
                || args
                    .iter()
                    .any(|arg| repl_expr_mutates_binding(arg, user_mutating_methods))
        }
        ExprKind::For { iter, body, .. } => {
            repl_expr_contains_ref_mut(iter)
                || repl_expr_mutates_binding(body, user_mutating_methods)
        }
        ExprKind::Block(block) | ExprKind::Unsafe(block) => {
            repl_block_mutates_binding(block, user_mutating_methods)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => repl_if_mutates_binding(
            condition,
            then_branch,
            else_branch.as_deref(),
            user_mutating_methods,
        ),
        ExprKind::Match { scrutinee, arms } => {
            repl_match_mutates_binding(scrutinee, arms, user_mutating_methods)
        }
        ExprKind::Loop { body, .. } => repl_expr_mutates_binding(body, user_mutating_methods),
        ExprKind::While {
            condition, body, ..
        } => repl_pair_mutates_binding(condition, body, user_mutating_methods),
        ExprKind::FieldAccess { receiver, .. } => {
            repl_expr_mutates_binding(receiver, user_mutating_methods)
        }
        ExprKind::Index { base, index } => {
            repl_expr_mutates_binding(base, user_mutating_methods)
                || repl_expr_mutates_binding(index, user_mutating_methods)
        }
        ExprKind::Unary { operand, .. } => {
            repl_expr_mutates_binding(operand, user_mutating_methods)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            repl_expr_mutates_binding(lhs, user_mutating_methods)
                || repl_expr_mutates_binding(rhs, user_mutating_methods)
        }
        ExprKind::Cast { value, .. } | ExprKind::Try(value) => {
            repl_expr_mutates_binding(value, user_mutating_methods)
        }
        ExprKind::Closure { body, .. } => repl_expr_mutates_binding(body, user_mutating_methods),
        ExprKind::Return(value) => value
            .as_deref()
            .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods)),
        ExprKind::Break { value, .. } => value
            .as_deref()
            .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods)),
        ExprKind::Tuple(elems) | ExprKind::MapLiteral(elems) | ExprKind::SetLiteral(elems) => elems
            .iter()
            .any(|expr| repl_expr_mutates_binding(expr, user_mutating_methods)),
        ExprKind::Struct { fields, base, .. } => {
            repl_struct_expr_mutates_binding(fields, base.as_deref(), user_mutating_methods)
        }
        ExprKind::Array(array) | ExprKind::FixedArray(array) => {
            repl_array_expr_mutates_binding(array, user_mutating_methods)
        }
        ExprKind::Range { start, end, .. } => repl_optional_pair_mutates_binding(
            start.as_deref(),
            end.as_deref(),
            user_mutating_methods,
        ),
        ExprKind::Select(arms) => arms
            .iter()
            .any(|arm| repl_expr_mutates_binding(&arm.body, user_mutating_methods)),
        ExprKind::Literal(_) | ExprKind::Path(_) | ExprKind::Continue { .. } | ExprKind::Error => {
            false
        }
    }
}

fn repl_block_mutates_binding(
    block: &gossamer_ast::expr::Block,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    block
        .stmts
        .iter()
        .any(|stmt| repl_stmt_mutates_binding(stmt, user_mutating_methods))
        || block
            .tail
            .as_deref()
            .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
}

fn repl_if_mutates_binding(
    condition: &gossamer_ast::Expr,
    then_branch: &gossamer_ast::Expr,
    else_branch: Option<&gossamer_ast::Expr>,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    repl_pair_mutates_binding(condition, then_branch, user_mutating_methods)
        || else_branch.is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
}

fn repl_match_mutates_binding(
    scrutinee: &gossamer_ast::Expr,
    arms: &[gossamer_ast::expr::MatchArm],
    user_mutating_methods: &HashSet<String>,
) -> bool {
    repl_expr_mutates_binding(scrutinee, user_mutating_methods)
        || arms.iter().any(|arm| {
            arm.guard
                .as_ref()
                .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
                || repl_expr_mutates_binding(&arm.body, user_mutating_methods)
        })
}

fn repl_pair_mutates_binding(
    left: &gossamer_ast::Expr,
    right: &gossamer_ast::Expr,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    repl_expr_mutates_binding(left, user_mutating_methods)
        || repl_expr_mutates_binding(right, user_mutating_methods)
}

fn repl_optional_pair_mutates_binding(
    left: Option<&gossamer_ast::Expr>,
    right: Option<&gossamer_ast::Expr>,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    left.is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
        || right.is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
}

fn repl_struct_expr_mutates_binding(
    fields: &[gossamer_ast::expr::StructExprField],
    base: Option<&gossamer_ast::Expr>,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    fields.iter().any(|field| {
        field
            .value
            .as_ref()
            .is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
    }) || base.is_some_and(|expr| repl_expr_mutates_binding(expr, user_mutating_methods))
}

fn repl_array_expr_mutates_binding(
    array: &gossamer_ast::expr::ArrayExpr,
    user_mutating_methods: &HashSet<String>,
) -> bool {
    match array {
        gossamer_ast::expr::ArrayExpr::List(elems) => elems
            .iter()
            .any(|expr| repl_expr_mutates_binding(expr, user_mutating_methods)),
        gossamer_ast::expr::ArrayExpr::Repeat { value, count } => {
            repl_expr_mutates_binding(value, user_mutating_methods)
                || repl_expr_mutates_binding(count, user_mutating_methods)
        }
    }
}

fn repl_callee_is_mutating_name(callee: &gossamer_ast::Expr) -> bool {
    let gossamer_ast::ExprKind::Path(path) = &callee.kind else {
        return false;
    };
    path.segments
        .last()
        .is_some_and(|segment| gossamer_types::is_mutating_method_name(&segment.name.name))
}

/// Validates that the accumulated declarations parse, resolve, and
/// compile onto the VM. The built `Vm` is discarded - the REPL keeps
/// declarations as source strings and full-recompiles each input - so
/// this is purely a probe: `Ok(())` means the declaration set is
/// loadable, `Err` rolls back the just-added declaration.
fn rebuild_session(declarations: &[String]) -> std::result::Result<(), String> {
    // Parse declarations before appending the synthetic probe function. A
    // missing item body must point at the user's end of input, not at the
    // generated `fn __irepl_probe` that follows it.
    let declarations_source = render_repl_declarations(declarations);
    let mut declarations_map = gossamer_lex::SourceMap::new();
    let declarations_file =
        declarations_map.add_file("<repl>".to_string(), declarations_source.clone());
    let (_, declaration_diags) =
        gossamer_parse::parse_source_file(&declarations_source, declarations_file);
    if !declaration_diags.is_empty() {
        return Err(format_parse_diags(&declaration_diags, &declarations_map));
    }

    let source = render_repl_declarations(declarations) + "\nfn __irepl_probe() { }\n";
    let source = gossamer_parse::autoderive::augment_source(&source);
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file("<repl>".to_string(), source.clone());
    let (mut sf, parse_diags) = gossamer_parse::autoderive::parse_with_autoderive(&source, file);
    if !parse_diags.is_empty() {
        return Err(format_parse_diags(&parse_diags, &map));
    }
    let (res, resolve_diags) = gossamer_resolve::resolve_source_file(&sf);
    if !resolve_diags.is_empty() {
        return Err(format_resolve_diags(&sf, &resolve_diags, &map));
    }
    // A labelled or defaulted argument and a std function named in value
    // position are caller-side spellings, rewritten into the one shape the
    // checker and every tier lower. The REPL drives the front-end phase by
    // phase rather than through `check_frontend`, so it runs the shared
    // normalisation itself to see the same calls a file does.
    let named_arg_diags = gossamer_types::normalize_caller_side_spellings(&mut sf, &res);
    if !named_arg_diags.is_empty() {
        return Err(format_resolve_diags(&sf, &named_arg_diags, &map));
    }
    let mut tcx = gossamer_types::TyCtxt::new();
    let (tbl, type_diags) = gossamer_types::typecheck_source_file(&sf, &res, &mut tcx);
    if !type_diags.is_empty() {
        return Err(format_type_diags(&type_diags, &map));
    }
    let program = gossamer_hir::lower_source_file(&sf, &res, &tbl, &mut tcx);
    let mut vm = gossamer_interp::Vm::new();
    // A session's next call is whatever the user types next, so every body
    // stays promotable.
    vm.set_entry_points(&[]);
    vm.load(&program, tcx, true).map_err(|e| format!("{e}"))?;
    Ok(())
}

fn build_and_call(
    source: &str,
    entry: &str,
) -> std::result::Result<gossamer_interp::Value, String> {
    build_and_call_with_type_inner(source, entry, false).map(|(value, _)| value)
}

fn build_and_call_with_type(
    source: &str,
    entry: &str,
) -> std::result::Result<(gossamer_interp::Value, ReplValueType), String> {
    build_and_call_with_type_inner(source, entry, false)
}

fn build_and_call_with_type_for_inspection(
    source: &str,
    entry: &str,
) -> std::result::Result<(gossamer_interp::Value, ReplValueType), String> {
    build_and_call_with_type_inner(source, entry, true)
}

fn infer_repl_tail_type(source: &str) -> std::result::Result<ReplValueType, String> {
    let source = gossamer_parse::autoderive::augment_source(source);
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file("<repl>".to_string(), source.clone());
    let (mut sf, parse_diags) = gossamer_parse::autoderive::parse_with_autoderive(&source, file);
    if !parse_diags.is_empty() {
        return Err(format_parse_diags(&parse_diags, &map));
    }
    let (res, resolve_diags) = gossamer_resolve::resolve_source_file(&sf);
    if !resolve_diags.is_empty() {
        return Err(format_resolve_diags(&sf, &resolve_diags, &map));
    }
    // A labelled or defaulted argument and a std function named in value
    // position are caller-side spellings, rewritten into the one shape the
    // checker and every tier lower. The REPL drives the front-end phase by
    // phase rather than through `check_frontend`, so it runs the shared
    // normalisation itself to see the same calls a file does.
    let named_arg_diags = gossamer_types::normalize_caller_side_spellings(&mut sf, &res);
    if !named_arg_diags.is_empty() {
        return Err(format_resolve_diags(&sf, &named_arg_diags, &map));
    }
    let mut tcx = gossamer_types::TyCtxt::new();
    let (tbl, type_diags) =
        gossamer_types::typecheck_source_file_for_repl_inspection(&sf, &res, &mut tcx);
    let tail_spans = repl_tail_diag_spans(&sf);
    let user_type_diags: Vec<_> = type_diags
        .iter()
        .filter(|diag| !is_implicit_repl_tail_diag(diag, &tail_spans))
        .collect();
    if !user_type_diags.is_empty() {
        return Err(format_type_diags(&user_type_diags, &map));
    }
    let tail_ty_id = repl_generated_tail_expr(&sf).and_then(|expr| tbl.get(expr.id));
    Ok(tail_ty_id.map_or_else(ReplValueType::unknown, |ty| {
        ReplValueType::from_ty(&tcx, ty)
    }))
}

fn build_and_call_with_type_inner(
    source: &str,
    entry: &str,
    inspection: bool,
) -> std::result::Result<(gossamer_interp::Value, ReplValueType), String> {
    let source = gossamer_parse::autoderive::augment_source(source);
    let mut map = gossamer_lex::SourceMap::new();
    let file = map.add_file("<repl>".to_string(), source.clone());
    let (mut sf, parse_diags) = gossamer_parse::autoderive::parse_with_autoderive(&source, file);
    if !parse_diags.is_empty() {
        return Err(format_parse_diags(&parse_diags, &map));
    }
    let (res, resolve_diags) = gossamer_resolve::resolve_source_file(&sf);
    if !resolve_diags.is_empty() {
        return Err(format_resolve_diags(&sf, &resolve_diags, &map));
    }
    // A labelled or defaulted argument and a std function named in value
    // position are caller-side spellings, rewritten into the one shape the
    // checker and every tier lower. The REPL drives the front-end phase by
    // phase rather than through `check_frontend`, so it runs the shared
    // normalisation itself to see the same calls a file does.
    let named_arg_diags = gossamer_types::normalize_caller_side_spellings(&mut sf, &res);
    if !named_arg_diags.is_empty() {
        return Err(format_resolve_diags(&sf, &named_arg_diags, &map));
    }
    let mut tcx = gossamer_types::TyCtxt::new();
    let (tbl, type_diags) = if inspection {
        gossamer_types::typecheck_source_file_for_repl_inspection(&sf, &res, &mut tcx)
    } else {
        gossamer_types::typecheck_source_file(&sf, &res, &mut tcx)
    };
    // REPL expressions are installed as the tail of a generated function with
    // no written return annotation. The REPL deliberately returns that value
    // as REPL output, so its tail is neither discarded nor a user error.
    // Suppress only that exact generated-body diagnostic, never one from the
    // submitted expression's children or declarations.
    let tail_spans = repl_tail_diag_spans(&sf);
    let user_type_diags: Vec<_> = type_diags
        .iter()
        .filter(|diag| !is_implicit_repl_tail_diag(diag, &tail_spans))
        .collect();
    if !user_type_diags.is_empty() {
        return Err(format_type_diags(&user_type_diags, &map));
    }
    let tail_ty_id = repl_generated_tail_expr(&sf).and_then(|expr| tbl.get(expr.id));
    let tail_ty = tail_ty_id.map_or_else(ReplValueType::unknown, |ty| {
        ReplValueType::from_ty(&tcx, ty)
    });
    let mut program = gossamer_hir::lower_source_file(&sf, &res, &tbl, &mut tcx);
    // Generated REPL functions intentionally return their tail even though
    // the user did not write a return annotation. Keep the HIR signature in
    // sync with that inferred tail type so non-inlined calls return aggregate
    // values instead of applying the ordinary implicit-unit ABI.
    if let Some(tail_ty_id) = tail_ty_id {
        for item in &mut program.items {
            if let gossamer_hir::HirItemKind::Fn(function) = &mut item.kind
                && function.name.name == entry
            {
                function.ret = Some(tail_ty_id);
                break;
            }
        }
    }
    let mut vm = gossamer_interp::Vm::new();
    // A session's next call is whatever the user types next, so every body
    // stays promotable.
    vm.set_entry_points(&[]);
    vm.load(&program, tcx, true).map_err(|e| format!("{e}"))?;
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| vm.call(entry, Vec::new())))
            .map_err(repl_panic_message)?;
    gossamer_interp::flush_runtime_stdout();
    result
        .map(|value| (value, tail_ty))
        .map_err(|e| format!("{e}"))
}

fn repl_panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    format!("panic: {message}")
}

fn repl_generated_tail_expr(sf: &gossamer_ast::SourceFile) -> Option<&gossamer_ast::Expr> {
    use gossamer_ast::{ExprKind, ItemKind};

    sf.items.iter().find_map(|item| {
        let ItemKind::Fn(decl) = &item.kind else {
            return None;
        };
        if !decl.name.name.starts_with("__irepl_") {
            return None;
        }
        let body = decl.body.as_ref()?;
        let ExprKind::Block(block) = &body.kind else {
            return None;
        };
        block.tail.as_deref()
    })
}

/// Span a diagnostic about the generated function's implicit tail carries.
///
/// The tail expression is the REPL's result rather than a discarded value,
/// so a diagnostic anchored to it is suppressed. Diagnostics reach it either
/// through the expression itself or through the enclosing body.
fn repl_tail_diag_spans(sf: &gossamer_ast::SourceFile) -> Vec<gossamer_lex::Span> {
    repl_generated_tail_expr(sf)
        .map(|expr| expr.span)
        .into_iter()
        .chain(repl_generated_body_span(sf))
        .collect()
}

fn repl_generated_body_span(sf: &gossamer_ast::SourceFile) -> Option<gossamer_lex::Span> {
    use gossamer_ast::ItemKind;

    sf.items.iter().find_map(|item| {
        let ItemKind::Fn(decl) = &item.kind else {
            return None;
        };
        if !decl.name.name.starts_with("__irepl_") {
            return None;
        }
        decl.body.as_ref().map(|body| body.span)
    })
}

fn is_implicit_repl_tail_diag(
    diag: &gossamer_types::TypeDiagnostic,
    tail_spans: &[gossamer_lex::Span],
) -> bool {
    let at_tail = tail_spans.contains(&diag.span);
    match &diag.error {
        gossamer_types::TypeError::TypeMismatch { expected, .. } => expected == "()" && at_tail,
        gossamer_types::TypeError::DiscardedResult => at_tail,
        _ => false,
    }
}

/// Renders a batch of diagnostics with the frame `gos check` and the LSP
/// produce, so one mistake reads the same wherever it is reported: stable
/// code, primary span, notes, helps, and fix suggestions.
fn format_diagnostics(
    diags: &[gossamer_diagnostics::Diagnostic],
    map: &gossamer_lex::SourceMap,
) -> String {
    let options = gossamer_diagnostics::RenderOptions { colour: false };
    let mut out = String::new();
    for diag in diags {
        out.push_str(&gossamer_diagnostics::render(diag, map, options));
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Renders hard type-checker failures before the REPL can lower a program.
/// Keeping this gate here is essential: lowering after a rejected call used
/// to let missing or wrongly typed arguments reach permissive runtime shims,
/// which then silently substituted defaults.
fn format_type_diags<D>(diags: &[D], map: &gossamer_lex::SourceMap) -> String
where
    D: std::borrow::Borrow<gossamer_types::TypeDiagnostic>,
{
    let structured = diags
        .iter()
        .map(|diag| diag.borrow().to_diagnostic())
        .collect::<Vec<_>>();
    format_diagnostics(&structured, map)
}

fn format_resolve_diags(
    sf: &gossamer_ast::SourceFile,
    diags: &[gossamer_resolve::ResolveDiagnostic],
    map: &gossamer_lex::SourceMap,
) -> String {
    let in_scope = collect_source_file_names(sf);
    let structured = diags
        .iter()
        .map(|diag| diag.to_diagnostic(&in_scope))
        .collect::<Vec<_>>();
    format_diagnostics(&structured, map)
}

fn collect_source_file_names(sf: &gossamer_ast::SourceFile) -> Vec<&str> {
    use gossamer_ast::ItemKind;

    let mut out = Vec::new();
    for item in &sf.items {
        let name = match &item.kind {
            ItemKind::Fn(decl) => decl.name.name.as_str(),
            ItemKind::Struct(decl) => decl.name.name.as_str(),
            ItemKind::Enum(decl) => decl.name.name.as_str(),
            ItemKind::Trait(decl) => decl.name.name.as_str(),
            ItemKind::TypeAlias(decl) => decl.name.name.as_str(),
            ItemKind::Const(decl) => decl.name.name.as_str(),
            ItemKind::Static(decl) => decl.name.name.as_str(),
            ItemKind::Mod(decl) => decl.name.name.as_str(),
            ItemKind::Impl(_) | ItemKind::AttrItem(_) => continue,
        };
        out.push(name);
    }
    out
}

/// Renders a parse-diagnostic batch through the shared frame, so the REPL
/// reports the same code, span, and fix as `gos check` and the LSP.
fn format_parse_diags(
    diags: &[gossamer_parse::ParseDiagnostic],
    map: &gossamer_lex::SourceMap,
) -> String {
    let structured = diags
        .iter()
        .map(gossamer_parse::ParseDiagnostic::to_diagnostic)
        .collect::<Vec<_>>();
    format_diagnostics(&structured, map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repl_output_wraps_to_narrow_columns_and_keeps_indent() {
        let wrapped = wrap_repl_line(
            "    This description is intentionally long enough to wrap cleanly.",
            32,
        );
        assert!(wrapped.len() > 1);
        assert!(wrapped.iter().all(|line| line.chars().count() <= 32));
        assert!(wrapped.iter().skip(1).all(|line| line.starts_with("    ")));
    }

    #[test]
    fn repl_catalog_wrapping_uses_a_small_consistent_indent() {
        let mut output = String::new();
        push_catalog_entry(
            &mut output,
            "std::strings::replace",
            "fn",
            "Replaces every matching substring in the value.",
        );
        assert!(output.starts_with("std::strings::replace [fn]\n  Replaces"));
        let wrapped = wrap_repl_line("  Replaces every matching substring in the value.", 32);
        assert!(wrapped.len() > 1);
        assert!(wrapped.iter().all(|line| line.starts_with("  ")));
    }

    #[test]
    fn repl_metadata_never_exposes_untyped_runtime_registrations() {
        let incomplete = core_method_entries()
            .into_iter()
            .filter(|entry| entry.signature.contains("..."))
            .map(|entry| format!("{}::{}", entry.owner, entry.name))
            .collect::<Vec<_>>();
        assert!(
            incomplete.is_empty(),
            "REPL must not expose runtime registrations without concrete signatures: {incomplete:?}"
        );
    }

    #[test]
    fn sync_map_info_lists_complete_callable_signatures() {
        let entries = core_method_entries()
            .into_iter()
            .filter(|entry| entry.owner == "sync::Map")
            .collect::<Vec<_>>();
        assert_eq!(
            entries.len(),
            7,
            "incomplete sync::Map surface: {entries:?}"
        );
        assert!(
            entries.iter().all(|entry| {
                !entry.signature.trim().is_empty()
                    && entry.signature.starts_with("fn ")
                    && !entry.doc.starts_with("Built-in ")
            }),
            "sync::Map contains placeholder metadata: {entries:?}"
        );
        let rendered = render_catalog_query_matches("sync::Map", false);
        for expected in [
            "sync::Map::new() -> sync::Map [associated function]",
            "sync::Map::insert(self: sync::Map, key: String, value: String) -> () [method]",
            "sync::Map::get(self: sync::Map, key: String) -> Option<String> [method]",
            "sync::Map::remove(self: sync::Map, key: String) -> () [method]",
            "sync::Map::len(self: sync::Map) -> i64 [method]",
            "sync::Map::contains_key(self: sync::Map, key: String) -> bool [method]",
            "sync::Map::keys(self: sync::Map) -> Vec<String> [method]",
        ] {
            assert!(
                rendered.contains(expected),
                "missing `{expected}`:\n{rendered}"
            );
        }
    }

    /// Completion offers exactly what discovery reports: every member
    /// `%explain` lists for a binding is a candidate after that binding's
    /// dot, whatever its type. A fixed array is the receiver this most
    /// easily misses, because its surface is derived from `Vec` rather
    /// than written out.
    #[test]
    fn every_member_explain_lists_is_completed() {
        let declarations = vec![
            "use std::time".to_string(),
            "struct Point { x: i64, y: i64 }".to_string(),
            "impl Point { fn norm(&self) -> i64 { self.x + self.y } }".to_string(),
        ];
        let lets = vec![
            "let array = [1, 2, 3]".to_string(),
            "let vector = #[1, 2, 3]".to_string(),
            "let text = \"hi\"".to_string(),
            "let scores = {\"a\": 1}".to_string(),
            "let sorted = BTreeMap::from([(1, 2)])".to_string(),
            "let names = #{\"a\"}".to_string(),
            "let deque = Deque::from([1, 2])".to_string(),
            "let queue = Queue::from([1, 2])".to_string(),
            "let stack = Stack::from([1, 2])".to_string(),
            "let heap = MinHeap::from([1, 2])".to_string(),
            "let maybe = Some(1)".to_string(),
            "let outcome: Result<i64, String> = Ok(1)".to_string(),
            "let pair = (1, 2)".to_string(),
            "let point = Point { x: 1, y: 2 }".to_string(),
            "let span = time::Duration::from_millis(5)".to_string(),
            "let mut cursor = #[1, 2].iter()".to_string(),
        ];
        for name in [
            "array", "vector", "text", "scores", "sorted", "names", "deque", "queue", "stack",
            "heap", "maybe", "outcome", "pair", "point", "span", "cursor",
        ] {
            let var = ReplBindingVar {
                name: name.to_string(),
                mutable: name == "cursor",
            };
            let ty = infer_repl_binding_type(&declarations, &lets, name)
                .unwrap_or_else(|error| panic!("infer {name}: {error}"));
            let surface = BindingSurface::of(&var, &ty);
            let members = binding_member_names(&surface, &declarations);
            assert!(!members.is_empty(), "{name} reaches no members");
            let listing = repl_binding_listing_for(&declarations, &lets, &var)
                .unwrap_or_else(|error| panic!("explain {name}: {error}"));
            let prefix = format!("{name}.");
            for line in listing.lines() {
                let line = line.trim();
                let Some(call) = line.strip_prefix(&prefix) else {
                    continue;
                };
                let member = call
                    .split(['(', '<', ' ', ':'])
                    .next()
                    .expect("a member name");
                assert!(
                    members.contains(&member.to_string()),
                    "{name}.{member} is listed but not completed: {members:?}"
                );
            }
        }
        let point = ReplBindingVar {
            name: "point".to_string(),
            mutable: false,
        };
        let ty = infer_repl_binding_type(&declarations, &lets, "point").expect("infer point");
        let members = binding_member_names(&BindingSurface::of(&point, &ty), &declarations);
        assert_eq!(members, vec!["norm", "x", "y"], "{members:?}");
    }

    /// A binding that cannot be written through does not complete a method
    /// that would write through it.
    #[test]
    fn an_immutable_binding_completes_no_mutating_method() {
        let lets = vec!["let values = #[1, 2, 3]".to_string()];
        let var = ReplBindingVar {
            name: "values".to_string(),
            mutable: false,
        };
        let ty = infer_repl_binding_type(&[], &lets, "values").expect("infer values");
        let members = binding_member_names(&BindingSurface::of(&var, &ty), &[]);
        assert!(members.contains(&"len".to_string()), "{members:?}");
        assert!(!members.contains(&"push".to_string()), "{members:?}");

        let mutable = ReplBindingVar {
            name: "values".to_string(),
            mutable: true,
        };
        let members = binding_member_names(&BindingSurface::of(&mutable, &ty), &[]);
        assert!(members.contains(&"push".to_string()), "{members:?}");
    }

    /// Every catalog entry is filed under a type a user can name. A
    /// receiver written as a bracketed sequence shape names no type, so the
    /// entry keeps the owner it already carries.
    #[test]
    fn every_catalog_owner_is_a_named_type() {
        let anonymous: Vec<String> = core_method_entries()
            .into_iter()
            .filter(|entry| {
                entry.owner.trim().is_empty()
                    || !entry
                        .owner
                        .starts_with(|ch: char| ch.is_alphabetic() || ch == '_')
            })
            .map(|entry| format!("{}::{}", entry.owner, entry.name))
            .collect();
        assert!(
            anonymous.is_empty(),
            "unnamed catalog owners: {anonymous:?}"
        );
    }

    /// A constructor is named through its type, so it is not offered on a
    /// value of that type.
    #[test]
    fn a_constructor_is_not_a_member_of_its_own_type() {
        let entries = core_method_entries();
        for (owner, name) in [("Duration", "from_millis"), ("Instant", "now")] {
            let entry = entries
                .iter()
                .find(|entry| entry.owner == owner && entry.name == name)
                .unwrap_or_else(|| panic!("{owner}::{name} is cataloged"));
            assert_eq!(entry.kind, "assoc", "{owner}::{name}");
        }
    }

    #[test]
    fn repl_binding_type_inference_tracks_queue_and_stack_owners() {
        let lets = vec![
            "let mut queue = Queue::from([1, 2, 3])".to_string(),
            "let mut stack = Stack::from([1, 2, 3])".to_string(),
        ];

        let queue = infer_repl_binding_type(&[], &lets, "queue").expect("infer queue type");
        let stack = infer_repl_binding_type(&[], &lets, "stack").expect("infer stack type");

        assert_eq!(queue.method_owner.as_deref(), Some("Queue"));
        assert_eq!(stack.method_owner.as_deref(), Some("Stack"));
    }

    #[test]
    fn audited_byte_handle_builtins_have_concrete_public_contracts() {
        let mut incomplete = core_method_entries()
            .into_iter()
            .filter(|entry| matches!(entry.owner.as_str(), "Buffer" | "Builder"))
            .filter(|entry| entry.signature.contains("..."))
            .map(|entry| format!("{}::{}", entry.owner, entry.name))
            .collect::<Vec<_>>();
        incomplete.sort();
        assert!(
            incomplete.is_empty(),
            "runtime type builtins without concrete public signatures: {incomplete:?}"
        );
    }

    #[test]
    fn string_methods_reuse_the_complete_stdlib_signature_catalog() {
        let mut incomplete = core_method_entries()
            .into_iter()
            .filter(|entry| entry.owner == "String")
            .filter(|entry| entry.signature.contains("..."))
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        incomplete.sort();
        assert!(
            incomplete.is_empty(),
            "String methods without concrete public signatures: {incomplete:?}"
        );
    }

    #[test]
    fn repl_metadata_does_not_leak_runtime_registration_text_for_core_types() {
        let checked = [
            "String", "Vec", "Map", "BTreeMap", "Set", "BTreeSet", "Deque", "Queue", "Stack",
            "MaxHeap", "MinHeap", "Option", "Result",
        ];
        let mut leaked = Vec::new();
        for entry in core_method_entries() {
            if checked.contains(&entry.owner.as_str())
                && entry.doc.contains("Runtime builtin registered")
            {
                leaked.push(format!("{}::{}", entry.owner, entry.name));
            }
        }
        assert!(
            leaked.is_empty(),
            "core type methods should have user-facing REPL docs: {leaked:?}"
        );
    }

    #[test]
    fn repl_metadata_keeps_checked_core_collection_methods() {
        for query in [
            "String::parse",
            "Vec::push",
            "Vec::get",
            "Vec::capacity",
            "Vec::reserve",
            "Vec::truncate",
            "Map::insert",
            "BTreeMap::insert",
            "BTreeMap::from",
            "Set::union",
            "BTreeSet::union",
            "Deque::push_back",
            "Deque::clear",
            "Queue::push",
            "Queue::len",
            "Stack::pop",
            "Stack::peek",
            "MaxHeap::push",
            "MinHeap::push",
            "Option::map",
            "Result::map_err",
        ] {
            assert!(
                !matching_core_methods(query).is_empty(),
                "missing REPL metadata for {query}"
            );
        }
    }

    #[test]
    fn every_std_defined_type_is_available_to_info() {
        let mut missing = Vec::new();
        for module in gossamer_std::registry::modules() {
            for item in module.items {
                if item.kind == StdItemKind::Type
                    && matching_items(&format!("{}::{}", module.path, item.name)).is_empty()
                {
                    missing.push(format!("{}::{}", module.path, item.name));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "std types missing from %info: {missing:?}"
        );
    }

    #[test]
    fn every_detailed_catalog_description_is_followed_by_an_example() {
        for method in core_method_entries() {
            let mut rendered = String::new();
            push_core_method_match(&mut rendered, &method, true);
            assert!(
                rendered.contains(&format!("    {}\n    Builtin\n    Example: ", method.doc)),
                "missing example for {}::{}:\n{rendered}",
                method.owner,
                method.name
            );
        }

        for module in gossamer_std::registry::modules() {
            let mut rendered = String::new();
            push_module_match(&mut rendered, module, true);
            assert!(
                rendered.contains(&format!(
                    "    {}\n    Defined in: {}\n    Example: ",
                    module.summary, module.path
                )),
                "missing module example for {}:\n{rendered}",
                module.path
            );
            for item in module.items {
                let mut rendered = String::new();
                push_item_match(&mut rendered, module, item, true);
                assert!(
                    rendered.contains(&format!(
                        "    {}\n    Defined in: {}\n    Example: ",
                        item.doc, module.path
                    )),
                    "missing item example for {}::{}:\n{rendered}",
                    module.path,
                    item.name
                );
            }
        }

        for builtin in PRELUDE_BUILTINS {
            let mut rendered = String::new();
            push_catalog_match(
                &mut rendered,
                builtin.name,
                "builtin",
                builtin.signature,
                builtin.doc,
                None,
                true,
            );
            assert!(
                rendered.contains(&format!("    {}\n    Builtin\n    Example: ", builtin.doc)),
                "missing builtin example for {}:\n{rendered}",
                builtin.name
            );
        }
        for builtin in BUILTIN_MACROS {
            let mut rendered = String::new();
            push_catalog_match(
                &mut rendered,
                builtin.name,
                "builtin",
                builtin.signature,
                builtin.doc,
                None,
                true,
            );
            assert!(
                rendered.contains(&format!("    {}\n    Builtin\n    Example: ", builtin.doc)),
                "missing macro example for {}:\n{rendered}",
                builtin.name
            );
        }

        let directory = render_stdlib_dir();
        assert_eq!(
            directory.matches("\n  Example: ").count(),
            directory.matches(" [module]\n").count(),
            "every directory description must have an example:\n{directory}"
        );
    }

    #[test]
    fn every_runtime_method_on_a_std_defined_type_is_available_to_explain() {
        let std_type_names = gossamer_std::registry::modules()
            .iter()
            .flat_map(|module| module.items)
            .filter(|item| item.kind == StdItemKind::Type)
            .map(|item| item.name)
            .collect::<std::collections::BTreeSet<_>>();
        let catalog = core_method_entries();
        let mut missing = Vec::new();
        for registered in gossamer_interp::registered_names() {
            let Some((owner, name)) = registered_core_method_path(registered) else {
                continue;
            };
            // The runtime's builtin names are global, so one registration
            // serves every receiver: discovery follows what the checker
            // resolves on the owner, and so does this gate.
            if !gossamer_types::core_type_accepts_method(&owner, &name) {
                continue;
            }
            let short_owner = owner.rsplit("::").next().unwrap_or(&owner);
            if std_type_names.contains(short_owner)
                && !catalog
                    .iter()
                    .any(|entry| entry.owner == owner && entry.name == name)
            {
                missing.push(format!("{owner}::{name}"));
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "runtime methods on std types missing from %explain: {missing:?}"
        );
    }

    /// Discovery must not advertise a call the checker refuses: a name the
    /// runtime registers globally is not evidence a given receiver has it.
    #[test]
    fn explain_never_advertises_a_method_the_checker_rejects() {
        let mut advertised = Vec::new();
        for entry in core_method_entries() {
            if entry.kind != "method" {
                continue;
            }
            if !gossamer_types::core_type_accepts_method(&entry.owner, &entry.name) {
                advertised.push(format!("{}::{}", entry.owner, entry.name));
            }
        }
        advertised.sort();
        advertised.dedup();
        assert!(
            advertised.is_empty(),
            "%info advertises methods the checker rejects: {advertised:?}"
        );
    }

    #[test]
    fn option_has_one_complete_method_surface() {
        let expected = gossamer_std::registry::module("std::option")
            .expect("std::option module")
            .items
            .iter()
            .filter(|item| item.kind == StdItemKind::Function)
            .map(|item| item.name)
            .collect::<std::collections::BTreeSet<_>>();
        let entries = core_method_entries()
            .into_iter()
            .filter(|entry| entry.owner == "Option")
            .collect::<Vec<_>>();
        let actual = entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(actual, expected);
        assert_eq!(entries.len(), expected.len(), "duplicate Option metadata");
        assert!(entries.iter().all(|entry| !entry.signature.contains("...")));
    }

    #[test]
    fn iterator_info_and_explain_have_complete_methods() {
        assert!(!matching_core_namespaces("Iterator").is_empty());
        let entries = core_method_entries()
            .into_iter()
            .filter(|entry| entry.owner == "Iterator")
            .collect::<Vec<_>>();
        assert!(!entries.is_empty(), "Iterator has no %explain methods");
        assert!(entries.iter().all(|entry| !entry.signature.contains("...")));
        assert!(entries.iter().all(|entry| !entry.signature.is_empty()));
        assert!(
            entries
                .iter()
                .all(|entry| gossamer_types::is_iterator_method(&entry.name))
        );
        let names = entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        // An iterator answers the whole sequence surface, including the
        // terminals and the eager-only operations, which drain it first.
        for available in ["for_each", "position", "max_by_key"] {
            assert!(names.contains(available), "{available} is missing");
        }
        let map = entries.iter().find(|entry| entry.name == "map").unwrap();
        assert!(map.signature.contains("self: Iterator<T>"));
        assert!(map.signature.ends_with("-> Iterator<U>"));
        let fold = entries.iter().find(|entry| entry.name == "fold").unwrap();
        assert!(fold.signature.contains("init: U, f: Fn(U, T) -> U"));
    }

    /// A rendered signature states the type the call actually has: a lazy
    /// adapter answers with an iterator on both iterator-shaped receivers,
    /// and a terminal never does.
    #[test]
    fn iterator_signatures_state_the_type_the_call_has() {
        for owner in ["Iterator", "Range"] {
            let entries = core_method_entries()
                .into_iter()
                .filter(|entry| entry.owner == owner)
                .collect::<Vec<_>>();
            assert!(!entries.is_empty(), "{owner} has no methods");
            for entry in &entries {
                let lazy = gossamer_types::iterator_adapter_is_lazy(&entry.name);
                // The function's own return is the tail after the last arrow;
                // an earlier one belongs to a closure parameter.
                let declared_return = entry
                    .signature
                    .rsplit("->")
                    .next()
                    .unwrap_or_default()
                    .trim();
                let returns_iterator = declared_return.starts_with("Iterator<");
                assert_eq!(
                    lazy, returns_iterator,
                    "{owner}::{} renders `{}`, which disagrees with its \
                     lazy/terminal classification",
                    entry.name, entry.signature
                );
                assert!(
                    !(lazy && declared_return.starts_with("Vec<")),
                    "{owner}::{} renders a materialised return for a lazy adapter: {}",
                    entry.name,
                    entry.signature
                );
            }
        }
    }

    #[test]
    fn history_search_filters_saved_and_current_entries() {
        let history = vec![
            "let prior = 1".to_string(),
            "prior".to_string(),
            "let current = 2".to_string(),
        ];
        assert_eq!(
            render_repl_history(&history, "^let").unwrap(),
            ["let prior = 1", "let current = 2"]
        );
    }

    #[test]
    fn repl_metadata_covers_typechecked_vec_method_surface() {
        let checked_vec_methods = [
            "clone",
            "push",
            "pop",
            "insert",
            "remove",
            "clear",
            "extend",
            "extend_from_slice",
            "truncate",
            "reserve",
            "reserve_exact",
            "capacity",
            "len",
            "is_empty",
            "slice",
            "first",
            "last",
            "get",
            "rev",
            "dedup",
            "take",
            "skip",
            "step_by",
            "chain",
            "zip",
            "windows",
            "chunks",
            "pairwise",
            "flatten",
            "join",
            "contains",
            "index_of",
            "count_of",
            "sort",
            "sort_by",
            "sort_by_key",
            "reverse",
            "swap",
            "fill",
            "map",
            "filter",
            "fold",
            "for_each",
            "any",
            "all",
            "find",
            "position",
            "count",
            "enumerate",
            "sum",
            "min",
            "max",
            "min_by_key",
            "max_by_key",
        ];
        let mut missing = Vec::new();
        for name in checked_vec_methods {
            let query = format!("Vec::{name}");
            if matching_core_methods(&query).is_empty() {
                missing.push(query);
            }
        }
        assert!(
            missing.is_empty(),
            "missing REPL metadata for typechecked Vec methods: {missing:?}"
        );
    }

    #[test]
    fn sequence_catalogs_match_the_canonical_type_surfaces() {
        let catalog = core_method_entries();
        for owner in ["Array", "Slice"] {
            let methods = catalog
                .iter()
                .filter(|entry| entry.owner == owner && entry.kind == "method")
                .collect::<Vec<_>>();
            assert!(!methods.is_empty(), "{owner} catalog is empty");
            for method in methods {
                let expected = if owner == "Array" {
                    gossamer_types::is_array_sequence_method(&method.name)
                } else {
                    gossamer_types::is_slice_sequence_method(&method.name)
                };
                assert!(expected, "{owner} unexpectedly exposes {}", method.name);
                assert!(
                    !gossamer_types::is_vec_only_sequence_method(&method.name),
                    "{owner} exposes Vec-only method {}",
                    method.name
                );
            }
        }

        // Resizing and capacity stay Vec-only; a traversal reads the values
        // any sequence already holds, so it is on arrays and slices too.
        for name in ["push", "pop", "capacity", "reserve"] {
            assert!(
                catalog
                    .iter()
                    .any(|entry| entry.owner == "Vec" && entry.name == name),
                "Vec is missing {name}"
            );
            assert!(
                !catalog.iter().any(|entry| {
                    matches!(entry.owner.as_str(), "Array" | "Slice") && entry.name == name
                }),
                "{name} leaked from Vec into Array or Slice"
            );
        }

        for name in ["map", "fold", "sum"] {
            for owner in ["Vec", "Array", "Slice"] {
                assert!(
                    catalog
                        .iter()
                        .any(|entry| entry.owner == owner && entry.name == name),
                    "{owner} is missing {name}"
                );
            }
        }

        let array_clone = catalog
            .iter()
            .find(|entry| entry.owner == "Array" && entry.name == "clone")
            .expect("Array::clone metadata");
        assert!(array_clone.signature.ends_with("-> [T; N]"));
        assert!(
            !catalog
                .iter()
                .any(|entry| entry.owner == "Slice" && entry.name == "clone")
        );
    }
}

/// Folds the session's facts about a type into the catalog's rendering
/// of it, under the one header both would otherwise print.
///
/// The catalog's leading entry is its header line plus the indented
/// lines describing it; the session's sections belong directly after
/// that, ahead of the method list.
fn splice_session_into_catalog(session: &str, catalog: &str) -> String {
    let mut session_lines = session.lines();
    let session_header = session_lines.next().unwrap_or_default();
    let session_body: Vec<&str> = session_lines.collect();
    let mut catalog_lines = catalog.lines();
    let Some(catalog_header) = catalog_lines.next() else {
        return session.to_string();
    };
    if catalog_header != session_header {
        return format!("{session}\n{catalog}");
    }
    let rest: Vec<&str> = catalog_lines.collect();
    let lead = rest
        .iter()
        .take_while(|line| line.starts_with("    "))
        .count();
    let mut out = vec![catalog_header];
    out.extend_from_slice(&rest[..lead]);
    out.extend(session_body);
    out.extend_from_slice(&rest[lead..]);
    out.join("\n")
}

/// `true` when a catalog lookup found no entries. The catalog reports a
/// miss as ordinary text rather than an error, so a caller merging its
/// result has to recognise the sentence.
fn is_nothing_found(text: &str) -> bool {
    text.trim_start().starts_with("nothing found for")
}

/// The bare type name inside a rendered type, so `&mut Point` and
/// `Vec<Point>` both look up under `Point`.
fn base_type_name(rendered: &str) -> &str {
    rendered
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim()
        .split(['<', '['])
        .next()
        .unwrap_or(rendered)
        .trim()
}

/// `true` when the session declares anything about `name`.
fn index_has_facts(declarations: &[String], name: &str) -> bool {
    !session_index(declarations).is_empty_for(name)
}

/// What the session's stored declarations say about the types in it.
///
/// `%info` otherwise answers only from the stdlib catalog, so a struct
/// or trait declared at the prompt has nowhere to be looked up.
#[derive(Debug, Default)]
struct SessionIndex {
    /// Struct name to its declared fields, in declaration order.
    fields: BTreeMap<String, Vec<(String, String)>>,
    /// Type name to the traits implemented for it in this session.
    implements: BTreeMap<String, Vec<String>>,
    /// Trait name to the types implementing it.
    implementors: BTreeMap<String, Vec<String>>,
    /// Type name to its methods, each tagged with the trait it came from
    /// or `None` for an inherent `impl`.
    methods: BTreeMap<String, Vec<(String, Option<String>)>>,
    /// Trait name to its declared method signatures.
    trait_methods: BTreeMap<String, Vec<String>>,
    /// Declared item name to the kind label `%info` prints for it.
    kinds: BTreeMap<String, &'static str>,
    /// Type-alias name to the type it stands for, as written.
    aliases: BTreeMap<String, String>,
}

impl SessionIndex {
    /// `true` when nothing in the session declares `name`.
    fn is_empty_for(&self, name: &str) -> bool {
        !self.kinds.contains_key(name)
            && !self.aliases.contains_key(name)
            && !self.fields.contains_key(name)
            && !self.implements.contains_key(name)
            && !self.implementors.contains_key(name)
            && !self.methods.contains_key(name)
    }

    /// Every name the session declares, in sorted order.
    fn declared_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .kinds
            .keys()
            .chain(self.aliases.keys())
            .chain(self.fields.keys())
            .chain(self.implements.keys())
            .chain(self.implementors.keys())
            .chain(self.methods.keys())
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

/// Renders one AST type as source.
fn render_type(ty: &gossamer_ast::Type) -> String {
    let mut printer = gossamer_ast::Printer::new();
    printer.print_type(ty);
    printer.finish()
}

/// Renders a trait bound's path as source.
fn render_trait_path(bound: &gossamer_ast::TraitBound) -> String {
    let mut printer = gossamer_ast::Printer::new();
    printer.print_type_path(&bound.path);
    printer.finish()
}

/// Renders a method's parameter list and return type, without its body.
fn render_fn_signature(decl: &gossamer_ast::FnDecl) -> String {
    use gossamer_ast::{FnParam, Receiver};

    let params = decl
        .params
        .iter()
        .map(|param| match param {
            FnParam::Receiver(Receiver::Owned) => "self".to_string(),
            FnParam::Receiver(Receiver::RefShared) => "&self".to_string(),
            FnParam::Receiver(Receiver::RefMut) => "&mut self".to_string(),
            FnParam::Typed { pattern, ty, .. } => {
                let mut printer = gossamer_ast::Printer::new();
                printer.print_pattern(pattern);
                format!("{}: {}", printer.finish(), render_type(ty))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let ret = decl
        .ret
        .as_ref()
        .map_or_else(String::new, |ty| format!(" -> {}", render_type(ty)));
    format!("({params}){ret}")
}

/// The base name of a self type, so `impl Trait for Wrapper<T>` indexes
/// under `Wrapper`.
fn self_type_name(ty: &gossamer_ast::Type) -> String {
    let rendered = render_type(ty);
    rendered
        .split(['<', '['])
        .next()
        .unwrap_or(&rendered)
        .trim()
        .trim_start_matches('&')
        .trim()
        .to_string()
}

/// Builds the session's type index from the declarations replayed into
/// every REPL evaluation.
fn session_index(declarations: &[String]) -> SessionIndex {
    use gossamer_ast::{ImplItem, ItemKind, StructBody, TraitItem};

    let mut index = SessionIndex::default();
    for declaration in declarations {
        let mut map = gossamer_lex::SourceMap::new();
        let file = map.add_file("irepl-session-index".to_string(), declaration.clone());
        let (source_file, diags) = gossamer_parse::parse_source_file(declaration, file);
        if !diags.is_empty() {
            continue;
        }
        for item in &source_file.items {
            match &item.kind {
                ItemKind::Struct(decl) => {
                    let name = decl.name.name.clone();
                    index.kinds.insert(name.clone(), "struct");
                    let fields = match &decl.body {
                        StructBody::Named(fields) => fields
                            .iter()
                            .map(|field| (field.name.name.clone(), render_type(&field.ty)))
                            .collect(),
                        StructBody::Tuple(fields) => fields
                            .iter()
                            .enumerate()
                            .map(|(position, field)| (position.to_string(), render_type(&field.ty)))
                            .collect(),
                        StructBody::Unit => Vec::new(),
                    };
                    index.fields.insert(name, fields);
                }
                ItemKind::Enum(decl) => {
                    index.kinds.insert(decl.name.name.clone(), "enum");
                }
                ItemKind::Trait(decl) => {
                    let name = decl.name.name.clone();
                    index.kinds.insert(name.clone(), "trait");
                    let signatures = decl
                        .items
                        .iter()
                        .filter_map(|trait_item| match trait_item {
                            TraitItem::Fn(fn_decl) => Some(format!(
                                "fn {}{}",
                                fn_decl.name.name,
                                render_fn_signature(fn_decl)
                            )),
                            _ => None,
                        })
                        .collect();
                    index.trait_methods.insert(name, signatures);
                }
                ItemKind::Fn(decl) => {
                    index.kinds.insert(decl.name.name.clone(), "fn");
                }
                ItemKind::TypeAlias(decl) => {
                    index.kinds.insert(decl.name.name.clone(), "type");
                    index
                        .aliases
                        .insert(decl.name.name.clone(), render_type(&decl.ty));
                }
                ItemKind::Impl(decl) => {
                    let owner = self_type_name(&decl.self_ty);
                    let trait_name = decl.trait_ref.as_ref().map(render_trait_path);
                    if let Some(trait_name) = &trait_name {
                        push_unique(
                            index.implements.entry(owner.clone()).or_default(),
                            trait_name.clone(),
                        );
                        push_unique(
                            index.implementors.entry(trait_name.clone()).or_default(),
                            owner.clone(),
                        );
                        // A name reached only through an `impl X for Y` header
                        // is a trait; without this it would fall to the
                        // default kind and be listed as a type.
                        index.kinds.entry(trait_name.clone()).or_insert("trait");
                    }
                    let methods = index.methods.entry(owner).or_default();
                    for impl_item in &decl.items {
                        if let ImplItem::Fn(fn_decl) = impl_item {
                            methods.push((
                                format!("{}{}", fn_decl.name.name, render_fn_signature(fn_decl)),
                                trait_name.clone(),
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    index
}

/// Names the session declares that a type position may name, for the
/// highlighter: structs, enums, traits, and aliases.
pub(crate) fn session_type_names(declarations: &[String]) -> std::collections::HashSet<String> {
    session_index(declarations)
        .kinds
        .into_iter()
        .filter(|(_, kind)| matches!(*kind, "struct" | "enum" | "trait" | "type"))
        .map(|(name, _)| name)
        .collect()
}

/// Appends `value` unless the list already carries it.
fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// Renders what the session knows about `name`: a struct's fields, the
/// traits implemented for a type, a trait's methods and implementors.
///
/// `receiver` names the binding a method would be called on, so
/// `%explain p` shows `p.area()` where `%info Point` shows
/// `Point::area(&self)`. Returns `None` when the session declares
/// nothing under `name`.
fn render_session_type(index: &SessionIndex, name: &str, receiver: Option<&str>) -> Option<String> {
    if index.is_empty_for(name) {
        return None;
    }
    let mut out = String::new();
    if let Some(target) = index.aliases.get(name) {
        out.push_str(&format!("  alias of\n    {target}\n"));
    }
    if let Some(fields) = index.fields.get(name)
        && !fields.is_empty()
    {
        out.push_str("  fields\n");
        for (field, ty) in fields {
            out.push_str(&format!("    {field}: {ty}\n"));
        }
    }
    if let Some(traits) = index.implements.get(name)
        && !traits.is_empty()
    {
        out.push_str("  implements\n");
        for trait_name in traits {
            out.push_str(&format!("    {trait_name}\n"));
        }
    }
    if let Some(signatures) = index.trait_methods.get(name)
        && !signatures.is_empty()
    {
        out.push_str("  methods\n");
        for signature in signatures {
            out.push_str(&format!("    {signature}\n"));
        }
    }
    if let Some(methods) = index.methods.get(name)
        && !methods.is_empty()
    {
        out.push_str("  methods\n");
        for (signature, from_trait) in methods {
            let origin = from_trait
                .as_ref()
                .map_or_else(|| "[inherent]".to_string(), |t| format!("[{t}]"));
            // An associated function has no receiver to be called through, so
            // it keeps its qualified spelling even where a binding is named.
            match receiver.filter(|_| receiver_method_name(signature).is_some()) {
                Some(binding) => {
                    let call = signature.replacen("(&mut self, ", "(", 1);
                    let call = call.replacen("(&mut self)", "()", 1);
                    let call = call.replacen("(&self, ", "(", 1);
                    let call = call.replacen("(&self)", "()", 1);
                    let call = call.replacen("(self, ", "(", 1);
                    let call = call.replacen("(self)", "()", 1);
                    out.push_str(&format!("    {binding}.{call} {origin}\n"));
                }
                None => out.push_str(&format!("    {name}::{signature} {origin}\n")),
            }
        }
    }
    if let Some(types) = index.implementors.get(name)
        && !types.is_empty()
    {
        out.push_str("  implemented by\n");
        for ty in types {
            out.push_str(&format!("    {ty}\n"));
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out.trim_end().to_string())
}

/// The `%info` rendering for each name the session declares that `query`
/// matches.
fn repl_session_info(index: &SessionIndex, query: &str) -> Option<String> {
    let mut sections = Vec::new();
    for name in index.declared_names() {
        if !symbol_query_matches(name, query) {
            continue;
        }
        let Some(body) = render_session_type(index, name, None) else {
            continue;
        };
        let kind = index.kinds.get(name).copied().unwrap_or("type");
        sections.push(format!("{name} [{kind}]\n{body}"));
    }
    (!sections.is_empty()).then(|| sections.join("\n"))
}
