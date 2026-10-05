/// `true` when `source` mentions the module path `marker` (`"sql::"`,
/// `"pem::"`), rather than merely containing those characters inside a
/// longer name.
///
/// The wrappers a marker pulls in declare top-level items, so a spurious
/// match injects names that collide with the program's own: without the
/// boundary check, a package called `psql` or `mysql` drags in the
/// `std::database::sql` wrappers and their `Null` / `Int` / `Text` variants.
fn mentions_path(source: &str, marker: &str) -> bool {
    let mut from = 0;
    while let Some(offset) = source[from..].find(marker) {
        let at = from + offset;
        let preceded_by_name = source[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if !preceded_by_name {
            return true;
        }
        from = at + marker.len();
    }
    false
}

/// `true` when `source` names `module::item`, in either spelling that binds
/// it: the qualified path, or a braced import (`use std::sync::{shield}`)
/// whose entry brings the bare name into scope.
fn mentions_module_item(source: &str, module: &str, item: &str) -> bool {
    mentions_path(source, &format!("{module}::{item}"))
        || (mentions_path(source, &format!("{module}::{{")) && source.contains(item))
}

/// Gossamer source the stdlib-wrapper pass would inject for `source`.
/// Exposed so a gate can pin the constants these wrappers spell out
/// against the Rust values they mirror.
#[must_use]
pub fn stdlib_wrapper_source(source: &str) -> String {
    synthesize_stdlib_wrappers(source)
}

/// What `source` reached through `use std::...`: the stdlib modules in scope,
/// and the wrapper names its item imports bind. `None` where the source does
/// not parse cleanly - a tree built by error recovery answers for no import,
/// and the parse diagnostics are that program's report.
struct StdImports {
    /// Binding name to the stdlib module it reaches.
    modules: std::collections::HashMap<String, String>,
    wrapper_names: std::collections::HashSet<String>,
}

fn imported_std(source: &str) -> Option<StdImports> {
    let mut probe = SourceMap::new();
    let file = probe.add_file("<stdlib-wrapper-probe>", source.to_string());
    let (parsed, diags) = crate::parse_source_file(source, file);
    if !diags.is_empty() {
        return None;
    }
    let modules = stdlib_modules_in_scope(&parsed);
    // The rewrite decides which wrapper names the program's paths become, so
    // the names it produces are the ones whose wrappers are needed: a bare
    // import, a module alias, and a qualified path all reach them the same way.
    let mut rewritten = parsed;
    rewrite_stdlib_struct_surface(&mut rewritten);
    let mut collector = WrapperNameCollector::default();
    gossamer_ast::Visitor::visit_source_file(&mut collector, &rewritten);
    Some(StdImports {
        modules,
        wrapper_names: collector.names,
    })
}

/// The injected wrapper names a rewritten tree's paths spell.
#[derive(Default)]
struct WrapperNameCollector {
    names: std::collections::HashSet<String>,
}

impl WrapperNameCollector {
    fn note<'a>(&mut self, segments: impl Iterator<Item = &'a str>) {
        for name in segments.filter(|name| name.starts_with("__gos_")) {
            self.names.insert(name.to_string());
        }
    }
}

impl gossamer_ast::Visitor for WrapperNameCollector {
    fn visit_path_expr(&mut self, path: &gossamer_ast::PathExpr) {
        self.note(path.segments.iter().map(|segment| segment.name.name.as_str()));
        gossamer_ast::visitor::walk_path_expr(self, path);
    }

    fn visit_type_path(&mut self, path: &gossamer_ast::ty::TypePath) {
        self.note(path.segments.iter().map(|segment| segment.name.name.as_str()));
        gossamer_ast::visitor::walk_type_path(self, path);
    }
}

/// The mangled names a wrapper source declares, each a `fn` or `struct` the
/// rewrite of an item import can name.
fn declared_wrapper_names(wrappers: &str) -> impl Iterator<Item = &str> {
    wrappers.lines().filter_map(|line| {
        let rest = line
            .trim_start()
            .strip_prefix("fn ")
            .or_else(|| line.trim_start().strip_prefix("struct "))?;
        let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
        Some(&rest[..end]).filter(|name| name.starts_with("__gos_"))
    })
}

fn synthesize_stdlib_wrappers(source: &str) -> String {
    // A wrapper set declares top-level items, so it is injected only for a
    // module the source reached through `use std::...`: a project with its
    // own `sql` module keeps its own `Value` and its own `Int`.
    let imported = imported_std(source);
    let from_std = |module: &str| {
        imported.as_ref().is_none_or(|found| {
            // The module itself was imported, or it is reached through one
            // that was: `use std::archive` then `archive::tar::read`.
            found.modules.values().any(|reached| reached == module)
                || found
                    .modules
                    .keys()
                    .any(|outer| mentions_path(source, &format!("{outer}::{module}::")))
        })
    };
    // An item import (`use std::fs::{read_dir}`) rewrites the bare name to the
    // wrapper it reaches, so that wrapper set is injected whatever the source
    // text spells.
    let item_imported = |wrappers: &str| {
        imported.as_ref().is_some_and(|found| {
            declared_wrapper_names(wrappers).any(|name| found.wrapper_names.contains(name))
        })
    };

    let mut stdlib_wrappers = String::new();
    if (mentions_path(source, "pem::") && from_std("pem")) || item_imported(PEM_WRAPPERS) {
        stdlib_wrappers.push_str(PEM_WRAPPERS);
    }
    if (mentions_path(source, "x509::") && from_std("x509")) || item_imported(X509_WRAPPERS) {
        stdlib_wrappers.push_str(X509_WRAPPERS);
    }
    system_wrappers(source, &from_std, &item_imported, &mut stdlib_wrappers);
    if source.contains("Http2Config") {
        stdlib_wrappers.push_str(HTTP2_CONFIG_WRAPPERS);
    }
    if (mentions_path(source, "tar::") && from_std("tar")) || item_imported(TAR_WRAPPERS) {
        stdlib_wrappers.push_str(TAR_WRAPPERS);
    }
    if (mentions_path(source, "zip::") && from_std("zip")) || item_imported(ZIP_WRAPPERS) {
        stdlib_wrappers.push_str(ZIP_WRAPPERS);
    }
    if (mentions_path(source, "sql::") && from_std("sql")) || item_imported(SQL_WRAPPERS) {
        stdlib_wrappers.push_str(SQL_WRAPPERS);
    }
    if HTTP_SECURITY_MARKERS.iter().any(|m| source.contains(m)) {
        stdlib_wrappers.push_str(HTTP_SECURITY_WRAPPERS);
    }
    if item_imported(TIME_TIMER_WRAPPERS)
        || (mentions_module_item(source, "time", "after") && from_std("time"))
    {
        stdlib_wrappers.push_str(TIME_TIMER_WRAPPERS);
    }
    if item_imported(SYNC_SHIELD_WRAPPERS)
        || (mentions_module_item(source, "sync", "shield") && from_std("sync"))
    {
        stdlib_wrappers.push_str(SYNC_SHIELD_WRAPPERS);
    }
    if item_imported(SYNC_TIMEOUT_WRAPPERS)
        || (mentions_module_item(source, "sync", "with_timeout") && from_std("sync"))
    {
        stdlib_wrappers.push_str(SYNC_TIMEOUT_WRAPPERS);
    }
    let wants_time = item_imported(TIME_TIME_WRAPPERS)
        || (mentions_path(source, "time::Time") && from_std("time"));
    if wants_time
        || item_imported(TIME_CIVIL_WRAPPERS)
        || (TIME_CIVIL_MARKERS.iter().any(|marker| source.contains(marker))
            && from_std("time"))
    {
        stdlib_wrappers.push_str(TIME_CIVIL_WRAPPERS);
    }
    if wants_time {
        stdlib_wrappers.push_str(TIME_TIME_WRAPPERS);
    }
    stdlib_wrappers
}

/// The wrapper sets for the host-facing modules `source` reaches: file
/// metadata and directory walks, processes and commands, foreign memory,
/// descriptors and the terminal, signals, and paths.
fn system_wrappers(
    source: &str,
    from_std: &impl Fn(&str) -> bool,
    item_imported: &impl Fn(&str) -> bool,
    out: &mut String,
) {
    if (source.contains("fs::metadata") && from_std("fs")) || item_imported(FS_METADATA_WRAPPERS) {
        out.push_str(FS_METADATA_WRAPPERS);
    }
    if item_imported(FS_DIR_WRAPPERS)
        || (FS_DIR_MARKERS.iter().any(|m| source.contains(m))
            && (from_std("fs") || from_std("path")))
    {
        out.push_str(FS_DIR_WRAPPERS);
    }
    let wants_command = item_imported(PROCESS_COMMAND_WRAPPERS)
        || (PROCESS_COMMAND_MARKERS.iter().any(|m| source.contains(m))
            && (from_std("process") || from_std("exec")));
    if wants_command
        || item_imported(PROCESS_WRAPPERS)
        || (PROCESS_MARKERS.iter().any(|m| source.contains(m))
            && (from_std("process") || from_std("exec")))
    {
        out.push_str(PROCESS_WRAPPERS);
    }
    if wants_command {
        out.push_str(PROCESS_COMMAND_WRAPPERS);
    }
    if (mentions_path(source, "ffi::") && from_std("ffi")) || item_imported(FFI_WRAPPERS) {
        out.push_str(FFI_WRAPPERS);
    }
    let wants_term =
        (mentions_path(source, "term::") && from_std("term")) || item_imported(TERM_WRAPPERS);
    if wants_term || (mentions_path(source, "fd::") && from_std("fd")) || item_imported(FD_WRAPPERS)
    {
        out.push_str(FD_WRAPPERS);
    }
    if wants_term {
        out.push_str(TERM_WRAPPERS);
    }
    if (mentions_path(source, "signal::SIG") && from_std("signal"))
        || item_imported(SIGNAL_WRAPPERS)
    {
        out.push_str(SIGNAL_WRAPPERS);
    }
    if (source.contains("path::Path") && from_std("path")) || item_imported(PATH_WRAPPERS) {
        out.push_str(PATH_WRAPPERS);
    }
}

/// Real-struct + wrapper source for `std::encoding::pem`. The
/// wrappers fold the leaf intrinsics' tuple/byte returns into real
/// `__gos_pem_Block` structs, which lower natively on every tier.
/// Source substrings that pull in [`HTTP_SECURITY_WRAPPERS`]. Only the
/// request/response-integrated gap surface triggers injection; the bare
/// `csrf::issue_token` / `session::sign` / `cookie::*` primitives are
/// already wired and must not drag the wrappers (and their `use`s) into
/// programs that only touch them.
const HTTP_SECURITY_MARKERS: &[&str] = &[
    "csrf::Config",
    "csrf::config",
    "csrf::check",
    "csrf::extract_token",
    "csrf::attach_cookie",
    "csrf::origin_allowed",
    "csrf::RouteAuth",
    "session::signed",
    "session::encrypted",
    "session::with_session",
    "session::save",
    "session::load",
    "session::Store",
    "form::Form",
    "form::parse",
    "multipart::parse",
    "multipart::Part",
    "multipart::boundary",
    "form_file",
];

const PEM_WRAPPERS: &str = r"
struct __gos_pem_Block { block_type: String, bytes: Vec<u8> }
fn __gos_pem_decode(s: String) -> Result<__gos_pem_Block, errors::Error> {
    let t, b = __gos_pem_decode_raw(s)?
    Ok(__gos_pem_Block { block_type: t, bytes: b })
}
fn __gos_pem_decode_all(s: String) -> Result<Vec<__gos_pem_Block>, errors::Error> {
    let raws = __gos_pem_decode_all_raw(s)?
    let mut out: Vec<__gos_pem_Block> = Vec::from([])
    for r in raws {
        out.push(__gos_pem_Block { block_type: r.0, bytes: r.1 })
    }
    Ok(out)
}
fn __gos_pem_encode(b: __gos_pem_Block) -> String {
    __gos_pem_encode_raw(b.block_type, b.bytes)
}
";

/// Channel-returning timer wrapper for `std::time`. `time::after(d)` returns a
/// `Receiver` that yields once after `d`, firing on a goroutine that completes,
/// so the result composes with `select` / `while let`.
const TIME_TIMER_WRAPPERS: &str = r"
fn __gos_time_after_fire(tx: Sender<i64>, d: time::Duration) {
    time::sleep(d)
    tx.send(1)
    tx.close()
}
fn __gos_time_after(d: time::Duration) -> Receiver<i64> {
    let tx, rx = channel(1)
    spawn(|| __gos_time_after_fire(tx, d))
    rx
}
";

/// `sync::shield(f)` - runs `f` in a cohort exempt from cancellation, so a
/// cancel landing on an enclosing cohort does not reach the work inside.
/// Written as the `cohort { }` desugar rather than the block so the
/// callable's own value is the wrapper's, which a block expression - whose
/// value is the cohort's outcome - has no room for. `on_error` is `Log`
/// (`1`) for the same reason: `f`'s value is the answer, so a child `f`
/// spawned is named on stderr where it fails rather than silently dropped.
const SYNC_SHIELD_WRAPPERS: &str = r"
fn __gos_sync_shield<T>(f: Fn() -> T) -> T {
    runtime::cohort_push(0, 0, 0, 1, 1, 0)
    defer runtime::cohort_pop()
    let value = f()
    let _ = runtime::cohort_join()
    value
}
";

