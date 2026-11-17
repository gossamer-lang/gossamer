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

// ---------------------------------------------------------------
// Time (seconds since UNIX epoch as f64 - interpreter parity)
// ---------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_time_now() -> f64 {
    ffi_entry!({
        use std::time::UNIX_EPOCH;
        crate::platform::system_time_now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64())
    })
}

/// The instant `time::monotonic_ms` / `monotonic_nanos` count from, one per
/// process, so readings taken on any thread compare.
pub fn monotonic_base() -> crate::platform::Instant {
    static BASE: std::sync::OnceLock<crate::platform::Instant> = std::sync::OnceLock::new();
    *BASE.get_or_init(crate::platform::Instant::now)
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_monotonic_ms() -> i64 {
    ffi_entry!({ i64::try_from(monotonic_base().elapsed().as_millis()).unwrap_or(i64::MAX) })
}

/// `time::now_nanos() -> i64` - nanoseconds since the UNIX epoch.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_time_now_nanos() -> i64 {
    ffi_entry!({ crate::clock::wall_nanos() })
}

/// `time::since_ms(start) -> i64` - monotonic milliseconds elapsed
/// since the `start` value previously returned by `monotonic_ms`.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_time_since_ms(start: i64) -> i64 {
    ffi_entry!({
        let now = i64::try_from(monotonic_base().elapsed().as_millis()).unwrap_or(i64::MAX);
        now.saturating_sub(start)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_monotonic_nanos() -> i64 {
    ffi_entry!({ i64::try_from(monotonic_base().elapsed().as_nanos()).unwrap_or(i64::MAX) })
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
    gos_rt_monotonic_nanos()
}

/// `inst.elapsed()`: nanoseconds since `start`, never negative.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_instant_elapsed(start: i64) -> i64 {
    gos_rt_instant_duration_since(gos_rt_monotonic_nanos(), start)
}

/// `inst.elapsed_ms()`: whole milliseconds since `start`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_instant_elapsed_ms(start: i64) -> i64 {
    (gos_rt_instant_elapsed(start)) / NANOS_PER_MILLI
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

use std::os::raw::c_char;

use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::{AmbiguousOffset, Offset, TimeZone};

use crate::c_abi::{alloc_cstring, gos_rt_result_new, gos_str_arg_text};

/// # Safety
///
/// `ptr` is null or a live string body.
unsafe fn read_time_string(ptr: *const c_char) -> Result<String, String> {
    if ptr.is_null() {
        return Err("time: null string".to_string());
    }
    // SAFETY: this `unsafe fn`'s caller passes `ptr` live; non-null, checked above.
    Ok(unsafe { crate::c_abi::gos_str_arg_string(ptr) })
}

/// The zone `spec` names: `UTC`, `Local` (the zone the system runs in), an
/// IANA name spelled exactly as the database spells it, or a `UTC±hh:mm`
/// fixed offset.
fn parse_location(spec: &str) -> Result<TimeZone, String> {
    match spec {
        "UTC" => return Ok(TimeZone::UTC),
        "Local" => return Ok(TimeZone::try_system().unwrap_or(TimeZone::UTC)),
        _ => {}
    }
    if let Some(offset) = spec.strip_prefix("UTC") {
        return parse_fixed(spec, offset);
    }
    named_zone(spec).ok_or_else(|| format!("time: unknown location {spec:?}"))
}

/// Zones already read, by name. A zone's rules are read once per process, as
/// Go's `time.LoadLocation` reads them.
static ZONES: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<String, TimeZone>>> =
    std::sync::LazyLock::new(Default::default);

/// The IANA zone spelled exactly `name`, from the system's zoneinfo tree,
/// else from jiff's database (bundled on Windows and wasm).
fn named_zone(name: &str) -> Option<TimeZone> {
    if let Some(zone) = ZONES.lock().get(name) {
        return Some(zone.clone());
    }
    // jiff's database matches names in any case; a location keeps the one
    // spelling it is stored and printed under.
    let zone = read_zone_file(name)
        .or_else(|| {
            TimeZone::get(name)
                .ok()
                .filter(|zone| zone.iana_name() == Some(name))
        })
        .or_else(|| embedded_zone(name))?;
    ZONES.lock().insert(name.to_string(), zone.clone());
    Some(zone)
}

/// The `TZif` data of each zone the program embeds, by name, for a host with
/// no zoneinfo tree. Set by the program's first lookup.
static EMBEDDED_ZONES: std::sync::OnceLock<std::collections::HashMap<String, Vec<u8>>> =
    std::sync::OnceLock::new();

/// Records the zones the program embeds: `name\tbase64 TZif\n` lines, as the
/// compiler writes them. The first call's data stays; the program passes the
/// same data on every call.
pub fn register_zones(data: &str) {
    if EMBEDDED_ZONES.get().is_some() {
        return;
    }
    let zones = data
        .lines()
        .filter_map(|line| {
            let (name, encoded) = line.split_once('\t')?;
            let tzif = base64_simd::STANDARD.decode_to_vec(encoded).ok()?;
            Some((name.to_string(), tzif))
        })
        .collect();
    let _ = EMBEDDED_ZONES.set(zones);
}

/// The zone `name` from the data the program embeds.
fn embedded_zone(name: &str) -> Option<TimeZone> {
    let tzif = EMBEDDED_ZONES.get()?.get(name)?;
    TimeZone::tzif(name, tzif).ok()
}

/// `__gos_time_register_zones_raw(data)`: [`register_zones`], answering
/// `Ok(())`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_register_zones_raw(data: *const c_char) -> i128 {
    ffi_entry!({
        if !data.is_null() && EMBEDDED_ZONES.get().is_none() {
            // SAFETY: `data` is a String argument from compiled code, non-null (checked
            // above), live for the whole call.
            register_zones(unsafe { gos_str_arg_text(data) });
        }
        gos_rt_result_new(0, 0)
    })
}

