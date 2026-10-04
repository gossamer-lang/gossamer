# `std::os::fd`

Status: experimental

Waiting for a file descriptor (a handle on Windows) to be readable or writable without holding a scheduler worker.

## Items

| Item | Signature | Description |
|---|---|---|
| `wait_readable` | `fn wait_readable(fd: i64, timeout_ms: i64) -> Result<bool, errors::Error>` | `wait_readable(fd, timeout_ms) -> Result<bool, errors::Error>`: true when `fd` has input (or reached end of input) within `timeout_ms` milliseconds; a negative timeout waits indefinitely. False at the timeout or when the goroutine's cohort is cancelled. The goroutine parks; other goroutines keep running. |
| `wait_writable` | `fn wait_writable(fd: i64, timeout_ms: i64) -> Result<bool, errors::Error>` | `wait_writable(fd, timeout_ms) -> Result<bool, errors::Error>`: true when `fd` accepts a write within the timeout. Windows handles always do. |
