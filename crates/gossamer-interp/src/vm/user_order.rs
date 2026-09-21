//! The `cmp` a key type declares, as an ordered container reaches it on the
//! bytecode tier.
//!
//! A `BTreeMap` or `BTreeSet` whose key type writes its own `cmp` orders by
//! that body, the way the compiled tiers call the comparator the program
//! compiled for the key type. The container holds the comparator's name; the
//! VM running it is reachable here for the length of a run.

use std::cell::Cell;
use std::cmp::Ordering;

use gossamer_ast::USER_COMPARATOR_PREFIX;

use super::Vm;
use crate::value::{AggShape, MapKey};

thread_local! {
    /// The VM a comparator call runs in. Null outside a run.
    static HOST: Cell<*const Vm> = const { Cell::new(std::ptr::null()) };
}

impl Vm {
    /// Runs `body` with this VM reachable as the host an ordered container
    /// calls a key type's own `cmp` through. Restores the previous host, so a
    /// nested run - a goroutine, a comptime fold - leaves the outer one
    /// standing.
    pub(crate) fn as_comparator_host<R>(&self, body: impl FnOnce() -> R) -> R {
        let previous = HOST.with(|slot| slot.replace(std::ptr::from_ref(self)));
        let out = body();
        HOST.with(|slot| slot.set(previous));
        out
    }

    /// The comparator ordering keys of the type `name` tags, `None` when the
    /// program declares none. Answered once per key type.
    fn user_comparator(&self, name: &str, tag: u64) -> Option<&'static str> {
        if let Some(found) = self.user_comparators.borrow().get(&tag) {
            return *found;
        }
        // A type's comparator is named for the symbol its module path spells,
        // `module__Name`, whose last segment is the name a value carries at
        // run time.
        let leaf = name.rsplit("::").next().unwrap_or(name);
        let found = self
            .globals
            .keys()
            .filter(|global| global.starts_with(USER_COMPARATOR_PREFIX))
            .find(|global| global[USER_COMPARATOR_PREFIX.len()..].rsplit("__").next() == Some(leaf))
            .copied();
        self.user_comparators.borrow_mut().insert(tag, found);
        found
    }
}

/// Runs `body` on the host VM, or answers `None` outside a run.
fn with_host<R>(body: impl FnOnce(&Vm) -> R) -> Option<R> {
    let vm = HOST.with(Cell::get);
    if vm.is_null() {
        return None;
    }
    // SAFETY: the slot holds a borrow of a VM live for the whole of
    // `as_comparator_host`, which is the only window it is set in.
    Some(body(unsafe { &*vm }))
}

/// The comparator ordering `key`'s type, when that type declares one.
pub(crate) fn comparator_for(key: &MapKey) -> Option<&'static str> {
    let MapKey::Agg(agg) = key else {
        return None;
    };
    // An enum value carries its variant's name, so the type that declares the
    // variant is what names the comparator.
    let name = match agg.shape {
        AggShape::Struct(_) => Some(agg.name.as_str()),
        AggShape::Variant => crate::builtins::variant_owner_of(agg.name.as_str()),
        AggShape::Tuple | AggShape::Array => None,
    }?;
    with_host(|vm| vm.user_comparator(name, agg.name.id())).flatten()
}

/// Orders two keys through `comparator`, or answers `None` when no VM hosts
/// the call or the comparator raised.
pub(crate) fn order(comparator: &'static str, a: &MapKey, b: &MapKey) -> Option<Ordering> {
    with_host(|vm| {
        // The comparator runs inside the frame that reached this comparison,
        // so the chain above it stays intact for a fault raised in its body.
        let callee = vm.lookup_global(comparator)?;
        let verdict = vm.apply(callee, vec![a.to_value(), b.to_value()]).ok()?;
        match verdict {
            crate::value::Value::Int(n) => Some(n.cmp(&0)),
            _ => None,
        }
    })
    .flatten()
}
