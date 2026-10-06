//! The caller-side spellings every pass after the front end's resolve
//! phase should never see.
//!
//! A labelled argument, a defaulted parameter, and a std function named
//! in value position are all written for the reader's benefit and mean
//! something the checker, HIR, and each tier's codegen already handle in
//! one canonical shape. Rewriting them in one place is what keeps a
//! REPL line, a playground snippet, and a file on disk agreeing about
//! what a call means: every front end calls this, so none of them can
//! drift into accepting a spelling the others reject.

#![forbid(unsafe_code)]

use gossamer_ast::{ImplItem, Item, ItemKind, ModBody, SourceFile};
use gossamer_resolve::{Resolutions, ResolveDiagnostic};

/// Rewrites `sf` in place into the canonical call spellings and returns
/// the diagnostics the named-argument rewrite produced.
pub fn normalize_caller_side_spellings(
    sf: &mut SourceFile,
    resolutions: &Resolutions,
) -> Vec<ResolveDiagnostic> {
    let (mut diagnostics, deferred) = gossamer_resolve::resolve_named_arguments(sf, resolutions);
    let _ = crate::std_fn_eta::expand_std_fn_values(sf, resolutions);
    diagnostics.extend(crate::data_first::rotate_data_first_calls(sf, resolutions));
    name_display_channel(&mut sf.items);
    // A method name several types declare with different parameters is
    // rewritten against the declaration its receiver's type reaches, which
    // only a check of the otherwise canonical program can say.
    if !deferred.is_empty() {
        let receivers =
            crate::checker::receiver_owners(sf, resolutions, deferred.iter().map(|d| d.call));
        diagnostics.extend(gossamer_resolve::resolve_deferred_method_arguments(
            sf,
            resolutions,
            deferred,
            &|call| receivers.get(&call).cloned(),
        ));
    }
    diagnostics
}

/// Renames the `fn fmt` of an `impl Display for T` block to the name the
/// `{}` channel dispatches on. `Display` and `Debug` each declare one
/// method that answers a `String` and both are written `fn fmt`, so the
/// `impl` header is what separates the two renderings; downstream every
/// channel needs a name of its own, and `to_string` is the one `{}` and
/// `x.to_string()` both reach.
fn name_display_channel(items: &mut [Item]) {
    for item in items {
        match &mut item.kind {
            ItemKind::Impl(decl) => {
                let displays = decl
                    .trait_ref
                    .as_ref()
                    .and_then(|bound| bound.path.segments.last())
                    .is_some_and(|segment| segment.name.name == "Display");
                if !displays {
                    continue;
                }
                for impl_item in &mut decl.items {
                    if let ImplItem::Fn(fn_decl) = impl_item
                        && fn_decl.name.name == "fmt"
                    {
                        fn_decl.name.name = "to_string".to_string();
                    }
                }
            }
            ItemKind::Mod(decl) => {
                if let ModBody::Inline(inner) = &mut decl.body {
                    name_display_channel(inner);
                }
            }
            _ => {}
        }
    }
}
