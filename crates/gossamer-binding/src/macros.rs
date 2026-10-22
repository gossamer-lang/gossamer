//! Declarative macros that turn idiomatic Rust function items
//! into [`crate::ItemFn`] entries inside a [`crate::Module`]
//! registered in [`crate::REGISTRY`].
//!
//! Two binding shapes are supported:
//!
//! - `fn name(arg: T, ...) -> R { body }` - plain binding fn.
//! - `cb_fn name(dispatch, arg: T, ...) -> R { body }` - same as
//!   plain, but the body has access to `dispatch: &mut dyn
//!   NativeDispatch` so it can call Gossamer callables back through
//!   [`crate::NativeDispatch::invoke_callback`] (`Terminal::draw` and
//!   any other higher-order binding APIs).

/// Internal: counts the supplied identifiers at compile time, in one step
/// whatever their number.
#[doc(hidden)]
#[macro_export]
macro_rules! __binding_count {
    ($($x:ident)*) => { <[()]>::len(&[$($crate::__binding_unit!($x)),*]) };
}

/// Internal: `()` for any identifier, so [`__binding_count!`] has one element
/// per identifier.
#[doc(hidden)]
#[macro_export]
macro_rules! __binding_unit {
    ($x:ident) => {
        ()
    };
}

/// Declares one or more Gossamer-callable functions and registers
/// them as a single [`crate::Module`].
///
/// Three top-level forms are accepted; pick whichever is least
/// boilerplate for the call site.
///
/// **New ergonomic form (single-segment path):**
///
/// ```text
/// register_module!(
///     name: echo,                            // both the Gossamer-side spelling
///                                            // and the C-ABI symbol prefix.
///     doc: "String helpers exposed by echo.",
///
///     /// Uppercase the input.                // `///` docs flow through to
///     fn shout(s: String) -> String { ... }   // `gos doc echo::shout`.
/// );
/// ```
///
/// **Legacy form (explicit `symbol_prefix:`):** keeps working for
/// nested paths like `"tuigoose::layout"` where mangling has to
/// be supplied by hand.
///
/// ```text
/// register_module!(
///     binding,                               // internal mod ident
///     path: "tuigoose::layout",
///     symbol_prefix: tuigoose__layout,
///     doc: "...",
///     fn rect(...) -> ... { ... }
/// );
/// ```
///
/// **VM-only form (no `symbol_prefix:`):** legacy compat; modules declared
/// this way have no wire thunks, so native builds cannot call them.
#[macro_export]
macro_rules! register_module {
    // New ergonomic form - single-segment path: `name: <ident>`
    // doubles as both the Gossamer-side spelling and the symbol
    // prefix. An auto-generated internal mod (`__gos_<name>`)
    // wraps the items.
    (
        name: $name:ident,
        doc: $doc:literal,
        $($body:tt)*
    ) => {
        $crate::__paste::paste! {
            $crate::__rm_module! {
                [< __gos_ $name >], stringify!($name), $name, $doc,
                $($body)*
            }
        }
    };

    (
        $modname:ident,
        path: $path:literal,
        symbol_prefix: $sym:ident,
        doc: $doc:literal,
        $($body:tt)*
    ) => {
        $crate::__rm_module! {
            $modname, $path, $sym, $doc,
            $($body)*
        }
    };

    // Backwards-compatible form without `symbol_prefix:` - only the VM
    // thunks are emitted, so binding fns from these modules are reachable
    // from `gos run` but not from a native build. New bindings should
    // specify `symbol_prefix:` explicitly.
    (
        $modname:ident,
        path: $path:literal,
        doc: $doc:literal,
        $($body:tt)*
    ) => {
        $crate::__rm_module! {
            $modname, $path, __nosym, $doc,
            $($body)*
        }
    };
}

