//! Runtime support for `std::net`.
//!
//! TCP listener / stream and UDP socket types. Two execution paths
//! are exposed through the same API surface:
//!
//! - **Default (poller-aware)**: socket FDs are set non-blocking and
//!   registered with the global netpoller. A read / write that would
//!   block parks the calling goroutine on a waker; the poller wakes
//!   it when the kernel reports readiness. This is the production
//!   path.
//! - **Blocking fallback**: if the global poller cannot be reached
//!   (e.g. unit tests, single-threaded harnesses), the call falls
//!   back to a plain blocking `std::io::Read`/`Write`.
//!
//! Both paths are observably identical from user code; the blocking
//! fallback is the floor when the runtime is not wired up.

#![forbid(unsafe_code)]
#![allow(
    clippy::similar_names,
    clippy::items_after_statements,
    clippy::needless_continue,
    clippy::let_and_return,
    clippy::missing_errors_doc,
    clippy::redundant_closure,
    clippy::redundant_closure_for_method_calls,
    clippy::manual_let_else,
    clippy::doc_markdown,
    clippy::io_other_error,
    clippy::cast_possible_truncation,
    clippy::unchecked_time_subtraction,
    clippy::missing_const_for_fn,
    clippy::checked_conversions
)]

use std::io::{self, ErrorKind, Read, Write};
use std::net::{
    SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream, ToSocketAddrs,
    UdpSocket as StdUdpSocket,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use gossamer_runtime::platform::Instant;
use gossamer_sched::Interest;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme, StreamOwned,
};

use crate::io::IoError;
use crate::sched_global;

/// Bound TCP listener.
#[derive(Debug)]
pub struct TcpListener {
    inner: StdTcpListener,
    mio: Option<mio::net::TcpListener>,
}

/// Milliseconds a deadline is stored as; 0 means unbounded.
fn duration_millis(timeout: Option<Duration>) -> u64 {
    timeout.map_or(0, |d| d.as_millis().max(1).min(u128::from(u64::MAX)) as u64)
}

impl TcpListener {
    /// Binds the listener to `addr`.
    pub fn bind(addr: &str) -> Result<Self, IoError> {
        let inner =
            gossamer_runtime::listen::bind_tcp(addr).map_err(|e| IoError::from_std(e, addr))?;
        inner
            .set_nonblocking(true)
            .map_err(|e| IoError::from_std(e, addr))?;
        // Build the mio mirror by stealing the FD via try_clone +
        // into. mio::net::TcpListener::from_std requires a
        // non-blocking std listener.
        let mirror = inner.try_clone().map(mio::net::TcpListener::from_std).ok();
        Ok(Self { inner, mio: mirror })
    }

    /// Returns the bound local address.
    pub fn local_addr(&self) -> Result<SocketAddr, IoError> {
        self.inner
            .local_addr()
            .map_err(|e| IoError::from_std(e, "local_addr"))
    }

    /// Accepts a single incoming connection. Parks the caller on the
    /// poller when no connection is currently pending.
    pub fn accept(&mut self) -> Result<(TcpStream, SocketAddr), IoError> {
        loop {
            match self.inner.accept() {
                Ok((stream, addr)) => {
                    stream
                        .set_nonblocking(true)
                        .map_err(|e| IoError::from_std(e, "accept"))?;
                    return Ok((TcpStream::from_std(stream)?, addr));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.wait_readable()?;
                }
                Err(e) => return Err(IoError::from_std(e, "accept")),
            }
        }
    }

    /// Cancellation-aware variant of [`Self::accept`].
    pub fn accept_ctx(
        &mut self,
        ctx: &crate::context::Context,
    ) -> Result<(TcpStream, SocketAddr), IoError> {
        if let Some(err) = ctx.err() {
            return Err(IoError::cancelled(err));
        }
        let gid = crate::sched_global::current_gid();
        if let Some(g) = gid {
            ctx.register_waiter(g);
        }
        loop {
            match self.inner.accept() {
                Ok((stream, addr)) => {
                    if let Some(g) = gid {
                        ctx.deregister_waiter(g);
                    }
                    stream
                        .set_nonblocking(true)
                        .map_err(|e| IoError::from_std(e, "accept_ctx"))?;
                    return Ok((TcpStream::from_std(stream)?, addr));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if ctx.is_cancelled() {
                        if let Some(g) = gid {
                            ctx.deregister_waiter(g);
                        }
                        return Err(IoError::cancelled(
                            ctx.err()
                                .unwrap_or_else(|| crate::errors::Error::new("context cancelled")),
                        ));
                    }
                    self.wait_readable()?;
                }
                Err(e) => {
                    if let Some(g) = gid {
                        ctx.deregister_waiter(g);
                    }
                    return Err(IoError::from_std(e, "accept_ctx"));
                }
            }
        }
    }

    fn wait_readable(&mut self) -> Result<(), IoError> {
        let Some(mio_handle) = self.mio.as_mut() else {
            gossamer_runtime::platform::sleep(Duration::from_millis(1));
            return Ok(());
        };
        sched_global::wait_io(mio_handle, Interest::Readable)
            .map_err(|e| IoError::from_std(e, "poller wait"))
    }
}