/// `sync::with_timeout(f, ms)` - runs `f` on a child of a cohort bounded by
/// `ms`. The bound is a runtime value, which a `cohort(timeout: ..)` header
/// cannot take, so this is written as the desugar the header compiles to.
/// The child's value comes back through a captured `Vec`, which a closure
/// reaches by managed reference; an empty one after the join means the
/// bound elapsed first.
const SYNC_TIMEOUT_WRAPPERS: &str = r#"
fn __gos_sync_with_timeout<T>(f: Fn() -> T, ms: i64) -> Result<T, errors::Error> {
    let mut out: Vec<T> = #[]
    runtime::cohort_push(0, ms, 0, 0, 0, 0)
    defer runtime::cohort_pop()
    spawn(|| out.push(f()), reason: "sync::with_timeout")
    runtime::cohort_join().map_err(|e| errors::wrap(e, format("sync::with_timeout: bound of {} ms", ms)))?
    match out.pop() {
        Some(value) => Ok(value)
        None => Err(errors::new("sync::with_timeout: the work did not finish inside its bound"))
    }
}
"#;

const TIME_CIVIL_WRAPPERS: &str = r#"
struct __gos_time_CivilTime { year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64, nanosecond: i64, offset_seconds: i64, weekday: i64 }
enum __gos_time_CivilResolution { Unique(i64), Gap, Fold(i64, i64) }
struct __gos_time_Location { spec: String }
impl __gos_time_Location {
    fn lookup(name: String) -> Result<__gos_time_Location, errors::Error> {
        Ok(__gos_time_Location { spec: __gos_time_location_raw(name)? })
    }
    fn utc() -> __gos_time_Location { __gos_time_Location { spec: "UTC" } }
    fn fixed(offset_seconds: i64) -> Result<__gos_time_Location, errors::Error> {
        Ok(__gos_time_Location { spec: __gos_time_fixed_location_raw(offset_seconds)? })
    }
    fn name(&self) -> String { self.spec }
    fn civil(&self, unix_ms: i64) -> Result<__gos_time_CivilTime, errors::Error> {
        let year, month, day, hour, minute, second, nano, offset, weekday = __gos_time_civil_raw(unix_ms, self.spec)?
        Ok(__gos_time_CivilTime { year: year, month: month, day: day, hour: hour, minute: minute, second: second, nanosecond: nano, offset_seconds: offset, weekday: weekday })
    }
    fn resolve(&self, civil: __gos_time_CivilTime) -> Result<__gos_time_CivilResolution, errors::Error> {
        let kind, earlier, later = __gos_time_resolve_raw(self.spec, civil.year, civil.month, civil.day, civil.hour, civil.minute, civil.second, civil.nanosecond)?
        if kind == 0 { Ok(__gos_time_CivilResolution::Gap) }
        else if kind == 1 { Ok(__gos_time_CivilResolution::Unique(earlier)) }
        else { Ok(__gos_time_CivilResolution::Fold(earlier, later)) }
    }
}
fn __gos_time_format_in(layout: String, unix_ms: i64, location: __gos_time_Location) -> Result<String, errors::Error> {
    __gos_time_format_in_raw(layout, unix_ms, location.spec)
}
fn __gos_time_add_date(unix_ms: i64, location: __gos_time_Location, years: i64, months: i64, days: i64) -> Result<i64, errors::Error> {
    __gos_time_add_date_raw(unix_ms, location.spec, years, months, days)
}
"#;

/// `time::Time`: a wall-clock instant in nanoseconds since the Unix epoch
/// together with the location it is read in. Arithmetic takes a
/// `Duration`, two instants subtract to one, and RFC 3339 text keeps the
/// offset it was written with. Written over the civil-time wrappers, so
/// every tier runs the same code.
const TIME_TIME_WRAPPERS: &str = r#"
struct __gos_time_Time { unix_ns: i64, location: __gos_time_Location }
fn __gos_time_floor_div(n: i64, d: i64) -> i64 {
    if n >= 0 { n / d } else { 0 - ((0 - n + d - 1) / d) }
}
fn __gos_time_days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year }
    let era = __gos_time_floor_div(y, 400)
    let yoe = y - era * 400
    let mp = (month + 9) % 12
    let doy = (153 * mp + 2) / 5 + day - 1
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy
    era * 146097 + doe - 719468
}
fn __gos_time_digits(text: String, at: i64, count: i64) -> Result<i64, errors::Error> {
    let mut value = 0
    let mut i = 0
    while i < count {
        let b = text.byte_at(at + i)
        if b < 48 || b > 57 {
            return Err(errors::new(format("time::Time::parse_rfc3339: expected a digit at byte {} of `{}`", at + i, text)))
        }
        value = value * 10 + (b - 48)
        i += 1
    }
    Ok(value)
}
fn __gos_time_days_in_month(year: i64, month: i64) -> i64 {
    if month == 2 {
        if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 { 29 } else { 28 }
    } else if month == 4 || month == 6 || month == 9 || month == 11 {
        30
    } else {
        31
    }
}
fn __gos_time_two(n: i64) -> String {
    if n < 10 { format("0{}", n) } else { format("{}", n) }
}
impl __gos_time_Time {
    fn now() -> __gos_time_Time {
        __gos_time_Time { unix_ns: time::now_nanos(), location: __gos_time_Location::utc() }
    }
    fn from_unix(secs: i64) -> __gos_time_Time {
        __gos_time_Time { unix_ns: secs * 1000000000, location: __gos_time_Location::utc() }
    }
    fn from_unix_ms(ms: i64) -> __gos_time_Time {
        __gos_time_Time { unix_ns: ms * 1000000, location: __gos_time_Location::utc() }
    }
    fn from_unix_nanos(ns: i64) -> __gos_time_Time {
        __gos_time_Time { unix_ns: ns, location: __gos_time_Location::utc() }
    }
    fn unix(&self) -> i64 { __gos_time_floor_div(self.unix_ns, 1000000000) }
    fn unix_ms(&self) -> i64 { __gos_time_floor_div(self.unix_ns, 1000000) }
    fn unix_nanos(&self) -> i64 { self.unix_ns }
    fn location(&self) -> __gos_time_Location { self.location }
    fn in_location(&self, location: __gos_time_Location) -> __gos_time_Time {
        __gos_time_Time { unix_ns: self.unix_ns, location: location }
    }
    fn utc(&self) -> __gos_time_Time {
        __gos_time_Time { unix_ns: self.unix_ns, location: __gos_time_Location::utc() }
    }
    fn civil(&self) -> Result<__gos_time_CivilTime, errors::Error> {
        let mut c = self.location.civil(self.unix_ms())?
        c.nanosecond = self.unix_ns - __gos_time_floor_div(self.unix_ns, 1000000000) * 1000000000
        Ok(c)
    }
    fn duration_since(&self, earlier: __gos_time_Time) -> time::Duration {
        time::Duration::from_nanos(self.unix_ns - earlier.unix_ns)
    }
    fn before(&self, other: __gos_time_Time) -> bool { self.unix_ns < other.unix_ns }
    fn after(&self, other: __gos_time_Time) -> bool { self.unix_ns > other.unix_ns }
    fn eq(&self, other: __gos_time_Time) -> bool { self.unix_ns == other.unix_ns }
    fn cmp(&self, other: __gos_time_Time) -> i64 {
        if self.unix_ns < other.unix_ns { -1 } else if self.unix_ns > other.unix_ns { 1 } else { 0 }
    }
    fn parse_rfc3339(text: String) -> Result<__gos_time_Time, errors::Error> {
        let n = text.byte_len()
        if n < 20 {
            return Err(errors::new(format("time::Time::parse_rfc3339: `{}` is too short for RFC 3339", text)))
        }
        let year = __gos_time_digits(text, 0, 4)?
        let month = __gos_time_digits(text, 5, 2)?
        let day = __gos_time_digits(text, 8, 2)?
        let hour = __gos_time_digits(text, 11, 2)?
        let minute = __gos_time_digits(text, 14, 2)?
        let second = __gos_time_digits(text, 17, 2)?
        let t = text.byte_at(10)
        if text.byte_at(4) != 45 || text.byte_at(7) != 45 || (t != 84 && t != 116 && t != 32) || text.byte_at(13) != 58 || text.byte_at(16) != 58 {
            return Err(errors::new(format("time::Time::parse_rfc3339: `{}` is not `YYYY-MM-DDTHH:MM:SS`", text)))
        }
        if month < 1 || month > 12 || day < 1 || day > __gos_time_days_in_month(year, month) || hour > 23 || minute > 59 || second > 59 {
            return Err(errors::new(format("time::Time::parse_rfc3339: `{}` names no calendar instant", text)))
        }
        let mut at = 19
        let mut nanos = 0
        if at < n && text.byte_at(at) == 46 {
            at += 1
            let mut digits = 0
            while at < n && text.byte_at(at) >= 48 && text.byte_at(at) <= 57 {
                if digits < 9 {
                    nanos = nanos * 10 + (text.byte_at(at) - 48)
                    digits += 1
                }
                at += 1
            }
            if digits == 0 {
                return Err(errors::new(format("time::Time::parse_rfc3339: `{}` has an empty fraction", text)))
            }
            while digits < 9 {
                nanos *= 10
                digits += 1
            }
        }
        if at >= n {
            return Err(errors::new(format("time::Time::parse_rfc3339: `{}` has no offset", text)))
        }
        let zone = text.byte_at(at)
        let mut offset = 0
        let mut utc = false
        if zone == 90 || zone == 122 {
            utc = true
            at += 1
        } else if (zone == 43 || zone == 45) && at + 6 == n && text.byte_at(at + 3) == 58 {
            let oh = __gos_time_digits(text, at + 1, 2)?
            let om = __gos_time_digits(text, at + 4, 2)?
            if oh > 23 || om > 59 {
                return Err(errors::new(format("time::Time::parse_rfc3339: `{}` has an offset out of range", text)))
            }
            offset = oh * 3600 + om * 60
            if zone == 45 {
                offset = 0 - offset
            }
            at += 6
        } else {
            return Err(errors::new(format("time::Time::parse_rfc3339: `{}` has no valid offset", text)))
        }
        if at != n {
            return Err(errors::new(format("time::Time::parse_rfc3339: `{}` has text after the offset", text)))
        }
        let days = __gos_time_days_from_civil(year, month, day)
        let secs = days * 86400 + hour * 3600 + minute * 60 + second - offset
        let location = if utc { __gos_time_Location::utc() } else { __gos_time_Location::fixed(offset)? }
        Ok(__gos_time_Time { unix_ns: secs * 1000000000 + nanos, location: location })
    }
    fn format_rfc3339(&self) -> Result<String, errors::Error> {
        let c = self.civil()?
        let mut out = format("{:04}-{}-{}T{}:{}:{}", c.year, __gos_time_two(c.month), __gos_time_two(c.day), __gos_time_two(c.hour), __gos_time_two(c.minute), __gos_time_two(c.second))
        if c.nanosecond != 0 {
            let mut frac = format("{:09}", c.nanosecond)
            while frac.ends_with("0") {
                frac = frac.substring(0, frac.byte_len() - 1)
            }
            out += "."
            out += frac
        }
        if c.offset_seconds == 0 && self.location.spec == "UTC" {
            out += "Z"
        } else {
            let sign = if c.offset_seconds < 0 { "-" } else { "+" }
            let a = if c.offset_seconds < 0 { 0 - c.offset_seconds } else { c.offset_seconds }
            out += format("{}{}:{}", sign, __gos_time_two(a / 3600), __gos_time_two((a % 3600) / 60))
        }
        Ok(out)
    }
}
impl Add<time::Duration> for __gos_time_Time {
    fn add(self, d: time::Duration) -> __gos_time_Time {
        __gos_time_Time { unix_ns: self.unix_ns + d.as_nanos(), location: self.location }
    }
}
impl Sub<time::Duration> for __gos_time_Time {
    fn sub(self, d: time::Duration) -> __gos_time_Time {
        __gos_time_Time { unix_ns: self.unix_ns - d.as_nanos(), location: self.location }
    }
}
impl Sub for __gos_time_Time {
    type Output = time::Duration
    fn sub(self, other: __gos_time_Time) -> time::Duration {
        time::Duration::from_nanos(self.unix_ns - other.unix_ns)
    }
}
impl Display for __gos_time_Time {
    fn fmt(&self) -> String {
        match self.format_rfc3339() {
            Ok(text) => text,
            Err(_) => format("{}ns", self.unix_ns),
        }
    }
}
impl Debug for __gos_time_Time {
    fn fmt(&self) -> String {
        match self.format_rfc3339() {
            Ok(text) => format("Time({})", text),
            Err(_) => format("Time({}ns)", self.unix_ns),
        }
    }
}
"#;

/// Real-struct + wrapper source for `std::crypto::x509`.
const X509_WRAPPERS: &str = r"
struct __gos_x509_CertInfo { subject: String, issuer: String, serial: Vec<u8>, not_before_unix: i64, not_after_unix: i64, san_dns: Vec<String>, sha256: Vec<u8> }
fn __gos_x509_parse_pem(s: String) -> Result<__gos_x509_CertInfo, errors::Error> {
    let subject, issuer, serial, nb, na, san, sha = __gos_x509_parse_pem_raw(s)?
    Ok(__gos_x509_CertInfo { subject: subject, issuer: issuer, serial: serial, not_before_unix: nb, not_after_unix: na, san_dns: san, sha256: sha })
}
";

/// Real-struct + wrapper source for `std::fs::metadata`. Folds the
/// leaf intrinsic's 6-tuple into a real `Metadata` struct so
/// `fs::metadata(p).size` / `.is_file` lower natively on every tier.
/// Field order MUST match the VM's `fs::Metadata` (see
/// `builtin_fs_metadata`).
const FS_METADATA_WRAPPERS: &str = r"
struct __gos_fs_Metadata { size: i64, is_file: bool, is_dir: bool, is_symlink: bool, readonly: bool, modified_unix_ms: i64 }
fn __gos_fs_metadata(path: String) -> Result<__gos_fs_Metadata, errors::Error> {
    let size, is_file, is_dir, is_symlink, readonly, modified = __gos_fs_metadata_raw(path)?
    Ok(__gos_fs_Metadata { size: size, is_file: is_file, is_dir: is_dir, is_symlink: is_symlink, readonly: readonly, modified_unix_ms: modified })
}
";

