//! Runtime support for `std::tls` - TLS termination and dialling.
//! Backed by [`rustls`] + the Mozilla root-CA bundle from
//! `webpki-roots`.
//!
//! Three sets of builders are exposed:
//! - [`server_config`] / [`server_config_with_client_auth`] produce a
//!   TLS-terminating `ServerConfig` from PEM-encoded certificates.
//!   The `_with_client_auth` variant turns on mutual TLS by requiring
//!   a client certificate signed by the supplied trust store.
//! - [`client_config`] / [`client_config_with_certificate`] produce
//!   client-side configurations: the bare form pins to the bundled
//!   Mozilla roots, the `_with_certificate` form additionally
//!   presents a client certificate (for mTLS) and lets callers swap
//!   in a custom root store.
//! - ALPN / SNI helpers thread through both sides.
//!
//! All handles are opaque wrappers around the underlying `rustls`
//! configs, kept behind a struct so programmatic callers can not
//! depend on the rustls version.

#![forbid(unsafe_code)]

use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::{
    ClientConnection, RootCertStore, ServerConfig as RustlsServerConfig, ServerConnection,
};

use crate::errors::Error;

/// A PEM-encoded certificate plus its matching private key. Consumed
/// by [`server_config`] to configure a TLS-terminating listener.
#[derive(Debug, Clone)]
pub struct CertKey {
    /// PEM-encoded certificate chain (leaf first).
    pub cert_pem: Vec<u8>,
    /// PEM-encoded private key.
    pub key_pem: Vec<u8>,
}

/// Server-side TLS configuration. Clone cheaply.
#[derive(Clone)]
pub struct ServerConfig {
    inner: ServerConfigSource,
}

/// Where a [`ServerConfig`] reads its rustls configuration: one fixed for
/// its lifetime, or a live one a program reloads while the server runs.
#[derive(Clone)]
enum ServerConfigSource {
    Fixed(Arc<RustlsServerConfig>),
    Live(Arc<gossamer_runtime::tls_server::LiveConfig>),
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("ServerConfig(...)")
    }
}

impl ServerConfig {
    /// Borrows the underlying rustls handle. Useful when wiring the
    /// config into a `rustls::ServerConnection` inside the HTTP
    /// server.
    /// A live configuration reads again for every call, so a server that
    /// asks per connection sees each reload.
    #[must_use]
    pub fn rustls(&self) -> Arc<RustlsServerConfig> {
        match &self.inner {
            ServerConfigSource::Fixed(config) => Arc::clone(config),
            ServerConfigSource::Live(live) => live.current(),
        }
    }

    /// The live configuration registered under `id` by
    /// `tls::ServerConfig::new`, or `None` once it is released.
    #[must_use]
    pub fn live(id: i64) -> Option<Self> {
        gossamer_runtime::tls_server::live(id).map(|live| Self {
            inner: ServerConfigSource::Live(live),
        })
    }

    /// Constructs a `rustls::ServerConnection` ready to be paired with
    /// an underlying TCP stream.
    pub fn new_connection(&self) -> Result<ServerConnection, Error> {
        ServerConnection::new(self.rustls()).map_err(|e| wrap_err("new_connection", e))
    }
}

/// Client-side TLS configuration.
#[derive(Clone)]
pub struct ClientConfig {
    inner: Arc<rustls::ClientConfig>,
    // Source PEM bytes preserved for cross-stack bridging. The
    // HTTP client implementation (currently ureq-backed) builds
    // its own rustls config internally from PEM, so we keep the
    // PEM bytes around to feed it. Optional because the default
    // Mozilla-roots config doesn't carry caller-supplied PEM.
    extra_roots_pem: Option<Arc<Vec<u8>>>,
    client_cert_pem: Option<Arc<Vec<u8>>>,
    client_key_pem: Option<Arc<Vec<u8>>>,
    alpn_protocols: Vec<Vec<u8>>,
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("ClientConfig(...)")
    }
}

