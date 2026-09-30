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
        outln!("{}", style_repl_output_line(line));
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

/// A built-in operator, named in `%info` by its spelling.
struct CoreOperatorHelp {
    spelling: &'static str,
    signature: &'static str,
    doc: &'static str,
    example: &'static str,
}

// Operators have no type or module to be found through, so each one the
// language defines beyond the arithmetic every reader already knows is listed
// by the spelling a program writes.
const CORE_OPERATORS: &[CoreOperatorHelp] = &[
    CoreOperatorHelp {
        spelling: "+%",
        signature: "a +% b -> T",
        doc: "Wrapping add: adds two values of one integer type `T` with two's-complement \
              wrapping at `T`'s declared width, on every tier and in every build profile. \
              Binds like `+` (level 6). A float or `String` operand is GT0003; \
              `x.wrapping_add(y)` is not a method (GT0087).",
        example: "let hash: u32 = (hash << 5) +% hash +% b as u32",
    },
    CoreOperatorHelp {
        spelling: "-%",
        signature: "a -% b -> T",
        doc: "Wrapping subtract: subtracts two values of one integer type `T` with \
              two's-complement wrapping at `T`'s declared width, on every tier and in \
              every build profile. Binds like `-` (level 6). A float or `String` operand \
              is GT0003.",
        example: "let before: u8 = 0 as u8 -% 1",
    },
    CoreOperatorHelp {
        spelling: "*%",
        signature: "a *% b -> T",
        doc: "Wrapping multiply: multiplies two values of one integer type `T` with \
              two's-complement wrapping at `T`'s declared width, on every tier and in \
              every build profile. Binds like `*` (level 5). A float or `String` operand \
              is GT0003; `x.wrapping_mul(y)` is not a method (GT0087).",
        example: "let mixed: u32 = hash *% 16_777_619",
    },
    CoreOperatorHelp {
        spelling: "+%=",
        signature: "place +%= value",
        doc: "Compound wrapping add: `place = place +% value`, evaluating the place once. \
              The place must be writable and hold an integer.",
        example: "let mut total: i32 = 2_147_483_000; total +%= 1_000",
    },
    CoreOperatorHelp {
        spelling: "-%=",
        signature: "place -%= value",
        doc: "Compound wrapping subtract: `place = place -% value`, evaluating the place \
              once. The place must be writable and hold an integer.",
        example: "let mut countdown: u8 = 2; countdown -%= 5",
    },
    CoreOperatorHelp {
        spelling: "*%=",
        signature: "place *%= value",
        doc: "Compound wrapping multiply: `place = place *% value`, evaluating the place \
              once. The place must be writable and hold an integer.",
        example: "let mut hash: u32 = 2_166_136_261; hash *%= 16_777_619",
    },
];

