//! Runtime support for `std::time`.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use std::time::SystemTime as StdSystemTime;

use std::time::Duration as StdDuration;

use gossamer_runtime::platform::Instant as StdInstant;

/// Monotonic point-in-time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(StdInstant);

impl Instant {
    /// Returns the current monotonic instant.
    #[must_use]
    pub fn now() -> Self {
        Self(StdInstant::now())
    }

    /// Returns the duration elapsed since `earlier`, saturating at
    /// zero if `earlier` is in the future.
    #[must_use]
    pub fn duration_since(self, earlier: Self) -> Duration {
        Duration(self.0.saturating_duration_since(earlier.0))
    }

    /// Returns how much time has elapsed since this instant was
    /// captured.
    #[must_use]
    pub fn elapsed(self) -> Duration {
        Duration(self.0.elapsed())
    }
}

/// Difference between two [`Instant`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Duration(StdDuration);

impl Duration {
    /// Zero duration.
    pub const ZERO: Self = Self(StdDuration::ZERO);

    /// Builds a duration from whole milliseconds.
    #[must_use]
    pub const fn from_millis(ms: u64) -> Self {
        Self(StdDuration::from_millis(ms))
    }

    /// Builds a duration from whole microseconds.
    #[must_use]
    pub const fn from_micros(us: u64) -> Self {
        Self(StdDuration::from_micros(us))
    }

    /// Builds a duration from whole seconds.
    #[must_use]
    pub const fn from_secs(secs: u64) -> Self {
        Self(StdDuration::from_secs(secs))
    }

    /// Returns the duration as whole milliseconds, saturating at
    /// `u64::MAX`.
    #[must_use]
    pub const fn as_millis(self) -> u128 {
        self.0.as_millis()
    }

    /// Returns the duration as whole microseconds.
    #[must_use]
    pub const fn as_micros(self) -> u128 {
        self.0.as_micros()
    }

    /// Returns the seconds portion.
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

/// Wall-clock point-in-time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SystemTime {
    unix_seconds: i64,
    nanos: u32,
}

impl SystemTime {
    /// Current wall-clock time.
    #[must_use]
    pub fn now() -> Self {
        Self::from_std(StdSystemTime::now())
    }

    /// Signed milliseconds from the Unix epoch.
    ///
    /// Values before 1970 are negative. Values outside the `i64`
    /// millisecond range are clamped at the corresponding bound.
    #[must_use]
    pub fn unix_millis(self) -> i64 {
        let millis = i128::from(self.unix_seconds) * 1_000 + i128::from(self.nanos / 1_000_000);
        i64::try_from(millis).unwrap_or(if millis < 0 { i64::MIN } else { i64::MAX })
    }

    /// Wraps a `std::time::SystemTime` into the Gossamer-native
    /// `SystemTime` type. Useful when bridging from filesystem
    /// metadata (`fs::Metadata::modified()` etc.) into formatters
    /// like [`format_rfc1123_gmt`].
    #[must_use]
    pub fn from_std(t: StdSystemTime) -> Self {
        match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => {
                let unix_seconds = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
                let nanos = if unix_seconds == i64::MAX {
                    999_999_999
                } else {
                    duration.subsec_nanos()
                };
                Self {
                    unix_seconds,
                    nanos,
                }
            }
            Err(error) => {
                let duration = error.duration();
                let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
                if duration.subsec_nanos() == 0 {
                    Self {
                        unix_seconds: secs.checked_neg().unwrap_or(i64::MIN),
                        nanos: 0,
                    }
                } else {
                    Self {
                        unix_seconds: secs
                            .checked_add(1)
                            .and_then(i64::checked_neg)
                            .unwrap_or(i64::MIN),
                        nanos: 1_000_000_000 - duration.subsec_nanos(),
                    }
                }
            }
        }
    }

    /// Returns the underlying `std::time::SystemTime`.
    #[must_use]
    pub fn as_std(self) -> StdSystemTime {
        if self.unix_seconds >= 0 {
            let duration = std::time::Duration::new(self.unix_seconds as u64, self.nanos);
            return std::time::UNIX_EPOCH
                .checked_add(duration)
                .unwrap_or(std::time::UNIX_EPOCH);
        }
        let duration = if self.nanos == 0 {
            std::time::Duration::from_secs(self.unix_seconds.unsigned_abs())
        } else {
            std::time::Duration::new(
                self.unix_seconds
                    .checked_add(1)
                    .map_or(i64::MAX as u64, i64::unsigned_abs),
                1_000_000_000 - self.nanos,
            )
        };
        std::time::UNIX_EPOCH
            .checked_sub(duration)
            .unwrap_or(std::time::UNIX_EPOCH)
    }

    /// Returns seconds since the Unix epoch (negative for
    /// pre-1970 instants).
    #[must_use]
    pub fn unix_seconds(self) -> i64 {
        self.unix_seconds
    }

    /// Constructs a `SystemTime` from a millisecond offset relative
    /// to the Unix epoch. Negative offsets refer to pre-1970 times.
    /// Mirrors Go's `time.UnixMilli`.
    #[must_use]
    pub fn from_unix_millis(ms: i64) -> Self {
        let unix_seconds = ms.div_euclid(1_000);
        let nanos = (ms.rem_euclid(1_000) as u32) * 1_000_000;
        Self {
            unix_seconds,
            nanos,
        }
    }

    fn from_unix_parts(unix_seconds: i64, nanos: u32) -> Self {
        let extra_seconds = i64::from(nanos / 1_000_000_000);
        Self {
            unix_seconds: unix_seconds.saturating_add(extra_seconds),
            nanos: nanos % 1_000_000_000,
        }
    }
}

