//! Proc-macros for `gossamer-binding`.
//!
//! Three attribute macros plus one derive:
//!
//! - `#[gos_module("name")]` - alternative to `register_module!`
//!   that takes the module path as a string literal argument and
//!   captures every `pub fn` inside the annotated `mod { ... }`
//!   as a binding item. Doc-comments flow through.
//! - `#[gos_opaque]` on an `impl Type { ... }` block - turns every
//!   `pub fn` (including `&self` / `&mut self` methods) into a
//!   binding item named `Type::method`, backed by a per-type
//!   `gossamer_binding::Registry<T>` keyed by `i64` handle.
//! - `#[gos_blocking]` on a binding fn - wraps the body in a
//!   blocking-pool dispatch so a long sync call doesn't stall the
//!   scheduler.
//! - `#[derive(GosStruct)]` on a Rust struct - emits
//!   `FromGos`/`ToGos`/`BindingAbi` so the struct passes through
//!   binding signatures as a `Type::Opaque(name)` struct value.
//!
//! All four are thin wrappers around the existing `macro_rules`
//! `register_module!` and the binding-side trait implementations
//! in `gossamer-binding::native` / `gossamer-binding::conv`.

#![deny(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    Attribute, Expr, FnArg, ImplItem, ItemImpl, ItemMod, LitStr, Pat, Type, parse_macro_input,
};

/// `#[gos_module("name")]` - register the items inside an
/// annotated `mod { ... }` as Gossamer-callable binding items
/// under the given path.
///
/// Each `pub fn` in the module body becomes one item. Doc-comments
/// flow into the `ItemFn::doc` field, so `gos doc <path>::<fn>`
/// renders them. The C-ABI symbol prefix is derived from the path
/// by replacing `::` with `__`; the prefix is the entire path for
/// single-segment names.
#[proc_macro_attribute]
pub fn gos_module(args: TokenStream, input: TokenStream) -> TokenStream {
    let path_lit = parse_macro_input!(args as LitStr);
    let module: ItemMod = parse_macro_input!(input as ItemMod);
    expand_gos_module(&path_lit, &module).into()
}

