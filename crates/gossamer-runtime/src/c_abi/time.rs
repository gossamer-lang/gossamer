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

// ---------------------------------------------------------------
// Time (seconds since UNIX epoch as f64 - interpreter parity)
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_now() -> f64 {
    ffi_entry!(f64::NAN, {
        use std::time::UNIX_EPOCH;
        crate::platform::system_time_now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64())
    })
}

// Process-wide monotonic base, initialised on first use. Mirrors
// the interpreter's per-thread `MONOTONIC_BASE` in
// `gossamer-interp`; a single process-global base gives identical
// `monotonic_ms` / `monotonic_nanos` deltas across the compiled
// tiers without the thread-local indirection.
fn monotonic_base() -> crate::platform::Instant {
    static BASE: std::sync::OnceLock<crate::platform::Instant> = std::sync::OnceLock::new();
    *BASE.get_or_init(crate::platform::Instant::now)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_monotonic_ms() -> i64 {
    ffi_entry!(-1, {
        i64::try_from(monotonic_base().elapsed().as_millis()).unwrap_or(i64::MAX)
    })
}

/// `time::now_nanos() -> i64` - nanoseconds since the UNIX epoch.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_now_nanos() -> i64 {
    ffi_entry!(-1, { crate::clock::wall_nanos() })
}

/// `time::since_ms(start) -> i64` - monotonic milliseconds elapsed
/// since the `start` value previously returned by `monotonic_ms`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_since_ms(start: i64) -> i64 {
    ffi_entry!(-1, {
        let now = i64::try_from(monotonic_base().elapsed().as_millis()).unwrap_or(i64::MAX);
        now.saturating_sub(start)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_monotonic_nanos() -> i64 {
    ffi_entry!(-1, {
        i64::try_from(monotonic_base().elapsed().as_nanos()).unwrap_or(i64::MAX)
    })
}

// `time::Duration` is a count of nanoseconds and `time::Instant` a reading of
// the monotonic clock in nanoseconds, both carried as an `i64`. Every
// conversion saturates rather than wrapping.

const NANOS_PER_MICRO: i64 = 1_000;
const NANOS_PER_MILLI: i64 = 1_000_000;
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// `time::Duration::from_nanos(n)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_duration_from_nanos(ns: i64) -> i64 {
    ns
}

/// `time::Duration::from_micros(n)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_duration_from_micros(us: i64) -> i64 {
    us.saturating_mul(NANOS_PER_MICRO)
}

/// `time::Duration::from_millis(n)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_duration_from_millis(ms: i64) -> i64 {
    ms.saturating_mul(NANOS_PER_MILLI)
}

/// `time::Duration::from_secs(n)`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_duration_from_secs(secs: i64) -> i64 {
    secs.saturating_mul(NANOS_PER_SEC)
}

/// `time::Duration::from_secs_f64(s)`: the nearest whole nanosecond count,
/// saturating at the `i64` range; NaN is zero.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_duration_from_secs_f64(secs: f64) -> i64 {
    // A float-to-int `as` saturates and maps NaN to zero.
    (secs * NANOS_PER_SEC as f64).round() as i64
}

/// `d.as_nanos()`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_duration_as_nanos(ns: i64) -> i64 {
    ns
}

/// `d.as_micros()`, truncated toward zero.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_duration_as_micros(ns: i64) -> i64 {
    ns / NANOS_PER_MICRO
}

/// `d.as_millis()`, truncated toward zero.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_duration_as_millis(ns: i64) -> i64 {
    ns / NANOS_PER_MILLI
}

/// `d.as_secs()`, truncated toward zero.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_duration_as_secs(ns: i64) -> i64 {
    ns / NANOS_PER_SEC
}

/// `d.as_secs_f64()`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_duration_as_secs_f64(ns: i64) -> f64 {
    ns as f64 / NANOS_PER_SEC as f64
}

/// `time::Instant::now()`: the monotonic clock in nanoseconds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_instant_now() -> i64 {
    unsafe { gos_rt_monotonic_nanos() }
}