/// Internal: one module's items, matched in one step whatever their number:
/// each is `fn name(args) -> R { .. }` or `cb_fn name(dispatch, args) -> R
/// { .. }`, with optional `///` docs and an optional return type.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_module {
    (
        $modname:tt, $path:expr, $sym:tt, $doc:literal,
        $(
            $(#[doc = $fdoc:literal])*
            $kind:ident $name:ident ( $($args:tt)* ) $(-> $ret:ty)? $body:block
        )*
    ) => {
        #[allow(non_snake_case, dead_code, clippy::missing_docs_in_private_items, reason = "the module is named after the user's binding and holds generated thunks nothing else documents")]
        mod $modname {
            use super::*;

            $(
                $crate::__rm_item! { $kind, $sym, $name, ( $($args)* ), ( $($ret)? ), $body }
            )*

            pub static ITEMS: &[$crate::ItemFn] = &[
                $(
                    $crate::__rm_entry! { $kind, $sym, $name, ( $($args)* ), ( $($ret)? ), [ $($fdoc)* ] },
                )*
            ];

            pub static MODULE: $crate::Module = $crate::Module {
                path: $path,
                doc: $doc,
                items: ITEMS,
            };

            #[$crate::linkme::distributed_slice($crate::REGISTRY)]
            #[linkme(crate = $crate::linkme)]
            #[allow(non_upper_case_globals, reason = "the static is named after the user's binding, whose case the macro cannot change")]
            static REGISTERED: &'static $crate::Module = &MODULE;

            /// Holds a reference to `MODULE` so the linker keeps its
            /// registry entry through LTO.
            pub fn force_link() {
                let _: &'static $crate::Module = &MODULE;
            }
        }
    };
}

/// Internal: an item's return type, `()` when it states none.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_ret {
    () => {
        ()
    };
    ($ret:ty) => {
        $ret
    };
}

/// Internal: one item's function, its VM thunk, and its wire thunk.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_item {
    (fn, $sym:tt, $name:ident, ( $($arg:ident : $argty:ty),* $(,)? ), ( $($ret:ty)? ), $body:block) => {
        pub fn $name($($arg : $argty),*) -> $crate::__rm_ret!($($ret)?) $body

        $crate::__paste::paste! {
            #[allow(non_snake_case, reason = "the thunk is named after the user's function, whose case the macro cannot change")]
            unsafe extern "C" fn [< __gos_vm_ $name >](
                ctx: *mut $crate::__abi::CallCtx,
                args: *const $crate::__abi::AbiValue,
                arg_count: usize,
                out: *mut $crate::__abi::CallOut,
            ) -> u32 {
                // SAFETY: the host passes `arg_count` live arguments, a
                // writable answer, and this call's context.
                unsafe {
                    $crate::__vm_call(ctx, args, arg_count, out, |_dispatch, args| {
                        let expected = $crate::__binding_count!($($arg)*);
                        if args.len() != expected {
                            return Err($crate::RuntimeError::Arity {
                                expected,
                                found: args.len(),
                            });
                        }
                        let mut iter = args.iter();
                        $(
                            let $arg: $argty = $crate::__rm_vm_arg!(
                                $sym,
                                $argty,
                                iter.next().ok_or($crate::RuntimeError::Arity {
                                    expected,
                                    found: args.len(),
                                })?
                            )?;
                        )*
                        let out: $crate::__rm_ret!($($ret)?) = $name($($arg),*);
                        Ok($crate::__rm_vm_ret!($sym, $crate::__rm_ret!($($ret)?), out))
                    })
                }
            }
        }

        $crate::__rm_emit_native_export! {
            $sym, $name, ( $($arg : $argty),* ), $crate::__rm_ret!($($ret)?)
        }
    };
    (cb_fn, $sym:tt, $name:ident, ( $disp:ident, $($arg:ident : $argty:ty),* $(,)? ), ( $($ret:ty)? ), $body:block) => {
        pub fn $name(
            $disp: &mut dyn $crate::NativeDispatch,
            $($arg : $argty),*
        ) -> $crate::__rm_ret!($($ret)?) $body

        $crate::__paste::paste! {
            #[allow(non_snake_case, reason = "the thunk is named after the user's function, whose case the macro cannot change")]
            unsafe extern "C" fn [< __gos_vm_ $name >](
                ctx: *mut $crate::__abi::CallCtx,
                args: *const $crate::__abi::AbiValue,
                arg_count: usize,
                out: *mut $crate::__abi::CallOut,
            ) -> u32 {
                // SAFETY: the host passes `arg_count` live arguments, a
                // writable answer, and this call's context.
                unsafe {
                    $crate::__vm_call(ctx, args, arg_count, out, |dispatch, args| {
                        let expected = $crate::__binding_count!($($arg)*);
                        if args.len() != expected {
                            return Err($crate::RuntimeError::Arity {
                                expected,
                                found: args.len(),
                            });
                        }
                        let mut iter = args.iter();
                        $(
                            let $arg: $argty = $crate::__rm_vm_arg!(
                                $sym,
                                $argty,
                                iter.next().ok_or($crate::RuntimeError::Arity {
                                    expected,
                                    found: args.len(),
                                })?
                            )?;
                        )*
                        let out: $crate::__rm_ret!($($ret)?) = $name(dispatch, $($arg),*);
                        Ok($crate::__rm_vm_ret!($sym, $crate::__rm_ret!($($ret)?), out))
                    })
                }
            }
        }

        $crate::__rm_emit_native_export_cb! {
            $sym, $name, ( $($arg : $argty),* ), $crate::__rm_ret!($($ret)?)
        }
    };
}