/// Suspends the current goroutine (or OS thread, when called from
/// outside a goroutine context) for `duration`. Internally registers
/// a one-shot timer with the netpoller so a sleeping goroutine does
/// not consume an OS thread while it waits.
pub fn sleep(duration: Duration) {
    if duration.0.is_zero() {
        return;
    }
    let deadline = gossamer_runtime::platform::Instant::now() + duration.0;
    crate::sched_global::sleep_until(deadline);
}

/// Cancellation-aware variant of [`sleep`].
///
/// Behaves identically to `sleep(duration)` when `ctx` is not
/// cancelled. If `ctx` is cancelled while the goroutine is parked -
/// either via `Cancel::cancel_with` or via a `with_deadline`
/// elapsing - the sleep returns early. The return value is
/// `Ok(())` for a natural completion and `Err(context error)`
/// for a cancellation-driven wake-up.
///
/// Race-free against an already-cancelled `ctx`: the function
/// checks before parking and again after parking; the wait-list
/// registration is the synchronisation point that lets
/// `Cancel::cancel_with` reach the goroutine while it sleeps.
pub fn sleep_ctx(
    ctx: &crate::context::Context,
    duration: Duration,
) -> Result<(), crate::errors::Error> {
    if let Some(err) = ctx.err() {
        return Err(err);
    }
    if duration.0.is_zero() {
        return Ok(());
    }
    let gid = crate::sched_global::current_gid().expect(
        "time::sleep_ctx must be called from a goroutine; use time::sleep outside a goroutine",
    );
    ctx.register_waiter(gid);
    // Re-check after registration; cancel may have fired between
    // the entry check and the registration.
    if ctx.is_cancelled() {
        ctx.deregister_waiter(gid);
        return Err(ctx
            .err()
            .unwrap_or_else(|| crate::errors::Error::new("context cancelled")));
    }
    let deadline = gossamer_runtime::platform::Instant::now() + duration.0;
    crate::sched_global::sleep_until(deadline);
    ctx.deregister_waiter(gid);
    if ctx.is_cancelled() {
        return Err(ctx
            .err()
            .unwrap_or_else(|| crate::errors::Error::new("context cancelled")));
    }
    Ok(())
}

/// Convenience wrapper around [`Instant::now`].
#[must_use]
pub fn now() -> Instant {
    Instant::now()
}

/// Errors raised by [`format_rfc3339`] / [`parse_rfc3339`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// Input string did not match the expected layout.
    #[error("time::parse: {0}")]
    BadInput(String),
    /// Time fell outside the representable Gregorian range.
    #[error("time::format: {0}")]
    OutOfRange(String),
}

