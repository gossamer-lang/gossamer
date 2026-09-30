//! Lowering `hash` and `crypto` free functions.

use super::*;

impl<'a> Builder<'a> {
    pub(super) fn lower_hash_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            // 0.10.0 - hash::* checksums previously VM-only.
            "hash::crc32::checksum" => (
                "gos_rt_hash_crc32_checksum",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::crc32::checksum_string" => (
                "gos_rt_hash_crc32_checksum_string",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::crc32::update" => (
                "gos_rt_hash_crc32_update",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::crc32::update_window" => (
                "gos_rt_hash_crc32_update_window",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::crc32c::checksum" => (
                "gos_rt_hash_crc32c_checksum",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::crc32c::checksum_string" => (
                "gos_rt_hash_crc32c_checksum_string",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::crc32c::update" => (
                "gos_rt_hash_crc32c_update",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::crc32c::update_window" => (
                "gos_rt_hash_crc32c_update_window",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::adler32::checksum" => (
                "gos_rt_hash_adler32_checksum",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::adler32::checksum_string" => (
                "gos_rt_hash_adler32_checksum_string",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::adler32::update" => (
                "gos_rt_hash_adler32_update",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::fnv::hash32" => (
                "gos_rt_hash_fnv32",
                self.tcx.int_ty(gossamer_types::IntTy::U32),
            ),
            "hash::fnv::hash64" => (
                "gos_rt_hash_fnv64",
                self.tcx.int_ty(gossamer_types::IntTy::U64),
            ),
            "hash::fnv::hash_string" => (
                "gos_rt_hash_fnv_string",
                self.tcx.int_ty(gossamer_types::IntTy::U64),
            ),
            _ => return None,
        })
    }

    pub(super) fn lower_crypto_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "crypto::subtle::constant_time_eq" => {
                ("gos_rt_crypto_subtle_ct_eq", self.tcx.bool_ty())
            }
            "crypto::hmac::sha256_mac" | "hmac::sha256_mac" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_hmac_sha256_mac",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::rand::bytes" => ("gos_rt_crypto_rand_bytes", self.result_vec_u8_error_ty()),
            "crypto::password::hash" => (
                "gos_rt_crypto_password_hash",
                self.result_string_error_adt_ty(),
            ),
            "crypto::password::verify" => (
                "gos_rt_crypto_password_verify",
                self.result_bool_error_adt_ty(),
            ),
            "crypto::password::needs_rehash" => {
                ("gos_rt_crypto_password_needs_rehash", self.tcx.bool_ty())
            }
            "crypto::kdf::pbkdf2_sha256" | "kdf::pbkdf2_sha256" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let v = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
                ("gos_rt_crypto_pbkdf2_sha256", v)
            }
            "crypto::kdf::scrypt_interactive" | "kdf::scrypt_interactive" => (
                "gos_rt_crypto_scrypt_interactive",
                self.result_vec_u8_error_ty(),
            ),
            "crypto::kdf::argon2id_hash" | "kdf::argon2id_hash" => (
                "gos_rt_crypto_argon2id_hash",
                self.result_string_error_adt_ty(),
            ),
            "crypto::kdf::argon2id_verify" | "kdf::argon2id_verify" => (
                "gos_rt_crypto_argon2id_verify",
                self.result_bool_error_adt_ty(),
            ),
            "crypto::aead::aes_256_gcm_seal" | "aead::aes_256_gcm_seal" => (
                "gos_rt_crypto_aes256gcm_seal",
                self.result_vec_u8_error_ty(),
            ),
            "crypto::aead::aes_256_gcm_open" | "aead::aes_256_gcm_open" => (
                "gos_rt_crypto_aes256gcm_open",
                self.result_vec_u8_error_ty(),
            ),
            "crypto::aead::chacha20_poly1305_seal" | "aead::chacha20_poly1305_seal" => (
                "gos_rt_crypto_chacha20poly1305_seal",
                self.result_vec_u8_error_ty(),
            ),
            "crypto::aead::chacha20_poly1305_open" | "aead::chacha20_poly1305_open" => (
                "gos_rt_crypto_chacha20poly1305_open",
                self.result_vec_u8_error_ty(),
            ),
            "crypto::x509::verify_server_certificate_with_crls"
            | "x509::verify_server_certificate_with_crls" => (
                "gos_rt_x509_verify_server_certificate_with_crls",
                self.result_unit_error_adt_ty(),
            ),
            "crypto::ed25519::keypair" | "ed25519::keypair" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                let vec_u8 = self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty));
                let tup = self
                    .tcx
                    .intern(gossamer_types::TyKind::Tuple(vec![vec_u8, vec_u8]));
                ("gos_rt_crypto_ed25519_keypair", self.result_of(tup))
            }
            "crypto::ed25519::sign" | "ed25519::sign" => {
                ("gos_rt_crypto_ed25519_sign", self.result_vec_u8_error_ty())
            }
            "crypto::ed25519::verify" | "ed25519::verify" => (
                "gos_rt_crypto_ed25519_verify",
                self.result_unit_error_adt_ty(),
            ),
            "crypto::ecdsa::keypair_pem" | "ecdsa::keypair_pem" => {
                let s = self.tcx.string_ty();
                let tup = self.tcx.intern(gossamer_types::TyKind::Tuple(vec![s, s]));
                ("gos_rt_crypto_ecdsa_keypair_pem", self.result_of(tup))
            }
            "crypto::ecdsa::sign_pem" | "ecdsa::sign_pem" => (
                "gos_rt_crypto_ecdsa_sign_pem",
                self.result_vec_u8_error_ty(),
            ),
            _ => return None,
        })
    }

    pub(super) fn lower_crypto_2_free(
        &mut self,
        joined: &str,
        _args: &[HirExpr],
    ) -> Option<(&'static str, gossamer_types::Ty)> {
        Some(match joined {
            "crypto::ecdsa::verify_pem" | "ecdsa::verify_pem" => (
                "gos_rt_crypto_ecdsa_verify_pem",
                self.result_unit_error_adt_ty(),
            ),
            "jwt::sign_hs" => ("gos_rt_jwt_sign_hs", self.result_string_error_adt_ty()),
            "jwt::verify_hs" => ("gos_rt_jwt_verify_hs", self.result_string_error_adt_ty()),
            "jwt::verify" => ("gos_rt_jwt_verify", self.result_string_error_adt_ty()),
            "jwt::header" => ("gos_rt_jwt_header", self.result_string_error_adt_ty()),
            "jwt::sign_es256" => ("gos_rt_jwt_sign_es256", self.result_string_error_adt_ty()),
            "jwt::verify_es256" => ("gos_rt_jwt_verify_es256", self.result_string_error_adt_ty()),
            "jwt::sign_eddsa" => ("gos_rt_jwt_sign_eddsa", self.result_string_error_adt_ty()),
            "jwt::verify_eddsa" => ("gos_rt_jwt_verify_eddsa", self.result_string_error_adt_ty()),
            "crypto::sha256::hex" | "sha256::hex" | "crypto::sha256_hex" => {
                ("gos_rt_sha256_hex", self.tcx.string_ty())
            }
            "crypto::sha256::digest" | "sha256::digest" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_sha256_digest",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::sha512::digest" | "sha512::digest" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_sha512_digest",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::blake3::digest" | "blake3::digest" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_blake3_digest",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::insecure::md5" | "insecure::md5" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_md5",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::insecure::md5_hex" | "insecure::md5_hex" => {
                ("gos_rt_crypto_md5_hex", self.tcx.string_ty())
            }
            "crypto::insecure::sha1" | "insecure::sha1" => {
                let u8_ty = self.tcx.int_ty(gossamer_types::IntTy::U8);
                (
                    "gos_rt_crypto_sha1",
                    self.tcx.intern(gossamer_types::TyKind::Vec(u8_ty)),
                )
            }
            "crypto::insecure::sha1_hex" | "insecure::sha1_hex" => {
                ("gos_rt_crypto_sha1_hex", self.tcx.string_ty())
            }
            "crypto::sha512::hex" | "sha512::hex" | "crypto::sha512_hex" => {
                ("gos_rt_sha512_hex", self.tcx.string_ty())
            }
            "crypto::blake3::hex" | "blake3::hex" | "crypto::blake3_hex" => {
                ("gos_rt_blake3_hex", self.tcx.string_ty())
            }
            "crypto::hmac::sha256_hex" | "hmac::sha256_hex" | "crypto::hmac_sha256_hex" => {
                ("gos_rt_hmac_sha256_hex", self.tcx.string_ty())
            }
            _ => return None,
        })
    }
}