/// Connected TCP byte stream.
#[derive(Debug)]
pub struct TcpStream {
    inner: StdTcpStream,
    mio: Option<mio::net::TcpStream>,
    // The socket is non-blocking so the poller can park a goroutine on it,
    // which makes `SO_RCVTIMEO` / `SO_SNDTIMEO` unreachable: a read returns
    // `WouldBlock` at once and the deadline the kernel holds never fires. The
    // deadline a caller sets is therefore kept here and applied to the park.
    read_timeout: Arc<AtomicU64>,
    write_timeout: Arc<AtomicU64>,
}

impl TcpStream {
    /// Connects to `addr`.
    pub fn connect(addr: &str) -> Result<Self, IoError> {
        let inner = StdTcpStream::connect(addr).map_err(|e| IoError::from_std(e, addr))?;
        Self::from_std(inner)
    }

    fn from_std(inner: StdTcpStream) -> Result<Self, IoError> {
        inner
            .set_nonblocking(true)
            .map_err(|e| IoError::from_std(e, "set_nonblocking"))?;
        let mirror = inner.try_clone().map(mio::net::TcpStream::from_std).ok();
        Ok(Self {
            inner,
            mio: mirror,
            read_timeout: Arc::new(AtomicU64::new(0)),
            write_timeout: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Duplicates the underlying socket while preserving this wrapper's
    /// netpoller-aware nonblocking configuration and its deadlines - the
    /// clone reads the same socket, so it waits on the same terms.
    pub fn try_clone(&self) -> Result<Self, IoError> {
        let mut cloned = self
            .inner
            .try_clone()
            .map_err(|e| IoError::from_std(e, "TcpStream::try_clone"))
            .and_then(Self::from_std)?;
        cloned.read_timeout = Arc::clone(&self.read_timeout);
        cloned.write_timeout = Arc::clone(&self.write_timeout);
        Ok(cloned)
    }

    /// Sets the read deadline.
    ///
    /// The socket is non-blocking so a goroutine can park on the poller
    /// rather than hold a thread, which puts `SO_RCVTIMEO` out of reach: the
    /// read returns `WouldBlock` at once and the kernel's own deadline never
    /// fires. The option is still set for a caller that takes the socket
    /// blocking, and the duration is kept here so [`Self::read`] bounds its
    /// park by it.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.read_timeout
            .store(duration_millis(timeout), Ordering::Relaxed);
        self.inner
            .set_read_timeout(timeout)
            .map_err(|e| IoError::from_std(e, "TcpStream::set_read_timeout"))
    }

    /// Sets the write deadline; see [`Self::set_read_timeout`].
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.write_timeout
            .store(duration_millis(timeout), Ordering::Relaxed);
        self.inner
            .set_write_timeout(timeout)
            .map_err(|e| IoError::from_std(e, "TcpStream::set_write_timeout"))
    }

    /// The deadline a wait should not run past, or `None` when unbounded.
    fn deadline(slot: &AtomicU64) -> Option<Instant> {
        match slot.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(Instant::now() + Duration::from_millis(ms)),
        }
    }

    /// Parks until `interest` is ready or `deadline` passes; `Ok(false)`
    /// means the deadline won.
    fn wait_io_deadline(
        &mut self,
        interest: Interest,
        deadline: Option<Instant>,
    ) -> Result<bool, IoError> {
        let Some(deadline) = deadline else {
            self.wait_io(interest)?;
            return Ok(true);
        };
        let Some(mio_handle) = self.mio.as_mut() else {
            gossamer_runtime::platform::sleep(Duration::from_millis(1));
            return Ok(Instant::now() < deadline);
        };
        sched_global::wait_io_until(mio_handle, interest, deadline)
            .map_err(|e| IoError::from_std(e, "poller wait"))
    }

    /// Convenience wrapper for Gossamer code: non-positive values clear.
    pub fn set_read_timeout_ms(&self, ms: i64) -> Result<(), IoError> {
        self.set_read_timeout((ms > 0).then(|| Duration::from_millis(ms as u64)))
    }

    /// Convenience wrapper for Gossamer code: non-positive values clear.
    pub fn set_write_timeout_ms(&self, ms: i64) -> Result<(), IoError> {
        self.set_write_timeout((ms > 0).then(|| Duration::from_millis(ms as u64)))
    }

    /// Sets whether small writes go out immediately rather than being coalesced.
    ///
    /// A stream arrives with this on, so passing `false` is what asks for
    /// Nagle's algorithm back.
    pub fn set_nodelay(&self, on: bool) -> Result<(), IoError> {
        self.inner
            .set_nodelay(on)
            .map_err(|e| IoError::from_std(e, "TcpStream::set_nodelay"))
    }

    /// Clears the read timeout.
    pub fn clear_read_timeout(&self) -> Result<(), IoError> {
        self.set_read_timeout(None)
    }

    /// Clears the write timeout.
    pub fn clear_write_timeout(&self) -> Result<(), IoError> {
        self.set_write_timeout(None)
    }

