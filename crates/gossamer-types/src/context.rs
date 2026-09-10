//! Type interner.
//! Types in Gossamer are content-addressed through the [`TyCtxt`]
//! interner. Interning returns a stable [`Ty`] handle whose equality
//! is pointer-equality on the backing table. All compiler passes share
//! a single context so that type comparisons are O(1).

#![forbid(unsafe_code)]

use std::collections::HashMap;

use crate::ty::{FloatTy, IntTy, Ty, TyKind};

/// Interner that maps [`TyKind`]s to stable [`Ty`] handles.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TyCtxt {
    kinds: Vec<TyKind>,
    index: HashMap<TyKind, Ty>,
    primitives: Primitives,
    struct_fields: HashMap<gossamer_resolve::DefId, Vec<Ty>>,
    /// Per-variant field types of a user enum, in declaration (discriminant)
    /// order - variant `i` at index `i`, unit variants an empty vec. Populated
    /// by the typechecker; consumed by MIR to build the structural-equality
    /// descriptor for heap (recursive / `Box`) enum `==` / `!=`.
    enum_variant_tys: HashMap<gossamer_resolve::DefId, Vec<Vec<Ty>>>,
    /// Declared variant names for each user enum.
    enum_variant_names: HashMap<gossamer_resolve::DefId, Vec<String>>,
    /// Bits each enum stores its discriminant in, keyed by definition.
    /// Absent for an enum whose declaration asked for nothing special.
    enum_repr_bits: HashMap<gossamer_resolve::DefId, u32>,
    /// Concrete non-generic user-enum `Adt` type by name. Lets MIR recover the
    /// enum type from the name its `==` dispatch resolves (operand types are
    /// often still inference vars at lowering time).
    enum_ty_by_name: HashMap<String, Ty>,
    /// Per-instantiation field types of a generic struct, keyed by
    /// `(def, substs)`. A generic struct's `struct_fields` entry holds
    /// the declared field types with rigid `Param` slots; this table
    /// holds the same fields with the instantiation's concrete types
    /// substituted in (`Wrapper<Point>` -> `[Point]`). Populated by the
    /// monomorphizer, which has the `&mut TyCtxt` needed to intern any
    /// composite substituted field types. Consulted through
    /// `adt_field_tys` so every layout / field-projection site sees the
    /// concrete field shape instead of an opaque `Param`.
    struct_fields_inst: HashMap<(gossamer_resolve::DefId, crate::Substs), Vec<Ty>>,
    /// Human-readable names for ADT / alias / fn `DefId`s, indexed by
    /// the local component. Populated by the type checker for user
    /// structs and by sentinel registrations for `Result`/`Option`.
    def_names: HashMap<gossamer_resolve::DefId, String>,
    /// Reference-counting type-meta blobs, keyed by the codegen symbol
    /// name (`gos_rc_meta_<id>`). Populated by MIR lowering when it
    /// emits a `gos_rc_alloc` for an RC-managed ADT; consumed by both
    /// codegen backends to emit one module-global constant per blob and
    /// reference its address at the allocation site. The blob is the
    /// flat `[i64]` child-layout format the runtime's `gos_rt_rc_release`
    /// walks (see `gossamer-runtime` `c_abi::rc`).
    rc_metas: HashMap<String, Vec<i64>>,
    /// Guarded copy-blob meta symbol per aggregate type, for struct /
    /// tuple values whose escaped heap copies are reference counted
    /// (`RC_KIND_STRUCT_GUARDED`). Populated by MIR lowering; consulted
    /// by the LLVM backend at the heap-copy site and by the drop pass
    /// when emitting guarded retain/release walks for stack aggregates.
    aggr_copy_metas: HashMap<Ty, String>,
    /// Aggregate types an enum variant payload holds by pointer to a
    /// heap-boxed copy rather than inline in the payload word. Populated by
    /// MIR lowering at the boxing site; consulted by the LLVM backend, whose
    /// payload read has to materialise the binding through the box.
    boxed_enum_payload_tys: std::collections::HashSet<Ty>,
    /// Interned self-types of every heap-allocated (RC-managed) user
    /// enum. Populated by MIR lowering from the enum index; consulted by
    /// the drop pass to recognise locals holding RC pointers that need a
    /// `gos_rt_rc_release` at end of life. Membership must stay
    /// conservative - see `rc_enum_tys` in the MIR enum index.
    rc_managed_tys: std::collections::HashSet<Ty>,
    /// `DefId.local`s of payload-bearing user enums, registered
    /// eagerly by the typechecker's enum collection. Heap-enum
    /// values of these defs are reference counted regardless of
    /// which interned `Adt` handle a body uses - the per-handle
    /// `rc_managed_tys` registration only happens at constructor
    /// lowering, which made RC accounting depend on item order (a
    /// body lowered before the enum's first constructor skipped
    /// every retain/release for it).
    rc_managed_enum_defs: std::collections::HashSet<u32>,
    /// `DefId.local` of tuple structs (`struct Pt(i64, i64)`), whose fields
    /// are modelled as named "0".."N-1". Lets HIR lowering route positional
    /// access `p.0` on a tuple struct through the named-field path.
    tuple_struct_defs: std::collections::HashSet<u32>,
    /// `DefId.local` of user enums whose every variant has at most one
    /// field that fits in a single 8-byte slot. Such enums use the 2-word
    /// by-value `i128` [disc, payload] representation (no heap node);
    /// `render_ty` / `cl_type_of` map them to `i128` and `is_rc_managed`
    /// reports them as values (their payload, if a managed pointer, is
    /// released per-discriminant on drop).
    inline_enum_defs: std::collections::HashSet<u32>,
}

