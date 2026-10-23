//! The values a binding exchanges with Gossamer code through a callback or a
//! hand-written conversion, independent of how any execution tier lays its
//! own values out.

use std::fmt;
use std::sync::Arc;

/// Immutable shared text.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Str(Arc<str>);

impl Str {
    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for Str {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Str {
    fn from(text: &str) -> Self {
        Self(Arc::from(text))
    }
}

impl From<String> for Str {
    fn from(text: String) -> Self {
        Self(Arc::from(text))
    }
}

impl fmt::Display for Str {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Str {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

/// An enum variant: its name and positional payload.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantValue {
    /// The variant's name, as the Gossamer enum spells it.
    pub name: String,
    /// The payload, in declaration order.
    pub fields: Vec<Value>,
}

/// A struct: its name and its fields by name.
#[derive(Debug, Clone, PartialEq)]
pub struct StructValue {
    /// The struct's name.
    pub name: String,
    /// The fields, in declaration order.
    pub fields: Vec<(String, Value)>,
}

/// Which side registered a callback handle, and so which side calls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallbackOrigin {
    /// A callable of the bytecode VM, called through the running VM call.
    Vm,
    /// A compiled closure, called through the runtime's callback registry.
    Compiled,
}

/// A handle to a Gossamer callable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallbackRef {
    /// The handle the registering side issued.
    pub handle: u64,
    /// The side that issued it.
    pub origin: CallbackOrigin,
}

/// A Gossamer value as a binding sees it.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `()`.
    Unit,
    /// `bool`.
    Bool(bool),
    /// Any integer, widened to 64 bits.
    Int(i64),
    /// `f64`.
    Float(f64),
    /// `char`.
    Char(char),
    /// `String`.
    String(Str),
    /// A byte buffer.
    Bytes(Vec<u8>),
    /// A sequence.
    Array(Arc<Vec<Value>>),
    /// A tuple.
    Tuple(Arc<Vec<Value>>),
    /// A map, as its entries.
    Map(Arc<Vec<(Value, Value)>>),
    /// An enum variant, `Option` and `Result` included.
    Variant(Arc<VariantValue>),
    /// A struct.
    Struct(Arc<StructValue>),
    /// A callable.
    Callback(CallbackRef),
}

impl Value {
    /// The variant `name` with `fields` as its payload.
    #[must_use]
    pub fn variant(name: impl Into<String>, fields: Vec<Value>) -> Self {
        Self::Variant(Arc::new(VariantValue {
            name: name.into(),
            fields,
        }))
    }

    /// The struct `name` with `fields`.
    #[must_use]
    pub fn struct_(name: impl Into<String>, fields: Vec<(String, Value)>) -> Self {
        Self::Struct(Arc::new(StructValue {
            name: name.into(),
            fields,
        }))
    }

    /// The unit value.
    #[must_use]
    pub const fn unit() -> Self {
        Self::Unit
    }

    /// A short name for this value's kind, for error messages.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unit => "()",
            Self::Bool(_) => "bool",
            Self::Int(_) => "i64",
            Self::Float(_) => "f64",
            Self::Char(_) => "char",
            Self::String(_) => "String",
            Self::Bytes(_) => "bytes",
            Self::Array(_) => "vec",
            Self::Tuple(_) => "tuple",
            Self::Map(_) => "map",
            Self::Variant(_) => "enum variant",
            Self::Struct(_) => "struct",
            Self::Callback(_) => "callable",
        }
    }
}

/// Why a conversion or a call across the boundary failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// A value was not of the type the binding declared.
    Type(String),
    /// A call passed the wrong number of arguments.
    Arity {
        /// The number the item declares.
        expected: usize,
        /// The number the call passed.
        found: usize,
    },
    /// The running tier cannot do what was asked.
    Unsupported(&'static str),
    /// Gossamer code the binding called panicked.
    Panic(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Type(message) | Self::Panic(message) => f.write_str(message),
            Self::Arity { expected, found } => {
                write!(f, "expected {expected} arguments, found {found}")
            }
            Self::Unsupported(what) => write!(f, "unsupported here: {what}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// The result of a conversion or a call across the boundary.
pub type RuntimeResult<T> = Result<T, RuntimeError>;

/// What a `cb_fn` binding calls Gossamer code back through.
pub trait NativeDispatch {
    /// Calls the callable `callback` names with `args`.
    fn invoke_callback(&mut self, callback: CallbackRef, args: Vec<Value>) -> RuntimeResult<Value>;
}