    /// Reads up to `buf.len()` bytes into `buf`. Parks the caller on
    /// the poller while the kernel buffer is empty.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        let deadline = Self::deadline(&self.read_timeout);
        loop {
            match self.inner.read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if !self.wait_io_deadline(Interest::Readable, deadline)? {
                        return Err(IoError::from_std(
                            io::Error::new(ErrorKind::WouldBlock, "read timed out"),
                            "TcpStream::read",
                        ));
                    }
                }
                Err(e) => return Err(IoError::from_std(e, "TcpStream::read")),
            }
        }
    }

    /// Reads one chunk of at most `max` bytes into `buf`, replacing its
    /// contents and answering the byte count. `Ok(0)` is a clean close.
    ///
    /// The buffer's own storage is what the read fills, so a connection loop
    /// that keeps one buffer pays no allocation per chunk.
    pub fn read_into(&mut self, buf: &mut Vec<u8>, max: usize) -> Result<usize, IoError> {
        buf.resize(max, 0);
        let n = self.read(&mut buf[..max])?;
        buf.truncate(n);
        Ok(n)
    }

    /// Cancellation-aware variant of [`Self::read`].
    ///
    /// Loops through the same non-blocking-read / poll-park
    /// cycle as `read`, but checks `ctx.is_cancelled()` on
    /// every iteration (and registers the current goroutine
    /// with the context's wait-list so `Cancel::cancel_with`
    /// can wake it out of the poller park). Returns
    /// `IoError::cancelled(ctx.err())` when the context fires
    /// before any data arrives.
    pub fn read_ctx(
        &mut self,
        ctx: &crate::context::Context,
        buf: &mut [u8],
    ) -> Result<usize, IoError> {
        if let Some(err) = ctx.err() {
            return Err(IoError::cancelled(err));
        }
        let gid = crate::sched_global::current_gid();
        if let Some(g) = gid {
            ctx.register_waiter(g);
        }
        let deadline = Self::deadline(&self.read_timeout);
        loop {
            match self.inner.read(buf) {
                Ok(n) => {
                    if let Some(g) = gid {
                        ctx.deregister_waiter(g);
                    }
                    return Ok(n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // Cancellation check on each loop iteration:
                    // if cancel fired between the previous park
                    // unblock and now, return Err without
                    // re-parking.
                    if ctx.is_cancelled() {
                        if let Some(g) = gid {
                            ctx.deregister_waiter(g);
                        }
                        return Err(IoError::cancelled(
                            ctx.err()
                                .unwrap_or_else(|| crate::errors::Error::new("context cancelled")),
                        ));
                    }
                    if !self.wait_io_deadline(Interest::Readable, deadline)? {
                        if let Some(g) = gid {
                            ctx.deregister_waiter(g);
                        }
                        return Err(IoError::from_std(
                            io::Error::new(ErrorKind::WouldBlock, "read timed out"),
                            "TcpStream::read_ctx",
                        ));
                    }
                }
                Err(e) => {
                    if let Some(g) = gid {
                        ctx.deregister_waiter(g);
                    }
                    return Err(IoError::from_std(e, "TcpStream::read_ctx"));
                }
            }
        }
    }

    /// Writes every byte in `buf`.
    pub fn write_all(&mut self, buf: &[u8]) -> Result<(), IoError> {
        let write_deadline = Self::deadline(&self.write_timeout);
        let mut written = 0;
        while written < buf.len() {
            match self.inner.write(&buf[written..]) {
                Ok(0) => {
                    return Err(IoError::from_std(
                        io::Error::new(ErrorKind::WriteZero, "wrote zero bytes"),
                        "TcpStream::write_all",
                    ));
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if !self.wait_io_deadline(Interest::Writable, write_deadline)? {
                        return Err(IoError::from_std(
                            io::Error::new(ErrorKind::WouldBlock, "write timed out"),
                            "TcpStream::write_all",
                        ));
                    }
                }
                Err(e) => return Err(IoError::from_std(e, "TcpStream::write_all")),
            }
        }
        Ok(())
    }

    /// Cancellation-aware variant of [`Self::write_all`].
    pub fn write_all_ctx(
        &mut self,
        ctx: &crate::context::Context,
        buf: &[u8],
    ) -> Result<(), IoError> {
        if let Some(err) = ctx.err() {
            return Err(IoError::cancelled(err));
        }
        let gid = crate::sched_global::current_gid();
        if let Some(g) = gid {
            ctx.register_waiter(g);
        }
        let deadline = Self::deadline(&self.write_timeout);
        let mut written = 0;
        while written < buf.len() {
            match self.inner.write(&buf[written..]) {
                Ok(0) => {
                    if let Some(g) = gid {
                        ctx.deregister_waiter(g);
                    }
                    return Err(IoError::from_std(
                        io::Error::new(ErrorKind::WriteZero, "wrote zero bytes"),
                        "TcpStream::write_all_ctx",
                    ));
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if ctx.is_cancelled() {
                        if let Some(g) = gid {
                            ctx.deregister_waiter(g);
                        }
                        return Err(IoError::cancelled(
                            ctx.err()
                                .unwrap_or_else(|| crate::errors::Error::new("context cancelled")),
                        ));
                    }
                    if !self.wait_io_deadline(Interest::Writable, deadline)? {
                        if let Some(g) = gid {
                            ctx.deregister_waiter(g);
                        }
                        return Err(IoError::from_std(
                            io::Error::new(ErrorKind::WouldBlock, "write timed out"),
                            "TcpStream::write_all_ctx",
                        ));
                    }
                }
                Err(e) => {
                    if let Some(g) = gid {
                        ctx.deregister_waiter(g);
                    }
                    return Err(IoError::from_std(e, "TcpStream::write_all_ctx"));
                }
            }
        }
        if let Some(g) = gid {
            ctx.deregister_waiter(g);
        }
        Ok(())
    }

    fn wait_io(&mut self, interest: Interest) -> Result<(), IoError> {
        let Some(mio_handle) = self.mio.as_mut() else {
            gossamer_runtime::platform::sleep(Duration::from_millis(1));
            return Ok(());
        };
        sched_global::wait_io(mio_handle, interest).map_err(|e| IoError::from_std(e, "poller wait"))
    }

    /// Adopts a blocking `std::net::TcpStream` (e.g. one
    /// returned by `std::net::TcpListener::accept`) and wraps
    /// it in the netpoller-aware shell. Used by the h2c
    /// debug entrypoint that does its own blocking accept.
    pub fn from_std_blocking(stream: StdTcpStream) -> Result<Self, IoError> {
        Self::from_std(stream)
    }

    /// Single non-blocking read attempt. Returns `Err` with
    /// `ErrorKind::WouldBlock` when no bytes are immediately
    /// available. Used by the async bridge in
    /// `crate::async_tcp` to integrate with futures-shaped APIs
    /// without consuming a goroutine slot via `wait_io`.
    pub fn try_read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }

    /// Single non-blocking write attempt. Returns `Err` with
    /// `ErrorKind::WouldBlock` when the kernel buffer is full.
    pub fn try_write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    /// Registers this stream's underlying file descriptor with
    /// the netpoller for `interest` events tagged with `gid`.
    /// The caller is responsible for installing a waker via
    /// [`crate::sched_global::register_waker`] before calling
    /// this so the wakeup has somewhere to land.
    pub fn register_with_poller(
        &mut self,
        interest: Interest,
        gid: gossamer_runtime::sched::Gid,
    ) -> std::io::Result<()> {
        let Some(mio_handle) = self.mio.as_mut() else {
            return Err(std::io::Error::other("stream has no mio handle"));
        };
        sched_global::with_poller(|p| p.register_io(mio_handle, interest, gid))?;
        Ok(())
    }
}

