//! Type interner.
//! Types in Gossamer are content-addressed through the [`TyCtxt`]
//! interner. Interning returns a stable [`Ty`] handle whose equality
//! is pointer-equality on the backing table. All compiler passes share
//! a single context so that type comparisons are O(1).

#![forbid(unsafe_code)]

use std::collections::HashMap;

use crate::ty::{FloatTy, IntTy, Ty, TyKind};

/// The byte layout of a plain-data value: see [`TyCtxt::plain_layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlainLayout {
    /// Size in bytes, a multiple of `align`.
    pub size: u32,
    /// Alignment in bytes: the largest alignment of any leaf.
    pub align: u32,
    /// Byte offset of each field of a tuple or struct, in declaration order;
    /// empty for a scalar or an array.
    pub field_offsets: Vec<u32>,
}

/// The registered name of the injected `ffi::Union` struct, whose layout is
/// its members' rather than its declared field's.
pub const UNION_TYPE_NAME: &str = "__gos_ffi_Union";

/// The storage layout of a struct whose fields narrower than a word sit at
/// their own width: see [`TyCtxt::packed_layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedLayout {
    /// Size in bytes, a whole number of words.
    pub size: u32,
    /// Byte offset of each field, in declaration order.
    pub field_offsets: Vec<u32>,
    /// Bytes each field occupies at its offset.
    pub field_bytes: Vec<u32>,
}

/// Where the fields of a by-value enum live.
///
/// The value is a struct whose first field is the discriminant (an `i64`)
/// and whose remaining fields are `regions`. Each variant's fields take
/// regions of their storage type, two fields of one variant never sharing
/// one, so every region holds one type in every variant that uses it. A
/// region the current variant does not use holds zeroes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FlatEnumLayout {
    /// The type of each region, after the discriminant.
    pub regions: Vec<Ty>,
    /// For each variant, the region each of its fields occupies.
    pub variant_regions: Vec<Vec<u32>>,
    /// For each variant, the type of each of its fields. A field whose type
    /// differs from its region's is a one-word scalar held in a shared word:
    /// an integer, `bool`, `char`, or unit-only enum in an `i64`, a float in
    /// an `f64`.
    pub variant_field_tys: Vec<Vec<Ty>>,
}

impl FlatEnumLayout {
    /// The struct field holding field `field` of variant `variant`: the
    /// discriminant is field 0.
    #[must_use]
    pub fn field_of(&self, variant: u32, field: u32) -> Option<u32> {
        self.variant_regions
            .get(variant as usize)?
            .get(field as usize)
            .map(|region| region + 1)
    }

    /// The struct-shaped field list: the discriminant, then every region.
    #[must_use]
    pub fn struct_fields(&self, tag: Ty) -> Vec<Ty> {
        std::iter::once(tag)
            .chain(self.regions.iter().copied())
            .collect()
    }
}

