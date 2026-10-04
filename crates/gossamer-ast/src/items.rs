//! Item-level declarations: `fn`, `struct`, `enum`, `trait`, `impl`, ...

#![forbid(unsafe_code)]

use gossamer_lex::Span;

use crate::common::{Ident, Mutability, Visibility};
use crate::expr::{Expr, PathExpr};
use crate::node_id::NodeId;
use crate::pattern::Pattern;
use crate::ty::{Type, TypePath};

/// Name prefix of the comparator the autoderive pass emits for a type whose
/// source writes its own `cmp`.
///
/// A sequence orders its elements structurally, and the ordering primitives
/// take a comparator rather than a method, so an order the source declares
/// reaches them only through a function of this shape.
pub const USER_COMPARATOR_PREFIX: &str = "__gos_cmp_";

/// Name prefix of the synthesized field-by-field comparator, for the ordering
/// primitives that cannot compare an aggregate element on their own.
pub const STRUCTURAL_COMPARATOR_PREFIX: &str = "__gos_ord_";

/// Name prefix of the synthesized sorted-sequence search over a type, which
/// runs the same binary search the primitive does but through the type's
/// comparator.
pub const BINARY_SEARCH_PREFIX: &str = "__gos_bsearch_";

/// Prefix of the inherent method an operator impl with a right-hand type of
/// its own becomes (`impl Mul<f64> for V2` -> `__gos_op_mul_f64`). The
/// operator's method name and the right-hand type follow it.
pub const OPERATOR_METHOD_PREFIX: &str = "__gos_op_";

/// Name prefix of the synthesized sorted-sequence insertion-point search.
pub const PARTITION_POINT_PREFIX: &str = "__gos_ppoint_";

/// An item-level declaration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Item {
    /// Unique id within the enclosing source file.
    pub id: NodeId,
    /// Source range covered by this item.
    pub span: Span,
    /// Attributes attached to the item (`#[...]` and `#![...]`).
    pub attrs: Attrs,
    /// Declared visibility.
    pub visibility: Visibility,
    /// The kind of item being declared.
    pub kind: ItemKind,
}

impl Item {
    /// Constructs a new item node with the given id, span, attrs, visibility, and kind.
    #[must_use]
    pub fn new(
        id: NodeId,
        span: Span,
        attrs: Attrs,
        visibility: Visibility,
        kind: ItemKind,
    ) -> Self {
        Self {
            id,
            span,
            attrs,
            visibility,
            kind,
        }
    }
}

impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        self.attrs == other.attrs && self.visibility == other.visibility && self.kind == other.kind
    }
}

/// Every item production in the grammar.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ItemKind {
    /// `fn name<G>(params) -> ret where ... { body }`.
    Fn(FnDecl),
    /// `struct Name`, `struct Name { ... }`, or `struct Name(T, U)`.
    Struct(StructDecl),
    /// `enum Name<G> { V1, V2(T), V3 { x: T } }`.
    Enum(EnumDecl),
    /// `trait Name<G>: Bounds { items }`.
    Trait(TraitDecl),
    /// `impl<G> Type { items }` or `impl<G> Trait for Type { items }`.
    Impl(ImplDecl),
    /// `type Name<G> = Type;`.
    TypeAlias(TypeAliasDecl),
    /// `const NAME: Type = Expr;`.
    Const(ConstDecl),
    /// `static [mut] NAME: Type = Expr;`.
    Static(StaticDecl),
    /// `mod name { items }` or `mod name;`.
    Mod(ModDecl),
    /// A free-standing attribute item `#![attr]` - uncommon outside
    /// crate-level headers but included for completeness.
    AttrItem(Attribute),
}

/// Attributes attached to a declaration.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Attrs {
    /// Outer attributes written as `#[...]` before the item.
    pub outer: Vec<Attribute>,
    /// Inner attributes written as `#![...]` inside the item.
    pub inner: Vec<Attribute>,
}