/// A TLS client stream layered over a blocking TCP socket.
///
/// Produced by [`TcpStream::start_tls`] once any plaintext
/// pre-handshake (e.g. PostgreSQL's `SSLRequest`) is complete. The
/// underlying socket is switched to blocking mode and left out of the
/// netpoller: rustls drives the handshake and record layer
/// synchronously, and the goroutine scheduler tolerates the parked OS
/// thread.
pub struct TlsStream {
    inner: StreamOwned<ClientConnection, StdTcpStream>,
}

impl TlsStream {
    /// The DER bytes of the end-entity certificate the peer presented, or
    /// an empty vector when it sent none.
    ///
    /// This is what SCRAM's `tls-server-end-point` channel binding hashes,
    /// so a client can prove its authentication exchange and its TLS
    /// connection are the same one. The bytes are handed over unhashed:
    /// which digest the binding calls for is the caller's rule.
    pub fn peer_certificate(&mut self) -> Vec<u8> {
        // rustls hands the certificate over only once the handshake it was
        // presented in has finished, and a session opens without any I/O -
        // so drive the handshake to completion before reading it, rather
        // than reporting "no certificate" for a peer that sent one.
        while self.inner.conn.is_handshaking() {
            if self.inner.conn.complete_io(&mut self.inner.sock).is_err() {
                break;
            }
        }
        self.inner
            .conn
            .peer_certificates()
            .and_then(<[_]>::first)
            .map(|cert| cert.as_ref().to_vec())
            .unwrap_or_default()
    }

    /// Sets the per-syscall read timeout on the underlying socket.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.inner
            .sock
            .set_read_timeout(timeout)
            .map_err(|e| IoError::from_std(e, "TlsStream::set_read_timeout"))
    }

    /// Sets the per-syscall write timeout on the underlying socket.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> Result<(), IoError> {
        self.inner
            .sock
            .set_write_timeout(timeout)
            .map_err(|e| IoError::from_std(e, "TlsStream::set_write_timeout"))
    }

    /// Convenience wrapper for Gossamer code: non-positive values clear.
    pub fn set_read_timeout_ms(&self, ms: i64) -> Result<(), IoError> {
        self.set_read_timeout((ms > 0).then(|| Duration::from_millis(ms as u64)))
    }

    /// Convenience wrapper for Gossamer code: non-positive values clear.
    pub fn set_write_timeout_ms(&self, ms: i64) -> Result<(), IoError> {
        self.set_write_timeout((ms > 0).then(|| Duration::from_millis(ms as u64)))
    }

    /// Sets whether small writes go out immediately; see
    /// [`TcpStream::set_nodelay`].
    pub fn set_nodelay(&self, on: bool) -> Result<(), IoError> {
        self.inner
            .sock
            .set_nodelay(on)
            .map_err(|e| IoError::from_std(e, "TlsStream::set_nodelay"))
    }

    /// Clears the read timeout.
    pub fn clear_read_timeout(&self) -> Result<(), IoError> {
        self.set_read_timeout(None)
    }

    /// Clears the write timeout.
    pub fn clear_write_timeout(&self) -> Result<(), IoError> {
        self.set_write_timeout(None)
    }

    /// Reads up to `buf.len()` decrypted bytes; `Ok(0)` is a clean close.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        self.inner
            .read(buf)
            .map_err(|e| IoError::from_std(e, "TlsStream::read"))
    }

    /// Encrypts and writes `buf` in full, flushing the record to the
    /// peer before returning.
    pub fn write_all(&mut self, buf: &[u8]) -> Result<(), IoError> {
        self.inner
            .write_all(buf)
            .map_err(|e| IoError::from_std(e, "TlsStream::write"))?;
        self.inner
            .flush()
            .map_err(|e| IoError::from_std(e, "TlsStream::flush"))
    }
}