/// The zoneinfo trees jiff searches after `TZDIR`.
#[cfg(unix)]
const ZONEINFO_DIRECTORIES: [&str; 3] = [
    "/usr/share/zoneinfo",
    "/usr/share/lib/zoneinfo",
    "/etc/zoneinfo",
];

/// The zone `name` read from its own file in the system's zoneinfo tree. One
/// file read replaces the walk of the whole tree jiff takes to index every
/// name, which costs a lookup hundreds of kilobytes.
#[cfg(unix)]
fn read_zone_file(name: &str) -> Option<TimeZone> {
    let valid_part = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'.'))
    };
    if !name.split('/').all(valid_part) {
        return None;
    }
    let tzdir = std::env::var_os("TZDIR").map(std::path::PathBuf::from);
    let mut dirs = tzdir
        .into_iter()
        .chain(ZONEINFO_DIRECTORIES.iter().map(std::path::PathBuf::from));
    let dir = dirs.find(|dir| dir.is_dir())?;
    // A case-insensitive file system opens `europe/paris` too, so each
    // component is matched against the directory's own entry names.
    let mut path = dir;
    for part in name.split('/') {
        let listed = std::fs::read_dir(&path)
            .ok()?
            .flatten()
            .any(|entry| entry.file_name() == part);
        if !listed {
            return None;
        }
        path.push(part);
    }
    TimeZone::tzif(name, &std::fs::read(&path).ok()?).ok()
}

/// Platforms without a zoneinfo tree read every zone from jiff's database.
#[cfg(not(unix))]
fn read_zone_file(_name: &str) -> Option<TimeZone> {
    None
}

/// The fixed zone of `offset`, the `±hh:mm` that follows `UTC` in `spec`.
fn parse_fixed(spec: &str, offset: &str) -> Result<TimeZone, String> {
    let invalid = || format!("time: invalid fixed location {spec:?}");
    let (sign, rest) = match offset.as_bytes().first() {
        Some(b'+') => (1, &offset[1..]),
        Some(b'-') => (-1, &offset[1..]),
        _ => return Err(invalid()),
    };
    let (hours, minutes) = rest.split_once(':').ok_or_else(invalid)?;
    let field = |text: &str, max: i32| {
        text.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| text.parse::<i32>().ok())
            .flatten()
            .filter(|value| (0..=max).contains(value))
    };
    let (Some(hours), Some(minutes)) = (field(hours, 23), field(minutes, 59)) else {
        return Err(invalid());
    };
    Offset::from_seconds(sign * (hours * 3_600 + minutes * 60))
        .map(TimeZone::fixed)
        .map_err(|_| invalid())
}

