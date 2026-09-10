//! Method signatures for the standard library's handle types.
//!
//! A handle type (`net::TcpStream`, `http::Client`, `sync::Mutex`, ...)
//! is a runtime registration rather than a manifest export, so neither
//! the stdlib function catalogue nor the manifest carries a contract for
//! its methods. Each row here states one, so `%info` / `%explain` answer
//! with a signature instead of a bare name.
//!
//! Owners are written without their module prefix: the REPL lists a type
//! under both `TcpStream` and `net::TcpStream`, and one row serves both.

/// `(owner, method, signature)`, sorted by owner then method.
pub const HANDLE_SIGNATURES: &[(&str, &str, &str)] = &[
    (
        "AtomicBool",
        "compare_exchange",
        "fn compare_exchange(self: sync::AtomicBool, current: bool, new: bool) -> bool",
    ),
    (
        "AtomicBool",
        "load",
        "fn load(self: sync::AtomicBool) -> bool",
    ),
    (
        "AtomicBool",
        "new",
        "fn new(value: bool) -> sync::AtomicBool",
    ),
    (
        "AtomicBool",
        "store",
        "fn store(self: sync::AtomicBool, value: bool) -> ()",
    ),
    (
        "AtomicI32",
        "fetch_add",
        "fn fetch_add(self: sync::AtomicI32, delta: i64) -> i64",
    ),
    ("AtomicI32", "load", "fn load(self: sync::AtomicI32) -> i64"),
    ("AtomicI32", "new", "fn new(value: i64) -> sync::AtomicI32"),
    (
        "AtomicI32",
        "store",
        "fn store(self: sync::AtomicI32, value: i64) -> ()",
    ),
    (
        "AtomicI64",
        "compare_exchange",
        "fn compare_exchange(self: sync::AtomicI64, current: i64, new: i64) -> bool",
    ),
    (
        "AtomicI64",
        "fetch_add",
        "fn fetch_add(self: sync::AtomicI64, delta: i64) -> i64",
    ),
    (
        "AtomicI64",
        "fetch_sub",
        "fn fetch_sub(self: sync::AtomicI64, delta: i64) -> i64",
    ),
    ("AtomicI64", "load", "fn load(self: sync::AtomicI64) -> i64"),
    ("AtomicI64", "new", "fn new(value: i64) -> sync::AtomicI64"),
    (
        "AtomicI64",
        "store",
        "fn store(self: sync::AtomicI64, value: i64) -> ()",
    ),
    (
        "AtomicU64",
        "compare_exchange",
        "fn compare_exchange(self: sync::AtomicU64, current: i64, new: i64) -> bool",
    ),
    (
        "AtomicU64",
        "fetch_add",
        "fn fetch_add(self: sync::AtomicU64, delta: i64) -> i64",
    ),
    ("AtomicU64", "load", "fn load(self: sync::AtomicU64) -> i64"),
    ("AtomicU64", "new", "fn new(value: i64) -> sync::AtomicU64"),
    (
        "AtomicU64",
        "store",
        "fn store(self: sync::AtomicU64, value: i64) -> ()",
    ),
    ("Barrier", "new", "fn new(parties: i64) -> sync::Barrier"),
    ("Barrier", "wait", "fn wait(self: sync::Barrier) -> ()"),
    (
        "CacheControl",
        "immutable_for",
        "fn immutable_for(seconds: i64) -> String",
    ),
    ("CacheControl", "no_store", "fn no_store() -> String"),
    ("Channel", "close", "fn close<T>(self: Sender<T>) -> ()"),
    (
        "Channel",
        "join",
        "fn join<T>(self: JoinHandle<T>) -> Result<T, String>",
    ),
    (
        "Channel",
        "new",
        "fn new<T>(capacity: i64) -> (Sender<T>, Receiver<T>)",
    ),
    (
        "Channel",
        "recv",
        "fn recv<T>(self: Receiver<T>) -> Option<T>",
    ),
    (
        "Channel",
        "recv_ctx",
        "fn recv_ctx<T>(self: Receiver<T>, ctx: context::Context) -> Option<T>",
    ),
    (
        "Channel",
        "send",
        "fn send<T>(self: Sender<T>, value: T) -> ()",
    ),
    (
        "Channel",
        "try_recv",
        "fn try_recv<T>(self: Receiver<T>) -> Option<T>",
    ),
    (
        "Child",
        "close_stdin",
        "fn close_stdin(self: process::Child) -> ()",
    ),
    ("Child", "kill", "fn kill(self: process::Child) -> bool"),
    (
        "Child",
        "read_line",
        "fn read_line(self: process::Child) -> Option<String>",
    ),
    (
        "Child",
        "read_stdout",
        "fn read_stdout(self: process::Child) -> String",
    ),
    (
        "Child",
        "wait",
        "fn wait(self: process::Child) -> Result<i64, errors::Error>",
    ),
    (
        "Child",
        "write_stdin",
        "fn write_stdin(self: process::Child, text: String) -> bool",
    ),
    (
        "Client",
        "delete",
        "fn delete(self: http::Client, url: String) -> http::Request",
    ),
    (
        "Client",
        "get",
        "fn get(self: http::Client, url: String) -> http::Request",
    ),
    (
        "Client",
        "head",
        "fn head(self: http::Client, url: String) -> http::Request",
    ),
    (
        "Client",
        "options",
        "fn options(self: http::Client, url: String) -> http::Request",
    ),
    (
        "Client",
        "post",
        "fn post(self: http::Client, url: String) -> http::Request",
    ),
    (
        "Client",
        "put",
        "fn put(self: http::Client, url: String) -> http::Request",
    ),
    (
        "Client",
        "request",
        "fn request(self: http::Client, method: String, url: String, body: String, \
         headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>",
    ),
    (
        "Client",
        "request_bytes",
        "fn request_bytes(self: http::Client, method: String, url: String, body: Vec<u8>, \
         headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>",
    ),
    (
        "ClientBuilder",
        "build",
        "fn build(self: http::ClientBuilder) -> http::Client",
    ),
    (
        "ClientBuilder",
        "cookie_jar",
        "fn cookie_jar(self: http::ClientBuilder, enabled: bool) -> http::ClientBuilder",
    ),
    (
        "ClientBuilder",
        "max_redirects",
        "fn max_redirects(self: http::ClientBuilder, count: i64) -> http::ClientBuilder",
    ),
    (
        "ClientBuilder",
        "proxy",
        "fn proxy(self: http::ClientBuilder, url: String) -> http::ClientBuilder",
    ),
    (
        "ClientBuilder",
        "timeout_ms",
        "fn timeout_ms(self: http::ClientBuilder, timeout_ms: i64) -> http::ClientBuilder",
    ),
    (
        "Context",
        "background",
        "fn background() -> context::Context",
    ),
    (
        "Context",
        "cancel",
        "fn cancel(self: context::Context) -> ()",
    ),
    ("Context", "done", "fn done(self: context::Context) -> bool"),
    (
        "Context",
        "done_chan",
        "fn done_chan(self: context::Context) -> Receiver<i64>",
    ),
    (
        "Context",
        "is_cancelled",
        "fn is_cancelled(self: context::Context) -> bool",
    ),
    (
        "Context",
        "with_cancel",
        "fn with_cancel(parent: context::Context) -> context::Context",
    ),
    (
        "Context",
        "with_timeout",
        "fn with_timeout(parent: context::Context, timeout_ms: i64) -> context::Context",
    ),
    (
        "CorsConfig",
        "new",
        "fn new(origin: String, methods: String, headers: String, max_age: i64) -> String",
    ),
    ("CorsConfig", "permissive", "fn permissive() -> String"),
    ("Counter", "inc", "fn inc(self: metrics::Counter) -> ()"),
    (
        "Counter",
        "new",
        "fn new(name: String, help: String) -> metrics::Counter",
    ),
    (
        "Counter",
        "value",
        "fn value(self: metrics::Counter) -> i64",
    ),
    (
        "Duration",
        "as_micros",
        "fn as_micros(self: time::Duration) -> i64",
    ),
    (
        "Duration",
        "as_millis",
        "fn as_millis(self: time::Duration) -> i64",
    ),
    (
        "Duration",
        "as_secs",
        "fn as_secs(self: time::Duration) -> i64",
    ),
    (
        "Duration",
        "from_micros",
        "fn from_micros(micros: i64) -> time::Duration",
    ),
    (
        "Duration",
        "from_millis",
        "fn from_millis(millis: i64) -> time::Duration",
    ),
    (
        "Duration",
        "from_secs",
        "fn from_secs(secs: i64) -> time::Duration",
    ),
    (
        "EndedSpan",
        "to_otlp_json",
        "fn to_otlp_json(self: trace::EndedSpan) -> String",
    ),
    (
        "Error",
        "cause",
        "fn cause(self: errors::Error) -> Option<errors::Error>",
    ),
    (
        "Error",
        "chain",
        "fn chain(self: errors::Error) -> Vec<String>",
    ),
    (
        "Error",
        "field",
        "fn field(self: errors::Error, name: String) -> Option<String>",
    ),
    (
        "Error",
        "fields",
        "fn fields(self: errors::Error) -> Map<String, String>",
    ),
    ("Error", "from", "fn from<T>(value: T) -> errors::Error"),
    (
        "Error",
        "is",
        "fn is<T>(self: errors::Error, needle: T) -> bool",
    ),
    (
        "Error",
        "message",
        "fn message(self: errors::Error) -> String",
    ),
    (
        "Error",
        "with_field",
        "fn with_field(self: errors::Error, name: String, value: String) -> errors::Error",
    ),
    (
        "Errors",
        "add",
        "fn add(self: validate::Errors, path: String, error: validate::FieldError) -> ()",
    ),
    (
        "Errors",
        "collect",
        "fn collect(self: validate::Errors) -> String",
    ),
    (
        "Errors",
        "count",
        "fn count(self: validate::Errors, path: String) -> i64",
    ),
    (
        "Errors",
        "get",
        "fn get(self: validate::Errors, path: String) -> String",
    ),
    (
        "Errors",
        "is_empty",
        "fn is_empty(self: validate::Errors) -> bool",
    ),
    ("Errors", "len", "fn len(self: validate::Errors) -> i64"),
    (
        "FieldError",
        "code",
        "fn code(self: validate::FieldError) -> String",
    ),
    (
        "FieldError",
        "message",
        "fn message(self: validate::FieldError) -> String",
    ),
    (
        "FieldError",
        "path",
        "fn path(self: validate::FieldError) -> String",
    ),
    ("File", "close", "fn close(self: fs::File) -> ()"),
    (
        "File",
        "create",
        "fn create(path: String) -> Result<fs::File, errors::Error>",
    ),
    (
        "File",
        "flush",
        "fn flush(self: fs::File) -> Result<(), errors::Error>",
    ),
    (
        "File",
        "len",
        "fn len(self: fs::File) -> Result<i64, errors::Error>",
    ),
    (
        "File",
        "open",
        "fn open(path: String) -> Result<fs::File, errors::Error>",
    ),
    (
        "File",
        "read",
        "fn read(self: fs::File, max: i64) -> Result<Vec<u8>, errors::Error>",
    ),
    (
        "File",
        "read_at",
        "fn read_at(self: fs::File, len: i64, offset: i64) -> Result<Vec<u8>, errors::Error>",
    ),
    (
        "File",
        "read_to_string",
        "fn read_to_string(self: fs::File) -> Result<String, errors::Error>",
    ),
    (
        "File",
        "seek",
        "fn seek(self: fs::File, offset: i64, whence: i64) -> Result<i64, errors::Error>",
    ),
    (
        "File",
        "set_len",
        "fn set_len(self: fs::File, len: i64) -> Result<(), errors::Error>",
    ),
    (
        "File",
        "sync_all",
        "fn sync_all(self: fs::File) -> Result<(), errors::Error>",
    ),
    (
        "File",
        "sync_data",
        "fn sync_data(self: fs::File) -> Result<(), errors::Error>",
    ),
    (
        "File",
        "try_lock_exclusive",
        "fn try_lock_exclusive(self: fs::File) -> Result<bool, errors::Error>",
    ),
    (
        "File",
        "try_lock_range",
        "fn try_lock_range(self: fs::File, start: i64, len: i64, exclusive: bool) \
         -> Result<bool, errors::Error>",
    ),
    (
        "File",
        "try_lock_shared",
        "fn try_lock_shared(self: fs::File) -> Result<bool, errors::Error>",
    ),
    (
        "File",
        "unlock",
        "fn unlock(self: fs::File) -> Result<(), errors::Error>",
    ),
    (
        "File",
        "unlock_range",
        "fn unlock_range(self: fs::File, start: i64, len: i64) -> Result<(), errors::Error>",
    ),
    (
        "File",
        "write",
        "fn write(self: fs::File, data: String) -> Result<i64, errors::Error>",
    ),
    (
        "File",
        "write_all",
        "fn write_all(self: fs::File, data: String) -> Result<i64, errors::Error>",
    ),
    (
        "File",
        "write_at",
        "fn write_at(self: fs::File, data: Vec<u8>, offset: i64) -> Result<i64, errors::Error>",
    ),
    (
        "File",
        "write_bytes",
        "fn write_bytes(self: fs::File, data: Vec<u8>) -> Result<i64, errors::Error>",
    ),
    (
        "FileServer",
        "new",
        "fn new(root: String, prefix: String) -> http::static_files::FileServer",
    ),
    (
        "FileServer",
        "serve",
        "fn serve(self: http::static_files::FileServer, request: http::Request) \
         -> Result<http::Response, errors::Error>",
    ),
    (
        "FlagMap",
        "get",
        "fn get(self: flag::Set, name: String) -> String",
    ),
    ("Gauge", "dec", "fn dec(self: metrics::Gauge) -> ()"),
    ("Gauge", "inc", "fn inc(self: metrics::Gauge) -> ()"),
    (
        "Gauge",
        "new",
        "fn new(name: String, help: String) -> metrics::Gauge",
    ),
    (
        "Gauge",
        "set",
        "fn set(self: metrics::Gauge, value: f64) -> ()",
    ),
    ("Gauge", "value", "fn value(self: metrics::Gauge) -> f64"),
    (
        "Histogram",
        "count",
        "fn count(self: metrics::Histogram) -> i64",
    ),
    (
        "Histogram",
        "new",
        "fn new(name: String, help: String, buckets: Vec<f64>) -> metrics::Histogram",
    ),
    (
        "Histogram",
        "observe",
        "fn observe(self: metrics::Histogram, value: f64) -> ()",
    ),
    (
        "Histogram",
        "sum",
        "fn sum(self: metrics::Histogram) -> f64",
    ),
    ("HstsConfig", "safe_default", "fn safe_default() -> String"),
    ("HstsConfig", "strict", "fn strict() -> String"),
    (
        "Http2Config",
        "default",
        "fn default() -> http::Http2Config",
    ),
    (
        "I64Vec",
        "get_at",
        "fn get_at(self: I64Vec, index: i64) -> i64",
    ),
    ("I64Vec", "new", "fn new(length: i64) -> I64Vec"),
    (
        "I64Vec",
        "set_at",
        "fn set_at(self: I64Vec, index: i64, value: i64) -> ()",
    ),
    ("I64Vec", "vec_len", "fn vec_len(self: I64Vec) -> i64"),
    (
        "I64Vec",
        "write_lines_to_stdout",
        "fn write_lines_to_stdout(self: I64Vec, offset: i64, count: i64) -> ()",
    ),
    (
        "I64Vec",
        "write_range_to_stdout",
        "fn write_range_to_stdout(self: I64Vec, offset: i64, count: i64) -> ()",
    ),
    (
        "Instant",
        "elapsed_ms",
        "fn elapsed_ms(self: time::Instant) -> i64",
    ),
    ("Instant", "now", "fn now() -> time::Instant"),
    (
        "Middleware",
        "serve",
        "fn serve(self: http::middleware::Middleware, request: http::Request) \
         -> Result<http::Response, errors::Error>",
    ),
    ("Mutex", "lock", "fn lock<T>(self: sync::Mutex<T>) -> ()"),
    ("Mutex", "new", "fn new<T>(value: T) -> sync::Mutex<T>"),
    (
        "Mutex",
        "store",
        "fn store<T>(self: sync::Mutex<T>, value: T) -> ()",
    ),
    (
        "Mutex",
        "unlock",
        "fn unlock<T>(self: sync::Mutex<T>) -> ()",
    ),
    (
        "Once",
        "call",
        "fn call(self: sync::Once, f: Fn() -> ()) -> bool",
    ),
    ("Once", "new", "fn new() -> sync::Once"),
    (
        "OpenOptions",
        "append",
        "fn append(self: fs::OpenOptions, enabled: bool) -> fs::OpenOptions",
    ),
    (
        "OpenOptions",
        "create",
        "fn create(self: fs::OpenOptions, enabled: bool) -> fs::OpenOptions",
    ),
    (
        "OpenOptions",
        "create_new",
        "fn create_new(self: fs::OpenOptions, enabled: bool) -> fs::OpenOptions",
    ),
    ("OpenOptions", "new", "fn new() -> fs::OpenOptions"),
    (
        "OpenOptions",
        "open",
        "fn open(self: fs::OpenOptions, path: String) -> Result<fs::File, errors::Error>",
    ),
    (
        "OpenOptions",
        "read",
        "fn read(self: fs::OpenOptions, enabled: bool) -> fs::OpenOptions",
    ),
    (
        "OpenOptions",
        "truncate",
        "fn truncate(self: fs::OpenOptions, enabled: bool) -> fs::OpenOptions",
    ),
    (
        "OpenOptions",
        "write",
        "fn write(self: fs::OpenOptions, enabled: bool) -> fs::OpenOptions",
    ),
    (
        "Pattern",
        "captures",
        "fn captures(self: regex::Pattern, text: String) -> Option<Vec<Option<String>>>",
    ),
    (
        "Pattern",
        "captures_all",
        "fn captures_all(self: regex::Pattern, text: String) -> Vec<Vec<Option<String>>>",
    ),
    (
        "Pattern",
        "compile",
        "fn compile(pattern: String) -> Result<regex::Pattern, errors::Error>",
    ),
    (
        "Pattern",
        "count",
        "fn count(self: regex::Pattern, text: String) -> i64",
    ),
    (
        "Pattern",
        "find",
        "fn find(self: regex::Pattern, text: String) -> Option<(i64, i64, String)>",
    ),
    (
        "Pattern",
        "find_all",
        "fn find_all(self: regex::Pattern, text: String) -> Vec<(i64, i64, String)>",
    ),
    (
        "Pattern",
        "is_match",
        "fn is_match(self: regex::Pattern, text: String) -> bool",
    ),
    (
        "Pattern",
        "replace",
        "fn replace(self: regex::Pattern, text: String, replacement: String) -> String",
    ),
    (
        "Pattern",
        "replace_all",
        "fn replace_all(self: regex::Pattern, text: String, replacement: String) -> String",
    ),
    (
        "Pattern",
        "split",
        "fn split(self: regex::Pattern, text: String) -> Vec<String>",
    ),
    (
        "RateLimit",
        "per_ip",
        "fn per_ip(burst: i64, per_second: i64) -> String",
    ),
    ("Registry", "new", "fn new() -> metrics::Registry"),
    (
        "Registry",
        "register",
        "fn register<M>(self: metrics::Registry, metric: M) -> ()",
    ),
    (
        "Registry",
        "render",
        "fn render(self: metrics::Registry) -> String",
    ),
    (
        "Request",
        "basic_auth",
        "fn basic_auth(self: http::Request) -> Option<(String, String)>",
    ),
    (
        "Request",
        "body",
        "fn body(self: http::Request, body: String) -> http::Request",
    ),
    (
        "Request",
        "form_value",
        "fn form_value(self: http::Request, name: String) -> String",
    ),
    (
        "Request",
        "header",
        "fn header(self: http::Request, name: String, value: String) -> http::Request",
    ),
    (
        "Request",
        "path_float",
        "fn path_float(self: http::Request, name: String) -> Option<f64>",
    ),
    (
        "Request",
        "path_int",
        "fn path_int(self: http::Request, name: String) -> Option<i64>",
    ),
    (
        "Request",
        "path_value",
        "fn path_value(self: http::Request, name: String) -> String",
    ),
    (
        "Request",
        "send",
        "fn send(self: http::Request) -> Result<http::Response, errors::Error>",
    ),
    (
        "Request",
        "set_value",
        "fn set_value(self: http::Request, key: String, value: String) -> http::Request",
    ),
    (
        "Request",
        "value",
        "fn value(self: http::Request, key: String) -> String",
    ),
    (
        "Response",
        "bytes",
        "fn bytes(self: http::Response) -> Vec<u8>",
    ),
    (
        "Response",
        "json",
        "fn json(status: i64, body: String) -> http::Response",
    ),
    (
        "Response",
        "stream",
        "fn stream(status: i64, content_type: String, body: http::ResponseStream) \
         -> http::Response",
    ),
    (
        "Response",
        "text",
        "fn text(status: i64, body: String) -> http::Response",
    ),
    (
        "Response",
        "with_header",
        "fn with_header(self: http::Response, name: String, value: String) -> http::Response",
    ),
    (
        "ResponseStream",
        "next_chunk",
        "fn next_chunk(self: http::ResponseStream, size: i64) -> Option<Vec<u8>>",
    ),
    (
        "ResponseStream",
        "next_line",
        "fn next_line(self: http::ResponseStream) -> Option<String>",
    ),
    ("Rng", "new", "fn new(seed: i64) -> rand::Rng"),
    ("Rng", "next_f64", "fn next_f64(self: rand::Rng) -> f64"),
    ("Rng", "next_u32", "fn next_u32(self: rand::Rng) -> i64"),
    ("Rng", "next_u64", "fn next_u64(self: rand::Rng) -> i64"),
    (
        "Rng",
        "range_u64",
        "fn range_u64(self: rand::Rng, low: i64, high: i64) -> i64",
    ),
    (
        "Router",
        "delete",
        "fn delete(self: http::router::Router, path: String, \
         handler: Fn(http::Request) -> Result<http::Response, errors::Error>) \
         -> http::router::Router",
    ),
    (
        "Router",
        "get",
        "fn get(self: http::router::Router, path: String, \
         handler: Fn(http::Request) -> Result<http::Response, errors::Error>) \
         -> http::router::Router",
    ),
    (
        "Router",
        "head",
        "fn head(self: http::router::Router, path: String, \
         handler: Fn(http::Request) -> Result<http::Response, errors::Error>) \
         -> http::router::Router",
    ),
    ("Router", "new", "fn new() -> http::router::Router"),
    (
        "Router",
        "options",
        "fn options(self: http::router::Router, path: String, \
         handler: Fn(http::Request) -> Result<http::Response, errors::Error>) \
         -> http::router::Router",
    ),
    (
        "Router",
        "patch",
        "fn patch(self: http::router::Router, path: String, \
         handler: Fn(http::Request) -> Result<http::Response, errors::Error>) \
         -> http::router::Router",
    ),
    (
        "Router",
        "post",
        "fn post(self: http::router::Router, path: String, \
         handler: Fn(http::Request) -> Result<http::Response, errors::Error>) \
         -> http::router::Router",
    ),
    (
        "Router",
        "put",
        "fn put(self: http::router::Router, path: String, \
         handler: Fn(http::Request) -> Result<http::Response, errors::Error>) \
         -> http::router::Router",
    ),
    (
        "Router",
        "serve",
        "fn serve(self: http::router::Router, request: http::Request) \
         -> Result<http::Response, errors::Error>",
    ),
    ("RwLock", "new", "fn new(value: i64) -> sync::RwLock<i64>"),
    ("RwLock", "read", "fn read(self: sync::RwLock<i64>) -> i64"),
    (
        "RwLock",
        "with_read",
        "fn with_read<T>(self: sync::RwLock<i64>, f: Fn(i64) -> T) -> T",
    ),
    (
        "RwLock",
        "with_write",
        "fn with_write<T>(self: sync::RwLock<i64>, f: Fn(i64) -> T) -> T",
    ),
    (
        "RwLock",
        "write",
        "fn write(self: sync::RwLock<i64>, value: i64) -> ()",
    ),
    (
        "Scanner",
        "new",
        "fn new(source: io::Stream) -> bufio::Scanner",
    ),
    (
        "Scanner",
        "next",
        "fn next(self: bufio::Scanner) -> Option<String>",
    ),
    ("Scanner", "scan", "fn scan(self: bufio::Scanner) -> bool"),
    ("Scanner", "text", "fn text(self: bufio::Scanner) -> String"),
    ("SecurityHeaders", "off", "fn off() -> String"),
    ("SecurityHeaders", "strict", "fn strict() -> String"),
    ("Set", "new", "fn new<T>() -> Set<T>"),
    (
        "Span",
        "end",
        "fn end(self: trace::Span) -> trace::EndedSpan",
    ),
    (
        "Span",
        "set_attribute",
        "fn set_attribute(self: trace::Span, key: String, value: String) -> ()",
    ),
    (
        "Span",
        "set_status",
        "fn set_status(self: trace::Span, ok: bool, message: String) -> ()",
    ),
    ("Stream", "flush", "fn flush(self: io::Stream) -> ()"),
    (
        "Stream",
        "read_line",
        "fn read_line(self: io::Stream) -> Option<String>",
    ),
    (
        "Stream",
        "read_to_string",
        "fn read_to_string(self: io::Stream) -> String",
    ),
    (
        "Stream",
        "write",
        "fn write(self: io::Stream, text: String) -> ()",
    ),
    (
        "Stream",
        "write_byte",
        "fn write_byte(self: io::Stream, byte: i64) -> ()",
    ),
    (
        "Stream",
        "write_byte_array",
        "fn write_byte_array(self: io::Stream, bytes: Vec<u8>) -> ()",
    ),
    (
        "Stream",
        "write_str",
        "fn write_str(self: io::Stream, text: String) -> ()",
    ),
    (
        "TcpListener",
        "accept",
        "fn accept(self: net::TcpListener) -> Result<(net::TcpStream, String), errors::Error>",
    ),
    (
        "TcpListener",
        "bind",
        "fn bind(address: String) -> Result<net::TcpListener, errors::Error>",
    ),
    (
        "TcpListener",
        "close",
        "fn close(self: net::TcpListener) -> ()",
    ),
    (
        "TcpListener",
        "local_addr",
        "fn local_addr(self: net::TcpListener) -> Result<String, errors::Error>",
    ),
    (
        "TcpStream",
        "clear_read_timeout",
        "fn clear_read_timeout(self: net::TcpStream) -> ()",
    ),
    (
        "TcpStream",
        "clear_write_timeout",
        "fn clear_write_timeout(self: net::TcpStream) -> ()",
    ),
    ("TcpStream", "close", "fn close(self: net::TcpStream) -> ()"),
    (
        "TcpStream",
        "connect",
        "fn connect(address: String) -> Result<net::TcpStream, errors::Error>",
    ),
    (
        "TcpStream",
        "peer_certificate",
        "fn peer_certificate(self: net::TcpStream) -> Vec<u8>",
    ),
    (
        "TcpStream",
        "read",
        "fn read(self: net::TcpStream, max: i64) -> Result<Vec<u8>, errors::Error>",
    ),
    (
        "TcpStream",
        "read_into",
        "fn read_into(self: net::TcpStream, buf: &mut Vec<u8>, max: i64) -> Result<i64, errors::Error>",
    ),
    (
        "TcpStream",
        "read_to_string",
        "fn read_to_string(self: net::TcpStream) -> Result<String, errors::Error>",
    ),
    (
        "TcpStream",
        "set_nodelay",
        "fn set_nodelay(self: net::TcpStream, on: bool) -> ()",
    ),
    (
        "TcpStream",
        "set_read_timeout_ms",
        "fn set_read_timeout_ms(self: net::TcpStream, timeout_ms: i64) -> ()",
    ),
    (
        "TcpStream",
        "set_write_timeout_ms",
        "fn set_write_timeout_ms(self: net::TcpStream, timeout_ms: i64) -> ()",
    ),
    (
        "TcpStream",
        "start_tls",
        "fn start_tls(self: net::TcpStream, host: String) -> Result<net::TcpStream, errors::Error>",
    ),
    (
        "TcpStream",
        "start_tls_ca",
        "fn start_tls_ca(self: net::TcpStream, host: String, ca_pem: String) \
         -> Result<net::TcpStream, errors::Error>",
    ),
    (
        "TcpStream",
        "start_tls_insecure",
        "fn start_tls_insecure(self: net::TcpStream, host: String) \
         -> Result<net::TcpStream, errors::Error>",
    ),
    (
        "TcpStream",
        "write",
        "fn write(self: net::TcpStream, data: Vec<u8>) -> Result<(), errors::Error>",
    ),
    (
        "TcpStream",
        "write_all",
        "fn write_all(self: net::TcpStream, data: Vec<u8>) -> Result<(), errors::Error>",
    ),
    ("Tracer", "new", "fn new() -> trace::Tracer"),
    (
        "Tracer",
        "start_span",
        "fn start_span(self: trace::Tracer, name: String) -> trace::Span",
    ),
    ("U8Vec", "byte_len", "fn byte_len(self: U8Vec) -> i64"),
    (
        "U8Vec",
        "count_kmers",
        "fn count_kmers(self: U8Vec, length: i64, k: i64) -> Map<i64, i64>",
    ),
    (
        "U8Vec",
        "count_pairs",
        "fn count_pairs(self: U8Vec, length: i64) -> Vec<i64>",
    ),
    (
        "U8Vec",
        "count_singles",
        "fn count_singles(self: U8Vec, length: i64) -> Vec<i64>",
    ),
    (
        "U8Vec",
        "get_byte",
        "fn get_byte(self: U8Vec, index: i64) -> i64",
    ),
    ("U8Vec", "new", "fn new(length: i64) -> U8Vec"),
    (
        "U8Vec",
        "set_byte",
        "fn set_byte(self: U8Vec, index: i64, value: i64) -> ()",
    ),
    (
        "U8Vec",
        "to_string",
        "fn to_string(self: U8Vec, length: i64) -> String",
    ),
    (
        "U8Vec",
        "window_key",
        "fn window_key(self: U8Vec, index: i64, k: i64) -> i64",
    ),
    (
        "U8Vec",
        "write_byte_lines_to_stdout",
        "fn write_byte_lines_to_stdout(self: U8Vec, offset: i64, count: i64) -> ()",
    ),
    (
        "U8Vec",
        "write_byte_range_to_stdout",
        "fn write_byte_range_to_stdout(self: U8Vec, offset: i64, count: i64) -> ()",
    ),
    (
        "UdpSocket",
        "bind",
        "fn bind(address: String) -> Result<net::UdpSocket, errors::Error>",
    ),
    ("UdpSocket", "close", "fn close(self: net::UdpSocket) -> ()"),
    (
        "UdpSocket",
        "local_addr",
        "fn local_addr(self: net::UdpSocket) -> Result<String, errors::Error>",
    ),
    (
        "UdpSocket",
        "recv_from",
        "fn recv_from(self: net::UdpSocket, max: i64) -> Result<(Vec<u8>, String), errors::Error>",
    ),
    (
        "UdpSocket",
        "send_to",
        "fn send_to(self: net::UdpSocket, data: Vec<u8>, address: String) \
         -> Result<i64, errors::Error>",
    ),
    (
        "UnixListener",
        "accept",
        "fn accept(self: net::UnixListener) -> Result<(net::UnixStream, String), errors::Error>",
    ),
    (
        "UnixListener",
        "bind",
        "fn bind(path: String) -> Result<net::UnixListener, errors::Error>",
    ),
    (
        "UnixListener",
        "close",
        "fn close(self: net::UnixListener) -> ()",
    ),
    (
        "UnixStream",
        "close",
        "fn close(self: net::UnixStream) -> ()",
    ),
    (
        "UnixStream",
        "connect",
        "fn connect(path: String) -> Result<net::UnixStream, errors::Error>",
    ),
    (
        "UnixStream",
        "read",
        "fn read(self: net::UnixStream, max: i64) -> Result<Vec<u8>, errors::Error>",
    ),
    (
        "UnixStream",
        "read_to_string",
        "fn read_to_string(self: net::UnixStream) -> Result<String, errors::Error>",
    ),
    (
        "UnixStream",
        "write",
        "fn write(self: net::UnixStream, data: Vec<u8>) -> Result<(), errors::Error>",
    ),
    (
        "UnixStream",
        "write_all",
        "fn write_all(self: net::UnixStream, data: Vec<u8>) -> Result<(), errors::Error>",
    ),
    (
        "Value",
        "object",
        "fn object(pairs: Vec<(String, json::Value)>) -> json::Value",
    ),
    (
        "WaitGroup",
        "add",
        "fn add(self: sync::WaitGroup, delta: i64) -> ()",
    ),
    ("WaitGroup", "done", "fn done(self: sync::WaitGroup) -> ()"),
    ("WaitGroup", "new", "fn new() -> sync::WaitGroup"),
    ("WaitGroup", "wait", "fn wait(self: sync::WaitGroup) -> ()"),
    (
        "WaitGroup",
        "wait_ctx",
        "fn wait_ctx(self: sync::WaitGroup, ctx: context::Context) -> bool",
    ),
];

