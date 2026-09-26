//! Declarative macros that turn idiomatic Rust function items
//! into [`crate::ItemFn`] entries inside a [`crate::Module`]
//! registered in [`crate::REGISTRY`].
//!
//! Two binding shapes are supported:
//!
//! - `fn name(arg: T, ...) -> R { body }` - plain binding fn.
//! - `cb_fn name(dispatch, arg: T, ...) -> R { body }` - same as
//!   plain, but the body has access to `dispatch: &mut dyn
//!   NativeDispatch` so it can re-enter the interpreter through
//!   [`crate::NativeDispatch::call_value`] (`Terminal::draw` and
//!   any other higher-order binding APIs).

/// Internal: counts the supplied identifiers at compile time.
#[doc(hidden)]
#[macro_export]
macro_rules! __binding_count {
    () => { 0_usize };
    ($head:ident $($tail:ident)*) => { 1_usize + $crate::__binding_count!($($tail)*) };
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
/// **Interp-only form (no `symbol_prefix:`):** legacy compat;
/// modules declared this way are not reachable from `gos build`.
#[macro_export]
macro_rules! register_module {
    // New ergonomic form - single-segment path: `name: <ident>`
    // doubles as both the Gossamer-side spelling and the symbol
    // prefix. An auto-generated internal mod (`__gos_<name>`)
    // wraps the items. No explicit `__bindings_force_link()`
    // needed at crate root - the force-link entry is published
    // through the link-time `__GOS_FORCE_LINK_FNS` distributed
    // slice and the runner walks it automatically.
    (
        name: $name:ident,
        doc: $doc:literal,
        $($body:tt)*
    ) => {
        $crate::__paste::paste! {
            $crate::__rm_munch! {
                [< __gos_ $name >], stringify!($name), $name, $doc,
                simple = [],
                cb = [],
                rest = [ $($body)* ]
            }
        }
        // Publish a link-time force-link entry so the runner
        // doesn't need a per-crate `__bindings_force_link()` shim.
        $crate::__paste::paste! {
            #[$crate::linkme::distributed_slice($crate::FORCE_LINK_FNS)]
            #[linkme(crate = $crate::linkme)]
            #[allow(non_upper_case_globals, unreachable_pub, reason = "the linker anchor is named after the user's module, and the macro expands at the caller's visibility")]
            static [< __GOS_FORCE_LINK_ $name >]: fn() = [< __gos_ $name >]::force_link;
        }
    };

    (
        $modname:ident,
        path: $path:literal,
        symbol_prefix: $sym:ident,
        doc: $doc:literal,
        $($body:tt)*
    ) => {
        $crate::__rm_munch! {
            $modname, $path, $sym, $doc,
            simple = [],
            cb = [],
            rest = [ $($body)* ]
        }
        $crate::__paste::paste! {
            #[$crate::linkme::distributed_slice($crate::FORCE_LINK_FNS)]
            #[linkme(crate = $crate::linkme)]
            #[allow(non_upper_case_globals, unreachable_pub, reason = "the linker anchor is named after the user's module, and the macro expands at the caller's visibility")]
            static [< __GOS_FORCE_LINK_ $sym >]: fn() = $modname::force_link;
        }
    };

    // Backwards-compatible form without `symbol_prefix:` - only
    // the interpreter thunks are emitted, so binding fns from
    // these modules are reachable from `gos` but not
    // `gos build`. Documented as the legacy path; new bindings
    // should specify `symbol_prefix:` explicitly.
    (
        $modname:ident,
        path: $path:literal,
        doc: $doc:literal,
        $($body:tt)*
    ) => {
        $crate::__rm_munch! {
            $modname, $path, __nosym, $doc,
            simple = [],
            cb = [],
            rest = [ $($body)* ]
        }
        $crate::__paste::paste! {
            #[$crate::linkme::distributed_slice($crate::FORCE_LINK_FNS)]
            #[linkme(crate = $crate::linkme)]
            #[allow(non_upper_case_globals, unreachable_pub, reason = "the linker anchor is named after the user's module, and the macro expands at the caller's visibility")]
            static [< __GOS_FORCE_LINK_ $modname >]: fn() = $modname::force_link;
        }
    };
}

/// Internal: tt-muncher that walks the binding-fn list and
/// classifies each entry as plain or callback-aware before the
/// final emit step.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_munch {
    // ---- terminal: emit module ---------------------------------
    (
        $modname:tt, $path:expr, $sym:tt, $doc:literal,
        simple = [ $({
            $sn:ident,
            ( $($sa:ident : $st:ty),* ),
            $sr:ty,
            $sb:block,
            doc = [ $($sd:literal)* ]
        })* ],
        cb = [ $({
            $cn:ident,
            $cdisp:ident,
            ( $($ca:ident : $ct:ty),* ),
            $cr:ty,
            $cb_body:block,
            doc = [ $($cd:literal)* ]
        })* ],
        rest = []
    ) => {
        #[allow(non_snake_case, dead_code, clippy::missing_docs_in_private_items, reason = "the module is named after the user's binding and holds generated thunks nothing else documents")]
        mod $modname {
            use super::*;

            $crate::__paste::paste! {
                $(
                    pub fn $sn($($sa : $st),*) -> $sr $sb

                    #[allow(non_snake_case, reason = "the thunk is named after the user's function, whose case the macro cannot change")]
                    pub fn [< __thunk_ $sn >](
                        _dispatch: &mut dyn $crate::NativeDispatch,
                        args: &[$crate::Value],
                    ) -> $crate::RuntimeResult<$crate::Value> {
                        let expected = $crate::__binding_count!($($sa)*);
                        if args.len() != expected {
                            return Err($crate::RuntimeError::Arity {
                                expected,
                                found: args.len(),
                            });
                        }
                        let mut iter = args.iter();
                        $(
                            let $sa: $st =
                                <$st as $crate::FromGos>::from_gos(iter.next().unwrap())?;
                        )*
                        let out: $sr = $sn($($sa),*);
                        Ok(<$sr as $crate::ToGos>::to_gos(out))
                    }

                    $crate::__rm_emit_native_export! {
                        $sym, $sn, ( $($sa : $st),* ), $sr
                    }
                )*

                $(
                    pub fn $cn(
                        $cdisp: &mut dyn $crate::NativeDispatch,
                        $($ca : $ct),*
                    ) -> $cr $cb_body

                    #[allow(non_snake_case, reason = "the thunk is named after the user's function, whose case the macro cannot change")]
                    pub fn [< __thunk_ $cn >](
                        _dispatch: &mut dyn $crate::NativeDispatch,
                        args: &[$crate::Value],
                    ) -> $crate::RuntimeResult<$crate::Value> {
                        let expected = $crate::__binding_count!($($ca)*);
                        if args.len() != expected {
                            return Err($crate::RuntimeError::Arity {
                                expected,
                                found: args.len(),
                            });
                        }
                        let mut iter = args.iter();
                        $(
                            let $ca: $ct =
                                <$ct as $crate::FromGos>::from_gos(iter.next().unwrap())?;
                        )*
                        let out: $cr = $cn(_dispatch, $($ca),*);
                        Ok(<$cr as $crate::ToGos>::to_gos(out))
                    }

                    $crate::__rm_emit_native_export_cb! {
                        $sym, $cn, ( $($ca : $ct),* ), $cr
                    }
                )*

                pub static ITEMS: &[$crate::ItemFn] = &[
                    $(
                        $crate::ItemFn {
                            name: stringify!($sn),
                            call: [< __thunk_ $sn >],
                            signature: $crate::Signature {
                                params: &[
                                    $( <$st as $crate::SigType>::TYPE ),*
                                ],
                                ret: <$sr as $crate::SigType>::TYPE,
                            },
                            doc: concat!( "" $(, $sd, "\n")* ),
                        },
                    )*
                    $(
                        $crate::ItemFn {
                            name: stringify!($cn),
                            call: [< __thunk_ $cn >],
                            signature: $crate::Signature {
                                params: &[
                                    $( <$ct as $crate::SigType>::TYPE ),*
                                ],
                                ret: <$cr as $crate::SigType>::TYPE,
                            },
                            doc: concat!( "" $(, $cd, "\n")* ),
                        },
                    )*
                ];

                // Compile-time signature validation: every param
                // and return type must implement both `SigType`
                // (for the type-checker) and `FromGos`/`ToGos`
                // (for the interp thunk). A binding fn that names
                // an unsupported type fails here with a clear
                // trait-bound error at the binding's compile,
                // rather than as a runtime install_all() panic.
                const _: () = {
                    $(
                        const fn [< __validate_ $sn >]() {
                            let _ = <$sr as $crate::SigType>::TYPE;
                            $( let _ = <$st as $crate::SigType>::TYPE; )*
                        }
                        let _ = [< __validate_ $sn >];
                    )*
                    $(
                        const fn [< __validate_ $cn >]() {
                            let _ = <$cr as $crate::SigType>::TYPE;
                            $( let _ = <$ct as $crate::SigType>::TYPE; )*
                        }
                        let _ = [< __validate_ $cn >];
                    )*
                };
            }

            pub static MODULE: $crate::Module = $crate::Module {
                path: $path,
                doc: $doc,
                items: ITEMS,
            };

            #[$crate::linkme::distributed_slice($crate::REGISTRY)]
            #[linkme(crate = $crate::linkme)]
            #[allow(non_upper_case_globals, reason = "the static is named after the user's binding, whose case the macro cannot change")]
            static REGISTERED: &'static $crate::Module = &MODULE;

            /// Emits a hard reference to `MODULE` so the linker
            /// keeps the [`linkme`] entry alive across LTO. Every
            /// binding crate must expose `pub fn
            /// __bindings_force_link()` at its crate root that
            /// chains into this; see
            /// [`crate::register_module!`] for the convention.
            ///
            /// Also publishes each item's `gos_binding_<...>` C-ABI
            /// thunk address into the codegen's native-symbol
            /// registry so the cranelift JIT can resolve calls into
            /// this module from JIT-compiled bodies.
            pub fn force_link() {
                let _: &'static $crate::Module = &MODULE;
                $crate::__paste::paste! {
                    $(
                        $crate::__rm_register_native_export! {
                            $sym, $sn
                        }
                    )*
                    $(
                        $crate::__rm_register_native_export! {
                            $sym, $cn
                        }
                    )*
                }
            }
        }
    };

    // ---- munch: cb_fn ------------------------------------------
    (
        $modname:tt, $path:expr, $sym:tt, $doc:literal,
        simple = [ $($simple:tt)* ],
        cb = [ $($cb:tt)* ],
        rest = [
            $(#[doc = $fdoc:literal])*
            cb_fn $name:ident( $disp:ident, $($arg:ident : $argty:ty),* $(,)? ) -> $ret:ty $body:block
            $($rest:tt)*
        ]
    ) => {
        $crate::__rm_munch! {
            $modname, $path, $sym, $doc,
            simple = [ $($simple)* ],
            cb = [ $($cb)* {
                $name,
                $disp,
                ( $($arg : $argty),* ),
                $ret,
                $body,
                doc = [ $($fdoc)* ]
            } ],
            rest = [ $($rest)* ]
        }
    };

    // ---- munch: fn with no return type --------------------------
    (
        $modname:tt, $path:expr, $sym:tt, $doc:literal,
        simple = [ $($simple:tt)* ],
        cb = [ $($cb:tt)* ],
        rest = [
            $(#[doc = $fdoc:literal])*
            fn $name:ident( $($arg:ident : $argty:ty),* $(,)? ) $body:block
            $($rest:tt)*
        ]
    ) => {
        $crate::__rm_munch! {
            $modname, $path, $sym, $doc,
            simple = [ $($simple)* ],
            cb = [ $($cb)* ],
            rest = [
                $(#[doc = $fdoc])*
                fn $name( $($arg : $argty),* ) -> () $body
                $($rest)*
            ]
        }
    };

    // ---- munch: plain fn ---------------------------------------
    (
        $modname:tt, $path:expr, $sym:tt, $doc:literal,
        simple = [ $($simple:tt)* ],
        cb = [ $($cb:tt)* ],
        rest = [
            $(#[doc = $fdoc:literal])*
            fn $name:ident( $($arg:ident : $argty:ty),* $(,)? ) -> $ret:ty $body:block
            $($rest:tt)*
        ]
    ) => {
        $crate::__rm_munch! {
            $modname, $path, $sym, $doc,
            simple = [ $($simple)* {
                $name,
                ( $($arg : $argty),* ),
                $ret,
                $body,
                doc = [ $($fdoc)* ]
            } ],
            cb = [ $($cb)* ],
            rest = [ $($rest)* ]
        }
    };
}

/// Internal: emits the `extern "C"` thunk for one plain binding
/// fn, plus a [`crate::NativeSymbolEntry`] entry into the
/// link-time `NATIVE_SYMBOLS` slice so the cranelift JIT can
/// resolve calls into the binding without a runtime registration
/// hop. Skipped when `$sym` is the `__nosym` sentinel (the legacy
/// `register_module!` form without `symbol_prefix:`).
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
                result.unwrap_or_else(|_| {
                    <<$ret as $crate::native::BindingAbi>::Output as ::core::default::Default>::default()
                })
            }

            #[allow(non_snake_case, reason = "the thunk is named after the user's function, whose case the macro cannot change")]
            fn [< __addr_ $sym __ $name >]() -> *const u8 {
                [< gos_binding_ $sym __ $name >] as *const u8
            }

            #[$crate::linkme::distributed_slice($crate::NATIVE_SYMBOLS)]
            #[linkme(crate = $crate::linkme)]
            #[allow(non_upper_case_globals, reason = "the static is named after the user's binding, whose case the macro cannot change")]
            static [< __NATIVE_SYM_ $sym __ $name >]: $crate::NativeSymbolEntry =
                $crate::NativeSymbolEntry {
                    name: concat!(
                        "gos_binding_",
                        stringify!($sym),
                        "__",
                        stringify!($name),
                    ),
                    addr_fn: [< __addr_ $sym __ $name >],
                };
        }
    };
}