impl ClientConfig {
    /// Borrows the underlying rustls handle.
    #[must_use]
    pub fn rustls(&self) -> Arc<rustls::ClientConfig> {
        Arc::clone(&self.inner)
    }

    /// Builds a TLS connection state for a SNI hostname. The hostname
    /// must be DNS-valid; IP-only servers return `Err`.
    pub fn new_connection(&self, server_name: &str) -> Result<ClientConnection, Error> {
        let name = ServerName::try_from(server_name.to_string())
            .map_err(|e| wrap_err("server name", e))?;
        ClientConnection::new(Arc::clone(&self.inner), name)
            .map_err(|e| wrap_err("new_connection", e))
    }

    /// Returns PEM bytes for extra trust roots beyond the bundled
    /// Mozilla set, if the config was built with any.
    #[must_use]
    pub fn extra_roots_pem(&self) -> Option<&[u8]> {
        self.extra_roots_pem.as_deref().map(Vec::as_slice)
    }

    /// Returns the client-cert chain PEM if the config carries
    /// client-auth credentials.
    #[must_use]
    pub fn client_cert_pem(&self) -> Option<&[u8]> {
        self.client_cert_pem.as_deref().map(Vec::as_slice)
    }

    /// Returns the client private-key PEM if the config carries
    /// client-auth credentials.
    #[must_use]
    pub fn client_key_pem(&self) -> Option<&[u8]> {
        self.client_key_pem.as_deref().map(Vec::as_slice)
    }

    /// Returns the ALPN protocol list configured for this client.
    #[must_use]
    pub fn alpn_protocols(&self) -> &[Vec<u8>] {
        &self.alpn_protocols
    }
}

/// Produces a server-side TLS configuration from a PEM-encoded
/// certificate chain and matching private key. No client-cert
/// verification.
pub fn server_config(cert: CertKey) -> Result<ServerConfig, Error> {
    install_ring_provider();
    let certs = read_certs(&cert.cert_pem).map_err(|e| wrap_err("cert parse", e))?;
    if certs.is_empty() {
        return Err(Error::new(
            "std::tls::server_config: no certificates in PEM",
        ));
    }
    let key = read_private_key(&cert.key_pem).map_err(|e| wrap_err("key parse", e))?;
    let config = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| wrap_err("build server config", e))?;
    Ok(ServerConfig {
        inner: ServerConfigSource::Fixed(Arc::new(config)),
    })
}

/// Mutual-TLS server configuration: clients must present a
/// certificate signed by the supplied PEM trust bundle. Use the
/// returned config in place of [`server_config`] for service-mesh
/// or partner-channel deployments.
pub fn server_config_with_client_auth(
    cert: CertKey,
    client_ca_pem: &[u8],
) -> Result<ServerConfig, Error> {
    install_ring_provider();
    let certs = read_certs(&cert.cert_pem).map_err(|e| wrap_err("cert parse", e))?;
    if certs.is_empty() {
        return Err(Error::new(
            "std::tls::server_config_with_client_auth: no server certificates",
        ));
    }
    let key = read_private_key(&cert.key_pem).map_err(|e| wrap_err("key parse", e))?;
    let mut roots = RootCertStore::empty();
    let mut count = 0;
    for cert in read_certs(client_ca_pem).map_err(|e| wrap_err("client ca", e))? {
        roots.add(cert).map_err(|e| wrap_err("client root", e))?;
        count += 1;
    }
    if count == 0 {
        return Err(Error::new(
            "std::tls::server_config_with_client_auth: no client CAs in PEM",
        ));
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| wrap_err("client verifier", e))?;
    let config = RustlsServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| wrap_err("build server config", e))?;
    Ok(ServerConfig {
        inner: ServerConfigSource::Fixed(Arc::new(config)),
    })
}

