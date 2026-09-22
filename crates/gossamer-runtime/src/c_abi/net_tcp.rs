//! C-ABI dispatch shims for `std::net::TcpListener` / `TcpStream`.
//! Mirrors the bytecode-VM builtins in
//! `gossamer-interp/src/stdlib_builtins/net.rs` so the compiled
//! (Cranelift / LLVM) tier resolves the same calls natively instead of
//! failing to link.
//!
//! Handle model - process-global registries keyed by an `i64` handle,
//! the same shape the SQL handle registry uses (`c_abi/sql.rs`). A
//! handle is a plain integer at the Gossamer level, so it crosses
//! goroutine boundaries freely; the underlying `std::net` sockets are
//! `Send + Sync`. Sockets are stored behind `Arc` so a blocking call
//! never holds the registry lock: each op clones the `Arc` under the
//! lock, releases it, then performs the (possibly blocking) `accept` /
//! `read` / `write` on the shared handle outside the lock. This is
//! deadlock-free under the goroutine scheduler - a server parked in
//! `accept()` does not block a peer goroutine from registering its
//! `connect()`ed stream. `close` drops the registry's `Arc`; any
//! in-flight clone keeps the socket alive until it too drops (no
//! use-after-free on a concurrent close).
//!
//! All reads/writes go through `&TcpStream` (`std` implements
//! `Read`/`Write` for `&TcpStream`), and `TcpListener::accept` /
//! `local_addr` take `&self`, so no `&mut` ownership is needed past the
//! `Arc` - the registry never hands out exclusive access.
//!
//! Cross-platform: built entirely on `std::net` (Linux / macOS /
//! Windows). `std` performs `WSAStartup` lazily on Windows; there is no
//! libc / raw-fd / unix-only surface here.
//!
//! Result shapes (packed `i128` via `gos_rt_result_new`):
//! - `bind` / `connect`  -> `Result<TcpListener|TcpStream, Error>` (Ok payload = i64 handle)
//! - `accept`            -> `Result<(TcpStream, String), Error>` (Ok payload = *Pair{handle, addr})
//! - `local_addr`        -> `Result<String, Error>`
//! - `read`              -> `Result<[u8], Error>`
//! - `read_to_string`    -> `Result<String, Error>`
//! - `write`             -> `Result<i64, Error>` (bytes written)
//! - `close`             -> () (Void)

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_lossless)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::wildcard_imports)]

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme, StreamOwned,
};

use super::vec::GosVec;

// Process-global handle registries shared with every linked copy of the
// runtime. `Option` so the `Mutex::new(None)` initialiser is const.
static TCP_LISTENERS: Mutex<Option<HashMap<i64, Arc<TcpListener>>>> = Mutex::new(None);
static TCP_STREAMS: Mutex<Option<HashMap<i64, Arc<TcpStream>>>> = Mutex::new(None);
static NEXT_TCP_HANDLE: AtomicI64 = AtomicI64::new(1);

fn next_handle() -> i64 {
    NEXT_TCP_HANDLE.fetch_add(1, Ordering::Relaxed)
}

fn cstr_to_str(p: *const c_char) -> String {
    unsafe { crate::c_abi::gos_str_arg_string(p) }
}

/// Packs `Err(errors::Error)` as the runtime's `i128` Result.
fn tcp_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    super::vec::gos_rt_result_new(1, err as i64)
}

/// The text a socket failure reads as, matching the interp tier's
/// `IoError` rendering so one failure reads the same on every tier.
fn socket_error(err: &std::io::Error, context: &str) -> String {
    crate::c_abi::fs::classify_io_error(err, context)
}

/// The text a transfer failure reads as. The sockets behind these shims
/// are blocking, so `WouldBlock` is the deadline the caller set coming
/// due, which reads as the wait that ended rather than as the kernel's
/// code for it.
fn transfer_error(err: &std::io::Error, context: &str, timed_out: &str) -> String {
    match err.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => format!("io: {context}: {timed_out}"),
        _ => socket_error(err, context),
    }
}