/// `inst.elapsed()`: nanoseconds since `start`, never negative.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_instant_elapsed(start: i64) -> i64 {
    gos_rt_instant_duration_since(unsafe { gos_rt_monotonic_nanos() }, start)
}

/// `inst.elapsed_ms()`: whole milliseconds since `start`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_instant_elapsed_ms(start: i64) -> i64 {
    (unsafe { gos_rt_instant_elapsed(start) }) / NANOS_PER_MILLI
}

/// `later.duration_since(earlier)`: the nanoseconds between two readings,
/// zero when `earlier` is the later one.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_instant_duration_since(later: i64, earlier: i64) -> i64 {
    later.saturating_sub(earlier).max(0)
}

// Civil-time bridge used by the source-level `time` wrappers. Locations are
// encoded as stable strings so values remain immutable and require no native
// handle lifetime management.

use chrono::{
    DateTime, Datelike, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, Offset, TimeZone,
    Timelike, Utc,
};
use chrono_tz::Tz;
use std::os::raw::c_char;

use crate::c_abi::gos_rt_result_new;

enum CivilLocation {
    Iana(Tz),
    Fixed(FixedOffset),
}

fn read_time_string(ptr: *const c_char) -> Result<String, String> {
    if ptr.is_null() {
        return Err("time: null string".to_string());
    }
    Ok(unsafe { crate::c_abi::gos_str_arg_string(ptr) })
}

fn parse_location(spec: &str) -> Result<CivilLocation, String> {
    if spec == "UTC" {
        return Ok(CivilLocation::Iana(chrono_tz::UTC));
    }
    if let Ok(zone) = spec.parse::<Tz>() {
        return Ok(CivilLocation::Iana(zone));
    }
    let Some(offset) = spec.strip_prefix("UTC") else {
        return Err(format!("time: unknown location {spec:?}"));
    };
    let sign = match offset.as_bytes().first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return Err(format!("time: invalid fixed location {spec:?}")),
    };
    let mut parts = offset[1..].split(':');
    let hours = parts.next().and_then(|part| part.parse::<i32>().ok());
    let minutes = parts.next().and_then(|part| part.parse::<i32>().ok());
    if parts.next().is_some() {
        return Err(format!("time: invalid fixed location {spec:?}"));
    }
    let seconds = match (hours, minutes) {
        (Some(hours), Some(minutes)) if hours <= 23 && minutes <= 59 => {
            sign * (hours * 3_600 + minutes * 60)
        }
        _ => return Err(format!("time: invalid fixed location {spec:?}")),
    };
    FixedOffset::east_opt(seconds)
        .map(CivilLocation::Fixed)
        .ok_or_else(|| format!("time: invalid fixed location {spec:?}"))
}

fn time_error(message: &str) -> i128 {
    let error = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
    gos_rt_result_new(1, error as i64)
}

fn time_ok_string(value: &str) -> i128 {
    gos_rt_result_new(0, super::string::alloc_cstring(value.as_bytes()) as i64)
}

fn alloc_i64_words(values: &[i64]) -> i64 {
    crate::c_abi::rc::counted_words(values, &crate::c_abi::rc::LEAF_BLOB_META) as i64
}

fn naive_civil(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    nanos: i64,
) -> Result<NaiveDateTime, String> {
    let year = i32::try_from(year).map_err(|_| "time: year out of range".to_string())?;
    let month = u32::try_from(month).map_err(|_| "time: month out of range".to_string())?;
    let day = u32::try_from(day).map_err(|_| "time: day out of range".to_string())?;
    let hour = u32::try_from(hour).map_err(|_| "time: hour out of range".to_string())?;
    let minute = u32::try_from(minute).map_err(|_| "time: minute out of range".to_string())?;
    let second = u32::try_from(second).map_err(|_| "time: second out of range".to_string())?;
    let nanos = u32::try_from(nanos).map_err(|_| "time: nanoseconds out of range".to_string())?;
    NaiveDate::from_ymd_opt(year, month, day)
        .and_then(|date| date.and_hms_nano_opt(hour, minute, second, nanos))
        .ok_or_else(|| "time: invalid civil time".to_string())
}

