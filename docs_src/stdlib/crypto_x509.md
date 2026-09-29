# `std::crypto::x509`

X.509 certificate parsing.

<!-- hand-maintained from here: preserved by `gos doc --emit-stdlib` -->

## API details and source

The [implementation source](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) contains the complete declarations and implementation notes. The table below lists canonical Gossamer call signatures; every item name links directly to its implementation file.

| Item | Canonical signature or declaration | Description |
|---|---|---|
| [`CertInfo`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `type CertInfo` | Inspected fields of an X.509 certificate. |
| [`parse_pem`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn parse_pem(pem: String) -> Result<x509::Certificate, errors::Error>` | Parses one PEM-encoded certificate. |
| [`verify_server_certificate_with_crls`](https://github.com/gossamer-lang/gossamer/blob/main/crates/gossamer-std/src/crypto.rs) | `fn verify_server_certificate_with_crls(chain_pem: String, roots_pem: String, hostname: String, crl_pem: String) -> Result<(), errors::Error>` | Verifies a leaf-first server chain and hostname against private roots and mandatory CRLs. Unknown revocation status and expired CRLs fail closed. |

## Private-root verification contract

`verify_server_certificate_with_crls` is the portable PKI verification API on
the VM, Cranelift JIT, and LLVM AOT tiers. `chain_pem` contains one or more
PEM `CERTIFICATE` blocks with the server leaf first and each intermediate after
it. `roots_pem` contains one or more private trust anchors. The root is not
included in `chain_pem` unless a deployment deliberately supplies it as an
intermediate.

`hostname` must be a valid DNS name or IP address accepted by Rustls. The
verifier checks the peer name, server-auth extended-key usage, the complete
chain, and the current wall-clock validity period. The time source is the host
clock at verification time.

`crl_pem` must contain PEM `X509 CRL` blocks for every non-root issuer whose
revocation status is needed. The verifier rejects empty or malformed CRL input,
an unknown revocation status, an expired CRL, a revoked certificate, a bad CRL
signature, an invalid chain, a hostname mismatch, and every parsing error. An
`Err(errors::Error)` is therefore a denial, never a soft warning.

System roots are not part of this contract. The API does not retrieve AIA
intermediates, CRLs, or OCSP responses; applications own transport, refresh,
caching, and any broader policy. It does not issue certificates, create CSRs,
or integrate key stores or HSMs.

## TLS host configuration

Rust-hosted TLS constructors remain host APIs rather than Gossamer source
items. `tls::client_config_with_crls` accepts optional private roots plus
server CRLs. `tls::server_config_with_client_auth` configures mTLS with client
roots, and `tls::server_config_with_client_auth_and_crls` adds fail-closed
client-certificate revocation checking. Use those constructors when wiring a
real Rust TLS connection, and use the public source API above when a
Gossamer program receives peer PEM material. See
`examples/x509_private_ca_peer_verify.gos` for the source-level flow.