impl TcpStream {
    /// Upgrades this connected stream to a TLS client session for
    /// `host`, consuming the plaintext stream. Verification uses the
    /// webpki root store - the same trust anchors as the HTTP client.
    /// The TLS equivalent of PostgreSQL `sslmode=verify-full` against a
    /// publicly-trusted CA.
    pub fn start_tls(self, host: &str) -> Result<TlsStream, IoError> {
        self.upgrade_tls(host, tls_client_config())
    }

    /// Upgrades this connected stream to a TLS client session for
    /// `host` without authenticating the peer certificate. The
    /// connection is encrypted but the server is not verified - the TLS
    /// equivalent of PostgreSQL `sslmode=require`. Prefer
    /// [`TcpStream::start_tls`] or [`TcpStream::start_tls_ca`] whenever
    /// the peer's trust anchor is known.
    pub fn start_tls_insecure(self, host: &str) -> Result<TlsStream, IoError> {
        self.upgrade_tls(host, tls_client_config_insecure())
    }

    /// Upgrades this connected stream to a TLS client session for
    /// `host`, verifying the server certificate chain and hostname
    /// against the PEM-encoded CA bundle in `ca_pem` instead of the
    /// bundled public roots - the TLS equivalent of PostgreSQL
    /// `sslmode=verify-full` against a private CA.
    pub fn start_tls_ca(self, host: &str, ca_pem: &str) -> Result<TlsStream, IoError> {
        let config = tls_client_config_ca(ca_pem.as_bytes())?;
        self.upgrade_tls(host, config)
    }

    /// Shared upgrade path: switch the socket to blocking, resolve the
    /// SNI hostname, and hand the stream to a rustls client session
    /// built from `config`.
    fn upgrade_tls(
        self,
        host: &str,
        config: Arc<rustls::ClientConfig>,
    ) -> Result<TlsStream, IoError> {
        self.inner
            .set_nonblocking(false)
            .map_err(|e| IoError::from_std(e, "start_tls: set_blocking"))?;
        let name = ServerName::try_from(host.to_string()).map_err(|_| {
            IoError::from_std(
                io::Error::other(format!("invalid server name `{host}`")),
                "start_tls",
            )
        })?;
        let conn = ClientConnection::new(config, name)
            .map_err(|e| IoError::from_std(io::Error::other(format!("{e}")), "start_tls"))?;
        Ok(TlsStream {
            inner: StreamOwned::new(conn, self.inner),
        })
    }
}

/// Shared TLS client config (ring provider, webpki roots, verification
/// always on) - identical trust anchors to the compiled-tier
/// `gos_rt_tcp_start_tls`, so an upgraded stream validates the same on
/// every execution tier.
fn tls_client_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring provider supports default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(config)
    });
    CONFIG.clone()
}

/// Client config that accepts any server certificate (PostgreSQL
/// `sslmode=require`). Built once and shared; verification is delegated
/// to [`NoCertVerify`], which still validates the handshake signature
/// against the presented key but skips chain and hostname checks.
fn tls_client_config_insecure() -> Arc<rustls::ClientConfig> {
    static CONFIG: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .expect("ring provider supports default protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertVerify(provider)))
            .with_no_client_auth();
        Arc::new(config)
    });
    CONFIG.clone()
}

