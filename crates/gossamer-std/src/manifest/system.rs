#![allow(
    unused_imports,
    dead_code,
    unreachable_pub,
    missing_docs,
    clippy::wildcard_imports,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::items_after_statements,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::option_if_let_else,
    clippy::match_same_arms,
    clippy::if_not_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::redundant_else,
    clippy::collapsible_if,
    clippy::collapsible_else_if,
    clippy::map_unwrap_or,
    clippy::struct_excessive_bools,
    clippy::module_name_repetitions,
    clippy::unnecessary_wraps,
    clippy::large_enum_variant,
    clippy::if_same_then_else,
    clippy::single_match,
    clippy::useless_conversion,
    clippy::needless_borrows_for_generic_args,
    clippy::let_and_return,
    clippy::needless_collect,
    clippy::elidable_lifetime_names,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::needless_range_loop,
    clippy::ptr_arg,
    clippy::ptr_as_ptr,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::semicolon_if_nothing_returned,
    clippy::single_call_fn,
    clippy::unused_self,
    clippy::range_plus_one,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::cast_ptr_alignment,
    clippy::manual_assert,
    clippy::manual_string_new,
    clippy::match_bool,
    clippy::nonminimal_bool,
    clippy::redundant_pattern_matching,
    clippy::useless_let_if_seq
)]
//! Static manifest of every registered stdlib module.
//! Each stdlib milestone extends this table with
//! the modules it adds. Entries are listed in phase-introduction order
//! so a `gos doc` walk renders modules in the same sequence as the
//! implementation plan.

#![forbid(unsafe_code)]
use crate::registry::{StdItem, StdItemKind, StdModule};

use super::*;

pub const OS_EXEC: StdModule = StdModule {
    path: "std::os::exec",
    summary: "Deprecated compatibility facade for child processes; new code uses std::process.",
    items: &[
        StdItem {
            name: "Path",
            kind: StdItemKind::Type,
            doc: "Immutable UTF-8 lexical path value with value-returning operations.",
        },
        StdItem {
            name: "Child",
            kind: StdItemKind::Type,
            doc: "Handle to a still-running child supporting wait / kill.",
        },
        StdItem {
            name: "Pipeline",
            kind: StdItemKind::Type,
            doc: "Multi-stage subprocess pipeline (stdout-to-stdin chain).",
        },
        StdItem {
            name: "Signal",
            kind: StdItemKind::Type,
            doc: "Portable signal selector (Term/Kill/Stop/Cont/Hup/Int/Usr1/Usr2/Pipe/Quit).",
        },
        StdItem {
            name: "run",
            kind: StdItemKind::Function,
            doc: "One-shot: runs a program with args, captures stdout/stderr, returns Result<{stdout, stderr, code}, String>.",
        },
        StdItem {
            name: "spawn",
            kind: StdItemKind::Function,
            doc: "Non-blocking launch; returns the child PID as Result<i64, errors::Error>.",
        },
        StdItem {
            name: "spawn_piped",
            kind: StdItemKind::Function,
            doc: "Spawns a child with piped stdin/stdout; returns Result<Child, errors::Error>. The Child's write_stdin / close_stdin / read_line / read_stdout / wait / kill methods drive it interactively.",
        },
        StdItem {
            name: "kill",
            kind: StdItemKind::Function,
            doc: "Best-effort SIGTERM by pid; returns true on success.",
        },
        StdItem {
            name: "signal",
            kind: StdItemKind::Function,
            doc: "Send an arbitrary signal number to a pid; returns true on success.",
        },
        StdItem {
            name: "kill_group",
            kind: StdItemKind::Function,
            doc: "Send SIGTERM to the entire process group (Unix); best-effort TerminateProcess on Windows.",
        },
        StdItem {
            name: "wait_timeout",
            kind: StdItemKind::Function,
            doc: "Wait up to N ms for a pid to exit; returns exit code, -1 on timeout, -2 on error.",
        },
        StdItem {
            name: "pipeline_run",
            kind: StdItemKind::Function,
            doc: "Run a Vec<String> of shell-tokenised commands as a stdout-to-stdin pipeline.",
        },
    ],
};

pub const OS_SIGNAL: StdModule = StdModule {
    path: "std::os::signal",
    summary: "POSIX-style signal subscription (Go's os/signal shape).",
    items: &[
        StdItem {
            name: "Signal",
            kind: StdItemKind::Type,
            doc: "Opaque signal name; constructors live in `sigs`.",
        },
        StdItem {
            name: "Notifier",
            kind: StdItemKind::Type,
            doc: "Returned by `on(sig)`; supports wait / try_wait / stop.",
        },
        StdItem {
            name: "on",
            kind: StdItemKind::Function,
            doc: "Subscribes to a signal; returns a Notifier.",
        },
        StdItem {
            name: "wait",
            kind: StdItemKind::Function,
            doc: "`wait() -> bool`: blocks the calling goroutine until the subscribed signal fires (`true`) or its cohort is cancelled (`false`), without holding a scheduler worker. On wasm32, which delivers no signals, it answers `false` at once.",
        },
        StdItem {
            name: "try_wait",
            kind: StdItemKind::Function,
            doc: "Non-blocking poll: returns true if the subscribed signal has fired.",
        },
        StdItem {
            name: "stop",
            kind: StdItemKind::Function,
            doc: "`stop()`: the notifier delivers nothing more, so its waits answer `false`; once no notifier subscribes to the signal it takes its default disposition again. Go's `signal.Stop`.",
        },
        StdItem {
            name: "SIGHUP",
            kind: StdItemKind::Const,
            doc: "Hangup: the controlling terminal closed (1).",
        },
        StdItem {
            name: "SIGINT",
            kind: StdItemKind::Const,
            doc: "Interrupt, Ctrl-C (2).",
        },
        StdItem {
            name: "SIGQUIT",
            kind: StdItemKind::Const,
            doc: "Quit, Ctrl-\\ (3); also renders the goroutine dump.",
        },
        StdItem {
            name: "SIGTERM",
            kind: StdItemKind::Const,
            doc: "Termination request (15).",
        },
        StdItem {
            name: "SIGUSR1",
            kind: StdItemKind::Const,
            doc: "User signal 1 (10 on Linux, 30 on macOS).",
        },
        StdItem {
            name: "SIGUSR2",
            kind: StdItemKind::Const,
            doc: "User signal 2 (12 on Linux, 31 on macOS).",
        },
        StdItem {
            name: "SIGWINCH",
            kind: StdItemKind::Const,
            doc: "The terminal window changed size (28). Never delivered on Windows.",
        },
        StdItem {
            name: "SIGTSTP",
            kind: StdItemKind::Const,
            doc: "Terminal stop, Ctrl-Z (20 on Linux, 18 on macOS). Subscribing replaces the default stop, so the program stops itself. Never delivered on Windows.",
        },
        StdItem {
            name: "SIGCONT",
            kind: StdItemKind::Const,
            doc: "Continue after a stop (18 on Linux, 19 on macOS). Never delivered on Windows.",
        },
    ],
};