/// The name a location is stored under once `spec` is known to name one.
///
/// # Errors
/// When `spec` is neither `UTC`, `Local`, an IANA zone, nor a `UTC±hh:mm`
/// offset.
pub fn location_name(spec: &str) -> Result<String, String> {
    if spec == "Local" {
        return Ok(local_location_name());
    }
    parse_location(spec)?;
    Ok(spec.to_string())
}

/// The IANA name of the zone the system runs in, read as POSIX specifies:
/// `TZ` first (an IANA name, a zoneinfo path, or a rule string), then the
/// platform's own setting. `UTC` when the system names no zone or a `TZ` no
/// zone can be read from, as POSIX and Go treat it. A zone with no IANA
/// name, such as a `TZ` rule string, is `Local`, which every later call reads
/// from the system again.
#[must_use]
pub fn local_location_name() -> String {
    let Ok(zone) = TimeZone::try_system() else {
        // A host with no zoneinfo cannot read `TZ`'s zone, but the program
        // may carry it.
        return std::env::var("TZ")
            .ok()
            .map(|tz| tz.trim_start_matches(':').to_string())
            .filter(|name| named_zone(name).is_some())
            .unwrap_or_else(|| "UTC".to_string());
    };
    zone.iana_name()
        .filter(|name| named_zone(name).is_some())
        .map_or_else(|| "Local".to_string(), str::to_string)
}

/// The `UTC±hh:mm` name of a fixed offset of `offset_seconds` east of UTC.
///
/// # Errors
/// When the offset is a day or more.
pub fn fixed_location_name(offset_seconds: i64) -> Result<String, String> {
    let offset = i32::try_from(offset_seconds)
        .ok()
        .filter(|offset| offset.unsigned_abs() < 86_400)
        .ok_or_else(|| "time: fixed offset out of range".to_string())?;
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.unsigned_abs();
    Ok(format!(
        "UTC{sign}{:02}:{:02}",
        magnitude / 3_600,
        (magnitude / 60) % 60
    ))
}

/// The instant `ms` milliseconds from the Unix epoch.
fn instant_ms(ms: i64) -> Result<Timestamp, String> {
    Timestamp::from_millisecond(ms).map_err(|_| "time: timestamp out of range".to_string())
}

/// The civil fields of Unix milliseconds `ms` in `spec`: year, month, day,
/// hour, minute, second, nanosecond, offset seconds east of UTC, and weekday
/// counted from Monday.
///
/// # Errors
/// An unknown location, or a timestamp out of range.
pub fn civil_fields(ms: i64, spec: &str) -> Result<[i64; 9], String> {
    let zoned = instant_ms(ms)?.to_zoned_datetime(parse_location(spec)?);
    let local = zoned.datetime();
    Ok([
        i64::from(local.year()),
        i64::from(local.month()),
        i64::from(local.day()),
        i64::from(local.hour()),
        i64::from(local.minute()),
        i64::from(local.second()),
        i64::from(local.subsec_nanosecond()),
        i64::from(zoned.offset().seconds()),
        i64::from(local.weekday().to_monday_zero_offset()),
    ])
}

/// How the civil time `fields` (year, month, day, hour, minute, second,
/// nanosecond) reads in `spec`: `[1, ms, 0]` for one instant, `[0, 0, 0]` for
/// a gap, `[2, earlier, later]` for a fold.
///
/// # Errors
/// An unknown location, or fields naming no calendar time.
pub fn resolve_fields(spec: &str, fields: [i64; 7]) -> Result<[i64; 3], String> {
    let zone = parse_location(spec)?;
    let [year, month, day, hour, minute, second, nanos] = fields;
    let civil = civil_datetime(year, month, day, hour, minute, second, nanos)?;
    let at = |offset: Offset| {
        offset
            .to_timestamp(civil)
            .map(Timestamp::as_millisecond)
            .map_err(|_| "time: timestamp out of range".to_string())
    };
    Ok(match zone.to_ambiguous_timestamp(civil).offset() {
        AmbiguousOffset::Unambiguous { offset } => [1, at(offset)?, 0],
        AmbiguousOffset::Gap { .. } => [0, 0, 0],
        AmbiguousOffset::Fold { before, after } => {
            let mut values = [at(before)?, at(after)?];
            values.sort_unstable();
            [2, values[0], values[1]]
        }
    })
}

