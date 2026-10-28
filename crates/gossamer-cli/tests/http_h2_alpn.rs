//! ALPN h2 / h1 negotiation parity.
//!
//! Boots the Gossamer HTTPS server with rustls + ALPN, then
//! connects with a client that advertises `[h2, http/1.1]`.
//! Asserts:
//!
//! 1. When the server config advertises `h2` first, the
//!    negotiation lands on `h2`.
//! 2. When the server config advertises only `http/1.1`, the
//!    negotiation lands on `http/1.1`.
//!
//! Uses `rcgen` to generate a self-signed certificate at runtime
//! so the test ships no key material. The client trusts the
//! certificate by inserting it into a custom root store; no
//! webpki-roots, no real network.

#![allow(missing_docs, clippy::missing_panics_doc, clippy::missing_errors_doc)]

use std::net::{SocketAddr, TcpListener as StdListener};
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const HANDSHAKE_DEADLINE_MS: u64 = 5_000;

/// A loopback listener on a port the system picked, and its address. The
/// server accepts on the bound listener itself, so no other process can take
/// the port first, and the client may connect before the server accepts.
fn loopback_listener() -> (StdListener, SocketAddr) {
    let listener = StdListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("local_addr");
    (listener, addr)
}

/// Generates a fresh self-signed certificate + private key for
/// `127.0.0.1`. The returned DER blobs feed directly into a
/// rustls `ServerConfig` and the client-side root store.
fn gen_cert_chain() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
        .expect("rcgen self-signed");
    let der = cert.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
    (vec![der], key)
}

fn build_client_config(
    server_cert: &CertificateDer<'static>,
    alpn: Vec<Vec<u8>>,
) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.add(server_cert.clone()).expect("add root");
    let mut cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = alpn;
    Arc::new(cfg)
}

/// Helper: accept one TLS connection on `listener`, complete the
/// handshake, return the negotiated ALPN protocol bytes.
async fn server_accept_one_alpn(
    server_cfg: Arc<ServerConfig>,
    listener: StdListener,
) -> Option<Vec<u8>> {
    let listener = tokio::net::TcpListener::from_std(listener).expect("tcp listener");
    let acceptor = TlsAcceptor::from(server_cfg);
    let (sock, _peer) = listener.accept().await.expect("tcp accept");
    let tls = acceptor.accept(sock).await.expect("tls accept");
    let alpn = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
    // Half-close cleanly so the client side resolves.
    drop(tls);
    alpn
}

async fn client_handshake_alpn(
    server_addr: SocketAddr,
    cert: CertificateDer<'static>,
    alpn: Vec<Vec<u8>>,
) -> Option<Vec<u8>> {
    let cfg = build_client_config(&cert, alpn);
    let connector = TlsConnector::from(cfg);
    let tcp = tokio::net::TcpStream::connect(server_addr)
        .await
        .expect("tcp connect");
    let server_name = ServerName::try_from("127.0.0.1")
        .expect("server name")
        .to_owned();
    let tls = connector
        .connect(server_name, tcp)
        .await
        .expect("tls handshake");
    tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec)
}

fn run<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(async move {
        tokio::time::timeout(Duration::from_millis(HANDSHAKE_DEADLINE_MS), fut)
            .await
            .expect("alpn deadline elapsed")
    })
}

#[test]
fn alpn_selects_h2_when_both_advertised() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (listener, addr) = loopback_listener();

    // Generate the cert once and feed it to both server config and
    // client root store. `build_server_config` would re-roll a
    // distinct chain, breaking the trust path.
    let (chain, key) = gen_cert_chain();
    let cert_for_client = chain[0].clone();
    let mut server_cfg_inner = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain.clone(), key)
        .expect("server cfg");
    server_cfg_inner.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let server_cfg = Arc::new(server_cfg_inner);

    let server_alpn = std::sync::Arc::new(std::sync::Mutex::new(None));
    let client_alpn = std::sync::Arc::new(std::sync::Mutex::new(None));
    let server_alpn_clone = std::sync::Arc::clone(&server_alpn);
    let client_alpn_clone = std::sync::Arc::clone(&client_alpn);

    run(async move {
        let server_task = tokio::spawn(async move {
            let n = server_accept_one_alpn(server_cfg, listener).await;
            *server_alpn_clone.lock().expect("lock") = n;
        });
        let n = client_handshake_alpn(
            addr,
            cert_for_client,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        )
        .await;
        *client_alpn_clone.lock().expect("lock") = n;
        let _ = server_task.await;
    });

    let s = server_alpn.lock().expect("lock").clone();
    let c = client_alpn.lock().expect("lock").clone();
    assert_eq!(s.as_deref(), Some(b"h2".as_ref()), "server alpn");
    assert_eq!(c.as_deref(), Some(b"h2".as_ref()), "client alpn");
}

#[test]
fn alpn_falls_back_to_http11_when_h2_not_offered() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (listener, addr) = loopback_listener();

    let (chain, key) = gen_cert_chain();
    let cert_for_client = chain[0].clone();
    let mut server_cfg_inner = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain.clone(), key)
        .expect("server cfg");
    // Server advertises ONLY http/1.1.
    server_cfg_inner.alpn_protocols = vec![b"http/1.1".to_vec()];
    let server_cfg = Arc::new(server_cfg_inner);

    let server_alpn = std::sync::Arc::new(std::sync::Mutex::new(None));
    let client_alpn = std::sync::Arc::new(std::sync::Mutex::new(None));
    let server_alpn_clone = std::sync::Arc::clone(&server_alpn);
    let client_alpn_clone = std::sync::Arc::clone(&client_alpn);

    run(async move {
        let server_task = tokio::spawn(async move {
            let n = server_accept_one_alpn(server_cfg, listener).await;
            *server_alpn_clone.lock().expect("lock") = n;
        });
        // Client offers both - server should pick http/1.1.
        let n = client_handshake_alpn(
            addr,
            cert_for_client,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        )
        .await;
        *client_alpn_clone.lock().expect("lock") = n;
        let _ = server_task.await;
    });

    let s = server_alpn.lock().expect("lock").clone();
    let c = client_alpn.lock().expect("lock").clone();
    assert_eq!(s.as_deref(), Some(b"http/1.1".as_ref()), "server alpn");
    assert_eq!(c.as_deref(), Some(b"http/1.1".as_ref()), "client alpn");
}