/// Real-struct + wrapper source for `std::fs::read_dir`. The leaf answers
/// each entry as a tuple its vec owns; the wrapper folds them into
/// `DirInfo` structs, so every field reads through ordinary ownership on
/// every tier. Field order matches the VM's `fs::DirInfo`.
const FS_DIR_WRAPPERS: &str = r"
struct __gos_fs_DirInfo { name: String, path: String, is_file: bool, is_dir: bool, is_symlink: bool, size: i64, modified_ms: i64 }
fn __gos_fs_read_dir(path: String) -> Result<Vec<__gos_fs_DirInfo>, errors::Error> {
    let raws = __gos_fs_read_dir_raw(path)?
    let mut out: Vec<__gos_fs_DirInfo> = Vec::from([])
    for r in raws {
        out.push(__gos_fs_DirInfo { name: r.0, path: r.1, is_file: r.2, is_dir: r.3, is_symlink: r.4, size: r.5, modified_ms: r.6 })
    }
    Ok(out)
}
fn __gos_fs_walk_dir(root: String, visit: Fn(__gos_fs_DirInfo) -> Result<(), errors::Error>) -> Result<(), errors::Error> {
    __gos_fs_walk_dir_raw(root, |r: (String, String, bool, bool, bool, i64, i64)| visit(__gos_fs_DirInfo { name: r.0, path: r.1, is_file: r.2, is_dir: r.3, is_symlink: r.4, size: r.5, modified_ms: r.6 }))
}
";

/// Spellings that reach the civil-time wrappers.
const TIME_CIVIL_MARKERS: &[&str] = &[
    "time::Location",
    "time::CivilTime",
    "time::CivilResolution",
    "time::format_in",
    "time::add_date",
];

/// Spellings that reach the `fs` directory wrappers, including the older
/// `path::walk`.
const FS_DIR_MARKERS: &[&str] = &["fs::read_dir", "fs::walk_dir", "fs::DirInfo", "path::walk"];

/// `std::ffi`: the C type names, chosen per target, and the C string
/// conversions, written in Gossamer so every tier runs the same code.
const FFI_WRAPPERS: &str = r#"
#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "riscv64")))]
type __gos_ffi_c_char = u8
#[cfg(not(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "riscv64"))))]
type __gos_ffi_c_char = i8
type __gos_ffi_c_schar = i8
type __gos_ffi_c_uchar = u8
type __gos_ffi_c_short = i16
type __gos_ffi_c_ushort = u16
type __gos_ffi_c_int = i32
type __gos_ffi_c_uint = u32
#[cfg(windows)]
type __gos_ffi_c_long = i32
#[cfg(not(windows))]
type __gos_ffi_c_long = i64
#[cfg(windows)]
type __gos_ffi_c_ulong = u32
#[cfg(not(windows))]
type __gos_ffi_c_ulong = u64
type __gos_ffi_c_longlong = i64
type __gos_ffi_c_ulonglong = u64
type __gos_ffi_size_t = usize
type __gos_ffi_ssize_t = isize
type __gos_ffi_c_float = f32
type __gos_ffi_c_double = f64

fn __gos_ffi_cstring(text: String) -> Result<Vec<u8>, errors::Error> {
    let mut bytes = text.bytes()
    for byte in bytes {
        if byte == 0 {
            return Err(errors::new("ffi::cstring: the string holds a NUL byte"))
        }
    }
    bytes.push(0)
    Ok(bytes)
}

fn __gos_ffi_from_cstr(bytes: [u8]) -> Result<String, errors::Error> {
    let mut end = 0
    while end < bytes.len() && bytes[end] != 0 {
        end += 1
    }
    String::from_utf8(bytes[0..end]).map_err(|e| errors::wrap(e, "ffi::from_cstr"))
}

#[__gos_foreign_type]
struct __gos_ffi_c_void

struct __gos_ffi_Ptr<T> { __addr: u64 }

impl<T> __gos_ffi_Ptr<T> {
    fn cast<U>(self) -> __gos_ffi_Ptr<U> {
        unsafe { __gos_ffi_Ptr { __addr: self.__addr } }
    }

    fn address(self) -> u64 {
        self.__addr
    }

    fn null() -> __gos_ffi_Ptr<T> {
        unsafe { __gos_ffi_Ptr { __addr: 0 } }
    }

    fn is_null(self) -> bool {
        self.__addr == 0
    }

    fn from_address(addr: u64) -> Option<__gos_ffi_Ptr<T>> {
        if addr == 0 {
            None
        } else {
            Some(unsafe { __gos_ffi_Ptr { __addr: addr } })
        }
    }

    fn fmt(&self) -> String {
        f"Ptr(0x{self.__addr:x})"
    }

    fn to_string(&self) -> String {
        f"Ptr(0x{self.__addr:x})"
    }

    fn eq(&self, other: __gos_ffi_Ptr<T>) -> bool {
        self.__addr == other.__addr
    }
}


unsafe extern "C" {
    fn gos_rt_ffi_read(dst: &mut [u8], src: u64, len: u64)
    fn gos_rt_ffi_write(dst: u64, src: [u8], len: u64)
    fn gos_rt_ffi_strlen(src: u64) -> u64
    fn gos_rt_ffi_free(addr: u64)
}

fn __gos_ffi_read<T>(p: __gos_ffi_Ptr<T>) -> T {
    panic("ffi::read is lowered at its call site")
}

fn __gos_ffi_read_at<T>(p: __gos_ffi_Ptr<T>, index: i64) -> T {
    panic("ffi::read_at is lowered at its call site")
}

fn __gos_ffi_write<T>(p: __gos_ffi_Ptr<T>, value: T) {
    panic("ffi::write is lowered at its call site")
}

fn __gos_ffi_write_at<T>(p: __gos_ffi_Ptr<T>, index: i64, value: T) {
    panic("ffi::write_at is lowered at its call site")
}

fn __gos_ffi_alloc<T>(count: i64) -> __gos_ffi_Ptr<T> {
    panic("ffi::alloc is lowered at its call site")
}

fn __gos_ffi_size_of<T>() -> i64 {
    panic("ffi::size_of is lowered at its call site")
}

fn __gos_ffi_align_of<T>() -> i64 {
    panic("ffi::align_of is lowered at its call site")
}

fn __gos_ffi_offset_of<T>(field: String) -> i64 {
    panic("ffi::offset_of is lowered at its call site")
}

fn __gos_ffi_addr_of<T>(symbol: T) -> __gos_ffi_Ptr<T> {
    panic("ffi::addr_of is lowered at its call site")
}

fn __gos_ffi_atomic_load<T>(p: __gos_ffi_Ptr<T>) -> T {
    panic("ffi::atomic_load is lowered at its call site")
}

fn __gos_ffi_atomic_store<T>(p: __gos_ffi_Ptr<T>, value: T) {
    panic("ffi::atomic_store is lowered at its call site")
}

fn __gos_ffi_atomic_swap<T>(p: __gos_ffi_Ptr<T>, value: T) -> T {
    panic("ffi::atomic_swap is lowered at its call site")
}

fn __gos_ffi_atomic_compare_exchange<T>(p: __gos_ffi_Ptr<T>, current: T, new: T) -> Result<T, T> {
    panic("ffi::atomic_compare_exchange is lowered at its call site")
}

fn __gos_ffi_atomic_fetch_add<T>(p: __gos_ffi_Ptr<T>, value: T) -> T {
    panic("ffi::atomic_fetch_add is lowered at its call site")
}

fn __gos_ffi_atomic_fetch_sub<T>(p: __gos_ffi_Ptr<T>, value: T) -> T {
    panic("ffi::atomic_fetch_sub is lowered at its call site")
}

fn __gos_ffi_atomic_fetch_and<T>(p: __gos_ffi_Ptr<T>, value: T) -> T {
    panic("ffi::atomic_fetch_and is lowered at its call site")
}

fn __gos_ffi_atomic_fetch_or<T>(p: __gos_ffi_Ptr<T>, value: T) -> T {
    panic("ffi::atomic_fetch_or is lowered at its call site")
}

fn __gos_ffi_atomic_fetch_xor<T>(p: __gos_ffi_Ptr<T>, value: T) -> T {
    panic("ffi::atomic_fetch_xor is lowered at its call site")
}

struct __gos_ffi_View<T> { __addr: u64, __len: i64 }

impl<T> __gos_ffi_View<T> {
    fn new(p: __gos_ffi_Ptr<T>, len: i64) -> __gos_ffi_View<T> {
        unsafe { __gos_ffi_View { __addr: p.__addr, __len: len } }
    }

    fn len(&self) -> i64 {
        self.__len
    }

    fn is_empty(&self) -> bool {
        self.__len == 0
    }

    fn ptr(&self) -> __gos_ffi_Ptr<T> {
        unsafe { __gos_ffi_Ptr { __addr: self.__addr } }
    }

    fn get(&self, index: i64) -> T {
        panic("ffi::View::get is lowered at its call site")
    }

    fn index(&self, index: i64) -> T {
        panic("ffi::View indexing is lowered at its call site")
    }

    fn set(&self, index: i64, value: T) {
        panic("ffi::View::set is lowered at its call site")
    }

    fn slice(&self, lo: i64, hi: i64) -> __gos_ffi_View<T> {
        panic("ffi::View::slice is lowered at its call site")
    }

    fn to_vec(&self) -> Vec<T> {
        panic("ffi::View::to_vec is lowered at its call site")
    }

    fn copy_from(&self, values: [T]) {
        panic("ffi::View::copy_from is lowered at its call site")
    }

    fn fill(&self, value: T) {
        panic("ffi::View::fill is lowered at its call site")
    }

    fn fmt(&self) -> String {
        f"View(0x{self.__addr:x}, {self.__len})"
    }

    fn to_string(&self) -> String {
        f"View(0x{self.__addr:x}, {self.__len})"
    }

    fn eq(&self, other: __gos_ffi_View<T>) -> bool {
        self.__addr == other.__addr && self.__len == other.__len
    }
}

struct __gos_ffi_Union<M> { __bytes: [u8; 0] }

impl<M> __gos_ffi_Union<M> {
    fn new<T>(value: T) -> __gos_ffi_Union<M> {
        panic("ffi::Union::new is lowered at its call site")
    }

    fn zeroed() -> __gos_ffi_Union<M> {
        panic("ffi::Union::zeroed is lowered at its call site")
    }

    fn get<T>(self) -> T {
        panic("ffi::Union::get is lowered at its call site")
    }

    fn set<T>(&mut self, value: T) {
        panic("ffi::Union::set is lowered at its call site")
    }
}

fn __gos_ffi_free<T>(p: __gos_ffi_Ptr<T>) {
    unsafe { gos_rt_ffi_free(p.__addr) }
}

fn __gos_ffi_read_bytes(p: __gos_ffi_Ptr<u8>, len: i64) -> Vec<u8> {
    let mut out = #[0u8; len]
    unsafe { gos_rt_ffi_read(&mut out, p.__addr, len as u64) }
    out
}

fn __gos_ffi_read_cstr(p: __gos_ffi_Ptr<u8>) -> Result<String, errors::Error> {
    let len = unsafe { gos_rt_ffi_strlen(p.__addr) } as i64
    let bytes = unsafe { __gos_ffi_read_bytes(p, len) }
    String::from_utf8(bytes).map_err(|e| errors::wrap(e, "ffi::read_cstr"))
}

fn __gos_ffi_write_bytes(p: __gos_ffi_Ptr<u8>, bytes: [u8]) {
    unsafe { gos_rt_ffi_write(p.__addr, bytes, bytes.len() as u64) }
}

fn __gos_ffi_to_c_bytes(bytes: [u8]) -> __gos_ffi_Ptr<u8> {
    let copy: __gos_ffi_Ptr<u8> = unsafe { __gos_ffi_alloc(bytes.len()) }
    unsafe { __gos_ffi_write_bytes(copy, bytes) }
    copy
}

fn __gos_ffi_fn_addr<F>(f: F) -> __gos_ffi_Ptr<__gos_ffi_c_void> {
    panic("ffi::fn_addr is lowered at its call site")
}

fn __gos_ffi_fn_from_ptr<F>(p: __gos_ffi_Ptr<__gos_ffi_c_void>) -> F {
    panic("ffi::fn_from_ptr is lowered at its call site")
}

struct __gos_ffi_Handle<T> { __id: u64 }

impl<T> __gos_ffi_Handle<T> {
    fn new(value: T) -> __gos_ffi_Handle<T> {
        panic("ffi::Handle::new is lowered at its call site")
    }

    fn from_ptr(p: __gos_ffi_Ptr<__gos_ffi_c_void>) -> __gos_ffi_Handle<T> {
        unsafe { __gos_ffi_Handle { __id: p.__addr } }
    }

    fn as_ptr(self) -> __gos_ffi_Ptr<__gos_ffi_c_void> {
        unsafe { __gos_ffi_Ptr { __addr: self.__id } }
    }

    fn get(self) -> T {
        panic("ffi::Handle::get is lowered at its call site")
    }

    fn set(self, value: T) {
        panic("ffi::Handle::set is lowered at its call site")
    }

    fn update(self, f: Fn(&mut T)) {
        panic("ffi::Handle::update is lowered at its call site")
    }

    fn take(self) -> T {
        panic("ffi::Handle::take is lowered at its call site")
    }

    fn release(self) {
        panic("ffi::Handle::release is lowered at its call site")
    }

    fn fmt(&self) -> String {
        f"Handle({self.__id})"
    }

    fn to_string(&self) -> String {
        f"Handle({self.__id})"
    }

    fn eq(&self, other: __gos_ffi_Handle<T>) -> bool {
        self.__id == other.__id
    }
}
"#;

