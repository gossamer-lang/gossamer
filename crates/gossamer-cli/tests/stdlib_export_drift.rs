//! Keeps the resolver's checked-in stdlib export table in sync with
//! the runtime builtin registry. If the stdlib surface changes, this
//! fails with the diff so the table in
//! `gossamer-resolve/src/stdlib_exports.rs` is regenerated - without
//! it, a newly-added `module::fn` would be wrongly rejected by
//! `gos check` / the LSP as an unknown member.

/// Public members implemented by parse-time injected Gossamer wrappers rather
/// than direct interpreter registrations.
const SOURCE_VISIBLE_VIA_REWRITE: &[&str] = &[
    "path::Path",
    "path::Path::as_str",
    "path::Path::extension",
    "path::Path::file_name",
    "path::Path::is_absolute",
    "path::Path::join",
    "path::Path::new",
    "path::Path::normalize",
    "path::Path::parent",
    "path::Path::starts_with",
    "path::Path::stem",
    "time::CivilResolution::Fold",
    "time::CivilResolution::Gap",
    "time::CivilResolution::Unique",
    "time::Location::civil",
    "time::Location::fixed",
    "time::Location::lookup",
    "time::Location::name",
    "time::Location::resolve",
    "time::Location::utc",
    "time::add_date",
    "time::format_in",
];

#[test]
fn resolver_stdlib_table_matches_runtime() {
    let mut live: Vec<&str> = gossamer_interp::registered_names()
        .into_iter()
        .filter(|n| n.contains("::") && n.chars().next().is_some_and(char::is_lowercase))
        .collect();
    live.sort_unstable();
    live.dedup();

    let table: Vec<&str> = gossamer_resolve::STDLIB_QUALIFIED.to_vec();

    let missing: Vec<&str> = live
        .iter()
        .filter(|n| !n.contains("::__gos_"))
        .filter(|n| !table.contains(n))
        .copied()
        .collect();
    let extra: Vec<&str> = table
        .iter()
        .filter(|n| !SOURCE_VISIBLE_VIA_REWRITE.contains(n))
        .filter(|n| !live.contains(n))
        .copied()
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "stdlib export table drifted from runtime registry.\n  \
         missing from table (regenerate stdlib_exports.rs): {missing:?}\n  \
         extra in table (no longer registered): {extra:?}"
    );
}

/// Intentional deprecated re-exports the team keeps callable even
/// though their canonical spelling lives under a different module in
/// the manifest. This is a closed list, not a dumping ground - every
/// entry must be a deliberate alias, not an unmanifested member.
const ALLOWED_UNMANIFESTED: &[&str] = &[
    // Each entry's canonical spelling is the manifest member; these
    // are convenience / deprecated aliases the runtime keeps callable.
    "channel::new",                           // -> sync::channel
    "channel::unbounded",                     // -> sync::channel_unbounded
    "fs::create_dir",                         // -> fs::create_dir_all
    "fs::create_dir_all",                     // -> fs::create_dir_all
    "fs::read",                               // -> fs::read
    "math::rem",                              // -> math::rem
    "os::home",                               // -> env::home_dir
    "os::list_dir",                           // -> os::read_dir
    "os::set_cwd",                            // -> env::set_current_dir
    "path::walk",                             // -> fs::walk_dir
    "thread::sleep_ms",                       // -> time::sleep
    "encoding::utf16::is_surrogate",          // -> utf16::is_surrogate
    "encoding::utf16::rune_len",              // -> utf16::rune_len
    "encoding::utf16::decode_surrogate_pair", // -> utf16::decode_surrogate_pair
    "encoding::utf16::encode_string",         // -> utf16::encode_string
    "encoding::utf16::decode_to_string",      // -> utf16::decode_to_string
];

/// Primitive integer associated methods belong to the language type catalog,
/// not to a `std::` module manifest. Keep this recognition structural and
/// closed so an unrelated lowercase runtime owner cannot bypass the manifest
/// audit merely by looking like a primitive type.
fn is_primitive_scalar_method(name: &str) -> bool {
    let Some((owner, method)) = name.split_once("::") else {
        return false;
    };
    matches!(owner, "f32" | "f64") && matches!(method, "to_bits" | "from_bits")
}