/// Renders a wall-clock instant in RFC 1123 (HTTP date) form
/// (`Sun, 06 Nov 1994 08:49:37 GMT`). Always GMT - this is the
/// canonical encoding for HTTP `Date`, `Last-Modified`, and
/// `If-Modified-Since` headers.
pub fn format_rfc1123_gmt(when: SystemTime) -> Result<String, FormatError> {
    let secs = i128::from(when.unix_seconds);
    if secs > i128::from(i64::MAX) || secs < i128::from(i64::MIN) {
        return Err(FormatError::OutOfRange(format!(
            "{secs} seconds out of range"
        )));
    }
    let civil = unix_to_civil(secs)?;
    // Day-of-week from days-since-1970-01-01 (a Thursday).
    let days = (secs as i64).div_euclid(86_400);
    let dow_idx = ((days + 4).rem_euclid(7)) as usize;
    let dow = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][dow_idx];
    let month_names = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mo_idx = (civil.month as usize)
        .checked_sub(1)
        .ok_or_else(|| FormatError::OutOfRange(format!("month {} out of range", civil.month)))?;
    let mo = month_names
        .get(mo_idx)
        .ok_or_else(|| FormatError::OutOfRange(format!("month {} out of range", civil.month)))?;
    Ok(format!(
        "{dow}, {day:02} {mo} {year:04} {hour:02}:{min:02}:{sec:02} GMT",
        day = civil.day,
        year = civil.year,
        hour = civil.hour,
        min = civil.minute,
        sec = civil.second,
    ))
}

/// Renders a wall-clock instant in RFC 3339 form
/// (`2006-01-02T15:04:05Z`). Always emits UTC; offset-aware
/// formatting waits on a real timezone surface.
pub fn format_rfc3339(when: SystemTime) -> Result<String, FormatError> {
    let secs = i128::from(when.unix_seconds);
    let civil = unix_to_civil(secs)?;
    Ok(format!(
        "{year:04}-{mo:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z",
        year = civil.year,
        mo = civil.month,
        day = civil.day,
        hour = civil.hour,
        min = civil.minute,
        sec = civil.second,
    ))
}

/// Parses an RFC 3339 timestamp. Accepts `T` or space as the
/// date/time separator; accepts `Z`, `+HH:MM`, `-HH:MM`, or no
/// suffix (assumes UTC). Sub-second fractions are accepted but
/// silently dropped - full precision waits on a real time type.
pub fn parse_rfc3339(s: &str) -> Result<SystemTime, FormatError> {
    let bytes = s.as_bytes();
    let bad = || FormatError::BadInput(s.to_string());
    if bytes.len() < 19 {
        return Err(bad());
    }
    let year: i32 = parse_signed(&bytes[0..4]).ok_or_else(bad)?;
    if bytes[4] != b'-' {
        return Err(bad());
    }
    let month: u32 = parse_unsigned(&bytes[5..7]).ok_or_else(bad)?;
    if bytes[7] != b'-' {
        return Err(bad());
    }
    let day: u32 = parse_unsigned(&bytes[8..10]).ok_or_else(bad)?;
    if !matches!(bytes[10], b'T' | b' ') {
        return Err(bad());
    }
    let hour: u32 = parse_unsigned(&bytes[11..13]).ok_or_else(bad)?;
    if bytes[13] != b':' {
        return Err(bad());
    }
    let minute: u32 = parse_unsigned(&bytes[14..16]).ok_or_else(bad)?;
    if bytes[16] != b':' {
        return Err(bad());
    }
    let second: u32 = parse_unsigned(&bytes[17..19]).ok_or_else(bad)?;
    let mut cursor = 19;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
    }
    let mut offset_seconds: i64 = 0;
    if cursor < bytes.len() {
        match bytes[cursor] {
            b'Z' => cursor += 1,
            b'+' | b'-' => {
                if cursor + 5 >= bytes.len() {
                    return Err(bad());
                }
                let sign: i64 = if bytes[cursor] == b'+' { 1 } else { -1 };
                let oh: u32 = parse_unsigned(&bytes[cursor + 1..cursor + 3]).ok_or_else(bad)?;
                if bytes[cursor + 3] != b':' {
                    return Err(bad());
                }
                let om: u32 = parse_unsigned(&bytes[cursor + 4..cursor + 6]).ok_or_else(bad)?;
                offset_seconds = sign * i64::from(oh * 3600 + om * 60);
                cursor += 6;
            }
            _ => return Err(bad()),
        }
    }
    if cursor != bytes.len() {
        return Err(bad());
    }
    if !valid_civil(year, month, day, hour, minute, second) {
        return Err(bad());
    }
    let unix = civil_to_unix(&CivilTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    }) - offset_seconds;
    Ok(SystemTime::from_unix_parts(unix, 0))
}

