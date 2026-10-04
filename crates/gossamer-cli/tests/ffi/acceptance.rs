//! Real C libraries bound with nothing but `unsafe extern` declarations:
//! `SQLite` (handles, out-parameters, owned and borrowed text, row callbacks,
//! and a user-defined SQL function), zlib (caller-owned buffers and a
//! `z_stream` with pointer fields), and Expat (SAX callbacks with user
//! data).
//!
//! `SQLite` ships with every platform the toolchain targets (`winsqlite3` on
//! Windows). zlib and Expat ship with Linux and macOS and not with Windows,
//! so their tests run where the libraries are part of the system.

use crate::support::Project;

/// The `SQLite` declarations, for the library each platform ships.
fn sqlite_decls() -> String {
    let block = r#"unsafe extern "C" {
    type Sqlite3
    type Stmt
    type Context
    type Value

    fn sqlite3_open(path: [u8], db: &mut Option<Ptr<Sqlite3>>) -> ffi::c_int
    fn sqlite3_close(db: Ptr<Sqlite3>) -> ffi::c_int
    fn sqlite3_errmsg(db: Ptr<Sqlite3>) -> Ptr<u8>
    fn sqlite3_exec(
        db: Ptr<Sqlite3>,
        sql: [u8],
        row: Fn(Ptr<ffi::c_void>, ffi::c_int, Ptr<Ptr<u8>>, Ptr<Ptr<u8>>) -> ffi::c_int,
        context: Ptr<ffi::c_void>,
        error: &mut Option<Ptr<u8>>,
    ) -> ffi::c_int
    fn sqlite3_free(p: Ptr<u8>)
    fn sqlite3_prepare_v2(
        db: Ptr<Sqlite3>,
        sql: [u8],
        len: ffi::c_int,
        stmt: &mut Option<Ptr<Stmt>>,
        tail: &mut Option<Ptr<u8>>,
    ) -> ffi::c_int
    fn sqlite3_bind_int64(stmt: Ptr<Stmt>, index: ffi::c_int, value: i64) -> ffi::c_int
    fn sqlite3_bind_text(stmt: Ptr<Stmt>, index: ffi::c_int, text: [u8], len: ffi::c_int, destructor: isize) -> ffi::c_int
    fn sqlite3_step(stmt: Ptr<Stmt>) -> ffi::c_int
    fn sqlite3_column_int64(stmt: Ptr<Stmt>, column: ffi::c_int) -> i64
    fn sqlite3_column_text(stmt: Ptr<Stmt>, column: ffi::c_int) -> Option<Ptr<u8>>
    fn sqlite3_finalize(stmt: Ptr<Stmt>) -> ffi::c_int
    fn sqlite3_create_function(
        db: Ptr<Sqlite3>,
        name: [u8],
        args: ffi::c_int,
        encoding: ffi::c_int,
        app: Option<Ptr<ffi::c_void>>,
        call: Fn(Ptr<Context>, ffi::c_int, Ptr<Ptr<Value>>),
        step: Option<Ptr<ffi::c_void>>,
        last: Option<Ptr<ffi::c_void>>,
    ) -> ffi::c_int
    fn sqlite3_value_int64(value: Ptr<Value>) -> i64
    fn sqlite3_result_int64(context: Ptr<Context>, value: i64)
}"#;
    format!(
        "use std::ffi\nuse std::ffi::Ptr\n\n#[cfg(windows)]\n#[link(name = \"winsqlite3\")]\n{block}\n\n#[cfg(not(windows))]\n#[link(name = \"sqlite3\")]\n{block}\n"
    )
}

const SQLITE_PROGRAM: &str = r#"
/// SQLite copies text bound with this destructor before the call returns.
const TRANSIENT: isize = -1

fn on_row(context: Ptr<ffi::c_void>, columns: ffi::c_int, values: Ptr<Ptr<u8>>, names: Ptr<Ptr<u8>>) -> ffi::c_int {
    let rows = unsafe { ffi::Handle::<Vec<String>>::from_ptr(context) }
    let mut line = ""
    for i in 0..columns as i64 {
        let name = unsafe { ffi::read_cstr(ffi::read_at(names, i)) }.unwrap()
        let value: Ptr<u8> = unsafe { ffi::read_at(values, i) }
        let text = if value.address() == 0 { "NULL" } else { unsafe { ffi::read_cstr(value) }.unwrap() }
        line += f"{name}={text} "
    }
    rows.update(|all: &mut Vec<String>| all.push(line.trim()))
    0
}

