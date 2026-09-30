//! `Result` and `Option` carriers: the packed two-word value, construction, unwrapping, payload ownership, and debug rendering.

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::items_after_statements)]
#![allow(clippy::same_length_and_capacity)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_ptr_alignment)]
#![allow(clippy::ptr_as_ptr)]
#![allow(static_mut_refs)]
#![allow(clippy::wildcard_imports)]

use super::*;

// Tagged-union encoding for `Result<T, E>` and `Option<T>`: a 2-word
// BY-VALUE `i128` with the discriminant in the low 64 bits and the
// payload in the high 64 bits. Convention: `disc == 0` = Ok / Some,
// `disc == 1` = Err / None - the distinguishing bit pattern dispatch
// reads. Construction is a register pack with zero allocation; the
// payload flows as a normal value (a scalar, or a pointer to a
// heap-copied aggregate) managed by RC like any other binding.

/// Pack `(disc, payload)` into the 2-word Result/Option value.
#[inline]
#[must_use]
pub fn pack_result(disc: i64, payload: i64) -> i128 {
    (((payload as u64 as u128) << 64) | (disc as u64 as u128)) as i128
}

#[inline]
pub(crate) fn result_disc_of(r: i128) -> i64 {
    (r as u128 as u64) as i64
}

#[inline]
pub(crate) fn result_payload_of(r: i128) -> i64 {
    ((r as u128 >> 64) as u64) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_new(disc: i64, payload: i64) -> i128 {
    pack_result(disc, payload)
}

/// `gos_rt_result_new` for a carrier whose aggregate payload took the share
/// its source was holding.
///
/// The packing is the same two words; the spelling is what tells a back end
/// to box the payload by taking those words rather than minting a second
/// share of the children they name. A back end that boxes before the call
/// reaches this entry unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_new_owned(disc: i64, payload: i64) -> i128 {
    pack_result(disc, payload)
}

/// `gos_rt_result_new` variant for f64 payloads - stores the value's
/// `to_bits()` so the symmetric `gos_rt_result_payload_f64` reads back the
/// original f64.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_new_f64(disc: i64, payload: f64) -> i128 {
    pack_result(disc, payload.to_bits() as i64)
}