/// Internal: the `extern "C"` thunk for one `cb_fn` binding, the compiled
/// tiers' way in. A compiled program has no interpreter to re-enter, so the
/// body receives [`crate::native::CompiledDispatch`]; a callback argument
/// arrives as the runtime handle its `invoke` calls through.
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
                result.unwrap_or_else(|_| {
                    <<$ret as $crate::native::BindingAbi>::Output as ::core::default::Default>::default()
                })
            }

            #[allow(non_snake_case, reason = "the thunk is named after the user's function, whose case the macro cannot change")]
            fn [< __addr_ $sym __ $name >]() -> *const u8 {
                [< gos_binding_ $sym __ $name >] as *const u8
            }

            #[$crate::linkme::distributed_slice($crate::NATIVE_SYMBOLS)]
            #[linkme(crate = $crate::linkme)]
            #[allow(non_upper_case_globals, reason = "the static is named after the user's binding, whose case the macro cannot change")]
            static [< __NATIVE_SYM_ $sym __ $name >]: $crate::NativeSymbolEntry =
                $crate::NativeSymbolEntry {
                    name: concat!(
                        "gos_binding_",
                        stringify!($sym),
                        "__",
                        stringify!($name),
                    ),
                    addr_fn: [< __addr_ $sym __ $name >],
                };
        }
    };
}

/// Internal: at `force_link()` time, registers one binding's C-ABI
/// thunk address with the codegen's native-symbol registry so the
/// cranelift JIT can resolve calls into the binding from
/// JIT-compiled bodies.
///
/// Skipped (no-op) when `$sym` is the `__nosym` sentinel - that
/// form of `register_module!` doesn't emit a C-ABI thunk to begin
/// with, so there's nothing to publish.
#[doc(hidden)]
#[macro_export]
macro_rules! __rm_register_native_export {
    (__nosym, $name:ident) => {};
    ($sym:ident, $name:ident) => {
        $crate::__paste::paste! {
            $crate::__register_native_symbol(
                concat!(
                    "gos_binding_",
                    stringify!($sym),
                    "__",
                    stringify!($name),
                ),
                [< gos_binding_ $sym __ $name >] as *const u8,
            );
        }
    };
}
