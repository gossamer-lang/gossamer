//! Conservative effect metadata for VM builtin dispatch.
//!
//! The table is intentionally usable before every builtin has a hand-written
//! row: unknown names are treated as potentially allocating and blocking, so a
//! newly registered builtin cannot accidentally receive an optimisation or
//! scheduler exemption reserved for known-pure operations.

/// Bitset of observable builtin effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuiltinEffects(u8);

impl BuiltinEffects {
    /// No observable side effects and no allocation.
    pub const PURE: Self = Self(1 << 0);
    /// May allocate Gossamer or host heap storage.
    pub const ALLOCATING: Self = Self(1 << 1);
    /// May wait on filesystem, process, DNS, socket, database, or terminal I/O.
    pub const BLOCKING: Self = Self(1 << 2);
    /// Cooperates with goroutine scheduling or may park/unpark a goroutine.
    pub const SCHEDULER_AWARE: Self = Self(1 << 3);
    /// Depends on time, randomness, environment, process state, or external I/O.
    pub const NONDETERMINISTIC: Self = Self(1 << 4);
    /// No precise declaration exists yet. This is conservatively blocking.
    pub const UNKNOWN: Self = Self(1 << 5);

    /// Returns whether `self` includes `effect`.
    #[must_use]
    pub const fn contains(self, effect: Self) -> bool {
        self.0 & effect.0 != 0
    }

    const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Returns conservative effects for a builtin's canonical or compatibility name.
///
/// Names not covered by an explicit family return `ALLOCATING | BLOCKING |
/// UNKNOWN`. Callers that need a fast scheduling decision should use
/// [`may_block`].
#[must_use]
pub fn builtin_effects(name: &str) -> BuiltinEffects {
    if matches!(
        name,
        "abs"
            | "ceil"
            | "cos"
            | "exp"
            | "floor"
            | "ln"
            | "log"
            | "math::abs"
            | "math::ceil"
            | "math::cos"
            | "math::exp"
            | "math::floor"
            | "math::ln"
            | "math::log"
            | "math::pow"
            | "math::sin"
            | "math::sqrt"
            | "pow"
            | "sin"
            | "sqrt"
    ) {
        return BuiltinEffects::PURE;
    }

    let mut effects = BuiltinEffects(0);
    if has_prefix(
        name,
        &[
            "fs::",
            "bufio::",
            "database::",
            "http::",
            "net::",
            "os::",
            "process::",
            "tls::",
        ],
    ) {
        effects = effects
            .union(BuiltinEffects::BLOCKING)
            .union(BuiltinEffects::NONDETERMINISTIC);
    }
    if has_prefix(name, &["channel::", "sync::", "thread::"])
        || matches!(name, "spawn" | "select" | "sleep" | "time::sleep")
    {
        effects = effects.union(BuiltinEffects::SCHEDULER_AWARE);
    }
    if has_prefix(name, &["rand::", "time::", "uuid::", "env::"])
        || matches!(name, "panic" | "process::id")
    {
        effects = effects.union(BuiltinEffects::NONDETERMINISTIC);
    }
    if has_prefix(
        name,
        &[
            "collections::",
            "format",
            "json::",
            "regex::",
            "strings::",
            "vec::",
        ],
    ) || effects.0 != 0
    {
        effects = effects.union(BuiltinEffects::ALLOCATING);
    }
    if effects.0 == 0 {
        BuiltinEffects::ALLOCATING
            .union(BuiltinEffects::BLOCKING)
            .union(BuiltinEffects::UNKNOWN)
    } else {
        effects
    }
}

/// Returns whether a builtin must be scheduled as potentially blocking.
#[must_use]
pub fn may_block(name: &str) -> bool {
    let effects = builtin_effects(name);
    effects.contains(BuiltinEffects::BLOCKING) || effects.contains(BuiltinEffects::UNKNOWN)
}

fn has_prefix(name: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| name.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_math_has_no_scheduler_or_allocation_effect() {
        let effects = builtin_effects("math::sqrt");
        assert_eq!(effects, BuiltinEffects::PURE);
    }

    #[test]
    fn filesystem_and_dns_are_blocking_and_nondeterministic() {
        for name in ["fs::read", "net::resolve"] {
            let effects = builtin_effects(name);
            assert!(effects.contains(BuiltinEffects::BLOCKING), "{name}");
            assert!(effects.contains(BuiltinEffects::NONDETERMINISTIC), "{name}");
        }
    }

    #[test]
    fn unknown_builtin_cannot_be_optimised_as_pure() {
        let effects = builtin_effects("future::unclassified");
        assert!(effects.contains(BuiltinEffects::UNKNOWN));
        assert!(may_block("future::unclassified"));
    }
}
