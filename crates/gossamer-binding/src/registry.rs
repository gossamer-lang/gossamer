//! The link-time registry every `register_module!` call adds a [`Module`]
//! to.

use gossamer_binding_abi::VmThunk;
use linkme::distributed_slice;

use crate::types::Type;

/// One binding item.
#[derive(Debug, Clone, Copy)]
pub struct ItemFn {
    /// The item's name within its module.
    pub name: &'static str,
    /// The entry the bytecode VM calls.
    pub vm: VmThunk,
    /// The wire thunk's symbol, empty for a module declared without a
    /// `symbol_prefix:`, which compiled programs cannot call.
    pub wire_symbol: &'static str,
    /// The wire thunk's address.
    pub wire: Option<fn() -> *const u8>,
    /// Whether the item is a `cb_fn`, which calls Gossamer callables back.
    pub calls_back: bool,
    /// The conversions between each position's value and the value its VM
    /// entry takes, for an item with a wire thunk.
    pub wire_text: Option<WireText>,
    /// The item's signature.
    pub signature: Signature,
    /// Its documentation.
    pub doc: &'static str,
}

/// A position's value as its VM entry takes it, or the reverse: the text
/// of a type that crosses as text, any other value itself.
pub type TextConversion =
    fn(&crate::value::Value) -> crate::value::RuntimeResult<crate::value::Value>;

/// An item's [`TextConversion`]s: one per parameter, value to entry, and the
/// return's, entry to value.
#[derive(Debug, Clone, Copy)]
pub struct WireText {
    /// Each parameter's value to the value its entry takes.
    pub args: &'static [TextConversion],
    /// The value the entry answers to the return's value.
    pub ret: TextConversion,
}

/// An item's parameter and return types.
#[derive(Debug, Clone, Copy)]
pub struct Signature {
    /// Parameter types, in order.
    pub params: &'static [Type],
    /// Return type.
    pub ret: Type,
    /// The positions that cross as JSON text: bit 0 the return, bit `i + 1`
    /// parameter `i`.
    pub json: u64,
}

/// One module of bindings.
#[derive(Debug, Clone, Copy)]
pub struct Module {
    /// The module's path as Gossamer code names it.
    pub path: &'static str,
    /// Its documentation.
    pub doc: &'static str,
    /// Its items.
    pub items: &'static [ItemFn],
}

/// Every module the binding crates in this link registered.
#[distributed_slice]
pub static REGISTRY: [&'static Module] = [..];
