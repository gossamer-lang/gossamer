#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::must_use_candidate)]

//! `std::crypto::x509::parse_pem` leaf intrinsic. Returns a 7-slot
//! `(subject, issuer, serial, not_before_unix, not_after_unix,
//! san_dns, sha256)` tuple via the by-value-aggregate ABI; the
//! injected Gossamer wrapper folds it into a real `CertInfo` struct.
//! Mirrors `gossamer_std::crypto::x509::parse_der` so the compiled
//! tier matches the VM byte-for-byte.

use std::os::raw::c_char;
use std::sync::Arc;

use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};

use super::string::alloc_cstring;
use super::vec::{GosVec, gos_rt_result_new, gos_rt_vec_push};

unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    unsafe { crate::c_abi::gos_str_arg_text(p) }
}

fn byte_vec(bytes: &[u8]) -> *mut GosVec {
    super::encoding::bytes_to_gosvec(bytes)
}

// STRING-typed: the vec owns each element, so `gos_rt_vec_free`
// deep-frees them.
fn str_vec(items: &[String]) -> *mut GosVec {
    let v = unsafe {
        crate::c_abi::vec::gos_rt_vec_with_capacity_typed(
            8,
            items.len() as i64,
            crate::c_abi::vec::vec_elem_kind::STRING,
        )
    };
    for s in items {
        let pv = alloc_cstring(s.as_bytes()) as i64;
        unsafe { gos_rt_vec_push(v, std::ptr::addr_of!(pv).cast::<u8>()) };
    }
    v
}

fn err(msg: &str) -> i128 {
    let e = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    unsafe { gos_rt_result_new(1, e as i64) }
}

/// Layout of the parsed certificate tuple `(subject, issuer, serial,
/// not_before, not_after, san_dns, sha256)`: its strings and vectors are owned
/// by the blob.
static CERT_INFO_META: [i64; 9] = [
    gossamer_abi::rc::RC_KIND_STRUCT,
    1,
    0,
    5,
    gossamer_abi::rc::RC_CHILD_RC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT,
    (gossamer_abi::rc::RC_CHILD_RC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | 1,
    (gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | 2,
    (gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | 5,
    (gossamer_abi::rc::RC_CHILD_VEC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | 6,
];

/// `crypto::x509::parse_pem(s)` leaf -> Result<(subject, issuer,
/// serial, not_before_unix, not_after_unix, san_dns, sha256), Error>.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_x509_parse_pem_raw(s: *const c_char) -> i128 {
    ffi_entry!(0i128, {
        let pem = unsafe { cstr(s) }.as_bytes();
        let der = match x509_parser::pem::parse_x509_pem(pem) {
            Ok((_, p)) => p.contents,
            Err(e) => return err(&format!("x509: pem: {e}")),
        };
        let cert = match x509_parser::parse_x509_certificate(&der) {
            Ok((_, c)) => c,
            Err(e) => return err(&format!("x509: der: {e}")),
        };
        let subject = cert.subject().to_string();
        let issuer = cert.issuer().to_string();
        let serial = cert.serial.to_bytes_be();
        let not_before = cert.validity().not_before.timestamp();
        let not_after = cert.validity().not_after.timestamp();
        let mut san_dns: Vec<String> = Vec::new();
        if let Ok(Some(san)) = cert.subject_alternative_name() {
            for name in &san.value.general_names {
                if let x509_parser::extensions::GeneralName::DNSName(d) = name {
                    san_dns.push((*d).to_string());
                }
            }
        }
        let sha256 = Sha256::digest(&der);

        let words = [
            alloc_cstring(subject.as_bytes()) as i64,
            alloc_cstring(issuer.as_bytes()) as i64,
            byte_vec(&serial) as i64,
            not_before,
            not_after,
            str_vec(&san_dns) as i64,
            byte_vec(&sha256) as i64,
        ];
        let blob = crate::c_abi::rc::counted_words(&words, &CERT_INFO_META);
        if blob.is_null() {
            return err("x509: alloc failed");
        }
        unsafe { gos_rt_result_new(0, blob as i64) }
    })
}

/// `crypto::x509::verify_server_certificate_with_crls(chain, roots,
/// hostname, crls) -> Result<(), Error>`.
///
/// Checks the leaf-first PEM chain and DNS/IP hostname against the supplied
/// private roots. Revocation is fail-closed: a CRL is required, unknown
/// revocation status is rejected by default, and expired CRLs are rejected.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_x509_verify_server_certificate_with_crls(
    chain_pem: *const c_char,
    roots_pem: *const c_char,
    hostname: *const c_char,
    crl_pem: *const c_char,
) -> i128 {
    ffi_entry!(0i128, {
        let chain = match CertificateDer::pem_slice_iter(unsafe { cstr(chain_pem) }.as_bytes())
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(chain) if !chain.is_empty() => chain,
            Ok(_) => return err("x509: certificate chain contains no certificates"),
            Err(e) => return err(&format!("x509: certificate chain: {e}")),
        };
        let (leaf, intermediates) = chain.split_first().expect("checked non-empty chain");

        let root_certs = match CertificateDer::pem_slice_iter(unsafe { cstr(roots_pem) }.as_bytes())
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(roots) if !roots.is_empty() => roots,
            Ok(_) => return err("x509: roots contain no certificates"),
            Err(e) => return err(&format!("x509: roots: {e}")),
        };
        let mut roots = RootCertStore::empty();
        for root in root_certs {
            if let Err(e) = roots.add(root) {
                return err(&format!("x509: root: {e}"));
            }
        }

        let crls =
            match CertificateRevocationListDer::pem_slice_iter(unsafe { cstr(crl_pem) }.as_bytes())
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(crls) if !crls.is_empty() => crls,
                Ok(_) => return err("x509: CRL input contains no X509 CRLs"),
                Err(e) => return err(&format!("x509: CRL: {e}")),
            };
        let verifier = match WebPkiServerVerifier::builder(Arc::new(roots))
            .with_crls(crls)
            .enforce_revocation_expiration()
            .build()
        {
            Ok(verifier) => verifier,
            Err(e) => return err(&format!("x509: verifier: {e}")),
        };
        let server_name = match ServerName::try_from(unsafe { cstr(hostname) }.to_owned()) {
            Ok(server_name) => server_name,
            Err(e) => return err(&format!("x509: hostname: {e}")),
        };
        match verifier.verify_server_cert(leaf, intermediates, &server_name, &[], UnixTime::now()) {
            Ok(_) => unsafe { gos_rt_result_new(0, 0) },
            Err(e) => err(&format!("x509: verify server certificate: {e}")),
        }
    })
}
