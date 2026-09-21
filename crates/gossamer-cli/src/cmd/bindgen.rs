//! `gos bindgen` - scaffold a `#[gos_module]` binding skeleton
//! from a Rust source file.
//!
//! Walks the supplied file's `pub fn` items via the `syn` parser
//! and emits a ready-to-edit binding crate. Functions whose
//! signatures use unsupported types are emitted as `/// Unsupported`
//! comments so the binding author sees the gap immediately.
//!
//! The classifier is conservative: anything outside the
//! `BindingAbi`-implemented vocabulary (primitive numerics,
//! `bool`, `char`, `String`, `Vec<T>`, `Option<T>`, `Result<T, E>`,
//! `Bytes`, tuples of those, `HashMap<K, V>` for declared pairs)
//! is flagged. User structs that derive `GosStruct` ride through
//! as `Type::Opaque(name)` - bindgen can't see the derive at this
//! stage, so any non-primitive bare-ident type is preserved
//! literally and the author keeps or replaces it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// Bindgen entry point.
pub(crate) fn run(input: &Path, output: Option<&Path>, module: Option<&str>) -> Result<()> {
    let src = fs::read_to_string(input).with_context(|| format!("reading {}", input.display()))?;
    let parsed = syn_parse_file(&src)
        .with_context(|| format!("parsing {} as Rust source", input.display()))?;

    let crate_name = derive_crate_name(input);
    let module_name = module.map_or_else(|| crate_name.replace('-', "_"), str::to_string);
    let out_dir = output.map_or_else(
        || PathBuf::from(".gos-bindings").join(crate_name.clone()),
        Path::to_path_buf,
    );

    let mut supported: Vec<RenderedItem> = Vec::new();
    let mut unsupported: Vec<UnsupportedItem> = Vec::new();
    for item in &parsed.items {
        let Some(rendered) = render_item(item) else {
            continue;
        };
        match rendered {
            ItemResult::Supported(r) => supported.push(r),
            ItemResult::Unsupported(u) => unsupported.push(u),
        }
    }

    fs::create_dir_all(out_dir.join("src"))
        .with_context(|| format!("creating {}", out_dir.display()))?;
    let cargo_toml = render_cargo_toml(&crate_name);
    let lib_rs = render_lib_rs(&module_name, &supported, &unsupported);
    fs::write(out_dir.join("Cargo.toml"), cargo_toml)?;
    fs::write(out_dir.join("src").join("lib.rs"), lib_rs)?;

    outln!(
        "bindgen: scaffolded {} items ({} unsupported) at {}",
        supported.len(),
        unsupported.len(),
        out_dir.display()
    );
    Ok(())
}

fn syn_parse_file(src: &str) -> Result<syn::File> {
    syn::parse_file(src).map_err(|e| anyhow!("syn parse: {e}"))
}

fn derive_crate_name(input: &Path) -> String {
    // Prefer the parent directory name (rust-crate convention); fall
    // back to the file stem.
    if let Some(parent) = input.parent()
        && let Some(name) = parent.file_name().and_then(|s| s.to_str())
        && !name.is_empty()
        && name != "."
    {
        return name.to_string();
    }
    input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("bindgen-out")
        .to_string()
}

enum ItemResult {
    Supported(RenderedItem),
    Unsupported(UnsupportedItem),
}

struct RenderedItem {
    #[allow(dead_code, reason = "kept for future per-item diagnostics")]
    name: String,
    signature: String,
    doc: Vec<String>,
}

struct UnsupportedItem {
    name: String,
    reason: String,
}

fn render_item(item: &syn::Item) -> Option<ItemResult> {
    let syn::Item::Fn(f) = item else {
        return None;
    };
    if !matches!(f.vis, syn::Visibility::Public(_)) {
        return None;
    }
    let name = f.sig.ident.to_string();
    let doc = collect_docs(&f.attrs);

    if let Some(reason) = classify_unsupported(&f.sig) {
        return Some(ItemResult::Unsupported(UnsupportedItem { name, reason }));
    }

    let signature = render_signature(&f.sig);
    Some(ItemResult::Supported(RenderedItem {
        name,
        signature,
        doc,
    }))
}

fn collect_docs(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut out = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta
            && let syn::Expr::Lit(lit) = &nv.value
            && let syn::Lit::Str(s) = &lit.lit
        {
            out.push(s.value());
        }
    }
    out
}

fn render_signature(sig: &syn::Signature) -> String {
    let mut buf = format!("fn {}(", sig.ident);
    let mut first = true;
    for input in &sig.inputs {
        if let syn::FnArg::Typed(pt) = input {
            if !first {
                buf.push_str(", ");
            }
            first = false;
            buf.push_str(&format!(
                "{}: {}",
                pat_to_string(&pt.pat),
                type_to_string(&pt.ty)
            ));
        }
    }
    buf.push(')');
    if let syn::ReturnType::Type(_, ty) = &sig.output {
        buf.push_str(&format!(" -> {}", type_to_string(ty)));
    }
    buf
}

fn pat_to_string(pat: &syn::Pat) -> String {
    if let syn::Pat::Ident(pi) = pat {
        pi.ident.to_string()
    } else {
        "_arg".to_string()
    }
}