/// Every registered `module::fn` must name a member the canonical
/// manifest advertises. Without this guard, a runtime builtin can
/// register an unmanifested alias that passes `gos check` and runs on
/// the VM, yet has no manifest entry - the structural hole that let a
/// drift of VM-only aliases accumulate. `module::Type::method` forms
/// (the segment before the member is uppercase) are type-associated
/// methods, not free functions, and are not manifest members.
#[test]
fn registry_members_match_manifest() {
    use std::collections::{HashMap, HashSet};

    // (canonical_path, member) every manifest module advertises.
    let mut pairs: HashSet<(&str, &str)> = HashSet::new();
    // Source-spelling binding -> the canonical paths it can resolve to.
    // Keyed by BOTH the full path (`encoding::json`) and its last
    // segment (`json`), so `json::parse` and `encoding::json::parse`
    // both reach `std::encoding::json`.
    let mut binding_to_paths: HashMap<&str, Vec<&str>> = HashMap::new();
    for m in gossamer_std::manifest::ALL_MODULES {
        let path = m.path.strip_prefix("std::").unwrap_or(m.path);
        for it in m.items {
            pairs.insert((path, it.name));
        }
        binding_to_paths.entry(path).or_default().push(path);
        if let Some(seg) = path.rsplit("::").next() {
            binding_to_paths.entry(seg).or_default().push(path);
        }
    }

    let unmatched: Vec<&str> = gossamer_interp::registered_names()
        .into_iter()
        .filter(|n| n.contains("::") && n.chars().next().is_some_and(char::is_lowercase))
        .filter(|n| !n.contains("::__gos_"))
        .filter(|n| !ALLOWED_UNMANIFESTED.contains(n))
        .filter(|n| !is_primitive_scalar_method(n))
        .filter(|n| {
            let mut segs: Vec<&str> = n.split("::").collect();
            // A leading `std` segment is just the crate root.
            if segs.first() == Some(&"std") {
                segs.remove(0);
            }
            let binding_segs = &segs[..segs.len() - 1];
            let type_name = binding_segs
                .last()
                .filter(|segment| segment.chars().next().is_some_and(char::is_uppercase))
                .copied();
            // Type-associated methods do not need a manifest entry for every
            // method: the manifest describes the type export. They must,
            // however, belong to a manifest-listed Type, so a runtime-only
            // type or an accidental owner-name typo cannot silently drift.
            let (binding_segs, member) = match type_name {
                Some(type_name) => (&binding_segs[..binding_segs.len() - 1], type_name),
                None => (binding_segs, segs[segs.len() - 1]),
            };
            let binding = binding_segs.join("::");
            let matched = binding_to_paths
                .get(binding.as_str())
                .into_iter()
                .flatten()
                .any(|p| pairs.contains(&(*p, member)));
            !matched
        })
        .collect();

    assert!(
        unmatched.is_empty(),
        "{} registered member(s) have no canonical manifest entry. Add a \
         StdItem to the right manifest/*.rs module (or, if it is a \
         deliberate deprecated alias, to ALLOWED_UNMANIFESTED):\n  {unmatched:#?}",
        unmatched.len()
    );
}

/// Manifest item exports whose implementation is reached through a
/// parse-time call rewrite (`gossamer-parse`), so the public spelling is
/// absent from the interp builtin registry yet the call resolves on every
/// tier. A closed, mechanism-annotated list: each entry is rewritten /
/// injected by a named mechanism in `gossamer-parse` (verified to build +
/// run). The resolver never sees these names - the rewrite fires before
/// resolution - so the three-segment phantom gate cannot reject them.
const MANIFEST_IMPL_VIA_REWRITE: &[&str] = &[
    // `Parser::rewrite_errors_newf` desugars to `errors::new(format!(..))`.
    "errors::newf",
    // `rewrite_stdlib_struct_surface` maps these to injected
    // `__gos_http_*` wrappers (HTTP_SECURITY_WRAPPERS).
    "http::csrf::extract_token",
    "http::csrf::origin_allowed",
    "http::csrf::check",
    "http::csrf::attach_cookie",
    "http::session::with_session",
    "http::multipart::parse",
    // `rewrite_stdlib_struct_surface` maps the public SQL facade to
    // injected `__gos_sql_*` wrappers. Those wrappers reach the same
    // interpreter, C-ABI, Cranelift, and LLVM implementations.
    "database::sql::register_native",
    "database::sql::drivers",
    "database::sql::open",
    "database::sql::migrate_up",
    // Civil-time calls are rewritten to wrappers that pass Location's compact
    // source representation into the registered raw runtime leaves.
    "time::add_date",
    "time::format_in",
    // `TIME_TIMER_WRAPPERS`: a goroutine that sleeps then sends, so the
    // deadline composes with `select` and `while let` like any channel.
    "time::after",
    // `SYNC_SHIELD_WRAPPERS` / `SYNC_TIMEOUT_WRAPPERS`: the two library
    // combinators over `cohort` + `spawn`, written as the desugar the block
    // compiles to so the callable's own value is the wrapper's.
    "sync::shield",
    "sync::with_timeout",
];