/// Converts a `[rust-bindings]` `*mut GosVariant` (the binding ABI's
/// `Result` / `Option` wire shape, tag `1` = Ok/Some) into the
/// runtime's packed i128 result (disc `0` = Ok/Some). String payloads
/// arrive as bare NUL-terminated arena bytes and are re-allocated as
/// header'd runtime strings; every other payload word passes through
/// bit-exact (i64/bool/char values, f64 bits, GosVec / nested
/// pointers).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_binding_variant_to_result(p: *const u8) -> i128 {
    ffi_entry!(0i128, {
        if p.is_null() {
            return pack_result(1, 0);
        }
        // GosVariant layout (repr(C) in gossamer-binding):
        // tag i32 | payload_len i32 | payload *mut GosVariantValue.
        // SAFETY: `p` is this shim's `u8` argument, non-null (checked above), live for the call
        // (C-ABI contract).
        let tag = unsafe { *p.cast::<i32>() };
        // SAFETY: `p` is this shim's `u8` argument, non-null (checked above), live for the call
        // (C-ABI contract).
        let payload_len = unsafe { *p.add(4).cast::<i32>() };
        // SAFETY: `p` is this shim's `u8` argument, non-null (checked above), live for the call
        // (C-ABI contract).
        let payload_ptr = unsafe { *p.add(8).cast::<*const u8>() };
        let disc = i64::from(tag != 1);
        if payload_len <= 0 || payload_ptr.is_null() {
            return pack_result(disc, 0);
        }
        // GosVariantValue layout: tag i32 | (pad) | data union at +8.
        // Value tag 4 = string; see `gossamer-binding::native`.
        // SAFETY: `payload_ptr` is the non-null variant payload, whose tag word is its first four
        // bytes.
        let value_tag = unsafe { *payload_ptr.cast::<i32>() };
        // SAFETY: the variant payload holds its data word at offset 8.
        let word = unsafe { *payload_ptr.add(8).cast::<i64>() };
        let payload = if value_tag == 4 && word != 0 {
            // HOST-CSTRING: a native Rust binding owns this pointer and
            // publishes it as a NUL-terminated C string, not a Gossamer
            // `String`, so it carries no length header.
            // SAFETY: a non-zero string payload is a NUL-terminated C string the binding owns.
            let c = unsafe { std::ffi::CStr::from_ptr(word as *const std::ffi::c_char) };
            super::string::alloc_cstring(c.to_bytes()) as i64
        } else {
            word
        };
        pack_result(disc, payload)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_disc(r: i128) -> i64 {
    result_disc_of(r)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_dbg(p: i64) -> i64 {
    eprintln!("[rt] dbg called with raw i64 = {p:#x}");
    p
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_payload(r: i128) -> i64 {
    result_payload_of(r)
}

/// `Result<f64, _>` / `Option<f64>` Ok-payload extractor that reinterprets
/// the stored bits as f64.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_payload_f64(r: i128) -> f64 {
    f64::from_bits(result_payload_of(r) as u64)
}

/// Payload extractor for payloads that are themselves a 2-word
/// by-value enum (`Result<Option<T>, E>`, nested Results, inline
/// user enums). Construction heap-copied the inner 2-word value and
/// stored its address in the payload word; this loads it back by
/// value so the destination local holds `[disc, payload]` directly.
///
/// # Safety
///
/// The payload of `r` addresses the live two-word copy its construction made.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_payload_i128(r: i128) -> i128 {
    let addr = result_payload_of(r);
    if addr == 0 {
        return 0;
    }
    // SAFETY: the payload word of an enum-payload Result/Option is a
    // pointer to the live 16-byte heap copy made at construction
    // (`gos_rt_result_new` aggregate path).
    unsafe {
        let p = addr as usize as *const i64;
        let lo = (*p) as u64 as u128;
        let hi = (*p.add(1)) as u64 as u128;
        ((hi << 64) | lo) as i128
    }
}

/// Signature of a derived `Type::fmt`: it reads the value's flat slot buffer
/// and returns a freshly allocated runtime String the caller owns.
type AdtFmt = unsafe extern "C" fn(*const u8) -> *mut std::ffi::c_char;

/// Renders one aggregate by calling the derived `fmt` at `fmt`, taking
/// ownership of the String it returns. `value` carries whatever that `fmt`
/// receives as its receiver: a struct's slot address, or an inline enum's
/// own word - which may be zero for a unit variant, so it is not guarded.
pub(crate) unsafe fn adt_fmt_string(value: *const u8, fmt: *const std::ffi::c_void) -> String {
    if fmt.is_null() {
        return String::new();
    }
    // SAFETY: callers pass the address of a derived `Type::fmt`, emitted with
    // the `ptr(ptr)` signature `AdtFmt` names, and `value` is the receiver
    // that `fmt` expects for its type.
    let f: AdtFmt = unsafe { std::mem::transmute::<*const std::ffi::c_void, AdtFmt>(fmt) };
    // SAFETY: `f` is the formatter this `unsafe fn`'s contract names, and `value` its receiver.
    unsafe { take_rt_string(f(value)) }
}

/// Renders one enum payload word, extending [`debug_payload_string`] with the
/// aggregate tag: the word is then the address of the payload's slot buffer
/// and `fmt` its derived formatter.
///
/// # Safety
///
/// `payload` is a live value of the shape its descriptor names.
unsafe fn debug_payload_string_with(
    payload: i64,
    kind: i64,
    fmt: *const std::ffi::c_void,
) -> String {
    if kind == i64::from(gossamer_abi::DEBUG_PAYLOAD_ADT) {
        let slots: *const u8 = std::ptr::with_exposed_provenance(payload as usize);
        // SAFETY: `slots` is the aggregate payload and `fmt` its formatter (this `unsafe fn`'s
        // contract).
        return unsafe { adt_fmt_string(slots, fmt) };
    }
    // A tuple payload is its slot buffer, and `fmt` addresses a tag stream
    // that opens with the nested marker and the tuple's arity.
    if kind == i64::from(gossamer_abi::DEBUG_PAYLOAD_TUPLE) {
        let slots: *const i64 = std::ptr::with_exposed_provenance(payload as usize);
        let tags: *const u8 = fmt.cast();
        if slots.is_null() || tags.is_null() {
            return String::new();
        }
        // SAFETY: `tags` is non-null (checked above), a tuple descriptor whose second byte is its
        // arity.
        let arity = unsafe { *tags.add(1) } as usize;
        let mut out = String::new();
        let mut slot_cursor = 0usize;
        let mut tag_cursor = 2usize;
        // SAFETY: `slots` holds the tuple, laid out as `tags` describes.
        unsafe {
            crate::c_abi::desc_format::render_tuple_elements(
                &mut out,
                slots,
                crate::c_abi::desc_format::DescStream::bare(tags),
                arity,
                &mut slot_cursor,
                &mut tag_cursor,
            );
        }
        return out;
    }
    // A descriptor payload renders through the recursive walk, so a nested
    // container needs no formatter of its own.
    if kind == i64::from(gossamer_abi::DEBUG_PAYLOAD_DESC) {
        let tags: *const u8 = fmt.cast();
        if tags.is_null() {
            return String::new();
        }
        // SAFETY: `tags` is a non-null descriptor block (checked above).
        let tags = unsafe { crate::c_abi::desc_format::DescStream::new(tags) };
        let mut out = String::new();
        let mut cursor = 0usize;
        let slot = std::ptr::from_ref(&payload).cast::<u8>();
        // SAFETY: `slot` addresses the payload word, laid out as the descriptor describes.
        unsafe {
            crate::c_abi::desc_format::render_desc_storage(
                &mut out,
                slot,
                tags,
                &mut cursor,
                crate::c_abi::desc_format::Storage::ByWord,
            );
        }
        return out;
    }
    // SAFETY: this function's contract covers `payload`.
    unsafe { debug_payload_string(payload, kind) }
}

/// Renders a single enum payload word for `{:?}` Debug output, as the VM
/// renders a nested value: a `char` and a `String` in the spelling that builds
/// them. `kind`: 0=i64, 1=u64, 2=f64 (bit pattern), 3=bool, 4=char,
/// 5=String pointer.
///
/// # Safety
///
/// `payload` is a live value of the shape its descriptor names.
unsafe fn debug_payload_string(payload: i64, kind: i64) -> String {
    match kind {
        1 => (payload as u64).to_string(),
        2 => crate::builtins::format_float_debug(f64::from_bits(payload as u64)),
        k if k == i64::from(gossamer_abi::TUPLE_TAG_F32) => {
            crate::builtins::format_f32_debug(f64::from_bits(payload as u64))
        }
        3 => if payload != 0 { "true" } else { "false" }.to_string(),
        4 => {
            let mut out = String::new();
            crate::c_abi::desc_format::push_quoted_char(&mut out, payload);
            out
        }
        5 => {
            if payload == 0 {
                String::new()
            } else {
                let sptr: *const std::ffi::c_char =
                    std::ptr::with_exposed_provenance(payload as usize);
                let mut out = String::new();
                // SAFETY: a string payload is a live string body.
                super::desc_format::push_quoted_str(&mut out, &unsafe {
                    crate::c_abi::gos_str_arg_string(sptr)
                });
                out
            }
        }
        // A collection payload arrives as its `GosVec` pointer, so the
        // element formatter that renders a bare `{:?}` of that vec renders
        // it inside the variant too.
        // SAFETY: a payload of kind 6 is a live `Vec<i64>` or null, which the formatter accepts.
        6 => unsafe { take_rt_string(super::btmap::gos_rt_vec_format_i64(vec_ptr(payload), 0)) },
        // SAFETY: a payload of kind 7 is a live `Vec<String>` or null, which the formatter
        // accepts.
        7 => unsafe { take_rt_string(super::btmap::gos_rt_vec_format_string(vec_ptr(payload), 0)) },
        // SAFETY: a payload of kind 8 is a live `Vec<f64>` or null, which the formatter accepts.
        8 => unsafe {
            take_rt_string(crate::c_abi::gos_rt_json_debug(
                std::ptr::with_exposed_provenance(payload as usize),
            ))
        },
        // An error payload renders as the colon-joined cause chain, the way
        // a bare `{}` on the error does.
        // SAFETY: a payload of kind 10 is a live error or null, which the display accepts.
        10 => unsafe {
            take_rt_string(crate::c_abi::gos_rt_error_display(
                std::ptr::with_exposed_provenance(payload as usize),
            ))
        },
        // A unit payload carries no value: the arm renders as `()`.
        13 => "()".to_string(),
        // A `dyn::Value` payload arrives as its runtime handle; render it
        // through the DynValue debug renderer, which quotes string payloads
        // the way the VM's Debug output does.
        // SAFETY: a payload of kind 14 is a live `DynValue` handle or null, which the renderer
        // accepts.
        14 => unsafe {
            take_rt_string(crate::c_abi::gos_rt_dyn_format(
                std::ptr::with_exposed_provenance(payload as usize),
            ))
        },
        _ => payload.to_string(),
    }
}

/// Reinterprets a payload slot as the `GosVec` pointer it holds.
///
/// # Safety
///
/// A non-zero `payload` is a live `Vec` handle.
unsafe fn vec_ptr(payload: i64) -> *const crate::c_abi::GosVec {
    std::ptr::with_exposed_provenance(payload as usize)
}

/// Consumes a runtime-allocated C string into an owned `String`, freeing the
/// allocation the formatter handed back.
unsafe fn take_rt_string(ptr: *mut std::ffi::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: this `unsafe fn`'s caller passes `ptr` live; non-null, checked above.
    let out = unsafe { crate::c_abi::gos_str_arg_string(ptr) };
    // SAFETY: `ptr` is the fresh string this call took ownership of.
    unsafe { super::string::gos_rt_str_free(ptr) };
    out
}

/// `{:?}` of an `Option<T>` (the by-value `i128` enum, disc 0 = Some): renders
/// `Some(<payload>)` or `None`, matching the VM. `payload_kind` selects the
/// payload formatter (see `debug_payload_string`).
///
/// # Safety
///
/// A `Some` payload of `opt` is a live value of the shape `payload_kind` names.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_debug_option(
    opt: i128,
    payload_kind: i64,
) -> *mut std::ffi::c_char {
    let s = if result_disc_of(opt) != 0 {
        "None".to_string()
    } else {
        // SAFETY: this function's contract covers the payload.
        let payload = unsafe { debug_payload_string(result_payload_of(opt), payload_kind) };
        format!("Some({payload})")
    };
    super::string::alloc_cstring(s.as_bytes())
}

/// `{:?}` of a `Result<T, E>` (the by-value `i128` enum, disc 0 = Ok): renders
/// `Ok(<payload>)` or `Err(<payload>)`, matching the VM. `ok_kind` / `err_kind`
/// select the per-arm payload formatter.
///
/// # Safety
///
/// The payload of `res` is a live value of the shape its arm's kind names.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_debug_result(
    res: i128,
    ok_kind: i64,
    err_kind: i64,
) -> *mut std::ffi::c_char {
    let payload = result_payload_of(res);
    let kind = if result_disc_of(res) == 0 {
        ok_kind
    } else {
        err_kind
    };
    // SAFETY: this function's contract covers the payload of either arm.
    let rendered = unsafe { debug_payload_string(payload, kind) };
    let s = if result_disc_of(res) == 0 {
        format!("Ok({rendered})")
    } else {
        format!("Err({rendered})")
    };
    super::string::alloc_cstring(s.as_bytes())
}