fn listener_clone(h: i64) -> Option<Arc<TcpListener>> {
    TCP_LISTENERS
        .lock()
        .as_ref()
        .and_then(|m| m.get(&h).cloned())
}

fn stream_clone(h: i64) -> Option<Arc<TcpStream>> {
    TCP_STREAMS.lock().as_ref().and_then(|m| m.get(&h).cloned())
}

fn insert_stream(s: TcpStream) -> i64 {
    let h = next_handle();
    TCP_STREAMS
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(h, Arc::new(s));
    h
}

// --- TLS upgrade --------------------------------------------------
//
// `start_tls` wraps an already-connected plaintext stream in a rustls
// client session and registers it under a fresh handle in the SAME
// integer namespace as plain streams. `read` / `write` / `close`
// consult `TLS_STREAMS` first, so the existing `net::TcpStream` method
// surface transparently drives either an encrypted or a plaintext
// socket - no separate language-level type is needed.
//
// rustls' `StreamOwned` mutates its session state on every I/O, so the
// stream is held behind `Arc<Mutex<_>>` (unlike the `Arc<TcpStream>`
// plaintext path, where `&TcpStream` is `Read`/`Write`). One PostgreSQL
// connection is request/response-serialised, so the per-op lock never
// contends.
type TlsStream = StreamOwned<ClientConnection, TcpStream>;

static TLS_STREAMS: Mutex<Option<HashMap<i64, Arc<Mutex<TlsStream>>>>> = Mutex::new(None);

fn tls_clone(h: i64) -> Option<Arc<Mutex<TlsStream>>> {
    TLS_STREAMS.lock().as_ref().and_then(|m| m.get(&h).cloned())
}

/// Shared TLS client config: ring provider, webpki roots, verification
/// always on - the same trust anchors as the HTTP client so an upgraded
/// stream validates certificates identically on every tier.
static TLS_CLIENT_CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // ring always supports the default protocol versions; the only
    // error path is a provider missing them, which cannot happen here.
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring provider supports default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
});

/// `net::TcpStream::start_tls(h, host) -> Result<TcpStream, Error>`.
/// Upgrades an already-connected plaintext stream to TLS in place once
/// the caller has finished any plaintext pre-handshake (e.g.
/// PostgreSQL's `SSLRequest`). Returns a fresh handle for the encrypted
/// stream; the plaintext handle `h` is consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_start_tls(h: i64, host: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        start_tls_with(h, host, Arc::clone(&TLS_CLIENT_CONFIG))
    })
}

/// `net::TcpStream::start_tls_insecure(h, host) -> Result<TcpStream, Error>`.
/// Encrypts the connection without authenticating the peer certificate
/// (PostgreSQL `sslmode=require`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_start_tls_insecure(h: i64, host: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        start_tls_with(h, host, Arc::clone(&TLS_CLIENT_CONFIG_INSECURE))
    })
}

/// `net::TcpStream::start_tls_ca(h, host, ca_pem) -> Result<TcpStream, Error>`.
/// Verifies the server certificate chain and hostname against the
/// PEM-encoded CA bundle in `ca_pem` (PostgreSQL `sslmode=verify-full`
/// against a private CA).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_start_tls_ca(
    h: i64,
    host: *const c_char,
    ca_pem: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        let ca = cstr_to_str(ca_pem);
        let config = match tls_config_ca(ca.as_bytes()) {
            Ok(c) => c,
            Err(e) => return tcp_err(&format!("io: start_tls_ca: {e}")),
        };
        start_tls_with(h, host, config)
    })
}