#[test]
fn manifest_functions_have_implementations() {
    use std::collections::{HashMap, HashSet};

    // Canonical path (no `std::`) -> itself, plus last-segment -> path,
    // so a registered `json::parse` and a manifest `encoding::json` both
    // reach the same canonical module. Mirrors the binding map in
    // `registry_members_match_manifest`, inverted.
    let mut binding_to_paths: HashMap<&str, Vec<&str>> = HashMap::new();
    for m in gossamer_std::manifest::ALL_MODULES {
        let path = m.path.strip_prefix("std::").unwrap_or(m.path);
        binding_to_paths.entry(path).or_default().push(path);
        if let Some(seg) = path.rsplit("::").next() {
            binding_to_paths.entry(seg).or_default().push(path);
        }
    }

    // (canonical_path, member) pairs the interp actually binds, reached
    // by reverse-mapping every registered free-function name through its
    // binding spelling. `module::Type::method` names are type-associated
    // methods, never manifest free-function members, so they are skipped.
    let mut implemented: HashSet<(&str, &str)> = HashSet::new();
    for name in gossamer_interp::registered_names() {
        if !name.contains("::") || !name.chars().next().is_some_and(char::is_lowercase) {
            continue;
        }
        let mut segs: Vec<&str> = name.split("::").collect();
        if segs.first() == Some(&"std") {
            segs.remove(0);
        }
        let member = segs[segs.len() - 1];
        let binding_segs = &segs[..segs.len() - 1];
        if binding_segs
            .last()
            .is_some_and(|s| s.chars().next().is_some_and(char::is_uppercase))
        {
            continue;
        }
        let binding = binding_segs.join("::");
        if let Some(paths) = binding_to_paths.get(binding.as_str()) {
            for p in paths {
                implemented.insert((*p, member));
            }
        }
    }

    let phantoms: Vec<String> = gossamer_std::manifest::ALL_MODULES
        .iter()
        .flat_map(|m| {
            let path = m.path.strip_prefix("std::").unwrap_or(m.path);
            m.items
                .iter()
                .filter(|it| it.kind == gossamer_std::registry::StdItemKind::Function)
                .map(move |it| (path, it.name))
        })
        .filter(|(path, name)| !implemented.contains(&(*path, *name)))
        .map(|(path, name)| format!("{path}::{name}"))
        .filter(|p| !MANIFEST_IMPL_VIA_REWRITE.contains(&p.as_str()))
        .collect();

    assert!(
        phantoms.is_empty(),
        "{n} manifest Function item(s) advertise a function that resolves to NO \
         implementation - they are listed by `gos doc`, pass `gos check` (for \
         3-segment paths), then fail at runtime with GX0002 / a compiled build \
         error.\nEither wire the function on all three tiers (interp builtin + \
         c_abi shim + cranelift + llvm dispatch) and add a tier-parity fixture, \
         or remove the StdItem from its manifest/*.rs module:\n  {phantoms:#?}",
        n = phantoms.len()
    );

    // Guard the allowlist against rot: every rewrite-backed entry must
    // still be an advertised manifest Function, else it is dead weight
    // masking a future regression.
    let manifest_fns: std::collections::HashSet<String> = gossamer_std::manifest::ALL_MODULES
        .iter()
        .flat_map(|m| {
            let path = m.path.strip_prefix("std::").unwrap_or(m.path);
            m.items
                .iter()
                .filter(|it| it.kind == gossamer_std::registry::StdItemKind::Function)
                .map(move |it| format!("{path}::{}", it.name))
        })
        .collect();
    let stale: Vec<&&str> = MANIFEST_IMPL_VIA_REWRITE
        .iter()
        .filter(|p| !manifest_fns.contains(**p))
        .collect();
    assert!(
        stale.is_empty(),
        "MANIFEST_IMPL_VIA_REWRITE entries no longer advertised as manifest \
         Functions (remove them): {stale:#?}"
    );
}