impl Attrs {
    /// Returns `true` when no attributes are attached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.outer.is_empty() && self.inner.is_empty()
    }

    /// Returns `true` when a bare `#[name]` or `#![name]` is present.
    #[must_use]
    pub fn has_word(&self, name: &str) -> bool {
        self.outer
            .iter()
            .chain(&self.inner)
            .any(|attr| attr.is_word(name))
    }

    /// Returns `true` when an attribute `name(..)` lists `argument` among its
    /// comma-separated arguments: `#[repr(C)]` lists `C` under `repr`.
    #[must_use]
    pub fn lists_argument(&self, name: &str, argument: &str) -> bool {
        self.outer
            .iter()
            .chain(&self.inner)
            .any(|attr| attr.lists_argument(name, argument))
    }

    /// The library a `#[link(name = "lib")]` attribute names.
    #[must_use]
    pub fn link_library(&self) -> Option<String> {
        self.link_argument("name")
    }

    /// The directory a `#[link(name = "...", search = "dir")]` attribute
    /// names for its library, as written: relative to the root of the
    /// package that declares it.
    #[must_use]
    pub fn link_search(&self) -> Option<String> {
        self.link_argument("search")
    }

    /// The value of `key` in the `#[link(key = "value", ..)]` attribute.
    fn link_argument(&self, key: &str) -> Option<String> {
        let tokens = self
            .outer
            .iter()
            .find(|attr| attr.is_named("link"))?
            .tokens
            .as_deref()?;
        tokens.split(',').find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name.trim() == key).then(|| value.trim().trim_matches('"').to_string())
        })
    }

    /// The symbol a `#[link_name = "symbol"]` attribute names.
    #[must_use]
    pub fn link_name(&self) -> Option<String> {
        let tokens = self
            .outer
            .iter()
            .find(|attr| attr.is_named("link_name"))?
            .tokens
            .as_deref()?;
        let value = tokens.trim().strip_prefix('=')?.trim().trim_matches('"');
        (!value.is_empty()).then(|| value.to_string())
    }

    /// Returns `true` when `#[allow(lint)]` or `#![allow(lint)]` names
    /// `lint` among its comma-separated arguments.
    #[must_use]
    pub fn allows(&self, lint: &str) -> bool {
        self.outer
            .iter()
            .chain(&self.inner)
            .any(|attr| attr.lists_argument("allow", lint))
    }
}

impl Attribute {
    /// Returns `true` when this attribute is the bare word `name` with no
    /// argument list.
    #[must_use]
    pub fn is_word(&self, name: &str) -> bool {
        self.tokens.is_none() && self.is_named(name)
    }

    /// Returns `true` when this attribute is `name(..)` and `argument`
    /// appears among the comma-separated arguments.
    #[must_use]
    pub fn lists_argument(&self, name: &str, argument: &str) -> bool {
        self.is_named(name)
            && self
                .tokens
                .as_deref()
                .is_some_and(|tokens| tokens.split(',').any(|tok| tok.trim() == argument))
    }

    /// Returns `true` when the attribute path is the single segment `name`.
    #[must_use]
    pub fn is_named(&self, name: &str) -> bool {
        self.path.segments.len() == 1 && self.path.segments[0].name.name == name
    }

    /// The contents of `name("...")` with the quotes removed, when this
    /// attribute is `name` carrying a single string literal argument.
    #[must_use]
    pub fn string_argument(&self, name: &str) -> Option<&str> {
        if !self.is_named(name) {
            return None;
        }
        let tokens = self.tokens.as_deref()?.trim();
        tokens
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
    }
}

/// A single `#[...]` or `#![...]` attribute.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Attribute {
    /// Path naming the attribute (e.g. `derive`, `allow`).
    pub path: PathExpr,
    /// Raw delimited token contents preserved verbatim, without the outer
    /// delimiters. `None` means the attribute had no argument list.
    pub tokens: Option<String>,
}

/// Generic parameter list `<A, B: Bound, const N: usize>`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Generics {
    /// Parameters in source order.
    pub params: Vec<GenericParam>,
}

impl Generics {
    /// Returns `true` when no generic parameters are declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }
}