/// `net::TcpStream::peer_certificate(h) -> Vec<u8>` - the DER bytes of the
/// end-entity certificate the peer presented, empty when the stream is not
/// TLS or the peer sent none.
///
/// This is what SCRAM's `tls-server-end-point` channel binding hashes, so a
/// client can prove its authentication exchange and its TLS connection are
/// the same one. The bytes are handed over unhashed: which digest the
/// binding calls for is the caller's rule, not the socket's.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_tls_peer_cert(h: i64) -> *mut super::vec::GosVec {
    ffi_entry!(std::ptr::null_mut(), {
        let Some(stream) = tls_clone(h) else {
            return crate::c_abi::encoding::bytes_to_gosvec(&[]);
        };
        let mut guard = stream.lock();
        // rustls hands the certificate over only once the handshake it was
        // presented in has finished, and a session opens without any I/O.
        while guard.conn.is_handshaking() {
            let stream = &mut *guard;
            if stream.conn.complete_io(&mut stream.sock).is_err() {
                break;
            }
        }
        let der = guard
            .conn
            .peer_certificates()
            .and_then(|chain| chain.first())
            .map(|cert| cert.as_ref().to_vec())
            .unwrap_or_default();
        drop(guard);
        crate::c_abi::encoding::bytes_to_gosvec(&der)
    })
}

/// Shared TLS upgrade: duplicate the connected socket, drop the
/// plaintext handle so no caller reads cleartext on it, and register a
/// rustls client session built from `config` under a fresh handle.
fn start_tls_with(h: i64, host: *const c_char, config: Arc<rustls::ClientConfig>) -> i128 {
    let Some(stream) = stream_clone(h) else {
        return tcp_err("TcpStream::start_tls: stale handle");
    };
    let host = cstr_to_str(host);
    let sock = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => return tcp_err(&socket_error(&e, "start_tls")),
    };
    if let Some(m) = TCP_STREAMS.lock().as_mut() {
        m.remove(&h);
    }
    let Ok(name) = ServerName::try_from(host.clone()) else {
        return tcp_err(&format!("io: start_tls: invalid server name `{host}`"));
    };
    let conn = match ClientConnection::new(config, name) {
        Ok(c) => c,
        Err(e) => return tcp_err(&format!("io: start_tls: {e}")),
    };
    let nh = next_handle();
    TLS_STREAMS
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(nh, Arc::new(Mutex::new(StreamOwned::new(conn, sock))));
    super::vec::gos_rt_result_new(0, nh)
}

/// Client config that accepts any server certificate (PostgreSQL
/// `sslmode=require`); handshake signatures are still checked against
/// the presented key, only chain and hostname verification are skipped.
static TLS_CLIENT_CONFIG_INSECURE: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring provider supports default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertVerify(provider)))
        .with_no_client_auth();
    Arc::new(config)
});

/// Builds a client config that validates the server certificate chain
/// and hostname against `ca_pem` rather than the bundled public roots.
fn tls_config_ca(ca_pem: &[u8]) -> Result<Arc<rustls::ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    let mut added = 0usize;
    for cert in CertificateDer::pem_slice_iter(ca_pem) {
        let cert = cert.map_err(|e| format!("parse CA: {e}"))?;
        roots.add(cert).map_err(|e| format!("add CA: {e}"))?;
        added += 1;
    }
    if added == 0 {
        return Err("no certificates in CA PEM".to_string());
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring provider supports default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Server-certificate verifier that accepts any presented chain. Mirror
/// of the interp-tier verifier in `gossamer-std/src/net.rs` so an
/// insecure upgrade behaves identically on every execution tier.
#[derive(Debug)]
struct NoCertVerify(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for NoCertVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// `net::TcpListener::bind(addr) -> Result<TcpListener, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_listener_bind(addr: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let a = cstr_to_str(addr);
        match crate::listen::bind_tcp(&a) {
            Ok(l) => {
                let h = next_handle();
                TCP_LISTENERS
                    .lock()
                    .get_or_insert_with(HashMap::new)
                    .insert(h, Arc::new(l));
                super::vec::gos_rt_result_new(0, h)
            }
            Err(e) => tcp_err(&socket_error(&e, &a)),
        }
    })
}