/// Internal: one item's [`crate::ItemFn`] entry.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_entry {
    (fn, $sym:tt, $name:ident, ( $($arg:ident : $argty:ty),* $(,)? ), ( $($ret:ty)? ), [ $($doc:literal)* ]) => {
        $crate::__rm_entry! { @entry false, $sym, $name, ( $($argty),* ), ( $($ret)? ), [ $($doc)* ] }
    };
    (cb_fn, $sym:tt, $name:ident, ( $disp:ident, $($arg:ident : $argty:ty),* $(,)? ), ( $($ret:ty)? ), [ $($doc:literal)* ]) => {
        $crate::__rm_entry! { @entry true, $sym, $name, ( $($argty),* ), ( $($ret)? ), [ $($doc)* ] }
    };
    (@entry $calls_back:literal, $sym:tt, $name:ident, ( $($argty:ty),* ), ( $($ret:ty)? ), [ $($doc:literal)* ]) => {
        $crate::__paste::paste! {
            $crate::ItemFn {
                name: stringify!($name),
                vm: [< __gos_vm_ $name >],
                wire_symbol: $crate::__rm_wire_symbol!($sym, $name),
                wire: $crate::__rm_wire_addr!($sym, $name),
                calls_back: $calls_back,
                wire_text: $crate::__rm_wire_text!(
                    $sym, ( $($argty),* ), $crate::__rm_ret!($($ret)?)
                ),
                signature: $crate::Signature {
                    params: &[ $( <$argty as $crate::SigType>::TYPE ),* ],
                    ret: <$crate::__rm_ret!($($ret)?) as $crate::SigType>::TYPE,
                    json: $crate::__rm_json_mask!(
                        $sym, ( $($argty),* ), $crate::__rm_ret!($($ret)?)
                    ),
                },
                doc: concat!( "" $(, $doc, "\n")* ),
            }
        }
    };
}

/// Internal: a VM call's argument. An item with a wire thunk takes a
/// JSON-wired type as the text the compiled tiers pass.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_vm_arg {
    (__nosym, $ty:ty, $value:expr) => {
        <$ty as $crate::FromGos>::from_gos($value)
    };
    ($sym:ident, $ty:ty, $value:expr) => {
        $crate::__vm_arg::<$ty>($value)
    };
}

/// Internal: a VM call's answer, as [`__rm_vm_arg`] takes its arguments.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_vm_ret {
    (__nosym, $ty:ty, $value:expr) => {
        <$ty as $crate::ToGos>::to_gos($value)
    };
    ($sym:ident, $ty:ty, $value:expr) => {
        $crate::__vm_ret::<$ty>($value)
    };
}

/// Internal: an item's [`crate::WireText`], `None` for the `__nosym` form.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_wire_text {
    (__nosym, ( $($argty:ty),* ), $ret:ty) => {
        None
    };
    ($sym:ident, ( $($argty:ty),* ), $ret:ty) => {
        Some($crate::WireText {
            args: &[ $( $crate::__value_to_entry::<$argty> ),* ],
            ret: $crate::__entry_to_value::<$ret>,
        })
    };
}

/// Internal: an item's [`crate::Signature::json`] mask, zero for the
/// `__nosym` form.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_json_mask {
    (__nosym, ( $($argty:ty),* ), $ret:ty) => {
        0
    };
    ($sym:ident, ( $($argty:ty),* ), $ret:ty) => {
        $crate::__json_mask(
            &[ $( <$argty as $crate::native::BindingAbi>::JSON ),* ],
            <$ret as $crate::native::BindingAbi>::JSON,
        )
    };
}

/// Internal: the wire symbol of an item, empty for the `__nosym` form.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_wire_symbol {
    (__nosym, $name:ident) => {
        ""
    };
    ($sym:ident, $name:ident) => {
        concat!("gos_binding_", stringify!($sym), "__", stringify!($name))
    };
}