/// [`gos_rt_debug_option`] for an `Option` whose payload is an aggregate:
/// `payload_kind` may be `gossamer_abi::DEBUG_PAYLOAD_ADT`, in which case `fmt` is the
/// payload type's derived formatter.
///
/// # Safety
///
/// A `Some` payload of `opt` is a live value of the shape `payload_kind` names,
/// and `fmt` is its formatter when that shape is an aggregate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_debug_option_fmt(
    opt: i128,
    payload_kind: i64,
    fmt: *const std::ffi::c_void,
) -> *mut std::ffi::c_char {
    let s = if result_disc_of(opt) != 0 {
        "None".to_string()
    } else {
        // SAFETY: this function's contract covers the payload and `fmt`.
        let payload =
            unsafe { debug_payload_string_with(result_payload_of(opt), payload_kind, fmt) };
        format!("Some({payload})")
    };
    super::string::alloc_cstring(s.as_bytes())
}

/// [`gos_rt_debug_result`] for a `Result` with an aggregate arm: either kind
/// may be `gossamer_abi::DEBUG_PAYLOAD_ADT`, with the matching `fmt` naming that arm's
/// derived formatter.
///
/// # Safety
///
/// The payload of `res` is a live value of the shape its arm's kind names, and
/// that arm's formatter is its formatter when the shape is an aggregate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_debug_result_fmt(
    res: i128,
    ok_kind: i64,
    err_kind: i64,
    ok_fmt: *const std::ffi::c_void,
    err_fmt: *const std::ffi::c_void,
) -> *mut std::ffi::c_char {
    let payload = result_payload_of(res);
    let (kind, fmt) = if result_disc_of(res) == 0 {
        (ok_kind, ok_fmt)
    } else {
        (err_kind, err_fmt)
    };
    // SAFETY: this function's contract covers the payload of either arm and
    // its formatter.
    let rendered = unsafe { debug_payload_string_with(payload, kind, fmt) };
    let s = if result_disc_of(res) == 0 {
        format!("Ok({rendered})")
    } else {
        format!("Err({rendered})")
    };
    super::string::alloc_cstring(s.as_bytes())
}

