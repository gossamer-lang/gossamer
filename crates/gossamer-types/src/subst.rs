//! Generic substitutions applied to a polymorphic type or trait ref.

#![forbid(unsafe_code)]

use crate::ty::Ty;

/// One argument in a [`Substs`] list.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum GenericArg {
    /// A type argument - the `T` in `Vec<T>`.
    Type(Ty),
    /// A const argument - the `N` in `Array<T, N>`. Stored as a
    /// pre-evaluated i128 so that trait solving can treat equal values
    /// as equal without re-evaluating expressions.
    Const(i128),
    /// A const argument that is the enclosing item's own const generic
    /// parameter at this position - the `N` in `impl<const N: usize> Ring<N>`.
    /// A use site substitutes the value the instantiation carries.
    ConstParam(crate::ParamIdx),
}

/// Ordered list of generic arguments attached to an item reference.
///
/// The first entries are type arguments in declaration order; const
/// arguments follow. The empty substitution is written `Substs::EMPTY`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Substs {
    args: Vec<GenericArg>,
}

impl Substs {
    /// The empty substitution - no generic arguments applied.
    #[must_use]
    pub const fn new() -> Self {
        Self { args: Vec::new() }
    }

    /// Returns a substitution wrapping the given ordered argument list.
    #[must_use]
    pub fn from_args(args: Vec<GenericArg>) -> Self {
        Self { args }
    }

    /// Returns a substitution consisting solely of type arguments.
    #[must_use]
    pub fn from_types<I>(types: I) -> Self
    where
        I: IntoIterator<Item = Ty>,
    {
        Self {
            args: types.into_iter().map(GenericArg::Type).collect(),
        }
    }

    /// Returns the number of generic arguments.
    #[must_use]
    pub fn len(&self) -> usize {
        self.args.len()
    }

    /// Returns `true` when the substitution is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.args.is_empty()
    }

    /// Borrows the underlying argument slice.
    #[must_use]
    pub fn as_slice(&self) -> &[GenericArg] {
        &self.args
    }

    /// Returns the type-argument portion of this substitution.
    #[must_use]
    pub fn types(&self) -> Vec<Ty> {
        self.args
            .iter()
            .filter_map(|arg| match arg {
                GenericArg::Type(ty) => Some(*ty),
                GenericArg::Const(_) | GenericArg::ConstParam(_) => None,
            })
            .collect()
    }
}
