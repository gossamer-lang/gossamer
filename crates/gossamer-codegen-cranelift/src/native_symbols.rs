//! The wire symbols of `[rust-bindings]` items the JIT calls.
//!
//! `gos run` loads a binding library on demand, so its symbols are not in
//! the process's own symbol table: the loader records their names here, and
//! the JIT resolves each one through the loader's resolver when it finalizes
//! a body calling it.

use parking_lot::Mutex;

/// Resolves a binding's wire symbol the JIT names, loading its library when
/// a body that calls it is first compiled.
pub type BindingSymbolResolver = fn(&str) -> Option<*const u8>;

static LAZY_BINDINGS: Mutex<(Vec<&'static str>, Option<BindingSymbolResolver>)> =
    Mutex::new((Vec::new(), None));

/// Records the wire symbols of bindings whose libraries load on demand, and
/// the function that resolves them.
pub fn register_lazy_binding_symbols(names: Vec<&'static str>, resolver: BindingSymbolResolver) {
    let mut guard = LAZY_BINDINGS.lock();
    for name in names {
        if !guard.0.contains(&name) {
            guard.0.push(name);
        }
    }
    guard.1 = Some(resolver);
}

/// The wire symbols [`register_lazy_binding_symbols`] recorded.
#[must_use]
pub fn lazy_binding_symbol_names() -> Vec<&'static str> {
    LAZY_BINDINGS.lock().0.clone()
}

/// The address of a lazily loaded binding's wire symbol `name`.
#[must_use]
pub fn resolve_lazy_binding_symbol(name: &str) -> Option<*const u8> {
    let resolver = LAZY_BINDINGS.lock().1?;
    resolver(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn probe() {}

    fn resolve(name: &str) -> Option<*const u8> {
        (name == "gos_binding_test__probe").then_some(probe as *const u8)
    }

    #[test]
    fn a_recorded_name_resolves_through_the_resolver() {
        register_lazy_binding_symbols(vec!["gos_binding_test__probe"], resolve);
        register_lazy_binding_symbols(vec!["gos_binding_test__probe"], resolve);
        let names = lazy_binding_symbol_names();
        assert_eq!(
            names
                .iter()
                .filter(|n| **n == "gos_binding_test__probe")
                .count(),
            1
        );
        assert_eq!(
            resolve_lazy_binding_symbol("gos_binding_test__probe"),
            Some(probe as *const u8)
        );
        assert_eq!(resolve_lazy_binding_symbol("gos_binding_test__other"), None);
    }
}