/// `result.unwrap()` / `option.unwrap()`. Returns the payload on the happy
/// path; panics on Err / None.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_unwrap(r: i128) -> i64 {
    ffi_entry!(-1, {
        if result_disc_of(r) != 0 {
            crate::c_abi::panic::panic_text("called `Result::unwrap()` on an `Err` value");
            return 0;
        }
        result_payload_of(r)
    })
}

/// `option.unwrap()` / `option.expect(msg)`. Shares the two-word carrier with
/// [`gos_rt_result_unwrap`] and differs only in the message the empty case
/// panics with, which names the shape the program actually wrote.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_option_unwrap(r: i128) -> i64 {
    ffi_entry!(-1, {
        if result_disc_of(r) != 0 {
            crate::c_abi::panic::panic_text("called `Option::unwrap()` on a `None` value");
            return 0;
        }
        result_payload_of(r)
    })
}

/// The carrier a two-word payload was boxed as, read back by value. A null box
/// reads as `None`.
///
/// # Safety
///
/// The payload of `r`, when non-null, addresses a live boxed two-word carrier.
unsafe fn boxed_carrier_of(r: i128) -> i128 {
    let boxed: *const u8 = std::ptr::with_exposed_provenance(result_payload_of(r) as usize);
    if boxed.is_null() {
        return gos_rt_result_new(1, 0);
    }
    // SAFETY: the payload word of a carrier whose payload is itself a carrier
    // is the address of the 16-byte box the constructor copied it into, alive
    // for as long as the outer carrier is.
    unsafe { boxed.cast::<i128>().read_unaligned() }
}