pub const OS_FD: StdModule = StdModule {
    path: "std::os::fd",
    summary: "Waiting for a file descriptor (a handle on Windows) to be readable or writable without holding a scheduler worker.",
    items: &[
        StdItem {
            name: "Descriptor",
            kind: StdItemKind::Trait,
            doc: "What a call that needs an OS descriptor takes: an `fs::File`, which stays open while the call uses it, or `term::STDIN`, `term::STDOUT`, or `term::STDERR` for a standard stream. Any other integer is refused, since a descriptor read off a file names whatever the OS reuses it for once the file closes (GT0119).",
        },
        StdItem {
            name: "wait_readable",
            kind: StdItemKind::Function,
            doc: "`wait_readable(fd, timeout_ms) -> Result<bool, errors::Error>`: true when `fd` has input (or reached end of input) within `timeout_ms` milliseconds; a negative timeout waits indefinitely. False at the timeout or when the goroutine's cohort is cancelled. The goroutine parks; other goroutines keep running.",
        },
        StdItem {
            name: "wait_writable",
            kind: StdItemKind::Function,
            doc: "`wait_writable(fd, timeout_ms) -> Result<bool, errors::Error>`: true when `fd` accepts a write within the timeout. Windows handles always do.",
        },
    ],
};

pub const FFI: StdModule = StdModule {
    path: "std::ffi",
    summary: "C type names and C strings for functions declared in `unsafe extern \"C\"` blocks, and the `errno` a foreign call left.",
    items: &[
        StdItem {
            name: "c_char",
            kind: StdItemKind::Type,
            doc: "C `char`: `i8`, or `u8` on Linux aarch64 and riscv64.",
        },
        StdItem {
            name: "c_schar",
            kind: StdItemKind::Type,
            doc: "C `signed char`: `i8`.",
        },
        StdItem {
            name: "c_uchar",
            kind: StdItemKind::Type,
            doc: "C `unsigned char`: `u8`.",
        },
        StdItem {
            name: "c_short",
            kind: StdItemKind::Type,
            doc: "C `short`: `i16`.",
        },
        StdItem {
            name: "c_ushort",
            kind: StdItemKind::Type,
            doc: "C `unsigned short`: `u16`.",
        },
        StdItem {
            name: "c_int",
            kind: StdItemKind::Type,
            doc: "C `int`: `i32`.",
        },
        StdItem {
            name: "c_uint",
            kind: StdItemKind::Type,
            doc: "C `unsigned int`: `u32`.",
        },
        StdItem {
            name: "c_long",
            kind: StdItemKind::Type,
            doc: "C `long`: `i64`, or `i32` on Windows.",
        },
        StdItem {
            name: "c_ulong",
            kind: StdItemKind::Type,
            doc: "C `unsigned long`: `u64`, or `u32` on Windows.",
        },
        StdItem {
            name: "c_longlong",
            kind: StdItemKind::Type,
            doc: "C `long long`: `i64`.",
        },
        StdItem {
            name: "c_ulonglong",
            kind: StdItemKind::Type,
            doc: "C `unsigned long long`: `u64`.",
        },
        StdItem {
            name: "size_t",
            kind: StdItemKind::Type,
            doc: "C `size_t`: `usize`.",
        },
        StdItem {
            name: "ssize_t",
            kind: StdItemKind::Type,
            doc: "C `ssize_t`: `isize`.",
        },
        StdItem {
            name: "c_float",
            kind: StdItemKind::Type,
            doc: "C `float`: `f32`.",
        },
        StdItem {
            name: "c_double",
            kind: StdItemKind::Type,
            doc: "C `double`: `f64`.",
        },
        StdItem {
            name: "cstring",
            kind: StdItemKind::Function,
            doc: "`cstring(text) -> Result<Vec<u8>, errors::Error>`: the bytes of `text` with a terminating NUL, to pass as `[u8]`; an error when `text` holds a NUL.",
        },
        StdItem {
            name: "from_cstr",
            kind: StdItemKind::Function,
            doc: "`from_cstr(bytes) -> Result<String, errors::Error>`: the UTF-8 text before the first NUL in `bytes`.",
        },
        StdItem {
            name: "last_errno",
            kind: StdItemKind::Function,
            doc: "`last_errno() -> i64`: the C `errno` the goroutine's most recent foreign call left, cleared just before the call and captured before anything else could change it, so a call that does not set it answers 0.",
        },
        StdItem {
            name: "last_os_error",
            kind: StdItemKind::Function,
            doc: "`last_os_error() -> i64`: the operating-system error code the goroutine's most recent foreign call left: `GetLastError` on Windows, `errno` elsewhere. Cleared just before the call, so a call that does not set it answers 0.",
        },
        StdItem {
            name: "c_void",
            kind: StdItemKind::Type,
            doc: "C `void`, as the pointee of `Ptr<c_void>` (`void *`): an opaque type with no value of its own.",
        },
        StdItem {
            name: "Ptr",
            kind: StdItemKind::Type,
            doc: "`Ptr<T>`: a non-null address of a `T` in memory Gossamer does not manage, as a foreign function takes or answers it; `Option<Ptr<T>>` is the nullable form. Holding, copying, comparing, storing, and sending one is safe; every access through it is `unsafe`. `p.cast::<U>()`, `p.address()`, `p.is_null()`, `unsafe { Ptr::from_address(n) }` (`None` for 0), and `Ptr::null()`, the NULL a `#[repr(C)]` struct field holds where C expects one (a table's terminating entry), which a `Ptr` copied out of C memory may be too.",
        },
        StdItem {
            name: "Handle",
            kind: StdItemKind::Type,
            doc: "`Handle<T>`: a value handed to native code as `void *` context and read back in a callback. `Handle::new(value)`, `h.as_ptr()`, `unsafe { Handle::<T>::from_ptr(p) }`, `h.get()`, `h.set(value)`, `h.update(|v| ..)`, `h.take()`, `h.release()`; a released or unknown handle is GX0014, never a read of freed memory.",
        },
        StdItem {
            name: "read",
            kind: StdItemKind::Function,
            doc: "`unsafe { read::<T>(p) }`: a copy of the `T` at `p`, for a scalar, a `Ptr`, or a `#[repr(C)]` plain-data struct.",
        },
        StdItem {
            name: "read_at",
            kind: StdItemKind::Function,
            doc: "`unsafe { read_at::<T>(p, i) }`: a copy of element `i` of the array of `T` at `p`.",
        },
        StdItem {
            name: "write",
            kind: StdItemKind::Function,
            doc: "`unsafe { write(p, value) }`: stores `value` at `p`.",
        },
        StdItem {
            name: "write_at",
            kind: StdItemKind::Function,
            doc: "`unsafe { write_at(p, i, value) }`: stores `value` as element `i` of the array of `T` at `p`.",
        },
        StdItem {
            name: "alloc",
            kind: StdItemKind::Function,
            doc: "`unsafe { alloc::<T>(count) }`: zeroed room for `count` values of `T` from the platform C allocator, freed with `free` or by a library that takes it over.",
        },
        StdItem {
            name: "size_of",
            kind: StdItemKind::Function,
            doc: "`size_of::<T>() -> i64`: the bytes a `T` occupies in C layout.",
        },
        StdItem {
            name: "align_of",
            kind: StdItemKind::Function,
            doc: "`align_of::<T>() -> i64`: the alignment, in bytes, of a `T` in C layout.",
        },
        StdItem {
            name: "offset_of",
            kind: StdItemKind::Function,
            doc: "`offset_of::<T>(\"field.path\") -> i64`: the byte offset of a field, or a field of a field, inside the `#[repr(C)]` struct `T`; the path is a string literal checked at compile time (GT0109).",
        },
        StdItem {
            name: "addr_of",
            kind: StdItemKind::Function,
            doc: "`addr_of(NAME) -> Ptr<T>`: the address of the C global `static NAME: T` an `unsafe extern` block declares, read and written with `ffi::read` and `ffi::write`; a foreign static is reached no other way (GT0111).",
        },
        StdItem {
            name: "View",
            kind: StdItemKind::Type,
            doc: "`View<T>`: `len` values of `T` in foreign memory, read and written in place. `unsafe { View::new(p, len) }` vouches that `p` addresses that many; `v.len()`, `v.get(i)` and `v[i]`, `v.set(i, value)`, `v.slice(lo, hi)`, `v.to_vec()`, `v.copy_from(values)`, `v.fill(value)`, and `v.ptr()` follow, each access bounds-checked against the length.",
        },
        StdItem {
            name: "atomic_load",
            kind: StdItemKind::Function,
            doc: "`unsafe { atomic_load(p) }`: the 32- or 64-bit integer at `p`, read atomically (sequentially consistent); an address not aligned to the integer's width panics.",
        },
        StdItem {
            name: "atomic_store",
            kind: StdItemKind::Function,
            doc: "`unsafe { atomic_store(p, value) }`: stores `value` at `p` atomically.",
        },
        StdItem {
            name: "atomic_swap",
            kind: StdItemKind::Function,
            doc: "`unsafe { atomic_swap(p, value) }`: stores `value` at `p` atomically and answers the value it replaced.",
        },
        StdItem {
            name: "atomic_compare_exchange",
            kind: StdItemKind::Function,
            doc: "`unsafe { atomic_compare_exchange(p, current, new) } -> Result<T, T>`: stores `new` at `p` when it holds `current`, atomically; `Ok` with the old value when it did, `Err` with the value found when it did not.",
        },
        StdItem {
            name: "atomic_fetch_add",
            kind: StdItemKind::Function,
            doc: "`unsafe { atomic_fetch_add(p, value) }`: adds `value` to the integer at `p` atomically, wrapping, and answers the value before.",
        },
        StdItem {
            name: "atomic_fetch_sub",
            kind: StdItemKind::Function,
            doc: "`unsafe { atomic_fetch_sub(p, value) }`: subtracts `value` atomically, wrapping, and answers the value before.",
        },
        StdItem {
            name: "atomic_fetch_and",
            kind: StdItemKind::Function,
            doc: "`unsafe { atomic_fetch_and(p, value) }`: ands `value` in atomically and answers the value before.",
        },
        StdItem {
            name: "atomic_fetch_or",
            kind: StdItemKind::Function,
            doc: "`unsafe { atomic_fetch_or(p, value) }`: ors `value` in atomically and answers the value before.",
        },
        StdItem {
            name: "atomic_fetch_xor",
            kind: StdItemKind::Function,
            doc: "`unsafe { atomic_fetch_xor(p, value) }`: xors `value` in atomically and answers the value before.",
        },
        StdItem {
            name: "Union",
            kind: StdItemKind::Type,
            doc: "`Union<(A, B, ..)>`: a C union of the listed members, sized and aligned for the largest, as a `#[repr(C)]` field, a pointee, or a foreign argument. `Union::new(v)`, `Union::zeroed()`, `u.get::<T>()`, and `u.set(v)` take and give the leading bytes as a member type (GT0110 for any other type).",
        },
        StdItem {
            name: "free",
            kind: StdItemKind::Function,
            doc: "`unsafe { free(p) }`: frees memory the platform C allocator handed out (`alloc`, `to_c_bytes`, or a library's `malloc`).",
        },
        StdItem {
            name: "read_bytes",
            kind: StdItemKind::Function,
            doc: "`unsafe { read_bytes(p, len) }`: a `Vec<u8>` copy of the `len` bytes at `p`.",
        },
        StdItem {
            name: "read_cstr",
            kind: StdItemKind::Function,
            doc: "`unsafe { read_cstr(p) } -> Result<String, errors::Error>`: the NUL-terminated text at `p`, copied into a `String`; an error when it is not UTF-8.",
        },
        StdItem {
            name: "write_bytes",
            kind: StdItemKind::Function,
            doc: "`unsafe { write_bytes(p, bytes) }`: copies `bytes` to the memory at `p`.",
        },
        StdItem {
            name: "to_c_bytes",
            kind: StdItemKind::Function,
            doc: "`unsafe { to_c_bytes(bytes) }`: a C-allocated copy of `bytes`, for native code to keep; free it or hand it to a library that takes it over.",
        },
        StdItem {
            name: "fn_addr",
            kind: StdItemKind::Function,
            doc: "`fn_addr(f) -> Ptr<c_void>`: the C function pointer for the named top-level function `f`, whose parameters and result have C forms (GT0107, GT0108 otherwise): a value for a function-pointer field of a C struct (an ops table, a `luaL_Reg` array). Native code calls it as it calls a callback.",
        },
        StdItem {
            name: "fn_from_ptr",
            kind: StdItemKind::Function,
            doc: "`unsafe { fn_from_ptr::<Fn(A..) -> R>(p) }`: a callable that calls the native function at `p` (a `dlsym` or `GetProcAddress` result) with that C signature.",
        },
    ],
};