/// Client config that validates the server certificate chain and
/// hostname against the CA bundle in `ca_pem` rather than the bundled
/// public roots (PostgreSQL `sslmode=verify-full` against a private CA).
fn tls_client_config_ca(ca_pem: &[u8]) -> Result<Arc<rustls::ClientConfig>, IoError> {
    let mut roots = RootCertStore::empty();
    let mut added = 0usize;
    for cert in CertificateDer::pem_slice_iter(ca_pem) {
        let cert = cert.map_err(|e| {
            IoError::from_std(io::Error::other(format!("{e}")), "start_tls_ca: parse CA")
        })?;
        roots.add(cert).map_err(|e| {
            IoError::from_std(io::Error::other(format!("{e}")), "start_tls_ca: add CA")
        })?;
        added += 1;
    }
    if added == 0 {
        return Err(IoError::from_std(
            io::Error::other("no certificates in CA PEM"),
            "start_tls_ca",
        ));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring provider supports default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Server-certificate verifier that accepts any presented chain. The
/// handshake signature is still checked against the presented key via
/// the crypto provider; only the chain-to-root and hostname checks are
/// skipped. Backs [`TcpStream::start_tls_insecure`].
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

/// Bound UDP socket.
#[derive(Debug)]
pub struct UdpSocket {
    inner: StdUdpSocket,
}

impl UdpSocket {
    /// Binds the socket to `addr`.
    pub fn bind(addr: &str) -> Result<Self, IoError> {
        let inner = StdUdpSocket::bind(addr).map_err(|e| IoError::from_std(e, addr))?;
        Ok(Self { inner })
    }

    /// Sends `buf` to `addr`, returning the number of bytes written.
    pub fn send_to(&self, buf: &[u8], addr: &str) -> Result<usize, IoError> {
        self.inner
            .send_to(buf, addr)
            .map_err(|e| IoError::from_std(e, addr))
    }

    /// Cancellation-aware variant of [`Self::send_to`]. The kernel UDP
    /// send path is essentially non-blocking, so cancellation is
    /// observed *before* the send attempt; the call itself does
    /// not park.
    pub fn send_to_ctx(
        &self,
        ctx: &crate::context::Context,
        buf: &[u8],
        addr: &str,
    ) -> Result<usize, IoError> {
        if let Some(err) = ctx.err() {
            return Err(IoError::cancelled(err));
        }
        self.inner
            .send_to(buf, addr)
            .map_err(|e| IoError::from_std(e, addr))
    }

    /// Receives a datagram, returning the length and source address.
    pub fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), IoError> {
        self.inner
            .recv_from(buf)
            .map_err(|e| IoError::from_std(e, "UdpSocket::recv_from"))
    }

    /// Cancellation-aware variant of [`Self::recv_from`].
    ///
    /// Sets a short `SO_RCVTIMEO` so each blocking syscall returns
    /// within 50ms, then re-checks `ctx.is_cancelled()` between
    /// attempts. Restores the original timeout (if any) on exit.
    pub fn recv_from_ctx(
        &self,
        ctx: &crate::context::Context,
        buf: &mut [u8],
    ) -> Result<(usize, SocketAddr), IoError> {
        if let Some(err) = ctx.err() {
            return Err(IoError::cancelled(err));
        }
        let gid = crate::sched_global::current_gid();
        if let Some(g) = gid {
            ctx.register_waiter(g);
        }
        let prior = self
            .inner
            .read_timeout()
            .map_err(|e| IoError::from_std(e, "UdpSocket::recv_from_ctx"))?;
        let slice = Duration::from_millis(50);
        let _ = self.inner.set_read_timeout(Some(slice));
        let result = loop {
            match self.inner.recv_from(buf) {
                Ok(v) => break Ok(v),
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    if ctx.is_cancelled() {
                        break Err(IoError::cancelled(ctx.err().unwrap_or_else(|| {
                            crate::errors::Error::new("context cancelled")
                        })));
                    }
                }
                Err(e) => break Err(IoError::from_std(e, "UdpSocket::recv_from_ctx")),
            }
        };
        let _ = self.inner.set_read_timeout(prior);
        if let Some(g) = gid {
            ctx.deregister_waiter(g);
        }
        result
    }

    /// Returns the bound local address.
    pub fn local_addr(&self) -> Result<SocketAddr, IoError> {
        self.inner
            .local_addr()
            .map_err(|e| IoError::from_std(e, "local_addr"))
    }
}

/// Resolves `host` to a list of socket addresses.
pub fn resolve(host: &str) -> Result<Vec<SocketAddr>, IoError> {
    let iter = host
        .to_socket_addrs()
        .map_err(|e| IoError::from_std(e, host))?;
    Ok(iter.collect())
}

impl TcpStream {
    /// Sets TCP keepalive on the stream. `None` disables.
    /// `Some(d)` enables with the system's default keepalive
    /// probing schedule (the per-OS knobs for `KEEPIDLE`,
    /// `KEEPINTVL`, `KEEPCNT` use platform defaults).
    pub fn set_keepalive(&self, dur: Option<Duration>) -> Result<(), IoError> {
        // std exposes only the boolean; the per-interval knobs
        // live behind `socket2`. For v1 we toggle the
        // boolean - fine for keep-the-connection-alive use
        // cases that don't need custom intervals.
        let on = dur.is_some();
        socket_option::set_keepalive(&self.inner, on)
            .map_err(|e| IoError::from_std(e, "set_keepalive"))
    }

    /// Happy-eyeballs connect: races IPv4 and IPv6 candidate
    /// addresses with the supplied stagger delay (Go 1.21
    /// behaviour). Returns the first successful connection.
    ///
    /// `addrs` is the list of candidate `SocketAddr`s (typically
    /// from [`resolve`]). `stagger` is the delay between
    /// successive connection attempts; Go uses 300 ms by
    /// default. Pass `Duration::ZERO` to attempt every address
    /// in parallel.
    pub fn connect_happy_eyeballs(
        addrs: &[SocketAddr],
        stagger: Duration,
        timeout: Duration,
    ) -> Result<Self, IoError> {
        let stream = connect_happy_eyeballs_std(addrs, stagger, timeout)
            .map_err(|e| IoError::from_std(e, "happy_eyeballs"))?;
        Self::from_std(stream)
    }
}