/// `option.unwrap()` / `option.expect(msg)` where the payload is itself an
/// `Option` / `Result`. The answer is the boxed carrier as it stands; the box
/// keeps its own share of that carrier's payload.
///
/// # Safety
///
/// A `Some` payload of `r` addresses a live boxed two-word carrier.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_unwrap_carrier(r: i128) -> i128 {
    ffi_entry!(0, {
        if result_disc_of(r) != 0 {
            crate::c_abi::panic::panic_text("called `Option::unwrap()` on a `None` value");
            return 0;
        }
        // SAFETY: this function's contract is the one the callee states.
        unsafe { boxed_carrier_of(r) }
    })
}

/// `result.unwrap()` / `result.expect(msg)` where the `Ok` payload is itself an
/// `Option` / `Result`, answered as [`gos_rt_option_unwrap_carrier`] does.
///
/// # Safety
///
/// An `Ok` payload of `r` addresses a live boxed two-word carrier.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_unwrap_carrier(r: i128) -> i128 {
    ffi_entry!(0, {
        if result_disc_of(r) != 0 {
            crate::c_abi::panic::panic_text("called `Result::unwrap()` on an `Err` value");
            return 0;
        }
        // SAFETY: this function's contract is the one the callee states.
        unsafe { boxed_carrier_of(r) }
    })
}

/// `result.unwrap_or(default)` / `option.unwrap_or(default)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_unwrap_or(r: i128, default: i64) -> i64 {
    if result_disc_of(r) == 0 {
        result_payload_of(r)
    } else {
        default
    }
}

/// `unwrap_or` where the value is a `Vec` / `[T]`.
///
/// The caller holds one share of the fallback and releases it where the
/// fallback's own binding ends, and one share of the answer. When the fallback
/// IS the answer those are the same Vec, so it needs the second share the
/// caller is going to give back; the word-returning form above cannot tell the
/// two apart and left the caller releasing one Vec twice.
///
/// # Safety
///
/// A non-zero `default` is a live `Vec` handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_unwrap_or_vec(r: i128, default: i64) -> i64 {
    if result_disc_of(r) == 0 {
        return result_payload_of(r);
    }
    if default != 0 {
        // SAFETY: a non-zero `default` is a live `Vec` (this shim's contract).
        unsafe { crate::c_abi::vec::gos_rt_vec_retain(default as usize as *mut GosVec) };
    }
    default
}

/// `unwrap_or` where the value is a `Map`.
///
/// A carrier never owns a table: an `Option<Map>` from a map read lends the
/// stored table, and the fallback belongs to its own binding, which frees it.
/// The answer is therefore a table of the caller's own either way.
///
/// # Safety
///
/// The payload of `r` and a non-zero `default` are live `Map` handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_unwrap_or_map(r: i128, default: i64) -> i64 {
    let chosen = if result_disc_of(r) == 0 {
        result_payload_of(r)
    } else {
        default
    };
    // SAFETY: a `Map` word is null or a live `GosMap` the carrier or the
    // fallback's binding holds for the length of this call.
    let copy =
        unsafe { crate::c_abi::gos_rt_map_clone(chosen as usize as *const crate::c_abi::GosMap) };
    copy as i64
}

