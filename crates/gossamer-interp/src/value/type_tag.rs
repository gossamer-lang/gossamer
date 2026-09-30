//! Interned type tags, field names, and small variant values.

use super::{SmolStr, Value, VariantInner};

use std::fmt;
use std::sync::{Arc, OnceLock};

use smallvec::SmallVec;

/// Compact integer identity for struct and enum-variant names.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeTag(Arc<str>);

pub(super) static TYPE_TAGS: std::sync::LazyLock<
    parking_lot::Mutex<rustc_hash::FxHashMap<String, std::sync::Weak<str>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()));

impl TypeTag {
    /// Returns the interned textual name for this tag.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the compact numeric identity stored in aggregate nodes.
    #[must_use]
    pub fn id(&self) -> u64 {
        Arc::as_ptr(&self.0).cast::<()>() as usize as u64
    }
}

impl fmt::Debug for TypeTag {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), out)
    }
}

impl fmt::Display for TypeTag {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.as_str())
    }
}

impl AsRef<str> for TypeTag {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for TypeTag {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for TypeTag {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// Returns a `&'static str` identity for `name`, allocating once
/// per distinct byte sequence. Used by [`Value::variant`],
/// [`Value::struct_`], and [`Value::float_array`] to deduplicate
/// type names across all values that share them - programs
/// typically have a fixed, small set of named types.
///
/// The leak is bounded by that set, not by call count.
#[must_use]
pub(crate) fn intern_type_name(name: &str) -> &'static str {
    static INTERNED: OnceLock<parking_lot::Mutex<rustc_hash::FxHashSet<&'static str>>> =
        OnceLock::new();
    let set = INTERNED.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashSet::default()));
    let mut guard = set.lock();
    if let Some(&s) = guard.get(name) {
        return s;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// A chunk's inline-site table under one process-wide identity, shared the way
/// its frame names are, so a call-stack frame can hold it by reference. Equal
/// tables from recompiling the same function share one entry.
pub(crate) fn intern_inline_sites(
    sites: &[crate::bytecode::InlineSite],
) -> &'static [crate::bytecode::InlineSite] {
    type Table = rustc_hash::FxHashSet<&'static [crate::bytecode::InlineSite]>;
    static INTERNED: OnceLock<parking_lot::Mutex<Table>> = OnceLock::new();
    if sites.is_empty() {
        return &[];
    }
    let set = INTERNED.get_or_init(|| parking_lot::Mutex::new(Table::default()));
    let mut guard = set.lock();
    if let Some(&interned) = guard.get(sites) {
        return interned;
    }
    let leaked: &'static [crate::bytecode::InlineSite] =
        Box::leak(sites.to_vec().into_boxed_slice());
    guard.insert(leaked);
    leaked
}

/// Smallest table size that triggers a sweep for dead entries, and the floor
/// the watermark returns to.
pub(super) const TYPE_TAG_SWEEP_MIN: usize = 64;
/// Table size at which the next sweep runs. Held under the `TYPE_TAGS` lock.
static TYPE_TAG_SWEEP_AT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(TYPE_TAG_SWEEP_MIN);

/// Resolves the wrapper names every optional- or fallible-returning builtin
/// constructs, without reaching the shared table. Each slot holds a strong
/// share of the same allocation the table hands out, so tag identity - which
/// aggregate dispatch compares by pointer - is unchanged.
fn cached_prelude_tag(name: &str) -> Option<TypeTag> {
    static SOME: OnceLock<TypeTag> = OnceLock::new();
    static NONE: OnceLock<TypeTag> = OnceLock::new();
    static OK: OnceLock<TypeTag> = OnceLock::new();
    static ERR: OnceLock<TypeTag> = OnceLock::new();
    let slot = match name {
        "Some" => &SOME,
        "None" => &NONE,
        "Ok" => &OK,
        "Err" => &ERR,
        _ => return None,
    };
    Some(slot.get_or_init(|| intern_type_tag_uncached(name)).clone())
}

/// Returns a compact identity for a type / variant name, allocating the name
/// text at most once through [`intern_type_name`].
#[must_use]
pub(crate) fn intern_type_tag(name: &str) -> TypeTag {
    if let Some(tag) = cached_prelude_tag(name) {
        return tag;
    }
    intern_type_tag_uncached(name)
}

fn intern_type_tag_uncached(name: &str) -> TypeTag {
    use std::sync::atomic::Ordering;
    let mut tags = TYPE_TAGS.lock();
    if let Some(existing) = tags.get(name).and_then(std::sync::Weak::upgrade) {
        return TypeTag(existing);
    }
    // A tag whose last share dropped leaves a dead entry behind. Reclaiming
    // the whole table on every intern costs one atomic load per distinct name
    // per construction, so the sweep runs only once the table has grown past
    // its size at the previous sweep. Re-interning a dead name reclaims that
    // entry directly through the insert below.
    if tags.len() >= TYPE_TAG_SWEEP_AT.load(Ordering::Relaxed) {
        tags.retain(|_, weak| weak.strong_count() != 0);
        TYPE_TAG_SWEEP_AT.store(
            tags.len().saturating_mul(2).max(TYPE_TAG_SWEEP_MIN),
            Ordering::Relaxed,
        );
    }
    let owned: Arc<str> = Arc::from(name);
    tags.insert(name.to_owned(), Arc::downgrade(&owned));
    TypeTag(owned)
}

#[must_use]
pub(super) fn type_tag_from_static(name: &'static str) -> TypeTag {
    intern_type_tag(name)
}

/// Closed integer range eligible for the small-variant cache, mirroring
/// the `CPython` small-int cache. Bounds the cache to
/// `names x (SMALL_INT_MAX - SMALL_INT_MIN + 1)` entries per thread.
const SMALL_VARIANT_INT_MIN: i64 = -128;
const SMALL_VARIANT_INT_MAX: i64 = 1024;
/// Max byte length of a single `String`-payload variant eligible for
/// interning. Bounded to the inline `SmolStr` range so the cache key never
/// holds a heap allocation, and so an unbounded space of distinct long
/// strings cannot grow the table. Covers the common case of a small set of
/// repeated string-payload variants (e.g. enum-like tags such as
/// `Str("alpha")` duplicated across many records).
const SMALL_VARIANT_STR_MAX: usize = 16;

/// Cache key for an interned single-small-scalar (or nullary) variant
/// node. `name` is an interned `&'static str`, unique per distinct
/// content, so it identifies the variant exactly.
#[derive(PartialEq, Eq, Hash)]
enum SmallVariantKey {
    Unit(TypeTag),
    Int(TypeTag, i64),
    Bool(TypeTag, bool),
    /// A single short `String` payload. The `SmolStr` is inline (≤ the
    /// `SMALL_VARIANT_STR_MAX` bound) so the key holds no heap allocation.
    Str(TypeTag, SmolStr),
}

thread_local! {
    /// Per-thread interning table for small immutable variant nodes
    /// (lever 3). Thread-local to avoid the cross-thread lock contention
    /// a shared global table would impose on per-connection VM threads.
    ///
    /// Holds a `Weak` rather than a strong reference so the cache never
    /// keeps a node alive on its own: identical small variants that are
    /// concurrently live share one allocation (every leaf of a tree), but
    /// once the last user reference drops, the node is freed and its
    /// liveness is observable through `downgrade()`/`upgrade()` exactly as
    /// for a non-interned node - preserving weak-reference tier parity.
    static SMALL_VARIANT_CACHE: std::cell::RefCell<
        rustc_hash::FxHashMap<SmallVariantKey, std::sync::Weak<VariantInner>>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Returns the cache key if this `(name, fields)` pair is eligible for
/// small-variant interning: nullary, or a single `Int` in the cached
/// range, or a single `Bool`. Everything else (multi-field nodes,
/// large ints, aggregate payloads) returns `None` and allocates fresh.
fn small_variant_key(name: &TypeTag, fields: &[Value]) -> Option<SmallVariantKey> {
    match fields {
        [] => Some(SmallVariantKey::Unit(name.clone())),
        [Value::Int(n)] if (SMALL_VARIANT_INT_MIN..=SMALL_VARIANT_INT_MAX).contains(n) => {
            Some(SmallVariantKey::Int(name.clone(), *n))
        }
        [Value::Bool(b)] => Some(SmallVariantKey::Bool(name.clone(), *b)),
        // A single short string payload: immutable, so sharing one node
        // across all identical occurrences is sound exactly as for scalars.
        [Value::String(s)] if s.byte_len() <= SMALL_VARIANT_STR_MAX => {
            Some(SmallVariantKey::Str(name.clone(), s.clone()))
        }
        _ => None,
    }
}

/// Returns a shared `Arc<VariantInner>` for an interning-eligible node:
/// reuses the cached node when a live one exists, otherwise allocates a
/// fresh one and records a `Weak` to it. The node is immutable, so all
/// aliases observe identical structure; the cache holding only a `Weak`
/// keeps liveness (and thus `Weak::upgrade`) faithful to the user's
/// references.
fn intern_small_variant(
    name: TypeTag,
    fields: SmallVec<[Value; 2]>,
    key: SmallVariantKey,
) -> Arc<VariantInner> {
    SMALL_VARIANT_CACHE.with(|cache| {
        if let Some(existing) = cache.borrow().get(&key).and_then(std::sync::Weak::upgrade) {
            return existing;
        }
        let node = Arc::new(VariantInner { name, fields });
        cache.borrow_mut().insert(key, Arc::downgrade(&node));
        node
    })
}

pub(super) fn variant_with_tag_and_fields(name: TypeTag, fields: SmallVec<[Value; 2]>) -> Value {
    if let Some(key) = small_variant_key(&name, &fields) {
        return Value::Variant(intern_small_variant(name, fields, key));
    }
    Value::Variant(Arc::new(VariantInner { name, fields }))
}

/// Converts a constructor's temporary field `Vec` into the inline payload
/// storage used by ordinary enum nodes. `SmallVec::from_vec` only inlines when
/// the source Vec's capacity is <= the inline capacity; VM call-argument Vecs
/// are pooled and may carry a larger spare capacity from an unrelated call
/// site. For arity <= 2, force a move into inline storage so a two-field enum
/// node does not retain an accidental heap buffer.
pub(super) fn variant_fields(fields: Vec<Value>) -> SmallVec<[Value; 2]> {
    if fields.len() <= 2 {
        fields.into_iter().collect()
    } else {
        SmallVec::from_vec(fields)
    }
}

/// Interns a struct field name to a `&'static str` with a leak
/// bounded by the program's fixed set of field names. Exposed for
/// `gossamer-binding`'s `#[derive(GosStruct)]` glue, which builds a
/// `Value::Struct` from runtime field-name strings.
#[must_use]
pub fn intern_field_name(name: &str) -> &'static str {
    intern_type_name(name)
}

pub(super) fn intern_struct_field_names(names: &[&'static str]) -> Arc<[&'static str]> {
    type StructShapeCache = rustc_hash::FxHashMap<Box<[&'static str]>, Arc<[&'static str]>>;

    thread_local! {
        static SHAPES: std::cell::RefCell<StructShapeCache> =
            std::cell::RefCell::new(rustc_hash::FxHashMap::default());
    }
    SHAPES.with(|shapes| {
        let mut shapes = shapes.borrow_mut();
        if let Some(existing) = shapes.get(names) {
            return Arc::clone(existing);
        }
        let shape: Arc<[&'static str]> = Arc::from(names);
        shapes.insert(names.into(), Arc::clone(&shape));
        shape
    })
}

pub(super) fn intern_struct_field_names_2(
    field0: &'static str,
    field1: &'static str,
) -> Arc<[&'static str]> {
    if field0 == "0" && field1 == "1" {
        static POSITIONAL_2: OnceLock<Arc<[&'static str]>> = OnceLock::new();
        return Arc::clone(POSITIONAL_2.get_or_init(|| Arc::from(["0", "1"])));
    }
    intern_struct_field_names(&[field0, field1])
}

/// Shared empty `Arc<Vec<Value>>` sentinel returned by every
/// constructor that would otherwise allocate a fresh empty `Vec`
/// plus Arc header (~32 B per call). All empty-payload variants
/// and arrays share this single allocation.
#[must_use]
pub(crate) fn empty_value_arc() -> Arc<Vec<Value>> {
    static EMPTY: OnceLock<Arc<Vec<Value>>> = OnceLock::new();
    Arc::clone(EMPTY.get_or_init(|| Arc::new(Vec::new())))
}

/// Shared empty `Arc<Vec<(&'static str, Value)>>` sentinel for
/// field-less struct constructors.
#[must_use]
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn empty_struct_fields() -> Vec<(&'static str, Value)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{TYPE_TAG_SWEEP_MIN, TYPE_TAGS, intern_type_tag};

    #[test]
    fn dead_type_tag_storage_is_not_retained_by_compatibility_lookup() {
        let name = "SessionOwnedTypeTagRegression";
        let tag = intern_type_tag(name);
        let weak = Arc::downgrade(&tag.0);
        drop(tag);
        assert!(weak.upgrade().is_none());
        // The sweep is amortized against table growth, so intern enough
        // distinct names to cross the watermark and prove the dead entry is
        // reclaimed rather than accumulating.
        for i in 0..(TYPE_TAG_SWEEP_MIN * 2 + 2) {
            let _ = intern_type_tag(&format!("SessionOwnedTypeTagSweep{i}"));
        }
        assert!(
            !TYPE_TAGS.lock().contains_key(name),
            "compatibility lookup retained a dead type tag"
        );
    }
}
