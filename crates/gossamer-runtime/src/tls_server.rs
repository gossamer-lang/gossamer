//! Live TLS server configurations: a default certificate, certificates chosen
//! by SNI name, and optional client-certificate (mutual TLS) verification,
//! all replaceable while a server runs. A server reads the current
//! configuration for each connection it accepts, so a reload reaches the next
//! handshake without a restart. The bytecode VM and compiled code share the
//! registry, so a configuration behaves the same on every tier.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};

use parking_lot::{Mutex, RwLock};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert, WebPkiClientVerifier};
use rustls::sign::CertifiedKey;
use rustls::{RootCertStore, ServerConfig};

/// What a configuration is built from: PEM text, kept so a change to one part
/// rebuilds the whole.
#[derive(Debug, Clone)]
struct ServerSpec {
    cert_pem: String,
    key_pem: String,
    client_ca_pem: Option<String>,
    client_auth_optional: bool,
    /// Per SNI host name, lower-cased: its certificate chain and key.
    sni: Vec<(String, String, String)>,
}

/// A configuration a server reads per connection.
#[derive(Debug)]
pub struct LiveConfig {
    spec: Mutex<ServerSpec>,
    current: RwLock<Arc<ServerConfig>>,
}

impl LiveConfig {
    /// The configuration the next handshake uses.
    #[must_use]
    pub fn current(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.current.read())
    }

    /// Applies `change` to the spec and installs the configuration it
    /// builds, leaving both as they were when the build fails.
    fn update(&self, change: impl FnOnce(&mut ServerSpec)) -> Result<(), String> {
        let mut spec = self.spec.lock();
        let mut next = spec.clone();
        change(&mut next);
        let built = build(&next)?;
        *spec = next;
        *self.current.write() = built;
        Ok(())
    }
}

static REGISTRY: LazyLock<Mutex<HashMap<i64, Arc<LiveConfig>>>> = LazyLock::new(Default::default);
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

/// Registers a configuration serving `cert_pem` / `key_pem`, verifying client
/// certificates against `client_ca_pem` when given (required unless
/// `client_auth_optional`), and answers its id.
///
/// # Errors
/// PEM text that holds no certificate or key, a key that does not match, or
/// a client CA bundle with no certificate.
pub fn create(
    cert_pem: &str,
    key_pem: &str,
    client_ca_pem: Option<&str>,
    client_auth_optional: bool,
) -> Result<i64, String> {
    let spec = ServerSpec {
        cert_pem: cert_pem.to_string(),
        key_pem: key_pem.to_string(),
        client_ca_pem: client_ca_pem.map(str::to_string),
        client_auth_optional,
        sni: Vec::new(),
    };
    let built = build(&spec)?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    REGISTRY.lock().insert(
        id,
        Arc::new(LiveConfig {
            spec: Mutex::new(spec),
            current: RwLock::new(built),
        }),
    );
    Ok(id)
}

/// The configuration `id` names, which a server holds for as long as it runs.
#[must_use]
pub fn live(id: i64) -> Option<Arc<LiveConfig>> {
    REGISTRY.lock().get(&id).cloned()
}

fn with_live(id: i64, f: impl FnOnce(&LiveConfig) -> Result<(), String>) -> Result<(), String> {
    let config = live(id).ok_or_else(|| "tls::ServerConfig: configuration released".to_string())?;
    f(&config)
}

/// Serves `cert_pem` / `key_pem` to a client that names `host`, replacing the
/// certificate that host had.
///
/// # Errors
/// As [`create`], for the new certificate.
pub fn add_sni(id: i64, host: &str, cert_pem: &str, key_pem: &str) -> Result<(), String> {
    let host = host.to_ascii_lowercase();
    with_live(id, |config| {
        config.update(|spec| {
            spec.sni.retain(|(name, _, _)| *name != host);
            spec.sni
                .push((host, cert_pem.to_string(), key_pem.to_string()));
        })
    })
}

/// Replaces the default certificate; handshakes after this one use it.
///
/// # Errors
/// As [`create`], for the new certificate; the old one stays in use.
pub fn reload(id: i64, cert_pem: &str, key_pem: &str) -> Result<(), String> {
    with_live(id, |config| {
        config.update(|spec| {
            spec.cert_pem = cert_pem.to_string();
            spec.key_pem = key_pem.to_string();
        })
    })
}

/// Gives back the registry's hold on `id`; a server already running keeps
/// the configuration it was handed.
pub fn release(id: i64) {
    REGISTRY.lock().remove(&id);
}

/// The certificate chain and signing key `cert_pem` / `key_pem` hold.
fn certified_key(cert_pem: &str, key_pem: &str) -> Result<Arc<CertifiedKey>, String> {
    let certs = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("certificate PEM: {e}"))?;
    if certs.is_empty() {
        return Err("no certificate in the PEM text".to_string());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| format!("private key PEM: {e}"))?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| format!("private key: {e}"))?;
    let certified = CertifiedKey::new(certs, signing);
    certified
        .keys_match()
        .map_err(|e| format!("certificate and key: {e}"))?;
    Ok(Arc::new(certified))
}

/// Chooses the certificate for the host name a client sends, else the
/// default.
#[derive(Debug)]
struct SniResolver {
    default: Arc<CertifiedKey>,
    by_name: HashMap<String, Arc<CertifiedKey>>,
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let named = client_hello
            .server_name()
            .and_then(|name| self.by_name.get(&name.to_ascii_lowercase()));
        Some(Arc::clone(named.unwrap_or(&self.default)))
    }
}

fn build(spec: &ServerSpec) -> Result<Arc<ServerConfig>, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let resolver = SniResolver {
        default: certified_key(&spec.cert_pem, &spec.key_pem)?,
        by_name: spec
            .sni
            .iter()
            .map(|(name, cert, key)| {
                certified_key(cert, key)
                    .map(|key| (name.clone(), key))
                    .map_err(|e| format!("{name}: {e}"))
            })
            .collect::<Result<_, _>>()?,
    };
    let builder = ServerConfig::builder();
    let builder = match &spec.client_ca_pem {
        None => builder.with_no_client_auth(),
        Some(ca_pem) => {
            let mut roots = RootCertStore::empty();
            for cert in CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
                let cert = cert.map_err(|e| format!("client CA PEM: {e}"))?;
                roots.add(cert).map_err(|e| format!("client CA: {e}"))?;
            }
            if roots.is_empty() {
                return Err("no certificate in the client CA PEM text".to_string());
            }
            let verifier = WebPkiClientVerifier::builder(Arc::new(roots));
            let verifier = if spec.client_auth_optional {
                verifier.allow_unauthenticated()
            } else {
                verifier
            };
            let verifier = verifier
                .build()
                .map_err(|e| format!("client verifier: {e}"))?;
            builder.with_client_cert_verifier(verifier)
        }
    };
    Ok(Arc::new(builder.with_cert_resolver(Arc::new(resolver))))
}