/// `unwrap_or` where the value is a `String`.
///
/// The caller holds one share of the fallback and releases it where the
/// fallback's own binding ends, and one share of the answer. When the fallback
/// IS the answer those are the same string, so it needs the second share the
/// caller is going to give back; the word-returning form above cannot tell the
/// two apart and left the caller releasing one string twice.
///
/// # Safety
///
/// A non-zero `default` is a live runtime string body.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_unwrap_or_str(r: i128, default: i64) -> i64 {
    if result_disc_of(r) == 0 {
        return result_payload_of(r);
    }
    if default != 0 {
        // SAFETY: a non-zero `String` word is a runtime string body; the
        // retain answers a range test and ignores anything else.
        unsafe {
            crate::c_abi::string::gos_rt_str_retain_typed(
                default as usize as *const std::ffi::c_char,
            );
        }
    }
    default
}

/// `unwrap_or` where the value is a counted node: a payload-enum node or a
/// callable's environment.
///
/// The answer carries a share of its own on both arms, as
/// [`gos_rt_result_unwrap_or_str`] does: the payload's share moves out of the
/// carrier, and a fallback that becomes the answer takes a second share, since
/// its own binding still gives one back.
///
/// # Safety
///
/// A non-zero `default` is a live counted node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_unwrap_or_node(r: i128, default: i64) -> i64 {
    if result_disc_of(r) == 0 {
        return result_payload_of(r);
    }
    // SAFETY: a non-null word of a counted-node type points at a live RC
    // allocation, and the retain masks the enum tag bits.
    unsafe { crate::c_abi::rc::gos_rt_rc_retain(default as usize as *mut u8) };
    default
}

/// Reads the two-word carrier a map keeps boxed, answering `None` for a null
/// box: a map reader answers the box's address rather than the carrier.
///
/// # Safety
///
/// A non-zero `word` addresses a live boxed two-word carrier.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_carrier_from_box(word: i64) -> i128 {
    let boxed: *const u8 = std::ptr::with_exposed_provenance(word as usize);
    if boxed.is_null() {
        return gos_rt_result_new(1, 0);
    }
    // SAFETY: a non-null word a map reader answers for a two-word value is the
    // address of the entry's 16-byte carrier.
    unsafe { boxed.cast::<i128>().read_unaligned() }
}

/// Takes a share of a carrier's `Ok` / `Some` `String` payload, for a field
/// copy that holds the carrier alongside its source. A no-op on the other arm.
///
/// # Safety
///
/// An `Ok` / `Some` payload of `r` is a live runtime string body.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_str_payload_retain(r: i128) {
    if result_disc_of(r) != 0 || result_payload_of(r) == 0 {
        return;
    }
    // SAFETY: the payload word of an `Ok`/`Some` arm whose static type is a
    // `String` is a runtime string body.
    unsafe {
        crate::c_abi::string::gos_rt_str_retain_typed(
            result_payload_of(r) as usize as *const std::ffi::c_char
        );
    }
}

/// Gives back a carrier field's `Ok` / `Some` `String` payload at the field's
/// death. A no-op on the other arm.
///
/// # Safety
///
/// An `Ok` / `Some` payload of `r` is a live runtime string body holding the
/// share this releases.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_str_payload_release(r: i128) {
    // SAFETY: this function's contract is the one the callee states.
    unsafe { gos_rt_result_ok_payload_release(r, 1) };
}

/// Takes a share of a carrier's `Ok` / `Some` `Vec` payload, for a field copy
/// that holds the carrier alongside its source. A no-op on the other arm.
///
/// # Safety
///
/// An `Ok` / `Some` payload of `r` is a live `Vec` handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_vec_payload_retain(r: i128) {
    if result_disc_of(r) != 0 || result_payload_of(r) == 0 {
        return;
    }
    // SAFETY: the payload word of an `Ok`/`Some` arm whose static type is a
    // `Vec` is a live `GosVec` header.
    unsafe { crate::c_abi::gos_rt_vec_retain(result_payload_of(r) as usize as *mut GosVec) };
}