/// Parses an HTTP-date (RFC 7231 §7.1.1.1). The preferred RFC 1123
/// form (`Sun, 06 Nov 1994 08:49:37 GMT`) is what browsers send in
/// `If-Modified-Since` / `If-Unmodified-Since`, echoing the server's
/// `Last-Modified`. The obsolete RFC 850 (`Sunday, 06-Nov-94
/// 08:49:37 GMT`) and asctime (`Sun Nov  6 08:49:37 1994`) forms are
/// also accepted, as RFC 7231 requires of recipients. Always GMT/UTC.
pub fn parse_rfc1123_gmt(s: &str) -> Result<SystemTime, FormatError> {
    let bad = || FormatError::BadInput(s.to_string());
    let trimmed = s.trim();
    let (day, month, year, hms) = if let Some(comma) = trimmed.find(',') {
        let toks: Vec<&str> = trimmed[comma + 1..].split_whitespace().collect();
        if toks.len() < 3 {
            return Err(bad());
        }
        if toks[0].contains('-') {
            // RFC 850: `06-Nov-94 08:49:37 GMT`.
            let p: Vec<&str> = toks[0].split('-').collect();
            if p.len() != 3 {
                return Err(bad());
            }
            let day: u32 = p[0].parse().map_err(|_| bad())?;
            let month = month_index(p[1]).ok_or_else(bad)?;
            let yy: i32 = p[2].parse().map_err(|_| bad())?;
            // Two-digit year window (RFC 6265 §5.1.1): 00..=68 -> 2000s.
            let year = if yy < 70 { 2000 + yy } else { 1900 + yy };
            (day, month, year, toks[1])
        } else {
            // RFC 1123: `06 Nov 1994 08:49:37 GMT`.
            if toks.len() < 4 {
                return Err(bad());
            }
            let day: u32 = toks[0].parse().map_err(|_| bad())?;
            let month = month_index(toks[1]).ok_or_else(bad)?;
            let year: i32 = toks[2].parse().map_err(|_| bad())?;
            (day, month, year, toks[3])
        }
    } else {
        // asctime: `Sun Nov  6 08:49:37 1994` (no comma).
        let toks: Vec<&str> = trimmed.split_whitespace().collect();
        if toks.len() < 5 {
            return Err(bad());
        }
        let month = month_index(toks[1]).ok_or_else(bad)?;
        let day: u32 = toks[2].parse().map_err(|_| bad())?;
        let year: i32 = toks[4].parse().map_err(|_| bad())?;
        (day, month, year, toks[3])
    };
    let t: Vec<&str> = hms.split(':').collect();
    if t.len() != 3 {
        return Err(bad());
    }
    let hour: u32 = t[0].parse().map_err(|_| bad())?;
    let minute: u32 = t[1].parse().map_err(|_| bad())?;
    let second: u32 = t[2].parse().map_err(|_| bad())?;
    if !valid_civil(year, month, day, hour, minute, second) {
        return Err(bad());
    }
    let unix = civil_to_unix(&CivilTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    });
    Ok(SystemTime::from_unix_parts(unix, 0))
}

/// Month abbreviation (`Jan`..`Dec`, case-insensitive) to 1-based index.
fn month_index(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(name))
        .map(|i| i as u32 + 1)
}

fn parse_unsigned(bytes: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(bytes).ok()?;
    s.parse::<u32>().ok()
}

fn parse_signed(bytes: &[u8]) -> Option<i32> {
    let s = std::str::from_utf8(bytes).ok()?;
    s.parse::<i32>().ok()
}