fn twice(context: Ptr<Context>, count: ffi::c_int, values: Ptr<Ptr<Value>>) {
    let first: Ptr<Value> = unsafe { ffi::read(values) }
    unsafe { sqlite3_result_int64(context, sqlite3_value_int64(first) * 2) }
}

fn exec(db: Ptr<Sqlite3>, sql: String) {
    let mut error: Option<Ptr<u8>> = None
    let rows = ffi::Handle::new(Vec::<String>::new())
    let code = unsafe { sqlite3_exec(db, ffi::cstring(sql).unwrap(), on_row, rows.as_ptr(), &mut error) }
    for row in rows.take() {
        println(row)
    }
    if let Some(message) = error {
        println(f"error {code}: {unsafe { ffi::read_cstr(message) }.unwrap()}")
        unsafe { sqlite3_free(message) }
    }
}

fn main() {
    let mut opened: Option<Ptr<Sqlite3>> = None
    let status = unsafe { sqlite3_open(ffi::cstring(":memory:").unwrap(), &mut opened) }
    let db = opened.unwrap()
    println(f"open {status}")
    exec(db, "create table people (id integer primary key, name text, age integer)")

    let mut stmt: Option<Ptr<Stmt>> = None
    let mut tail: Option<Ptr<u8>> = None
    let insert = ffi::cstring("insert into people (name, age) values (?, ?)").unwrap()
    unsafe { sqlite3_prepare_v2(db, insert, -1, &mut stmt, &mut tail) }
    let insert_stmt = stmt.unwrap()
    for person in #[("ada", 36), ("grace", 45), ("alan", 41)] {
        let name = ffi::cstring(person.0).unwrap()
        unsafe { sqlite3_bind_text(insert_stmt, 1, name, -1, TRANSIENT) }
        unsafe { sqlite3_bind_int64(insert_stmt, 2, person.1) }
        unsafe { sqlite3_step(insert_stmt) }
        unsafe { sqlite3_reset_stmt(insert_stmt) }
    }
    unsafe { sqlite3_finalize(insert_stmt) }

    let query = ffi::cstring("select name, age from people where age > ? order by age").unwrap()
    let mut select: Option<Ptr<Stmt>> = None
    unsafe { sqlite3_prepare_v2(db, query, -1, &mut select, &mut tail) }
    let select_stmt = select.unwrap()
    unsafe { sqlite3_bind_int64(select_stmt, 1, 40) }
    while unsafe { sqlite3_step(select_stmt) } == 100 {
        let name = unsafe { ffi::read_cstr(sqlite3_column_text(select_stmt, 0).unwrap()) }.unwrap()
        let age = unsafe { sqlite3_column_int64(select_stmt, 1) }
        println(f"row {name} {age}")
    }
    unsafe { sqlite3_finalize(select_stmt) }

    let utf8: ffi::c_int = 1
    unsafe { sqlite3_create_function(db, ffi::cstring("twice").unwrap(), 1, utf8, None, twice, None, None) }
    exec(db, "select name, twice(age) as doubled from people order by id")
    exec(db, "select nonsense from nowhere")
    println(f"close {unsafe { sqlite3_close(db) }}")
}
"#;

/// `sqlite3_reset`, named apart from the program's own `reset` words.
const SQLITE_RESET: &str = r#"
#[cfg(windows)]
#[link(name = "winsqlite3")]
unsafe extern "C" {
    #[link_name = "sqlite3_reset"]
    fn sqlite3_reset_stmt(stmt: Ptr<Stmt>) -> ffi::c_int
}

#[cfg(not(windows))]
#[link(name = "sqlite3")]
unsafe extern "C" {
    #[link_name = "sqlite3_reset"]
    fn sqlite3_reset_stmt(stmt: Ptr<Stmt>) -> ffi::c_int
}
"#;

#[test]
fn sqlite_binds_with_handles_out_parameters_and_callbacks() {
    let source = format!("{}{SQLITE_RESET}{SQLITE_PROGRAM}", sqlite_decls());
    let project = Project::new("accept-sqlite", &source);
    project.expect_everywhere(
        "open 0\n\
         row alan 41\n\
         row grace 45\n\
         name=ada doubled=72\n\
         name=grace doubled=90\n\
         name=alan doubled=82\n\
         error 1: no such table: nowhere\n\
         close 0",
    );
}

#[cfg(unix)]
const ZLIB_PROGRAM: &str = r#"use std::ffi
use std::ffi::Ptr