/// Gives back a carrier field's `Ok` / `Some` `Vec` payload at the field's
/// death. A no-op on the other arm.
///
/// # Safety
///
/// An `Ok` / `Some` payload of `r` is a live `Vec` handle holding the share
/// this releases.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_option_vec_payload_release(r: i128) {
    // SAFETY: this function's contract is the one the callee states.
    unsafe { gos_rt_result_ok_payload_release(r, 2) };
}

/// The carrier with its map payload replaced by a table of its own, so a
/// carrier a function answers owns the map it holds and the caller is the
/// one that frees it.
///
/// `ok_is_map` / `err_is_map` say which arm holds a `GosMap` word; the live
/// arm decides which one is read, and an arm holding anything else passes
/// through untouched.
///
/// # Safety
///
/// The payload of an arm `ok_is_map` / `err_is_map` names is a live `Map`
/// handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_carrier_own_map(r: i128, ok_is_map: i64, err_is_map: i64) -> i128 {
    let disc = result_disc_of(r);
    let is_map = match disc {
        0 => ok_is_map != 0,
        1 => err_is_map != 0,
        _ => false,
    };
    let payload = result_payload_of(r);
    if !is_map || payload == 0 {
        return r;
    }
    // SAFETY: the arm's payload word of a carrier whose static type names a
    // map is a live `GosMap` the caller still holds.
    let cloned = unsafe { crate::c_abi::map::gos_rt_map_clone(payload as usize as *mut _) };
    gos_rt_result_new(disc, cloned as usize as i64)
}

/// Releases the heap payload of a carrier's `Ok` / `Some` arm, and nothing on
/// the other arm, whose payload word belongs to the error value.
///
/// `kind` names the payload's storage: 1 a `String`, 2 a `Vec` / slice, 3 a
/// `json::Value` handle, 4 an `errors::Error` cell. This is the give-back for `map`, which hands the
/// payload to a closure that answers a value of its own; the carrier itself
/// never releases a payload of any of those kinds.
///
/// # Safety
///
/// An `Ok` payload of `r` is a live value of the storage `kind` names, holding
/// the share this releases.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_ok_payload_release(r: i128, kind: i64) {
    if result_disc_of(r) != 0 {
        return;
    }
    let payload = result_payload_of(r);
    if payload == 0 {
        return;
    }
    match kind {
        // SAFETY: the payload word of an `Ok`/`Some` arm whose static type is
        // a `String` is a runtime string body.
        1 => unsafe {
            crate::c_abi::string::gos_rt_str_free_typed(payload as usize as *mut std::ffi::c_char);
        },
        // SAFETY: as above, for a `Vec` header.
        2 => unsafe {
            crate::c_abi::gos_rt_vec_free(payload as usize as *mut GosVec);
        },
        // SAFETY: as above, for a `GosJson` handle box. The box holds one
        // share of the parsed document, so giving it back is what lets the
        // document die with the last handle onto it.
        3 => unsafe {
            crate::c_abi::json::gos_rt_json_free(
                payload as usize as *mut crate::c_abi::json::GosJson,
            );
        },
        // SAFETY: as above, for an error cell.
        4 => unsafe { crate::c_abi::rc::gos_rt_rc_release(payload as usize as *mut u8) },
        // SAFETY: as above, for a table the carrier owns outright: a `Map`
        // (5), a `Set` (6), or a `Deque` / `Queue` / `Stack` (7). Tables are
        // not counted, so only a carrier that is the table's sole owner is
        // ever released with these kinds.
        5 => unsafe {
            crate::c_abi::map::gos_rt_map_free(payload as usize as *mut crate::c_abi::map::GosMap);
        },
        // SAFETY: a payload of kind 6 is a `Set` the carrier owns.
        6 => unsafe {
            crate::c_abi::map::gos_rt_set_free(payload as usize as *mut crate::c_abi::set::GosSet);
        },
        // SAFETY: a payload of kind 7 is the storage this kind names, which the carrier owns.
        7 => unsafe {
            crate::c_abi::deque::gos_rt_deque_free(
                payload as usize as *mut crate::c_abi::deque::GosDeque,
            );
        },
        _ => {}
    }
}

/// Releases the heap payload of whichever arm a carrier holds: `ok_kind` names
/// the `Ok` / `Some` payload's storage and `err_kind` the `Err` payload's, in
/// the kinds [`gos_rt_result_ok_payload_release`] takes, with 0 for an arm
/// whose payload is not heap storage the carrier owns.
///
/// # Safety
///
/// The payload of `r` is a live value of the storage its arm's kind names,
/// holding the share this releases.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_payload_release(r: i128, ok_kind: i64, err_kind: i64) {
    // The error arm's payload word is laid out as the ok arm's is, so the
    // same release reads it once the discriminant has chosen the kind.
    let (as_ok, kind) = match result_disc_of(r) {
        0 => (r, ok_kind),
        1 => (gos_rt_result_new(0, result_payload_of(r)), err_kind),
        _ => return,
    };
    // SAFETY: this function's contract covers the payload of either arm.
    unsafe { gos_rt_result_ok_payload_release(as_ok, kind) };
}