fn type_to_string(ty: &syn::Type) -> String {
    use quote::ToTokens;
    ty.to_token_stream().to_string().replace(' ', "")
}

/// Returns `Some(reason)` if `sig` uses a type the binding ABI
/// doesn't support. Returns `None` if every type checks out.
fn classify_unsupported(sig: &syn::Signature) -> Option<String> {
    // Reject `&self` / `&mut self` - those are for `#[gos_opaque]`,
    // not free-fn bindings.
    for input in &sig.inputs {
        if let syn::FnArg::Receiver(_) = input {
            return Some("method receiver - use `#[gos_opaque]` instead".to_string());
        }
    }
    for input in &sig.inputs {
        if let syn::FnArg::Typed(pt) = input
            && let Some(reason) = type_unsupported_reason(&pt.ty)
        {
            return Some(reason);
        }
    }
    if let syn::ReturnType::Type(_, ty) = &sig.output
        && let Some(reason) = type_unsupported_reason(ty)
    {
        return Some(reason);
    }
    None
}

const SUPPORTED_PRIMITIVES: &[&str] = &[
    "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "usize", "isize", "f32", "f64", "bool",
    "char",
];
const SUPPORTED_CONTAINER_IDENTS: &[&str] = &[
    "String",
    "Bytes",
    "Vec",
    "Option",
    "Result",
    "HashMap",
    "DynValue",
    "BindingCallback",
    "NativeCallback",
    "PersistentCallback",
    "GosError",
];

fn type_unsupported_reason(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => {
            let last = p.path.segments.last()?;
            let ident = last.ident.to_string();
            if ident == "Self" {
                return Some("Self return - use `#[gos_opaque]` instead".to_string());
            }
            if SUPPORTED_PRIMITIVES.contains(&ident.as_str()) {
                return None;
            }
            if SUPPORTED_CONTAINER_IDENTS.contains(&ident.as_str()) {
                // Recurse into generic args.
                if let syn::PathArguments::AngleBracketed(args) = &last.arguments {
                    for arg in &args.args {
                        if let syn::GenericArgument::Type(inner) = arg
                            && let Some(reason) = type_unsupported_reason(inner)
                        {
                            return Some(reason);
                        }
                    }
                }
                return None;
            }
            // Bare-ident type: assume it's a user struct expected
            // to derive `GosStruct`. Pass through; the binding
            // author either keeps the derive or replaces it.
            None
        }
        syn::Type::Tuple(t) => {
            for elem in &t.elems {
                if let Some(reason) = type_unsupported_reason(elem) {
                    return Some(reason);
                }
            }
            None
        }
        syn::Type::Reference(r) => Some(format!(
            "reference type `&{}` - pass by value or use `String`/`Bytes` for borrowed payloads",
            type_to_string(&r.elem)
        )),
        syn::Type::Array(_) | syn::Type::Slice(_) => {
            Some("fixed array / slice - use `Vec<T>` or `Bytes` instead".to_string())
        }
        syn::Type::Ptr(_) | syn::Type::BareFn(_) | syn::Type::TraitObject(_) => Some(
            "pointer / trait-object / bare-fn type - not supported at the binding boundary"
                .to_string(),
        ),
        other => Some(format!("unsupported type shape: {}", type_to_string(other))),
    }
}

fn render_cargo_toml(crate_name: &str) -> String {
    format!(
        "[package]\n\
         name = \"{crate_name}-binding\"\n\
         version = \"0.0.1\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         [workspace]\n\
         \n\
         [lib]\n\
         crate-type = [\"rlib\"]\n\
         \n\
         [dependencies]\n\
         {crate_name} = \"*\"  # pin to the version you intend\n\
         gossamer-binding = \"{binding_version}\"\n",
        binding_version = gossamer_pkg::toolchain_version(),
    )
}

fn render_lib_rs(
    module_name: &str,
    supported: &[RenderedItem],
    unsupported: &[UnsupportedItem],
) -> String {
    let mut buf = String::new();
    buf.push_str(&format!(
        "//! Generated bindings for `{module_name}`.\n\
         //!\n\
         //! Bodies marked `todo()` are placeholders - fill them in.\n\
         //! Items flagged `Unsupported` need hand-shaped wrappers\n\
         //! before they can cross the binding boundary.\n\
         \n\
         use gossamer_binding::gos_module;\n\
         \n\
         #[gos_module(\"{module_name}\")]\n\
         mod bindings {{\n\
         \x20\x20\x20\x20use super::*;\n"
    ));
    for item in supported {
        for d in &item.doc {
            buf.push_str(&format!("\x20\x20\x20\x20///{d}\n"));
        }
        buf.push_str(&format!(
            "\x20\x20\x20\x20pub {} {{ todo() }}\n\n",
            item.signature
        ));
    }
    if !unsupported.is_empty() {
        buf.push_str(
            "\x20\x20\x20\x20// --- Unsupported items ----------------------------------\n",
        );
        for u in unsupported {
            buf.push_str(&format!("\x20\x20\x20\x20// `{}`: {}\n", u.name, u.reason));
        }
    }
    buf.push_str("}\n");
    buf
}