/// Mutual-TLS server configuration with fail-closed certificate
/// revocation checking. `client_crl_pem` must contain one or more PEM
/// `X509 CRL` blocks issued by the client trust roots. Rustls verifies
/// every non-root certificate in the presented chain, rejects unknown
/// revocation status, and rejects expired CRLs.
///
/// Use this form when client certificates are long-lived or issued to
/// devices that may need to be revoked. The simpler
/// [`server_config_with_client_auth`] intentionally does not perform
/// revocation checks because it has no CRL input.
pub fn server_config_with_client_auth_and_crls(
    cert: CertKey,
    client_ca_pem: &[u8],
    client_crl_pem: &[u8],
) -> Result<ServerConfig, Error> {
    install_ring_provider();
    let certs = read_certs(&cert.cert_pem).map_err(|e| wrap_err("cert parse", e))?;
    if certs.is_empty() {
        return Err(Error::new(
            "std::tls::server_config_with_client_auth_and_crls: no server certificates",
        ));
    }
    let key = read_private_key(&cert.key_pem).map_err(|e| wrap_err("key parse", e))?;
    let roots = read_root_store(client_ca_pem, "client ca")?;
    let crls = read_crls(client_crl_pem).map_err(|e| wrap_err("client crl", e))?;
    if crls.is_empty() {
        return Err(Error::new(
            "std::tls::server_config_with_client_auth_and_crls: no client CRLs in PEM",
        ));
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .with_crls(crls)
        .enforce_revocation_expiration()
        .build()
        .map_err(|e| wrap_err("client verifier", e))?;
    let config = RustlsServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| wrap_err("build server config", e))?;
    Ok(ServerConfig {
        inner: ServerConfigSource::Fixed(Arc::new(config)),
    })
}

/// Sets the ALPN protocol list negotiated with each connecting
/// client. Standard values: `b"h2"`, `b"http/1.1"`. Returns a fresh
/// [`ServerConfig`] - the input is not mutated.
#[must_use]
pub fn server_with_alpn(config: ServerConfig, protocols: &[&[u8]]) -> ServerConfig {
    let mut inner = (*config.rustls()).clone();
    inner.alpn_protocols = protocols.iter().map(|p| p.to_vec()).collect();
    ServerConfig {
        inner: ServerConfigSource::Fixed(Arc::new(inner)),
    }
}

/// Produces a client-side TLS configuration pinned to the bundled
/// Mozilla root certificate store.
pub fn client_config() -> Result<ClientConfig, Error> {
    install_ring_provider();
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(ClientConfig {
        inner: Arc::new(config),
        extra_roots_pem: None,
        client_cert_pem: None,
        client_key_pem: None,
        alpn_protocols: Vec::new(),
    })
}

/// Client-side mTLS configuration: roots determine which servers we
/// trust, `cert` is the client identity. Pass `None` for `extra_roots_pem`
/// to use the Mozilla bundle.
pub fn client_config_with_certificate(
    cert: CertKey,
    extra_roots_pem: Option<&[u8]>,
) -> Result<ClientConfig, Error> {
    install_ring_provider();
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = extra_roots_pem {
        for cert in read_certs(pem).map_err(|e| wrap_err("extra roots", e))? {
            roots.add(cert).map_err(|e| wrap_err("extra root", e))?;
        }
    }
    let certs = read_certs(&cert.cert_pem).map_err(|e| wrap_err("client cert parse", e))?;
    if certs.is_empty() {
        return Err(Error::new(
            "std::tls::client_config_with_certificate: no client cert in PEM",
        ));
    }
    let key = read_private_key(&cert.key_pem).map_err(|e| wrap_err("client key parse", e))?;
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(|e| wrap_err("build client config", e))?;
    Ok(ClientConfig {
        inner: Arc::new(config),
        extra_roots_pem: extra_roots_pem.map(|pem| Arc::new(pem.to_vec())),
        client_cert_pem: Some(Arc::new(cert.cert_pem.clone())),
        client_key_pem: Some(Arc::new(cert.key_pem.clone())),
        alpn_protocols: Vec::new(),
    })
}