#[repr(C)]
struct ZStream {
    next_in: Ptr<u8>,
    avail_in: u32,
    total_in: u64,
    next_out: Ptr<u8>,
    avail_out: u32,
    total_out: u64,
    msg: Ptr<u8>,
    state: Ptr<ffi::c_void>,
    zalloc: Ptr<ffi::c_void>,
    zfree: Ptr<ffi::c_void>,
    opaque: Ptr<ffi::c_void>,
    data_type: i32,
    adler: u64,
    reserved: u64,
}

#[link(name = "z")]
unsafe extern "C" {
    fn zlibVersion() -> Ptr<u8>
    fn compressBound(len: ffi::c_ulong) -> ffi::c_ulong
    fn compress2(dest: Ptr<u8>, dest_len: &mut ffi::c_ulong, source: [u8], source_len: ffi::c_ulong, level: ffi::c_int) -> ffi::c_int
    fn uncompress(dest: Ptr<u8>, dest_len: &mut ffi::c_ulong, source: Ptr<u8>, source_len: ffi::c_ulong) -> ffi::c_int
    fn deflateInit_(stream: Ptr<ZStream>, level: ffi::c_int, version: Ptr<u8>, size: ffi::c_int) -> ffi::c_int
    fn deflate(stream: Ptr<ZStream>, flush: ffi::c_int) -> ffi::c_int
    fn deflateEnd(stream: Ptr<ZStream>) -> ffi::c_int
    fn inflateInit_(stream: Ptr<ZStream>, version: Ptr<u8>, size: ffi::c_int) -> ffi::c_int
    fn inflate(stream: Ptr<ZStream>, flush: ffi::c_int) -> ffi::c_int
    fn inflateEnd(stream: Ptr<ZStream>) -> ffi::c_int
}

fn text() -> Vec<u8> {
    let mut out = ""
    for i in 0..200 {
        out += f"line {i % 7} of a repetitive text\n"
    }
    out.bytes()
}

/// Streams `input` through `deflate` (or `inflate`) in 64-byte output
/// chunks, through a `z_stream` that lives in C memory.
fn stream(input: Vec<u8>, compressing: bool) -> Vec<u8> {
    let z: Ptr<ZStream> = unsafe { ffi::alloc(1) }
    let size = ffi::size_of::<ZStream>() as ffi::c_int
    let version = unsafe { zlibVersion() }
    if compressing {
        unsafe { deflateInit_(z, 9, version, size) }
    } else {
        unsafe { inflateInit_(z, version, size) }
    }
    let source = unsafe { ffi::to_c_bytes(input) }
    let chunk: Ptr<u8> = unsafe { ffi::alloc(64) }
    let mut state: ZStream = unsafe { ffi::read(z) }
    state.next_in = source
    state.avail_in = input.len() as u32
    unsafe { ffi::write(z, state) }
    let mut out: Vec<u8> = #[]
    loop {
        let mut current: ZStream = unsafe { ffi::read(z) }
        current.next_out = chunk
        current.avail_out = 64
        unsafe { ffi::write(z, current) }
        let code = if compressing { unsafe { deflate(z, 4) } } else { unsafe { inflate(z, 0) } }
        let after: ZStream = unsafe { ffi::read(z) }
        let produced = 64 - after.avail_out as i64
        for byte in unsafe { ffi::read_bytes(chunk, produced) } {
            out.push(byte)
        }
        if code == 1 {
            break
        }
    }
    if compressing { unsafe { deflateEnd(z) } } else { unsafe { inflateEnd(z) } }
    unsafe { ffi::free(chunk) }
    unsafe { ffi::free(source) }
    unsafe { ffi::free(z) }
    out
}

fn main() {
    let input = text()
    let bound = unsafe { compressBound(input.len() as ffi::c_ulong) }
    let packed: Ptr<u8> = unsafe { ffi::alloc(bound as i64) }
    let mut packed_len = bound
    let status = unsafe { compress2(packed, &mut packed_len, input, input.len() as ffi::c_ulong, 9) }
    let restored: Ptr<u8> = unsafe { ffi::alloc(input.len()) }
    let mut restored_len = input.len() as ffi::c_ulong
    let back = unsafe { uncompress(restored, &mut restored_len, packed, packed_len) }
    let same = unsafe { ffi::read_bytes(restored, restored_len as i64) } == input
    println(f"one-shot {status} {back} smaller={packed_len < input.len() as ffi::c_ulong} same={same}")
    unsafe { ffi::free(packed) }
    unsafe { ffi::free(restored) }
    let compressed = stream(input, true)
    let expanded = stream(compressed, false)
    println(f"streamed smaller={compressed.len() < input.len()} same={expanded == input}")
}
"#;