/// Cached handles for the primitive types that every program uses. The
/// table is populated lazily on the first call to the corresponding
/// accessor.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct Primitives {
    unit: Option<Ty>,
    never: Option<Ty>,
    bool_: Option<Ty>,
    char_: Option<Ty>,
    string_: Option<Ty>,
    error: Option<Ty>,
    json_value: Option<Ty>,
    dyn_value: Option<Ty>,
    dyn_error: Option<Ty>,
    duration: Option<Ty>,
    instant: Option<Ty>,
}

impl TyCtxt {
    /// Returns a deterministic, complete description of this type context for
    /// compiler-artifact identity. Unlike `Debug` on the context itself, map
    /// and set members are sorted so randomized hash iteration cannot turn an
    /// otherwise identical compilation into a cache miss.
    #[must_use]
    pub fn stable_snapshot_key(&self) -> String {
        let mut output = String::new();
        self.write_stable_snapshot(&mut output)
            .expect("writing a type-context snapshot to String cannot fail");
        output
    }

    /// Streams the deterministic compiler-artifact description into a
    /// formatter. Hashing callers can avoid constructing a second large
    /// snapshot string at peak compilation memory.
    pub fn write_stable_snapshot(&self, output: &mut impl std::fmt::Write) -> std::fmt::Result {
        fn sorted<I>(items: I) -> Vec<String>
        where
            I: IntoIterator<Item = String>,
        {
            let mut items: Vec<_> = items.into_iter().collect();
            items.sort_unstable();
            items
        }

        write!(
            output,
            "kinds={:?};primitives={:?};struct_fields={:?};enum_variant_tys={:?};enum_variant_names={:?};enum_repr_bits={:?};enum_ty_by_name={:?};struct_fields_inst={:?};def_names={:?};rc_metas={:?};aggr_copy_metas={:?};rc_managed_tys={:?};rc_managed_enum_defs={:?};tuple_struct_defs={:?};inline_enum_defs={:?};boxed_enum_payload_tys={:?}",
            self.kinds,
            self.primitives,
            sorted(
                self.struct_fields
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{v:?}"))
            ),
            sorted(
                self.enum_variant_tys
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{v:?}"))
            ),
            sorted(
                self.enum_variant_names
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{v:?}"))
            ),
            sorted(
                self.enum_repr_bits
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{v}"))
            ),
            sorted(
                self.enum_ty_by_name
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{v:?}"))
            ),
            sorted(
                self.struct_fields_inst
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{v:?}"))
            ),
            sorted(self.def_names.iter().map(|(k, v)| format!("{k:?}:{v:?}"))),
            sorted(self.rc_metas.iter().map(|(k, v)| format!("{k:?}:{v:?}"))),
            sorted(
                self.aggr_copy_metas
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{v:?}"))
            ),
            sorted(self.rc_managed_tys.iter().map(|v| format!("{v:?}"))),
            sorted(self.rc_managed_enum_defs.iter().map(|v| format!("{v:?}"))),
            sorted(self.tuple_struct_defs.iter().map(|v| format!("{v:?}"))),
            sorted(self.inline_enum_defs.iter().map(|v| format!("{v:?}"))),
            sorted(self.boxed_enum_payload_tys.iter().map(|v| format!("{v:?}"))),
        )
    }