/// Interner that maps [`TyKind`]s to stable [`Ty`] handles.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TyCtxt {
    kinds: Vec<TyKind>,
    #[serde(serialize_with = "gossamer_ast::ordered::map_by_value")]
    index: HashMap<TyKind, Ty>,
    primitives: Primitives,
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    struct_fields: HashMap<gossamer_resolve::DefId, Vec<Ty>>,
    /// Per-variant field types of a user enum, in declaration (discriminant)
    /// order - variant `i` at index `i`, unit variants an empty vec. Populated
    /// by the typechecker; consumed by MIR to build the structural-equality
    /// descriptor for heap (recursive / `Box`) enum `==` / `!=`.
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    enum_variant_tys: HashMap<gossamer_resolve::DefId, Vec<Vec<Ty>>>,
    /// Declared variant names for each user enum.
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    enum_variant_names: HashMap<gossamer_resolve::DefId, Vec<String>>,
    /// Bits each enum stores its discriminant in, keyed by definition.
    /// Absent for an enum whose declaration asked for nothing special.
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    enum_repr_bits: HashMap<gossamer_resolve::DefId, u32>,
    /// Concrete non-generic user-enum `Adt` type by name. Lets MIR recover the
    /// enum type from the name its `==` dispatch resolves (operand types are
    /// often still inference vars at lowering time).
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
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
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    struct_fields_inst: HashMap<(gossamer_resolve::DefId, crate::Substs), Vec<Ty>>,
    /// Human-readable names for ADT / alias / fn `DefId`s, indexed by
    /// the local component. Populated by the type checker for user
    /// structs and by sentinel registrations for `Result`/`Option`.
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    def_names: HashMap<gossamer_resolve::DefId, String>,
    /// The `Param` type each generic type parameter's resolver `DefId`
    /// stands for, recorded by the type checker where a path is headed by
    /// the parameter (`T::zero`). Lowering reads it to name the parameter's
    /// position without re-deriving the enclosing generic scope.
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    type_param_defs: HashMap<gossamer_resolve::DefId, Ty>,
    /// Reference-counting type-meta blobs, keyed by the codegen symbol
    /// name (`gos_rc_meta_<id>`). Populated by MIR lowering when it
    /// emits a `gos_rc_alloc` for an RC-managed ADT; consumed by both
    /// codegen backends to emit one module-global constant per blob and
    /// reference its address at the allocation site. The blob is the
    /// flat `[i64]` child-layout format the runtime's `gos_rt_rc_release`
    /// walks (see `gossamer-runtime` `c_abi::rc`).
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    rc_metas: HashMap<String, Vec<i64>>,
    /// Guarded copy-blob meta symbol per aggregate type, for struct /
    /// tuple values whose escaped heap copies are reference counted
    /// (`RC_KIND_STRUCT_GUARDED`). Populated by MIR lowering; consulted
    /// by the LLVM backend at the heap-copy site and by the drop pass
    /// when emitting guarded retain/release walks for stack aggregates.
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    aggr_copy_metas: HashMap<Ty, String>,
    /// Aggregate types an enum variant payload holds by pointer to a
    /// heap-boxed copy rather than inline in the payload word. Populated by
    /// MIR lowering at the boxing site; consulted by the LLVM backend, whose
    /// payload read has to materialise the binding through the box.
    #[serde(serialize_with = "gossamer_ast::ordered::set")]
    boxed_enum_payload_tys: std::collections::HashSet<Ty>,
    /// Interned self-types of every heap-allocated (RC-managed) user
    /// enum. Populated by MIR lowering from the enum index; consulted by
    /// the drop pass to recognise locals holding RC pointers that need a
    /// `gos_rt_rc_release` at end of life. Membership must stay
    /// conservative - see `rc_enum_tys` in the MIR enum index.
    #[serde(serialize_with = "gossamer_ast::ordered::set")]
    rc_managed_tys: std::collections::HashSet<Ty>,
    /// `DefId.local`s of payload-bearing user enums, registered
    /// eagerly by the typechecker's enum collection. Heap-enum
    /// values of these defs are reference counted regardless of
    /// which interned `Adt` handle a body uses - the per-handle
    /// `rc_managed_tys` registration only happens at constructor
    /// lowering, which made RC accounting depend on item order (a
    /// body lowered before the enum's first constructor skipped
    /// every retain/release for it).
    #[serde(serialize_with = "gossamer_ast::ordered::set")]
    rc_managed_enum_defs: std::collections::HashSet<u32>,
    /// `DefId.local` of tuple structs (`struct Pt(i64, i64)`), whose fields
    /// are modelled as named "0".."N-1". Lets HIR lowering route positional
    /// access `p.0` on a tuple struct through the named-field path.
    #[serde(serialize_with = "gossamer_ast::ordered::set")]
    tuple_struct_defs: std::collections::HashSet<u32>,
    /// `DefId.local` of user enums whose every variant has at most one
    /// field that fits in a single 8-byte slot. Such enums use the 2-word
    /// by-value `i128` [disc, payload] representation (no heap node);
    /// `render_ty` / `cl_type_of` map them to `i128` and `is_rc_managed`
    /// reports them as values (their payload, if a managed pointer, is
    /// released per-discriminant on drop).
    #[serde(serialize_with = "gossamer_ast::ordered::set")]
    inline_enum_defs: std::collections::HashSet<u32>,
    /// `DefId.local` of user enums stored by value as a tag word followed by
    /// the regions their variants' fields share, with the layout of each.
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    flat_enums: HashMap<u32, FlatEnumLayout>,
    /// Whether every user enum keeps its heap node, as the bytecode VM's
    /// enum values are, so code compiled to run beside it passes them as is.
    #[serde(default)]
    enums_on_heap: bool,
    /// The tuple each `Option` / `Result` over a by-value enum is held as
    /// in a body, mapped to the carrier type it stands for.
    #[serde(serialize_with = "gossamer_ast::ordered::map")]
    flat_carriers: HashMap<Ty, Ty>,
    /// Structs a program takes a `&mut` to a narrow field of. The borrow hands
    /// its callee an address a whole word is written through, so such a
    /// struct keeps one word per field. MIR lowering records these before it
    /// reads a layout; they are not part of the front-end result.
    #[serde(skip)]
    #[serde(serialize_with = "gossamer_ast::ordered::set")]
    word_field_structs: std::collections::HashSet<gossamer_resolve::DefId>,
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
            "kinds={:?};primitives={:?};struct_fields={:?};enum_variant_tys={:?};enum_variant_names={:?};enum_repr_bits={:?};enum_ty_by_name={:?};struct_fields_inst={:?};def_names={:?};type_param_defs={:?};rc_metas={:?};aggr_copy_metas={:?};rc_managed_tys={:?};rc_managed_enum_defs={:?};tuple_struct_defs={:?};inline_enum_defs={:?};flat_enums={:?};flat_carriers={:?};boxed_enum_payload_tys={:?}",
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
            sorted(
                self.type_param_defs
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{v:?}"))
            ),
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
            sorted(self.flat_enums.iter().map(|(k, v)| format!("{k:?}:{v:?}"))),
            sorted(
                self.flat_carriers
                    .iter()
                    .map(|(k, v)| format!("{k:?}:{v:?}"))
            ),
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

    /// The type already interned for `kind`, without interning it.
    #[must_use]
    pub fn interned(&self, kind: &TyKind) -> Option<Ty> {
        self.index.get(kind).copied()
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

    /// Replaces every opaque alias in the registered struct and enum field
    /// types with its representation. HIR types are erased the same way at
    /// lowering, so a field read typed from these tables agrees with the
    /// expression that reads it.
    pub fn erase_nominal_field_tys(&mut self) {
        let mut structs = std::mem::take(&mut self.struct_fields);
        for ty in structs.values_mut().flatten() {
            *ty = crate::erase_nominal(self, *ty);
        }
        self.struct_fields = structs;
        let mut instances = std::mem::take(&mut self.struct_fields_inst);
        for ty in instances.values_mut().flatten() {
            *ty = crate::erase_nominal(self, *ty);
        }
        self.struct_fields_inst = instances;
        let mut variants = std::mem::take(&mut self.enum_variant_tys);
        for ty in variants.values_mut().flatten().flatten() {
            *ty = crate::erase_nominal(self, *ty);
        }
        self.enum_variant_tys = variants;
    }

    /// Whether a value of `ty` is two words held by value: an `Option`, a
    /// `Result`, or an inline user enum. Storage that holds one in place
    /// gives it both words.
    #[must_use]
    pub fn is_two_word_value(&self, ty: Ty) -> bool {
        matches!(
            self.kind_of(ty),
            TyKind::Adt { def, .. } if def.local == u32::MAX || def.local == u32::MAX - 1
        ) || self.is_inline_enum_ty(ty)
    }

    /// Whether `ty` is a struct or tuple with no fields, whose value carries
    /// no bits.
    #[must_use]
    pub fn is_field_free_aggregate(&self, ty: Ty) -> bool {
        match self.kind_of(ty) {
            TyKind::Tuple(elems) => elems.is_empty(),
            TyKind::Adt { def, .. } if def.local < u32::MAX - 16 => {
                self.enum_variant_tys(*def).is_none()
                    && self.struct_field_tys(*def).is_some_and(<[Ty]>::is_empty)
            }
            _ => false,
        }
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

    /// The field types recorded for the generic struct instantiation
    /// `(def, substs)`, or `None` when none has been recorded.
    #[must_use]
    pub fn struct_fields_instance(
        &self,
        def: gossamer_resolve::DefId,
        substs: &crate::Substs,
    ) -> Option<&[Ty]> {
        self.struct_fields_inst
            .get(&(def, substs.clone()))
            .map(Vec::as_slice)
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

    /// Every instantiation of the generic struct `def` the monomorphizer
    /// registered, as its interned type and its field types, ordered by type.
    #[must_use]
    pub fn struct_instantiations(&self, def: gossamer_resolve::DefId) -> Vec<(Ty, &[Ty])> {
        let mut out: Vec<(Ty, &[Ty])> = self
            .struct_fields_inst
            .iter()
            .filter(|((owner, _), _)| *owner == def)
            .filter_map(|((owner, substs), fields)| {
                let ty = self.interned(&TyKind::Adt {
                    def: *owner,
                    substs: substs.clone(),
                })?;
                Some((ty, fields.as_slice()))
            })
            .collect();
        out.sort_by_key(|(ty, _)| *ty);
        out
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
            TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Simd { .. } => true,
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

    /// Whether every leaf of this type is a scalar, so the value owns no
    /// heap child and moving its bytes is moving the whole value.
    #[must_use]
    pub fn scalar_leaves_only(&self, ty: Ty) -> bool {
        match self.kind_of(ty) {
            TyKind::Int(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char => true,
            TyKind::Tuple(elems) => {
                let elems = elems.clone();
                elems.iter().all(|t| self.scalar_leaves_only(*t))
            }
            TyKind::Array { elem, .. } | TyKind::Simd { elem, .. } => {
                let elem = *elem;
                self.scalar_leaves_only(elem)
            }
            TyKind::Adt { def, substs } => {
                let (def, substs) = (*def, substs.clone());
                def.local < u32::MAX - 16
                    && self
                        .adt_field_tys(def, &substs)
                        .is_some_and(|fields| fields.iter().all(|t| self.scalar_leaves_only(*t)))
            }
            _ => false,
        }
    }

    /// Whether a value of `ty` can be part of a reference cycle, which is what
    /// decides if the cycle collector has to consider it.
    ///
    /// A cycle needs a path of references from a value back to itself that a
    /// program can close after the values exist. Values cannot close one: every
    /// in-place write into a struct field, an element, an enum payload, or a
    /// map value lands in storage its writer alone holds, since a write
    /// through a shared one copies it first, and a value holding the target
    /// shares it, so the write reaches a copy. Only a reference written in
    /// place closes one, and so does anything whose contents the type does not
    /// state - a closure environment, a channel, a shared or locked cell, a
    /// `Weak`, a dynamic value, an unresolved type - which is assumed able to.
    #[must_use]
    pub fn can_join_cycle(&self, ty: Ty) -> bool {
        let mut seen: Vec<(Ty, bool)> = Vec::new();
        let mut stack: Vec<(Ty, bool)> = vec![(ty, false)];
        while let Some((current, through_write)) = stack.pop() {
            if seen.contains(&(current, through_write)) {
                continue;
            }
            seen.push((current, through_write));
            let Some(edges) = self.cycle_edges(current) else {
                return true;
            };
            for (next, writable) in edges {
                let through = through_write || writable;
                if next == ty && through {
                    return true;
                }
                stack.push((next, through));
            }
        }
        false
    }

    /// The types a value of `ty` holds references to, each with whether the
    /// program can write that reference in place where another holder sees
    /// it; `None` for a type whose references the type does not state.
    fn cycle_edges(&self, ty: Ty) -> Option<Vec<(Ty, bool)>> {
        let values = |items: &[Ty]| items.iter().map(|t| (*t, false)).collect::<Vec<_>>();
        Some(match self.kind_of(ty) {
            TyKind::Bool
            | TyKind::Char
            | TyKind::String
            | TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Unit
            | TyKind::Never
            | TyKind::Duration
            | TyKind::Instant
            | TyKind::JsonValue
            | TyKind::DynError
            | TyKind::FnDef { .. }
            | TyKind::Range(_) => Vec::new(),
            TyKind::Tuple(items) => values(items),
            TyKind::Array { elem, .. }
            | TyKind::Simd { elem, .. }
            | TyKind::Slice(elem)
            | TyKind::Vec(elem) => vec![(*elem, false)],
            TyKind::HashMap { key, value, .. } => vec![(*key, false), (*value, false)],
            TyKind::Ref { inner, .. } => vec![(*inner, true)],
            TyKind::Nominal { repr, .. } => vec![(*repr, false)],
            TyKind::Adt { def, substs } => return self.adt_cycle_edges(*def, substs),
            _ => return None,
        })
    }

    fn adt_cycle_edges(
        &self,
        def: gossamer_resolve::DefId,
        substs: &crate::Substs,
    ) -> Option<Vec<(Ty, bool)>> {
        // `Option` and `Result` hold their payloads as enums do.
        if def.local == u32::MAX || def.local == u32::MAX - 1 {
            return Some(substs.types().into_iter().map(|t| (t, false)).collect());
        }
        if def.local >= u32::MAX - 64 {
            // The standard library's element containers hold what their
            // arguments name; every other built-in handle hides its contents.
            let container = matches!(
                self.def_name(def),
                Some(
                    "Set"
                        | "BTreeSet"
                        | "BTreeMap"
                        | "Queue"
                        | "Stack"
                        | "Deque"
                        | "MinHeap"
                        | "MaxHeap"
                )
            );
            return container.then(|| substs.types().into_iter().map(|t| (t, false)).collect());
        }
        if let Some(variants) = self.enum_variant_tys(def) {
            let args = substs.types();
            let mut edges = Vec::new();
            for payload in variants.iter().flatten() {
                let payload = match self.kind_of(*payload) {
                    TyKind::Param { idx, .. } => *args.get(idx.as_u32() as usize)?,
                    _ if args.is_empty() => *payload,
                    // A generic enum's payload that is not one of its own
                    // parameters is not substituted here.
                    _ => return None,
                };
                edges.push((payload, false));
            }
            return Some(edges);
        }
        let fields = self.adt_field_tys(def, substs)?;
        Some(fields.iter().map(|t| (*t, false)).collect())
    }

    /// Whether a value of this type is plain data: every leaf a scalar, through
    /// tuples, fixed arrays, `Simd` lanes, and structs, with no handle, `Weak`,
    /// or heap child anywhere and no cycle through its own definition. Moving
    /// its bytes moves the whole value, so it needs no reference count.
    #[must_use]
    pub fn is_plain_data(&self, ty: Ty) -> bool {
        self.plain_layout(ty).is_some()
    }

    /// The byte layout a plain-data value takes under the C `repr(C)` rule:
    /// each field at its own size and alignment in declaration order, and the
    /// whole padded to its largest alignment. `None` for a type that is not
    /// plain data.
    #[must_use]
    pub fn plain_layout(&self, ty: Ty) -> Option<PlainLayout> {
        self.plain_layout_within(ty, &mut Vec::new())
    }

    fn plain_layout_within(
        &self,
        ty: Ty,
        visiting: &mut Vec<gossamer_resolve::DefId>,
    ) -> Option<PlainLayout> {
        let scalar = |size: u32| {
            Some(PlainLayout {
                size,
                align: size,
                field_offsets: Vec::new(),
            })
        };
        match self.kind_of(ty) {
            TyKind::Bool => scalar(1),
            TyKind::Char => scalar(4),
            TyKind::Int(int) => match int {
                IntTy::I8 | IntTy::U8 => scalar(1),
                IntTy::I16 | IntTy::U16 => scalar(2),
                IntTy::I32 | IntTy::U32 => scalar(4),
                IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize => scalar(8),
                IntTy::I128 | IntTy::U128 => scalar(16),
            },
            TyKind::Float(FloatTy::F32) => scalar(4),
            TyKind::Float(FloatTy::F64) => scalar(8),
            TyKind::Array { elem, len } | TyKind::Simd { elem, lanes: len } => {
                let (elem, count) = (*elem, u32::try_from(len.to_usize()).ok()?);
                let lane = self.plain_layout_within(elem, visiting)?;
                Some(PlainLayout {
                    size: lane.size.checked_mul(count)?,
                    align: lane.align,
                    field_offsets: Vec::new(),
                })
            }
            TyKind::Tuple(items) => {
                let items = items.clone();
                self.plain_record_layout(&items, visiting)
            }
            TyKind::Adt { def, substs } => {
                let (def, substs) = (*def, substs.clone());
                if def.local >= u32::MAX - 16 || visiting.contains(&def) {
                    return None;
                }
                if let Some(members) = self.union_members(def, &substs) {
                    visiting.push(def);
                    let layout = self.union_layout_within(&members, visiting);
                    visiting.pop();
                    return layout;
                }
                let fields = self.adt_field_tys(def, &substs)?;
                visiting.push(def);
                let layout = self.plain_record_layout(fields, visiting);
                visiting.pop();
                layout
            }
            _ => None,
        }
    }

    /// The member types of `ffi::Union<M>` when `def` is that struct: the
    /// elements of the tuple `M`, or `M` alone.
    #[must_use]
    pub fn union_members(
        &self,
        def: gossamer_resolve::DefId,
        substs: &crate::Substs,
    ) -> Option<Vec<Ty>> {
        if self.def_name(def)? != UNION_TYPE_NAME {
            return None;
        }
        let members = *substs.types().first()?;
        Some(match self.kind_of(members) {
            TyKind::Tuple(items) => items.clone(),
            _ => vec![members],
        })
    }

    /// A union's layout: as large and as aligned as its largest and most
    /// aligned member, every member at offset 0.
    fn union_layout_within(
        &self,
        members: &[Ty],
        visiting: &mut Vec<gossamer_resolve::DefId>,
    ) -> Option<PlainLayout> {
        let mut size = 0u32;
        let mut align = 1u32;
        for member in members {
            let layout = self.plain_layout_within(*member, visiting)?;
            size = size.max(layout.size);
            align = align.max(layout.align);
        }
        Some(PlainLayout {
            size: size.next_multiple_of(align),
            align,
            field_offsets: vec![0],
        })
    }

    /// The field type of the `ffi::Union<M>` instantiation `(def, substs)`:
    /// its bytes, `[u8; size]`. `None` when `def` is not the union or a
    /// member has no C layout.
    pub fn union_bytes_ty(
        &mut self,
        def: gossamer_resolve::DefId,
        substs: &crate::Substs,
    ) -> Option<Ty> {
        let members = self.union_members(def, substs)?;
        let layout = self.union_layout_within(&members, &mut vec![def])?;
        let byte = self.int_ty(IntTy::U8);
        Some(self.intern(TyKind::Array {
            elem: byte,
            len: crate::ArrayLen::Concrete(layout.size as usize),
        }))
    }

    /// Fields laid out one after another at their own alignment.
    fn plain_record_layout(
        &self,
        fields: &[Ty],
        visiting: &mut Vec<gossamer_resolve::DefId>,
    ) -> Option<PlainLayout> {
        let mut offset = 0u32;
        let mut align = 1u32;
        let mut field_offsets = Vec::with_capacity(fields.len());
        for field in fields {
            let layout = self.plain_layout_within(*field, visiting)?;
            offset = offset.next_multiple_of(layout.align);
            field_offsets.push(offset);
            offset = offset.checked_add(layout.size)?;
            align = align.max(layout.align);
        }
        Some(PlainLayout {
            size: offset.next_multiple_of(align),
            align,
            field_offsets,
        })
    }

    /// Records that `def` keeps one word per field, because a program borrows
    /// one of its narrow fields.
    pub fn keep_word_fields(&mut self, def: gossamer_resolve::DefId) {
        self.word_field_structs.insert(def);
    }

    /// Bytes a scalar field occupies inside a packed struct, or `None` for a
    /// field that is not a scalar. An `f32` is held as a double on every tier,
    /// so it keeps a whole word.
    fn packed_scalar_bytes(&self, ty: Ty) -> Option<u32> {
        match self.kind_of(ty) {
            TyKind::Bool | TyKind::Int(IntTy::I8 | IntTy::U8) => Some(1),
            TyKind::Int(IntTy::I16 | IntTy::U16) => Some(2),
            TyKind::Char | TyKind::Int(IntTy::I32 | IntTy::U32) => Some(4),
            TyKind::Int(IntTy::I64 | IntTy::U64 | IntTy::Isize | IntTy::Usize)
            | TyKind::Float(_) => Some(8),
            _ => None,
        }
    }

    /// Whether `ty` is a non-generic struct whose fields are scalars and
    /// structs of scalars, the only shape a packed struct is made of.
    fn is_scalar_struct(&self, ty: Ty, depth: u32) -> bool {
        if depth > 16 {
            return false;
        }
        let TyKind::Adt { def, substs } = self.kind_of(ty) else {
            return false;
        };
        if def.local >= u32::MAX - 16
            || !substs.as_slice().is_empty()
            || self.is_inline_enum_ty(ty)
            || self.flat_enums.contains_key(&def.local)
        {
            return false;
        }
        self.struct_field_tys(*def).is_some_and(|fields| {
            fields.iter().all(|field| {
                self.packed_scalar_bytes(*field).is_some()
                    || self.is_scalar_struct(*field, depth + 1)
            })
        })
    }

    /// Walks the packed layout of `ty`, handing each field's offset and bytes
    /// to `on_field` in declaration order, and answers the struct's size, or
    /// `None` when the struct keeps one word per field.
    fn packed_walk(&self, ty: Ty, on_field: impl FnMut(u32, u32)) -> Option<u32> {
        let TyKind::Adt { def, substs } = self.kind_of(ty) else {
            return None;
        };
        if !substs.as_slice().is_empty() || self.is_inline_enum_ty(ty) {
            return None;
        }
        self.packed_walk_def(*def, on_field)
    }

    /// [`Self::packed_walk`] for the non-generic struct `def`.
    fn packed_walk_def(
        &self,
        def: gossamer_resolve::DefId,
        mut on_field: impl FnMut(u32, u32),
    ) -> Option<u32> {
        if def.local >= u32::MAX - 16 || self.word_field_structs.contains(&def) {
            return None;
        }
        let fields = self.struct_field_tys(def)?;
        if !fields.iter().all(|field| {
            self.packed_scalar_bytes(*field).is_some() || self.is_scalar_struct(*field, 1)
        }) {
            return None;
        }
        let mut offset = 0u32;
        let mut word_offset = 0u32;
        let mut changed = false;
        for field in fields {
            let (bytes, align) = match self.packed_scalar_bytes(*field) {
                Some(bytes) => (bytes, bytes),
                None => (self.slot_bytes(*field).max(8), 8),
            };
            offset = offset.next_multiple_of(align);
            changed |= offset != word_offset;
            on_field(offset, bytes);
            offset += bytes;
            word_offset += bytes.next_multiple_of(8);
        }
        let size = offset.next_multiple_of(8).max(8);
        changed |= size != word_offset.max(8);
        changed.then_some(size)
    }

    /// The layout of a struct whose fields narrower than a word sit at their
    /// own width, or `None` when the struct keeps one word per field.
    ///
    /// A non-generic struct of scalars and structs of scalars qualifies. A
    /// scalar field sits at its own size and alignment and a nested struct at
    /// a word boundary, and the whole rounds up to a word, so a copy, an
    /// element stride, and a slot count are still whole words. A struct this
    /// would not change, or one a program borrows a narrow field of, answers
    /// `None`.
    #[must_use]
    pub fn packed_layout(&self, ty: Ty) -> Option<PackedLayout> {
        let mut field_offsets = Vec::new();
        let mut field_bytes = Vec::new();
        let size = self.packed_walk(ty, |offset, bytes| {
            field_offsets.push(offset);
            field_bytes.push(bytes);
        })?;
        Some(PackedLayout {
            size,
            field_offsets,
            field_bytes,
        })
    }

    /// The packed layout of the non-generic struct `def`: see
    /// [`Self::packed_layout`].
    #[must_use]
    pub fn packed_struct_layout(&self, def: gossamer_resolve::DefId) -> Option<PackedLayout> {
        let mut field_offsets = Vec::new();
        let mut field_bytes = Vec::new();
        let size = self.packed_walk_def(def, |offset, bytes| {
            field_offsets.push(offset);
            field_bytes.push(bytes);
        })?;
        Some(PackedLayout {
            size,
            field_offsets,
            field_bytes,
        })
    }

    /// Every scalar leaf of a packed struct with its byte offset in the value,
    /// in field order, nested structs flattened.
    #[must_use]
    pub fn packed_leaves(&self, ty: Ty) -> Option<Vec<(u32, Ty)>> {
        self.packed_walk(ty, |_, _| {})?;
        let mut leaves = Vec::new();
        self.push_scalar_leaves(ty, 0, &mut leaves);
        Some(leaves)
    }

    fn push_scalar_leaves(&self, ty: Ty, base: u32, out: &mut Vec<(u32, Ty)>) {
        if self.packed_scalar_bytes(ty).is_some() {
            out.push((base, ty));
            return;
        }
        let TyKind::Adt { def, .. } = self.kind_of(ty) else {
            return;
        };
        let Some(fields) = self.struct_field_tys(*def) else {
            return;
        };
        let offsets: Vec<u32> = match self.packed_layout(ty) {
            Some(layout) => layout.field_offsets,
            None => fields
                .iter()
                .scan(0u32, |at, field| {
                    let here = *at;
                    *at += self.slot_bytes(*field).max(8);
                    Some(here)
                })
                .collect(),
        };
        for (field, offset) in fields.iter().zip(offsets) {
            self.push_scalar_leaves(*field, base + offset, out);
        }
    }

    /// Whether a value of this type is an inline aggregate of more than one
    /// slot whose every leaf is a scalar.
    #[must_use]
    pub fn is_scalar_inline_aggregate(&self, ty: Ty) -> bool {
        self.slot_bytes(ty) > 8
            && self.elem_is_addressed_aggregate(ty)
            && self.scalar_leaves_only(ty)
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
            TyKind::Array { elem, len } | TyKind::Simd { elem, lanes: len } => {
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
                if let Some(size) = self.packed_walk(ty, |_, _| {}) {
                    return size;
                }
                if let Some(field_tys) = self.adt_field_tys(*def, substs) {
                    let total_slots: u32 = field_tys
                        .iter()
                        .map(|t| self.slot_bytes(*t).max(8) / 8)
                        .sum();
                    // A plain struct of word-wide fields already sits at the
                    // offsets its natural layout gives it.
                    debug_assert!(
                        self.plain_layout(ty).is_none_or(|layout| {
                            layout.align != 8
                                || field_tys
                                    .iter()
                                    .any(|t| self.plain_layout(*t).is_none_or(|f| f.size != 8))
                                || layout.size == total_slots.max(1) * 8
                        }),
                        "slot layout and plain layout disagree for a word-field struct"
                    );
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

    /// Bytes one element of a `Vec<elem>` or slice occupies in its buffer
    /// when the element is not held inline as an aggregate: a `bool`, a
    /// narrow integer, or a `char` packs to its own width and every other
    /// such element is one word. `None` for a tuple, a fixed array, or an
    /// ADT, which occupies its slot bytes.
    ///
    /// The width a vector is built with. A vector a runtime shim builds may
    /// hold narrow elements as whole words, so a read takes the width from
    /// the vector's header.
    #[must_use]
    pub fn vec_scalar_elem_bytes(&self, elem: Ty) -> Option<u32> {
        match self.kind_of(elem) {
            TyKind::Bool | TyKind::Int(IntTy::I8 | IntTy::U8) => Some(1),
            TyKind::Int(IntTy::I16 | IntTy::U16) => Some(2),
            TyKind::Int(IntTy::I32 | IntTy::U32) | TyKind::Char | TyKind::Float(FloatTy::F32) => {
                Some(4)
            }
            TyKind::Tuple(_) | TyKind::Array { .. } | TyKind::Adt { .. } => None,
            _ => Some(8),
        }
    }

    /// Whether a vector of `elem` packs it as a four-byte `f32`, which a read
    /// widens to the `f64` compiled code carries an `f32` in.
    #[must_use]
    pub fn vec_packs_f32(&self, elem: Ty) -> bool {
        matches!(self.kind_of(elem), TyKind::Float(FloatTy::F32))
    }

    /// The narrow width and signedness of a vector element read as an
    /// integer word, for an element type a vector may pack: `None` for an
    /// element that is always a whole word or a `bool` or `u8` byte.
    #[must_use]
    pub fn vec_narrow_scalar(&self, elem: Ty) -> Option<(u32, bool)> {
        match self.kind_of(elem) {
            TyKind::Int(IntTy::I8) => Some((1, true)),
            TyKind::Int(IntTy::I16) => Some((2, true)),
            TyKind::Int(IntTy::U16) => Some((2, false)),
            TyKind::Int(IntTy::I32) => Some((4, true)),
            TyKind::Int(IntTy::U32) | TyKind::Char => Some((4, false)),
            _ => None,
        }
    }

    /// The element-kind tag (see [`crate::vec_elem_kind`]) a vector of
    /// `elem` carries in its header, naming what one element owns.
    #[must_use]
    pub fn vec_elem_kind(&self, elem: Ty) -> u8 {
        use crate::vec_elem_kind as kind;
        match self.kind_of(elem) {
            TyKind::String => kind::STRING,
            TyKind::Vec(_) | TyKind::Slice(_) | TyKind::Iterator(_) => kind::VEC,
            TyKind::HashMap { .. } => kind::MAP,
            TyKind::DynError => kind::ERROR,
            // A `json::Value` element is a handle holding a share of the
            // document it views, so the vector's teardown gives each back.
            TyKind::JsonValue => kind::JSON,
            // A single-slot struct, tuple, or fixed array is held inline
            // and its slot address is the value; the store has to know,
            // since a one-word scalar element is the value itself.
            _ if self.is_flat_inline_aggregate(elem) => kind::AGGR_FLAT,
            TyKind::Int(IntTy::I8 | IntTy::I16 | IntTy::I32) => kind::SIGNED,
            TyKind::Float(FloatTy::F32) => kind::F32,
            _ => kind::PRIMITIVE,
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

    /// Records the `Param` type the generic type parameter `def` stands for.
    pub fn register_type_param_def(&mut self, def: gossamer_resolve::DefId, param: Ty) {
        self.type_param_defs.insert(def, param);
    }

    /// The `Param` type the generic type parameter `def` stands for, when the
    /// checker recorded one.
    #[must_use]
    pub fn type_param_of_def(&self, def: gossamer_resolve::DefId) -> Option<Ty> {
        self.type_param_defs.get(&def).copied()
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

    /// Whether binding an enum payload of type `ty` copies it out of a heap box,
    /// taking a share of each counted field: an aggregate wider than one slot,
    /// or one registered as boxed.
    #[must_use]
    pub fn is_boxed_payload_binding(&self, ty: Ty) -> bool {
        let aggregate = match self.kind_of(ty) {
            TyKind::Tuple(_) | TyKind::Array { .. } => true,
            TyKind::Adt { def, .. } => self.struct_field_tys(*def).is_some(),
            _ => false,
        };
        aggregate && (self.slot_bytes(ty) > 8 || self.is_boxed_enum_payload(ty))
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

    /// Keeps every user enum lowered after this call in its heap node, the
    /// representation the bytecode VM's enum values share with the JIT.
    pub fn keep_enums_on_heap(&mut self) {
        self.enums_on_heap = true;
    }

    /// Whether user enums may be stored by value; see [`Self::keep_enums_on_heap`].
    #[must_use]
    pub fn enums_by_value(&self) -> bool {
        !self.enums_on_heap
    }

    /// Registers a user enum (by `DefId.local`) as inline-able - its values
    /// are the 2-word by-value `i128` representation.
    pub fn register_inline_enum_def(&mut self, def_local: u32) {
        self.inline_enum_defs.insert(def_local);
    }

    /// Registers `def` as an enum stored by value with `layout`: its values
    /// are a struct of the discriminant and the layout's regions, so every
    /// layout and ownership walk reads them as that struct.
    pub fn register_flat_enum(&mut self, def: gossamer_resolve::DefId, layout: FlatEnumLayout) {
        let tag = self.int_ty(IntTy::I64);
        self.struct_fields.insert(def, layout.struct_fields(tag));
        self.word_field_structs.insert(def);
        self.rc_managed_enum_defs.remove(&def.local);
        self.flat_enums.insert(def.local, layout);
    }

    /// Every type a map or set in the program is keyed by.
    #[must_use]
    pub fn keyed_tys(&self) -> Vec<Ty> {
        let mut keys = Vec::new();
        for kind in &self.kinds {
            match kind {
                TyKind::HashMap { key, .. } => keys.push(*key),
                TyKind::Adt { def, substs }
                    if def.local >= u32::MAX - 64
                        && matches!(self.def_name(*def), Some("Set" | "BTreeSet" | "BTreeMap")) =>
                {
                    keys.extend(substs.types().first().copied());
                }
                _ => {}
            }
        }
        keys
    }

    /// The by-value layout of `ty`, when it is an enum stored by value.
    #[must_use]
    pub fn flat_enum_layout(&self, ty: Ty) -> Option<&FlatEnumLayout> {
        match self.kind(ty) {
            Some(TyKind::Adt { def, .. }) => self.flat_enums.get(&def.local),
            _ => None,
        }
    }

    /// The tuple a body holds the carrier `carrier` as, when its payload is
    /// a by-value enum: the discriminant, then the `Ok` / `Some` payload,
    /// then (for a `Result`) the `Err` payload.
    pub fn flat_carrier_ty(&mut self, carrier: Ty) -> Option<Ty> {
        let TyKind::Adt { def, substs } = self.kind(carrier)?.clone() else {
            return None;
        };
        if def.local != u32::MAX && def.local != u32::MAX - 1 {
            return None;
        }
        let payloads = substs.types();
        let arms = if def.local == u32::MAX { 2 } else { 1 };
        if payloads.len() < arms
            || !payloads[..arms]
                .iter()
                .any(|p| self.is_flat_enum_ty(self.peel_nominal(*p)))
        {
            return None;
        }
        let tag = self.int_ty(IntTy::I64);
        let mut fields = vec![tag];
        fields.extend(payloads[..arms].iter().map(|p| self.peel_nominal(*p)));
        let tuple = self.intern(TyKind::Tuple(fields));
        self.flat_carriers.insert(tuple, carrier);
        Some(tuple)
    }

    /// The carrier type a flattened carrier tuple stands for.
    #[must_use]
    pub fn carrier_of_flat(&self, tuple: Ty) -> Option<Ty> {
        self.flat_carriers.get(&tuple).copied()
    }

    /// Whether the user enum `def_local` names is stored by value.
    #[must_use]
    pub fn is_flat_enum_def(&self, def_local: u32) -> bool {
        self.flat_enums.contains_key(&def_local)
    }

    /// Whether `ty` is an enum stored by value as a tag and shared regions.
    #[must_use]
    pub fn is_flat_enum_ty(&self, ty: Ty) -> bool {
        self.flat_enum_layout(ty).is_some()
    }

    /// Whether the user enum `def_local` names is an inline-able one.
    #[must_use]
    pub fn is_inline_enum_def(&self, def_local: u32) -> bool {
        self.inline_enum_defs.contains(&def_local)
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
        // Opaque runtime-handle / heap-blob stdlib structs -
        // `http::ResponseStream` (`u32::MAX - 4`) and `http::Response`
        // (`- 5`). Each is a plain `Box`
        // handle with no RC header, so `gos_rt_rc_release` on the whole
        // local reads a non-existent header and corrupts the heap. They are
        // never reference-counted: handle locals leak (process-teardown
        // reclaim) exactly like `http::Client` / SQL handles. (`u32::MAX`
        // Result / `- 1` Option already returned above; `- 6` Weak stays
        // RC-managed via the weak helpers.)
        if matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. })
                if (u32::MAX - 5..=u32::MAX - 4).contains(&def.local)
        ) {
            return false;
        }
        // `signal::Notifier` (`u32::MAX - 17`) is a bare i64 handle with no
        // RC header, never reference-counted, like the opaque handles above.
        if matches!(
            self.kind(ty),
            Some(TyKind::Adt { def, .. }) if def.local == u32::MAX - 17
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
        if self.is_inline_enum_ty(ty) || self.is_flat_enum_ty(ty) {
            return false;
        }
        if matches!(self.kind(ty), Some(TyKind::String)) {
            return true;
        }
        // An `errors::Error` is a reference-counted cell holding its message,
        // a share of its cause, and its fields.
        if matches!(self.kind(ty), Some(TyKind::DynError)) {
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
        if self.is_managed_handle(ty) {
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

    /// Returns true when `ty` is a stdlib handle the runtime allocates as a
    /// counted node and frees with its last share: a channel's `Sender` and
    /// `Receiver`, a `JoinHandle`, `context::Context` (sentinel offset 11),
    /// the sockets `net::TcpStream` through `net::UnixListener` (12 to 16),
    /// `bytes::Builder` (27), `sync::Map` (34),
    /// `sync::RwLock` (35), `rand::Rng` (42), `fs::File` (44),
    /// `fs::OpenOptions` (45), `sync::Shared` (46), the band from
    /// `sync::Mutex` (50) through `sync::AtomicBool` (57), and
    /// `http::websocket::Conn` (62), `bytes::Buffer` (63), and
    /// `io::MemoryStream` (64).
    #[must_use]
    pub fn is_managed_handle(&self, ty: Ty) -> bool {
        match self.kind(ty) {
            Some(TyKind::Sender(_) | TyKind::Receiver(_) | TyKind::JoinHandle(_)) => true,
            Some(TyKind::Adt { def, .. }) => {
                matches!(
                    u32::MAX - def.local,
                    11..=16 | 27 | 34 | 35 | 42 | 44..=46 | 50..=57 | 62..=64
                )
            }
            _ => false,
        }
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

    /// Whether a value of `ty` is one word pointing at a counted heap node that
    /// `gos_rt_rc_retain` / `gos_rt_rc_release` manage: a payload-bearing enum,
    /// a `std::sync` handle, an `errors::Error` cell, or a callable value's capture
    /// environment (null
    /// for a callable that captures nothing, which both calls treat as a
    /// no-op).
    #[must_use]
    pub fn is_counted_node(&self, ty: Ty) -> bool {
        self.is_payload_enum(ty)
            || self.is_managed_handle(ty)
            || matches!(
                self.kind(ty),
                Some(TyKind::FnTrait(_) | TyKind::Closure { .. } | TyKind::DynError)
            )
    }
}