/// Manifest modules whose surface is real but invisible to the resolver
/// export table, with the mechanism that reaches it.
///
/// A closed, mechanism-annotated list, not a dumping ground. Anything
/// else that advertises items nothing can reach is a
/// documented-and-absent module - the shape that let `lifecycle`,
/// `http::state`, `http::health`, and the session types be listed by
/// `gos doc`, accepted by `gos check`, and then report `GX0002 ... is not
/// bound in this scope` at run time.
const MODULES_REACHED_WITHOUT_A_RESOLVER_EXPORT: &[(&str, &str)] = &[
    (
        "http::form",
        "parse-time injected Gossamer wrappers (`__gos_http_form_*` in \
         gossamer-parse/src/autoderive/stdlib_wrappers.rs); the rewrite \
         fires before resolution, so the resolver never sees the names",
    ),
    (
        "http::proxy",
        "handler-signature shapes; `forward` is the callable and is \
         gated as a Function",
    ),
];

/// Every manifest module that advertises items must expose at least one
/// item Gossamer code can reach: a `Function` (which the gate above
/// proves has an implementation) or a `Type` with an exported associated
/// item. A module with items and no reachable surface is documentation
/// with nothing behind it.
///
/// A module with no items at all passes: it is a pointer to the idiom
/// that replaced it, and `gos doc` shows its summary and an empty list,
/// which is the truth.
#[test]
fn manifest_modules_expose_a_reachable_item() {
    use std::collections::HashSet;

    let exported: HashSet<&str> = gossamer_resolve::STDLIB_QUALIFIED.iter().copied().collect();
    let by_other_mechanism: HashSet<&str> = MODULES_REACHED_WITHOUT_A_RESOLVER_EXPORT
        .iter()
        .map(|(path, _)| *path)
        .collect();

    let hollow: Vec<&str> = gossamer_std::manifest::ALL_MODULES
        .iter()
        .filter(|m| !m.items.is_empty())
        .filter(|m| {
            let path = m.path.strip_prefix("std::").unwrap_or(m.path);
            if by_other_mechanism.contains(path) {
                return false;
            }
            let leaf = path.rsplit("::").next().unwrap_or(path);
            let reachable = m.items.iter().any(|it| match it.kind {
                gossamer_std::registry::StdItemKind::Function
                | gossamer_std::registry::StdItemKind::Builtin
                | gossamer_std::registry::StdItemKind::Const => true,
                _ => {
                    let full = format!("{path}::{}::", it.name);
                    let short = format!("{leaf}::{}::", it.name);
                    exported
                        .iter()
                        .any(|e| e.starts_with(&full) || e.starts_with(&short))
                }
            });
            !reachable
        })
        .map(|m| m.path)
        .collect();

    assert!(
        hollow.is_empty(),
        "{n} manifest module(s) advertise items and expose nothing \
         Gossamer code can reach.\nEither wire an item on all three tiers \
         (interp builtin + c_abi shim + cranelift + llvm dispatch) with a \
         tier-parity fixture, empty the module's item list and point its \
         summary at the idiom that replaced it, or add the module to \
         MODULES_REACHED_WITHOUT_A_RESOLVER_EXPORT with the mechanism \
         that reaches it:\n  {hollow:#?}",
        n = hollow.len()
    );

    // Guard the allowlist against rot.
    let advertised: HashSet<&str> = gossamer_std::manifest::ALL_MODULES
        .iter()
        .filter(|m| !m.items.is_empty())
        .map(|m| m.path.strip_prefix("std::").unwrap_or(m.path))
        .collect();
    let stale: Vec<&&str> = by_other_mechanism
        .iter()
        .filter(|p| !advertised.contains(**p))
        .collect();
    assert!(
        stale.is_empty(),
        "MODULES_REACHED_WITHOUT_A_RESOLVER_EXPORT entries no longer \
         advertise any manifest items (remove them): {stale:#?}"
    );
}

#[test]
fn manifest_functions_have_checker_signatures() {
    use std::collections::HashSet;

    let manifest_fns: HashSet<(&str, &str)> = gossamer_std::manifest::ALL_MODULES
        .iter()
        .flat_map(|module| {
            module
                .items
                .iter()
                .filter(|item| item.kind == gossamer_std::registry::StdItemKind::Function)
                .map(move |item| (module.path, item.name))
        })
        .collect();

    let missing: Vec<String> = manifest_fns
        .iter()
        .filter(|(module_path, name)| {
            gossamer_types::stdlib_function_signature(module_path, name).is_none()
        })
        .map(|(module_path, name)| format!("{module_path}::{name}"))
        .collect();
    assert!(
        missing.is_empty(),
        "{} manifest Function item(s) have no checker-exposed signature:\n{missing:#?}",
        missing.len()
    );

    let mut seen = HashSet::new();
    let stale_or_duplicate: Vec<String> = gossamer_types::STD_FUNCTION_SIGNATURES
        .iter()
        .filter_map(|sig| {
            let key = (sig.module_path, sig.name);
            if !seen.insert(key) {
                return Some(format!("duplicate {}::{}", sig.module_path, sig.name));
            }
            (!manifest_fns.contains(&key))
                .then(|| format!("stale {}::{}", sig.module_path, sig.name))
        })
        .collect();
    assert!(
        stale_or_duplicate.is_empty(),
        "checker stdlib signature table drifted from the manifest:\n{stale_or_duplicate:#?}"
    );
}