/// A single generic parameter.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum GenericParam {
    /// Lifetime parameter `'a`. Parsed for FFI compatibility and otherwise
    /// ignored by the type checker (see SPEC §3.10).
    Lifetime {
        /// Name of the lifetime without the leading apostrophe.
        name: String,
    },
    /// Type parameter `T: Bound = Default`.
    Type {
        /// Parameter name.
        name: Ident,
        /// Trait bounds applied to this parameter.
        bounds: Vec<TraitBound>,
        /// Optional default type.
        default: Option<Type>,
    },
    /// Const parameter `const N: Type = default`.
    Const {
        /// Parameter name.
        name: Ident,
        /// Type of the constant.
        ty: Type,
        /// Optional default value.
        default: Option<Expr>,
    },
}

/// `where T: Bound, U: Bound + Bound, ...` clause.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WhereClause {
    /// Individual predicates.
    pub predicates: Vec<WherePredicate>,
}

impl WhereClause {
    /// Returns `true` when no predicates are declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.predicates.is_empty()
    }
}

/// A single `where` clause predicate.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WherePredicate {
    /// Type being constrained.
    pub bounded: Type,
    /// Bounds applied to that type.
    pub bounds: Vec<TraitBound>,
}

/// A single trait bound `Path<Args>` as used in generics, supertraits, or where clauses.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TraitBound {
    /// Path naming the trait.
    pub path: TypePath,
    /// Associated-type equality constraints written inside the bound's
    /// argument list (`Iterator<Item = i64>`). Ordinary type and const
    /// arguments stay in the path segment's `generics`.
    #[serde(default)]
    pub bindings: Vec<AssocBinding>,
}

impl TraitBound {
    /// Constructs a bound from its path with no associated-type bindings.
    #[must_use]
    pub const fn new(path: TypePath) -> Self {
        Self {
            path,
            bindings: Vec::new(),
        }
    }

    /// Source name of the trait this bound names, ignoring any module
    /// qualification.
    #[must_use]
    pub fn trait_name(&self) -> Option<&str> {
        self.path.segments.last().map(|s| s.name.name.as_str())
    }
}

/// One `Name = Type` associated-type constraint inside a trait bound.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AssocBinding {
    /// Associated type being constrained.
    pub name: Ident,
    /// Type it is constrained to.
    pub ty: Type,
}

/// A function declaration: signature plus optional body.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FnDecl {
    /// Attributes written on the declaration. A free function repeats its
    /// [`Item::attrs`] here, so a method - which has no enclosing `Item` -
    /// answers the question the same way.
    #[serde(default)]
    pub attrs: Attrs,
    /// Source range covered by the declaration, from the first attribute
    /// through the closing brace of the body.
    #[serde(default)]
    pub span: Span,
    /// `true` when the function is declared `unsafe`.
    pub is_unsafe: bool,
    /// `true` when the function is declared `comptime`. Every call to a
    /// comptime function is evaluated at compile time by the comptime
    /// fold pass and replaced with its result literal.
    #[serde(default)]
    pub is_comptime: bool,
    /// Declared visibility. For an `impl` item this is the only record
    /// of its `pub`; a free function repeats its [`Item::visibility`]
    /// here so both shapes answer the question the same way.
    #[serde(default)]
    pub visibility: Visibility,
    /// Function name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Generics,
    /// Parameter list (including an optional leading `self`).
    pub params: Vec<FnParam>,
    /// Optional return type; `None` is syntactically `()`.
    pub ret: Option<Type>,
    /// Optional `where` clause.
    pub where_clause: WhereClause,
    /// Function body. `None` means the signature is a trait-item declaration
    /// or a foreign function.
    pub body: Option<Box<Expr>>,
    /// The calling convention of a function declared in an
    /// `unsafe extern "C" { ... }` block (`"C"` or `"system"`), whose body is
    /// in a native library; `None` for every Gossamer function.
    #[serde(default)]
    pub extern_abi: Option<String>,
}