fn valid_civil(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> bool {
    if !(1..=12).contains(&mo) {
        return false;
    }
    if !(1..=days_in_month(y, mo)).contains(&d) {
        return false;
    }
    h < 24 && mi < 60 && s < 60
}

const fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

const fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

// Howard Hinnant's days_from_civil algorithm.
fn civil_to_days(y: i32, m: u32, d: u32) -> i64 {
    let m_i = m as i32;
    let y_adj = y - i32::from(m_i <= 2);
    let era = if y_adj >= 0 {
        y_adj / 400
    } else {
        (y_adj - 399) / 400
    };
    let yoe = (y_adj - era * 400) as u32;
    let m_eff = if m_i > 2 { m_i - 3 } else { m_i + 9 };
    let doy = (153 * m_eff as u32 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
}

struct CivilDate {
    year: i32,
    month: u32,
    day: u32,
}

struct CivilTime {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

fn civil_to_unix(c: &CivilTime) -> i64 {
    civil_to_days(c.year, c.month, c.day) * 86_400
        + i64::from(c.hour) * 3600
        + i64::from(c.minute) * 60
        + i64::from(c.second)
}

fn unix_to_civil(secs: i128) -> Result<CivilTime, FormatError> {
    if secs > i128::from(i64::MAX) || secs < i128::from(i64::MIN) {
        return Err(FormatError::OutOfRange(format!(
            "{secs} seconds out of range"
        )));
    }
    let secs = secs as i64;
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let date = days_to_civil(days);
    Ok(CivilTime {
        year: date.year,
        month: date.month,
        day: date.day,
        hour: (time_of_day / 3600) as u32,
        minute: ((time_of_day % 3600) / 60) as u32,
        second: (time_of_day % 60) as u32,
    })
}

fn days_to_civil(days: i64) -> CivilDate {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    CivilDate {
        year: year + i32::from(month <= 2),
        month,
        day,
    }
}

// --- AfterFunc (Go's time.AfterFunc) -------------------------------

/// The armed state of an [`after_func`] timer.
const TIMER_ARMED: u8 = 0;
/// The timer reached its deadline and ran its function.
const TIMER_FIRED: u8 = 1;
/// [`TimerHandle::cancel`] disarmed the timer first.
const TIMER_CANCELLED: u8 = 2;

/// One-shot timer (Go's `time.AfterFunc`): runs `f` once `delay` passes, on
/// the runtime's poller thread, which every timer and I/O wait shares, so `f`
/// must be short. The returned [`TimerHandle`] can cancel the timer before it
/// fires; dropping the handle leaves the timer armed.
pub fn after_func(delay: Duration, f: impl FnOnce() + Send + 'static) -> TimerHandle {
    let state = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(TIMER_ARMED));
    let fire_state = std::sync::Arc::clone(&state);
    let f = parking_lot::Mutex::new(Some(f));
    let deadline = gossamer_runtime::platform::Instant::now() + delay.0;
    let timer = crate::sched_global::add_timer(
        deadline,
        Box::new(move || {
            let won = fire_state
                .compare_exchange(
                    TIMER_ARMED,
                    TIMER_FIRED,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok();
            if won && let Some(f) = f.lock().take() {
                f();
            }
        }),
    );
    TimerHandle { state, timer }
}

/// Handle returned by [`after_func`].
pub struct TimerHandle {
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    timer: crate::sched_global::TimerHandle,
}

impl TimerHandle {
    /// Cancels the timer, answering whether it was still armed: `false`
    /// once it has fired or was cancelled before.
    pub fn cancel(&mut self) -> bool {
        let disarmed = self
            .state
            .compare_exchange(
                TIMER_ARMED,
                TIMER_CANCELLED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok();
        if disarmed {
            crate::sched_global::cancel_timer(self.timer);
        }
        disarmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_epoch_renders_zero() {
        let formatted = format_rfc3339(SystemTime::from_std(std::time::UNIX_EPOCH)).unwrap();
        assert_eq!(formatted, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn signed_unix_millis_preserve_pre_epoch_values() {
        for millis in [-1, -999, -1_001, -123_456_789] {
            assert_eq!(SystemTime::from_unix_millis(millis).unix_millis(), millis);
        }
        assert_eq!(
            SystemTime::from_unix_millis(i64::MIN).unix_millis(),
            i64::MIN
        );
    }

    #[test]
    fn round_trip_known_timestamp() {
        let t = parse_rfc3339("2026-04-25T16:30:45Z").unwrap();
        let formatted = format_rfc3339(t).unwrap();
        assert_eq!(formatted, "2026-04-25T16:30:45Z");
    }

    #[test]
    fn rfc1123_epoch() {
        let formatted = format_rfc1123_gmt(SystemTime::from_std(std::time::UNIX_EPOCH)).unwrap();
        // 1970-01-01 is a Thursday.
        assert_eq!(formatted, "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    #[test]
    fn rfc1123_known_timestamp() {
        // 1994-11-06 was a Sunday (canonical example from RFC 7231).
        let t = parse_rfc3339("1994-11-06T08:49:37Z").unwrap();
        let formatted = format_rfc1123_gmt(t).unwrap();
        assert_eq!(formatted, "Sun, 06 Nov 1994 08:49:37 GMT");
    }

    #[test]
    fn rfc1123_weekday_rotation_across_seven_days() {
        // 1970-01-01 = Thu, so 01..=07 covers all 7 weekday names.
        let expected = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        for (i, exp_dow) in expected.iter().enumerate() {
            let day = i + 1;
            let iso = format!("1970-01-0{day}T00:00:00Z");
            let t = parse_rfc3339(&iso).unwrap();
            let formatted = format_rfc1123_gmt(t).unwrap();
            let actual_dow = &formatted[..3];
            assert_eq!(
                actual_dow, *exp_dow,
                "1970-01-0{day} should be {exp_dow}, got {actual_dow} ({formatted})"
            );
        }
    }

    #[test]
    fn parse_accepts_offset_then_normalises_to_utc() {
        let t = parse_rfc3339("2026-04-25T18:30:00+02:00").unwrap();
        let formatted = format_rfc3339(t).unwrap();
        assert_eq!(formatted, "2026-04-25T16:30:00Z");
    }

    #[test]
    fn parse_accepts_space_separator_and_fractional_seconds() {
        let t = parse_rfc3339("2026-04-25 16:30:45.123456Z").unwrap();
        assert_eq!(format_rfc3339(t).unwrap(), "2026-04-25T16:30:45Z");
    }

    #[test]
    fn parse_rejects_invalid_dates() {
        assert!(parse_rfc3339("2026-13-01T00:00:00Z").is_err());
        assert!(parse_rfc3339("2026-02-30T00:00:00Z").is_err());
        assert!(parse_rfc3339("totally bogus").is_err());
        assert!(parse_rfc3339("2026-04-25T25:00:00Z").is_err());
    }

    #[test]
    fn handles_leap_year_february_29() {
        let t = parse_rfc3339("2024-02-29T12:00:00Z").unwrap();
        assert_eq!(format_rfc3339(t).unwrap(), "2024-02-29T12:00:00Z");
        // 2025 is not a leap year.
        assert!(parse_rfc3339("2025-02-29T12:00:00Z").is_err());
    }

    #[test]
    fn duration_constructors_round_trip() {
        let d = Duration::from_secs(42);
        assert_eq!(d.as_secs(), 42);
        assert_eq!(d.as_millis(), 42_000);
    }

    #[test]
    fn after_func_fires_after_delay() {
        // Condition-based wait: the callback signals a channel and the
        // test blocks on recv with a generous bound. This fails only if
        // the timer genuinely never fires; a slow CI scheduler cannot
        // turn it flaky the way a fixed post-delay sleep + load could.
        let (tx, rx) = std::sync::mpsc::channel();
        let _handle = after_func(Duration::from_millis(20), move || {
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "timer should fire within the bound"
        );
    }

    #[test]
    fn after_func_cancel_prevents_firing() {
        // Long delay so the cancel deterministically beats the deadline
        // regardless of scheduler jitter; a cancel that wins the timer's
        // state means its function never runs.
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_for_cb = std::sync::Arc::clone(&fired);
        let mut handle = after_func(Duration::from_secs(3600), move || {
            fired_for_cb.store(true, std::sync::atomic::Ordering::Release);
        });
        let before_fire = handle.cancel();
        assert!(before_fire, "cancel should land before the timer fires");
        assert!(
            !fired.load(std::sync::atomic::Ordering::Acquire),
            "cancelled timer must not run its callback"
        );
        assert!(!handle.cancel(), "a second cancel finds the timer disarmed");
    }

    #[test]
    fn after_func_timers_share_the_poller_thread() {
        // Each timer used to own a thread; a burst of them is now entries in
        // the one poller's queue, every one of which fires.
        let threads_before = thread_count();
        let (tx, rx) = std::sync::mpsc::channel();
        let handles: Vec<TimerHandle> = (0..200)
            .map(|i| {
                let tx = tx.clone();
                after_func(Duration::from_millis(30), move || {
                    let _ = tx.send(i);
                })
            })
            .collect();
        if let (Some(before), Some(during)) = (threads_before, thread_count()) {
            assert!(
                during < before + 8,
                "{during} threads with 200 timers armed"
            );
        }
        let mut seen: Vec<i32> = (0..200)
            .map(|_| rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap())
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..200).collect::<Vec<_>>());
        drop(handles);
    }

    /// Threads in this process, where the platform says.
    fn thread_count() -> Option<usize> {
        std::fs::read_dir("/proc/self/task")
            .ok()
            .map(Iterator::count)
    }
}