pub const TERM: StdModule = StdModule {
    path: "std::term",
    summary: "The terminal a program runs in: detection, size, raw mode, and input, on the standard streams or on a descriptor the program opened (`fs::File::fd` of `/dev/tty` or `CONIN$`). Written in Gossamer over the platform C library.",
    items: &[
        StdItem {
            name: "STDIN",
            kind: StdItemKind::Const,
            doc: "Standard input's descriptor (0), the default for `enter_raw` and `read_input`.",
        },
        StdItem {
            name: "STDOUT",
            kind: StdItemKind::Const,
            doc: "Standard output's descriptor (1), the default for `size` and `resized`.",
        },
        StdItem {
            name: "STDERR",
            kind: StdItemKind::Const,
            doc: "Standard error's descriptor (2).",
        },
        StdItem {
            name: "RawMode",
            kind: StdItemKind::Type,
            doc: "Returned by `enter_raw`; `restore()` puts the terminal back. Restoration also runs when the program ends, by any path. A raw mode entered on an `fs::File` keeps the file open.",
        },
        StdItem {
            name: "is_terminal",
            kind: StdItemKind::Function,
            doc: "`is_terminal(fd) -> bool`: whether `fd`, an `fs::File` or a standard stream, is a terminal.",
        },
        StdItem {
            name: "size",
            kind: StdItemKind::Function,
            doc: "`size(fd = STDOUT) -> Result<(i64, i64), errors::Error>`: the terminal's columns and rows. A standard stream that is not a terminal falls back to the other two.",
        },
        StdItem {
            name: "enter_raw",
            kind: StdItemKind::Function,
            doc: "`enter_raw(fd = STDIN) -> Result<RawMode, errors::Error>`: input on `fd` arrives byte by byte without echo or line editing, and output is not translated; the mode is restored when the program ends.",
        },
        StdItem {
            name: "read_input",
            kind: StdItemKind::Function,
            doc: "`read_input(timeout_ms, fd = STDIN) -> Result<Vec<u8>, errors::Error>`: the bytes of input available on `fd` within `timeout_ms` milliseconds (negative waits indefinitely), empty at the timeout or when the goroutine's cohort is cancelled. The goroutine parks while it waits.",
        },
        StdItem {
            name: "resized",
            kind: StdItemKind::Function,
            doc: "`resized(fd = STDOUT) -> bool`: whether the size changed since the last call; the first call answers false.",
        },
    ],
};