/// Internal: the wire thunk's address getter, `None` for the `__nosym` form.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_wire_addr {
    (__nosym, $name:ident) => {
        None
    };
    ($sym:ident, $name:ident) => {
        $crate::__paste::paste! { Some([< __addr_ $sym __ $name >]) }
    };
}

/// Internal: emits the `extern "C"` wire thunk compiled code calls for one
/// plain binding fn. Skipped when `$sym` is the `__nosym` sentinel (the
/// legacy `register_module!` form without `symbol_prefix:`).
///
/// A panic in the body is recorded through the host for the compiled caller
/// to raise once the thunk returns, and the thunk answers a default value
/// nothing reads.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_emit_native_export {
    (__nosym, $name:ident, ( $($arg:ident : $argty:ty),* ), $ret:ty) => {};
    ($sym:ident, $name:ident, ( $($arg:ident : $argty:ty),* ), $ret:ty) => {
        $crate::__paste::paste! {
            #[unsafe(no_mangle)]
            #[allow(non_snake_case, unused_variables, unused_unsafe, reason = "the export is named after the user's function, and a binding with no arguments uses none of them")]
            pub extern "C" fn [< gos_binding_ $sym __ $name >](
                $( $arg : <$argty as $crate::native::BindingAbi>::Input ),*
            ) -> <$ret as $crate::native::BindingAbi>::Output {
                let _call = $crate::native::enter_wire_call();
                let result = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    $(
                        // SAFETY: the codegen guarantees `$arg`
                        // is the C-ABI `Input` shape declared
                        // by `BindingAbi` for `$argty`.
                        let $arg: $argty =
                            unsafe { <$argty as $crate::native::BindingAbi>::from_input($arg) };
                    )*
                    let out: $ret = $name($($arg),*);
                    <$ret as $crate::native::BindingAbi>::to_output(out)
                }));
                result.unwrap_or_else(|payload| {
                    $crate::native::raise_wire_panic(&*payload);
                    <<$ret as $crate::native::BindingAbi>::Output as ::core::default::Default>::default()
                })
            }

            #[allow(non_snake_case, reason = "the thunk is named after the user's function, whose case the macro cannot change")]
            fn [< __addr_ $sym __ $name >]() -> *const u8 {
                [< gos_binding_ $sym __ $name >] as *const u8
            }
        }
    };
}

/// Internal: the `extern "C"` wire thunk for one `cb_fn` binding, the
/// compiled tiers' way in. The body receives
/// [`crate::native::CompiledDispatch`]; a callback argument arrives as the
/// runtime handle it calls through.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_emit_native_export_cb {
    (__nosym, $name:ident, ( $($arg:ident : $argty:ty),* ), $ret:ty) => {};
    ($sym:ident, $name:ident, ( $($arg:ident : $argty:ty),* ), $ret:ty) => {
        $crate::__paste::paste! {
            #[unsafe(no_mangle)]
            #[allow(non_snake_case, unused_variables, unused_unsafe, reason = "the export is named after the user's function, and a binding with no arguments uses none of them")]
            pub extern "C" fn [< gos_binding_ $sym __ $name >](
                $( $arg : <$argty as $crate::native::BindingAbi>::Input ),*
            ) -> <$ret as $crate::native::BindingAbi>::Output {
                let _call = $crate::native::enter_wire_call();
                let result = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                    $(
                        // SAFETY: the codegen guarantees `$arg`
                        // is the C-ABI `Input` shape declared
                        // by `BindingAbi` for `$argty`.
                        let $arg: $argty =
                            unsafe { <$argty as $crate::native::BindingAbi>::from_input($arg) };
                    )*
                    let out: $ret = $name(&mut $crate::native::CompiledDispatch, $($arg),*);
                    <$ret as $crate::native::BindingAbi>::to_output(out)
                }));
                result.unwrap_or_else(|payload| {
                    $crate::native::raise_wire_panic(&*payload);
                    <<$ret as $crate::native::BindingAbi>::Output as ::core::default::Default>::default()
                })
            }

            #[allow(non_snake_case, reason = "the thunk is named after the user's function, whose case the macro cannot change")]
            fn [< __addr_ $sym __ $name >]() -> *const u8 {
                [< gos_binding_ $sym __ $name >] as *const u8
            }
        }
    };
}