#[test]
fn stdlib_module_paths_match_manifest() {
    let mut live: Vec<&str> = gossamer_std::manifest::ALL_MODULES
        .iter()
        .map(|m| m.path.strip_prefix("std::").unwrap_or(m.path))
        .collect();
    live.sort_unstable();
    live.dedup();

    let table = gossamer_resolve::STDLIB_MODULE_PATHS;
    assert!(
        table.windows(2).all(|w| w[0] < w[1]),
        "STDLIB_MODULE_PATHS must be sorted for binary search"
    );
    let missing: Vec<&str> = live
        .iter()
        .filter(|n| !table.contains(n))
        .copied()
        .collect();
    let extra: Vec<&str> = table
        .iter()
        .filter(|n| !live.contains(n))
        .copied()
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "module path table drifted from the std manifest.\n  \
         missing from table: {missing:?}\n  extra in table: {extra:?}"
    );
}

/// The checker accepts an `impl` header naming a stdlib trait, and rejects
/// one naming anything undeclared (GT0070). Its allowlist is a static table,
/// so a trait added to or removed from the manifest must move it too - a new
/// stdlib trait would otherwise be rejected in every program that implements
/// it.
#[test]
fn checker_trait_allowlist_matches_the_std_manifest() {
    let mut live: Vec<&str> = gossamer_std::registry::modules()
        .iter()
        .flat_map(|m| m.items.iter())
        .filter(|item| matches!(item.kind, gossamer_std::registry::StdItemKind::Trait))
        .map(|item| item.name)
        .collect();
    live.sort_unstable();
    live.dedup();

    let table = gossamer_types::STDLIB_TRAIT_NAMES;
    assert!(
        table.windows(2).all(|w| w[0] < w[1]),
        "STDLIB_TRAIT_NAMES must be sorted"
    );
    let missing: Vec<&str> = live
        .iter()
        .filter(|n| !table.contains(n))
        .copied()
        .collect();
    let extra: Vec<&str> = table
        .iter()
        .filter(|n| !live.contains(n))
        .copied()
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "trait allowlist drifted from the std manifest.\n  \
         missing from table: {missing:?}\n  extra in table: {extra:?}"
    );
}

/// A built-in trait the catalog files under a module must be the one the
/// manifest declares there. `%info Display` answers from the manifest entry
/// and takes its contract from the catalog, so a module path that drifts
/// would leave the trait listed with no method an `impl` supplies.
#[test]
fn cataloged_trait_modules_match_the_std_manifest() {
    let mismatched: Vec<String> = gossamer_types::BUILTIN_TRAITS
        .iter()
        .filter_map(|entry| {
            let module = entry.module?;
            let declared = gossamer_std::registry::modules().iter().any(|m| {
                m.path == module
                    && m.items.iter().any(|item| {
                        item.name == entry.name
                            && matches!(item.kind, gossamer_std::registry::StdItemKind::Trait)
                    })
            });
            (!declared).then(|| format!("{module}::{}", entry.name))
        })
        .collect();
    assert!(
        mismatched.is_empty(),
        "cataloged traits name a module that does not declare them: {mismatched:?}"
    );
}

/// Every trait the manifest declares and the catalog also knows agrees on
/// which module owns it, so a bare-name query and a qualified one answer the
/// same entry rather than two.
#[test]
fn a_manifest_trait_the_catalog_knows_is_never_listed_bare() {
    let bare: Vec<&str> = gossamer_std::registry::modules()
        .iter()
        .flat_map(|m| m.items.iter())
        .filter(|item| matches!(item.kind, gossamer_std::registry::StdItemKind::Trait))
        .filter(|item| {
            gossamer_types::builtin_trait(item.name).is_some_and(|entry| entry.module.is_none())
        })
        .map(|item| item.name)
        .collect();
    assert!(
        bare.is_empty(),
        "a manifest trait must carry its module in the catalog: {bare:?}"
    );
}