pub const PATH: StdModule = StdModule {
    path: "std::path",
    summary: "Lexical filesystem-path operations; platform path grammar, no URL parsing.",
    items: &[
        StdItem {
            name: "join",
            kind: StdItemKind::Function,
            doc: "Joins two path fragments.",
        },
        StdItem {
            name: "walk",
            kind: StdItemKind::Function,
            doc: "Recursively visits every descendant entry under a directory, the path-module spelling of fs::walk_dir.",
        },
        StdItem {
            name: "components",
            kind: StdItemKind::Function,
            doc: "Returns Rust-like lexical path components.",
        },
        StdItem {
            name: "prefixes",
            kind: StdItemKind::Function,
            doc: "Returns cumulative Rust-like lexical path prefixes.",
        },
        StdItem {
            name: "unique_prefixes",
            kind: StdItemKind::Function,
            doc: "Returns sorted unique prefixes for newline-delimited paths.",
        },
        StdItem {
            name: "split",
            kind: StdItemKind::Function,
            doc: "Returns (dir, file) for the supplied path.",
        },
        StdItem {
            name: "parent",
            kind: StdItemKind::Function,
            doc: "Parent directory, or None at the root.",
        },
        StdItem {
            name: "file_name",
            kind: StdItemKind::Function,
            doc: "Final path component, or None.",
        },
        StdItem {
            name: "file_stem",
            kind: StdItemKind::Function,
            doc: "File name without its extension.",
        },
        StdItem {
            name: "extension",
            kind: StdItemKind::Function,
            doc: "Dotted extension as an Option.",
        },
        StdItem {
            name: "is_absolute",
            kind: StdItemKind::Function,
            doc: "Reports whether the path is absolute.",
        },
        StdItem {
            name: "normalize",
            kind: StdItemKind::Function,
            doc: "Lexically normalizes the path.",
        },
        StdItem {
            name: "starts_with",
            kind: StdItemKind::Function,
            doc: "Reports whether the path begins with a prefix component-wise.",
        },
        StdItem {
            name: "matches",
            kind: StdItemKind::Function,
            doc: "`matches(pattern, name) -> bool` - Go `filepath.Match` shell-glob test over a single path segment: `*` and `?` never cross a `/`, `[abc]` is a character class. Spelled `matches` because `match` is a keyword. Example: `path::matches(\"*.gos\", \"main.gos\")` is true, `path::matches(\"a*c\", \"a/c\")` is false.",
        },
        StdItem {
            name: "glob",
            kind: StdItemKind::Function,
            doc: "`glob(pattern) -> Result<Vec<String>, errors::Error>` - filesystem paths matching a shell glob, sorted so every tier reports the same order. Supports `*`, `?`, `[abc]`, and `**` (this directory and every descendant). Relative patterns resolve against the working directory. Example: `let found = path::glob(\"src/**/*.gos\")?`.",
        },
    ],
};

