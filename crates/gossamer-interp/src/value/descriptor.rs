//! Render, ordering, and JSON descriptors: the static type facts a value-only renderer needs.

use super::uint_desc;

/// The render descriptor for `ty`, or `None` when every value of that
/// type renders the way it always has.
///
/// The descriptor is what carries a static type into a renderer that
/// only sees values: a `Vec` and a fixed array share one runtime
/// representation, and a `u64` and an `i64` share one slot, so the
/// spelling and the signedness are knowable only from here. One
/// builder serves the bytecode compiler's format sites and the REPL,
/// so a value reads the same in both.
#[must_use]
pub fn render_descriptor(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> Option<String> {
    descriptor_of(tcx, ty, false)
}

/// The ordering descriptor for `ty`: where the type declared an integer `u64`
/// / `usize`, whose bits order unsigned, at any depth - sequence elements,
/// tuple and struct fields, carrier payloads. `None` when it declared none,
/// so every value of the type orders as the signed words the VM compares.
///
/// The shape is the render descriptor's: [`uint_leaves`](super::uint_leaves) walks a value
/// alongside it, and the re-boxed copy is what an ordering compares.
#[must_use]
pub fn ordering_descriptor(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> Option<String> {
    let mut out = Vec::new();
    push_desc(tcx, ty, &mut out, 0, Walk::ORDERING, &[]);
    out.contains(&uint_desc::UINT)
        .then(|| out.iter().map(|b| *b as char).collect())
}

/// [`render_descriptor`] that also describes a struct's fields, for a
/// value the REPL renders from the value alone rather than through the
/// `to_string` a program's format site calls.
#[must_use]
pub fn repl_render_descriptor(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<String> {
    descriptor_of(tcx, ty, true)
}

/// [`render_descriptor`] for a value encoded as JSON text. The encoder reads
/// a struct's fields from the value rather than through a synthesized
/// `to_string`, so every field is described, as the REPL describes them.
#[must_use]
pub fn json_descriptor(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> Option<String> {
    descriptor_of(tcx, ty, true)
}

fn descriptor_of(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    adts: bool,
) -> Option<String> {
    let mut out = Vec::new();
    let walk = if adts { Walk::REPL } else { Walk::RENDER };
    push_desc(tcx, ty, &mut out, 0, walk, &[]);
    out.iter()
        .any(|b| describes_something(*b))
        .then(|| out.iter().map(|b| *b as char).collect())
}

/// [`render_descriptor`] for a sequence whose ELEMENTS are rendered
/// while the sequence itself is not - what `xs.join(sep)` builds, since
/// the separator replaces the brackets. The outer `Vec` tag becomes the
/// bare-sequence one; every nested descriptor is unchanged.
#[must_use]
pub fn element_render_descriptor(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
) -> Option<String> {
    let mut out = Vec::new();
    push_render_desc(tcx, ty, &mut out, 0);
    if out.first() != Some(&uint_desc::VEC) {
        return None;
    }
    out[0] = uint_desc::SEQ;
    out.iter()
        .any(|b| describes_something(*b))
        .then(|| out.iter().map(|b| *b as char).collect())
}

/// `ty` with every reference peeled.
fn peel_refs(tcx: &gossamer_types::TyCtxt, mut ty: gossamer_types::Ty) -> gossamer_types::Ty {
    while let Some(gossamer_types::TyKind::Ref { inner, .. }) = tcx.kind(ty) {
        ty = *inner;
    }
    ty
}

fn is_unsigned64(tcx: &gossamer_types::TyCtxt, ty: gossamer_types::Ty) -> bool {
    matches!(
        tcx.kind(peel_refs(tcx, ty)),
        Some(gossamer_types::TyKind::Int(
            gossamer_types::IntTy::U64 | gossamer_types::IntTy::Usize
        ))
    )
}

/// A descriptor byte that makes the descriptor worth carrying: a value of a
/// type describing none of these renders exactly as it always has.
fn describes_something(byte: u8) -> bool {
    matches!(
        byte,
        uint_desc::UINT | uint_desc::SET | uint_desc::VEC | uint_desc::F32
    )
}

/// What a descriptor walk is for.
#[derive(Clone, Copy)]
struct Walk {
    /// Describes every struct's fields, for a value rendered from the value
    /// alone rather than through its synthesized `to_string`.
    adts: bool,
    /// Marks `f32` leaves, which only rendering distinguishes: an ordering
    /// compares an `f32` as the float its slot holds.
    f32s: bool,
}

impl Walk {
    const RENDER: Self = Self {
        adts: false,
        f32s: true,
    };
    const REPL: Self = Self {
        adts: true,
        f32s: true,
    };
    const ORDERING: Self = Self {
        adts: true,
        f32s: false,
    };
}

fn push_render_desc(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    out: &mut Vec<u8>,
    depth: u8,
) {
    push_desc(tcx, ty, out, depth, Walk::RENDER, &[]);
}

fn push_desc(
    tcx: &gossamer_types::TyCtxt,
    ty: gossamer_types::Ty,
    out: &mut Vec<u8>,
    depth: u8,
    walk: Walk,
    params: &[gossamer_types::Ty],
) {
    use gossamer_types::TyKind;
    if depth > 8 {
        out.push(uint_desc::NONE);
        return;
    }
    let peeled = peel_refs(tcx, ty);
    if is_unsigned64(tcx, peeled) {
        out.push(uint_desc::UINT);
        return;
    }
    if walk.f32s
        && matches!(
            tcx.kind(peeled),
            Some(TyKind::Float(gossamer_types::FloatTy::F32))
        )
    {
        out.push(uint_desc::F32);
        return;
    }
    match tcx.kind(peeled) {
        Some(TyKind::Param { idx, .. }) => match params.get(idx.0 as usize) {
            Some(arg) => push_desc(tcx, *arg, out, depth + 1, walk, &[]),
            None => out.push(uint_desc::NONE),
        },
        // A `Vec` renders in its own spelling; a fixed array and a slice
        // are written in bare brackets and render that way.
        Some(TyKind::Vec(elem)) => {
            let elem = *elem;
            out.push(uint_desc::VEC);
            push_desc(tcx, elem, out, depth + 1, walk, params);
        }
        Some(TyKind::Slice(elem) | TyKind::Array { elem, .. }) => {
            let elem = *elem;
            out.push(uint_desc::SEQ);
            push_desc(tcx, elem, out, depth + 1, walk, params);
        }
        Some(TyKind::Tuple(elems)) => {
            let elems = elems.clone();
            let Ok(arity) = u8::try_from(elems.len()) else {
                out.push(uint_desc::NONE);
                return;
            };
            out.push(uint_desc::TUPLE);
            out.push(arity);
            for elem in elems {
                push_desc(tcx, elem, out, depth + 1, walk, params);
            }
        }
        Some(TyKind::HashMap { key, value, .. }) => {
            let (key, value) = (*key, *value);
            out.push(uint_desc::MAP);
            push_desc(tcx, key, out, depth + 1, walk, params);
            push_desc(tcx, value, out, depth + 1, walk, params);
        }
        // `Option` and `Result` are the sentinel Adts `u32::MAX - 1` and
        // `u32::MAX`; a `Set` / `BTreeSet` is `u32::MAX - 7` / `- 18`.
        Some(TyKind::Adt { def, substs }) if def.local == u32::MAX - 1 => {
            let payload = substs.types().first().copied();
            out.push(uint_desc::OPTION);
            match payload {
                Some(payload) => push_desc(tcx, payload, out, depth + 1, walk, params),
                None => out.push(uint_desc::NONE),
            }
        }
        Some(TyKind::Adt { def, substs }) if def.local == u32::MAX => {
            let tys = substs.types();
            let (ok, err) = (tys.first().copied(), tys.get(1).copied());
            out.push(uint_desc::RESULT);
            for arm in [ok, err] {
                match arm {
                    Some(arm) => push_desc(tcx, arm, out, depth + 1, walk, params),
                    None => out.push(uint_desc::NONE),
                }
            }
        }
        // `Deque` (-19), `MaxHeap` (-28), `MinHeap` (-30), `Queue` (-31),
        // and `Stack` (-32) keep their elements in a runtime registry, so
        // the descriptor travels on the handle.
        Some(TyKind::Adt { def, substs })
            if matches!(u32::MAX - def.local, 19 | 28 | 30 | 31 | 32) =>
        {
            let elem = substs.types().first().copied();
            out.push(uint_desc::CONTAINER);
            match elem {
                Some(elem) => push_desc(tcx, elem, out, depth + 1, walk, params),
                None => out.push(uint_desc::NONE),
            }
        }
        Some(TyKind::Adt { def, substs })
            if def.local == u32::MAX - 7 || def.local == u32::MAX - 18 =>
        {
            let elem = substs.types().first().copied();
            push_set_render_desc(tcx, elem, out, depth, walk, params);
        }
        // A program renders a struct through the `to_string` synthesized for
        // its type, which describes each field at the format site inside it.
        // A field declared with a type parameter is the exception: that site
        // sees only the parameter, so the instantiated type is described here,
        // where the concrete type is known. The REPL describes every field,
        // since it renders from the value alone.
        Some(TyKind::Adt { def, substs }) if def.local < u32::MAX - 16 => {
            let (def, substs) = (*def, substs.clone());
            push_struct_render_desc(tcx, def, &substs, out, depth, walk, params);
        }
        _ => out.push(uint_desc::NONE),
    }
}

/// The descriptor of a `Set` / `BTreeSet` whose element is `elem`: the set tag
/// and the element's own descriptor when that element describes anything, and
/// nothing otherwise.
fn push_set_render_desc(
    tcx: &gossamer_types::TyCtxt,
    elem: Option<gossamer_types::Ty>,
    out: &mut Vec<u8>,
    depth: u8,
    walk: Walk,
    params: &[gossamer_types::Ty],
) {
    let mut elem_desc = Vec::new();
    match elem {
        Some(elem) => push_desc(tcx, elem, &mut elem_desc, depth + 1, walk, params),
        None => elem_desc.push(uint_desc::NONE),
    }
    if elem_desc.iter().any(|b| *b != uint_desc::NONE) {
        out.push(uint_desc::SET);
        out.extend(elem_desc);
    } else {
        out.push(uint_desc::NONE);
    }
}

/// The descriptor of the user struct `def` instantiated with `substs`: every
/// field for the REPL, and otherwise only the fields declared with a type
/// parameter.
fn push_struct_render_desc(
    tcx: &gossamer_types::TyCtxt,
    def: gossamer_resolve::DefId,
    substs: &gossamer_types::Substs,
    out: &mut Vec<u8>,
    depth: u8,
    walk: Walk,
    params: &[gossamer_types::Ty],
) {
    use gossamer_types::TyKind;
    // A field's declared type names the struct's own parameters, so it reads
    // them from this instantiation's arguments, each already resolved in the
    // enclosing one.
    let args: Vec<gossamer_types::Ty> = substs
        .types()
        .iter()
        .map(|arg| match tcx.kind(*arg) {
            Some(TyKind::Param { idx, .. }) => params.get(idx.0 as usize).copied().unwrap_or(*arg),
            _ => *arg,
        })
        .collect();
    let declared = tcx.struct_field_tys(def).map(<[_]>::to_vec);
    let Some(fields) = tcx.adt_field_tys(def, substs).map(<[_]>::to_vec) else {
        out.push(uint_desc::NONE);
        return;
    };
    if !walk.adts && substs.is_empty() {
        out.push(uint_desc::NONE);
        return;
    }
    let Ok(count) = u8::try_from(fields.len()) else {
        out.push(uint_desc::NONE);
        return;
    };
    out.push(uint_desc::ADT);
    out.push(count);
    for (index, field) in fields.into_iter().enumerate() {
        let generic = declared
            .as_ref()
            .and_then(|declared| declared.get(index))
            .is_some_and(|declared| crate::compile::mentions_param(tcx, *declared));
        if walk.adts || generic {
            push_desc(tcx, field, out, depth + 1, walk, &args);
        } else {
            out.push(uint_desc::NONE);
        }
    }
}