/// Client TLS configuration with explicit CRL-based server-certificate
/// revocation checking. The Mozilla trust store is always included;
/// `extra_roots_pem` adds private trust anchors. `server_crl_pem` must
/// contain one or more PEM `X509 CRL` blocks. Verification is
/// fail-closed for unknown status and expired CRLs and continues to
/// enforce the SNI hostname supplied to [`ClientConfig::new_connection`].
///
/// This is deliberately separate from [`client_config`]: revocation
/// data is deployment-specific and an absent CRL must not be mistaken
/// for a successful revocation check.
pub fn client_config_with_crls(
    extra_roots_pem: Option<&[u8]>,
    server_crl_pem: &[u8],
) -> Result<ClientConfig, Error> {
    install_ring_provider();
    let roots = client_roots(extra_roots_pem)?;
    let crls = read_crls(server_crl_pem).map_err(|e| wrap_err("server crl", e))?;
    if crls.is_empty() {
        return Err(Error::new(
            "std::tls::client_config_with_crls: no server CRLs in PEM",
        ));
    }
    let verifier = WebPkiServerVerifier::builder(Arc::new(roots))
        .with_crls(crls)
        .enforce_revocation_expiration()
        .build()
        .map_err(|e| wrap_err("server verifier", e))?;
    let config = rustls::ClientConfig::builder()
        .with_webpki_verifier(verifier)
        .with_no_client_auth();
    Ok(ClientConfig {
        inner: Arc::new(config),
        extra_roots_pem: extra_roots_pem.map(|pem| Arc::new(pem.to_vec())),
        client_cert_pem: None,
        client_key_pem: None,
        alpn_protocols: Vec::new(),
    })
}

/// Adds an ALPN protocol list to a client config.
#[must_use]
pub fn client_with_alpn(config: ClientConfig, protocols: &[&[u8]]) -> ClientConfig {
    let mut inner = (*config.inner).clone();
    let alpn: Vec<Vec<u8>> = protocols.iter().map(|p| p.to_vec()).collect();
    inner.alpn_protocols.clone_from(&alpn);
    ClientConfig {
        inner: Arc::new(inner),
        extra_roots_pem: config.extra_roots_pem,
        client_cert_pem: config.client_cert_pem,
        client_key_pem: config.client_key_pem,
        alpn_protocols: alpn,
    }
}

fn install_ring_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn wrap_err(context: &str, error: impl std::fmt::Display) -> Error {
    Error::new(format!("std::tls: {context}: {error}"))
}

fn read_certs(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, std::io::Error> {
    CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(std::io::Error::other)
}

fn read_crls(pem: &[u8]) -> Result<Vec<CertificateRevocationListDer<'static>>, std::io::Error> {
    CertificateRevocationListDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(std::io::Error::other)
}

fn read_root_store(pem: &[u8], context: &str) -> Result<RootCertStore, Error> {
    let mut roots = RootCertStore::empty();
    let certs = read_certs(pem).map_err(|e| wrap_err(context, e))?;
    if certs.is_empty() {
        return Err(Error::new(format!(
            "std::tls: {context}: no certificates in PEM"
        )));
    }
    for cert in certs {
        roots.add(cert).map_err(|e| wrap_err(context, e))?;
    }
    Ok(roots)
}

fn client_roots(extra_roots_pem: Option<&[u8]>) -> Result<RootCertStore, Error> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = extra_roots_pem {
        for cert in read_certs(pem).map_err(|e| wrap_err("extra roots", e))? {
            roots.add(cert).map_err(|e| wrap_err("extra root", e))?;
        }
    }
    Ok(roots)
}