pub const FS: StdModule = StdModule {
    path: "std::fs",
    summary: "Filesystem reading, writing, and traversal (Rust std::fs shape).",
    items: &[
        StdItem {
            name: "File",
            kind: StdItemKind::Type,
            doc: "Streaming file handle. Reads and writes at the handle's own cursor (read, read_to_string, write, write_bytes, seek), positionally (read_at, read_at_into, write_at), and reports size (len, set_len). Durability is sync_all / sync_data; multi-process safety is the try_lock_* / unlock family. The file closes at `close()` or with its last handle. `std::term` and `os::fd` calls take the file itself, which keeps it open while they use it; `fd()` answers the OS descriptor (a handle on Windows) for a foreign call.",
        },
        StdItem {
            name: "DirInfo",
            kind: StdItemKind::Type,
            doc: "Directory entry yielded by read_dir and walk_dir; carries path, name, is_file, is_dir, is_symlink, and size.",
        },
        StdItem {
            name: "OpenOptions",
            kind: StdItemKind::Type,
            doc: "Builder for opening files with read/write/append/create/truncate flags.",
        },
        StdItem {
            name: "open",
            kind: StdItemKind::Function,
            doc: "Opens a file for streaming reads.",
        },
        StdItem {
            name: "create",
            kind: StdItemKind::Function,
            doc: "Creates or truncates a file and returns a streaming file handle.",
        },
        StdItem {
            name: "temp_dir",
            kind: StdItemKind::Function,
            doc: "Creates a unique temporary directory; the caller removes it explicitly.",
        },
        StdItem {
            name: "temp_file",
            kind: StdItemKind::Function,
            doc: "Creates a unique temporary file and returns its handle plus path.",
        },
        StdItem {
            name: "read",
            kind: StdItemKind::Function,
            doc: "Reads an entire file into memory as bytes.",
        },
        StdItem {
            name: "read_to_string",
            kind: StdItemKind::Function,
            doc: "Reads an entire file into memory as UTF-8 text.",
        },
        StdItem {
            name: "write",
            kind: StdItemKind::Function,
            doc: "Writes bytes to a file, creating or truncating it.",
        },
        StdItem {
            name: "read_dir",
            kind: StdItemKind::Function,
            doc: "Returns immediate children as DirInfo values. Inspect their metadata fields directly; each path can be passed back to filesystem APIs.",
        },
        StdItem {
            name: "walk_dir",
            kind: StdItemKind::Function,
            doc: "Recursively visits every descendant entry.",
        },
        StdItem {
            name: "create_dir",
            kind: StdItemKind::Function,
            doc: "Creates a single directory. Fails if any parent is missing.",
        },
        StdItem {
            name: "create_dir_all",
            kind: StdItemKind::Function,
            doc: "Creates a directory and any missing ancestors.",
        },
        StdItem {
            name: "create_dir_mode",
            kind: StdItemKind::Function,
            doc: "Creates a single directory with exactly this mode, whatever the umask is. \
                  On Windows only the owner write bit is meaningful: it sets the read-only \
                  attribute.",
        },
        StdItem {
            name: "create_dir_all_mode",
            kind: StdItemKind::Function,
            doc: "Creates a directory and any missing ancestors, giving each one it creates \
                  exactly this mode.",
        },
        StdItem {
            name: "write_mode",
            kind: StdItemKind::Function,
            doc: "Writes a file and leaves it at exactly this mode, whatever the umask is.",
        },
        StdItem {
            name: "permissions",
            kind: StdItemKind::Function,
            doc: "The permission bits of a path, in the chmod(2) encoding. On Windows the \
                  read-only attribute is widened into the bits an equivalent Unix path \
                  would carry.",
        },
        StdItem {
            name: "set_permissions",
            kind: StdItemKind::Function,
            doc: "Sets the permission bits of a path, in the chmod(2) encoding. On Windows \
                  only the owner write bit is meaningful: it sets or clears the read-only \
                  attribute.",
        },
        StdItem {
            name: "remove_file",
            kind: StdItemKind::Function,
            doc: "Removes a single file.",
        },
        StdItem {
            name: "remove_dir",
            kind: StdItemKind::Function,
            doc: "Removes an empty directory.",
        },
        StdItem {
            name: "remove_dir_all",
            kind: StdItemKind::Function,
            doc: "Recursively removes a directory and its contents.",
        },
        StdItem {
            name: "copy",
            kind: StdItemKind::Function,
            doc: "Copies a file, creating parent dirs as needed.",
        },
        StdItem {
            name: "rename",
            kind: StdItemKind::Function,
            doc: "Renames a file or directory.",
        },
        StdItem {
            name: "exists",
            kind: StdItemKind::Function,
            doc: "Returns whether a path exists on the filesystem.",
        },
        StdItem {
            name: "is_file",
            kind: StdItemKind::Function,
            doc: "Returns whether a path exists and is a regular file.",
        },
        StdItem {
            name: "is_dir",
            kind: StdItemKind::Function,
            doc: "Returns whether a path exists and is a directory.",
        },
        StdItem {
            name: "is_symlink",
            kind: StdItemKind::Function,
            doc: "Returns whether a path exists and is a symbolic link.",
        },
        StdItem {
            name: "file_size",
            kind: StdItemKind::Function,
            doc: "Returns the file's size in bytes; 0 on error.",
        },
        StdItem {
            name: "metadata",
            kind: StdItemKind::Function,
            doc: "Returns filesystem metadata for a path.",
        },
        StdItem {
            name: "sync_dir",
            kind: StdItemKind::Function,
            doc: "Makes a directory's own entries durable - the barrier a create, rename, or delete needs after the file itself is synced. On Windows this is satisfied by NTFS metadata ordering and performs no flush.",
        },
        StdItem {
            name: "SEEK_SET",
            kind: StdItemKind::Const,
            doc: "File::seek whence: the offset is absolute from the start of the file.",
        },
        StdItem {
            name: "SEEK_CUR",
            kind: StdItemKind::Const,
            doc: "File::seek whence: the offset is relative to the current position.",
        },
        StdItem {
            name: "SEEK_END",
            kind: StdItemKind::Const,
            doc: "File::seek whence: the offset is relative to the end of the file.",
        },
        StdItem {
            name: "canonicalize",
            kind: StdItemKind::Function,
            doc: "Resolves a path to an absolute, symlink-free canonical form.",
        },
    ],
};