    /// Returns a fresh interner with no entries.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Interns `kind` and returns its stable handle. Calling this with
    /// two structurally-equal `TyKind`s returns the same [`Ty`].
    pub fn intern(&mut self, kind: TyKind) -> Ty {
        if let Some(ty) = self.index.get(&kind) {
            return *ty;
        }
        let ty = Ty(u32::try_from(self.kinds.len()).expect("ty interner overflow"));
        self.kinds.push(kind.clone());
        self.index.insert(kind, ty);
        ty
    }

    /// Looks up the [`TyKind`] backing a handle. Returns [`None`] if
    /// `ty` was not produced by this interner.
    #[must_use]
    pub fn kind(&self, ty: Ty) -> Option<&TyKind> {
        self.kinds.get(ty.0 as usize)
    }

    /// Borrows `kind(ty)`, panicking if the handle is not owned by this
    /// interner. Used in contexts where the caller knows the handle is
    /// valid (e.g. after a prior `intern`).
    ///
    /// # Panics
    ///
    /// Panics when `ty` was not produced by this interner.
    #[must_use]
    pub fn kind_of(&self, ty: Ty) -> &TyKind {
        self.kind(ty).expect("ty handle not owned by this interner")
    }

    /// Number of interned types, useful for tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    /// Returns `true` when no types have been interned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    /// Interns `()`.
    pub fn unit(&mut self) -> Ty {
        if let Some(ty) = self.primitives.unit {
            return ty;
        }
        let ty = self.intern(TyKind::Unit);
        self.primitives.unit = Some(ty);
        ty
    }

    /// Interns `Iterator<T>`, the linear state type used by lazy iterator MIR.
    pub fn iterator_ty(&mut self, item: Ty) -> Ty {
        self.intern(TyKind::Iterator(item))
    }

    /// Interns `Range<T>`, the type a range expression produces.
    pub fn range_ty(&mut self, item: Ty) -> Ty {
        self.intern(TyKind::Range(item))
    }

    /// Returns the interned `()` type if it already exists. Immutable
    /// counterpart to [`unit`](Self::unit) for passes holding `&TyCtxt`
    /// (e.g. the drop/RC inserter, which must type a throwaway local as unit
    /// without assuming the return slot is unit-typed).
    #[must_use]
    pub fn unit_interned(&self) -> Option<Ty> {
        self.primitives.unit
    }

    /// Interns `!`.
    pub fn never(&mut self) -> Ty {
        if let Some(ty) = self.primitives.never {
            return ty;
        }
        let ty = self.intern(TyKind::Never);
        self.primitives.never = Some(ty);
        ty
    }

    /// Interns `bool`.
    pub fn bool_ty(&mut self) -> Ty {
        if let Some(ty) = self.primitives.bool_ {
            return ty;
        }
        let ty = self.intern(TyKind::Bool);
        self.primitives.bool_ = Some(ty);
        ty
    }

    /// Interns `char`.
    pub fn char_ty(&mut self) -> Ty {
        if let Some(ty) = self.primitives.char_ {
            return ty;
        }
        let ty = self.intern(TyKind::Char);
        self.primitives.char_ = Some(ty);
        ty
    }