// Core receiver and associated methods are runtime builtins, not stdlib module
// exports. Keep them visible to REPL discovery so working calls such as
// `"123".parse()` are not hidden from `%help` and `%info`.
const CORE_METHODS: &[CoreMethodHelp] = &[
    CoreMethodHelp {
        owner: "Simd",
        name: "splat",
        kind: "assoc",
        signature: "fn splat(value: T) -> Simd<T, N>",
        doc: "A vector whose every lane holds `value`. The lane count comes from the \
              type the context expects, as in `let v: Simd<f64, 4> = Simd::splat(1.5)`.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "from_array",
        kind: "assoc",
        signature: "fn from_array(lanes: [T; N]) -> Simd<T, N>",
        doc: "A vector with one lane per array element, in order; the array's length is \
              the lane count.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "load",
        kind: "assoc",
        signature: "fn load(source: [T], offset: i64) -> Simd<T, N>",
        doc: "Reads N lanes from a `Vec`, slice, or fixed array starting at `offset`. The \
              whole window is checked once, and a window past either end panics before \
              any lane is read. The lane count comes from the type the context expects.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "store",
        kind: "method",
        signature: "fn store(self: Simd<T, N>, target: &mut [T], offset: i64) -> ()",
        doc: "Writes the lanes into a `Vec`, slice, or fixed array through `&mut`, starting \
              at `offset`. The whole window is checked once, before any lane is written.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "to_array",
        kind: "method",
        signature: "fn to_array(self: Simd<T, N>) -> [T; N]",
        doc: "The lanes as a fixed array, in lane order.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "min",
        kind: "method",
        signature: "fn min(self: Simd<T, N>, other: Simd<T, N>) -> Simd<T, N>",
        doc: "The smaller of each pair of lanes.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "max",
        kind: "method",
        signature: "fn max(self: Simd<T, N>, other: Simd<T, N>) -> Simd<T, N>",
        doc: "The larger of each pair of lanes.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "abs",
        kind: "method",
        signature: "fn abs(self: Simd<T, N>) -> Simd<T, N>",
        doc: "Each lane's absolute value.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "sqrt",
        kind: "method",
        signature: "fn sqrt(self: Simd<T, N>) -> Simd<T, N>",
        doc: "Each lane's square root. Float lanes only.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "lanes_eq",
        kind: "method",
        signature: "fn lanes_eq(self: Simd<T, N>, other: Simd<T, N>) -> Mask<N>",
        doc: "A mask whose lane is true where the two vectors' lanes are equal.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "lanes_lt",
        kind: "method",
        signature: "fn lanes_lt(self: Simd<T, N>, other: Simd<T, N>) -> Mask<N>",
        doc: "A mask whose lane is true where this vector's lane is less than the other's.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "lanes_le",
        kind: "method",
        signature: "fn lanes_le(self: Simd<T, N>, other: Simd<T, N>) -> Mask<N>",
        doc: "A mask whose lane is true where this vector's lane is at most the other's.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "reduce_sum",
        kind: "method",
        signature: "fn reduce_sum(self: Simd<T, N>) -> T",
        doc: "The sum of every lane, folded in one fixed pairing order, so every tier \
              answers the same bits.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "reduce_min",
        kind: "method",
        signature: "fn reduce_min(self: Simd<T, N>) -> T",
        doc: "The smallest lane.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "reduce_max",
        kind: "method",
        signature: "fn reduce_max(self: Simd<T, N>) -> T",
        doc: "The largest lane.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "reduce_and",
        kind: "method",
        signature: "fn reduce_and(self: Simd<T, N>) -> T",
        doc: "The bitwise AND of every lane. Integer lanes only.",
    },
    CoreMethodHelp {
        owner: "Simd",
        name: "reduce_or",
        kind: "method",
        signature: "fn reduce_or(self: Simd<T, N>) -> T",
        doc: "The bitwise OR of every lane. Integer lanes only.",
    },
    CoreMethodHelp {
        owner: "Mask",
        name: "select",
        kind: "method",
        signature: "fn select(self: Mask<N>, if_true: Simd<T, N>, if_false: Simd<T, N>) -> Simd<T, N>",
        doc: "A vector taking each lane from `if_true` where the mask's lane is true, and \
              from `if_false` where it is false.",
    },
    CoreMethodHelp {
        owner: "Mask",
        name: "reduce_and",
        kind: "method",
        signature: "fn reduce_and(self: Mask<N>) -> bool",
        doc: "True when every lane is true.",
    },
    CoreMethodHelp {
        owner: "Mask",
        name: "reduce_or",
        kind: "method",
        signature: "fn reduce_or(self: Mask<N>) -> bool",
        doc: "True when any lane is true.",
    },
    CoreMethodHelp {
        owner: "Mask",
        name: "to_array",
        kind: "method",
        signature: "fn to_array(self: Mask<N>) -> [bool; N]",
        doc: "The lanes as a fixed array of `bool`, in lane order.",
    },
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
        name: "push_json_quoted",
        kind: "method",
        signature: "fn push_json_quoted(self: &mut String, buf: Vec<u8>, start: i64, end: i64) -> bool",
        doc: "Appends the [start, end) byte window of buf as a quoted, escaped JSON string when it is valid UTF-8.",
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

    outln!(
        "gos {version} REPL [{arch}-{os}]\n\
         %help for commands · Enter continues until braces close · Ctrl-D or %q exits",
        version = env!("CARGO_PKG_VERSION"),
        arch = std::env::consts::ARCH,
        os = std::env::consts::OS,
    );
    let declares_bindings = crate::paths::project_context()
        .manifest_result()
        .is_some_and(|manifest| manifest.is_ok_and(|m| !m.rust_bindings.is_empty()));
    if declares_bindings {
        outln!(
            "note: this project's [rust-bindings] are not loaded in the REPL; \
             `gos run` and `gos build` load them"
        );
    }

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
                outln!();
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
                            outln!("{}", crate::style::repl_meta_accent(&entry));
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
                outln!("history cleared");
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
                        outln!(
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
                            outln!(
                                "{}",
                                crate::style::repl_meta_detail(&format!(
                                    "    no bindings match `{}`",
                                    options.pattern
                                ))
                            );
                            continue;
                        }
                        for entry in matches {
                            outln!("{}", crate::style::repl_meta_heading(&entry.line));
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
                                        outln!(
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
                            outln!(
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
                        outln!(
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
                            outln!(
                                "{}",
                                crate::style::repl_meta_detail(&format!(
                                    "    no declarations match `{}`",
                                    options.pattern
                                ))
                            );
                            continue;
                        }
                        for line in matches {
                            outln!("{}", crate::style::repl_meta_heading(line));
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
                    outln!("{}", crate::style::repl_meta_accent("session cleared"));
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
                        outln!("    added {} declarations", declarations.len());
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
                        outln!("    binding added ({} total)", bindings.len());
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
    /// the bytecode compiler builds for a `println` of the same value,
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
            Some(gossamer_types::TyKind::Simd { elem, .. }) => {
                let owner = if matches!(tcx.kind(*elem), Some(gossamer_types::TyKind::Bool)) {
                    "Mask"
                } else {
                    "Simd"
                };
                (Some(owner.to_string()), false)
            }
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
    // compiler builds for a `println` of the same value.
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
    outln!("{}", render_repl_binding_value(value, ty));
    std::io::stdout()
        .flush()
        .expect("flush REPL expression result");
}

mod bindings;
mod catalog;
mod evaluate;
mod mutation;
mod session;

use bindings::{
    ReplBinding, ReplBindingVar, binding_can_mutate, declaration_names, infer_repl_binding_type,
    input_is_declaration, prepare_repl_declaration_drop, prepare_repl_drop,
    render_dropped_binding_names, render_dropped_declaration_names, render_repl_bindings,
    render_repl_declarations, render_repl_setup, repl_binding_from_let_source, repl_binding_info,
    repl_binding_listing, repl_declaration_info, repl_info, repl_info_listing, split_meta_command,
    update_repl_bindings,
};
use catalog::{
    canonical_collection_owner, collect_repl_mut_self_method_names, compile_search_regex,
    core_method_entries, input_mutates_binding, parse_listing_options, render_info,
    render_repl_history, repl_info_matches, signature_example_arguments, signature_suffix,
    symbol_query_matches,
};
use evaluate::{
    build_and_call, build_and_call_with_type, build_and_call_with_type_for_inspection,
    collect_source_file_names, format_parse_diags, format_resolve_diags, format_type_diags,
    infer_repl_tail_type,
};
use mutation::{rebuild_session, repl_expr_mutates_binding};
pub(crate) use session::*;

#[cfg(test)]
mod tests {
    use super::bindings::repl_binding_listing_for;
    use super::catalog::{
        matching_core_methods, matching_core_namespaces, matching_items, push_catalog_entry,
        push_catalog_match, push_core_method_match, push_item_match, push_module_match,
        registered_core_method_path, render_catalog_query_matches, render_stdlib_dir,
    };
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
