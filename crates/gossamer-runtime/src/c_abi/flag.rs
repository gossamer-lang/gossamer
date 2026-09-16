#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
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
#![allow(unused_unsafe)]
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
            unsafe { crate::c_abi::gos_str_arg_string(name) }
        };
        Box::into_raw(Box::new(GosFlagSet {
            name: n,
            specs: Vec::new(),
            positional: Vec::new(),
        }))
    })
}

fn read_cstr(p: *const c_char) -> String {
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
        let n = read_cstr(name);
        let h = read_cstr(help);
        let dv = if default_v.is_null() {
            alloc_cstring(b"")
        } else {
            let bytes = unsafe { crate::c_abi::gos_str_arg_bytes(default_v) }.to_vec();
            alloc_cstring(&bytes)
        };
        let cell = Box::into_raw(Box::new(dv));
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
        let n = read_cstr(name);
        let h = read_cstr(help);
        let cell = Box::into_raw(Box::new(default_v));
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
        let n = read_cstr(name);
        let h = read_cstr(help);
        let cell = Box::into_raw(Box::new(default_v));
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
        let n = read_cstr(name);
        let h = read_cstr(help);
        let cell = Box::into_raw(Box::new(default_v));
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
        let n = read_cstr(name);
        let h = read_cstr(help);
        let cell = Box::into_raw(Box::new(default_v));
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
        let n = read_cstr(name);
        let h = read_cstr(help);
        let cell = Box::into_raw(Box::new(default_ms));
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
        let n = read_cstr(name);
        let h = read_cstr(help);
        let backing = unsafe { gos_rt_vec_new(8) };
        let cell = Box::into_raw(Box::new(backing));
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

fn parse_duration_text(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Some(rest) = text.strip_suffix("ms") {
        return rest.parse::<i64>().ok();
    }
    if let Some(rest) = text.strip_suffix("us") {
        return rest.parse::<i64>().ok().map(|n| n / 1_000);
    }
    if let Some(rest) = text.strip_suffix("ns") {
        return rest.parse::<i64>().ok().map(|n| n / 1_000_000);
    }
    if let Some(rest) = text.strip_suffix("s") {
        return rest.parse::<i64>().ok().map(|n| n * 1_000);
    }
    if let Some(rest) = text.strip_suffix("m") {
        return rest.parse::<i64>().ok().map(|n| n * 60_000);
    }
    if let Some(rest) = text.strip_suffix("h") {
        return rest.parse::<i64>().ok().map(|n| n * 3_600_000);
    }
    text.parse::<i64>().ok().map(|n| n * 1_000)
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
        let s = unsafe { crate::c_abi::gos_str_arg_string(p) };
        (s, 2)
    };
    match spec.kind {
        FlagKind::String => {
            let bytes = raw.as_bytes().to_vec();
            let leaked = alloc_cstring(&bytes);
            unsafe {
                *(spec.cell.cast::<*mut c_char>()) = leaked;
            }
        }
        FlagKind::Int => {
            if let Ok(n) = raw.parse::<i64>() {
                unsafe {
                    *(spec.cell.cast::<i64>()) = n;
                }
            }
        }
        FlagKind::Uint => {
            if let Ok(n) = raw.parse::<u64>() {
                unsafe {
                    *(spec.cell.cast::<u64>()) = n;
                }
            }
        }
        FlagKind::Float => {
            if let Ok(x) = raw.parse::<f64>() {
                unsafe {
                    *(spec.cell.cast::<f64>()) = x;
                }
            }
        }
        FlagKind::Bool => {
            if let Some(b) = parse_bool_text(&raw) {
                unsafe {
                    *(spec.cell.cast::<bool>()) = b;
                }
            }
        }
        FlagKind::Duration => {
            if let Some(ms) = parse_duration_text(&raw) {
                unsafe {
                    *(spec.cell.cast::<i64>()) = ms;
                }
            }
        }
        FlagKind::StringList => {
            let bytes = raw.as_bytes().to_vec();
            let cstr = alloc_cstring(&bytes);
            let ptr_val = cstr as i64;
            let backing = unsafe { *(spec.cell.cast::<*mut GosVec>()) };
            if !backing.is_null() {
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
    ffi_entry!(unsafe { crate::c_abi::vec::gos_rt_result_new(1, 0) }, {
        if set.is_null() {
            let out = unsafe { gos_rt_vec_new(8) };
            return unsafe { crate::c_abi::vec::gos_rt_result_new(0, out as i64) };
        }
        let set = unsafe { &mut *set };
        set.positional.clear();
        if args.is_null() {
            let out = unsafe { gos_rt_vec_new(8) };
            return unsafe { crate::c_abi::vec::gos_rt_result_new(0, out as i64) };
        }
        // Two callers reach this function: the runner-build path
        // passes a real `*mut GosVec` of c-string pointers; the
        // compiled path passes the `os::args()` sentinel - a raw
        // `argv + 1` pointer with `argc - 1` length stashed in the
        // process-global ARGS_PTR / ARGS_LEN. Detect the sentinel by
        // pointer-equality and route to a separate iteration path
        // that walks `argv` directly. Without this branch the code
        // tries to read a GosVec header out of an argv pointer and
        // segfaults on the first positional arg.
        let sentinel_ptr = ARGS_PTR.load(Ordering::SeqCst);
        let is_sentinel = sentinel_ptr != 0 && (args as usize) == sentinel_ptr;
        let (argc, start_i, get_arg_ptr): (i64, i64, Box<dyn Fn(i64) -> *const c_char>) =
            if is_sentinel {
                let argv = sentinel_ptr as *const *const c_char;
                let len = ARGS_LEN.load(Ordering::SeqCst);
                let getter: Box<dyn Fn(i64) -> *const c_char> =
                    Box::new(move |i: i64| unsafe { *argv.add(i as usize) });
                (len, 0, getter)
            } else {
                let v = args;
                let len = unsafe { gos_rt_vec_len(v) };
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
                unsafe { crate::c_abi::gos_str_arg_string(arg_ptr) }
            };
            if arg == "--" {
                i += 1;
                while i < argc {
                    let p = get_arg_ptr(i);
                    if !p.is_null() {
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
                unsafe { gos_rt_exit(0) };
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
        let out = unsafe {
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
            unsafe {
                gos_rt_vec_push(out, std::ptr::addr_of!(ptr_val).cast::<u8>());
            }
        }
        unsafe { crate::c_abi::vec::gos_rt_result_new(0, out as i64) }
    })
}