/// The attribute the parser puts on a `type Name` declared inside an
/// `unsafe extern` block: a C type with no Gossamer layout, reachable only
/// through `ffi::Ptr`.
pub const FOREIGN_TYPE_ATTR: &str = "__gos_foreign_type";

/// A single function parameter.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FnParam {
    /// `self` receiver (`self`, `&self`, `&mut self`).
    Receiver(Receiver),
    /// Regular parameter `pattern: type`.
    Typed {
        /// Binding pattern.
        pattern: Pattern,
        /// Parameter type.
        ty: Type,
        /// `true` when declared `comptime pattern: type`. The matching
        /// argument at each call site is evaluated at compile time and
        /// replaced with its result literal by the comptime fold.
        #[serde(default)]
        is_comptime: bool,
        /// Constant default written `pattern: type = expr`. A call that
        /// omits this parameter has the expression spliced in at its
        /// position, so every tier compiles the same positional call.
        #[serde(default)]
        default: Option<Box<Expr>>,
    },
}

/// Kind of `self` receiver on a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Receiver {
    /// `self`.
    Owned,
    /// `&self`.
    RefShared,
    /// `&mut self`.
    RefMut,
}

impl Receiver {
    /// Returns the canonical source spelling of this receiver form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owned => "self",
            Self::RefShared => "&self",
            Self::RefMut => "&mut self",
        }
    }
}

/// A struct declaration.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StructDecl {
    /// Struct name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Generics,
    /// Optional `where` clause.
    pub where_clause: WhereClause,
    /// Shape of the struct's body.
    pub body: StructBody,
}

/// Body shape of a struct declaration.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StructBody {
    /// Named fields `{ a: T, b: U }`.
    Named(Vec<StructField>),
    /// Tuple fields `(T, U)`.
    Tuple(Vec<TupleField>),
    /// Unit struct or enum variant.
    Unit,
}

/// A named field declaration in a struct or enum variant.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StructField {
    /// Field attributes.
    pub attrs: Attrs,
    /// Field visibility.
    pub visibility: Visibility,
    /// Field name.
    pub name: Ident,
    /// Field type.
    pub ty: Type,
}

/// A positional field declaration in a tuple struct or tuple variant.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TupleField {
    /// Field attributes.
    pub attrs: Attrs,
    /// Field visibility.
    pub visibility: Visibility,
    /// Field type.
    pub ty: Type,
}

/// How an enum's discriminant is stored.
///
/// A plain `enum` takes the smallest byte-aligned integer that holds every
/// variant. `packed` asks for the smallest number of *bits* instead, which
/// is what makes a sequence of them worth packing. Either form may name its
/// own width with `: uN`, and a width too narrow for the variants is
/// rejected at the declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct EnumRepr {
    /// Whether the declaration wrote `packed`.
    pub packed: bool,
    /// The width `: uN` named, in bits, or `None` when the compiler picks.
    pub declared_bits: Option<u32>,
}

impl EnumRepr {
    /// The bits `variants` need, before any declared width is applied.
    ///
    /// A byte-aligned representation rounds up to a whole byte and never
    /// answers less than one; a packed one answers exactly the bits the
    /// discriminants occupy.
    #[must_use]
    pub const fn natural_bits(packed: bool, variants: usize) -> u32 {
        let mut bits = 1u32;
        while bits < 64 && (1usize << bits) < variants {
            bits += 1;
        }
        if packed {
            return bits;
        }
        // Round up to a whole byte: 1..=8 -> 8, 9..=16 -> 16, and so on.
        let bytes = bits.div_ceil(8);
        bytes * 8
    }

    /// The width this declaration stores its discriminant in.
    #[must_use]
    pub const fn bits(self, variants: usize) -> u32 {
        match self.declared_bits {
            Some(bits) => bits,
            None => Self::natural_bits(self.packed, variants),
        }
    }

    /// Whether `bits` can represent `variants` distinct discriminants.
    #[must_use]
    pub const fn fits(bits: u32, variants: usize) -> bool {
        if bits >= 64 {
            return true;
        }
        (variants as u128) <= (1u128 << bits)
    }
}