/// RFC 8305-style staggered parallel connect over `std::net::TcpStream`,
/// returning the first established connection. Shared by
/// [`TcpStream::connect_happy_eyeballs`] and the native HTTP client so both
/// fall through from an unreachable first address (commonly a filtered AAAA)
/// to the next candidate instead of stalling for the whole timeout.
pub(crate) fn connect_happy_eyeballs_std(
    addrs: &[SocketAddr],
    stagger: Duration,
    timeout: Duration,
) -> std::io::Result<StdTcpStream> {
    use std::sync::mpsc;
    if addrs.is_empty() {
        return Err(std::io::Error::new(ErrorKind::InvalidInput, "no addresses"));
    }
    // Interleave v6 and v4 candidates (Go's policy: prefer v6 first when
    // available, then alternate).
    let mut v6: Vec<SocketAddr> = addrs.iter().copied().filter(|a| a.is_ipv6()).collect();
    let mut v4: Vec<SocketAddr> = addrs.iter().copied().filter(|a| a.is_ipv4()).collect();
    let mut order: Vec<SocketAddr> = Vec::with_capacity(addrs.len());
    while !v6.is_empty() || !v4.is_empty() {
        if !v6.is_empty() {
            order.push(v6.remove(0));
        }
        if !v4.is_empty() {
            order.push(v4.remove(0));
        }
    }

    let (tx, rx) = mpsc::channel::<Result<StdTcpStream, std::io::Error>>();
    let per_attempt_timeout = timeout;
    let started = gossamer_runtime::platform::Instant::now();
    for (i, addr) in order.iter().copied().enumerate() {
        let stagger_for_i = stagger.checked_mul(i as u32).unwrap_or(Duration::ZERO);
        let tx_for_attempt = tx.clone();
        let started_for_attempt = started;
        std::thread::spawn(move || {
            if !stagger_for_i.is_zero() {
                gossamer_runtime::platform::sleep(stagger_for_i);
            }
            let elapsed = started_for_attempt.elapsed();
            if elapsed >= per_attempt_timeout {
                return;
            }
            let remaining = per_attempt_timeout - elapsed;
            let result = StdTcpStream::connect_timeout(&addr, remaining);
            let _ = tx_for_attempt.send(result);
        });
    }
    drop(tx);

    let deadline = started + timeout;
    loop {
        let now = gossamer_runtime::platform::Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "happy_eyeballs deadline",
            ));
        }
        match rx.recv_timeout(deadline - now) {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(_)) => continue, // try next
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::new(ErrorKind::Other, "all attempts failed"));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
}

/// Socket-option bridge using `socket2`. Wraps the std stream
/// FD without taking ownership via `SockRef`, which carries a
/// `PhantomData` borrow rather than duplicating the descriptor.
/// The previous `try_clone` + `Socket::from` + `drop` dance
/// triggered `setsockopt: EINVAL` on macOS for `SO_KEEPALIVE`
/// when the cloned-FD wrapper closed in the same syscall window,
/// even though the option itself is universally supported.
mod socket_option {
    use socket2::SockRef;
    use std::net::TcpStream;

    pub(super) fn set_keepalive(stream: &TcpStream, on: bool) -> std::io::Result<()> {
        SockRef::from(stream).set_keepalive(on)
    }
}

// --- Unix domain sockets ---------------------------------------------

/// Bound Unix domain socket listener.
#[cfg(unix)]
#[derive(Debug)]
pub struct UnixListener {
    inner: std::os::unix::net::UnixListener,
}

#[cfg(unix)]
impl UnixListener {
    /// Binds the listener to the filesystem `path`. The path
    /// must NOT already exist (Unix sockets do not auto-replace).
    pub fn bind(path: &str) -> Result<Self, IoError> {
        let inner =
            std::os::unix::net::UnixListener::bind(path).map_err(|e| IoError::from_std(e, path))?;
        Ok(Self { inner })
    }

    /// Accepts one connection.
    pub fn accept(&self) -> Result<UnixStream, IoError> {
        let (stream, _) = self
            .inner
            .accept()
            .map_err(|e| IoError::from_std(e, "UnixListener::accept"))?;
        Ok(UnixStream { inner: stream })
    }

    /// Returns the bound path.
    pub fn local_addr(&self) -> Result<String, IoError> {
        let addr = self
            .inner
            .local_addr()
            .map_err(|e| IoError::from_std(e, "local_addr"))?;
        Ok(addr
            .as_pathname()
            .map(|p| p.display().to_string())
            .unwrap_or_default())
    }
}

/// Connected Unix domain socket stream.
#[cfg(unix)]
#[derive(Debug)]
pub struct UnixStream {
    inner: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl UnixStream {
    /// Connects to the socket at `path`.
    pub fn connect(path: &str) -> Result<Self, IoError> {
        let inner = std::os::unix::net::UnixStream::connect(path)
            .map_err(|e| IoError::from_std(e, path))?;
        Ok(Self { inner })
    }

    /// Duplicates the underlying Unix-domain socket for blocking-pool I/O.
    pub fn try_clone(&self) -> Result<Self, IoError> {
        self.inner
            .try_clone()
            .map(|inner| Self { inner })
            .map_err(|e| IoError::from_std(e, "UnixStream::try_clone"))
    }

    /// Reads up to `buf.len()` bytes.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        self.inner
            .read(buf)
            .map_err(|e| IoError::from_std(e, "UnixStream::read"))
    }

    /// Writes `buf`, returning the number of bytes consumed.
    pub fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        self.inner
            .write(buf)
            .map_err(|e| IoError::from_std(e, "UnixStream::write"))
    }

    /// Closes both halves of the stream.
    pub fn shutdown(&self) -> Result<(), IoError> {
        self.inner
            .shutdown(std::net::Shutdown::Both)
            .map_err(|e| IoError::from_std(e, "UnixStream::shutdown"))
    }
}