/// `net::TcpListener::accept(handle) -> Result<(TcpStream, String), Error>`.
/// The Ok payload is a heap `#[repr(C)] Pair { stream: i64, addr: i64 }` -
/// the 2-slot tuple `(TcpStream-handle, peer-address-string)` exactly
/// as `gos_rt_regex_find_opt` packs its triple.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_listener_accept(h: i64) -> i128 {
    ffi_entry!(0i128, {
        let Some(listener) = listener_clone(h) else {
            return tcp_err("TcpListener::accept: stale handle");
        };
        match listener.accept() {
            Ok((stream, peer)) => {
                // Nagle off, as Go leaves a `TCPConn`: a request/response
                // protocol writes a reply as a few small writes, and holding
                // the later ones until the peer acknowledges the first pairs
                // with the peer's delayed acknowledgement to cost a ~40 ms
                // stall per exchange. `set_nodelay` turns it back on.
                let _ = stream.set_nodelay(true);
                let sh = insert_stream(stream);
                let addr_cs = super::string::alloc_cstring(peer.to_string().as_bytes());
                #[repr(C)]
                struct Pair {
                    stream: i64,
                    addr: i64,
                }
                let pair = Box::into_raw(Box::new(Pair {
                    stream: sh,
                    addr: addr_cs as i64,
                }));
                super::vec::gos_rt_result_new(0, pair as i64)
            }
            Err(e) => tcp_err(&socket_error(&e, "accept")),
        }
    })
}

/// `net::TcpListener::local_addr(handle) -> Result<String, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_listener_local_addr(h: i64) -> i128 {
    ffi_entry!(0i128, {
        let Some(listener) = listener_clone(h) else {
            return tcp_err("TcpListener::local_addr: stale handle");
        };
        match listener.local_addr() {
            Ok(a) => super::vec::gos_rt_result_new(
                0,
                super::string::alloc_cstring(a.to_string().as_bytes()) as i64,
            ),
            Err(e) => tcp_err(&socket_error(&e, "local_addr")),
        }
    })
}

/// `net::TcpListener::close(handle)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_listener_close(h: i64) {
    ffi_entry!((), {
        if let Some(m) = TCP_LISTENERS.lock().as_mut() {
            m.remove(&h);
        }
    });
}

/// `net::TcpStream::connect(addr) -> Result<TcpStream, Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_connect(addr: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let a = cstr_to_str(addr);
        let dialed = a.clone();
        match crate::sched_global::run_blocking("tcp-connect", move || TcpStream::connect(&a)) {
            Ok(Ok(s)) => {
                // Nagle off by default, matching an accepted stream and Go's
                // own `TCPConn`; see `gos_rt_tcp_listener_accept`.
                let _ = s.set_nodelay(true);
                super::vec::gos_rt_result_new(0, insert_stream(s))
            }
            Ok(Err(e)) => tcp_err(&socket_error(&e, &dialed)),
            Err(e) => tcp_err(&e),
        }
    })
}

/// `net::TcpStream::read(handle, max) -> Result<[u8], Error>`. One read,
/// up to `max` bytes (clamped to a 16 MiB ceiling, matching the VM).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_read(h: i64, max: i64) -> i128 {
    ffi_entry!(0i128, {
        let cap = max.clamp(1, 1 << 24) as usize;
        let buf = if let Some(tls) = tls_clone(h) {
            match crate::sched_global::run_blocking("tls-stream-read", move || {
                let mut buf = vec![0u8; cap];
                let mut guard = tls.lock();
                guard.read(&mut buf).map(|n| {
                    buf.truncate(n);
                    buf
                })
            }) {
                Ok(Ok(buf)) => buf,
                Ok(Err(e)) => return tcp_err(&socket_error(&e, "TlsStream::read")),
                Err(e) => return tcp_err(&e),
            }
        } else if let Some(stream) = stream_clone(h) {
            let mut buf = vec![0u8; cap];
            let mut reader: &TcpStream = &stream;
            match reader.read(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    buf
                }
                Err(e) => return tcp_err(&transfer_error(&e, "TcpStream::read", "read timed out")),
            }
        } else {
            return tcp_err("TcpStream::read: stale handle");
        };
        super::vec::gos_rt_result_new(0, super::encoding::bytes_to_gosvec(&buf) as i64)
    })
}