pub const BYTES: StdModule = StdModule {
    path: "std::bytes",
    summary: "Byte buffers, builders, and slice helpers.",
    items: &[
        StdItem {
            name: "Buffer",
            kind: StdItemKind::Type,
            doc: "Growable byte buffer for incremental assembly: new, with_capacity, write_str, push, len, is_empty, clear, to_string. A buffer you index, slice, or edit at an offset is a Vec<u8>.",
        },
        StdItem {
            name: "Builder",
            kind: StdItemKind::Type,
            doc: "Incremental string builder: new, write, write_char, len, build, as_str. Cheaper than repeated `+` on a String, which copies.",
        },
        StdItem {
            name: "index_of",
            kind: StdItemKind::Function,
            doc: "First occurrence of a byte needle.",
        },
        StdItem {
            name: "split",
            kind: StdItemKind::Function,
            doc: "Splits on every separator occurrence.",
        },
        StdItem {
            name: "replace",
            kind: StdItemKind::Function,
            doc: "Replaces every occurrence of a byte needle.",
        },
    ],
};

pub const BUFIO: StdModule = StdModule {
    path: "std::bufio",
    summary: "Buffered readers, writers, and line scanners.",
    items: &[
        StdItem {
            name: "Reader",
            kind: StdItemKind::Type,
            doc: "Buffered reader.",
        },
        StdItem {
            name: "Writer",
            kind: StdItemKind::Type,
            doc: "Buffered writer.",
        },
        StdItem {
            name: "Scanner",
            kind: StdItemKind::Type,
            doc: "Line / token scanner.",
        },
        StdItem {
            name: "read_lines",
            kind: StdItemKind::Function,
            doc: "Reads every line from a file path; one-shot convenience over the streaming Scanner.",
        },
        StdItem {
            name: "read_lines_of",
            kind: StdItemKind::Function,
            doc: "Reads every line of a file path into a Vec<String>.",
        },
        StdItem {
            name: "read_to_string",
            kind: StdItemKind::Function,
            doc: "Reads an entire file path into a String.",
        },
        StdItem {
            name: "split_whitespace",
            kind: StdItemKind::Function,
            doc: "Splits a String on runs of whitespace.",
        },
    ],
};