fn expand_gos_module(path_lit: &LitStr, module: &ItemMod) -> TokenStream2 {
    let path_str = path_lit.value();
    let prefix_str = path_str.replace("::", "__");
    let prefix_ident = format_ident!("{}", sanitize_ident(&prefix_str));
    let doc = extract_module_doc(&module.attrs);

    // `register_module!` emits its items inside a synthetic
    // `mod <prefix> { use super::*; ... }`. To let user `use`
    // imports (and other non-`pub fn` items) be visible to those
    // bodies, we re-emit every non-fn item at the parent scope -
    // `super::*` then pulls them into the synthetic mod.
    let mut fn_emits: Vec<TokenStream2> = Vec::new();
    let mut item_at_parent: Vec<TokenStream2> = Vec::new();

    if let Some((_, items)) = &module.content {
        for item in items {
            match item {
                syn::Item::Fn(f) if matches!(f.vis, syn::Visibility::Public(_)) => {
                    let doc_attrs = collect_doc_attrs(&f.attrs);
                    // `register_module!` names every return type, so a fn
                    // written without one states the unit it returns.
                    let mut sig = f.sig.clone();
                    if matches!(sig.output, syn::ReturnType::Default) {
                        sig.output = syn::parse_quote! { -> () };
                    }
                    let block = &f.block;
                    fn_emits.push(quote! {
                        #( #doc_attrs )*
                        #sig
                        #block
                    });
                }
                // Drop `use` items: they're scope-relative to the
                // user's `mod` and won't resolve once we lift the
                // bodies out. Authors should keep imports outside
                // the `#[gos_module]` block - `register_module!`
                // emits a `use super::*;` automatically so anything
                // visible at the macro invocation site is reachable
                // inside the binding bodies.
                syn::Item::Use(_) => {}
                other => {
                    item_at_parent.push(quote! { #other });
                }
            }
        }
    }

    quote! {
        // Helpers / type aliases / consts the user declared inside
        // the mod body land at parent scope so `register_module!`'s
        // synthesized `use super::*;` picks them up.
        #( #item_at_parent )*

        ::gossamer_binding::register_module!(
            #prefix_ident,
            path: #path_str,
            symbol_prefix: #prefix_ident,
            doc: #doc,

            #( #fn_emits )*
        );
    }
}

/// `#[gos_blocking]` on a free fn - wraps the body in
/// `gossamer_binding::blocking_pool::run_blocking(|| body)` so
/// the call doesn't stall the M:N scheduler. The wrapper is a
/// best-effort fall-through: the runtime helper is a no-op stub
/// on tiers without blocking-pool support, preserving the sync
/// behaviour.
#[proc_macro_attribute]
pub fn gos_blocking(_args: TokenStream, input: TokenStream) -> TokenStream {
    let item: syn::ItemFn = parse_macro_input!(input as syn::ItemFn);
    let attrs = &item.attrs;
    let vis = &item.vis;
    let sig = &item.sig;
    let body = &item.block;
    quote! {
        #( #attrs )*
        #vis #sig {
            ::gossamer_binding::blocking_pool::run_blocking(|| { #body })
        }
    }
    .into()
}

/// `#[gos_opaque]` on an `impl Type { ... }` - every `pub fn`
/// inside becomes a binding item named `Type::method`, with the
/// receiver translated into an `i64` handle threading through a
/// per-type `Registry<Mutex<Type>>`.
///
/// The macro emits:
///
/// 1. A `static` per-type registry of `Mutex<T>` values.
/// 2. One free-fn binding per method that takes the handle as the
///    first arg, looks the value up, locks it, and dispatches.
/// 3. A `Self`-returning method (constructor) becomes a binding
///    that registers the new value and returns the handle.
/// 4. A `register_module!` invocation with all generated bindings,
///    so the items appear in the Gossamer-side module table.
#[proc_macro_attribute]
pub fn gos_opaque(_args: TokenStream, input: TokenStream) -> TokenStream {
    let block: ItemImpl = parse_macro_input!(input as ItemImpl);
    expand_gos_opaque(&block).into()
}

fn expand_gos_opaque(block: &ItemImpl) -> TokenStream2 {
    let self_ty = &*block.self_ty;
    let type_name = type_to_ident(self_ty);
    let type_str = type_name.to_string();
    let registry_ident = format_ident!("__GOS_OPAQUE_REGISTRY_{}", type_str.to_uppercase());
    let mod_path_str = type_str.clone();
    let mod_ident = format_ident!("opaque_{}", type_str.to_lowercase());
    // The C-ABI prefix is derived from the module path the codegen sees, so
    // the two are spelled from the same string.
    let symbol_prefix_ident = format_ident!("{}", sanitize_ident(&mod_path_str));

    let mut binding_fns: Vec<TokenStream2> = Vec::new();
    let mut keep_methods: Vec<TokenStream2> = Vec::new();

    for item in &block.items {
        if let ImplItem::Fn(method) = item {
            let doc_attrs = collect_doc_attrs(&method.attrs);
            keep_methods.push(quote! { #method });
            let sig = &method.sig;
            let method_name = &sig.ident;
            let item_name = method_name.clone();

            let inputs = &sig.inputs;
            let output = &sig.output;

            // Classify the receiver (None = associated; &self =
            // immutable borrow; &mut self / self = exclusive).
            let receiver_kind = classify_receiver(inputs);

            // Build the parameter list passed to the synthesized
            // binding fn, plus the call-site arguments forwarded
            // into the original method.
            let (param_list, fwd_args) = forwarded_args(inputs);

            match receiver_kind {
                ReceiverKind::None => {
                    // Associated fn - if return type is `Self`,
                    // wrap into a fresh registry handle. Otherwise
                    // pass through.
                    let is_self_returning = returns_self(output);
                    let body = if is_self_returning {
                        quote! {
                            let value = <#self_ty>::#method_name(#fwd_args);
                            let handle = #registry_ident.insert(::gossamer_binding::__paste::paste! {
                                ::gossamer_binding::parking_lot::Mutex::new(value)
                            });
                            handle
                        }
                    } else {
                        quote! { <#self_ty>::#method_name(#fwd_args) }
                    };
                    let ret_ty = if is_self_returning {
                        quote! { -> i64 }
                    } else {
                        quote! { #output }
                    };
                    binding_fns.push(quote! {
                        #( #doc_attrs )*
                        fn #item_name(#param_list) #ret_ty {
                            #body
                        }
                    });
                }
                ReceiverKind::Ref => {
                    let ret_ty = match output {
                        syn::ReturnType::Default => quote! { -> () },
                        syn::ReturnType::Type(_, t) => quote! { -> #t },
                    };
                    binding_fns.push(quote! {
                        #( #doc_attrs )*
                        fn #item_name(__handle: i64, #param_list) #ret_ty {
                            let cell = match #registry_ident.get(__handle) {
                                Ok(c) => c,
                                Err(_) => return ::core::default::Default::default(),
                            };
                            let guard = cell.lock();
                            <#self_ty>::#method_name(&*guard, #fwd_args)
                        }
                    });
                }
                ReceiverKind::Mut => {
                    let ret_ty = match output {
                        syn::ReturnType::Default => quote! { -> () },
                        syn::ReturnType::Type(_, t) => quote! { -> #t },
                    };
                    binding_fns.push(quote! {
                        #( #doc_attrs )*
                        fn #item_name(__handle: i64, #param_list) #ret_ty {
                            let cell = match #registry_ident.get(__handle) {
                                Ok(c) => c,
                                Err(_) => return ::core::default::Default::default(),
                            };
                            let mut guard = cell.lock();
                            <#self_ty>::#method_name(&mut *guard, #fwd_args)
                        }
                    });
                }
            }
        }
    }

    quote! {
        impl #self_ty {
            #( #keep_methods )*
        }

        #[allow(non_upper_case_globals, reason = "the registry static is named after the user's type, whose case the macro cannot change")]
        static #registry_ident: ::gossamer_binding::opaque::Registry<::gossamer_binding::parking_lot::Mutex<#self_ty>>
            = ::gossamer_binding::opaque::Registry::new();

        ::gossamer_binding::register_module!(
            #mod_ident,
            path: #mod_path_str,
            symbol_prefix: #symbol_prefix_ident,
            doc: "Opaque-handle bindings.",

            #( #binding_fns )*
        );
    }
}

enum ReceiverKind {
    None,
    Ref,
    Mut,
}

fn classify_receiver(inputs: &syn::punctuated::Punctuated<FnArg, syn::Token![,]>) -> ReceiverKind {
    match inputs.first() {
        Some(FnArg::Receiver(r)) => {
            if r.mutability.is_some() {
                ReceiverKind::Mut
            } else {
                ReceiverKind::Ref
            }
        }
        _ => ReceiverKind::None,
    }
}

fn forwarded_args(
    inputs: &syn::punctuated::Punctuated<FnArg, syn::Token![,]>,
) -> (TokenStream2, TokenStream2) {
    let mut params: Vec<TokenStream2> = Vec::new();
    let mut fwd: Vec<TokenStream2> = Vec::new();
    for input in inputs {
        if let FnArg::Typed(pt) = input {
            let pat = &pt.pat;
            let ty = &pt.ty;
            params.push(quote! { #pat: #ty });
            if let Pat::Ident(pi) = &**pat {
                let id = &pi.ident;
                fwd.push(quote! { #id });
            } else {
                fwd.push(quote! { #pat });
            }
        }
    }
    let params_ts = quote! { #( #params ),* };
    let fwd_ts = quote! { #( #fwd ),* };
    (params_ts, fwd_ts)
}

fn returns_self(output: &syn::ReturnType) -> bool {
    let syn::ReturnType::Type(_, ty) = output else {
        return false;
    };
    matches!(&**ty, Type::Path(p) if p.path.is_ident("Self"))
}

fn type_to_ident(ty: &Type) -> syn::Ident {
    if let Type::Path(p) = ty
        && let Some(last) = p.path.segments.last()
    {
        return last.ident.clone();
    }
    format_ident!("Opaque")
}

/// `#[derive(GosStruct)]` - emits `FromGos` / `ToGos` /
/// `BindingAbi` impls that round-trip a Rust struct through
/// `Value::Struct` (interp tier) and an opaque-handle wire shape
/// (compiled tier).
#[proc_macro_derive(GosStruct)]
pub fn derive_gos_struct(input: TokenStream) -> TokenStream {
    let item: syn::DeriveInput = parse_macro_input!(input as syn::DeriveInput);
    let name = &item.ident;
    let name_str = name.to_string();
    let syn::Data::Struct(s) = &item.data else {
        return syn::Error::new_spanned(item, "GosStruct can only be derived on structs")
            .to_compile_error()
            .into();
    };
    let fields: Vec<(syn::Ident, Type)> = match &s.fields {
        syn::Fields::Named(named) => named
            .named
            .iter()
            .filter_map(|f| f.ident.as_ref().map(|id| (id.clone(), f.ty.clone())))
            .collect(),
        _ => {
            return syn::Error::new_spanned(item, "GosStruct requires a struct with named fields")
                .to_compile_error()
                .into();
        }
    };

    let field_names: Vec<&syn::Ident> = fields.iter().map(|(id, _)| id).collect();
    let field_names_str: Vec<String> = fields.iter().map(|(id, _)| id.to_string()).collect();
    let field_tys: Vec<&Type> = fields.iter().map(|(_, t)| t).collect();

    let lookup_arms: Vec<TokenStream2> = field_names
        .iter()
        .zip(field_tys.iter())
        .zip(field_names_str.iter())
        .map(|((name, ty), name_str)| {
            quote! {
                #name: {
                    let v = ::gossamer_binding::struct_helpers::struct_field(
                        &inner.fields, #name_str
                    );
                    <#ty as ::gossamer_binding::FromGos>::from_gos(v)?
                }
            }
        })
        .collect();

    quote! {
        impl ::gossamer_binding::ToGos for #name {
            fn to_gos(self) -> ::gossamer_binding::value::Value {
                let fields: ::std::vec::Vec<(::std::string::String, ::gossamer_binding::value::Value)> = vec![
                    #(
                        (
                            ::std::string::String::from(#field_names_str),
                            <#field_tys as ::gossamer_binding::ToGos>::to_gos(self.#field_names)
                        ),
                    )*
                ];
                ::gossamer_binding::struct_helpers::build_struct(#name_str, fields)
            }
        }

        impl ::gossamer_binding::FromGos for #name {
            fn from_gos(
                value: &::gossamer_binding::value::Value,
            ) -> ::gossamer_binding::value::RuntimeResult<Self> {
                let inner = match value {
                    ::gossamer_binding::value::Value::Struct(s) => s,
                    other => {
                        return Err(::gossamer_binding::value::RuntimeError::Type(
                            format!("expected struct {}, found {:?}", #name_str, other),
                        ));
                    }
                };
                Ok(#name {
                    #( #lookup_arms ),*
                })
            }
        }

        impl ::gossamer_binding::SigType for #name {
            const TYPE: ::gossamer_binding::Type = ::gossamer_binding::Type::Opaque(#name_str);
        }

        // Compiled-tier wire shape: ride the `DynValue` ABI. The
        // struct is encoded as `DynValue::Tagged { name, payload:
        // [<field_value>...] }` so every field's wire shape is
        // already handled by the variant payload tag system. This
        // is a defensive default - for hot paths, declare a
        // hand-tuned `BindingAbi` impl with a dedicated wire shape.
        #[allow(unsafe_code, reason = "compiled-tier C-ABI bridge")]
        impl ::gossamer_binding::native::BindingAbi for #name {
            type Input = *const ::gossamer_binding::native::GosDynVariant;
            type Output = *mut ::gossamer_binding::native::GosDynVariant;
            const TYPE: ::gossamer_binding::Type =
                ::gossamer_binding::Type::Opaque(#name_str);

            unsafe fn from_input(input: Self::Input) -> Self {
                let dv: ::gossamer_binding::DynValue =
                    unsafe {
                        <::gossamer_binding::DynValue as ::gossamer_binding::native::BindingAbi>::from_input(input)
                    };
                // The wire carries the fields positionally, in declaration
                // order; a `Value::Struct` round trip would need field
                // names the payload does not carry.
                let payload: ::std::vec::Vec<::gossamer_binding::DynValue> = match dv {
                    ::gossamer_binding::DynValue::Tagged { payload, .. } => payload,
                    ::gossamer_binding::DynValue::List(items) => items,
                    other => ::std::vec![other],
                };
                let mut fields = payload.into_iter();
                Self {
                    #(
                        #field_names: {
                            let next = fields
                                .next()
                                .unwrap_or(::gossamer_binding::DynValue::Nil);
                            let value = <::gossamer_binding::DynValue as ::gossamer_binding::ToGos>::to_gos(next);
                            <#field_tys as ::gossamer_binding::FromGos>::from_gos(&value)
                                .unwrap_or_else(|_| panic!(concat!(
                                    "invalid `", #name_str, ".", #field_names_str,
                                    "` payload at binding boundary"
                                )))
                        },
                    )*
                }
            }

            fn to_output(self) -> Self::Output {
                let payload: ::std::vec::Vec<::gossamer_binding::DynValue> = ::std::vec![
                    #(
                        {
                            let value = <#field_tys as ::gossamer_binding::ToGos>::to_gos(self.#field_names);
                            <::gossamer_binding::DynValue as ::gossamer_binding::FromGos>::from_gos(&value)
                                .unwrap_or(::gossamer_binding::DynValue::Nil)
                        },
                    )*
                ];
                <::gossamer_binding::DynValue as ::gossamer_binding::native::BindingAbi>::to_output(
                    ::gossamer_binding::DynValue::Tagged {
                        name: ::std::string::String::from(#name_str),
                        payload,
                    },
                )
            }
        }
    }
    .into()
}

// --- helpers ---------------------------------------------------------

fn collect_doc_attrs(attrs: &[Attribute]) -> Vec<&Attribute> {
    attrs.iter().filter(|a| a.path().is_ident("doc")).collect()
}

fn extract_module_doc(attrs: &[Attribute]) -> String {
    let mut out = String::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta
            && let Expr::Lit(lit) = &nv.value
            && let syn::Lit::Str(s) = &lit.lit
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(s.value().trim());
        }
    }
    out
}

fn sanitize_ident(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