/// The signature for `owner::name`, with `owner` given bare or module
/// qualified (`TcpStream` and `net::TcpStream` name the same type).
#[must_use]
pub fn handle_signature(owner: &str, name: &str) -> Option<&'static str> {
    let owner = owner.rsplit("::").next().unwrap_or(owner);
    HANDLE_SIGNATURES
        .binary_search_by(|(row_owner, row_name, _)| (*row_owner, *row_name).cmp(&(owner, name)))
        .ok()
        .map(|idx| HANDLE_SIGNATURES[idx].2)
}

#[cfg(test)]
mod tests {
    use super::{HANDLE_SIGNATURES, handle_signature};

    /// The table is binary-searched, so an inversion hides every row
    /// past it.
    #[test]
    fn table_is_sorted_and_unique() {
        for pair in HANDLE_SIGNATURES.windows(2) {
            let (a_owner, a_name, _) = pair[0];
            let (b_owner, b_name, _) = pair[1];
            assert!(
                (a_owner, a_name) < (b_owner, b_name),
                "{a_owner}::{a_name} must sort before {b_owner}::{b_name}"
            );
        }
    }

    /// Every row states a full contract: a parameter list and a return.
    #[test]
    fn every_row_is_a_complete_signature() {
        for (owner, name, signature) in HANDLE_SIGNATURES {
            let head = signature
                .strip_prefix(&format!("fn {name}"))
                .unwrap_or_else(|| {
                    panic!("{owner}::{name} must open with its own name: {signature}")
                });
            assert!(
                head.starts_with('(') || head.starts_with('<'),
                "{owner}::{name} must state a parameter list: {signature}"
            );
            assert!(
                signature.contains(") -> "),
                "{owner}::{name} must state a return type: {signature}"
            );
        }
    }

    #[test]
    fn a_module_qualified_owner_finds_the_same_row() {
        assert_eq!(
            handle_signature("net::TcpStream", "close"),
            handle_signature("TcpStream", "close")
        );
        assert!(handle_signature("TcpStream", "close").is_some());
        assert!(handle_signature("TcpStream", "nonexistent").is_none());
    }
}