// Windows / non-unix stubs: every method returns the same error so
// the import resolves and `use std::net::UnixListener` doesn't
// fail at compile time on Gossamer code that conditionally falls
// back to TcpListener at runtime. Real AF_UNIX support on Windows
// 10 1803+ is a separate work item - std's `os::windows::net` has
// no UnixListener and the surface needs path-prefix coercion and
// abstract-namespace handling that the unix path doesn't share.
#[cfg(not(unix))]
fn unix_unsupported(op: &str) -> IoError {
    IoError::Other(format!(
        "{op}: AF_UNIX sockets are not supported on this platform"
    ))
}

/// Bound Unix domain socket listener (stub on non-Unix targets).
#[cfg(not(unix))]
#[derive(Debug)]
pub struct UnixListener {
    _private: (),
}

#[cfg(not(unix))]
impl UnixListener {
    /// Always returns an unsupported-platform error.
    pub fn bind(_path: &str) -> Result<Self, IoError> {
        Err(unix_unsupported("UnixListener::bind"))
    }
    /// Always returns an unsupported-platform error.
    pub fn accept(&self) -> Result<UnixStream, IoError> {
        Err(unix_unsupported("UnixListener::accept"))
    }
    /// Always returns an unsupported-platform error.
    pub fn local_addr(&self) -> Result<String, IoError> {
        Err(unix_unsupported("UnixListener::local_addr"))
    }
}

/// Connected Unix domain socket stream (stub on non-Unix targets).
#[cfg(not(unix))]
#[derive(Debug)]
pub struct UnixStream {
    _private: (),
}

#[cfg(not(unix))]
impl UnixStream {
    /// Always returns an unsupported-platform error.
    pub fn connect(_path: &str) -> Result<Self, IoError> {
        Err(unix_unsupported("UnixStream::connect"))
    }
    /// Always returns an unsupported-platform error.
    pub fn read(&mut self, _buf: &mut [u8]) -> Result<usize, IoError> {
        Err(unix_unsupported("UnixStream::read"))
    }
    /// Always returns an unsupported-platform error.
    pub fn write(&mut self, _buf: &[u8]) -> Result<usize, IoError> {
        Err(unix_unsupported("UnixStream::write"))
    }
    /// Always returns an unsupported-platform error.
    pub fn shutdown(&self) -> Result<(), IoError> {
        Err(unix_unsupported("UnixStream::shutdown"))
    }
}

/// IP address parsing and inspection utilities.
pub mod ip;

#[cfg(test)]
mod p9_tests {
    use super::*;

    #[test]
    fn keepalive_toggle_succeeds_on_loopback_stream() {
        // The previous shape of this test dropped the listener
        // immediately after `connect` and called `set_keepalive` on
        // the un-`accept`ed client side. On Linux the kernel kept
        // the half-orphaned connection alive long enough for the
        // setsockopt to succeed, but macOS reset the socket more
        // eagerly and `setsockopt(SO_KEEPALIVE)` returned `EINVAL`
        // ("Invalid argument") because the socket was no longer
        // in `ESTABLISHED`. Hold the listener and explicitly accept
        // the server side so both ends are stable for the toggles.
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            // Hold the server side until the client closes.
            let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 1];
            let _ = std::io::Read::read(&mut (&sock), &mut buf);
        });
        let stream = TcpStream::connect(&addr.to_string()).unwrap();
        // Both ends are now ESTABLISHED; the setsockopt call must
        // succeed in both directions.
        stream.set_keepalive(Some(Duration::from_mins(1))).unwrap();
        stream.set_keepalive(None).unwrap();
        drop(stream);
        let _ = acceptor.join();
    }

    #[test]
    fn happy_eyeballs_picks_reachable_endpoint() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let real = listener.local_addr().unwrap();
        // Unreachable v4 candidate first, then the real
        // loopback. With a 50ms stagger, the second attempt
        // wins after the first fails.
        let bogus: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let acceptor = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let stream = TcpStream::connect_happy_eyeballs(
            &[bogus, real],
            Duration::from_millis(50),
            Duration::from_secs(2),
        );
        let _ = acceptor.join();
        assert!(stream.is_ok(), "expected happy-eyeballs to succeed");
    }

    #[test]
    fn happy_eyeballs_returns_timeout_when_all_fail() {
        // Two ports that should be unbound (we don't actually
        // know they're free, but :1 is reserved + we're racing
        // a tight deadline).
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:2".parse().unwrap();
        let err = TcpStream::connect_happy_eyeballs(
            &[a, b],
            Duration::from_millis(50),
            Duration::from_millis(500),
        )
        .unwrap_err();
        let _ = err; // any IoError is fine - we just need this not to hang.
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_round_trip() {
        let dir = gossamer_runtime::platform::temp_dir();
        let socket_path = dir.join(format!(
            "gos-test-unix-{}.sock",
            gossamer_runtime::platform::process_id()
        ));
        let _ = std::fs::remove_file(&socket_path);
        let path_str = socket_path.display().to_string();

        let listener = UnixListener::bind(&path_str).unwrap();
        let path_for_thread = path_str.clone();
        let acceptor = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut buf = [0u8; 5];
            stream.read(&mut buf).unwrap();
            stream.write(b"world").unwrap();
            let _ = path_for_thread;
        });
        let mut client = UnixStream::connect(&path_str).unwrap();
        client.write(b"hello").unwrap();
        let mut response = [0u8; 5];
        client.read(&mut response).unwrap();
        assert_eq!(&response, b"world");
        acceptor.join().unwrap();
        let _ = std::fs::remove_file(&socket_path);
    }
}