/// Takes a share of the heap payload of whichever arm a carrier holds, by the
/// kinds [`gos_rt_result_payload_release`] takes: 1 a `String`, 2 a `Vec`, 4 an
/// `errors::Error` cell, 0 an arm with nothing to share.
///
/// # Safety
///
/// The payload of `r` is a live value of the storage its arm's kind names.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_payload_retain(r: i128, ok_kind: i64, err_kind: i64) {
    let kind = match result_disc_of(r) {
        0 => ok_kind,
        1 => err_kind,
        _ => 0,
    };
    let payload = result_payload_of(r);
    if payload == 0 {
        return;
    }
    match kind {
        // SAFETY: the payload word of an arm whose static type is a `String`
        // is a runtime string body.
        1 => unsafe {
            crate::c_abi::string::gos_rt_str_retain_typed(
                payload as usize as *const std::ffi::c_char,
            );
        },
        // SAFETY: as above, for a live `GosVec` header.
        2 => unsafe { crate::c_abi::gos_rt_vec_retain(payload as usize as *mut GosVec) },
        // SAFETY: as above, for a live error cell.
        4 => unsafe { crate::c_abi::rc::gos_rt_rc_retain(payload as usize as *mut u8) },
        _ => {}
    }
}

/// `unwrap_or` where the payload is itself a `Result` / `Option` carrier -
/// `spawn(f).join().unwrap_or(Ok(v))` is the shape that reaches it.
///
/// A carrier is two words, so the word-returning `unwrap_or` above keeps
/// only the payload half and loses the discriminant, which reads back as
/// an `Err` whatever the value was.
///
/// # Safety
///
/// The payload of `r` addresses a live boxed two-word carrier, and `default` is
/// a live carrier of the same type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_unwrap_or_carrier(r: i128, default: i128) -> i128 {
    if result_disc_of(r) != 0 {
        return default;
    }
    // A carrier does not fit the payload half, so a nested one is boxed
    // and the payload holds its address. Read the carrier back out.
    let boxed = result_payload_of(r) as usize as *const i128;
    if boxed.is_null() {
        return default;
    }
    // SAFETY: the pointer is the 16-byte aggregate `gos_rt_aggr_alloc`
    // handed the constructor, alive for as long as the carrier is.
    unsafe { boxed.read_unaligned() }
}

/// `result.ok()` / `option.ok()`. Returns the payload on Ok/Some, else 0.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_ok(r: i128) -> i64 {
    if result_disc_of(r) == 0 {
        result_payload_of(r)
    } else {
        0
    }
}

/// `result.err()`. Returns the error payload on Err, else 0.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_err(r: i128) -> i64 {
    if result_disc_of(r) == 1 {
        result_payload_of(r)
    } else {
        0
    }
}

/// `option.ok_or(new_err)`. On Some, answers the receiver and gives back the
/// replacement the caller handed over; on None, answers `Err(new_err)`, which
/// takes that share. The call consumes `new_err` on either arm, so the caller
/// hands its share over once and never gives it back itself. `err_kind` names
/// the replacement's storage in the kinds
/// [`gos_rt_result_ok_payload_release`] takes.
///
/// # Safety
///
/// A non-zero `new_err` is a live value of the storage `err_kind` names,
/// holding the share this call consumes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_result_ok_or(r: i128, new_err: i64, err_kind: i64) -> i128 {
    if result_disc_of(r) == 0 {
        // SAFETY: this function's contract covers `new_err`.
        unsafe { gos_rt_result_ok_payload_release(pack_result(0, new_err), err_kind) };
        r
    } else {
        pack_result(1, new_err)
    }
}

/// `result.is_ok()` / `option.is_some()`. 1 on Ok/Some, 0 on Err/None.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_is_ok(r: i128) -> i64 {
    i64::from(result_disc_of(r) == 0)
}

/// `result.is_err()` / `option.is_none()`. 1 on Err/None, 0 on Ok/Some.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_result_is_err(r: i128) -> i64 {
    i64::from(result_disc_of(r) != 0)
}
