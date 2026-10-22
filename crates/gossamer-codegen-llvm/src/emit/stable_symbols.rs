use std::collections::{HashMap, HashSet};

use gossamer_mir::Body;
use gossamer_types::TyCtxt;

/// Symbols spelled the same in every program that compiles the same code.
///
/// Inside one program a specialisation is named by interning ids
/// (`fn#412__mono__t77`) and a compiler-made helper by a definition number;
/// both depend on everything else the program holds, so a library's module
/// would render differently in each program that uses it and never share an
/// object. This maps each such symbol to one written by the names of the
/// template and types it was made from. A mapping that would collide with
/// another symbol is dropped, which keeps the renaming one-to-one.
pub(super) struct StableSymbols {
    renamed: HashMap<String, String>,
}

impl StableSymbols {
    /// The renaming for the bodies of one program, whose symbols are the
    /// mangled body names.
    pub(super) fn for_program(bodies: &[Body], tcx: &TyCtxt) -> Self {
        let symbols: Vec<String> = bodies
            .iter()
            .map(|body| crate::lower::mangle_fn_name(&body.name).into_owned())
            .collect();
        let existing: HashSet<&str> = symbols.iter().map(String::as_str).collect();
        let mut proposed: Vec<(String, String)> = Vec::new();
        for (body, symbol) in bodies.iter().zip(&symbols) {
            if *symbol != body.name {
                continue;
            }
            if let Some(stable) = stable_name(&body.name, tcx)
                && stable != body.name
            {
                proposed.push((symbol.clone(), stable));
            }
        }
        let mut uses: HashMap<&str, usize> = HashMap::new();
        for (_, stable) in &proposed {
            *uses.entry(stable.as_str()).or_default() += 1;
        }
        let renamed = proposed
            .iter()
            .filter(|(_, stable)| uses[stable.as_str()] == 1 && !existing.contains(stable.as_str()))
            .cloned()
            .collect();
        Self { renamed }
    }

    /// The symbol `symbol` is emitted under, including a thunk named after
    /// it (`name$cabi`).
    fn emitted(&self, symbol: &str) -> Option<std::borrow::Cow<'_, str>> {
        if let Some(stable) = self.renamed.get(symbol) {
            return Some(std::borrow::Cow::Borrowed(stable));
        }
        let (stem, suffix) = symbol.rsplit_once('$')?;
        let stable = self.renamed.get(stem)?;
        Some(std::borrow::Cow::Owned(format!("{stable}${suffix}")))
    }

    /// The name `symbol` is emitted under: its stable name, or itself.
    pub(super) fn emitted_name<'a>(&'a self, symbol: &'a str) -> std::borrow::Cow<'a, str> {
        self.emitted(symbol)
            .unwrap_or(std::borrow::Cow::Borrowed(symbol))
    }

    /// `ir` with every quoted global the renaming covers written under its
    /// stable name.
    pub(super) fn apply(&self, ir: &str) -> String {
        if self.renamed.is_empty() {
            return ir.to_string();
        }
        rewrite_globals(ir, |symbol| self.emitted(symbol))
    }
}

/// `ir` with each quoted global (`@"name"`) `rename` answers for written
/// under the answer, in one pass over the text. String constants are not
/// globals and are left as they are.
pub(super) fn rewrite_globals<'a>(
    ir: &str,
    rename: impl Fn(&str) -> Option<std::borrow::Cow<'a, str>>,
) -> String {
    let mut out = String::with_capacity(ir.len());
    let mut rest = ir;
    while let Some(at) = rest.find("@\"") {
        let after = &rest[at + 2..];
        let Some(close) = after.find('"') else {
            break;
        };
        let symbol = &after[..close];
        out.push_str(&rest[..at + 2]);
        match rename(symbol) {
            Some(stable) => out.push_str(&stable),
            None => out.push_str(symbol),
        }
        rest = &after[close..];
    }
    out.push_str(rest);
    out
}