/// `std::os::fd`: waiting for a descriptor to be readable or writable,
/// over the runtime's `__gos_fd_wait_raw` leaf.
const FD_WRAPPERS: &str = r"
fn __gos_fd_wait_readable(fd: i64, timeout_ms: i64) -> Result<bool, errors::Error> {
    let ready = __gos_fd_wait_raw(fd, 0, timeout_ms)?
    Ok(ready == 1)
}

fn __gos_fd_wait_writable(fd: i64, timeout_ms: i64) -> Result<bool, errors::Error> {
    let ready = __gos_fd_wait_raw(fd, 1, timeout_ms)?
    Ok(ready == 1)
}
";

/// `std::os::signal` signal numbers, per target, for `signal::on`.
const SIGNAL_WRAPPERS: &str = r#"
const __gos_signal_SIGHUP: i64 = 1
const __gos_signal_SIGINT: i64 = 2
const __gos_signal_SIGQUIT: i64 = 3
const __gos_signal_SIGTERM: i64 = 15
#[cfg(target_os = "macos")]
const __gos_signal_SIGUSR1: i64 = 30
#[cfg(not(target_os = "macos"))]
const __gos_signal_SIGUSR1: i64 = 10
#[cfg(target_os = "macos")]
const __gos_signal_SIGUSR2: i64 = 31
#[cfg(not(target_os = "macos"))]
const __gos_signal_SIGUSR2: i64 = 12
const __gos_signal_SIGWINCH: i64 = 28
#[cfg(target_os = "macos")]
const __gos_signal_SIGTSTP: i64 = 18
#[cfg(not(target_os = "macos"))]
const __gos_signal_SIGTSTP: i64 = 20
#[cfg(target_os = "macos")]
const __gos_signal_SIGCONT: i64 = 19
#[cfg(not(target_os = "macos"))]
const __gos_signal_SIGCONT: i64 = 18
"#;

/// `std::term`: terminal detection, size, raw mode, and input, written in
/// Gossamer over the platform C library (termios on Linux and macOS, the
/// console API on Windows). Restoring raw mode is registered with
/// `runtime::at_exit`, so it happens on every way the program ends.
const TERM_WRAPPERS: &str = r#"
const __gos_term_STDIN: i64 = 0
const __gos_term_STDOUT: i64 = 1
const __gos_term_STDERR: i64 = 2

static mut __gos_term_LAST_COLS: i64 = -1
static mut __gos_term_LAST_ROWS: i64 = -1

fn __gos_term_os_error(operation: String) -> errors::Error {
    errors::new(f"term::{operation}: os error {ffi::last_os_error()}")
}

