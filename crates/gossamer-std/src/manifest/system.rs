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
            doc: "Returned by `on(sig)`; supports wait / try_wait.",
        },
        StdItem {
            name: "on",
            kind: StdItemKind::Function,
            doc: "Subscribes to a signal; returns a Notifier.",
        },
        StdItem {
            name: "wait",
            kind: StdItemKind::Function,
            doc: "Blocks the calling goroutine until the subscribed signal fires.",
        },
        StdItem {
            name: "try_wait",
            kind: StdItemKind::Function,
            doc: "Non-blocking poll: returns true if the subscribed signal has fired.",
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
            doc: "Streaming file handle. Reads and writes at the handle's own cursor (read, read_to_string, write, write_bytes, seek), positionally (read_at, read_at_into, write_at), and reports size (len, set_len). Durability is sync_all / sync_data; multi-process safety is the try_lock_* / unlock family.",
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
            name: "Child",
            kind: StdItemKind::Type,
            doc: "Handle to a still-running child supporting wait / kill.",
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