pub const IO: StdModule = StdModule {
    path: "std::io",
    summary: "Stream-oriented I/O abstractions and process standard streams.",
    items: &[
        StdItem {
            name: "Reader",
            kind: StdItemKind::Trait,
            doc: "Pull-style byte source.",
        },
        StdItem {
            name: "Writer",
            kind: StdItemKind::Trait,
            doc: "Push-style byte sink.",
        },
        StdItem {
            name: "BufReader",
            kind: StdItemKind::Type,
            doc: "Buffered wrapper around any `Reader`.",
        },
        StdItem {
            name: "BufWriter",
            kind: StdItemKind::Type,
            doc: "Buffered wrapper around any `Writer`.",
        },
        StdItem {
            name: "stdin",
            kind: StdItemKind::Function,
            doc: "Returns a handle to the process's standard input stream. Use read_line(&mut String) for interactive prompts.",
        },
        StdItem {
            name: "stdout",
            kind: StdItemKind::Function,
            doc: "Returns a handle to the process's standard output stream.",
        },
        StdItem {
            name: "stderr",
            kind: StdItemKind::Function,
            doc: "Returns a handle to the process's standard error stream.",
        },
        StdItem {
            name: "ReadAll",
            kind: StdItemKind::Function,
            doc: "Drains a reader to a String. Mirrors Go's io.ReadAll.",
        },
        StdItem {
            name: "Copy",
            kind: StdItemKind::Function,
            doc: "Copies all bytes from src to dst; returns the byte count.",
        },
        StdItem {
            name: "Error",
            kind: StdItemKind::Type,
            doc: "Errors raised by I/O operations.",
        },
        StdItem {
            name: "string_reader",
            kind: StdItemKind::Function,
            doc: "`string_reader(text: String) -> i64` - a Reader handle over an in-memory buffer. Reader and Writer handles are plain integers, so the adapters below compose by value. Example: `let src = io::string_reader(\"hello\")`.",
        },
        StdItem {
            name: "buffer_writer",
            kind: StdItemKind::Function,
            doc: "`buffer_writer() -> i64` - a Writer handle collecting everything written to it; read it back with `io::contents`.",
        },
        StdItem {
            name: "limit_reader",
            kind: StdItemKind::Function,
            doc: "`limit_reader(src: i64, limit: i64) -> i64` - a Reader yielding at most `limit` bytes from `src`, Go's `io.LimitReader`. Example: `io::drain(io::limit_reader(src, 5))`.",
        },
        StdItem {
            name: "tee_reader",
            kind: StdItemKind::Function,
            doc: "`tee_reader(src: i64, sink: i64) -> i64` - a Reader mirroring every byte read from `src` into the Writer `sink`, Go's `io.TeeReader`.",
        },
        StdItem {
            name: "multi_reader",
            kind: StdItemKind::Function,
            doc: "`multi_reader(sources: Vec<i64>) -> i64` - a Reader draining each source in turn, Go's `io.MultiReader`. Example: `io::multi_reader(#[a, b])`.",
        },
        StdItem {
            name: "pipe",
            kind: StdItemKind::Function,
            doc: "`pipe() -> (i64, i64)` - a connected `(reader, writer)` pair sharing one in-memory buffer. Reads return the bytes written so far and never block; `io::close_writer` marks the writer done. Example: `let r, w = io::pipe()`.",
        },
        StdItem {
            name: "copy_n",
            kind: StdItemKind::Function,
            doc: "`copy_n(dst: i64, src: i64, n: i64) -> Result<i64, errors::Error>` - copies at most `n` bytes and returns the count actually transferred. Go's `io.CopyN`.",
        },
        StdItem {
            name: "drain",
            kind: StdItemKind::Function,
            doc: "`drain(src: i64) -> String` - reads a Reader handle to end of stream as UTF-8 text.",
        },
        StdItem {
            name: "contents",
            kind: StdItemKind::Function,
            doc: "`contents(writer: i64) -> String` - everything written to a buffer or pipe Writer, as UTF-8 text.",
        },
        StdItem {
            name: "write",
            kind: StdItemKind::Function,
            doc: "`write(writer: i64, text: String) -> i64` - appends text to a Writer handle and returns the byte count accepted.",
        },
        StdItem {
            name: "close_writer",
            kind: StdItemKind::Function,
            doc: "`close_writer(writer: i64)` - signals end of stream on a pipe Writer; later writes are rejected.",
        },
    ],
};

pub const OS: StdModule = StdModule {
    path: "std::os",
    summary: "Operating-system identity.",
    items: &[
        StdItem {
            name: "family",
            kind: StdItemKind::Function,
            doc: "Returns \"unix\" or \"windows\" for the running OS family.",
        },
        StdItem {
            name: "arch",
            kind: StdItemKind::Function,
            doc: "Returns the target CPU architecture (e.g. \"x86_64\").",
        },
    ],
};

pub const PROCESS: StdModule = StdModule {
    path: "std::process",
    summary: "Canonical process control and child-process API; std::os::exec is compatibility-only.",
    items: &[
        StdItem {
            name: "Command",
            kind: StdItemKind::Type,
            doc: "Builder for a child process: `Command::new(program)` then `arg` / `args`, `env` / `env_remove` / `env_clear`, `dir`, `stdin` / `stdout` / `stderr` (a `Stdio`), `new_process_group`, and on POSIX `new_session` and `controlling_terminal(fd)`; `spawn() -> Result<Child, errors::Error>`, `output() -> Result<Output, errors::Error>` (stdout and stderr read at once, so neither pipe stalls the child), and `status() -> Result<i64, errors::Error>`. Rust's `std::process::Command`, Go's `exec.Cmd`.",
        },
        StdItem {
            name: "Stdio",
            kind: StdItemKind::Type,
            doc: "Where a child's standard stream goes: `Stdio::Inherit` (this process's), `Stdio::Null`, `Stdio::Piped` (read or written through the `Child`), or `Stdio::File(f)` for an open `fs::File`.",
        },
        StdItem {
            name: "Child",
            kind: StdItemKind::Type,
            doc: "A running child: `id`, `write_stdin` / `write_stdin_bytes` / `close_stdin`, `read_line` / `read_stdout` / `read_stderr` and the `read_stdout_line` / `read_stderr_line` / `read_stdout_chunk(max)` / `read_stderr_chunk(max)` forms (each stream may be drained from its own goroutine), `wait`, `wait_timeout(ms)`, `kill`, and on POSIX `signal(sig)` and `kill_group(sig)` for a child started in its own process group or session. A child that has ended answers 128 plus the signal number when a signal ended it.",
        },
        StdItem {
            name: "run",
            kind: StdItemKind::Function,
            doc: "One-shot: runs a program with args, captures stdout/stderr plus the exit code.",
        },
        StdItem {
            name: "run_in",
            kind: StdItemKind::Function,
            doc: "run_in(program, args, dir, env): the same one-shot run with the \
                  child's working directory and environment supplied. An empty dir \
                  inherits the caller's; the env pairs override the inherited \
                  environment rather than replacing it, so a caller sets the two \
                  variables it cares about without restating PATH.",
        },
        StdItem {
            name: "run_inherit",
            kind: StdItemKind::Function,
            doc: "`run_inherit(program, args) -> Result<i64, errors::Error>`: runs the program on this \
                  process's own standard input, output, and error - a terminal included - and \
                  answers its exit code once it ends (128 plus the signal number for one a signal \
                  ended on Unix). The goroutine parks meanwhile. Go's `exec.Cmd.Run` with \
                  `os.Stdin`, `os.Stdout`, and `os.Stderr` attached.",
        },
        StdItem {
            name: "spawn",
            kind: StdItemKind::Function,
            doc: "Spawns a child process and returns its PID.",
        },
        StdItem {
            name: "spawn_piped",
            kind: StdItemKind::Function,
            doc: "Spawns a child with piped stdin/stdout; returns Result<Child, errors::Error>. The Child's write_stdin / close_stdin / read_line / read_stdout / wait / kill methods drive it interactively.",
        },
        StdItem {
            name: "kill",
            kind: StdItemKind::Function,
            doc: "Sends SIGKILL (or equivalent) to a Child.",
        },
        StdItem {
            name: "exit",
            kind: StdItemKind::Function,
            doc: "Exits the current process with the given status code.",
        },
        StdItem {
            name: "id",
            kind: StdItemKind::Function,
            doc: "Returns the current process ID.",
        },
        StdItem {
            name: "abort",
            kind: StdItemKind::Function,
            doc: "Aborts the current process without unwinding.",
        },
        StdItem {
            name: "signal",
            kind: StdItemKind::Function,
            doc: "Sends a signal to a process by PID (POSIX).",
        },
        StdItem {
            name: "kill_group",
            kind: StdItemKind::Function,
            doc: "Sends a signal to a process group (POSIX).",
        },
        StdItem {
            name: "wait_timeout",
            kind: StdItemKind::Function,
            doc: "Waits for a child with a timeout (POSIX).",
        },
        StdItem {
            name: "pipeline_run",
            kind: StdItemKind::Function,
            doc: "Runs a shell-tokenised pipeline and returns captured stdout/stderr plus the final exit code.",
        },
    ],
};