fn __gos_term_read_input(timeout_ms: i64, fd: i64 = 0) -> Result<Vec<u8>, errors::Error> {
    if __gos_fd_wait_raw(__gos_term_handle(fd), 0, timeout_ms)? != 1 {
        return Ok(#[])
    }
    __gos_term_read_ready(fd)
}

fn __gos_term_resized(fd: i64 = 1) -> bool {
    let Ok(size) = __gos_term_size(fd) else {
        return false
    }
    let cols, rows = size
    let changed = __gos_term_LAST_COLS >= 0 && (cols != __gos_term_LAST_COLS || rows != __gos_term_LAST_ROWS)
    __gos_term_LAST_COLS = cols
    __gos_term_LAST_ROWS = rows
    changed
}

#[cfg(target_os = "linux")]
type __gos_term_Flag = u32
#[cfg(target_os = "macos")]
type __gos_term_Flag = u64

#[repr(C)]
#[cfg(target_os = "linux")]
struct __gos_term_Termios {
    iflag: u32
    oflag: u32
    cflag: u32
    lflag: u32
    line: u8
    cc: [u8; 32]
    ispeed: u32
    ospeed: u32
}

#[cfg(target_os = "linux")]
fn __gos_term_blank_termios() -> __gos_term_Termios {
    __gos_term_Termios { iflag: 0, oflag: 0, cflag: 0, lflag: 0, line: 0, cc: [0u8; 32], ispeed: 0, ospeed: 0 }
}

#[repr(C)]
#[cfg(target_os = "macos")]
struct __gos_term_Termios {
    iflag: u64
    oflag: u64
    cflag: u64
    lflag: u64
    cc: [u8; 20]
    ispeed: u64
    ospeed: u64
}

#[cfg(target_os = "macos")]
fn __gos_term_blank_termios() -> __gos_term_Termios {
    __gos_term_Termios { iflag: 0, oflag: 0, cflag: 0, lflag: 0, cc: [0u8; 20], ispeed: 0, ospeed: 0 }
}

#[cfg(target_os = "linux")]
const __gos_term_INPUT_RAW_OFF: u32 = 0x5eb
#[cfg(target_os = "linux")]
const __gos_term_LOCAL_RAW_OFF: u32 = 0x804b
#[cfg(target_os = "linux")]
const __gos_term_CSIZE_PARENB: u32 = 0x130
#[cfg(target_os = "linux")]
const __gos_term_CS8: u32 = 0x30
#[cfg(target_os = "linux")]
const __gos_term_VMIN: i64 = 6
#[cfg(target_os = "linux")]
const __gos_term_VTIME: i64 = 5
#[cfg(target_os = "linux")]
const __gos_term_TIOCGWINSZ: u64 = 0x5413

#[cfg(target_os = "macos")]
const __gos_term_INPUT_RAW_OFF: u64 = 0x3eb
#[cfg(target_os = "macos")]
const __gos_term_LOCAL_RAW_OFF: u64 = 0x598
#[cfg(target_os = "macos")]
const __gos_term_CSIZE_PARENB: u64 = 0x1300
#[cfg(target_os = "macos")]
const __gos_term_CS8: u64 = 0x300
#[cfg(target_os = "macos")]
const __gos_term_VMIN: i64 = 16
#[cfg(target_os = "macos")]
const __gos_term_VTIME: i64 = 17
#[cfg(target_os = "macos")]
const __gos_term_TIOCGWINSZ: u64 = 0x40087468

#[cfg(any(target_os = "linux", target_os = "macos"))]
const __gos_term_OPOST: __gos_term_Flag = 1
#[cfg(any(target_os = "linux", target_os = "macos"))]
const __gos_term_TCSAFLUSH: i32 = 2

#[repr(C)]
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct __gos_term_Winsize {
    rows: u16
    cols: u16
    xpixel: u16
    ypixel: u16
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe extern "C" {
    #[link_name = "isatty"]
    fn __gos_term_isatty(fd: i32) -> i32
    #[link_name = "gos_rt_ffi_ioctl"]
    fn __gos_term_ioctl_winsize(fd: i32, request: u64, size: &mut __gos_term_Winsize) -> i32
    #[link_name = "read"]
    fn __gos_term_read(fd: i32, buf: &mut [u8], count: usize) -> isize
    #[link_name = "tcgetattr"]
    fn __gos_term_tcgetattr(fd: i32, attrs: &mut __gos_term_Termios) -> i32
    #[link_name = "tcsetattr"]
    fn __gos_term_tcsetattr(fd: i32, action: i32, attrs: &mut __gos_term_Termios) -> i32
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct __gos_term_RawMode {
    fd: i32
    saved: __gos_term_Termios
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl __gos_term_RawMode {
    fn restore(&self) {
        let mut saved = self.saved
        let _ = unsafe { __gos_term_tcsetattr(self.fd, __gos_term_TCSAFLUSH, &mut saved) }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn __gos_term_handle(fd: i64) -> i64 {
    fd
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn __gos_term_is_terminal(stream: i64) -> bool {
    unsafe { __gos_term_isatty(stream as i32) } == 1
}

// A standard stream asked for its size falls back to the other two, so a
// program whose output is redirected still reads the terminal's size.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn __gos_term_size(fd: i64 = 1) -> Result<(i64, i64), errors::Error> {
    let candidates = if fd < 3 { #[fd, 1, 0, 2] } else { #[fd] }
    for candidate in candidates {
        let mut size = __gos_term_Winsize { rows: 0, cols: 0, xpixel: 0, ypixel: 0 }
        if unsafe { __gos_term_ioctl_winsize(candidate as i32, __gos_term_TIOCGWINSZ, &mut size) } == 0 {
            return Ok((size.cols as i64, size.rows as i64))
        }
    }
    Err(__gos_term_os_error("size"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn __gos_term_enter_raw(fd: i64 = 0) -> Result<__gos_term_RawMode, errors::Error> {
    let fd = fd as i32
    let mut saved = __gos_term_blank_termios()
    if unsafe { __gos_term_tcgetattr(fd, &mut saved) } != 0 {
        return Err(__gos_term_os_error("enter_raw"))
    }
    let mut attrs = saved
    attrs.iflag = attrs.iflag & !__gos_term_INPUT_RAW_OFF
    attrs.oflag = attrs.oflag & !__gos_term_OPOST
    attrs.lflag = attrs.lflag & !__gos_term_LOCAL_RAW_OFF
    attrs.cflag = (attrs.cflag & !__gos_term_CSIZE_PARENB) | __gos_term_CS8
    attrs.cc[__gos_term_VMIN] = 1
    attrs.cc[__gos_term_VTIME] = 0
    if unsafe { __gos_term_tcsetattr(fd, __gos_term_TCSAFLUSH, &mut attrs) } != 0 {
        return Err(__gos_term_os_error("enter_raw"))
    }
    let mode = __gos_term_RawMode { fd: fd, saved: saved }
    runtime::at_exit(|| mode.restore())
    Ok(mode)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn __gos_term_read_ready(fd: i64) -> Result<Vec<u8>, errors::Error> {
    let mut buf = #[0u8; 4096]
    let n = unsafe { __gos_term_read(fd as i32, &mut buf, 4096) }
    if n < 0 {
        return Err(__gos_term_os_error("read_input"))
    }
    Ok(buf[0..n as i64])
}

#[repr(C)]
#[cfg(windows)]
struct __gos_term_ConsoleInfo {
    size_x: i16
    size_y: i16
    cursor_x: i16
    cursor_y: i16
    attributes: u16
    left: i16
    top: i16
    right: i16
    bottom: i16
    max_x: i16
    max_y: i16
}

#[cfg(windows)]
const __gos_term_ENABLE_PROCESSED_INPUT: u32 = 0x1
#[cfg(windows)]
const __gos_term_ENABLE_LINE_INPUT: u32 = 0x2
#[cfg(windows)]
const __gos_term_ENABLE_ECHO_INPUT: u32 = 0x4
#[cfg(windows)]
const __gos_term_ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x200
#[cfg(windows)]
const __gos_term_ENABLE_PROCESSED_OUTPUT: u32 = 0x1
#[cfg(windows)]
const __gos_term_ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4

#[cfg(windows)]
unsafe extern "C" {
    #[link_name = "GetStdHandle"]
    fn __gos_term_GetStdHandle(which: u32) -> usize
    #[link_name = "GetConsoleMode"]
    fn __gos_term_GetConsoleMode(handle: usize, mode: &mut [u32]) -> i32
    #[link_name = "SetConsoleMode"]
    fn __gos_term_SetConsoleMode(handle: usize, mode: u32) -> i32
    #[link_name = "GetConsoleScreenBufferInfo"]
    fn __gos_term_GetConsoleScreenBufferInfo(handle: usize, info: &mut __gos_term_ConsoleInfo) -> i32
    #[link_name = "ReadFile"]
    fn __gos_term_ReadFile(handle: usize, buf: &mut [u8], count: u32, read: &mut [u32], overlapped: usize) -> i32
}

// 0, 1, and 2 name the standard handles; any other value is a handle the
// program opened, such as `CONIN$` (real handles are multiples of four).
#[cfg(windows)]
fn __gos_term_handle(fd: i64) -> i64 {
    let which: u32 = match fd {
        0 => 0xfffffff6,
        1 => 0xfffffff5,
        2 => 0xfffffff4,
        _ => return fd,
    }
    unsafe { __gos_term_GetStdHandle(which) } as i64
}

#[cfg(windows)]
struct __gos_term_RawMode {
    input: usize
    input_mode: u32
    output_mode: Option<u32>
}

#[cfg(windows)]
impl __gos_term_RawMode {
    fn restore(&self) {
        let _ = unsafe { __gos_term_SetConsoleMode(self.input, self.input_mode) }
        if let Some(output_mode) = self.output_mode {
            let _ = unsafe { __gos_term_SetConsoleMode(__gos_term_handle(1) as usize, output_mode) }
        }
    }
}

#[cfg(windows)]
fn __gos_term_is_terminal(stream: i64) -> bool {
    let mut mode = #[0u32]
    unsafe { __gos_term_GetConsoleMode(__gos_term_handle(stream) as usize, &mut mode) } != 0
}

#[cfg(windows)]
fn __gos_term_size(fd: i64 = 1) -> Result<(i64, i64), errors::Error> {
    let mut info = __gos_term_ConsoleInfo { size_x: 0, size_y: 0, cursor_x: 0, cursor_y: 0, attributes: 0, left: 0, top: 0, right: 0, bottom: 0, max_x: 0, max_y: 0 }
    if unsafe { __gos_term_GetConsoleScreenBufferInfo(__gos_term_handle(fd) as usize, &mut info) } == 0 {
        return Err(__gos_term_os_error("size"))
    }
    Ok(((info.right - info.left) as i64 + 1, (info.bottom - info.top) as i64 + 1))
}

// Standard output gains virtual-terminal processing when it is a console;
// output redirected elsewhere leaves only the input in raw mode.
#[cfg(windows)]
fn __gos_term_enter_raw(fd: i64 = 0) -> Result<__gos_term_RawMode, errors::Error> {
    let input = __gos_term_handle(fd) as usize
    let output = __gos_term_handle(1) as usize
    let mut input_mode = #[0u32]
    if unsafe { __gos_term_GetConsoleMode(input, &mut input_mode) } == 0 {
        return Err(__gos_term_os_error("enter_raw"))
    }
    let raw_input = (input_mode[0] & !(__gos_term_ENABLE_PROCESSED_INPUT | __gos_term_ENABLE_LINE_INPUT | __gos_term_ENABLE_ECHO_INPUT)) | __gos_term_ENABLE_VIRTUAL_TERMINAL_INPUT
    if unsafe { __gos_term_SetConsoleMode(input, raw_input) } == 0 {
        return Err(__gos_term_os_error("enter_raw"))
    }
    let mut output_mode = #[0u32]
    let mut saved_output: Option<u32> = None
    if unsafe { __gos_term_GetConsoleMode(output, &mut output_mode) } != 0 {
        let raw_output = output_mode[0] | __gos_term_ENABLE_PROCESSED_OUTPUT | __gos_term_ENABLE_VIRTUAL_TERMINAL_PROCESSING
        if unsafe { __gos_term_SetConsoleMode(output, raw_output) } == 0 {
            let _ = unsafe { __gos_term_SetConsoleMode(input, input_mode[0]) }
            return Err(__gos_term_os_error("enter_raw"))
        }
        saved_output = Some(output_mode[0])
    }
    let mode = __gos_term_RawMode { input: input, input_mode: input_mode[0], output_mode: saved_output }
    runtime::at_exit(|| mode.restore())
    Ok(mode)
}

#[cfg(windows)]
fn __gos_term_read_ready(fd: i64) -> Result<Vec<u8>, errors::Error> {
    let mut buf = #[0u8; 4096]
    let mut read = #[0u32]
    if unsafe { __gos_term_ReadFile(__gos_term_handle(fd) as usize, &mut buf, 4096, &mut read, 0) } == 0 {
        return Err(__gos_term_os_error("read_input"))
    }
    Ok(buf[0..read[0] as i64])
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
struct __gos_term_RawMode {}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl __gos_term_RawMode {
    fn restore(&self) {
        ()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn __gos_term_is_terminal(stream: i64) -> bool {
    false
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn __gos_term_size(fd: i64 = 1) -> Result<(i64, i64), errors::Error> {
    Err(errors::new("term::size: this target has no terminal"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn __gos_term_enter_raw(fd: i64 = 0) -> Result<__gos_term_RawMode, errors::Error> {
    Err(errors::new("term::enter_raw: this target has no terminal"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn __gos_term_handle(fd: i64) -> i64 {
    fd
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn __gos_term_read_ready(fd: i64) -> Result<Vec<u8>, errors::Error> {
    Err(errors::new("term::read_input: this target has no terminal"))
}
"#;

/// Real-struct + wrapper source for `std::process::run` / `run_in`. The
/// leaf answers `(stdout, stderr, code)` as a counted tuple that owns both
/// strings; the wrapper folds it into the `Output` struct.
const PROCESS_WRAPPERS: &str = r"
struct __gos_process_Output { stdout: String, stderr: String, code: i64 }
fn __gos_process_run(program: String, args: Vec<String>) -> Result<__gos_process_Output, errors::Error> {
    let stdout, stderr, code = __gos_process_run_raw(program, args)?
    Ok(__gos_process_Output { stdout: stdout, stderr: stderr, code: code })
}
fn __gos_process_run_in(program: String, args: Vec<String>, dir: String, env: Vec<(String, String)>) -> Result<__gos_process_Output, errors::Error> {
    let stdout, stderr, code = __gos_process_run_in_raw(program, args, dir, env)?
    Ok(__gos_process_Output { stdout: stdout, stderr: stderr, code: code })
}
fn __gos_process_pipeline_run(commands: Vec<String>) -> Result<__gos_process_Output, errors::Error> {
    let stdout, stderr, code = __gos_process_pipeline_run_raw(commands)?
    Ok(__gos_process_Output { stdout: stdout, stderr: stderr, code: code })
}
";

/// `std::process::Command`, `Stdio`, and `Child`, written in Gossamer over
/// the runtime's `gos_rt_command_*` entries, so every tier runs the same
/// code. `spawn_piped` is a `Command` with piped stdin and stdout.
const PROCESS_COMMAND_WRAPPERS: &str = r#"
enum __gos_process_Stdio {
    Inherit,
    Null,
    Piped,
    File(fs::File),
}

struct __gos_process_Command {
    __program: String,
    __spec: Vec<u8>,
    __stdin: __gos_process_Stdio,
    __stdout: __gos_process_Stdio,
    __stderr: __gos_process_Stdio,
    __flags: i64,
    __ctty: i64,
}

struct __gos_process_Child {
    __handle: i64,
    __pid: i64,
}

fn __gos_process_record(mut spec: Vec<u8>, tag: u8, text: String) -> Vec<u8> {
    let bytes = text.bytes()
    let n = bytes.len()
    spec.push(tag)
    spec.push((n & 255) as u8)
    spec.push((n >> 8 & 255) as u8)
    spec.push((n >> 16 & 255) as u8)
    spec.push((n >> 24 & 255) as u8)
    for byte in bytes {
        spec.push(byte)
    }
    spec
}

fn __gos_process_stdio_mode(stdio: __gos_process_Stdio) -> Result<(i64, i64), errors::Error> {
    match stdio {
        __gos_process_Stdio::Inherit => Ok((0, -1)),
        __gos_process_Stdio::Null => Ok((1, -1)),
        __gos_process_Stdio::Piped => Ok((2, -1)),
        __gos_process_Stdio::File(file) => Ok((3, file.fd()?)),
    }
}

fn __gos_process_command_error(code: i64) -> errors::Error {
    let mut empty = #[0u8; 0]
    let len = unsafe { gos_rt_command_error(code, &mut empty, 0) } as i64
    let mut text = #[0u8; len]
    unsafe { gos_rt_command_error(code, &mut text, len as u64) }
    match String::from_utf8(text) {
        Ok(message) => errors::new(message),
        Err(e) => errors::wrap(e, "process::Command"),
    }
}

impl __gos_process_Command {
    fn new(program: String) -> __gos_process_Command {
        __gos_process_Command {
            __program: program,
            __spec: __gos_process_record(#[], 80u8, program),
            __stdin: __gos_process_Stdio::Inherit,
            __stdout: __gos_process_Stdio::Inherit,
            __stderr: __gos_process_Stdio::Inherit,
            __flags: 0,
            __ctty: -1,
        }
    }

    fn arg(self, arg: String) -> __gos_process_Command {
        let mut next = self
        next.__spec = __gos_process_record(next.__spec, 65u8, arg)
        next
    }

    fn args(self, args: [String]) -> __gos_process_Command {
        let mut next = self
        for arg in args {
            next.__spec = __gos_process_record(next.__spec, 65u8, arg)
        }
        next
    }

    fn env(self, key: String, value: String) -> __gos_process_Command {
        let mut next = self
        next.__spec = __gos_process_record(next.__spec, 69u8, f"{key}={value}")
        next
    }

    fn env_remove(self, key: String) -> __gos_process_Command {
        let mut next = self
        next.__spec = __gos_process_record(next.__spec, 82u8, key)
        next
    }

    fn env_clear(self) -> __gos_process_Command {
        let mut next = self
        next.__spec = __gos_process_record(next.__spec, 67u8, "")
        next
    }

    fn dir(self, dir: String) -> __gos_process_Command {
        let mut next = self
        next.__spec = __gos_process_record(next.__spec, 68u8, dir)
        next
    }

    fn stdin(self, stdio: __gos_process_Stdio) -> __gos_process_Command {
        let mut next = self
        next.__stdin = stdio
        next
    }

    fn stdout(self, stdio: __gos_process_Stdio) -> __gos_process_Command {
        let mut next = self
        next.__stdout = stdio
        next
    }

    fn stderr(self, stdio: __gos_process_Stdio) -> __gos_process_Command {
        let mut next = self
        next.__stderr = stdio
        next
    }

    fn new_process_group(self) -> __gos_process_Command {
        let mut next = self
        next.__flags = next.__flags | 1
        next
    }

    fn new_session(self) -> __gos_process_Command {
        let mut next = self
        next.__flags = next.__flags | 2
        next
    }

    fn controlling_terminal(self, fd: i64) -> __gos_process_Command {
        let mut next = self
        next.__ctty = fd
        next
    }

    fn spawn(&self) -> Result<__gos_process_Child, errors::Error> {
        let in_mode, in_fd = __gos_process_stdio_mode(self.__stdin)?
        let out_mode, out_fd = __gos_process_stdio_mode(self.__stdout)?
        let err_mode, err_fd = __gos_process_stdio_mode(self.__stderr)?
        let stdio = #[in_mode, out_mode, err_mode, in_fd, out_fd, err_fd]
        let spec = self.__spec
        let handle = unsafe {
            gos_rt_command_spawn(spec, spec.len() as u64, stdio, self.__flags, self.__ctty)
        }
        if handle < 0 {
            return Err(__gos_process_command_error(handle))
        }
        let pid = unsafe { gos_rt_command_pid(handle) }
        Ok(__gos_process_Child { __handle: handle, __pid: pid })
    }

    fn status(&self) -> Result<i64, errors::Error> {
        self.spawn()?.wait()
    }

    fn output(&self) -> Result<__gos_process_Output, errors::Error> {
        let mut piped = self.stdout(__gos_process_Stdio::Piped).stderr(__gos_process_Stdio::Piped)
        let inherits_stdin = match piped.__stdin {
            __gos_process_Stdio::Inherit => true,
            _ => false,
        }
        if inherits_stdin {
            piped = piped.stdin(__gos_process_Stdio::Null)
        }
        let child = piped.spawn()?
        let mut code = 0
        let done = unsafe { gos_rt_command_collect(child.__handle, &mut code) }
        if done < 0 {
            return Err(__gos_process_command_error(done))
        }
        let stdout = child.__take_text(1)?
        let stderr = child.__take_text(2)?
        Ok(__gos_process_Output { stdout: stdout, stderr: stderr, code: code })
    }
}

impl __gos_process_Child {
    fn id(&self) -> i64 {
        self.__pid
    }

    fn __read(&self, stream: i64, mode: i64, max: i64) -> Result<Option<Vec<u8>>, errors::Error> {
        let len = unsafe { gos_rt_command_read(self.__handle, stream, mode, max) }
        if len == -1 {
            return Ok(None)
        }
        if len < 0 {
            return Err(__gos_process_command_error(len))
        }
        let mut bytes = #[0u8; len]
        unsafe { gos_rt_command_take(self.__handle, stream, &mut bytes, len as u64) }
        Ok(Some(bytes))
    }

    fn __take_text(&self, stream: i64) -> Result<String, errors::Error> {
        let len = unsafe { gos_rt_command_pending(self.__handle, stream) } as i64
        let mut bytes = #[0u8; len]
        unsafe { gos_rt_command_take(self.__handle, stream, &mut bytes, len as u64) }
        String::from_utf8(bytes).map_err(|e| errors::wrap(e, "process::Child"))
    }

    fn __text(&self, stream: i64, mode: i64) -> Result<Option<String>, errors::Error> {
        match self.__read(stream, mode, 0)? {
            Some(bytes) => Ok(Some(String::from_utf8(bytes).map_err(|e| errors::wrap(e, "process::Child"))?)),
            None => Ok(None),
        }
    }

    fn write_stdin(&self, text: String) -> bool {
        self.write_stdin_bytes(text.bytes()).is_ok()
    }

    fn write_stdin_bytes(&self, data: [u8]) -> Result<(), errors::Error> {
        let done = unsafe { gos_rt_command_write(self.__handle, data, data.len() as u64) }
        if done < 0 {
            return Err(__gos_process_command_error(done))
        }
        Ok(())
    }

    fn close_stdin(&self) {
        unsafe { gos_rt_command_close(self.__handle, 0) }
    }

    fn read_line(&self) -> Option<String> {
        self.__text(1, 1).unwrap_or(None)
    }

    fn read_stdout(&self) -> String {
        self.__text(1, 2).unwrap_or(None).unwrap_or("")
    }

    fn read_stderr(&self) -> String {
        self.__text(2, 2).unwrap_or(None).unwrap_or("")
    }

    fn read_stdout_line(&self) -> Result<Option<String>, errors::Error> {
        self.__text(1, 1)
    }

    fn read_stderr_line(&self) -> Result<Option<String>, errors::Error> {
        self.__text(2, 1)
    }

    fn read_stdout_chunk(&self, max: i64) -> Result<Option<Vec<u8>>, errors::Error> {
        self.__read(1, 0, max)
    }

    fn read_stderr_chunk(&self, max: i64) -> Result<Option<Vec<u8>>, errors::Error> {
        self.__read(2, 0, max)
    }

    fn wait(&self) -> Result<i64, errors::Error> {
        let mut code = 0
        let done = unsafe { gos_rt_command_wait(self.__handle, -1, &mut code) }
        if done < 0 {
            return Err(__gos_process_command_error(done))
        }
        Ok(code)
    }

    fn wait_timeout(&self, ms: i64) -> Result<Option<i64>, errors::Error> {
        let mut code = 0
        let done = unsafe { gos_rt_command_wait(self.__handle, ms, &mut code) }
        if done < 0 {
            return Err(__gos_process_command_error(done))
        }
        if done == 0 {
            return Ok(None)
        }
        Ok(Some(code))
    }

    fn kill(&self) -> bool {
        unsafe { gos_rt_command_kill(self.__handle, 9, 0) } == 0
    }

    fn signal(&self, signal: i64) -> Result<(), errors::Error> {
        let done = unsafe { gos_rt_command_kill(self.__handle, signal, 0) }
        if done < 0 {
            return Err(__gos_process_command_error(done))
        }
        Ok(())
    }

    fn kill_group(&self, signal: i64 = 15) -> Result<(), errors::Error> {
        let done = unsafe { gos_rt_command_kill(self.__handle, signal, 1) }
        if done < 0 {
            return Err(__gos_process_command_error(done))
        }
        Ok(())
    }
}

fn __gos_process_spawn_piped(program: String, args: Vec<String>) -> Result<__gos_process_Child, errors::Error> {
    __gos_process_Command::new(program)
        .args(args)
        .stdin(__gos_process_Stdio::Piped)
        .stdout(__gos_process_Stdio::Piped)
        .stderr(__gos_process_Stdio::Null)
        .spawn()
}

#[cfg(not(target_family = "wasm"))]
unsafe extern "C" {
    fn gos_rt_command_spawn(spec: [u8], len: u64, stdio: [i64], flags: i64, ctty: i64) -> i64
    fn gos_rt_command_error(code: i64, dst: &mut [u8], cap: u64) -> u64
    fn gos_rt_command_pid(handle: i64) -> i64
    fn gos_rt_command_write(handle: i64, src: [u8], len: u64) -> i64
    fn gos_rt_command_close(handle: i64, stream: i64)
    fn gos_rt_command_read(handle: i64, stream: i64, mode: i64, max: i64) -> i64
    fn gos_rt_command_take(handle: i64, stream: i64, dst: &mut [u8], cap: u64) -> u64
    fn gos_rt_command_pending(handle: i64, stream: i64) -> u64
    fn gos_rt_command_wait(handle: i64, timeout_ms: i64, code: &mut i64) -> i64
    fn gos_rt_command_kill(handle: i64, signal: i64, group: i64) -> i64
    fn gos_rt_command_collect(handle: i64, code: &mut i64) -> i64
}
"#;

/// Spellings that reach [`PROCESS_COMMAND_WRAPPERS`].
const PROCESS_COMMAND_MARKERS: &[&str] = &[
    "process::Command",
    "process::Stdio",
    "process::Child",
    "process::spawn_piped",
    "exec::Command",
    "exec::Stdio",
    "exec::Child",
    "exec::spawn_piped",
];

/// Spellings that reach the `process` wrappers: the `process` module and
/// its older `exec` / `os::exec` names.
const PROCESS_MARKERS: &[&str] = &[
    "process::run",
    "process::pipeline_run",
    "process::Output",
    "exec::run",
    "exec::pipeline_run",
    "exec::Output",
];

/// `http::Http2Config` as a real Gossamer struct, so the tuning fields
/// read the same words on every tier instead of leaving the compiled
/// tiers with an opaque type they cannot construct. The defaults must
/// match `gossamer_std::http_h2::Config::default()`, which
/// `http2_config_defaults_match_the_runtime` pins.
const HTTP2_CONFIG_WRAPPERS: &str = r"
struct __gos_http_Http2Config { max_concurrent_streams: i64, initial_window_size: i64, initial_connection_window_size: i64, max_frame_size: i64, max_header_list_size: i64 }
fn __gos_http_Http2Config_default() -> __gos_http_Http2Config {
    __gos_http_Http2Config { max_concurrent_streams: 100, initial_window_size: 1048576, initial_connection_window_size: 8388608, max_frame_size: 16384, max_header_list_size: 16384 }
}
";

/// Immutable UTF-8 path value implemented in ordinary Gossamer so its
/// representation and behavior are identical in VM, JIT, and AOT execution.
const PATH_WRAPPERS: &str = r"
struct __gos_path_Path { value: String }
impl __gos_path_Path {
    fn new(value: String) -> __gos_path_Path { __gos_path_Path { value: value } }
    fn as_str(&self) -> String { self.value }
    fn join(&self, segment: String) -> __gos_path_Path { __gos_path_Path { value: path::join(self.value, segment) } }
    fn parent(&self) -> Option<__gos_path_Path> {
        match path::parent(self.value) { Some(value) => Some(__gos_path_Path { value: value }), None => None }
    }
    fn file_name(&self) -> Option<String> { path::file_name(self.value) }
    fn stem(&self) -> Option<String> { path::file_stem(self.value) }
    fn extension(&self) -> Option<String> { path::extension(self.value) }
    fn normalize(&self) -> __gos_path_Path { __gos_path_Path { value: path::normalize(self.value) } }
    fn is_absolute(&self) -> bool { path::is_absolute(self.value) }
    fn starts_with(&self, prefix: __gos_path_Path) -> bool { path::starts_with(self.value, prefix.value) }
}
";

/// Real-struct + wrapper source for `std::archive::tar` (read).
/// `write` lowers directly (no struct).
const TAR_WRAPPERS: &str = r"
struct __gos_tar_TarEntry { name: String, data: Vec<u8>, is_dir: bool }
fn __gos_tar_read(data: Vec<u8>) -> Result<Vec<__gos_tar_TarEntry>, errors::Error> {
    let raws = __gos_tar_read_raw(data)?
    let mut out: Vec<__gos_tar_TarEntry> = Vec::from([])
    for r in raws {
        out.push(__gos_tar_TarEntry { name: r.0, data: r.1, is_dir: r.2 })
    }
    Ok(out)
}
";

/// Real-struct + wrapper source for `std::archive::zip` (read).
const ZIP_WRAPPERS: &str = r"
struct __gos_zip_ZipEntry { name: String, data: Vec<u8>, is_dir: bool }
fn __gos_zip_read(data: Vec<u8>) -> Result<Vec<__gos_zip_ZipEntry>, errors::Error> {
    let raws = __gos_zip_read_raw(data)?
    let mut out: Vec<__gos_zip_ZipEntry> = Vec::from([])
    for r in raws {
        out.push(__gos_zip_ZipEntry { name: r.0, data: r.1, is_dir: r.2 })
    }
    Ok(out)
}
";

/// Real-struct + wrapper source for `std::database::sql`. `Conn` /
/// `Rows` / `Row` / `Tx` are real Gossamer structs holding an opaque
/// `i64` handle; methods call scalar-shaped `__gos_sql_*_raw` leaf
/// intrinsics (sentinel error convention, message via
/// `__gos_sql_last_error_raw`), so the same code runs on every tier.
const SQL_WRAPPERS: &str = r#"
enum __gos_sql_Value { Null, Bool(bool), Int(i64), Float(f64), Text(String), Blob(Vec<u8>) }
enum __gos_sql_IsolationLevel { Default, ReadUncommitted, ReadCommitted, RepeatableRead, Serializable }
struct __gos_sql_Conn { __handle: i64 }
struct __gos_sql_Rows { __handle: i64 }
struct __gos_sql_Row { __handle: i64 }
struct __gos_sql_Tx { __handle: i64 }
struct __gos_sql_Stmt { __handle: i64 }
struct __gos_sql_Pool { __handle: i64 }
struct __gos_sql_Notification { channel: String, payload: String, process_id: i64 }
struct __gos_sql_Select { table: String, cols: Vec<String>, wheres: Vec<String>, binds: Vec<__gos_sql_Value>, order: String, lim: i64, off: i64 }
fn __gos_sql_err() -> errors::Error {
    errors::new(__gos_sql_last_error_raw())
}
fn __gos_sql_row_guard(k: i64) -> Result<(), errors::Error> {
    if k == -2 { return Err(errors::new("sql: row is no longer valid (cursor advanced or rows closed)")) }
    Ok(())
}
fn __gos_sql_open(name: String, url: String) -> Result<__gos_sql_Conn, errors::Error> {
    let h = __gos_sql_open_raw(name, url)
    if h < 0 { return Err(__gos_sql_err()) }
    Ok(__gos_sql_Conn { __handle: h })
}
fn __gos_sql_drivers() -> Vec<String> {
    let joined = __gos_sql_drivers_raw()
    if joined == "" { return Vec::from([]) }
    joined.split(",")
}
fn __gos_sql_bind(params: [__gos_sql_Value]) -> i64 {
    let p = __gos_sql_params_new_raw()
    for v in params {
        match v {
            __gos_sql_Value::Null => __gos_sql_params_push_null_raw(p),
            __gos_sql_Value::Bool(b) => __gos_sql_params_push_bool_raw(p, if b { 1 } else { 0 }),
            __gos_sql_Value::Int(n) => __gos_sql_params_push_int_raw(p, n),
            __gos_sql_Value::Float(f) => __gos_sql_params_push_float_raw(p, f),
            __gos_sql_Value::Text(s) => __gos_sql_params_push_text_raw(p, s),
            __gos_sql_Value::Blob(b) => __gos_sql_params_push_blob_raw(p, b),
        }
    }
    p
}
impl __gos_sql_Conn {
    fn execute(&mut self, sql: String, params: [__gos_sql_Value]) -> Result<i64, errors::Error> {
        let n = __gos_sql_conn_execute_raw(self.__handle, sql, __gos_sql_bind(params))
        if n < 0 { return Err(__gos_sql_err()) }
        Ok(n)
    }
    fn query(&mut self, sql: String, params: [__gos_sql_Value]) -> Result<__gos_sql_Rows, errors::Error> {
        let h = __gos_sql_conn_query_raw(self.__handle, sql, __gos_sql_bind(params))
        if h < 0 { return Err(__gos_sql_err()) }
        Ok(__gos_sql_Rows { __handle: h })
    }
    fn query_each(&mut self, sql: String, params: [__gos_sql_Value], f: Fn(__gos_sql_Row)) -> Result<(), errors::Error> {
        let h = __gos_sql_conn_query_raw(self.__handle, sql, __gos_sql_bind(params))
        if h < 0 { return Err(__gos_sql_err()) }
        let mut rows = __gos_sql_Rows { __handle: h }
        defer rows.close()
        loop {
            let next = rows.next_row()?
            let Some(row) = next else { break }
            f(row)
        }
        Ok(())
    }
    fn begin(&mut self) -> Result<__gos_sql_Tx, errors::Error> {
        let h = __gos_sql_conn_begin_raw(self.__handle)
        if h < 0 { return Err(__gos_sql_err()) }
        Ok(__gos_sql_Tx { __handle: h })
    }
    fn begin_with(&mut self, iso: __gos_sql_IsolationLevel) -> Result<__gos_sql_Tx, errors::Error> {
        let code = match iso {
            __gos_sql_IsolationLevel::Default => 0,
            __gos_sql_IsolationLevel::ReadUncommitted => 1,
            __gos_sql_IsolationLevel::ReadCommitted => 2,
            __gos_sql_IsolationLevel::RepeatableRead => 3,
            __gos_sql_IsolationLevel::Serializable => 4,
        }
        let h = __gos_sql_conn_begin_with_raw(self.__handle, code)
        if h < 0 { return Err(__gos_sql_err()) }
        Ok(__gos_sql_Tx { __handle: h })
    }
    fn ping(&mut self) -> Result<(), errors::Error> {
        if __gos_sql_conn_ping_raw(self.__handle) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
    fn set_busy_timeout(&mut self, ms: i64) -> Result<(), errors::Error> {
        if __gos_sql_conn_set_busy_timeout_raw(self.__handle, ms) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
    fn interrupt(&self) {
        let _ = __gos_sql_conn_interrupt_raw(self.__handle)
    }
    fn prepare(&mut self, sql: String) -> Result<__gos_sql_Stmt, errors::Error> {
        let h = __gos_sql_conn_prepare_raw(self.__handle, sql)
        if h < 0 { return Err(__gos_sql_err()) }
        Ok(__gos_sql_Stmt { __handle: h })
    }
    fn copy_in(&mut self, sql: String, data: [u8]) -> Result<i64, errors::Error> {
        let n = __gos_sql_conn_copy_in_raw(self.__handle, sql, data)
        if n < 0 { return Err(__gos_sql_err()) }
        Ok(n)
    }
    fn copy_out(&mut self, sql: String) -> Result<Vec<u8>, errors::Error> {
        if __gos_sql_conn_copy_out_run_raw(self.__handle, sql) < 0 { return Err(__gos_sql_err()) }
        Ok(__gos_sql_conn_copy_out_take_raw(self.__handle))
    }
    fn listen(&mut self, channel: String) -> Result<(), errors::Error> {
        if __gos_sql_conn_listen_raw(self.__handle, channel) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
    fn unlisten(&mut self, channel: String) -> Result<(), errors::Error> {
        if __gos_sql_conn_unlisten_raw(self.__handle, channel) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
    fn poll_notification(&mut self, timeout_ms: i64) -> Result<Option<__gos_sql_Notification>, errors::Error> {
        let s = __gos_sql_conn_poll_notification_raw(self.__handle, timeout_ms)
        if s < 0 { return Err(__gos_sql_err()) }
        if s == 0 { return Ok(None) }
        Ok(Some(__gos_sql_Notification {
            channel: __gos_sql_notification_channel_raw(self.__handle),
            payload: __gos_sql_notification_payload_raw(self.__handle),
            process_id: __gos_sql_notification_pid_raw(self.__handle),
        }))
    }
    fn close(&mut self) -> Result<(), errors::Error> {
        if __gos_sql_conn_close_raw(self.__handle) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
}
impl __gos_sql_Stmt {
    fn execute(&mut self, params: [__gos_sql_Value]) -> Result<i64, errors::Error> {
        let n = __gos_sql_stmt_execute_raw(self.__handle, __gos_sql_bind(params))
        if n < 0 { return Err(__gos_sql_err()) }
        Ok(n)
    }
    fn query(&mut self, params: [__gos_sql_Value]) -> Result<__gos_sql_Rows, errors::Error> {
        let h = __gos_sql_stmt_query_raw(self.__handle, __gos_sql_bind(params))
        if h < 0 { return Err(__gos_sql_err()) }
        Ok(__gos_sql_Rows { __handle: h })
    }
    fn close(&mut self) {
        let _ = __gos_sql_stmt_close_raw(self.__handle)
    }
}
impl __gos_sql_Pool {
    fn acquire(&self) -> Result<__gos_sql_Conn, errors::Error> {
        let h = __gos_sql_pool_get_raw(self.__handle)
        if h < 0 { return Err(__gos_sql_err()) }
        Ok(__gos_sql_Conn { __handle: h })
    }
    fn live(&self) -> i64 {
        __gos_sql_pool_live_raw(self.__handle)
    }
    fn idle(&self) -> i64 {
        __gos_sql_pool_idle_raw(self.__handle)
    }
    fn close_idle(&self) {
        let _ = __gos_sql_pool_close_idle_raw(self.__handle)
    }
}
fn __gos_sql_pool_open(driver: String, url: String, max: i64) -> Result<__gos_sql_Pool, errors::Error> {
    __gos_sql_pool_open_with(driver, url, 0, max, 30000, 300000, 1800000)
}
fn __gos_sql_pool_open_with(driver: String, url: String, min: i64, max: i64, acquire_ms: i64, idle_ms: i64, lifetime_ms: i64) -> Result<__gos_sql_Pool, errors::Error> {
    let h = __gos_sql_pool_new_raw(driver, url, min, max, acquire_ms, idle_ms, lifetime_ms)
    if h < 0 { return Err(__gos_sql_err()) }
    Ok(__gos_sql_Pool { __handle: h })
}
fn __gos_sql_migrate_up(db: &mut __gos_sql_Conn, dir: String) -> Result<i64, errors::Error> {
    let n = __gos_sql_migrate_up_raw(db.__handle, dir)
    if n < 0 { return Err(__gos_sql_err()) }
    Ok(n)
}
fn __gos_sql_join(parts: [String], sep: String) -> String {
    let mut out = ""
    let mut first = true
    for p in parts {
        if first {
            out = format("{}", p)
            first = false
        } else {
            out = format("{}{}{}", out, sep, p)
        }
    }
    out
}
fn __gos_sql_select_new(table: String) -> __gos_sql_Select {
    __gos_sql_Select { table: table.clone(), cols: Vec::from([]), wheres: Vec::from([]), binds: Vec::from([]), order: "", lim: -1, off: -1 }
}
fn __gos_sql_copy_strs(xs: [String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::from([])
    for x in xs { out.push(x) }
    out
}
fn __gos_sql_copy_vals(xs: [__gos_sql_Value]) -> Vec<__gos_sql_Value> {
    let mut out: Vec<__gos_sql_Value> = Vec::from([])
    for x in xs { out.push(x) }
    out
}
fn __gos_sql_is_simple_ident(s: String) -> bool {
    let n = s.len()
    if n == 0 { return false }
    let mut i = 0
    let mut dots = 0
    let mut start = true
    while i < n {
        let b = s.byte_at(i)
        if b == 46 {
            if start { return false }
            dots += 1
            if dots > 1 { return false }
            start = true
            i += 1
            continue
        }
        let alpha = (b >= 65 && b <= 90) || (b >= 97 && b <= 122) || b == 95
        if start {
            if !alpha { return false }
            start = false
        } else {
            if !(alpha || (b >= 48 && b <= 57)) { return false }
        }
        i += 1
    }
    if start { return false }
    true
}
fn __gos_sql_quote_ident(ident: String) -> String {
    if __gos_sql_is_simple_ident(ident) {
        return format("{}", ident)
    }
    format("\"{}\"", ident.replace("\"", "\"\""))
}
fn __gos_sql_quote_idents(xs: [String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::from([])
    for x in xs { out.push(__gos_sql_quote_ident(x)) }
    out
}
impl __gos_sql_Select {
    fn columns(&self, cols: [String]) -> __gos_sql_Select {
        let mut c = __gos_sql_copy_strs(self.cols)
        for x in cols { c.push(x) }
        __gos_sql_Select { table: self.table, cols: c, wheres: __gos_sql_copy_strs(self.wheres), binds: __gos_sql_copy_vals(self.binds), order: self.order, lim: self.lim, off: self.off }
    }
    fn where_eq(&self, column: String, v: __gos_sql_Value) -> __gos_sql_Select {
        let mut b = __gos_sql_copy_vals(self.binds)
        b.push(v)
        let mut w = __gos_sql_copy_strs(self.wheres)
        w.push(format("{} = ${}", __gos_sql_quote_ident(column), b.len()))
        __gos_sql_Select { table: self.table, cols: __gos_sql_copy_strs(self.cols), wheres: w, binds: b, order: self.order, lim: self.lim, off: self.off }
    }
    fn order_by(&self, column: String, ascending: bool) -> __gos_sql_Select {
        let dir = if ascending { "ASC" } else { "DESC" }
        __gos_sql_Select { table: self.table, cols: __gos_sql_copy_strs(self.cols), wheres: __gos_sql_copy_strs(self.wheres), binds: __gos_sql_copy_vals(self.binds), order: format("{} {}", __gos_sql_quote_ident(column), dir), lim: self.lim, off: self.off }
    }
    fn limit(&self, n: i64) -> __gos_sql_Select {
        __gos_sql_Select { table: self.table, cols: __gos_sql_copy_strs(self.cols), wheres: __gos_sql_copy_strs(self.wheres), binds: __gos_sql_copy_vals(self.binds), order: self.order, lim: n, off: self.off }
    }
    fn offset(&self, n: i64) -> __gos_sql_Select {
        __gos_sql_Select { table: self.table, cols: __gos_sql_copy_strs(self.cols), wheres: __gos_sql_copy_strs(self.wheres), binds: __gos_sql_copy_vals(self.binds), order: self.order, lim: self.lim, off: n }
    }
    fn params(&self) -> Vec<__gos_sql_Value> {
        __gos_sql_copy_vals(self.binds)
    }
    fn render(&self) -> String {
        let cols = if self.cols.len() == 0 { "*" } else { __gos_sql_join(__gos_sql_quote_idents(self.cols), ", ") }
        let mut out = format("SELECT {} FROM {}", cols, __gos_sql_quote_ident(self.table))
        if self.wheres.len() > 0 {
            out = format("{} WHERE {}", out, __gos_sql_join(self.wheres, " AND "))
        }
        if self.order != "" {
            out = format("{} ORDER BY {}", out, self.order)
        }
        if self.lim >= 0 {
            out = format("{} LIMIT {}", out, self.lim)
        }
        if self.off >= 0 {
            out = format("{} OFFSET {}", out, self.off)
        }
        out
    }
}
impl __gos_sql_Rows {
    fn next_row(&mut self) -> Result<Option<__gos_sql_Row>, errors::Error> {
        let h = __gos_sql_rows_next_row_raw(self.__handle)
        if h < 0 { return Err(__gos_sql_err()) }
        if h == 0 { return Ok(None) }
        Ok(Some(__gos_sql_Row { __handle: h }))
    }
    fn close(&mut self) -> Result<(), errors::Error> {
        if __gos_sql_rows_close_raw(self.__handle) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
    fn columns(&self) -> Vec<String> {
        let joined = __gos_sql_rows_columns_raw(self.__handle)
        if joined == "" { return Vec::from([]) }
        joined.split(",")
    }
}
impl __gos_sql_Row {
    fn get_i64(&self, column: String) -> Result<i64, errors::Error> {
        let k = __gos_sql_row_kind_raw(self.__handle, column)
        __gos_sql_row_guard(k)?
        if k != 2 {
            return Err(errors::newf("sql: column {} is not Int", column))
        }
        Ok(__gos_sql_row_get_i64_raw(self.__handle, column))
    }
    fn get_f64(&self, column: String) -> Result<f64, errors::Error> {
        let k = __gos_sql_row_kind_raw(self.__handle, column)
        __gos_sql_row_guard(k)?
        if k != 3 && k != 2 {
            return Err(errors::newf("sql: column {} is not Float", column))
        }
        Ok(__gos_sql_row_get_f64_raw(self.__handle, column))
    }
    fn get_bool(&self, column: String) -> Result<bool, errors::Error> {
        let k = __gos_sql_row_kind_raw(self.__handle, column)
        __gos_sql_row_guard(k)?
        if k != 1 {
            return Err(errors::newf("sql: column {} is not Bool", column))
        }
        Ok(__gos_sql_row_get_bool_raw(self.__handle, column) != 0)
    }
    fn get_text(&self, column: String) -> Result<String, errors::Error> {
        let k = __gos_sql_row_kind_raw(self.__handle, column)
        __gos_sql_row_guard(k)?
        if k != 4 {
            return Err(errors::newf("sql: column {} is not Text", column))
        }
        Ok(__gos_sql_row_get_text_raw(self.__handle, column))
    }
    fn get_blob(&self, column: String) -> Result<Vec<u8>, errors::Error> {
        let k = __gos_sql_row_kind_raw(self.__handle, column)
        __gos_sql_row_guard(k)?
        if k != 5 {
            return Err(errors::newf("sql: column {} is not Blob", column))
        }
        Ok(__gos_sql_row_get_blob_raw(self.__handle, column))
    }
    fn get_opt_i64(&self, column: String) -> Result<Option<i64>, errors::Error> {
        let k = __gos_sql_row_kind_raw(self.__handle, column)
        __gos_sql_row_guard(k)?
        if k == 0 { return Ok(None) }
        if k != 2 { return Err(errors::newf("sql: column {} is not Int", column)) }
        Ok(Some(__gos_sql_row_get_i64_raw(self.__handle, column)))
    }
    fn get_opt_f64(&self, column: String) -> Result<Option<f64>, errors::Error> {
        let k = __gos_sql_row_kind_raw(self.__handle, column)
        __gos_sql_row_guard(k)?
        if k == 0 { return Ok(None) }
        if k != 3 && k != 2 { return Err(errors::newf("sql: column {} is not Float", column)) }
        Ok(Some(__gos_sql_row_get_f64_raw(self.__handle, column)))
    }
    fn get_opt_bool(&self, column: String) -> Result<Option<bool>, errors::Error> {
        let k = __gos_sql_row_kind_raw(self.__handle, column)
        __gos_sql_row_guard(k)?
        if k == 0 { return Ok(None) }
        if k != 1 { return Err(errors::newf("sql: column {} is not Bool", column)) }
        Ok(Some(__gos_sql_row_get_bool_raw(self.__handle, column) != 0))
    }
    fn get_opt_text(&self, column: String) -> Result<Option<String>, errors::Error> {
        let k = __gos_sql_row_kind_raw(self.__handle, column)
        __gos_sql_row_guard(k)?
        if k == 0 { return Ok(None) }
        if k != 4 { return Err(errors::newf("sql: column {} is not Text", column)) }
        Ok(Some(__gos_sql_row_get_text_raw(self.__handle, column)))
    }
    fn is_null(&self, column: String) -> bool {
        __gos_sql_row_kind_raw(self.__handle, column) == 0
    }
    fn width(&self) -> i64 {
        __gos_sql_row_width_raw(self.__handle)
    }
}
impl __gos_sql_Tx {
    fn commit(&mut self) -> Result<(), errors::Error> {
        if __gos_sql_tx_commit_raw(self.__handle) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
    fn rollback(&mut self) -> Result<(), errors::Error> {
        if __gos_sql_tx_rollback_raw(self.__handle) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
    fn execute(&mut self, sql: String) -> Result<i64, errors::Error> {
        let n = __gos_sql_tx_execute_raw(self.__handle, sql)
        if n < 0 { return Err(__gos_sql_err()) }
        Ok(n)
    }
    fn execute_params(&mut self, sql: String, params: [__gos_sql_Value]) -> Result<i64, errors::Error> {
        let n = __gos_sql_tx_execute_params_raw(self.__handle, sql, __gos_sql_bind(params))
        if n < 0 { return Err(__gos_sql_err()) }
        Ok(n)
    }
    fn query(&mut self, sql: String, params: [__gos_sql_Value]) -> Result<__gos_sql_Rows, errors::Error> {
        let h = __gos_sql_tx_query_params_raw(self.__handle, sql, __gos_sql_bind(params))
        if h < 0 { return Err(__gos_sql_err()) }
        Ok(__gos_sql_Rows { __handle: h })
    }
    fn savepoint(&mut self, name: String) -> Result<(), errors::Error> {
        if __gos_sql_tx_savepoint_raw(self.__handle, name) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
    fn release_savepoint(&mut self, name: String) -> Result<(), errors::Error> {
        if __gos_sql_tx_release_savepoint_raw(self.__handle, name) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
    fn rollback_to_savepoint(&mut self, name: String) -> Result<(), errors::Error> {
        if __gos_sql_tx_rollback_to_savepoint_raw(self.__handle, name) < 0 { return Err(__gos_sql_err()) }
        Ok(())
    }
}
"#;

/// Real-struct + wrapper source for the request/response-integrated
/// `std::http::{csrf, session, form, multipart}` surface. Pure
/// composition over the already-wired csrf / session / cookie / aead /
/// hmac / hex / url primitives, so it lowers natively on every tier.
const HTTP_SECURITY_WRAPPERS: &str = r##"
// ---- shared helpers ----
fn __gos_http_header_lookup(headers: [(String, String)], name: String) -> String {
    let target = name.to_lowercase()
    let mut found = ""
    for (k, v) in headers {
        if k.to_lowercase() == target { found = *v }
    }
    found
}
fn __gos_http_bytes_to_str(b: [u8]) -> String {
    let mut buf = bytes::Buffer::new()
    for x in b { buf.push(x) }
    buf.to_string()
}
fn __gos_http_first12(b: [u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::from([])
    let mut i = 0
    while i < 12 {
        out.push(b[i])
        i += 1
    }
    out
}
fn __gos_http_trim_slash(s: String) -> String {
    let n = s.len()
    if n > 0 && s.ends_with("/") { s.substring(0, n - 1) } else { s.substring(0, n) }
}
fn __gos_http_origin_host(origin: String) -> String {
    let mut host: String = match origin.split_once("://") {
        Some((_, r)) => r,
        None => origin.substring(0, origin.len()),
    }
    match host.split_once("/") { Some((h, _)) => host = h, None => {} }
    match host.split_once("?") { Some((h, _)) => host = h, None => {} }
    match host.split_once("#") { Some((h, _)) => host = h, None => {} }
    host
}
fn __gos_http_origin_from_referer(referer: String) -> String {
    match referer.split_once("://") {
        Some((scheme, _)) => scheme + "://" + &__gos_http_origin_host(referer),
        None => "",
    }
}
fn __gos_http_origins_equal(a: String, b: String) -> bool {
    __gos_http_trim_slash(a).to_lowercase() == __gos_http_trim_slash(b).to_lowercase()
}

// ---- csrf (request/response integrated) ----
struct __gos_http_csrf_Config {
    cookie_name: String,
    header_name: String,
    form_field: String,
    key: Vec<u8>,
    trusted_origins: Vec<String>,
    secure: bool,
    same_site: String,
    max_age_secs: i64,
    safe_methods: Vec<String>,
    exempt_prefixes: Vec<String>,
}
enum __gos_http_csrf_RouteAuth { BearerOnly, CookieSession, None }
fn __gos_http_csrf_config(key: Vec<u8>) -> __gos_http_csrf_Config {
    __gos_http_csrf_Config {
        cookie_name: "gos_csrf",
        header_name: "X-CSRF-Token",
        form_field: "_csrf",
        key: key,
        trusted_origins: Vec::from([]),
        secure: true,
        same_site: "Lax",
        max_age_secs: 86400,
        safe_methods: Vec::from(["GET", "HEAD", "OPTIONS", "TRACE"]),
        exempt_prefixes: Vec::from([]),
    }
}
fn __gos_http_csrf_is_safe(config: __gos_http_csrf_Config, method: String) -> bool {
    let m = method.to_lowercase()
    let mut safe = false
    for s in config.safe_methods {
        if s.to_lowercase() == m { safe = true }
    }
    safe
}
fn __gos_http_csrf_extract_token(r: http::Request, config: __gos_http_csrf_Config) -> Option<String> {
    let h = __gos_http_header_lookup(r.headers, config.header_name)
    if h != "" { return Some(h) }
    let ct = __gos_http_header_lookup(r.headers, "content-type")
    if ct.to_lowercase().starts_with("application/x-www-form-urlencoded") {
        let f = r.form_value(config.form_field)
        if f != "" { return Some(f) }
    }
    None
}
fn __gos_http_csrf_origin_allowed(r: http::Request, config: __gos_http_csrf_Config) -> bool {
    let method = r.method()
    let is_safe = __gos_http_csrf_is_safe(config, method)
    let origin = __gos_http_header_lookup(r.headers, "origin")
    let referer = __gos_http_header_lookup(r.headers, "referer")
    let mut candidate = ""
    if origin != "" {
        candidate = origin
    } else if referer != "" {
        let o = __gos_http_origin_from_referer(referer)
        if o == "" { return is_safe }
        candidate = o
    } else {
        return is_safe
    }
    if config.trusted_origins.len() > 0 {
        let mut ok = false
        for t in config.trusted_origins {
            if __gos_http_origins_equal(t, candidate) { ok = true }
        }
        return ok
    }
    let host = __gos_http_header_lookup(r.headers, "host")
    if host == "" { return false }
    __gos_http_origin_host(candidate).to_lowercase() == host.to_lowercase()
}
fn __gos_http_csrf_check(r: http::Request, route_auth: __gos_http_csrf_RouteAuth, config: __gos_http_csrf_Config) -> Result<(), errors::Error> {
    match route_auth {
        __gos_http_csrf_RouteAuth::BearerOnly => return Ok(()),
        _ => {}
    }
    let method = r.method()
    if __gos_http_csrf_is_safe(config, method) { return Ok(()) }
    if config.exempt_prefixes.len() > 0 {
        let path = r.path()
        for p in config.exempt_prefixes {
            if path.starts_with(p) { return Ok(()) }
        }
    }
    if !__gos_http_csrf_origin_allowed(r, config) {
        return Err(errors::new("csrf: origin not allowed"))
    }
    let cookie_header = __gos_http_header_lookup(r.headers, "cookie")
    if cookie_header == "" { return Err(errors::new("csrf: missing cookie header")) }
    let pairs = http::cookie::parse_cookie_header(cookie_header)
    let mut cookie_token = ""
    for (k, v) in pairs {
        if k == config.cookie_name { cookie_token = v }
    }
    if cookie_token == "" { return Err(errors::new("csrf: missing csrf cookie")) }
    let supplied = match __gos_http_csrf_extract_token(r, config) {
        Some(t) => t,
        None => return Err(errors::new("csrf: missing csrf token")),
    }
    http::csrf::verify_token(cookie_token, supplied, config.key)
}
// A function that returns an `http::Response` must stay strictly
// straight-line: a branch (`if` / `match`) between the handle param and
// the `with_header` that mutates it loses the mutation on the compiled
// tiers, so every conditional that shapes the header string lives in a
// pure `String` helper and the response builder only concatenates calls.
fn __gos_http_max_age_attr(max_age_secs: i64) -> String {
    if max_age_secs > 0 { "; Max-Age=" + &format("{}", max_age_secs) } else { "" }
}
fn __gos_http_secure_attr(secure: bool) -> String {
    if secure { "; Secure" } else { "" }
}
fn __gos_http_csrf_cookie_value(token: String, config: __gos_http_csrf_Config) -> String {
    let bare = http::cookie::serialize(config.cookie_name, token)
    bare + "; Path=/" + &__gos_http_max_age_attr(config.max_age_secs)
        + &__gos_http_secure_attr(config.secure) + "; SameSite=" + &config.same_site
}
fn __gos_http_csrf_attach_cookie(resp: http::Response, token: String, config: __gos_http_csrf_Config) -> http::Response {
    let sc = __gos_http_csrf_cookie_value(token, config)
    resp.with_header("set-cookie", sc)
}

// ---- session (signed + AES-256-GCM encrypted store) ----
struct __gos_http_session_Store {
    key: Vec<u8>,
    cookie_name: String,
    encrypted: bool,
    secure: bool,
    max_age_secs: i64,
}
fn __gos_http_session_signed(key: Vec<u8>) -> __gos_http_session_Store {
    __gos_http_session_Store { key: key, cookie_name: "gos_session", encrypted: false, secure: true, max_age_secs: 86400 }
}
fn __gos_http_session_encrypted(key: Vec<u8>) -> __gos_http_session_Store {
    __gos_http_session_Store { key: key, cookie_name: "gos_session", encrypted: true, secure: true, max_age_secs: 86400 }
}
fn __gos_http_session_seal(key: [u8], data: String) -> Result<String, errors::Error> {
    let pt = data.as_bytes()
    let mac = crypto::hmac::sha256_mac(key.to_vec(), pt)
    let nonce = __gos_http_first12(mac)
    let empty: Vec<u8> = Vec::from([])
    let ct = crypto::aead::aes_256_gcm_seal(key.to_vec(), nonce, pt, empty)?
    Ok(encoding::hex::encode(nonce) + "." + &encoding::hex::encode(ct))
}
fn __gos_http_session_open(key: [u8], cookie: String) -> Result<String, errors::Error> {
    let n, c = match cookie.split_once(".") {
        Some(p) => p,
        None => return Err(errors::new("session: bad framing")),
    }
    let nonce = encoding::hex::decode(n)?
    let ct = encoding::hex::decode(c)?
    let empty: Vec<u8> = Vec::from([])
    let pt = crypto::aead::aes_256_gcm_open(key.to_vec(), nonce, ct, empty)?
    Ok(__gos_http_bytes_to_str(pt))
}
fn __gos_http_session_encode(store: __gos_http_session_Store, data: String) -> String {
    if store.encrypted {
        match __gos_http_session_seal(store.key, data) {
            Ok(v) => v,
            Err(_) => "",
        }
    } else {
        http::session::sign(data, store.key)
    }
}
fn __gos_http_session_cookie_value(store: __gos_http_session_Store, data: String) -> String {
    let cookie_val = __gos_http_session_encode(store, data)
    let bare = http::cookie::serialize(store.cookie_name, cookie_val)
    bare + "; Path=/; HttpOnly" + &__gos_http_max_age_attr(store.max_age_secs)
        + &__gos_http_secure_attr(store.secure) + "; SameSite=Lax"
}
// load / save are free functions, not methods: a `&self` method that
// returns the 2-word `Result` while also taking an opaque-handle arg
// (`http::Request`) miscompiles the call on the LLVM tier, whereas the
// free-function form is sound - and `session::load(store, req)` /
// `session::save(store, resp, data)` is also the data-first spelling.
fn __gos_http_session_save(store: __gos_http_session_Store, resp: http::Response, data: String) -> http::Response {
    let sc = __gos_http_session_cookie_value(store, data)
    resp.with_header("set-cookie", sc)
}
fn __gos_http_session_cookie_raw(store: __gos_http_session_Store, r: http::Request) -> String {
    let cookie_header = __gos_http_header_lookup(r.headers, "cookie")
    let pairs = http::cookie::parse_cookie_header(cookie_header)
    let mut raw = ""
    for (k, v) in pairs {
        if k == store.cookie_name { raw = v }
    }
    raw
}
fn __gos_http_session_load(store: __gos_http_session_Store, r: http::Request) -> Result<String, errors::Error> {
    let raw = __gos_http_session_cookie_raw(store, r)
    if raw == "" { return Err(errors::new("session: cookie not present")) }
    if store.encrypted {
        __gos_http_session_open(store.key, raw)
    } else {
        http::session::verify(raw, store.key)
    }
}
fn __gos_http_session_load_or_empty(store: __gos_http_session_Store, r: http::Request) -> String {
    match __gos_http_session_load(store, r) {
        Ok(d) => d,
        Err(_) => "",
    }
}
fn __gos_http_session_with_session(store: __gos_http_session_Store, r: http::Request, resp: http::Response, f: Fn(String) -> String) -> http::Response {
    let current = __gos_http_session_load_or_empty(store, r)
    let updated = f(current)
    __gos_http_session_save(store, resp, updated)
}

// ---- form (application/x-www-form-urlencoded) ----
struct __gos_http_form_Form { pairs: Vec<(String, String)> }
fn __gos_http_form_parse(body: String) -> __gos_http_form_Form {
    let mut pairs: Vec<(String, String)> = Vec::from([])
    let raw_pairs: Vec<String> = strings::split(body, "&")
    for pair in raw_pairs {
        let p: String = pair
        if p == "" { continue }
        match p.split_once("=") {
            Some((k, v)) => pairs.push((url::query_unescape(k), url::query_unescape(v))),
            None => pairs.push((url::query_unescape(p), "")),
        }
    }
    __gos_http_form_Form { pairs: pairs }
}
fn __gos_http_form_get(form: __gos_http_form_Form, name: String) -> String {
    for (k, v) in form.pairs {
        if k == *name { return v }
    }
    ""
}
fn __gos_http_form_get_all(form: __gos_http_form_Form, name: String) -> Vec<String> {
    let mut out: Vec<String> = Vec::from([])
    for (k, v) in form.pairs {
        if k == *name { out.push(v) }
    }
    out
}
fn __gos_http_form_has(form: __gos_http_form_Form, name: String) -> bool {
    for (k, _v) in form.pairs {
        if k == *name { return true }
    }
    false
}
fn __gos_http_form_count(form: __gos_http_form_Form) -> i64 {
    form.pairs.len()
}

// ---- multipart (multipart/form-data, RFC 7578) ----
struct __gos_http_multipart_Part {
    name: String,
    filename: String,
    content_type: String,
    content: Vec<u8>,
}
fn __gos_http_multipart_boundary(content_type: String) -> String {
    match content_type.split_once("boundary=") {
        Some((_, rest)) => {
            let raw = match rest.split_once(";") {
                Some((b, _)) => b,
                None => rest,
            }
            raw.trim_matches("\"")
        },
        None => "",
    }
}
fn __gos_http_multipart_header_value(head: String, key: String) -> String {
    let target = key.to_lowercase()
    let lines: Vec<String> = strings::lines(head)
    for line in lines {
        let l: String = line
        match l.split_once(":") {
            Some((k, v)) => {
                if k.trim().to_lowercase() == target { return v.trim() }
            },
            None => {},
        }
    }
    ""
}
fn __gos_http_multipart_disp_param(disp: String, key: String) -> String {
    let needle = key.clone() + "=\""
    match disp.split_once(needle) {
        Some((_, rest)) => {
            match rest.split_once("\"") {
                Some((val, _)) => val,
                None => "",
            }
        },
        None => "",
    }
}
fn __gos_http_multipart_parse(body: [u8], boundary: String) -> Vec<__gos_http_multipart_Part> {
    let text = __gos_http_bytes_to_str(body)
    let delim = "--" + boundary
    let segments: Vec<String> = strings::split(text, delim)
    let mut parts: Vec<__gos_http_multipart_Part> = Vec::from([])
    for seg in segments {
        let s: String = seg
        let trimmed = s.trim()
        if trimmed == "" || trimmed == "--" { continue }
        match s.split_once("\r\n\r\n") {
            Some((head, rest)) => {
                let mut content_str: String = rest
                if content_str.ends_with("\r\n") {
                    content_str = content_str.substring(0, content_str.len() - 2)
                }
                let disp = __gos_http_multipart_header_value(head, "content-disposition")
                let name = __gos_http_multipart_disp_param(disp, "name")
                let filename = __gos_http_multipart_disp_param(disp, "filename")
                let ctype = __gos_http_multipart_header_value(head, "content-type")
                parts.push(__gos_http_multipart_Part {
                    name: name,
                    filename: filename,
                    content_type: ctype,
                    content: content_str.as_bytes(),
                })
            },
            None => {},
        }
    }
    parts
}
fn __gos_http_request_form_file(r: http::Request, name: String) -> Option<__gos_http_multipart_Part> {
    let ct = __gos_http_header_lookup(r.headers, "content-type")
    let boundary = __gos_http_multipart_boundary(ct)
    if boundary == "" { return None }
    let parts = __gos_http_multipart_parse(r.raw_body, boundary)
    for p in parts {
        if p.name == *name && p.filename != "" { return Some(p) }
    }
    None
}

"##;