/// The program-independent name of the body `name`, when it carries an
/// interning or definition number another program would number differently.
fn stable_name(name: &str, tcx: &TyCtxt) -> Option<String> {
    if let Some((base, args)) = name.rsplit_once("$mono$") {
        let base = if is_numbered(base) {
            stable_name(base, tcx)?
        } else {
            base.to_string()
        };
        return Some(format!("{base}<{}>", type_arguments(args, tcx)?));
    }
    if let Some((def, args)) = name
        .strip_prefix("fn#")
        .and_then(|rest| rest.split_once("__mono__"))
    {
        let template = tcx.generic_fn_name(def.parse().ok()?)?;
        let template = if is_numbered(template) {
            stable_name(template, tcx)?
        } else {
            template.to_string()
        };
        return Some(format!("{template}<{}>", type_arguments(args, tcx)?));
    }
    if let Some((kind, def)) = serde_helper(name) {
        let ty = tcx.def_name(gossamer_resolve::DefId::local(def))?;
        return Some(format!("__gos_serde_{kind}.{ty}"));
    }
    None
}

/// Whether `name` is spelled with numbers [`stable_name`] replaces.
fn is_numbered(name: &str) -> bool {
    name.contains("$mono$") || name.starts_with("fn#") || serde_helper(name).is_some()
}

/// The kind and type definition of a generated serde helper
/// (`__gos_serde_dec_412` is `("dec", 412)`).
fn serde_helper(name: &str) -> Option<(&str, u32)> {
    let (kind, def) = name.strip_prefix("__gos_serde_")?.split_once('_')?;
    if kind.is_empty() || !kind.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return None;
    }
    Some((kind, def.parse().ok()?))
}

/// The specialisation arguments a mangled name spells (`t77_c4`), written by
/// the names of their types; `None` when one has no such name.
fn type_arguments(args: &str, tcx: &TyCtxt) -> Option<String> {
    let mut out = Vec::new();
    for arg in args.split('_') {
        let (kind, value) = arg.split_at_checked(1)?;
        match kind {
            "t" => {
                let ty = tcx.ty_at_index(value.parse().ok()?)?;
                out.push(gossamer_types::stable_type_name(tcx, ty)?);
            }
            "c" => out.push(value.to_string()),
            _ => return None,
        }
    }
    Some(out.join(", "))
}

/// The name a chunk-`private` table is emitted under, taken from the
/// contents it holds, so two programs holding the same table spell it alike.
/// Tables with the same contents share a name, and the chunk defines it once.
pub(super) fn content_name(symbol: &str, blob: &[i64]) -> String {
    let kind = symbol.trim_end_matches(|c: char| c.is_ascii_digit() || c == '_');
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for value in blob {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{kind}.{hash:016x}.{}", blob.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_name_depends_on_the_table_not_its_number() {
        assert_eq!(
            content_name("gos_rc_meta_boxaggr_12", &[1, 2, 3]),
            content_name("gos_rc_meta_boxaggr_977", &[1, 2, 3])
        );
        assert_ne!(
            content_name("gos_rc_meta_boxaggr_12", &[1, 2, 3]),
            content_name("gos_rc_meta_boxaggr_12", &[1, 2, 4])
        );
        assert_ne!(
            content_name("gos_rc_meta_boxaggr_12", &[1]),
            content_name("gos_rc_slotchildren_12", &[1])
        );
    }

    #[test]
    fn apply_rewrites_quoted_globals_and_thunks_only() {
        let symbols = StableSymbols {
            renamed: [("fn#4__mono__t9".to_string(), "m::id<i64>".to_string())].into(),
        };
        let ir = "call i64 @\"fn#4__mono__t9\"(ptr @\"gos_rc_meta_x_3\")\n\
                  declare <16 x i8> @\"fn#4__mono__t9$cabi\"()\n\
                  @s = private constant [15 x i8] c\"fn#4__mono__t9\\00\"\n";
        let out = symbols.apply(ir);
        assert!(out.contains("call i64 @\"m::id<i64>\"(ptr @\"gos_rc_meta_x_3\")"));
        assert!(out.contains("@\"m::id<i64>$cabi\""));
        assert!(out.contains("c\"fn#4__mono__t9\\00\""));
    }

    #[test]
    fn serde_helpers_of_every_kind_are_recognised() {
        assert_eq!(serde_helper("__gos_serde_ren_17"), Some(("ren", 17)));
        assert_eq!(serde_helper("__gos_serde_dec_4"), Some(("dec", 4)));
        assert_eq!(serde_helper("__gos_serde_rd_array"), None);
        assert_eq!(serde_helper("__gos_serde_dec_x"), None);
    }
}