/// `net::TcpStream::read_into(handle, buf, max) -> Result<i64, Error>`.
///
/// Reads one chunk of at most `max` bytes into the buffer's own storage and
/// sets its length to the byte count, so a connection loop reuses one buffer
/// instead of paying an allocation per chunk. `Ok(0)` is the peer's clean
/// close. The buffer grows only when it cannot already hold `max`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_read_into(h: i64, buf: *mut GosVec, max: i64) -> i128 {
    ffi_entry!(0i128, {
        if buf.is_null() {
            return tcp_err("TcpStream::read_into: null buffer");
        }
        if max <= 0 {
            return tcp_err("TcpStream::read_into: size must be positive");
        }
        let cap = max.min(1 << 24);
        unsafe { super::vec::gos_rt_vec_reserve_at_least(buf, cap) };
        let dst = unsafe { (*buf).ptr.as_ptr() };
        if dst.is_null() {
            return tcp_err("TcpStream::read_into: buffer has no storage");
        }
        // The read fills one byte per element, which is the packed layout a
        // `Vec<u8>` has; a buffer whose slots are wider names a different
        // element type than this read can fill.
        if unsafe { (*buf).elem_bytes } != 1 {
            return tcp_err("TcpStream::read_into: buffer must be a Vec<u8>");
        }
        let slot = unsafe { std::slice::from_raw_parts_mut(dst, cap as usize) };
        let read = if let Some(tls) = tls_clone(h) {
            let window = cap as usize;
            match crate::sched_global::run_blocking("tls-stream-read-into", move || {
                let mut owned = vec![0u8; window];
                let mut guard = tls.lock();
                guard.read(&mut owned).map(|n| {
                    owned.truncate(n);
                    owned
                })
            }) {
                Ok(Ok(bytes)) => {
                    slot[..bytes.len()].copy_from_slice(&bytes);
                    bytes.len()
                }
                Ok(Err(e)) => return tcp_err(&socket_error(&e, "TlsStream::read_into")),
                Err(e) => return tcp_err(&e),
            }
        } else if let Some(stream) = stream_clone(h) {
            let mut reader: &TcpStream = &stream;
            match reader.read(slot) {
                Ok(n) => n,
                Err(e) => {
                    return tcp_err(&transfer_error(
                        &e,
                        "TcpStream::read_into",
                        "read timed out",
                    ));
                }
            }
        } else {
            return tcp_err("TcpStream::read_into: stale handle");
        };
        unsafe { (*buf).len = read as i64 };
        super::vec::gos_rt_result_new(0, read as i64)
    })
}

/// `net::TcpStream::read_to_string(handle) -> Result<String, Error>`.
/// Reads until the peer closes (EOF); UTF-8-lossy, matching the VM.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_read_to_string(h: i64) -> i128 {
    ffi_entry!(0i128, {
        let mut out = Vec::new();
        let mut chunk = [0u8; 4096];
        if let Some(tls) = tls_clone(h) {
            let result = crate::sched_global::run_blocking("tls-stream-read-string", move || {
                let mut guard = tls.lock();
                loop {
                    match guard.read(&mut chunk) {
                        Ok(0) => break Ok(out),
                        Ok(n) => out.extend_from_slice(&chunk[..n]),
                        Err(e) => break Err(e),
                    }
                }
            });
            match result {
                Ok(Ok(bytes)) => out = bytes,
                Ok(Err(e)) => return tcp_err(&socket_error(&e, "TlsStream::read")),
                Err(e) => return tcp_err(&e),
            }
        } else if let Some(stream) = stream_clone(h) {
            let mut reader: &TcpStream = &stream;
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => out.extend_from_slice(&chunk[..n]),
                    Err(e) => {
                        return tcp_err(&transfer_error(&e, "TcpStream::read", "read timed out"));
                    }
                }
            }
        } else {
            return tcp_err("TcpStream::read_to_string: stale handle");
        }
        let s = String::from_utf8_lossy(&out);
        super::vec::gos_rt_result_new(0, super::string::alloc_cstring(s.as_bytes()) as i64)
    })
}

