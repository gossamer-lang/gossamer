# `std::time`

Status: experimental

Wall-clock and monotonic time facilities.

## Items

| Item | Signature | Description |
|---|---|---|
| `Instant` | `type Instant` | A monotonic clock reading in nanoseconds: `now()`, `elapsed()`, `elapsed_ms()`, `duration_since(earlier)`. |
| `Duration` | `type Duration` | A span of nanoseconds: `from_nanos` / `from_micros` / `from_millis` / `from_secs` / `from_secs_f64`, the matching `as_*` accessors, `+`, `-`, and ordering. |
| `SystemTime` | `type SystemTime` | Wall-clock point-in-time. |
| `CivilTime` | `type CivilTime` | Calendar fields interpreted only with an explicit location. |
| `CivilResolution` | `type CivilResolution` | Explicit unique, gap, or fold result of resolving civil fields. |
| `Location` | `type Location` | Immutable IANA or fixed-offset time-zone location. |
| `Time` | `type Time` | A wall-clock instant with the location it is read in: `now()`, `from_unix` / `from_unix_ms` / `from_unix_nanos`, `parse_rfc3339` (keeps the written offset), `format_rfc3339`, `civil()`, `in_location` / `utc`, `+` / `-` a `Duration`, and `a - b` answering the `Duration` between two instants. |
| `sleep` | `fn sleep(ms: i64) -> ()` | Suspends the current goroutine for a `Duration`, or for an integer count of milliseconds. |
| `after` | `fn after(d: time::Duration) -> Receiver<i64>` | A `Receiver` that yields once after `Duration`, so a deadline composes with `select` and `while let` the way any other channel does. |
| `sleep_ctx` | `fn sleep_ctx(ctx: context::Context, ms: i64) -> bool` | Sleeps unless the context fires first; false when it cancelled the wait. |
| `__sleep_ns` | `builtin __sleep_ns` | Internal: suspends the current goroutine for a nanosecond count; the lowering of `sleep` and `sleep_ctx`. |
| `__sleep_ns_ctx` | `builtin __sleep_ns_ctx` | Internal: context-aware nanosecond sleep; the lowering of `sleep_ctx` that wakes early when the context fires. |
| `now` | `fn now() -> i64` | Wall-clock milliseconds since the Unix epoch; `Instant::now()` reads the monotonic clock. |
| `format_rfc3339` | `fn format_rfc3339(ms: i64) -> Result<String, errors::Error>` | Formats a `SystemTime` in RFC 3339 (`YYYY-MM-DDTHH:MM:SSZ`). |
| `format_in` | `fn format_in(layout: String, unix_ms: i64, location: time::Location) -> Result<String, errors::Error>` | Formats an instant in an explicit civil-time location. |
| `add_date` | `fn add_date(unix_ms: i64, location: time::Location, years: i64, months: i64, days: i64) -> Result<i64, errors::Error>` | Adds calendar units and rejects ambiguous or nonexistent results. |
| `parse_rfc3339` | `fn parse_rfc3339(text: String) -> Result<i64, errors::Error>` | Parses an RFC 3339 timestamp into a `SystemTime`. |
| `freeze` | `fn freeze(ms: i64) -> ()` | `freeze(ms)` - pin the wall clock at `ms` since the epoch, so a test over anything time-dependent - a token's expiry, a rate-limit window, a cache's TTL - reads a clock it controls. The monotonic clock and `sleep` are untouched: this pins what the program is told the time is, not how long anything takes. |
| `advance` | `fn advance(ms: i64) -> i64` | `advance(ms) -> i64` - move a frozen wall clock forward and answer the new reading. Freezes at the current reading first when it is not already frozen. |
| `unfreeze` | `fn unfreeze() -> ()` | `unfreeze()` - return to the real wall clock. |
| `is_frozen` | `fn is_frozen() -> bool` | `is_frozen() -> bool` - whether the wall clock is pinned. |
| `now_ms` | `fn now_ms() -> i64` | Wall-clock milliseconds since the Unix epoch. |
| `now_nanos` | `fn now_nanos() -> i64` | Wall-clock nanoseconds since the Unix epoch. |
| `unix_ms` | `fn unix_ms() -> i64` | Current Unix time in milliseconds. |
| `monotonic_ms` | `fn monotonic_ms() -> i64` | Monotonic clock reading in milliseconds. |
| `monotonic_nanos` | `fn monotonic_nanos() -> i64` | Monotonic clock reading in nanoseconds. |
| `since_ms` | `fn since_ms(start_ms: i64) -> i64` | Milliseconds elapsed since an earlier monotonic reading. |

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->
