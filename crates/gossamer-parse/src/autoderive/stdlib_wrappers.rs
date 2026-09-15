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
    if (source.contains("fs::metadata") && from_std("fs")) || item_imported(FS_METADATA_WRAPPERS) {
        stdlib_wrappers.push_str(FS_METADATA_WRAPPERS);
    }
    if item_imported(FS_DIR_WRAPPERS)
        || (FS_DIR_MARKERS.iter().any(|m| source.contains(m))
            && (from_std("fs") || from_std("path")))
    {
        stdlib_wrappers.push_str(FS_DIR_WRAPPERS);
    }
    if item_imported(PROCESS_WRAPPERS)
        || (PROCESS_MARKERS.iter().any(|m| source.contains(m))
            && (from_std("process") || from_std("exec")))
    {
        stdlib_wrappers.push_str(PROCESS_WRAPPERS);
    }
    if (source.contains("path::Path") && from_std("path")) || item_imported(PATH_WRAPPERS) {
        stdlib_wrappers.push_str(PATH_WRAPPERS);
    }
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
    if item_imported(TIME_CIVIL_WRAPPERS)
        || (TIME_CIVIL_MARKERS.iter().any(|marker| source.contains(marker))
            && from_std("time"))
    {
        stdlib_wrappers.push_str(TIME_CIVIL_WRAPPERS);
    }
    stdlib_wrappers
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