/// `net::TcpStream::write(handle, data: [u8]) -> Result<i64, Error>`.
/// `write_all`s the byte vector and returns the byte count. The MIR
/// dispatch coerces a `String` / byte-array-literal argument to the
/// `Vec<u8>` ABI before the call (see the delta report).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_write(h: i64, data: *const super::vec::GosVec) -> i128 {
    ffi_entry!(0i128, {
        let bytes = unsafe { super::encoding::gosvec_u8(data) };
        let bytes_len = bytes.len() as i64;
        if let Some(tls) = tls_clone(h) {
            // rustls buffers plaintext until flushed; flush so the
            // encrypted record reaches the peer before the call returns.
            match crate::sched_global::run_blocking("tls-stream-write", move || {
                let mut guard = tls.lock();
                guard
                    .write_all(&bytes)
                    .map_err(|e| socket_error(&e, "TlsStream::write"))
                    .and_then(|()| {
                        guard
                            .flush()
                            .map_err(|e| socket_error(&e, "TlsStream::flush"))
                    })
            }) {
                Ok(Ok(())) => super::vec::gos_rt_result_new(0, bytes_len),
                Ok(Err(msg)) => tcp_err(&msg),
                Err(e) => tcp_err(&e),
            }
        } else if let Some(stream) = stream_clone(h) {
            let mut writer: &TcpStream = &stream;
            match writer.write_all(&bytes) {
                Ok(()) => super::vec::gos_rt_result_new(0, bytes.len() as i64),
                Err(e) => tcp_err(&transfer_error(
                    &e,
                    "TcpStream::write_all",
                    "write timed out",
                )),
            }
        } else {
            tcp_err("TcpStream::write: stale handle")
        }
    })
}

fn timeout_duration(ms: i64) -> Option<Duration> {
    if ms <= 0 {
        None
    } else {
        Some(Duration::from_millis(ms as u64))
    }
}

/// `net::TcpStream::set_read_timeout_ms(handle, ms) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_set_read_timeout_ms(h: i64, ms: i64) -> i128 {
    ffi_entry!(0i128, {
        let timeout = timeout_duration(ms);
        if let Some(tls) = tls_clone(h) {
            match tls.lock().sock.set_read_timeout(timeout) {
                Ok(()) => super::vec::gos_rt_result_new(0, 0),
                Err(e) => tcp_err(&socket_error(&e, "TlsStream::set_read_timeout")),
            }
        } else if let Some(stream) = stream_clone(h) {
            match stream.set_read_timeout(timeout) {
                Ok(()) => super::vec::gos_rt_result_new(0, 0),
                Err(e) => tcp_err(&socket_error(&e, "TcpStream::set_read_timeout")),
            }
        } else {
            tcp_err("TcpStream::set_read_timeout_ms: stale handle")
        }
    })
}

/// `net::TcpStream::set_write_timeout_ms(handle, ms) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_set_write_timeout_ms(h: i64, ms: i64) -> i128 {
    ffi_entry!(0i128, {
        let timeout = timeout_duration(ms);
        if let Some(tls) = tls_clone(h) {
            match tls.lock().sock.set_write_timeout(timeout) {
                Ok(()) => super::vec::gos_rt_result_new(0, 0),
                Err(e) => tcp_err(&socket_error(&e, "TlsStream::set_write_timeout")),
            }
        } else if let Some(stream) = stream_clone(h) {
            match stream.set_write_timeout(timeout) {
                Ok(()) => super::vec::gos_rt_result_new(0, 0),
                Err(e) => tcp_err(&socket_error(&e, "TcpStream::set_write_timeout")),
            }
        } else {
            tcp_err("TcpStream::set_write_timeout_ms: stale handle")
        }
    })
}

