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

use std::os::raw::c_char;
use std::sync::atomic::Ordering;

use super::*;

// ---------------------------------------------------------------
// flag::Set - minimal CLI-flag parser. The compiled tier exposes
// a single mutable `*mut GosFlagSet` with `.string`, `.uint`,
// `.bool` registration and `.parse(args)`. Each registration
// returns a `*mut Cell<T>` so user code does `*name` to read
// the post-parse value.
// ---------------------------------------------------------------

pub struct GosFlagSet {
    name: String,
    specs: Vec<FlagSpec>,
    /// After `.parse()` runs, these hold the positional args left
    /// over. The handle returned via `gos_rt_flag_parse` is a
    /// `*mut GosVec` of c-string pointers.
    positional: Vec<String>,
}

struct FlagSpec {
    long_name: String,
    short: Option<char>,
    summary: String,
    kind: FlagKind,
    cell: SyncRawPtr<std::ffi::c_void>,
}

#[derive(Debug, Clone)]
pub enum FlagKind {
    String,
    Int,
    Uint,
    Float,
    Bool,
    /// Duration cell stores `i64` milliseconds - same wire shape as
    /// `time::Duration` in the compiled tier.
    Duration,
    /// String-list cell stores `*mut GosVec` of c-string pointers.
    StringList,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_new(name: *const c_char) -> *mut GosFlagSet {
    ffi_entry!(std::ptr::null_mut(), {
        let n = if name.is_null() {
            String::new()
        } else {
            // SAFETY: `name` is a String argument from compiled code, null or a live string body for the whole call.
            unsafe { crate::c_abi::gos_str_arg_string(name) }
        };
        Box::into_raw(Box::new(GosFlagSet {
            name: n,
            specs: Vec::new(),
            positional: Vec::new(),
        }))
    })
}

/// # Safety
///
/// `p` is null or a live string body.
unsafe fn read_cstr(p: *const c_char) -> String {
    // SAFETY: this function's contract is the one the reader states for `p`.
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_string(
    set: *mut GosFlagSet,
    name: *const c_char,
    default_v: *const c_char,
    help: *const c_char,
) -> *mut *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if set.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `name` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let n = unsafe { read_cstr(name) };
        // SAFETY: `help` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let h = unsafe { read_cstr(help) };
        let dv = if default_v.is_null() {
            alloc_cstring(b"")
        } else {
            // SAFETY: `default_v` is a String argument from compiled code, null or a live string body for the whole call.
            let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(default_v) }.to_vec();
            alloc_cstring(&bytes)
        };
        let cell = Box::into_raw(Box::new(dv));
        // SAFETY: `set` is a handle from compiled code, checked non-null above and live for the whole call.
        let set = unsafe { &mut *set };
        set.specs.push(FlagSpec {
            long_name: n,
            short: None,
            summary: h,
            kind: FlagKind::String,
            cell: SyncRawPtr::new(cell.cast::<std::ffi::c_void>()),
        });
        cell
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_int(
    set: *mut GosFlagSet,
    name: *const c_char,
    default_v: i64,
    help: *const c_char,
) -> *mut i64 {
    ffi_entry!(std::ptr::null_mut(), {
        if set.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `name` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let n = unsafe { read_cstr(name) };
        // SAFETY: `help` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let h = unsafe { read_cstr(help) };
        let cell = Box::into_raw(Box::new(default_v));
        // SAFETY: `set` is a handle from compiled code, checked non-null above and live for the whole call.
        let set = unsafe { &mut *set };
        set.specs.push(FlagSpec {
            long_name: n,
            short: None,
            summary: h,
            kind: FlagKind::Int,
            cell: SyncRawPtr::new(cell.cast::<std::ffi::c_void>()),
        });
        cell
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_uint(
    set: *mut GosFlagSet,
    name: *const c_char,
    default_v: u64,
    help: *const c_char,
) -> *mut u64 {
    ffi_entry!(std::ptr::null_mut(), {
        if set.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `name` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let n = unsafe { read_cstr(name) };
        // SAFETY: `help` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let h = unsafe { read_cstr(help) };
        let cell = Box::into_raw(Box::new(default_v));
        // SAFETY: `set` is a handle from compiled code, checked non-null above and live for the whole call.
        let set = unsafe { &mut *set };
        set.specs.push(FlagSpec {
            long_name: n,
            short: None,
            summary: h,
            kind: FlagKind::Uint,
            cell: SyncRawPtr::new(cell.cast::<std::ffi::c_void>()),
        });
        cell
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_float(
    set: *mut GosFlagSet,
    name: *const c_char,
    default_v: f64,
    help: *const c_char,
) -> *mut f64 {
    ffi_entry!(std::ptr::null_mut(), {
        if set.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `name` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let n = unsafe { read_cstr(name) };
        // SAFETY: `help` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let h = unsafe { read_cstr(help) };
        let cell = Box::into_raw(Box::new(default_v));
        // SAFETY: `set` is a handle from compiled code, checked non-null above and live for the whole call.
        let set = unsafe { &mut *set };
        set.specs.push(FlagSpec {
            long_name: n,
            short: None,
            summary: h,
            kind: FlagKind::Float,
            cell: SyncRawPtr::new(cell.cast::<std::ffi::c_void>()),
        });
        cell
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_bool(
    set: *mut GosFlagSet,
    name: *const c_char,
    default_v: bool,
    help: *const c_char,
) -> *mut bool {
    ffi_entry!(std::ptr::null_mut(), {
        if set.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `name` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let n = unsafe { read_cstr(name) };
        // SAFETY: `help` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let h = unsafe { read_cstr(help) };
        let cell = Box::into_raw(Box::new(default_v));
        // SAFETY: `set` is a handle from compiled code, checked non-null above and live for the whole call.
        let set = unsafe { &mut *set };
        set.specs.push(FlagSpec {
            long_name: n,
            short: None,
            summary: h,
            kind: FlagKind::Bool,
            cell: SyncRawPtr::new(cell.cast::<std::ffi::c_void>()),
        });
        cell
    })
}

/// Duration cell. `default_v` is interpreted as milliseconds (same
/// wire shape used by `time::Duration` in the compiled tier).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_duration(
    set: *mut GosFlagSet,
    name: *const c_char,
    default_ms: i64,
    help: *const c_char,
) -> *mut i64 {
    ffi_entry!(std::ptr::null_mut(), {
        if set.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `name` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let n = unsafe { read_cstr(name) };
        // SAFETY: `help` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let h = unsafe { read_cstr(help) };
        let cell = Box::into_raw(Box::new(default_ms));
        // SAFETY: `set` is a handle from compiled code, checked non-null above and live for the whole call.
        let set = unsafe { &mut *set };
        set.specs.push(FlagSpec {
            long_name: n,
            short: None,
            summary: h,
            kind: FlagKind::Duration,
            cell: SyncRawPtr::new(cell.cast::<std::ffi::c_void>()),
        });
        cell
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_string_list(
    set: *mut GosFlagSet,
    name: *const c_char,
    help: *const c_char,
) -> *mut *mut GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        if set.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: `name` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let n = unsafe { read_cstr(name) };
        // SAFETY: `help` is this shim's argument, as `read_cstr` requires (C-ABI contract).
        let h = unsafe { read_cstr(help) };
        let backing = gos_rt_vec_new(8);
        let cell = Box::into_raw(Box::new(backing));
        // SAFETY: `set` is a handle from compiled code, checked non-null above and live for the whole call.
        let set = unsafe { &mut *set };
        set.specs.push(FlagSpec {
            long_name: n,
            short: None,
            summary: h,
            kind: FlagKind::StringList,
            cell: SyncRawPtr::new(cell.cast::<std::ffi::c_void>()),
        });
        cell
    })
}

/// Attaches a one-character short alias to the most recently
/// registered flag - mirrors `Set::short` in `gossamer-std`.
/// `letter` is passed as i64 to match how single-char literals
/// flow through the compiled-tier C ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_short(set: *mut GosFlagSet, letter: i64) {
    ffi_entry!((), {
        if set.is_null() {
            return;
        }
        // SAFETY: `set` is a handle from compiled code, checked non-null above and live for the whole call.
        let set = unsafe { &mut *set };
        let Some(ch) = u32::try_from(letter).ok().and_then(char::from_u32) else {
            return;
        };
        if let Some(last) = set.specs.last_mut() {
            last.short = Some(ch);
        }
    });
}

/// Returns the auto-generated usage string as a heap-allocated
/// c-string. Matches `gossamer-std::flag::Set::usage`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_usage(set: *const GosFlagSet) -> *mut c_char {
    ffi_entry!(std::ptr::null_mut(), {
        if set.is_null() {
            return alloc_cstring(b"");
        }
        // SAFETY: `set` is a handle from compiled code, checked non-null above and live for the whole call.
        let set = unsafe { &*set };
        let bytes = render_flag_usage(set).into_bytes();
        alloc_cstring(&bytes)
    })
}

fn render_flag_usage(set: &GosFlagSet) -> String {
    let program = if set.name.is_empty() {
        "program"
    } else {
        &set.name
    };
    let mut out = format!("usage: {program} [FLAGS] [POSITIONAL]\n\nflags:\n");
    for def in &set.specs {
        let label = match def.short {
            Some(ch) => format!("  -{ch}, --{}", def.long_name),
            None => format!("      --{}", def.long_name),
        };
        out.push_str(&format!("{label:<30} {}\n", def.summary));
    }
    out
}

/// A duration flag's text as a `time::Duration`, in nanoseconds: a count
/// with an `ns`, `us`, `ms`, `s`, `m`, or `h` unit, or bare seconds.
pub fn parse_duration_text(text: &str) -> Option<i64> {
    let text = text.trim();
    let (digits, scale) = [
        ("ns", 1),
        ("us", 1_000),
        ("ms", 1_000_000),
        ("s", 1_000_000_000),
        ("m", 60_000_000_000),
        ("h", 3_600_000_000_000),
    ]
    .into_iter()
    .find_map(|(unit, scale)| text.strip_suffix(unit).map(|rest| (rest, scale)))
    .unwrap_or((text, 1_000_000_000));
    digits.parse::<i64>().ok().map(|n| n.saturating_mul(scale))
}

fn parse_bool_text(text: &str) -> Option<bool> {
    match text {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Resolves an explicit-or-following value for `spec` and writes
/// it into the spec's cell. Returns the number of argv tokens
/// consumed (1 for `--name=value`, `--bool`, `-v`; 2 for
/// `--name value`).
fn apply_flag_value(
    spec: &mut FlagSpec,
    explicit: Option<String>,
    get_arg_ptr: &dyn Fn(i64) -> *const c_char,
    idx: i64,
    argc: i64,
) -> i64 {
    // Bool with no explicit value is a "set true" form.
    if matches!(spec.kind, FlagKind::Bool) && explicit.is_none() {
        // SAFETY: a spec's `cell` is the box its registration made for the kind it names, which
        // lives for the program, here a `bool`.
        unsafe {
            *(spec.cell.cast::<bool>()) = true;
        }
        return 1;
    }
    let (raw, consumed) = if let Some(v) = explicit {
        (v, 1)
    } else {
        if idx + 1 >= argc {
            return 1;
        }
        let p = get_arg_ptr(idx + 1);
        if p.is_null() {
            return 1;
        }
        // SAFETY: `p` is a non-null argument word (checked above), a live string body.
        let s = unsafe { crate::c_abi::gos_str_arg_string(p) };
        (s, 2)
    };
    match spec.kind {
        FlagKind::String => {
            let bytes = raw.as_bytes().to_vec();
            let leaked = alloc_cstring(&bytes);
            // SAFETY: a spec's `cell` is the box its registration made for the kind it names,
            // which lives for the program, here a `String` slot.
            unsafe {
                *(spec.cell.cast::<*mut c_char>()) = leaked;
            }
        }
        FlagKind::Int => {
            if let Ok(n) = raw.parse::<i64>() {
                // SAFETY: a spec's `cell` is the box its registration made for the kind it names,
                // which lives for the program, here an `i64`.
                unsafe {
                    *(spec.cell.cast::<i64>()) = n;
                }
            }
        }
        FlagKind::Uint => {
            if let Ok(n) = raw.parse::<u64>() {
                // SAFETY: a spec's `cell` is the box its registration made for the kind it names,
                // which lives for the program, here a `u64`.
                unsafe {
                    *(spec.cell.cast::<u64>()) = n;
                }
            }
        }
        FlagKind::Float => {
            if let Ok(x) = raw.parse::<f64>() {
                // SAFETY: a spec's `cell` is the box its registration made for the kind it names,
                // which lives for the program, here an `f64`.
                unsafe {
                    *(spec.cell.cast::<f64>()) = x;
                }
            }
        }
        FlagKind::Bool => {
            if let Some(b) = parse_bool_text(&raw) {
                // SAFETY: a spec's `cell` is the box its registration made for the kind it names,
                // which lives for the program, here a `bool`.
                unsafe {
                    *(spec.cell.cast::<bool>()) = b;
                }
            }
        }
        FlagKind::Duration => {
            if let Some(ms) = parse_duration_text(&raw) {
                // SAFETY: a spec's `cell` is the box its registration made for the kind it names,
                // which lives for the program, here an `i64` of milliseconds.
                unsafe {
                    *(spec.cell.cast::<i64>()) = ms;
                }
            }
        }
        FlagKind::StringList => {
            let bytes = raw.as_bytes().to_vec();
            let cstr = alloc_cstring(&bytes);
            let ptr_val = cstr as i64;
            // SAFETY: a spec's `cell` is the box its registration made for the kind it names,
            // which lives for the program, here a `Vec<String>` slot.
            let backing = unsafe { *(spec.cell.cast::<*mut GosVec>()) };
            if !backing.is_null() {
                // SAFETY: `backing` is the list's non-null vec (checked above), and `ptr_val` is
                // one 8-byte element.
                unsafe {
                    gos_rt_vec_push(backing, std::ptr::addr_of!(ptr_val).cast::<u8>());
                }
            }
        }
    }
    consumed
}

/// Parses GNU-style `--name value` and `--bool` flags out of
/// `args` (a `*mut GosVec` of c-string pointers from
/// `os::args()`), filling in each registered cell. Returns a
/// `Result<Vec<String>, Error>` containing the leftover positional arguments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_flag_set_parse(set: *mut GosFlagSet, args: *const GosVec) -> i128 {
    ffi_entry!(crate::c_abi::vec::gos_rt_result_new(1, 0), {
        if set.is_null() {
            let out = gos_rt_vec_new(8);
            return crate::c_abi::vec::gos_rt_result_new(0, out as i64);
        }
        // SAFETY: `set` is non-null (checked above) and live for the call (C-ABI contract).
        let set = unsafe { &mut *set };
        set.positional.clear();
        if args.is_null() {
            let out = gos_rt_vec_new(8);
            return crate::c_abi::vec::gos_rt_result_new(0, out as i64);
        }
        // Two callers reach this function: the runner-build path
        // passes a real `*mut GosVec` of c-string pointers; the
        // compiled path passes the `os::args()` sentinel - a raw
        // `argv + 1` pointer with `argc - 1` length stashed in the
        // process-global ARGS_PTR / ARGS_LEN. An argv pointer has no
        // `GosVec` header, so the sentinel is recognised by pointer
        // equality and walked as `argv` directly.
        let sentinel_ptr = ARGS_PTR.load(Ordering::SeqCst);
        let is_sentinel = sentinel_ptr != 0 && (args as usize) == sentinel_ptr;
        let (argc, start_i, get_arg_ptr): (i64, i64, Box<dyn Fn(i64) -> *const c_char>) =
            if is_sentinel {
                let argv = sentinel_ptr as *const *const c_char;
                let len = ARGS_LEN.load(Ordering::SeqCst);
                let getter: Box<dyn Fn(i64) -> *const c_char> =
                    // SAFETY: `argv` is the program's argument array of `len` entries recorded at
                    // startup, and the parse asks only for `i` below `len`.
                    Box::new(move |i: i64| unsafe { *argv.add(i as usize) });
                (len, 0, getter)
            } else {
                let v = args;
                // SAFETY: `v` is this shim's non-null argument vec, live for the call (C-ABI
                // contract).
                let len = unsafe { gos_rt_vec_len(v) };
                // SAFETY: `v` is live for the call, and `gos_rt_vec_get_ptr` answers null for an
                // index outside it or a slot of a `Vec<String>`.
                let getter: Box<dyn Fn(i64) -> *const c_char> = Box::new(move |i: i64| unsafe {
                    let p = gos_rt_vec_get_ptr(v, i);
                    if p.is_null() {
                        std::ptr::null()
                    } else {
                        crate::c_abi::vec::slot_read_word(p)
                            .cast_const()
                            .cast::<c_char>()
                    }
                });
                (len, 0, getter) // GosVec from os::args() already excludes argv[0]
            };
        let mut i = start_i;
        while i < argc {
            let arg_ptr = get_arg_ptr(i);
            let arg = if arg_ptr.is_null() {
                String::new()
            } else {
                // SAFETY: `arg_ptr` is a non-null argument word (checked above), a live string
                // body.
                unsafe { crate::c_abi::gos_str_arg_string(arg_ptr) }
            };
            if arg == "--" {
                i += 1;
                while i < argc {
                    let p = get_arg_ptr(i);
                    if !p.is_null() {
                        // SAFETY: `p` is a non-null argument word (checked above), a live string
                        // body.
                        let s = unsafe { crate::c_abi::gos_str_arg_string(p) };
                        set.positional.push(s);
                    }
                    i += 1;
                }
                break;
            }
            if arg == "--help" || arg == "-h" {
                print!("{}", render_flag_usage(set));
                // Route through `gos_rt_exit` so the stdout cache is
                // flushed and the audited-exit list (Fix C3) stays
                // empty outside the two legitimate paths.
                gos_rt_exit(0);
            }
            if let Some(rest) = arg.strip_prefix("--") {
                let (name, explicit) = match rest.split_once('=') {
                    Some((n, v)) => (n.to_string(), Some(v.to_string())),
                    None => (rest.to_string(), None),
                };
                if let Some(spec) = set.specs.iter_mut().find(|s| s.long_name == name) {
                    let consumed = apply_flag_value(spec, explicit, &get_arg_ptr, i, argc);
                    i += consumed;
                    continue;
                }
                set.positional.push(arg);
                i += 1;
                continue;
            }
            if let Some(rest) = arg.strip_prefix('-')
                && !rest.is_empty()
            {
                let mut chars = rest.chars();
                let first = chars.next().unwrap();
                let remainder: String = chars.collect();
                if let Some(spec) = set.specs.iter_mut().find(|s| s.short == Some(first)) {
                    let explicit = if remainder.is_empty() {
                        None
                    } else if let Some(stripped) = remainder.strip_prefix('=') {
                        Some(stripped.to_string())
                    } else {
                        Some(remainder.clone())
                    };
                    let consumed = apply_flag_value(spec, explicit, &get_arg_ptr, i, argc);
                    i += consumed;
                    continue;
                }
            }
            set.positional.push(arg);
            i += 1;
        }
        // STRING-typed: the rest vec owns its fresh positional strings.
        let out = {
            crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
                8,
                set.positional.len() as i64,
                crate::c_abi::vec::vec_elem_kind::STRING,
            )
        };
        for s in &set.positional {
            let bytes = s.as_bytes();
            let cstr = alloc_cstring(bytes);
            let ptr_val = cstr as i64;
            // SAFETY: `out` is the fresh vec made above, or null, which `gos_rt_vec_push`
            // accepts, and `ptr_val` is one 8-byte element.
            unsafe {
                gos_rt_vec_push(out, std::ptr::addr_of!(ptr_val).cast::<u8>());
            }
        }
        crate::c_abi::vec::gos_rt_result_new(0, out as i64)
    })
}