fn resolve_civil(location: &CivilLocation, civil: NaiveDateTime) -> LocalResult<DateTime<Utc>> {
    match location {
        CivilLocation::Iana(zone) => zone
            .from_local_datetime(&civil)
            .map(|dt| dt.with_timezone(&Utc)),
        CivilLocation::Fixed(offset) => offset
            .from_local_datetime(&civil)
            .map(|dt| dt.with_timezone(&Utc)),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_location_raw(name: *const c_char) -> i128 {
    ffi_entry!(time_error("time: runtime panic"), {
        match read_time_string(name).and_then(|name| {
            parse_location(&name)?;
            Ok(name)
        }) {
            Ok(name) => time_ok_string(&name),
            Err(error) => time_error(&error),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_fixed_location_raw(offset_seconds: i64) -> i128 {
    ffi_entry!(time_error("time: runtime panic"), {
        let Ok(offset) = i32::try_from(offset_seconds) else {
            return time_error("time: fixed offset out of range");
        };
        if FixedOffset::east_opt(offset).is_none() {
            return time_error("time: fixed offset out of range");
        }
        let sign = if offset < 0 { '-' } else { '+' };
        let magnitude = offset.unsigned_abs();
        time_ok_string(&format!(
            "UTC{sign}{:02}:{:02}",
            magnitude / 3_600,
            (magnitude / 60) % 60
        ))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_civil_raw(ms: i64, location: *const c_char) -> i128 {
    ffi_entry!(time_error("time: runtime panic"), {
        let result = read_time_string(location)
            .and_then(|spec| parse_location(&spec))
            .and_then(|location| {
                let utc = DateTime::<Utc>::from_timestamp_millis(ms)
                    .ok_or_else(|| "time: timestamp out of range".to_string())?;
                let fields = match location {
                    CivilLocation::Iana(zone) => {
                        let value = utc.with_timezone(&zone);
                        [
                            value.year() as i64,
                            value.month() as i64,
                            value.day() as i64,
                            value.hour() as i64,
                            value.minute() as i64,
                            value.second() as i64,
                            value.nanosecond() as i64,
                            value.offset().fix().local_minus_utc() as i64,
                            value.weekday().num_days_from_monday() as i64,
                        ]
                    }
                    CivilLocation::Fixed(offset) => {
                        let value = utc.with_timezone(&offset);
                        [
                            value.year() as i64,
                            value.month() as i64,
                            value.day() as i64,
                            value.hour() as i64,
                            value.minute() as i64,
                            value.second() as i64,
                            value.nanosecond() as i64,
                            value.offset().local_minus_utc() as i64,
                            value.weekday().num_days_from_monday() as i64,
                        ]
                    }
                };
                Ok(alloc_i64_words(&fields))
            });
        match result {
            Ok(payload) if payload != 0 => gos_rt_result_new(0, payload),
            Ok(_) => time_error("time: allocation failed"),
            Err(error) => time_error(&error),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_resolve_raw(
    location: *const c_char,
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    nanos: i64,
) -> i128 {
    ffi_entry!(time_error("time: runtime panic"), {
        let result = read_time_string(location)
            .and_then(|spec| parse_location(&spec))
            .and_then(|location| {
                let civil = naive_civil(year, month, day, hour, minute, second, nanos)?;
                let values = match resolve_civil(&location, civil) {
                    LocalResult::Single(value) => [1, value.timestamp_millis(), 0],
                    LocalResult::None => [0, 0, 0],
                    LocalResult::Ambiguous(a, b) => {
                        let mut values = [a.timestamp_millis(), b.timestamp_millis()];
                        values.sort_unstable();
                        [2, values[0], values[1]]
                    }
                };
                Ok(alloc_i64_words(&values))
            });
        match result {
            Ok(payload) if payload != 0 => gos_rt_result_new(0, payload),
            Ok(_) => time_error("time: allocation failed"),
            Err(error) => time_error(&error),
        }
    })
}

fn chrono_layout(layout: &str) -> String {
    let mut output = layout.to_string();
    for (from, to) in [
        ("2006", "%Y"),
        ("January", "%B"),
        ("Jan", "%b"),
        ("01", "%m"),
        ("Monday", "%A"),
        ("Mon", "%a"),
        ("02", "%d"),
        ("15", "%H"),
        ("03", "%I"),
        ("04", "%M"),
        ("05", "%S"),
        ("PM", "%p"),
        ("MST", "%Z"),
        ("-07:00", "%:z"),
        ("-0700", "%z"),
    ] {
        output = output.replace(from, to);
    }
    output
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_format_in_raw(
    layout: *const c_char,
    ms: i64,
    location: *const c_char,
) -> i128 {
    ffi_entry!(time_error("time: runtime panic"), {
        let result = read_time_string(layout).and_then(|layout| {
            let spec = read_time_string(location)?;
            let location = parse_location(&spec)?;
            let utc = DateTime::<Utc>::from_timestamp_millis(ms)
                .ok_or_else(|| "time: timestamp out of range".to_string())?;
            let format = chrono_layout(&layout);
            Ok(match location {
                CivilLocation::Iana(zone) => utc.with_timezone(&zone).format(&format).to_string(),
                CivilLocation::Fixed(offset) => {
                    utc.with_timezone(&offset).format(&format).to_string()
                }
            })
        });
        match result {
            Ok(value) => time_ok_string(&value),
            Err(error) => time_error(&error),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_add_date_raw(
    ms: i64,
    location: *const c_char,
    years: i64,
    months: i64,
    days: i64,
) -> i128 {
    ffi_entry!(time_error("time: runtime panic"), {
        let result = read_time_string(location)
            .and_then(|spec| parse_location(&spec))
            .and_then(|location| {
                let utc = DateTime::<Utc>::from_timestamp_millis(ms)
                    .ok_or_else(|| "time: timestamp out of range".to_string())?;
                let local = match &location {
                    CivilLocation::Iana(zone) => utc.with_timezone(zone).naive_local(),
                    CivilLocation::Fixed(offset) => utc.with_timezone(offset).naive_local(),
                };
                let total_month = i64::from(local.year()) * 12
                    + i64::from(local.month0())
                    + years.saturating_mul(12)
                    + months;
                let year = i32::try_from(total_month.div_euclid(12))
                    .map_err(|_| "time: resulting year out of range".to_string())?;
                let month = u32::try_from(total_month.rem_euclid(12) + 1).unwrap_or(1);
                let next_month = if month == 12 {
                    NaiveDate::from_ymd_opt(year + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(year, month + 1, 1)
                }
                .ok_or_else(|| "time: resulting date out of range".to_string())?;
                let last_day = next_month.pred_opt().map_or(28, |date| date.day());
                let base = NaiveDate::from_ymd_opt(year, month, local.day().min(last_day))
                    .and_then(|date| {
                        date.and_hms_nano_opt(
                            local.hour(),
                            local.minute(),
                            local.second(),
                            local.nanosecond(),
                        )
                    })
                    .ok_or_else(|| "time: resulting date out of range".to_string())?;
                let shifted = base
                    .checked_add_signed(
                        chrono::Duration::try_days(days)
                            .ok_or_else(|| "time: day offset out of range".to_string())?,
                    )
                    .ok_or_else(|| "time: resulting date out of range".to_string())?;
                match resolve_civil(&location, shifted) {
                    LocalResult::Single(value) => Ok(value.timestamp_millis()),
                    LocalResult::None => {
                        Err("time: resulting civil time falls in a gap".to_string())
                    }
                    LocalResult::Ambiguous(_, _) => {
                        Err("time: resulting civil time is ambiguous".to_string())
                    }
                }
            });
        match result {
            Ok(value) => gos_rt_result_new(0, value),
            Err(error) => time_error(&error),
        }
    })
}