/// `net::TcpStream::set_nodelay(handle, on) -> Result<(), Error>`.
///
/// A stream arrives with Nagle already off, so this is for the caller that
/// wants it back on: a bulk writer sending many small pieces of one payload,
/// where coalescing them costs nothing and saves packets.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_set_nodelay(h: i64, on: i64) -> i128 {
    ffi_entry!(0i128, {
        let on = on != 0;
        if let Some(tls) = tls_clone(h) {
            match tls.lock().sock.set_nodelay(on) {
                Ok(()) => super::vec::gos_rt_result_new(0, 0),
                Err(e) => tcp_err(&socket_error(&e, "TlsStream::set_nodelay")),
            }
        } else if let Some(stream) = stream_clone(h) {
            match stream.set_nodelay(on) {
                Ok(()) => super::vec::gos_rt_result_new(0, 0),
                Err(e) => tcp_err(&socket_error(&e, "TcpStream::set_nodelay")),
            }
        } else {
            tcp_err("TcpStream::set_nodelay: stale handle")
        }
    })
}

/// `net::TcpStream::clear_read_timeout(handle) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_clear_read_timeout(h: i64) -> i128 {
    unsafe { gos_rt_tcp_stream_set_read_timeout_ms(h, 0) }
}

/// `net::TcpStream::clear_write_timeout(handle) -> Result<(), Error>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_clear_write_timeout(h: i64) -> i128 {
    unsafe { gos_rt_tcp_stream_set_write_timeout_ms(h, 0) }
}

/// `net::TcpStream::close(handle)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_tcp_stream_close(h: i64) {
    ffi_entry!((), {
        if let Some(m) = TCP_STREAMS.lock().as_mut() {
            m.remove(&h);
        }
        if let Some(m) = TLS_STREAMS.lock().as_mut() {
            m.remove(&h);
        }
    });
}

/// `smtp::send(addr, from, to, subject, body)
/// -> Result<(), errors::Error>` - one message, unauthenticated.
#[cfg(not(target_arch = "wasm32"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_smtp_send(
    addr: *const c_char,
    from: *const c_char,
    to: *const c_char,
    subject: *const c_char,
    body: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        unsafe { smtp_send(addr, from, to, subject, body, None) }
    })
}

/// `smtp::send_auth(addr, from, to, subject, body, username, password)
/// -> Result<(), errors::Error>` - the shape a transactional mail provider
/// takes. Credentials are only ever sent over TLS.
#[cfg(not(target_arch = "wasm32"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_smtp_send_auth(
    addr: *const c_char,
    from: *const c_char,
    to: *const c_char,
    subject: *const c_char,
    body: *const c_char,
    username: *const c_char,
    password: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        let credentials = unsafe { (cstr_or_empty(username), cstr_or_empty(password)) };
        unsafe { smtp_send(addr, from, to, subject, body, Some(credentials)) }
    })
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn cstr_or_empty(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { crate::c_abi::gos_str_arg_string(p) }
    }
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn smtp_send(
    addr: *const c_char,
    from: *const c_char,
    to: *const c_char,
    subject: *const c_char,
    body: *const c_char,
    credentials: Option<(String, String)>,
) -> i128 {
    let (addr, from, to, subject, body) = unsafe {
        (
            cstr_or_empty(addr),
            cstr_or_empty(from),
            cstr_or_empty(to),
            cstr_or_empty(subject),
            cstr_or_empty(body),
        )
    };
    let message = crate::smtp::Message {
        from: &from,
        to: &to,
        subject: &subject,
        body: &body,
    };
    let credentials = credentials
        .as_ref()
        .map(|(username, password)| crate::smtp::Credentials { username, password });
    match crate::smtp::send(&addr, &message, credentials.as_ref()) {
        Ok(()) => crate::c_abi::vec::pack_result(0, 0),
        Err(message) => {
            let err = crate::c_abi::errors::error_new_from_bytes(message.as_bytes());
            crate::c_abi::vec::pack_result(1, err as i64)
        }
    }
}