/// Unix milliseconds `ms` in `spec`, written by a Go-style `layout`.
///
/// # Errors
/// An unknown location, a timestamp out of range, or a layout the location
/// cannot fill.
pub fn format_in(layout: &str, ms: i64, spec: &str) -> Result<String, String> {
    let zoned = instant_ms(ms)?.to_zoned_datetime(parse_location(spec)?);
    jiff::fmt::strtime::format(strtime_layout(layout), &zoned)
        .map_err(|error| format!("time: {error}"))
}

/// Unix milliseconds `ms` moved by whole years, months, and days on the
/// calendar of `spec`, the day clamped to the target month's last.
///
/// # Errors
/// An unknown location, a result out of range, or one that lands in a gap or
/// a fold of the location's clock.
pub fn add_date(ms: i64, spec: &str, years: i64, months: i64, days: i64) -> Result<i64, String> {
    let zone = parse_location(spec)?;
    let local = instant_ms(ms)?.to_zoned_datetime(zone.clone()).datetime();
    let out_of_range = || "time: resulting date out of range".to_string();
    let total_month = (i64::from(local.year()) * 12 + i64::from(local.month()) - 1)
        .saturating_add(years.saturating_mul(12))
        .saturating_add(months);
    let year = i16::try_from(total_month.div_euclid(12))
        .map_err(|_| "time: resulting year out of range".to_string())?;
    // `rem_euclid(12) + 1` is a month number, 1 through 12.
    let month = i8::try_from(total_month.rem_euclid(12) + 1).map_err(|_| out_of_range())?;
    let last_day = Date::new(year, month, 1)
        .map_err(|_| out_of_range())?
        .last_of_month()
        .day();
    let base = DateTime::new(
        year,
        month,
        local.day().min(last_day),
        local.hour(),
        local.minute(),
        local.second(),
        local.subsec_nanosecond(),
    )
    .map_err(|_| out_of_range())?;
    let shift = jiff::Span::new()
        .try_days(days)
        .map_err(|_| "time: day offset out of range".to_string())?;
    let shifted = base.checked_add(shift).map_err(|_| out_of_range())?;
    match zone.to_ambiguous_timestamp(shifted).offset() {
        AmbiguousOffset::Unambiguous { offset } => offset
            .to_timestamp(shifted)
            .map(Timestamp::as_millisecond)
            .map_err(|_| out_of_range()),
        AmbiguousOffset::Gap { .. } => Err("time: resulting civil time falls in a gap".to_string()),
        AmbiguousOffset::Fold { .. } => Err("time: resulting civil time is ambiguous".to_string()),
    }
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

fn civil_datetime(
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    nanos: i64,
) -> Result<DateTime, String> {
    let year = i16::try_from(year).map_err(|_| "time: year out of range".to_string())?;
    let month = i8::try_from(month).map_err(|_| "time: month out of range".to_string())?;
    let day = i8::try_from(day).map_err(|_| "time: day out of range".to_string())?;
    let hour = i8::try_from(hour).map_err(|_| "time: hour out of range".to_string())?;
    let minute = i8::try_from(minute).map_err(|_| "time: minute out of range".to_string())?;
    let second = i8::try_from(second).map_err(|_| "time: second out of range".to_string())?;
    let nanos = i32::try_from(nanos).map_err(|_| "time: nanoseconds out of range".to_string())?;
    DateTime::new(year, month, day, hour, minute, second, nanos)
        .map_err(|_| "time: invalid civil time".to_string())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_location_raw(name: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `name` is this shim's argument, as `read_time_string` requires (C-ABI
        // contract).
        match unsafe { read_time_string(name) }.and_then(|name| location_name(&name)) {
            Ok(name) => time_ok_string(&name),
            Err(error) => time_error(&error),
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_time_fixed_location_raw(offset_seconds: i64) -> i128 {
    ffi_entry!({
        match fixed_location_name(offset_seconds) {
            Ok(name) => time_ok_string(&name),
            Err(error) => time_error(&error),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_civil_raw(ms: i64, location: *const c_char) -> i128 {
    ffi_entry!({
        // SAFETY: `location` is this shim's argument, as `read_time_string` requires (C-ABI
        // contract).
        let result = unsafe { read_time_string(location) }
            .and_then(|spec| civil_fields(ms, &spec))
            .map(|fields| alloc_i64_words(&fields));
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
    ffi_entry!({
        // SAFETY: `location` is this shim's argument, as `read_time_string` requires (C-ABI
        // contract).
        let result = unsafe { read_time_string(location) }
            .and_then(|spec| resolve_fields(&spec, [year, month, day, hour, minute, second, nanos]))
            .map(|fields| alloc_i64_words(&fields));
        match result {
            Ok(payload) if payload != 0 => gos_rt_result_new(0, payload),
            Ok(_) => time_error("time: allocation failed"),
            Err(error) => time_error(&error),
        }
    })
}

/// The `strftime` directives for a Go reference-time `layout`. A `%` in the
/// layout is literal text, as every character outside a Go token is.
fn strtime_layout(layout: &str) -> String {
    let mut output = layout.replace('%', "%%");
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
    ffi_entry!({
        // SAFETY: `layout` is this shim's argument, as `read_time_string` requires (C-ABI
        // contract).
        let result = unsafe { read_time_string(layout) }.and_then(|layout| {
            // SAFETY: `location` is this shim's argument, as `read_time_string` requires (C-ABI
            // contract).
            let spec = unsafe { read_time_string(location) }?;
            format_in(&layout, ms, &spec)
        });
        match result {
            Ok(value) => time_ok_string(&value),
            Err(error) => time_error(&error),
        }
    })
}

/// RFC 3339 text of the instant `unix_ns` read in the location `spec`: whole
/// seconds, the fraction with its trailing zeros dropped when there is one,
/// and `Z` for UTC itself or the numeric offset otherwise. This is
/// `time::Time::format_rfc3339`, shared by every tier.
///
/// # Errors
/// The location's refusal when `spec` names none, or a timestamp out of range.
pub fn format_rfc3339(unix_ns: i64, spec: &str) -> Result<String, String> {
    use std::fmt::Write;
    let zoned = instant_ms(unix_ns.div_euclid(1_000_000))?.to_zoned_datetime(parse_location(spec)?);
    let naive = zoned.datetime();
    let offset = zoned.offset().seconds();
    let mut out = String::with_capacity(35);
    let _ = write!(
        out,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        naive.year(),
        naive.month(),
        naive.day(),
        naive.hour(),
        naive.minute(),
        naive.second()
    );
    let nanos = unix_ns.rem_euclid(1_000_000_000);
    if nanos != 0 {
        let mut digits = [0u8; 9];
        let mut rest = nanos;
        for slot in digits.iter_mut().rev() {
            *slot = b'0' + (rest % 10) as u8;
            rest /= 10;
        }
        let kept = 9 - digits.iter().rev().take_while(|&&d| d == b'0').count();
        out.push('.');
        out.extend(digits[..kept].iter().map(|&d| char::from(d)));
    }
    if offset == 0 && spec == "UTC" {
        out.push('Z');
    } else {
        let sign = if offset < 0 { '-' } else { '+' };
        let a = offset.unsigned_abs();
        let _ = write!(out, "{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60);
    }
    Ok(out)
}

/// `__gos_time_format_rfc3339_raw(unix_ns, location)`: [`format_rfc3339`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_format_rfc3339_raw(
    unix_ns: i64,
    location: *const c_char,
) -> i128 {
    ffi_entry!({
        // SAFETY: `location` is this shim's argument, as `read_time_string` requires (C-ABI
        // contract).
        let result =
            unsafe { read_time_string(location) }.and_then(|spec| format_rfc3339(unix_ns, &spec));
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
    ffi_entry!({
        // SAFETY: `location` is this shim's argument, as `read_time_string` requires (C-ABI
        // contract).
        let result = unsafe { read_time_string(location) }
            .and_then(|spec| add_date(ms, &spec, years, months, days));
        match result {
            Ok(value) => gos_rt_result_new(0, value),
            Err(error) => time_error(&error),
        }
    })
}

/// `time::format_rfc3339(unix_ms) -> Result<String, errors::Error>`.
/// Renders a UTC RFC 3339 timestamp from a unix-milliseconds
/// instant. Mirrors the interpreter builtin.
#[unsafe(no_mangle)]
pub extern "C" fn gos_rt_time_format_rfc3339(unix_ms: i64) -> i128 {
    ffi_entry!({
        let secs = unix_ms.div_euclid(1_000);
        let nanos = (unix_ms.rem_euclid(1_000) * 1_000_000) as u32;
        let _ = nanos;
        let mut y: i64 = 1970;
        let mut remain = secs.div_euclid(86_400);
        let is_leap = |yr: i64| (yr % 4 == 0 && yr % 100 != 0) || yr % 400 == 0;
        let dy = |yr: i64| if is_leap(yr) { 366 } else { 365 };
        if remain < 0 {
            while remain < 0 {
                y -= 1;
                remain += dy(y);
            }
        } else {
            while remain >= dy(y) {
                remain -= dy(y);
                y += 1;
            }
        }
        let dim = |m: i64, yr: i64| -> i64 {
            match m {
                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
                4 | 6 | 9 | 11 => 30,
                2 => {
                    if is_leap(yr) {
                        29
                    } else {
                        28
                    }
                }
                _ => 30,
            }
        };
        let mut m = 1_i64;
        while remain >= dim(m, y) {
            remain -= dim(m, y);
            m += 1;
        }
        let day = remain + 1;
        let s = secs.rem_euclid(86_400);
        let h = s / 3600;
        let mi = (s % 3600) / 60;
        let se = s % 60;
        let s_str = format!("{y:04}-{m:02}-{day:02}T{h:02}:{mi:02}:{se:02}Z");
        let cs = alloc_cstring(s_str.as_bytes());
        gos_rt_result_new(0, cs as i64)
    })
}

/// `time::parse_rfc3339(s) -> Result<i64, errors::Error>`.
/// Parses an RFC 3339 timestamp and returns unix milliseconds.
/// Accepts `T` or space as the date/time separator; accepts `Z`,
/// `+HH:MM`, `-HH:MM`, or no suffix (assumes UTC); sub-second
/// fractions are accepted and dropped. A faithful port of
/// `gossamer_std::time::parse_rfc3339` so the compiled tier matches
/// the VM bit-for-bit (timezone offsets, day-of-month validation,
/// and pre-1970 negative results all included).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_time_parse_rfc3339(s: *const c_char) -> i128 {
    ffi_entry!({
        let err = || -> i128 {
            let cs = alloc_cstring(b"time::parse: bad input");
            gos_rt_result_new(1, cs as i64)
        };
        if s.is_null() {
            return err();
        }
        // SAFETY: `s` is a String argument from compiled code, null or a live string body for the whole call.
        let text = unsafe { gos_str_arg_text(s) };
        match parse_rfc3339_ms(text) {
            Some(ms) => gos_rt_result_new(0, ms),
            None => err(),
        }
    })
}

/// Parses one zero-padded unsigned field, mirroring `parse_unsigned`
/// in `gossamer_std::time` (rejects signs, spaces, and non-digits).
fn parse_rfc3339_uint(bytes: &[u8]) -> Option<i64> {
    std::str::from_utf8(bytes)
        .ok()?
        .parse::<u32>()
        .ok()
        .map(i64::from)
}

const fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

const fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant's `days_from_civil`, matching the i32/u32 version
/// in `gossamer_std::time` over the representable Gregorian range.
fn civil_to_days(y: i64, m: i64, d: i64) -> i64 {
    let y_adj = y - i64::from(m <= 2);
    let era = if y_adj >= 0 {
        y_adj / 400
    } else {
        (y_adj - 399) / 400
    };
    let yoe = y_adj - era * 400;
    let m_eff = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_eff + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Faithful port of `gossamer_std::time::parse_rfc3339` returning
/// unix milliseconds, or `None` for any malformed/out-of-range input.
fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4])
        .ok()?
        .parse::<i32>()
        .ok()? as i64;
    if bytes[4] != b'-' {
        return None;
    }
    let month = parse_rfc3339_uint(&bytes[5..7])?;
    if bytes[7] != b'-' {
        return None;
    }
    let day = parse_rfc3339_uint(&bytes[8..10])?;
    if !matches!(bytes[10], b'T' | b' ') {
        return None;
    }
    let hour = parse_rfc3339_uint(&bytes[11..13])?;
    if bytes[13] != b':' {
        return None;
    }
    let minute = parse_rfc3339_uint(&bytes[14..16])?;
    if bytes[16] != b':' {
        return None;
    }
    let second = parse_rfc3339_uint(&bytes[17..19])?;
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
                    return None;
                }
                let sign: i64 = if bytes[cursor] == b'+' { 1 } else { -1 };
                let oh = parse_rfc3339_uint(&bytes[cursor + 1..cursor + 3])?;
                if bytes[cursor + 3] != b':' {
                    return None;
                }
                let om = parse_rfc3339_uint(&bytes[cursor + 4..cursor + 6])?;
                offset_seconds = sign * (oh * 3600 + om * 60);
                cursor += 6;
            }
            _ => return None,
        }
    }
    if cursor != bytes.len() {
        return None;
    }
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour >= 24
        || minute >= 60
        || second >= 60
    {
        return None;
    }
    let unix_secs = civil_to_days(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
        - offset_seconds;
    unix_secs.checked_mul(1_000)
}

#[cfg(test)]
mod tests {
    use super::format_rfc3339;

    #[test]
    fn rfc3339_trims_the_fraction_and_names_utc_as_z() {
        assert_eq!(format_rfc3339(0, "UTC").unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_rfc3339(1_700_000_000_100_000_000, "UTC").unwrap(),
            "2023-11-14T22:13:20.1Z"
        );
        assert_eq!(
            format_rfc3339(-1, "UTC").unwrap(),
            "1969-12-31T23:59:59.999999999Z"
        );
    }

    #[test]
    fn rfc3339_writes_the_offset_of_any_other_location() {
        assert_eq!(
            format_rfc3339(0, "UTC-00:00").unwrap(),
            "1970-01-01T00:00:00+00:00"
        );
        assert_eq!(
            format_rfc3339(0, "UTC-09:30").unwrap(),
            "1969-12-31T14:30:00-09:30"
        );
        assert_eq!(
            format_rfc3339(0, "America/New_York").unwrap(),
            "1969-12-31T19:00:00-05:00"
        );
        assert!(format_rfc3339(0, "Bogus/Zone").is_err());
    }

    #[test]
    fn a_zone_the_program_embeds_resolves_when_no_database_has_it() {
        let ny = std::fs::read("/usr/share/zoneinfo/America/New_York")
            .ok()
            .or_else(|| jiff_tzdb::get("America/New_York").map(|(_, data)| data.to_vec()))
            .expect("New York's TZif data");
        assert!(super::location_name("Embedded/Only_Here").is_err());
        let encoded = base64_simd::STANDARD.encode_to_string(&ny);
        super::register_zones(&format!("Embedded/Only_Here\t{encoded}\n"));
        assert_eq!(
            super::location_name("Embedded/Only_Here").unwrap(),
            "Embedded/Only_Here"
        );
        assert_eq!(
            super::format_in("2006-01-02 15:04 MST", 0, "Embedded/Only_Here").unwrap(),
            "1969-12-31 19:00 EST"
        );
    }

    #[test]
    fn a_percent_sign_in_a_layout_is_literal_text() {
        assert_eq!(
            super::format_in("100% of 2006", 0, "UTC").unwrap(),
            "100% of 1970"
        );
    }

    #[test]
    fn zone_names_match_the_database_spelling_exactly() {
        assert_eq!(
            super::location_name("Europe/Paris").unwrap(),
            "Europe/Paris"
        );
        assert!(super::location_name("europe/paris").is_err());
        assert!(super::location_name("../zoneinfo/UTC").is_err());
    }

    #[test]
    fn fixed_offsets_take_two_unsigned_fields() {
        assert_eq!(super::location_name("UTC+05:45").unwrap(), "UTC+05:45");
        for bad in [
            "UTC+-5:00",
            "UTC+05",
            "UTC+5:60",
            "UTC+24:00",
            "UTC05:00",
            "UTC+05:00:00",
        ] {
            assert!(super::location_name(bad).is_err(), "{bad}");
        }
        assert!(super::fixed_location_name(86_400).is_err());
        assert_eq!(super::fixed_location_name(-34_200).unwrap(), "UTC-09:30");
    }
}
