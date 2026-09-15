//! C-ABI dispatch shims for `std::crypto::ecdsa` (ECDSA over NIST
//! P-256). These mirror the bytecode-VM builtins in
//! `gossamer-interp/src/stdlib_builtins/crypto_breadth.rs` so the
//! compiled (Cranelift / LLVM) tier resolves the same calls natively
//! instead of failing to link.
//!
//! The construction is copied verbatim from
//! `gossamer-std/src/crypto.rs::ecdsa` (the runtime cannot depend on
//! `gossamer-std` - that would cycle, since `gossamer-std` already
//! depends on `gossamer-runtime` - so the leaf logic is reimplemented
//! over the same `p256` crate, producing byte-identical results):
//! - `keypair_pem()` returns `Result<(String, String), errors::Error>` -
//!   `(secret_pkcs8_pem, public_spki_pem)` with LF line endings.
//!   The Ok payload is a 16-byte heap pair `(secret_ptr, public_ptr)`
//!   of `*mut c_char`, matching the bytecode VM's
//!   `Value::Tuple([secret, public])`.
//! - `sign_pem(secret_pem, message)` returns
//!   `Result<Vec<u8>, errors::Error>` - the DER-encoded signature.
//! - `verify_pem(public_pem, message, signature)` returns
//!   `Result<(), errors::Error>` - `Ok(())` packed as disc 0,
//!   payload 0 (mirrors the VM's `ok_variant(Value::Unit)`).

#![allow(clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]

use std::os::raw::c_char;

use p256::elliptic_curve::rand_core::{CryptoRng, RngCore};

use super::encoding::{bytes_to_gosvec, gosvec_u8};
use super::vec::{GosVec, gos_rt_result_new};

/// OS-CSPRNG adapter for `p256::ecdsa::SigningKey::random`. Mirrors
/// `gossamer_std::crypto::rand::OsRng` - backed by `getrandom`. On the
/// (essentially never) failure of the OS RNG, `fill_bytes` zero-fills;
/// the entry point probes the RNG up front and refuses on failure, so
/// a zero-filled scalar is never reached in practice.
struct OsRng;

impl RngCore for OsRng {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        if getrandom::fill(dest).is_err() {
            dest.fill(0);
        }
    }
    fn try_fill_bytes(
        &mut self,
        dest: &mut [u8],
    ) -> Result<(), p256::elliptic_curve::rand_core::Error> {
        getrandom::fill(dest).map_err(|_| {
            p256::elliptic_curve::rand_core::Error::from(
                core::num::NonZeroU32::new(1).expect("static"),
            )
        })
    }
}

impl CryptoRng for OsRng {}

/// Packs an `Err(errors::Error)` result (disc 1).
fn ecdsa_err(msg: &str) -> i128 {
    let err = crate::c_abi::errors::error_new_from_bytes(msg.as_bytes());
    gos_rt_result_new(1, err as i64)
}

unsafe fn cstr<'a>(p: *const c_char) -> &'a str {
    unsafe { crate::c_abi::gos_str_arg_text(p) }
}

/// Layout of the `(String, String)` pem keypair: both strings are owned by the
/// blob.
static KEYPAIR_STRINGS_META: [i64; 6] = [
    gossamer_abi::rc::RC_KIND_STRUCT,
    1,
    0,
    2,
    gossamer_abi::rc::RC_CHILD_RC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT,
    (gossamer_abi::rc::RC_CHILD_RC << gossamer_abi::rc::RC_CHILD_KIND_SHIFT) | 1,
];

/// `crypto::ecdsa::keypair_pem()
/// -> Result<(String, String), errors::Error>` - fresh P-256 keypair
/// `(secret_pkcs8_pem, public_spki_pem)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_keypair_pem() -> i128 {
    ffi_entry!(0i128, {
        use p256::ecdsa::SigningKey;
        use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};

        // Probe the OS RNG before generating, so an entropy failure is
        // a clean Err rather than an invalid all-zero key.
        let mut probe = [0u8; 1];
        if getrandom::fill(&mut probe).is_err() {
            return ecdsa_err("ecdsa: rng failure");
        }
        let signing = SigningKey::random(&mut OsRng);
        let secret_pem = match signing.to_pkcs8_pem(LineEnding::LF) {
            Ok(p) => p.to_string(),
            Err(e) => return ecdsa_err(&format!("ecdsa: encode secret: {e}")),
        };
        let public_pem = match signing.verifying_key().to_public_key_pem(LineEnding::LF) {
            Ok(p) => p,
            Err(e) => return ecdsa_err(&format!("ecdsa: encode public: {e}")),
        };
        let secret_ptr = super::string::alloc_cstring(secret_pem.as_bytes()) as i64;
        let public_ptr = super::string::alloc_cstring(public_pem.as_bytes()) as i64;
        let pair =
            crate::c_abi::rc::counted_words(&[secret_ptr, public_ptr], &KEYPAIR_STRINGS_META);
        gos_rt_result_new(0, pair as i64)
    })
}

/// `crypto::ecdsa::sign_pem(secret_pem, message)
/// -> Result<Vec<u8>, errors::Error>` - DER-encoded signature.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_sign_pem(
    secret_pem: *const c_char,
    message: *const GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        use p256::ecdsa::{Signature, SigningKey, signature::Signer};
        use p256::pkcs8::DecodePrivateKey;

        let msg = unsafe { gosvec_u8(message) };
        let signing = match SigningKey::from_pkcs8_pem(unsafe { cstr(secret_pem) }) {
            Ok(k) => k,
            Err(e) => return ecdsa_err(&format!("ecdsa: secret pem: {e}")),
        };
        let sig: Signature = signing.sign(&msg);
        let der = sig.to_der();
        gos_rt_result_new(0, bytes_to_gosvec(der.as_bytes()) as i64)
    })
}

/// `crypto::ecdsa::verify_pem(public_pem, message, signature)
/// -> Result<(), errors::Error>` - `Ok(())` on a valid DER signature.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gos_rt_crypto_ecdsa_verify_pem(
    public_pem: *const c_char,
    message: *const GosVec,
    signature: *const GosVec,
) -> i128 {
    ffi_entry!(0i128, {
        use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
        use p256::pkcs8::DecodePublicKey;

        let msg = unsafe { gosvec_u8(message) };
        let sig_bytes = unsafe { gosvec_u8(signature) };
        let key = match VerifyingKey::from_public_key_pem(unsafe { cstr(public_pem) }) {
            Ok(k) => k,
            Err(e) => return ecdsa_err(&format!("ecdsa: public pem: {e}")),
        };
        let sig = match Signature::from_der(&sig_bytes) {
            Ok(s) => s,
            Err(e) => return ecdsa_err(&format!("ecdsa: signature: {e}")),
        };
        match key.verify(&msg, &sig) {
            Ok(()) => gos_rt_result_new(0, 0),
            Err(e) => ecdsa_err(&format!("ecdsa: verify: {e}")),
        }
    })
}
