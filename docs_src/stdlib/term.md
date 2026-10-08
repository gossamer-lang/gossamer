# `std::term`

Status: experimental

The terminal a program runs in: detection, size, raw mode, and input, on the standard streams or on a descriptor the program opened (`fs::File::fd` of `/dev/tty` or `CONIN$`). Written in Gossamer over the platform C library.

## Items

| Item | Signature | Description |
|---|---|---|
| `STDIN` | `const STDIN` | Standard input's descriptor (0), the default for `enter_raw` and `read_input`. |
| `STDOUT` | `const STDOUT` | Standard output's descriptor (1), the default for `size` and `resized`. |
| `STDERR` | `const STDERR` | Standard error's descriptor (2). |
| `RawMode` | `type RawMode` | Returned by `enter_raw`; `restore()` puts the terminal back. Restoration also runs when the program ends, by any path. A raw mode entered on an `fs::File` keeps the file open. |
| `is_terminal` | `fn is_terminal<D: os::fd::Descriptor>(fd: D) -> bool` | `is_terminal(fd) -> bool`: whether `fd`, an `fs::File` or a standard stream, is a terminal. |
| `size` | `fn size<D: os::fd::Descriptor>(fd: D = term::STDOUT) -> Result<(i64, i64), errors::Error>` | `size(fd = STDOUT) -> Result<(i64, i64), errors::Error>`: the terminal's columns and rows. A standard stream that is not a terminal falls back to the other two. |
| `enter_raw` | `fn enter_raw<D: os::fd::Descriptor>(fd: D = term::STDIN) -> Result<term::RawMode, errors::Error>` | `enter_raw(fd = STDIN) -> Result<RawMode, errors::Error>`: input on `fd` arrives byte by byte without echo or line editing, and output is not translated; the mode is restored when the program ends. |
| `read_input` | `fn read_input<D: os::fd::Descriptor>(timeout_ms: i64, fd: D = term::STDIN) -> Result<Vec<u8>, errors::Error>` | `read_input(timeout_ms, fd = STDIN) -> Result<Vec<u8>, errors::Error>`: the bytes of input available on `fd` within `timeout_ms` milliseconds (negative waits indefinitely), empty at the timeout or when the goroutine's cohort is cancelled. The goroutine parks while it waits. |
| `resized` | `fn resized<D: os::fd::Descriptor>(fd: D = term::STDOUT) -> bool` | `resized(fd = STDOUT) -> bool`: whether the size changed since the last call; the first call answers false. |