/// An enum declaration.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnumDecl {
    /// Enum name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Generics,
    /// Optional `where` clause.
    pub where_clause: WhereClause,
    /// Variants in source order.
    pub variants: Vec<EnumVariant>,
    /// How the discriminant is stored.
    pub repr: EnumRepr,
}

/// A single enum variant.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnumVariant {
    /// Attributes on the variant.
    pub attrs: Attrs,
    /// Variant name.
    pub name: Ident,
    /// Variant payload shape.
    pub body: StructBody,
    /// Optional explicit discriminant `= expr`.
    pub discriminant: Option<Expr>,
}

/// A trait declaration.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TraitDecl {
    /// Trait name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Generics,
    /// Supertrait bounds after `:`.
    pub supertraits: Vec<TraitBound>,
    /// Optional `where` clause.
    pub where_clause: WhereClause,
    /// Trait items (methods, associated types, associated constants).
    pub items: Vec<TraitItem>,
}

/// One item inside a `trait` body.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TraitItem {
    /// Method signature with optional default body.
    Fn(FnDecl),
    /// Associated type `type Name: Bounds = Default;`.
    Type {
        /// Attributes on the associated type.
        attrs: Attrs,
        /// Name of the associated type.
        name: Ident,
        /// Trait bounds applied to the associated type.
        bounds: Vec<TraitBound>,
        /// Optional default type.
        default: Option<Type>,
    },
    /// Associated constant `const NAME: Ty = Expr;`.
    Const {
        /// Attributes on the associated constant.
        attrs: Attrs,
        /// Constant name.
        name: Ident,
        /// Constant type.
        ty: Type,
        /// Optional default value.
        default: Option<Expr>,
    },
}

/// An `impl` block.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImplDecl {
    /// Generic parameters on the impl.
    pub generics: Generics,
    /// Trait being implemented, if this is a trait impl.
    pub trait_ref: Option<TraitBound>,
    /// Self type the impl attaches to.
    pub self_ty: Type,
    /// Optional `where` clause.
    pub where_clause: WhereClause,
    /// Impl items in source order.
    pub items: Vec<ImplItem>,
}

/// One item inside an `impl` body.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ImplItem {
    /// Method or associated function.
    Fn(FnDecl),
    /// Associated type definition `type Name = Type;`.
    Type {
        /// Attributes on the associated type.
        attrs: Attrs,
        /// Name of the associated type.
        name: Ident,
        /// Concrete type.
        ty: Type,
    },
    /// Associated constant `const NAME: Ty = Expr;`.
    Const {
        /// Attributes on the associated constant.
        attrs: Attrs,
        /// Constant name.
        name: Ident,
        /// Constant type.
        ty: Type,
        /// Constant value.
        value: Expr,
    },
}

/// Type alias declaration `type Name<G> = Type;`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TypeAliasDecl {
    /// Alias name.
    pub name: Ident,
    /// Generic parameters.
    pub generics: Generics,
    /// Right-hand type.
    pub ty: Type,
    /// `true` for the opaque form `type Name = new Type`, which declares a
    /// distinct nominal type over the same representation instead of a
    /// transparent spelling of it.
    #[serde(default)]
    pub nominal: bool,
}

/// `const` item declaration.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConstDecl {
    /// Constant name.
    pub name: Ident,
    /// Constant type.
    pub ty: Type,
    /// Constant value expression.
    pub value: Expr,
}

/// `static` item declaration.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StaticDecl {
    /// Mutability of the static.
    pub mutability: Mutability,
    /// Static name.
    pub name: Ident,
    /// Static type.
    pub ty: Type,
    /// Static value expression.
    pub value: Expr,
}

/// `mod` item declaration - inline or external.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModDecl {
    /// Module name.
    pub name: Ident,
    /// Module body.
    pub body: ModBody,
}

/// Body of a module declaration.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ModBody {
    /// Inline module: `mod name { items }`.
    Inline(Vec<Item>),
    /// External module reference: `mod name;`.
    External,
}