    /// Interns `String`.
    pub fn string_ty(&mut self) -> Ty {
        if let Some(ty) = self.primitives.string_ {
            return ty;
        }
        let ty = self.intern(TyKind::String);
        self.primitives.string_ = Some(ty);
        ty
    }

    /// Interns the poisoned [`TyKind::Error`] sentinel.
    pub fn error_ty(&mut self) -> Ty {
        if let Some(ty) = self.primitives.error {
            return ty;
        }
        let ty = self.intern(TyKind::Error);
        self.primitives.error = Some(ty);
        ty
    }

    /// Interns the opaque dynamic JSON value type. Cached because
    /// MIR lowering and type checking both reach for it on every
    /// `json::Value`-typed expression.
    pub fn json_value_ty(&mut self) -> Ty {
        if let Some(ty) = self.primitives.json_value {
            return ty;
        }
        let ty = self.intern(TyKind::JsonValue);
        self.primitives.json_value = Some(ty);
        ty
    }

    /// Interns the open dynamic value type. Cached because type checking
    /// and MIR lowering both reach for it on every `DynValue` expression.
    pub fn dyn_value_ty(&mut self) -> Ty {
        if let Some(ty) = self.primitives.dyn_value {
            return ty;
        }
        let ty = self.intern(TyKind::DynValue);
        self.primitives.dyn_value = Some(ty);
        ty
    }

    /// Interns the opaque dynamic `errors::Error` type.
    pub fn dyn_error_ty(&mut self) -> Ty {
        if let Some(ty) = self.primitives.dyn_error {
            return ty;
        }
        let ty = self.intern(TyKind::DynError);
        self.primitives.dyn_error = Some(ty);
        ty
    }

    /// Interns the transparent `time::Duration` newtype. Backed by an
    /// `i64` at runtime; the distinct kind only steers method-form
    /// accessor dispatch.
    pub fn duration_ty(&mut self) -> Ty {
        if let Some(ty) = self.primitives.duration {
            return ty;
        }
        let ty = self.intern(TyKind::Duration);
        self.primitives.duration = Some(ty);
        ty
    }

    /// Interns the transparent `time::Instant` newtype. Backed by an
    /// `i64` (monotonic-ms) at runtime; the distinct kind only steers
    /// method-form accessor dispatch (`inst.elapsed_ms()`).
    pub fn instant_ty(&mut self) -> Ty {
        if let Some(ty) = self.primitives.instant {
            return ty;
        }
        let ty = self.intern(TyKind::Instant);
        self.primitives.instant = Some(ty);
        ty
    }

    /// Interns the opaque nominal alias `def` over representation `repr`.
    pub fn nominal_ty(&mut self, def: gossamer_resolve::DefId, repr: Ty) -> Ty {
        self.intern(TyKind::Nominal { def, repr })
    }

    /// Follows a chain of nominal aliases down to the representation.
    ///
    /// Lowering erases nominal aliases, so a backend should never meet
    /// one; the backends peel with this anyway so that a value which does
    /// reach them behaves as the type it actually is at runtime.
    #[must_use]
    pub fn peel_nominal(&self, mut ty: Ty) -> Ty {
        while let Some(TyKind::Nominal { repr, .. }) = self.kind(ty) {
            ty = *repr;
        }
        ty
    }

    /// Interns an integer primitive.
    pub fn int_ty(&mut self, int: IntTy) -> Ty {
        self.intern(TyKind::Int(int))
    }

    /// Interns a floating-point primitive.
    pub fn float_ty(&mut self, float: FloatTy) -> Ty {
        self.intern(TyKind::Float(float))
    }

    /// Records the field types of a named struct in source order.
    /// Called by the typechecker once per struct declaration.
    pub fn register_struct_fields(&mut self, def: gossamer_resolve::DefId, fields: Vec<Ty>) {
        self.struct_fields.insert(def, fields);
    }

    /// Returns the registered field types of the struct identified
    /// by `def`, or `None` when no registration has been made.
    #[must_use]
    pub fn struct_field_tys(&self, def: gossamer_resolve::DefId) -> Option<&[Ty]> {
        self.struct_fields.get(&def).map(Vec::as_slice)
    }