pub const ENV: StdModule = StdModule {
    path: "std::env",
    summary: "Process environment, command-line arguments, working directory.",
    items: &[
        StdItem {
            name: "args",
            kind: StdItemKind::Function,
            doc: "Returns the program's command-line arguments.",
        },
        StdItem {
            name: "program_name",
            kind: StdItemKind::Function,
            doc: "Returns the path used to invoke the program (argv[0]).",
        },
        StdItem {
            name: "var",
            kind: StdItemKind::Function,
            doc: "Returns the value of an environment variable.",
        },
        StdItem {
            name: "set_var",
            kind: StdItemKind::Function,
            doc: "Sets an environment variable in the current process.",
        },
        StdItem {
            name: "unset_var",
            kind: StdItemKind::Function,
            doc: "Removes an environment variable from the current process.",
        },
        StdItem {
            name: "current_dir",
            kind: StdItemKind::Function,
            doc: "Returns the current working directory.",
        },
        StdItem {
            name: "set_current_dir",
            kind: StdItemKind::Function,
            doc: "Changes the current working directory.",
        },
        StdItem {
            name: "home_dir",
            kind: StdItemKind::Function,
            doc: "Returns the calling user's home directory if known.",
        },
        StdItem {
            name: "temp_dir",
            kind: StdItemKind::Function,
            doc: "Returns the system's temporary directory.",
        },
        StdItem {
            name: "vars",
            kind: StdItemKind::Function,
            doc: "vars() -> Map<String, String>. Every environment variable this process has, as a snapshot.",
        },
    ],
};

pub const OS_USER: StdModule = StdModule {
    path: "std::os::user",
    summary: "POSIX user / group lookup. Unix-backed by `nix`; Windows falls back to env vars.",
    items: &[
        StdItem {
            name: "current_name",
            kind: StdItemKind::Function,
            doc: "Login name of the current process user, or empty string.",
        },
        StdItem {
            name: "current_uid",
            kind: StdItemKind::Function,
            doc: "uid of the current process user, or -1 on non-unix.",
        },
        StdItem {
            name: "current_gid",
            kind: StdItemKind::Function,
            doc: "gid of the current process user, or -1 on non-unix.",
        },
        StdItem {
            name: "current_home",
            kind: StdItemKind::Function,
            doc: "Home directory of the current process user.",
        },
        StdItem {
            name: "lookup_uid",
            kind: StdItemKind::Function,
            doc: "Login name for the given uid, or empty string if unknown.",
        },
        StdItem {
            name: "lookup_name",
            kind: StdItemKind::Function,
            doc: "uid for the user with the given login name, or -1 if not found.",
        },
    ],
};

pub const LIFECYCLE: StdModule = StdModule {
    path: "std::lifecycle",
    summary: "Process readiness and graceful shutdown, with systemd sd_notify. Shutdown is observed, not dispatched: wait for it, then drain with ordinary statements - `spawn(|| serve())`, `lifecycle::ready()`, `lifecycle::await_shutdown()`, then the cleanup.",
    items: &[
        StdItem {
            name: "ready",
            kind: StdItemKind::Function,
            doc: "`ready()` - declare the process ready to serve traffic; emits `sd_notify(READY=1)` under a service manager.",
        },
        StdItem {
            name: "set_ready",
            kind: StdItemKind::Function,
            doc: "`set_ready(ready: bool)` - set readiness explicitly. Readiness also drops on its own when shutdown begins.",
        },
        StdItem {
            name: "is_ready",
            kind: StdItemKind::Function,
            doc: "`is_ready() -> bool` - whether the process is ready to serve. False before `ready()` and once shutdown has begun, so a readiness probe fails ahead of the drain.",
        },
        StdItem {
            name: "shutdown",
            kind: StdItemKind::Function,
            doc: "`shutdown()` - begin the graceful shutdown sequence: readiness drops and every server stops accepting while in-flight requests finish.",
        },
        StdItem {
            name: "is_shutting_down",
            kind: StdItemKind::Function,
            doc: "`is_shutting_down() -> bool` - whether shutdown has begun. A long-running worker polls it to leave on its own terms.",
        },
        StdItem {
            name: "await_shutdown",
            kind: StdItemKind::Function,
            doc: "`await_shutdown()` - block until shutdown begins, whether from SIGTERM, SIGINT, or `shutdown()`. The statements after it are the drain sequence.",
        },
        StdItem {
            name: "notify_status",
            kind: StdItemKind::Function,
            doc: "`notify_status(message: String)` - report free-text status to the service manager (`sd_notify(STATUS=...)`).",
        },
    ],
};
