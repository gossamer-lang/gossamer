//! Stable identifiers for named definitions.
//! Every top-level item, impl item, trait item, and generic parameter that
//! can be referenced by name receives a [`DefId`]. Ids are local to a
//! single resolver run; cross-crate identifiers are layered on top by
//! pairing a [`DefId`] with its [`CrateId`].

#![forbid(unsafe_code)]

/// Opaque identifier for a single crate. The current compiler only ever
/// resolves a single crate in one pass, so every [`CrateId`] carried
/// around inside a resolution is the [`CrateId::LOCAL`] constant.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct CrateId(u32);

impl CrateId {
    /// Crate id assigned to the source file currently being resolved.
    pub const LOCAL: Self = Self(0);

    /// Returns the raw numeric index of this identifier.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// Constructs a [`CrateId`] from a raw numeric index.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
}

/// Identifier for a module within a crate. The crate root is `ModId::ROOT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ModId(u32);

impl ModId {
    /// Module id of the crate root (the source file being resolved).
    pub const ROOT: Self = Self(0);

    /// Returns the raw numeric index of this identifier.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Stable identifier for a named definition, unique within a crate.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct DefId {
    /// Crate the definition belongs to.
    pub krate: CrateId,
    /// Monotonic index within that crate.
    pub local: u32,
}

impl DefId {
    /// Constructs a `DefId` for the local crate at the given local index.
    #[must_use]
    pub const fn local(index: u32) -> Self {
        Self {
            krate: CrateId::LOCAL,
            local: index,
        }
    }
}

/// Hands out fresh [`DefId`] values in monotonic order for a single crate.
#[derive(Debug, Clone)]
pub struct DefIdGenerator {
    krate: CrateId,
    next: u32,
}

impl DefIdGenerator {
    /// Returns a generator rooted at the local crate.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            krate: CrateId::LOCAL,
            next: 0,
        }
    }

    /// Produces the next fresh [`DefId`] and advances the counter.
    #[allow(
        clippy::should_implement_trait,
        reason = "infallible generator; Iterator::next would force Option<DefId>"
    )]
    pub fn next(&mut self) -> DefId {
        let id = DefId {
            krate: self.krate,
            local: self.next,
        };
        self.next = self.next.saturating_add(1);
        id
    }

    /// Returns the number of ids issued so far.
    #[must_use]
    pub const fn issued(&self) -> u32 {
        self.next
    }
}

impl Default for DefIdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Kind tag attached to each [`DefId`] to distinguish items by shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DefKind {
    /// A free function or associated function (`fn ...`).
    Fn,
    /// A `struct` type.
    Struct,
    /// An `enum` type.
    Enum,
    /// A `trait` declaration.
    Trait,
    /// A `type` alias.
    TypeAlias,
    /// A `const` item.
    Const,
    /// A `static` item.
    Static,
    /// A nested `mod` declaration.
    Mod,
    /// A type parameter introduced by a generic parameter list.
    TypeParam,
    /// An enum variant.
    Variant,
}

impl DefKind {
    /// Returns a short human-readable label used in diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fn => "function",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::TypeAlias => "type alias",
            Self::Const => "constant",
            Self::Static => "static",
            Self::Mod => "module",
            Self::TypeParam => "type parameter",
            Self::Variant => "enum variant",
        }
    }

    /// Returns `true` when the definition introduces a name in the type
    /// namespace.
    #[must_use]
    pub const fn is_type_ns(self) -> bool {
        matches!(
            self,
            Self::Struct | Self::Enum | Self::Trait | Self::TypeAlias | Self::Mod | Self::TypeParam
        )
    }

    /// Returns `true` when the definition introduces a name in the value
    /// namespace.
    #[must_use]
    pub const fn is_value_ns(self) -> bool {
        matches!(
            self,
            Self::Fn | Self::Const | Self::Static | Self::Variant | Self::Struct
        )
    }
}