fn read_private_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, std::io::Error> {
    PrivateKeyDer::pem_slice_iter(pem)
        .next()
        .ok_or_else(|| std::io::Error::other("no private key found in PEM"))?
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_builds_against_mozilla_roots() {
        let config = client_config().expect("client config");
        assert!(!format!("{config:?}").is_empty());
    }

    #[test]
    fn server_config_rejects_empty_pem() {
        let err = server_config(CertKey {
            cert_pem: Vec::new(),
            key_pem: Vec::new(),
        })
        .unwrap_err();
        assert!(err.message().contains("no certificates"));
    }

    #[test]
    fn client_with_alpn_sets_protocols() {
        let cfg = client_config().unwrap();
        let with_alpn = client_with_alpn(cfg, &[b"h2", b"http/1.1"]);
        assert_eq!(with_alpn.inner.alpn_protocols.len(), 2);
        assert_eq!(with_alpn.inner.alpn_protocols[0], b"h2".to_vec());
    }

    #[test]
    fn client_crl_config_rejects_missing_crl() {
        let err = client_config_with_crls(None, b"").unwrap_err();
        assert!(err.message().contains("no server CRLs"));
    }

    #[test]
    fn client_crl_config_accepts_a_signed_crl_for_a_custom_root() {
        use rcgen::{
            BasicConstraints, CertificateParams, CertificateRevocationListParams, CertifiedIssuer,
            ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SerialNumber, date_time_ymd,
        };
        use rustls::StreamOwned;
        use rustls::pki_types::PrivatePkcs8KeyDer;
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};

        let mut params = CertificateParams::new(vec!["test-ca.invalid".to_owned()]).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let crl = CertificateRevocationListParams {
            this_update: date_time_ymd(2025, 1, 1),
            next_update: date_time_ymd(2030, 1, 1),
            crl_number: SerialNumber::from(1_u64),
            issuing_distribution_point: None,
            revoked_certs: Vec::new(),
            key_identifier_method: rcgen::KeyIdMethod::Sha256,
        }
        .signed_by(&issuer)
        .unwrap();

        let ca_pem = issuer.pem();
        let crl_pem = crl.pem().unwrap();
        let config = client_config_with_crls(Some(ca_pem.as_bytes()), crl_pem.as_bytes())
            .expect("signed CRL and custom root configure a verifier");
        assert!(config.extra_roots_pem().is_some());

        let mut server_params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_key = KeyPair::generate().unwrap();
        let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();
        crate::crypto::x509::verify_server_certificate_with_crls(
            server_cert.pem().as_bytes(),
            ca_pem.as_bytes(),
            "localhost",
            crl_pem.as_bytes(),
        )
        .expect("public verifier accepts a CA-signed localhost certificate and current CRL");
        assert!(
            crate::crypto::x509::verify_server_certificate_with_crls(
                server_cert.pem().as_bytes(),
                ca_pem.as_bytes(),
                "wrong.invalid",
                crl_pem.as_bytes(),
            )
            .is_err()
        );
        let server = RustlsServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(server_cert.der().to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
            )
            .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let join = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let conn = ServerConnection::new(Arc::new(server)).unwrap();
            let mut tls = StreamOwned::new(conn, socket);
            let mut byte = [0_u8; 1];
            tls.read_exact(&mut byte).unwrap();
            assert_eq!(byte, [0xA5]);
        });
        let conn = config.new_connection("localhost").unwrap();
        let socket = TcpStream::connect(address).unwrap();
        let mut tls = StreamOwned::new(conn, socket);
        tls.write_all(&[0xA5]).unwrap();
        tls.flush().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn mtls_crl_config_rejects_missing_server_cert_before_crl_parsing() {
        let err = server_config_with_client_auth_and_crls(
            CertKey {
                cert_pem: Vec::new(),
                key_pem: Vec::new(),
            },
            b"",
            b"",
        )
        .unwrap_err();
        assert!(err.message().contains("no server certificates"));
    }
}