    /// Records the per-variant field types of a user enum, in declaration
    /// (discriminant) order. Called by the typechecker once per enum.
    pub fn register_enum_variant_tys(
        &mut self,
        def: gossamer_resolve::DefId,
        variants: Vec<Vec<Ty>>,
    ) {
        self.enum_variant_tys.insert(def, variants);
    }

    /// Per-variant field types of the enum identified by `def`, in declaration
    /// (discriminant) order, or `None` if unregistered.
    #[must_use]
    pub fn enum_variant_tys(&self, def: gossamer_resolve::DefId) -> Option<&[Vec<Ty>]> {
        self.enum_variant_tys.get(&def).map(Vec::as_slice)
    }

    /// Records variant names in declaration order for a user enum.
    pub fn register_enum_variant_names(
        &mut self,
        def: gossamer_resolve::DefId,
        variants: Vec<String>,
    ) {
        self.enum_variant_names.insert(def, variants);
    }

    /// Records the width `def` stores its discriminant in.
    pub fn register_enum_repr_bits(&mut self, def: gossamer_resolve::DefId, bits: u32) {
        self.enum_repr_bits.insert(def, bits);
    }

    /// The width `def` stores its discriminant in, when the declaration
    /// asked for one narrower or wider than the default.
    #[must_use]
    pub fn enum_repr_bits(&self, def: gossamer_resolve::DefId) -> Option<u32> {
        self.enum_repr_bits.get(&def).copied()
    }

    /// Every enum that has registered variant names, with those names in
    /// declaration order. Used to bind a Rust binding's declared arm set to
    /// the Gossamer enum that spells the same arms.
    #[must_use]
    pub fn enums_with_variant_names(&self) -> Vec<(gossamer_resolve::DefId, &[String])> {
        self.enum_variant_names
            .iter()
            .map(|(def, names)| (*def, names.as_slice()))
            .collect()
    }

    /// Returns the declared variant names for `def`.
    #[must_use]
    pub fn enum_variant_names(&self, def: gossamer_resolve::DefId) -> Option<&[String]> {
        self.enum_variant_names.get(&def).map(Vec::as_slice)
    }

    /// Records the concrete `Adt` type of a non-generic user enum by name.
    pub fn register_enum_ty_by_name(&mut self, name: impl Into<String>, ty: Ty) {
        self.enum_ty_by_name.insert(name.into(), ty);
    }

    /// The concrete `Adt` type of the non-generic user enum named `name`.
    #[must_use]
    pub fn enum_ty_by_name(&self, name: &str) -> Option<Ty> {
        self.enum_ty_by_name.get(name).copied()
    }

    /// Records the concrete field types of a generic struct instantiation
    /// `(def, substs)`. Called by the monomorphizer once per distinct
    /// instantiation so the compiled tiers can read the substituted layout.
    pub fn register_struct_fields_inst(
        &mut self,
        def: gossamer_resolve::DefId,
        substs: crate::Substs,
        fields: Vec<Ty>,
    ) {
        self.struct_fields_inst.insert((def, substs), fields);
    }

    /// Returns the field types of the ADT `(def, substs)` with the
    /// instantiation's generic arguments applied. For a non-generic
    /// struct (or an empty `substs`) this is the declared field list. For
    /// a generic instantiation it is the per-instantiation list the
    /// monomorphizer registered; if none was registered (the substs are
    /// all rigid `Param`s, e.g. inside an un-specialised template) it
    /// falls back to the declared field list. Every layout / field-type
    /// site routes through here so a generic struct lays out by its
    /// concrete fields rather than an opaque `Param` slot.
    #[must_use]
    pub fn adt_field_tys(
        &self,
        def: gossamer_resolve::DefId,
        substs: &crate::Substs,
    ) -> Option<&[Ty]> {
        if !substs.is_empty()
            && let Some(fields) = self.struct_fields_inst.get(&(def, substs.clone()))
        {
            return Some(fields.as_slice());
        }
        self.struct_fields.get(&def).map(Vec::as_slice)
    }