#[cfg(unix)]
#[test]
fn zlib_compresses_through_caller_owned_buffers_and_a_stream() {
    let project = Project::new("accept-zlib", ZLIB_PROGRAM);
    project
        .expect_everywhere("one-shot 0 0 smaller=true same=true\nstreamed smaller=true same=true");
}

#[cfg(unix)]
const EXPAT_PROGRAM: &str = r#"use std::ffi
use std::ffi::Ptr

#[link(name = "expat")]
unsafe extern "C" {
    type Parser

    fn XML_ParserCreate(encoding: Option<Ptr<u8>>) -> Ptr<Parser>
    fn XML_SetUserData(parser: Ptr<Parser>, data: Ptr<ffi::c_void>)
    fn XML_SetElementHandler(
        parser: Ptr<Parser>,
        start: Fn(Ptr<ffi::c_void>, Ptr<u8>, Ptr<Ptr<u8>>),
        end: Fn(Ptr<ffi::c_void>, Ptr<u8>),
    )
    fn XML_SetCharacterDataHandler(parser: Ptr<Parser>, text: Fn(Ptr<ffi::c_void>, Ptr<u8>, ffi::c_int))
    fn XML_Parse(parser: Ptr<Parser>, data: [u8], len: ffi::c_int, last: ffi::c_int) -> ffi::c_int
    fn XML_ParserFree(parser: Ptr<Parser>)
}

struct Outline {
    depth: i64,
    lines: Vec<String>,
}

fn outline(data: Ptr<ffi::c_void>) -> ffi::Handle<Outline> {
    unsafe { ffi::Handle::<Outline>::from_ptr(data) }
}

fn on_start(data: Ptr<ffi::c_void>, name: Ptr<u8>, attributes: Ptr<Ptr<u8>>) {
    let tag = unsafe { ffi::read_cstr(name) }.unwrap()
    let mut attrs = ""
    let mut i = 0
    loop {
        let key: Ptr<u8> = unsafe { ffi::read_at(attributes, i) }
        if key.address() == 0 {
            break
        }
        let value: Ptr<u8> = unsafe { ffi::read_at(attributes, i + 1) }
        attrs += f" {unsafe { ffi::read_cstr(key) }.unwrap()}={unsafe { ffi::read_cstr(value) }.unwrap()}"
        i += 2
    }
    outline(data).update(|o: &mut Outline| {
        o.lines.push(f"{"  ".repeat(o.depth)}<{tag}{attrs}>")
        o.depth += 1
    })
}

fn on_end(data: Ptr<ffi::c_void>, name: Ptr<u8>) {
    outline(data).update(|o: &mut Outline| o.depth -= 1)
}

fn on_text(data: Ptr<ffi::c_void>, text: Ptr<u8>, len: ffi::c_int) {
    let chunk = String::from_utf8(unsafe { ffi::read_bytes(text, len as i64) }).unwrap().trim()
    if chunk.len() > 0 {
        outline(data).update(|o: &mut Outline| o.lines.push(f"{"  ".repeat(o.depth)}{chunk}"))
    }
}

fn main() {
    let xml = "<library><book id=\"1\" lang=\"en\">Dune</book><book id=\"2\">Solaris</book></library>"
    let state = ffi::Handle::new(Outline { depth: 0, lines: #[] })
    let parser = unsafe { XML_ParserCreate(None) }
    unsafe { XML_SetUserData(parser, state.as_ptr()) }
    unsafe { XML_SetElementHandler(parser, on_start, on_end) }
    unsafe { XML_SetCharacterDataHandler(parser, on_text) }
    let bytes = xml.bytes()
    let ok = unsafe { XML_Parse(parser, bytes, bytes.len() as ffi::c_int, 1) }
    unsafe { XML_ParserFree(parser) }
    for line in state.take().lines {
        println(line)
    }
    println(f"parsed {ok}")
}
"#;

#[cfg(unix)]
#[test]
fn expat_parses_with_sax_callbacks_and_user_data() {
    let project = Project::new("accept-expat", EXPAT_PROGRAM);
    project.expect_everywhere(
        "<library>\n  <book id=1 lang=en>\n    Dune\n  <book id=2>\n    Solaris\nparsed 1",
    );
}