    /// Whether a container element of this type is reached by the address
    /// of its slots rather than read as a word: a struct, a tuple, or a
    /// fixed array, at any width. A one-field struct and a one-element
    /// tuple each occupy a single slot yet a field projection still
    /// dereferences off the slot address, so the width alone cannot
    /// answer this - a one-word scalar element is the value itself.
    /// The opaque stdlib handles (`def.local` in the `u32::MAX - 16 ..=
    /// u32::MAX` sentinel range, `Option` and `Result` among them) hold a
    /// pointer in their word and are addressed only when they span more
    /// than one slot.
    #[must_use]
    pub fn elem_is_addressed_aggregate(&self, ty: Ty) -> bool {
        match self.kind_of(ty) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            TyKind::Adt { def, substs } => {
                let (def, substs) = (*def, substs.clone());
                let user_struct =
                    def.local < u32::MAX - 16 && self.adt_field_tys(def, &substs).is_some();
                user_struct || self.slot_bytes(ty) > 8
            }
            _ => false,
        }
    }

    /// An addressed aggregate that occupies a single slot, which is the
    /// case the element width cannot report on its own.
    #[must_use]
    pub fn is_flat_inline_aggregate(&self, ty: Ty) -> bool {
        self.slot_bytes(ty) <= 8 && self.elem_is_addressed_aggregate(ty)
    }

    /// Inline slot size in bytes of a value of type `ty` on the compiled
    /// tiers, where every slot is 8-byte-aligned. Aggregates sum their
    /// fields' rounded-up slot widths; `Option`/`Result` are the 2-word
    /// (16-byte) by-value representation; opaque stdlib handles are one
    /// slot. The MIR builder's `type_slot_bytes` delegates here so the
    /// vec-element-layout passes and the builder agree exactly.
    #[must_use]
    pub fn slot_bytes(&self, ty: Ty) -> u32 {
        match self.kind_of(ty) {
            TyKind::Tuple(elems) => {
                let total: u32 = elems.iter().map(|t| self.slot_bytes(*t).max(8) / 8).sum();
                total.max(1) * 8
            }
            TyKind::Array { elem, len } => {
                let elem_bytes = self.slot_bytes(*elem).max(8);
                u32::try_from(len.to_usize())
                    .unwrap_or(1)
                    .saturating_mul(elem_bytes)
            }
            TyKind::Adt { def, substs } => {
                if def.local == u32::MAX || def.local == u32::MAX - 1 {
                    return 16;
                }
                if def.local >= u32::MAX - 6 {
                    return 8;
                }
                if let Some(field_tys) = self.adt_field_tys(*def, substs) {
                    let total_slots: u32 = field_tys
                        .iter()
                        .map(|t| self.slot_bytes(*t).max(8) / 8)
                        .sum();
                    return total_slots.max(1) * 8;
                }
                8
            }
            TyKind::Bool => 1,
            TyKind::Char => 4,
            TyKind::Int(_) | TyKind::Float(_) | TyKind::String => 8,
            _ => 8,
        }
    }

    /// Records a human-readable name for the given `DefId`.
    /// Overwrites any previous registration.
    pub fn register_def_name(&mut self, def: gossamer_resolve::DefId, name: impl Into<String>) {
        self.def_names.insert(def, name.into());
    }

    /// Returns the registered name for `def`, or `None`.
    #[must_use]
    pub fn def_name(&self, def: gossamer_resolve::DefId) -> Option<&str> {
        self.def_names.get(&def).map(String::as_str)
    }

    /// Records a reference-counting type-meta blob under `symbol`,
    /// keeping the first registration if one already exists (the blob is
    /// a pure function of the type, so duplicates are identical).
    pub fn register_rc_meta(&mut self, symbol: impl Into<String>, blob: Vec<i64>) {
        self.rc_metas.entry(symbol.into()).or_insert(blob);
    }

    /// Records the guarded copy-blob meta symbol for an aggregate type
    /// (idempotent). The blob itself goes through [`Self::register_rc_meta`].
    pub fn register_aggr_copy_meta(&mut self, ty: Ty, symbol: impl Into<String>) {
        self.aggr_copy_metas
            .entry(ty)
            .or_insert_with(|| symbol.into());
    }

    /// The guarded copy-blob meta symbol for `ty`, when registered.
    #[must_use]
    pub fn aggr_copy_meta(&self, ty: Ty) -> Option<&str> {
        self.aggr_copy_metas.get(&ty).map(String::as_str)
    }

    /// Records that an enum variant payload of type `ty` is stored as a
    /// pointer to a heap-boxed copy.
    pub fn register_boxed_enum_payload(&mut self, ty: Ty) {
        self.boxed_enum_payload_tys.insert(ty);
    }

    /// Whether an enum variant payload of type `ty` is a pointer to a
    /// heap-boxed copy rather than the aggregate's words in the payload slot.
    #[must_use]
    pub fn is_boxed_enum_payload(&self, ty: Ty) -> bool {
        self.boxed_enum_payload_tys.contains(&ty)
    }

    /// Returns the RC type-meta blob registered under `symbol`, if any.
    #[must_use]
    pub fn rc_meta(&self, symbol: &str) -> Option<&[i64]> {
        self.rc_metas.get(symbol).map(Vec::as_slice)
    }

    /// Iterates every registered `(symbol, blob)` pair so a codegen
    /// backend can emit one module-global constant per RC type-meta.
    pub fn rc_metas(&self) -> impl Iterator<Item = (&str, &[i64])> {
        self.rc_metas
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// Records that `ty` is a heap-allocated (RC-managed) user enum.
    pub fn register_rc_managed_ty(&mut self, ty: Ty) {
        self.rc_managed_tys.insert(ty);
    }

    /// Registers a payload-bearing user enum (by `DefId.local`) as
    /// reference counted. Called eagerly during typechecking so RC
    /// accounting never depends on which body lowers first.
    /// All-unit enums must NOT be registered - they lower as bare
    /// `i64` discriminants and releasing one would treat the
    /// integer as a pointer.
    pub fn register_rc_managed_enum_def(&mut self, def_local: u32) {
        self.rc_managed_enum_defs.insert(def_local);
    }

    /// Registers a tuple struct (by `DefId.local`) so positional access
    /// `p.0` lowers through the named-field path.
    pub fn register_tuple_struct(&mut self, def_local: u32) {
        self.tuple_struct_defs.insert(def_local);
    }

    /// `true` when `def_local` names a tuple struct.
    #[must_use]
    pub fn is_tuple_struct(&self, def_local: u32) -> bool {
        self.tuple_struct_defs.contains(&def_local)
    }

    /// Registers a user enum (by `DefId.local`) as inline-able - its values
    /// are the 2-word by-value `i128` representation.
    pub fn register_inline_enum_def(&mut self, def_local: u32) {
        self.inline_enum_defs.insert(def_local);
    }

    /// True when `ty` is an inline-able user enum (2-word by-value).
    #[must_use]
    pub fn is_inline_enum_ty(&self, ty: Ty) -> bool {
        matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. }) if self.inline_enum_defs.contains(&def.local)
        )
    }

    /// Returns true when `ty` is a heap-allocated user enum whose values
    /// are reference-counted (`gos_rc_alloc`) and must be released with
    /// `gos_rt_rc_release` at end of life.
    #[must_use]
    pub fn is_rc_managed(&self, ty: Ty) -> bool {
        // `Result<T,E>` (sentinel `u32::MAX`) and `Option<T>` (`u32::MAX - 1`)
        // are by-value 2-word (`i128`) values, NOT RC pointers. Their payload
        // is RC-managed through the binding it is extracted into; the
        // Result/Option local itself must never be RC-released (that would
        // treat the packed `i128` as a pointer and corrupt the heap).
        if matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX || def.local == u32::MAX - 1
        ) {
            return false;
        }
        // Opaque runtime-handle / heap-blob stdlib structs - `fs::DirInfo`
        // (`u32::MAX - 2`), `process::Output` (`- 3`), `http::ResponseStream`
        // (`- 4`), and `http::Response` (`- 5`). Each is a plain `Box`
        // handle with no RC header, so `gos_rt_rc_release` on the whole
        // local reads a non-existent header and corrupts the heap. They are
        // never reference-counted: handle locals leak (process-teardown
        // reclaim) exactly like `http::Client` / SQL handles. (`u32::MAX`
        // Result / `- 1` Option already returned above; `- 6` Weak stays
        // RC-managed via the weak helpers.)
        if matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. })
                if (u32::MAX - 5..=u32::MAX - 2).contains(&def.local)
        ) {
            return false;
        }
        // `net::TcpStream` / `TcpListener` / `UdpSocket` / `UnixStream` /
        // `UnixListener` (sentinels `u32::MAX - 16 ..= - 12`) and
        // `signal::Notifier` (`u32::MAX - 17`) are bare i64 handles with
        // no RC header - never reference-counted, freed by runtime
        // ownership / process teardown like the other opaque handles above.
        if matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. })
                if (u32::MAX - 17..=u32::MAX - 12).contains(&def.local)
        ) {
            return false;
        }
        // `flag::Set` is an opaque parser handle, owned by the runtime
        // registry rather than an RC allocation.
        if matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX - 21
        ) {
            return false;
        }
        // HTTP client builder/request handles and io stream handles are
        // runtime-owned opaque pointers, not reference-counted user aggregates.
        if matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. })
                if (u32::MAX - 24..=u32::MAX - 22).contains(&def.local)
                    || def.local == u32::MAX - 25
        ) {
            return false;
        }
        // Inline-able user enums are by-value (their payload's RC, if any, is
        // released per-discriminant on drop), never RC-managed as a whole.
        if self.is_inline_enum_ty(ty) {
            return false;
        }
        if matches!(self.kind(ty), Some(TyKind::String)) {
            return true;
        }
        // A callable value carries its captures in an environment the closure
        // lowering allocates with an RC header, and the body address lives at
        // its first word. That allocation has an owner-shaped lifetime, so the
        // ordinary drop passes reclaim it; a non-capturing callable coerced to
        // the same type carries a null environment, which releases as a no-op.
        // `FnPtr` and `FnDef` are the raw code-pointer shapes, which carry no
        // environment: a bare item and a non-capturing closure both reach a
        // call site as an address, and only the environment-carrying form
        // owns an allocation.
        if matches!(
            self.kind(ty),
            Some(TyKind::FnTrait(_) | TyKind::Closure { .. })
        ) {
            return true;
        }
        if self.rc_managed_tys.contains(&ty) {
            return true;
        }
        // Payload-bearing user enums are heap nodes (tagged-pointer
        // or header-disc) on the compiled tiers; recognise every
        // instantiation by def, not by interned handle. Inline
        // (2-word by-value) enums already returned false above.
        if matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. }) if self.rc_managed_enum_defs.contains(&def.local)
        ) {
            return true;
        }
        // `Weak<T>` (sentinel def `u32::MAX - 6`) is a weak-counted
        // pointer into an RC allocation, regardless of its payload
        // substitution. It must be processed by the drop pass - which
        // branches on `is_weak_ty` to emit the weak retain/release
        // helpers instead of the strong ones - so report it here even
        // though no per-instantiation registration exists.
        self.is_weak_ty(ty)
    }

    /// Returns true when `ty` is a `Weak<T>` reference (the sentinel
    /// `DefId` `u32::MAX - 6`). Used by the drop pass to route the
    /// retain/release of a weak binding through
    /// `gos_rt_rc_weak_retain` / `gos_rt_rc_weak_release`.
    #[must_use]
    pub fn is_weak_ty(&self, ty: Ty) -> bool {
        matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX - 6
        )
    }

    /// Returns true when `ty` is a payload-bearing user enum - a heap node
    /// (tagged-pointer / header-disc) on the compiled tiers, as opposed to an
    /// inline two-word by-value enum. A `Vec` of such an enum carries boxed,
    /// individually reference-counted elements.
    #[must_use]
    pub fn is_payload_enum(&self, ty: Ty) -> bool {
        matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. }) if self.rc_managed_enum_defs.contains(&def.local)
        )
    }
}
