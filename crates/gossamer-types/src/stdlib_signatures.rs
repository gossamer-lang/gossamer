//! Checker-owned source-facing stdlib signatures.
//!
//! This is the public signature surface exposed by `gossamer-types` for docs,
//! diagnostics, and stdlib drift tests. The call checker still has a few
//! specialized paths for inference-sensitive builtins; those paths should
//! converge on this module rather than growing a separate docs table.

#![allow(clippy::too_many_lines)]

/// Source-facing signature metadata for one stdlib function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdFunctionSignature {
    /// Module path, e.g. `std::strings`.
    pub module_path: &'static str,
    /// Function name inside the module.
    pub name: &'static str,
    /// Complete source-facing callable signature.
    pub signature: &'static str,
}

/// One parameter parsed out of a source-facing stdlib signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdSignatureParam {
    /// Parameter name.
    pub name: &'static str,
    /// Source-facing parameter type text.
    pub ty: &'static str,
}

/// Parsed shape of one source-facing stdlib function signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdSignatureShape {
    /// Source-facing parameters in declaration order.
    pub params: Vec<StdSignatureParam>,
    /// Source-facing return type text.
    pub return_ty: &'static str,
}

/// Checker-owned source-facing signature rows for stdlib functions.
pub const STD_FUNCTION_SIGNATURES: &[StdFunctionSignature] = &[
    StdFunctionSignature {
        module_path: "std::archive::tar",
        name: "read",
        signature: "fn read(data: Vec<u8>) -> Result<Vec<tar::TarEntry>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::archive::tar",
        name: "write",
        signature: "fn write(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::archive::zip",
        name: "read",
        signature: "fn read(data: Vec<u8>) -> Result<Vec<zip::ZipEntry>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::archive::zip",
        name: "write",
        signature: "fn write(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::bufio",
        name: "read_lines",
        signature: "fn read_lines(path: String) -> Result<Vec<String>, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::bufio",
        name: "read_lines_of",
        signature: "fn read_lines_of(path: String) -> Result<Vec<String>, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::bufio",
        name: "read_to_string",
        signature: "fn read_to_string(path: String) -> Result<String, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::bufio",
        name: "split_whitespace",
        signature: "fn split_whitespace(text: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::bytes",
        name: "index_of",
        signature: "fn index_of(haystack: Vec<u8>, needle: Vec<u8>) -> Option<i64>",
    },
    StdFunctionSignature {
        module_path: "std::bytes",
        name: "replace",
        signature: "fn replace(haystack: Vec<u8>, from: Vec<u8>, to: Vec<u8>) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::bytes",
        name: "split",
        signature: "fn split(haystack: Vec<u8>, sep: Vec<u8>) -> Vec<Vec<u8>>",
    },
    StdFunctionSignature {
        module_path: "std::compress::bzip2",
        name: "compress",
        signature: "fn compress(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::compress::bzip2",
        name: "decompress",
        signature: "fn decompress(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::compress::flate",
        name: "compress",
        signature: "fn compress(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::compress::flate",
        name: "decompress",
        signature: "fn decompress(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::compress::gzip",
        name: "decode",
        signature: "fn decode(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::compress::gzip",
        name: "encode",
        signature: "fn encode(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::compress::zlib",
        name: "compress",
        signature: "fn compress(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::compress::zlib",
        name: "decompress",
        signature: "fn decompress(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::compress::zstd",
        name: "decode",
        signature: "fn decode(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::compress::zstd",
        name: "encode",
        signature: "fn encode(data: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::compress::zstd",
        name: "encode_level",
        signature: "fn encode_level(data: Vec<u8>, level: i64) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::aead",
        name: "aes_256_gcm_open",
        signature: "fn aes_256_gcm_open(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::aead",
        name: "aes_256_gcm_seal",
        signature: "fn aes_256_gcm_seal(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::aead",
        name: "chacha20_poly1305_open",
        signature: "fn chacha20_poly1305_open(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::aead",
        name: "chacha20_poly1305_seal",
        signature: "fn chacha20_poly1305_seal(key: Vec<u8>, nonce: Vec<u8>, data: Vec<u8>, aad: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::blake3",
        name: "digest",
        signature: "fn digest(data: Vec<u8>) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::blake3",
        name: "hex",
        signature: "fn hex(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::crypto::ecdsa",
        name: "keypair_pem",
        signature: "fn keypair_pem() -> Result<(String, String), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::ecdsa",
        name: "sign_pem",
        signature: "fn sign_pem(secret_pem: String, message: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::ecdsa",
        name: "verify_pem",
        signature: "fn verify_pem(public_pem: String, message: Vec<u8>, signature: Vec<u8>) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::ed25519",
        name: "keypair",
        signature: "fn keypair() -> Result<(Vec<u8>, Vec<u8>), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::ed25519",
        name: "sign",
        signature: "fn sign(secret: Vec<u8>, message: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::ed25519",
        name: "verify",
        signature: "fn verify(public: Vec<u8>, message: Vec<u8>, signature: Vec<u8>) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::hmac",
        name: "sha256_hex",
        signature: "fn sha256_hex(key: String, message: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::crypto::hmac",
        name: "sha256_mac",
        signature: "fn sha256_mac(key: Vec<u8>, message: Vec<u8>) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::insecure",
        name: "md5",
        signature: "fn md5(data: Vec<u8>) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::insecure",
        name: "md5_hex",
        signature: "fn md5_hex(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::crypto::insecure",
        name: "sha1",
        signature: "fn sha1(data: Vec<u8>) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::insecure",
        name: "sha1_hex",
        signature: "fn sha1_hex(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::crypto::kdf",
        name: "argon2id_hash",
        signature: "fn argon2id_hash(password: Vec<u8>) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::kdf",
        name: "argon2id_verify",
        signature: "fn argon2id_verify(password: Vec<u8>, phc: String) -> Result<bool, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::kdf",
        name: "pbkdf2_sha256",
        signature: "fn pbkdf2_sha256(password: Vec<u8>, salt: Vec<u8>, iterations: i64, length: i64) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::kdf",
        name: "scrypt_interactive",
        signature: "fn scrypt_interactive(password: Vec<u8>, salt: Vec<u8>) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::password",
        name: "hash",
        signature: "fn hash(password: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::password",
        name: "needs_rehash",
        signature: "fn needs_rehash(hash: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::crypto::password",
        name: "verify",
        signature: "fn verify(password: String, hash: String) -> Result<bool, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::rand",
        name: "bytes",
        signature: "fn bytes(n: i64) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::sha256",
        name: "digest",
        signature: "fn digest(data: Vec<u8>) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::sha256",
        name: "hex",
        signature: "fn hex(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::crypto::sha512",
        name: "digest",
        signature: "fn digest(data: Vec<u8>) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::sha512",
        name: "hex",
        signature: "fn hex(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::crypto::subtle",
        name: "constant_time_eq",
        signature: "fn constant_time_eq(a: Vec<u8>, b: Vec<u8>) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::crypto::x509",
        name: "parse_pem",
        signature: "fn parse_pem(pem: String) -> Result<x509::Certificate, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::crypto::x509",
        name: "verify_server_certificate_with_crls",
        signature: "fn verify_server_certificate_with_crls(chain_pem: String, roots_pem: String, hostname: String, crl_pem: String) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::image",
        name: "new",
        signature: "fn new(width: i64, height: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::image",
        name: "filled",
        signature: "fn filled(width: i64, height: i64, rgba: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::image",
        name: "decode_base64",
        signature: "fn decode_base64(data: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::image",
        name: "width",
        signature: "fn width(image: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::image",
        name: "height",
        signature: "fn height(image: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::image",
        name: "pixel",
        signature: "fn pixel(image: i64, x: i64, y: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::image",
        name: "set_pixel",
        signature: "fn set_pixel(image: i64, x: i64, y: i64, rgba: i64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::image",
        name: "encode_png_base64",
        signature: "fn encode_png_base64(image: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::image",
        name: "encode_jpeg_base64",
        signature: "fn encode_jpeg_base64(image: i64, quality: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::database::sql",
        name: "drivers",
        signature: "fn drivers() -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::database::sql",
        name: "migrate_up",
        signature: "fn migrate_up(conn: database::sql::Conn, dir: String) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::database::sql",
        name: "open",
        signature: "fn open(driver: String, url: String) -> Result<database::sql::Conn, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::database::sql",
        name: "register_native",
        signature: "fn register_native(name: String, driver: database::sql::Driver) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::encoding::ascii85",
        name: "decode",
        signature: "fn decode(text: String) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::ascii85",
        name: "encode",
        signature: "fn encode(data: Vec<u8>) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::base32",
        name: "decode",
        signature: "fn decode(text: String) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::base32",
        name: "decode_hex",
        signature: "fn decode_hex(text: String) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::base32",
        name: "decode_string",
        signature: "fn decode_string(text: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::base32",
        name: "encode",
        signature: "fn encode(data: Vec<u8>) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::base32",
        name: "encode_hex",
        signature: "fn encode_hex(data: Vec<u8>) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::base32",
        name: "encode_string",
        signature: "fn encode_string(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::base64",
        name: "decode",
        signature: "fn decode(text: String) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::base64",
        name: "encode",
        signature: "fn encode(data: Vec<u8>) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u16_be_at",
        signature: "fn get_u16_be_at(bytes: Vec<u8>, offset: i64) -> Result<u16, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u16_be_at",
        signature: "fn put_u16_be_at(buf: &mut Vec<u8>, offset: i64, value: u16) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u16_le_at",
        signature: "fn get_u16_le_at(bytes: Vec<u8>, offset: i64) -> Result<u16, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u16_le_at",
        signature: "fn put_u16_le_at(buf: &mut Vec<u8>, offset: i64, value: u16) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u32_be_at",
        signature: "fn get_u32_be_at(bytes: Vec<u8>, offset: i64) -> Result<u32, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u32_be_at",
        signature: "fn put_u32_be_at(buf: &mut Vec<u8>, offset: i64, value: u32) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u32_le_at",
        signature: "fn get_u32_le_at(bytes: Vec<u8>, offset: i64) -> Result<u32, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u32_le_at",
        signature: "fn put_u32_le_at(buf: &mut Vec<u8>, offset: i64, value: u32) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u64_be_at",
        signature: "fn get_u64_be_at(bytes: Vec<u8>, offset: i64) -> Result<u64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u64_be_at",
        signature: "fn put_u64_be_at(buf: &mut Vec<u8>, offset: i64, value: u64) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u64_le_at",
        signature: "fn get_u64_le_at(bytes: Vec<u8>, offset: i64) -> Result<u64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u64_le_at",
        signature: "fn put_u64_le_at(buf: &mut Vec<u8>, offset: i64, value: u64) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u16_be",
        signature: "fn get_u16_be(bytes: Vec<u8>) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u16_le",
        signature: "fn get_u16_le(bytes: Vec<u8>) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u32_be",
        signature: "fn get_u32_be(bytes: Vec<u8>) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u32_le",
        signature: "fn get_u32_le(bytes: Vec<u8>) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u64_be",
        signature: "fn get_u64_be(bytes: Vec<u8>) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u64_le",
        signature: "fn get_u64_le(bytes: Vec<u8>) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "get_u8",
        signature: "fn get_u8(bytes: Vec<u8>) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u16_be",
        signature: "fn put_u16_be(buf: Vec<u8>, value: i64) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u16_le",
        signature: "fn put_u16_le(buf: Vec<u8>, value: i64) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u32_be",
        signature: "fn put_u32_be(buf: Vec<u8>, value: i64) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u32_le",
        signature: "fn put_u32_le(buf: Vec<u8>, value: i64) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u64_be",
        signature: "fn put_u64_be(buf: Vec<u8>, value: i64) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u64_le",
        signature: "fn put_u64_le(buf: Vec<u8>, value: i64) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_u8",
        signature: "fn put_u8(buf: Vec<u8>, value: i64) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_uvarint",
        signature: "fn put_uvarint(buf: Vec<u8>, value: i64) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "put_varint",
        signature: "fn put_varint(buf: Vec<u8>, value: i64) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "uvarint",
        signature: "fn uvarint(bytes: Vec<u8>) -> Result<(i64, i64), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::binary",
        name: "varint",
        signature: "fn varint(bytes: Vec<u8>) -> Result<(i64, i64), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::csv",
        name: "parse_line",
        signature: "fn parse_line(line: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::csv",
        name: "read",
        signature: "fn read(text: String) -> Result<Vec<Vec<String>>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::csv",
        name: "write",
        signature: "fn write(rows: Vec<Vec<String>>) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::hex",
        name: "decode",
        signature: "fn decode(text: String) -> Result<Vec<u8>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::hex",
        name: "encode",
        signature: "fn encode(data: Vec<u8>) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "as_array",
        signature: "fn as_array(value: json::Value) -> Option<Vec<json::Value>>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "as_bool",
        signature: "fn as_bool(value: json::Value) -> Option<bool>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "as_f64",
        signature: "fn as_f64(value: json::Value) -> Option<f64>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "as_i64",
        signature: "fn as_i64(value: json::Value) -> Option<i64>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "as_str",
        signature: "fn as_str(value: json::Value) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "at",
        signature: "fn at(value: json::Value, index: i64) -> json::Value",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "decode",
        signature: "fn decode(source: String) -> Result<json::Value, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "encode",
        signature: "fn encode(value: json::Value) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "encode_pretty",
        signature: "fn encode_pretty(value: json::Value) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "get",
        signature: "fn get(value: json::Value, key: String) -> Option<json::Value>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "is_null",
        signature: "fn is_null(value: json::Value) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "keys",
        signature: "fn keys(value: json::Value) -> Option<Vec<String>>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "len",
        signature: "fn len(value: json::Value) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "parse",
        signature: "fn parse(source: String) -> Result<json::Value, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "render",
        signature: "fn render(value: json::Value) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "set",
        signature: "fn set(value: json::Value, key: String, next: json::Value) -> json::Value",
    },
    StdFunctionSignature {
        module_path: "std::encoding::json",
        name: "valid",
        signature: "fn valid(source: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::encoding::pem",
        name: "decode",
        signature: "fn decode(data: String) -> Result<pem::Block, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::pem",
        name: "decode_all",
        signature: "fn decode_all(data: String) -> Result<Vec<pem::Block>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::pem",
        name: "encode",
        signature: "fn encode(block: pem::Block) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::toml",
        name: "from_json",
        signature: "fn from_json(source: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::toml",
        name: "is_valid",
        signature: "fn is_valid(source: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::encoding::toml",
        name: "pretty",
        signature: "fn pretty(source: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::toml",
        name: "to_json",
        signature: "fn to_json(source: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::xml",
        name: "encode",
        signature: "fn encode(value: json::Value) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::xml",
        name: "escape",
        signature: "fn escape(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::encoding::xml",
        name: "parse",
        signature: "fn parse(source: String) -> Result<json::Value, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::yaml",
        name: "encode",
        signature: "fn encode(value: json::Value) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::yaml",
        name: "from_json",
        signature: "fn from_json(source: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::yaml",
        name: "is_valid",
        signature: "fn is_valid(source: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::encoding::yaml",
        name: "parse",
        signature: "fn parse(source: String) -> Result<json::Value, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::yaml",
        name: "parse_all",
        signature: "fn parse_all(source: String) -> Result<Vec<json::Value>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::encoding::yaml",
        name: "to_json",
        signature: "fn to_json(source: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::env",
        name: "args",
        signature: "fn args() -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::env",
        name: "current_dir",
        signature: "fn current_dir() -> Result<String, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::env",
        name: "home_dir",
        signature: "fn home_dir() -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::env",
        name: "program_name",
        signature: "fn program_name() -> String",
    },
    StdFunctionSignature {
        module_path: "std::env",
        name: "set_current_dir",
        signature: "fn set_current_dir(path: String) -> Result<(), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::env",
        name: "set_var",
        signature: "fn set_var(name: String, value: String) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::env",
        name: "vars",
        signature: "fn vars() -> Map<String, String>",
    },
    StdFunctionSignature {
        module_path: "std::env",
        name: "temp_dir",
        signature: "fn temp_dir() -> String",
    },
    StdFunctionSignature {
        module_path: "std::env",
        name: "unset_var",
        signature: "fn unset_var(name: String) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::env",
        name: "var",
        signature: "fn var(name: String) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::errors",
        name: "is",
        signature: "fn is(error: errors::Error, needle: T) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::errors",
        name: "join",
        signature: "fn join(errors: Vec<errors::Error>) -> Option<errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::errors",
        name: "new",
        signature: "fn new(message: String) -> errors::Error",
    },
    StdFunctionSignature {
        module_path: "std::errors",
        name: "newf",
        signature: "fn newf(format: String, args: Vec<String>) -> errors::Error",
    },
    StdFunctionSignature {
        module_path: "std::errors",
        name: "wrap",
        signature: "fn wrap(error: errors::Error, context: String) -> errors::Error",
    },
    StdFunctionSignature {
        module_path: "std::flag",
        name: "bool",
        signature: "fn bool(name: String, default: bool, usage: String, short: char) -> flag::Flag",
    },
    StdFunctionSignature {
        module_path: "std::flag",
        name: "define",
        signature: "fn define(name: String, flags: Vec<flag::Flag>) -> flag::FlagSet",
    },
    StdFunctionSignature {
        module_path: "std::flag",
        name: "int",
        signature: "fn int(name: String, default: i64, usage: String, short: char) -> flag::Flag",
    },
    StdFunctionSignature {
        module_path: "std::flag",
        name: "parse",
        signature: "fn parse(args: Vec<String>) -> Result<Vec<String>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::flag",
        name: "string",
        signature: "fn string(name: String, default: String, usage: String, short: char) -> flag::Flag",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "canonicalize",
        signature: "fn canonicalize(path: String) -> Result<String, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "copy",
        signature: "fn copy(src: String, dst: String) -> Result<i64, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "create",
        signature: "fn create(path: String) -> Result<fs::File, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "sync_dir",
        signature: "fn sync_dir(path: String) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "create_dir",
        signature: "fn create_dir(path: String) -> Result<(), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "create_dir_all",
        signature: "fn create_dir_all(path: String) -> Result<(), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "create_dir_mode",
        signature: "fn create_dir_mode(path: String, mode: i64) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "create_dir_all_mode",
        signature: "fn create_dir_all_mode(path: String, mode: i64) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "write_mode",
        signature: "fn write_mode(path: String, contents: String, mode: i64) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "permissions",
        signature: "fn permissions(path: String) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "set_permissions",
        signature: "fn set_permissions(path: String, mode: i64) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "temp_dir",
        signature: "fn temp_dir(prefix: String) -> Result<String, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "temp_file",
        signature: "fn temp_file(prefix: String) -> Result<(fs::File, String), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "exists",
        signature: "fn exists(path: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "file_size",
        signature: "fn file_size(path: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "is_dir",
        signature: "fn is_dir(path: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "is_file",
        signature: "fn is_file(path: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "is_symlink",
        signature: "fn is_symlink(path: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "metadata",
        signature: "fn metadata(path: String) -> Result<fs::Metadata, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "open",
        signature: "fn open(path: String) -> Result<fs::File, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "read",
        signature: "fn read(path: String) -> Result<Vec<u8>, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "read_dir",
        signature: "fn read_dir(path: String) -> Result<Vec<fs::DirInfo>, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "read_to_string",
        signature: "fn read_to_string(path: String) -> Result<String, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "remove_dir",
        signature: "fn remove_dir(path: String) -> Result<(), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "remove_dir_all",
        signature: "fn remove_dir_all(path: String) -> Result<(), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "remove_file",
        signature: "fn remove_file(path: String) -> Result<(), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "rename",
        signature: "fn rename(src: String, dst: String) -> Result<(), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "walk_dir",
        signature: "fn walk_dir(path: String, visit: Fn(fs::DirInfo) -> Result<(), io::Error>) -> Result<(), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::fs",
        name: "write",
        signature: "fn write(path: String, contents: Vec<u8>) -> Result<(), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::hash::adler32",
        name: "checksum",
        signature: "fn checksum(data: Vec<u8>) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::hash::adler32",
        name: "checksum_string",
        signature: "fn checksum_string(text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::hash::adler32",
        name: "update",
        signature: "fn update(seed: i64, data: Vec<u8>) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::hash::crc32",
        name: "checksum",
        signature: "fn checksum(data: Vec<u8>) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::hash::crc32",
        name: "checksum_string",
        signature: "fn checksum_string(text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::hash::crc32",
        name: "update",
        signature: "fn update(seed: i64, data: Vec<u8>) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::hash::crc32",
        name: "update_window",
        signature: "fn update_window(seed: i64, data: Vec<u8>, start: i64, end: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::hash::fnv",
        name: "hash32",
        signature: "fn hash32(data: Vec<u8>) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::hash::fnv",
        name: "hash64",
        signature: "fn hash64(data: Vec<u8>) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::hash::fnv",
        name: "hash_string",
        signature: "fn hash_string(text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::html",
        name: "escape",
        signature: "fn escape(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::html",
        name: "unescape",
        signature: "fn unescape(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::html::template",
        name: "render_json",
        signature: "fn render_json(template: String, data: json::Value) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "delete",
        signature: "fn delete(url: String, body: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "get",
        signature: "fn get(url: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "head",
        signature: "fn head(url: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "options",
        signature: "fn options(url: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "post",
        signature: "fn post(url: String, body: String, content_type: String) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "put",
        signature: "fn put(url: String, body: String, content_type: String) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "request",
        signature: "fn request(method: String, url: String, body: String, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "request_bytes",
        signature: "fn request_bytes(method: String, url: String, body: Vec<u8>, headers: Vec<(String, String)>) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "serve",
        signature: "fn serve(addr: String, handler: http::Handler) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "serve_h2c",
        signature: "fn serve_h2c(addr: String, handler: http::Handler) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "serve_tls",
        signature: "fn serve_tls(addr: String, cert_pem: String, key_pem: String, handler: http::Handler) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http",
        name: "stream",
        signature: "fn stream(method: String, url: String, body: String, headers: Vec<(String, String)>) -> Result<http::ResponseStream, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::chunked",
        name: "decode",
        signature: "fn decode(body: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::http::chunked",
        name: "encode",
        signature: "fn encode(body: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::http::cookie",
        name: "parse_cookie_header",
        signature: "fn parse_cookie_header(header: String) -> Vec<http::cookie::Cookie>",
    },
    StdFunctionSignature {
        module_path: "std::http::cookie",
        name: "serialize",
        signature: "fn serialize(name: String, value: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::http::csrf",
        name: "attach_cookie",
        signature: "fn attach_cookie(request: http::Request, secret: String) -> Result<http::Request, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::csrf",
        name: "check",
        signature: "fn check(request: http::Request, secret: String) -> Result<http::Request, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::csrf",
        name: "extract_token",
        signature: "fn extract_token(request: http::Request, secret: String) -> Result<http::Request, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::csrf",
        name: "issue_token",
        signature: "fn issue_token(secret: Vec<u8>) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::csrf",
        name: "origin_allowed",
        signature: "fn origin_allowed(request: http::Request, secret: String) -> Result<http::Request, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::csrf",
        name: "verify_token",
        signature: "fn verify_token(cookie_token: String, supplied_token: String, secret: Vec<u8>) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "accepts_gzip",
        signature: "fn accepts_gzip(request: http::Request) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "bearer_ok",
        signature: "fn bearer_ok(request: http::Request, verify: Fn(String) -> bool) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "decode_basic_auth",
        signature: "fn decode_basic_auth(request: http::Request) -> Option<(String, String)>",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "new_request_id",
        signature: "fn new_request_id() -> String",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "tag",
        signature: "fn tag(handler: http::Handler) -> http::Handler",
    },
    StdFunctionSignature {
        module_path: "std::http::multipart",
        name: "parse",
        signature: "fn parse(request: http::Request) -> Result<http::multipart::Form, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::native_client",
        name: "delete",
        signature: "fn delete(url: String) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::native_client",
        name: "get",
        signature: "fn get(url: String) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::native_client",
        name: "post",
        signature: "fn post(url: String, body: Vec<u8>, content_type: String) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::native_client",
        name: "put",
        signature: "fn put(url: String, body: Vec<u8>, content_type: String) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::proxy",
        name: "forward",
        signature: "fn forward(url: String, method: String, body: Vec<u8>) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::router",
        name: "add",
        signature: "fn add(router: http::router::Router, method: String, pattern: String) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::router",
        name: "lookup",
        signature: "fn lookup(router: http::router::Router, method: String, path: String) -> Option<http::router::Match>",
    },
    StdFunctionSignature {
        module_path: "std::http::router",
        name: "new",
        signature: "fn new() -> http::router::Router",
    },
    StdFunctionSignature {
        module_path: "std::http::session",
        name: "sign",
        signature: "fn sign(value: String, secret: Vec<u8>) -> String",
    },
    StdFunctionSignature {
        module_path: "std::http::session",
        name: "verify",
        signature: "fn verify(value: String, secret: Vec<u8>) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::session",
        name: "with_session",
        signature: "fn with_session(request: http::Request, secret: String) -> Result<http::Request, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::sse",
        name: "encode_comment",
        signature: "fn encode_comment(comment: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::http::sse",
        name: "encode_event",
        signature: "fn encode_event(event: String, data: String, id: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::http::sse",
        name: "encode_retry",
        signature: "fn encode_retry(ms: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::http::static_files",
        name: "mime_for_path",
        signature: "fn mime_for_path(path: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::http::static_files",
        name: "serve_file",
        signature: "fn serve_file(path: String) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::websocket",
        name: "accept",
        signature: "fn accept(request: http::Request) -> Result<http::websocket::Conn, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::websocket",
        name: "accept_key",
        signature: "fn accept_key(key: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::http::websocket",
        name: "close",
        signature: "fn close(conn: http::websocket::Conn) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::websocket",
        name: "connect",
        signature: "fn connect(url: String) -> Result<http::websocket::Conn, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::websocket",
        name: "is_websocket_upgrade",
        signature: "fn is_websocket_upgrade(request: http::Request) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::http::websocket",
        name: "recv",
        signature: "fn recv(conn: http::websocket::Conn) -> Result<http::websocket::Message, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::websocket",
        name: "send_binary",
        signature: "fn send_binary(conn: http::websocket::Conn, data: Vec<u8>) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::websocket",
        name: "send_text",
        signature: "fn send_text(conn: http::websocket::Conn, text: String) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http::websocket",
        name: "serve",
        signature: "fn serve(addr: String, handler: Fn(http::websocket::Conn) -> ()) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::http_h3",
        name: "serve",
        signature: "fn serve(addr: String, cert_path: String, key_path: String, handler: http_h3::Handler) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "Copy",
        signature: "fn Copy(dst: io::Writer, src: io::Reader) -> Result<i64, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "ReadAll",
        signature: "fn ReadAll(reader: io::Reader) -> Result<String, io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "stderr",
        signature: "fn stderr() -> io::Writer",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "stdin",
        signature: "fn stdin() -> io::Reader",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "stdout",
        signature: "fn stdout() -> io::Writer",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "all",
        signature: "fn all<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "any",
        signature: "fn any<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "chain",
        signature: "fn chain<T>(left: Vec<T>, right: Vec<T>) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "chunk_by",
        signature: "fn chunk_by<T, K: Eq>(items: Vec<T>, key: Fn(T) -> K) -> Map<K, Vec<T>>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "chunks",
        signature: "fn chunks<T>(items: Vec<T>, n: i64) -> Vec<Vec<T>>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "collect",
        signature: "fn collect<T>(items: Vec<T>) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "count",
        signature: "fn count<T>(items: Vec<T>) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "count_by",
        signature: "fn count_by<T, K: Eq>(items: Vec<T>, key: Fn(T) -> K) -> Map<K, i64>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "dedup",
        signature: "fn dedup<T: Eq>(items: Vec<T>) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "empty",
        signature: "fn empty<T>() -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "enumerate",
        signature: "fn enumerate<T>(items: Vec<T>) -> Iterator<(i64, T)>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "filter",
        signature: "fn filter<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "filter_map",
        signature: "fn filter_map<T, U>(items: Vec<T>, f: Fn(T) -> Option<U>) -> Vec<U>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "find",
        signature: "fn find<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "find_map",
        signature: "fn find_map<T, U>(items: Vec<T>, f: Fn(T) -> Option<U>) -> Option<U>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "flat_map",
        signature: "fn flat_map<T, U>(items: Vec<T>, f: Fn(T) -> Vec<U>) -> Vec<U>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "flatten",
        signature: "fn flatten<T>(items: Vec<Vec<T>>) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "fold",
        signature: "fn fold<T, U>(items: Vec<T>, init: U, f: Fn(U, T) -> U) -> U",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "for_each",
        signature: "fn for_each<T>(items: Vec<T>, f: Fn(T) -> ()) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "map",
        signature: "fn map<T, U>(items: Vec<T>, f: Fn(T) -> U) -> Vec<U>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "max",
        signature: "fn max<T: Ord>(items: Vec<T>) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "max_by",
        signature: "fn max_by<T>(items: Vec<T>, compare: Fn(T, T) -> i64) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "max_by_key",
        signature: "fn max_by_key<T, K: Ord>(items: Vec<T>, key: Fn(T) -> K) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "min",
        signature: "fn min<T: Ord>(items: Vec<T>) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "min_by",
        signature: "fn min_by<T>(items: Vec<T>, compare: Fn(T, T) -> i64) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "min_by_key",
        signature: "fn min_by_key<T, K: Ord>(items: Vec<T>, key: Fn(T) -> K) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "once",
        signature: "fn once<T>(value: T) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "pairwise",
        signature: "fn pairwise<T>(items: Vec<T>) -> Vec<(T, T)>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "partition",
        signature: "fn partition<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> (Vec<T>, Vec<T>)",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "position",
        signature: "fn position<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> Option<i64>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "product",
        signature: "fn product<T>(items: Vec<T>) -> T",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "product_by",
        signature: "fn product_by<T>(items: Vec<T>, f: Fn(T) -> i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "range",
        signature: "fn range(start: i64, end: i64) -> Vec<i64>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "range_inclusive",
        signature: "fn range_inclusive(start: i64, end: i64) -> Vec<i64>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "reduce",
        signature: "fn reduce<T>(items: Vec<T>, f: Fn(T, T) -> T) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "repeat",
        signature: "fn repeat<T>(value: T, count: i64) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "rev",
        signature: "fn rev<T>(items: Vec<T>) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "scan",
        signature: "fn scan<T, S>(items: Vec<T>, init: S, f: Fn(S, T) -> S) -> Vec<S>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "skip",
        signature: "fn skip<T>(items: Vec<T>, n: i64) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "skip_while",
        signature: "fn skip_while<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "sort_by",
        signature: "fn sort_by<T>(items: Vec<T>, compare: Fn(T, T) -> i64) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "sort_by_key",
        signature: "fn sort_by_key<T, K: Ord>(items: Vec<T>, key: Fn(T) -> K) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "sum",
        signature: "fn sum<T>(items: Vec<T>) -> T",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "sum_by",
        signature: "fn sum_by<T>(items: Vec<T>, f: Fn(T) -> i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "take",
        signature: "fn take<T>(items: Vec<T>, n: i64) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "take_while",
        signature: "fn take_while<T>(items: Vec<T>, predicate: Fn(T) -> bool) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "step_by",
        signature: "fn step_by<T>(items: Vec<T>, step: i64) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "unzip",
        signature: "fn unzip<A, B>(items: Vec<(A, B)>) -> (Vec<A>, Vec<B>)",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "windows",
        signature: "fn windows<T>(items: Vec<T>, n: i64) -> Vec<Vec<T>>",
    },
    StdFunctionSignature {
        module_path: "std::iter",
        name: "zip",
        signature: "fn zip<A, B>(left: Vec<A>, right: Vec<B>) -> Vec<(A, B)>",
    },
    StdFunctionSignature {
        module_path: "std::jwt",
        name: "sign_eddsa",
        signature: "fn sign_eddsa(claims_json: String, signing_key_pem: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::jwt",
        name: "sign_es256",
        signature: "fn sign_es256(claims_json: String, signing_key_pem: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::jwt",
        name: "sign_hs",
        signature: "fn sign_hs(alg: String, claims_json: String, key: Vec<u8>) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::jwt",
        name: "header",
        signature: "fn header(token: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::jwt",
        name: "verify",
        signature: "fn verify(token: String, alg: String, key: String, leeway_secs: i64, issuer: String, audience: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::jwt",
        name: "verify_eddsa",
        signature: "fn verify_eddsa(token: String, verifying_key_pem: String, leeway_secs: i64) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::jwt",
        name: "verify_es256",
        signature: "fn verify_es256(token: String, verifying_key_pem: String, leeway_secs: i64) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::jwt",
        name: "verify_hs",
        signature: "fn verify_hs(token: String, alg: String, key: Vec<u8>, leeway_secs: i64) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "abs",
        signature: "fn abs(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "acos",
        signature: "fn acos(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "asin",
        signature: "fn asin(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "atan",
        signature: "fn atan(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "atan2",
        signature: "fn atan2(y: f64, x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "cbrt",
        signature: "fn cbrt(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "ceil",
        signature: "fn ceil(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "clamp",
        signature: "fn clamp(x: f64, min: f64, max: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "copysign",
        signature: "fn copysign(x: f64, y: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "cos",
        signature: "fn cos(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "cosh",
        signature: "fn cosh(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "exp",
        signature: "fn exp(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "exp2",
        signature: "fn exp2(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "floor",
        signature: "fn floor(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "hypot",
        signature: "fn hypot(x: f64, y: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "is_inf",
        signature: "fn is_inf(x: f64, sign: i64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "is_nan",
        signature: "fn is_nan(x: f64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "ln",
        signature: "fn ln(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "log",
        signature: "fn log(x: f64, y: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "log10",
        signature: "fn log10(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "log2",
        signature: "fn log2(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "max",
        signature: "fn max(x: f64, y: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "min",
        signature: "fn min(x: f64, y: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "positive_diff",
        signature: "fn positive_diff(x: f64, y: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "pow",
        signature: "fn pow(x: f64, y: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "rem",
        signature: "fn rem(x: f64, y: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "round",
        signature: "fn round(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "sin",
        signature: "fn sin(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "sinh",
        signature: "fn sinh(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "sqrt",
        signature: "fn sqrt(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "tan",
        signature: "fn tan(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "tanh",
        signature: "fn tanh(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math",
        name: "trunc",
        signature: "fn trunc(x: f64) -> f64",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "factorial",
        signature: "fn factorial(n: i64) -> big::Uint",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_abs",
        signature: "fn int_abs(value: big::Int) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_add",
        signature: "fn int_add(a: big::Int, b: big::Int) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_cmp",
        signature: "fn int_cmp(a: big::Int, b: big::Int) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_div",
        signature: "fn int_div(a: big::Int, b: big::Int) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_from_i64",
        signature: "fn int_from_i64(value: i64) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_from_str",
        signature: "fn int_from_str(text: String) -> Result<big::Int, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_gcd",
        signature: "fn int_gcd(a: big::Int, b: big::Int) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_is_negative",
        signature: "fn int_is_negative(value: big::Int) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_is_positive",
        signature: "fn int_is_positive(value: big::Int) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_is_zero",
        signature: "fn int_is_zero(value: big::Int) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_lcm",
        signature: "fn int_lcm(a: big::Int, b: big::Int) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_mul",
        signature: "fn int_mul(a: big::Int, b: big::Int) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_neg",
        signature: "fn int_neg(value: big::Int) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_pow",
        signature: "fn int_pow(value: big::Int, exp: i64) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_rem",
        signature: "fn int_rem(a: big::Int, b: big::Int) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_sub",
        signature: "fn int_sub(a: big::Int, b: big::Int) -> big::Int",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_to_hex",
        signature: "fn int_to_hex(value: big::Int) -> String",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_to_i64",
        signature: "fn int_to_i64(value: big::Int) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "int_to_str",
        signature: "fn int_to_str(value: big::Int) -> String",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_add",
        signature: "fn uint_add(a: big::Uint, b: big::Uint) -> big::Uint",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_bit_len",
        signature: "fn uint_bit_len(value: big::Uint) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_from_str",
        signature: "fn uint_from_str(text: String) -> Result<big::Uint, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_from_u64",
        signature: "fn uint_from_u64(value: u64) -> big::Uint",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_is_zero",
        signature: "fn uint_is_zero(value: big::Uint) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_mul",
        signature: "fn uint_mul(a: big::Uint, b: big::Uint) -> big::Uint",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_pow",
        signature: "fn uint_pow(value: big::Uint, exp: i64) -> big::Uint",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_pow_mod",
        signature: "fn uint_pow_mod(value: big::Uint, exp: big::Uint, modulus: big::Uint) -> big::Uint",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_to_hex",
        signature: "fn uint_to_hex(value: big::Uint) -> String",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_to_str",
        signature: "fn uint_to_str(value: big::Uint) -> String",
    },
    StdFunctionSignature {
        module_path: "std::math::big",
        name: "uint_to_u64",
        signature: "fn uint_to_u64(value: big::Uint) -> Result<u64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "add",
        signature: "fn add(x: u64, y: u64, carry: u64) -> (u64, u64)",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "count_ones",
        signature: "fn count_ones(x: u64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "count_zeros",
        signature: "fn count_zeros(x: u64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "div",
        signature: "fn div(hi: u64, lo: u64, y: u64) -> (u64, u64)",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "leading_zeros",
        signature: "fn leading_zeros(x: u64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "len",
        signature: "fn len(x: u64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "mul",
        signature: "fn mul(x: u64, y: u64) -> (u64, u64)",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "reverse_bits",
        signature: "fn reverse_bits(x: u64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "reverse_bytes",
        signature: "fn reverse_bytes(x: u64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "rotate_left",
        signature: "fn rotate_left(x: u64, n: i64) -> u64",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "rotate_right",
        signature: "fn rotate_right(x: u64, n: i64) -> u64",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "sub",
        signature: "fn sub(x: u64, y: u64, borrow: u64) -> (u64, u64)",
    },
    StdFunctionSignature {
        module_path: "std::math::bits",
        name: "trailing_zeros",
        signature: "fn trailing_zeros(x: u64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::metrics",
        name: "serve_metrics",
        signature: "fn serve_metrics(addr: String) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::mime",
        name: "boundary",
        signature: "fn boundary(mime: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::mime",
        name: "charset",
        signature: "fn charset(mime: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::mime",
        name: "extension_by_type",
        signature: "fn extension_by_type(mime: String) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::mime",
        name: "is_valid",
        signature: "fn is_valid(value: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::mime",
        name: "param",
        signature: "fn param(mime: String, name: String) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::mime",
        name: "parse",
        signature: "fn parse(value: String) -> Result<mime::Mime, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::mime",
        name: "sub",
        signature: "fn sub(mime: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::mime",
        name: "top",
        signature: "fn top(mime: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::mime",
        name: "type_by_extension",
        signature: "fn type_by_extension(ext: String) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::net",
        name: "lookup",
        signature: "fn lookup(host: String) -> Result<Vec<String>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::net::ip",
        name: "is_loopback",
        signature: "fn is_loopback(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::ip",
        name: "is_multicast",
        signature: "fn is_multicast(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::ip",
        name: "is_private",
        signature: "fn is_private(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::ip",
        name: "is_unspecified",
        signature: "fn is_unspecified(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::ip",
        name: "is_v4",
        signature: "fn is_v4(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::ip",
        name: "is_v6",
        signature: "fn is_v6(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::ip",
        name: "is_valid",
        signature: "fn is_valid(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::ip",
        name: "octets",
        signature: "fn octets(addr: net::ip::Addr) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::net::ip",
        name: "parse",
        signature: "fn parse(addr: String) -> Result<net::ip::Addr, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::net::ip",
        name: "to_string",
        signature: "fn to_string(addr: net::ip::Addr) -> String",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "host_of",
        signature: "fn host_of(addr_port: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "is_loopback",
        signature: "fn is_loopback(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "is_multicast",
        signature: "fn is_multicast(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "is_private",
        signature: "fn is_private(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "is_unspecified",
        signature: "fn is_unspecified(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "is_v4",
        signature: "fn is_v4(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "is_v6",
        signature: "fn is_v6(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "is_valid",
        signature: "fn is_valid(addr: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "join_addr_port",
        signature: "fn join_addr_port(addr: String, port: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "normalize",
        signature: "fn normalize(addr: String) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::net::netip",
        name: "port_of",
        signature: "fn port_of(addr_port: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::net::url",
        name: "path_escape",
        signature: "fn path_escape(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::net::url",
        name: "path_unescape",
        signature: "fn path_unescape(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::net::url",
        name: "query_escape",
        signature: "fn query_escape(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::net::url",
        name: "query_unescape",
        signature: "fn query_unescape(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "and_then",
        signature: "fn and_then<T, U>(value: Option<T>, f: Fn(T) -> Option<U>) -> Option<U>",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "expect",
        signature: "fn expect<T>(value: Option<T>, message: String) -> T",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "filter",
        signature: "fn filter<T>(value: Option<T>, predicate: Fn(T) -> bool) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "flatten",
        signature: "fn flatten<T>(value: Option<Option<T>>) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "is_none",
        signature: "fn is_none<T>(value: Option<T>) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "is_some",
        signature: "fn is_some<T>(value: Option<T>) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "ok_or",
        signature: "fn ok_or<T, E>(value: Option<T>, err: E) -> Result<T, E>",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "ok_or_else",
        signature: "fn ok_or_else<T, E>(value: Option<T>, err: Fn() -> E) -> Result<T, E>",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "iter",
        signature: "fn iter<T>(value: Option<T>) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "map",
        signature: "fn map<T, U>(value: Option<T>, f: Fn(T) -> U) -> Option<U>",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "or",
        signature: "fn or<T>(value: Option<T>, fallback: Option<T>) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "or_else",
        signature: "fn or_else<T>(value: Option<T>, fallback: Fn() -> Option<T>) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "unwrap_or",
        signature: "fn unwrap_or<T>(value: Option<T>, fallback: T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "unwrap_or_else",
        signature: "fn unwrap_or_else<T>(value: Option<T>, fallback: Fn() -> T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "unwrap",
        signature: "fn unwrap<T>(value: Option<T>) -> T",
    },
    StdFunctionSignature {
        module_path: "std::option",
        name: "zip",
        signature: "fn zip<T, U>(value: Option<T>, other: Option<U>) -> Option<(T, U)>",
    },
    StdFunctionSignature {
        module_path: "std::os",
        name: "arch",
        signature: "fn arch() -> String",
    },
    StdFunctionSignature {
        module_path: "std::os",
        name: "family",
        signature: "fn family() -> String",
    },
    StdFunctionSignature {
        module_path: "std::os::exec",
        name: "kill",
        signature: "fn kill(pid: i64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::os::exec",
        name: "kill_group",
        signature: "fn kill_group(pid: i64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::os::exec",
        name: "pipeline_run",
        signature: "fn pipeline_run(commands: Vec<String>) -> Result<process::Output, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::os::exec",
        name: "run",
        signature: "fn run(program: String, args: Vec<String>) -> Result<process::Output, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::os::exec",
        name: "signal",
        signature: "fn signal(pid: i64, signum: i64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::os::exec",
        name: "spawn",
        signature: "fn spawn(program: String, args: Vec<String>) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::os::exec",
        name: "spawn_piped",
        signature: "fn spawn_piped(program: String, args: Vec<String>) -> Result<process::Child, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::os::exec",
        name: "wait_timeout",
        signature: "fn wait_timeout(pid: i64, ms: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::os::signal",
        name: "on",
        signature: "fn on(signum: i64) -> os::signal::Notifier",
    },
    StdFunctionSignature {
        module_path: "std::os::signal",
        name: "try_wait",
        signature: "fn try_wait(notifier: os::signal::Notifier) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::os::signal",
        name: "wait",
        signature: "fn wait(notifier: os::signal::Notifier) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::os::user",
        name: "current_gid",
        signature: "fn current_gid() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::os::user",
        name: "current_home",
        signature: "fn current_home() -> String",
    },
    StdFunctionSignature {
        module_path: "std::os::user",
        name: "current_name",
        signature: "fn current_name() -> String",
    },
    StdFunctionSignature {
        module_path: "std::os::user",
        name: "current_uid",
        signature: "fn current_uid() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::os::user",
        name: "lookup_name",
        signature: "fn lookup_name(name: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::os::user",
        name: "lookup_uid",
        signature: "fn lookup_uid(uid: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "walk",
        signature: "fn walk(path: String, visit: Fn(fs::DirInfo) -> Result<(), io::Error>) -> Result<(), io::Error>",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "extension",
        signature: "fn extension(path: String) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "file_name",
        signature: "fn file_name(path: String) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "file_stem",
        signature: "fn file_stem(path: String) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "is_absolute",
        signature: "fn is_absolute(path: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "join",
        signature: "fn join(base: String, segment: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "components",
        signature: "fn components(path: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "prefixes",
        signature: "fn prefixes(path: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "unique_prefixes",
        signature: "fn unique_prefixes(text: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "normalize",
        signature: "fn normalize(path: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "parent",
        signature: "fn parent(path: String) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "split",
        signature: "fn split(path: String) -> (String, String)",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "starts_with",
        signature: "fn starts_with(path: String, prefix: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "request_id",
        signature: "fn request_id(inner: T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "cors",
        signature: "fn cors(inner: T, config: String) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "security_headers",
        signature: "fn security_headers(inner: T, preset: String) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "etag",
        signature: "fn etag(inner: T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "rate_limit",
        signature: "fn rate_limit(inner: T, config: String) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "hsts",
        signature: "fn hsts(inner: T, config: String) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "cache_control",
        signature: "fn cache_control(inner: T, config: String) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "body_limit",
        signature: "fn body_limit(inner: T, max_bytes: i64) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "compress_gzip",
        signature: "fn compress_gzip(inner: T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "logger",
        signature: "fn logger(inner: T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "recoverer",
        signature: "fn recoverer(inner: T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "timeout",
        signature: "fn timeout(inner: T, budget_ms: i64) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "basic_auth",
        signature: "fn basic_auth(inner: T, realm: String) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "bearer_auth",
        signature: "fn bearer_auth(inner: T, realm: String) -> T",
    },
    StdFunctionSignature {
        module_path: "std::http::middleware",
        name: "safe_defaults",
        signature: "fn safe_defaults(inner: T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "string_reader",
        signature: "fn string_reader(text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "buffer_writer",
        signature: "fn buffer_writer() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "limit_reader",
        signature: "fn limit_reader(src: i64, limit: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "tee_reader",
        signature: "fn tee_reader(src: i64, sink: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "multi_reader",
        signature: "fn multi_reader(sources: Vec<i64>) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "pipe",
        signature: "fn pipe() -> (i64, i64)",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "copy_n",
        signature: "fn copy_n(dst: i64, src: i64, n: i64) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "drain",
        signature: "fn drain(src: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "contents",
        signature: "fn contents(writer: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "write",
        signature: "fn write(writer: i64, text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::io",
        name: "close_writer",
        signature: "fn close_writer(writer: i64)",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "matches",
        signature: "fn matches(pattern: String, name: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::path",
        name: "glob",
        signature: "fn glob(pattern: String) -> Result<Vec<String>, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::net::smtp",
        name: "send",
        signature: "fn send(addr: String, from: String, to: String, subject: String, body: String) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::net::smtp",
        name: "send_auth",
        signature: "fn send_auth(addr: String, from: String, to: String, subject: String, body: String, username: String, password: String) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::sort",
        name: "sort_stable",
        signature: "fn sort_stable(xs: Vec<T>) -> Vec<T>",
    },
    StdFunctionSignature {
        module_path: "std::sort",
        name: "binary_search",
        signature: "fn binary_search(xs: Vec<T>, target: T) -> Option<i64>",
    },
    StdFunctionSignature {
        module_path: "std::sort",
        name: "partition_point",
        signature: "fn partition_point(xs: Vec<T>, pivot: T) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "abort",
        signature: "fn abort() -> !",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "exit",
        signature: "fn exit(code: i64) -> !",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "id",
        signature: "fn id() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "kill",
        signature: "fn kill(pid: i64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "kill_group",
        signature: "fn kill_group(pid: i64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "pipeline_run",
        signature: "fn pipeline_run(commands: Vec<String>) -> Result<process::Output, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "run",
        signature: "fn run(program: String, args: Vec<String>) -> Result<process::Output, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "run_in",
        signature: "fn run_in(program: String, args: Vec<String>, dir: String, env: Vec<(String, String)>) -> Result<process::Output, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "signal",
        signature: "fn signal(pid: i64, signum: i64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "spawn",
        signature: "fn spawn(program: String, args: Vec<String>) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "spawn_piped",
        signature: "fn spawn_piped(program: String, args: Vec<String>) -> Result<process::Child, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::process",
        name: "wait_timeout",
        signature: "fn wait_timeout(pid: i64, ms: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::regex",
        name: "captures",
        signature: "fn captures(pattern: regex::Pattern, text: String) -> Option<Vec<Option<String>>>",
    },
    StdFunctionSignature {
        module_path: "std::regex",
        name: "captures_all",
        signature: "fn captures_all(pattern: regex::Pattern, text: String) -> Vec<Vec<Option<String>>>",
    },
    StdFunctionSignature {
        module_path: "std::regex",
        name: "compile",
        signature: "fn compile(pattern: String) -> Result<regex::Pattern, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::regex",
        name: "find",
        signature: "fn find(pattern: regex::Pattern, text: String) -> Option<(i64, i64, String)>",
    },
    StdFunctionSignature {
        module_path: "std::regex",
        name: "find_all",
        signature: "fn find_all(pattern: regex::Pattern, text: String) -> Vec<(i64, i64, String)>",
    },
    StdFunctionSignature {
        module_path: "std::regex",
        name: "count",
        signature: "fn count(pattern: regex::Pattern, text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::regex",
        name: "is_match",
        signature: "fn is_match(pattern: regex::Pattern, text: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::regex",
        name: "replace",
        signature: "fn replace(pattern: regex::Pattern, text: String, replacement: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::regex",
        name: "replace_all",
        signature: "fn replace_all(pattern: regex::Pattern, text: String, replacement: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::regex",
        name: "split",
        signature: "fn split(pattern: regex::Pattern, text: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::result",
        name: "and_then",
        signature: "fn and_then<T, E, U>(value: Result<T, E>, f: Fn(T) -> Result<U, E>) -> Result<U, E>",
    },
    StdFunctionSignature {
        module_path: "std::result",
        name: "err",
        signature: "fn err<T, E>(value: Result<T, E>) -> Option<E>",
    },
    StdFunctionSignature {
        module_path: "std::result",
        name: "is_err",
        signature: "fn is_err<T, E>(value: Result<T, E>) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::result",
        name: "is_ok",
        signature: "fn is_ok<T, E>(value: Result<T, E>) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::result",
        name: "map",
        signature: "fn map<T, E, U>(value: Result<T, E>, f: Fn(T) -> U) -> Result<U, E>",
    },
    StdFunctionSignature {
        module_path: "std::result",
        name: "map_err",
        signature: "fn map_err<T, E, F>(value: Result<T, E>, f: Fn(E) -> F) -> Result<T, F>",
    },
    StdFunctionSignature {
        module_path: "std::result",
        name: "ok",
        signature: "fn ok<T, E>(value: Result<T, E>) -> Option<T>",
    },
    StdFunctionSignature {
        module_path: "std::result",
        name: "or_else",
        signature: "fn or_else<T, E, F>(value: Result<T, E>, f: Fn(E) -> Result<T, F>) -> Result<T, F>",
    },
    StdFunctionSignature {
        module_path: "std::result",
        name: "unwrap_or",
        signature: "fn unwrap_or<T, E>(value: Result<T, E>, fallback: T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::result",
        name: "unwrap_or_else",
        signature: "fn unwrap_or_else<T, E>(value: Result<T, E>, f: Fn(E) -> T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::lifecycle",
        name: "ready",
        signature: "fn ready() -> ()",
    },
    StdFunctionSignature {
        module_path: "std::lifecycle",
        name: "set_ready",
        signature: "fn set_ready(ready: bool) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::lifecycle",
        name: "is_ready",
        signature: "fn is_ready() -> bool",
    },
    StdFunctionSignature {
        module_path: "std::lifecycle",
        name: "shutdown",
        signature: "fn shutdown() -> ()",
    },
    StdFunctionSignature {
        module_path: "std::lifecycle",
        name: "is_shutting_down",
        signature: "fn is_shutting_down() -> bool",
    },
    StdFunctionSignature {
        module_path: "std::lifecycle",
        name: "await_shutdown",
        signature: "fn await_shutdown() -> ()",
    },
    StdFunctionSignature {
        module_path: "std::lifecycle",
        name: "notify_status",
        signature: "fn notify_status(message: String) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "cohort_cancel",
        signature: "fn cohort_cancel() -> ()",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "cohorts",
        signature: "fn cohorts() -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "root",
        signature: "fn root() -> String",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "cohort_cancelled",
        signature: "fn cohort_cancelled() -> bool",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "cohort_join",
        signature: "fn cohort_join() -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "cohort_pop",
        signature: "fn cohort_pop() -> ()",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "cohort_push",
        signature: "fn cohort_push(policy: i64, timeout_ms: i64, isolation: i64, on_error: i64, uncancellable: i64, drain_ms: i64) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "arena_pop",
        signature: "fn arena_pop() -> ()",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "arena_push",
        signature: "fn arena_push() -> ()",
    },
    StdFunctionSignature {
        module_path: "std::pprof",
        name: "cpu_profile",
        signature: "fn cpu_profile(millis: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::pprof",
        name: "heap_profile",
        signature: "fn heap_profile(millis: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::pprof",
        name: "goroutine_profile",
        signature: "fn goroutine_profile() -> String",
    },
    StdFunctionSignature {
        module_path: "std::pprof",
        name: "mutex_profile",
        signature: "fn mutex_profile() -> String",
    },
    StdFunctionSignature {
        module_path: "std::pprof",
        name: "block_profile",
        signature: "fn block_profile() -> String",
    },
    StdFunctionSignature {
        module_path: "std::pprof",
        name: "execution_trace",
        signature: "fn execution_trace(millis: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::pprof",
        name: "route",
        signature: "fn route(path: String, query: String) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "collect_cycles",
        signature: "fn collect_cycles() -> ()",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "cycle_collection_supported",
        signature: "fn cycle_collection_supported() -> bool",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "scheduler_stats_json",
        signature: "fn scheduler_stats_json() -> String",
    },
    StdFunctionSignature {
        module_path: "std::runtime",
        name: "set_panic_hook",
        signature: "fn set_panic_hook(hook: Fn(String) -> ()) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::slog",
        name: "debug",
        signature: "fn debug(message: String) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::slog",
        name: "error",
        signature: "fn error(message: String) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::slog",
        name: "info",
        signature: "fn info(message: String) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::slog",
        name: "warn",
        signature: "fn warn(message: String) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::strconv",
        name: "format_f64",
        signature: "fn format_f64(value: f64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strconv",
        name: "format_i64",
        signature: "fn format_i64(value: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strconv",
        name: "format_i64_radix",
        signature: "fn format_i64_radix(value: i64, base: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strconv",
        name: "parse_bool",
        signature: "fn parse_bool(text: String) -> Result<bool, strconv::ParseError>",
    },
    StdFunctionSignature {
        module_path: "std::strconv",
        name: "parse_f64",
        signature: "fn parse_f64(text: String) -> Result<f64, strconv::ParseError>",
    },
    StdFunctionSignature {
        module_path: "std::strconv",
        name: "parse_i64",
        signature: "fn parse_i64(text: String) -> Result<i64, strconv::ParseError>",
    },
    StdFunctionSignature {
        module_path: "std::strconv",
        name: "parse_i64_radix",
        signature: "fn parse_i64_radix(text: String, base: i64) -> Result<i64, strconv::ParseError>",
    },
    StdFunctionSignature {
        module_path: "std::strconv",
        name: "parse_u64",
        signature: "fn parse_u64(text: String) -> Result<u64, strconv::ParseError>",
    },
    StdFunctionSignature {
        module_path: "std::strconv",
        name: "quote",
        signature: "fn quote(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strconv",
        name: "unquote",
        signature: "fn unquote(text: String) -> Result<String, strconv::ParseError>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "bytes",
        signature: "fn bytes(text: String) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "center",
        signature: "fn center(text: String, width: i64, fill: char) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "chars",
        signature: "fn chars(text: String) -> Iterator<char>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "contains",
        signature: "fn contains(text: String, needle: String | char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "contains_any",
        signature: "fn contains_any(text: String, needle: String | char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "count",
        signature: "fn count(text: String, needle: String | char) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "ends_with",
        signature: "fn ends_with(text: String, needle: String | char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "equal_fold",
        signature: "fn equal_fold(text: String, needle: String | char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "find",
        signature: "fn find(text: String, needle: String | char) -> Option<i64>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "find_any",
        signature: "fn find_any(text: String, needle: String | char) -> Option<i64>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "join",
        signature: "fn join(parts: Vec<String>, sep: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "lines",
        signature: "fn lines(text: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "pad_left",
        signature: "fn pad_left(text: String, width: i64, fill: char) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "pad_right",
        signature: "fn pad_right(text: String, width: i64, fill: char) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "repeat",
        signature: "fn repeat(text: String, count: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "replace",
        signature: "fn replace(text: String, from: String | char, to: String | char) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "replacen",
        signature: "fn replacen(text: String, from: String | char, to: String | char, n: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "rfind",
        signature: "fn rfind(text: String, needle: String | char) -> Option<i64>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "rfind_any",
        signature: "fn rfind_any(text: String, needle: String | char) -> Option<i64>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "rsplit_once",
        signature: "fn rsplit_once(text: String, sep: String | char) -> Option<(String, String)>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "byte_len",
        signature: "fn byte_len(text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "byte_at",
        signature: "fn byte_at(text: String, index: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "slice",
        signature: "fn slice(text: String, start: i64, end: i64) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "substring",
        signature: "fn substring(text: String, start: i64, end: i64) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "split",
        signature: "fn split(text: String, sep: String | char) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "split_once",
        signature: "fn split_once(text: String, sep: String | char) -> Option<(String, String)>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "split_whitespace",
        signature: "fn split_whitespace(text: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "splitn",
        signature: "fn splitn(text: String, n: i64, sep: String | char) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "starts_with",
        signature: "fn starts_with(text: String, needle: String | char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "strip_prefix",
        signature: "fn strip_prefix(text: String, prefix: String | char) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "strip_suffix",
        signature: "fn strip_suffix(text: String, suffix: String | char) -> Option<String>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "to_bool",
        signature: "fn to_bool(text: String) -> Option<bool>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "to_f64",
        signature: "fn to_f64(text: String) -> Option<f64>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "to_i64",
        signature: "fn to_i64(text: String) -> Option<i64>",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "to_lowercase",
        signature: "fn to_lowercase(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "to_title",
        signature: "fn to_title(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "to_uppercase",
        signature: "fn to_uppercase(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "trim",
        signature: "fn trim(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "trim_end",
        signature: "fn trim_end(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "trim_end_matches",
        signature: "fn trim_end_matches(text: String, cutset: String | char) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "trim_matches",
        signature: "fn trim_matches(text: String, cutset: String | char) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "trim_start",
        signature: "fn trim_start(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::strings",
        name: "trim_start_matches",
        signature: "fn trim_start_matches(text: String, cutset: String | char) -> String",
    },
    StdFunctionSignature {
        module_path: "std::sync",
        name: "channel",
        signature: "fn channel<T>(capacity: i64) -> sync::Channel<T>",
    },
    StdFunctionSignature {
        module_path: "std::sync",
        name: "channel_unbounded",
        signature: "fn channel_unbounded<T>() -> sync::Channel<T>",
    },
    StdFunctionSignature {
        module_path: "std::sync",
        name: "shield",
        signature: "fn shield<T>(f: Fn() -> T) -> T",
    },
    StdFunctionSignature {
        module_path: "std::sync",
        name: "with_timeout",
        signature: "fn with_timeout<T>(f: Fn() -> T, ms: i64) -> Result<T, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::testing",
        name: "check",
        signature: "fn check(cond: bool, message: String) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::testing",
        name: "check_eq",
        signature: "fn check_eq<T: Debug + Eq>(left: T, right: T, message: String) -> Result<(), errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::testing",
        name: "check_ok",
        signature: "fn check_ok<T, E: Debug>(result: Result<T, E>, message: String) -> Result<T, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::testing",
        name: "wait_for_scheduler_idle",
        signature: "fn wait_for_scheduler_idle(timeout_ms: i64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::httptest",
        name: "record",
        signature: "fn record(handler: http::Handler, method: String, path: String, body: String) -> Result<http::Response, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::httptest",
        name: "server",
        signature: "fn server(status: i64, body: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::thread",
        name: "num_cpus",
        signature: "fn num_cpus() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::thread",
        name: "yield_now",
        signature: "fn yield_now() -> ()",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "add_date",
        signature: "fn add_date(unix_ms: i64, location: time::Location, years: i64, months: i64, days: i64) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "format_in",
        signature: "fn format_in(layout: String, unix_ms: i64, location: time::Location) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "format_rfc3339",
        signature: "fn format_rfc3339(ms: i64) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "monotonic_ms",
        signature: "fn monotonic_ms() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "monotonic_nanos",
        signature: "fn monotonic_nanos() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "now",
        signature: "fn now() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "advance",
        signature: "fn advance(ms: i64) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "freeze",
        signature: "fn freeze(ms: i64) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "is_frozen",
        signature: "fn is_frozen() -> bool",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "unfreeze",
        signature: "fn unfreeze() -> ()",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "now_ms",
        signature: "fn now_ms() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "now_nanos",
        signature: "fn now_nanos() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "parse_rfc3339",
        signature: "fn parse_rfc3339(text: String) -> Result<i64, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "since_ms",
        signature: "fn since_ms(instant: time::Instant) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "sleep",
        signature: "fn sleep(ms: i64) -> ()",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "after",
        signature: "fn after(d: time::Duration) -> Receiver<i64>",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "sleep_ctx",
        signature: "fn sleep_ctx(ctx: context::Context, ms: i64) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::time",
        name: "unix_ms",
        signature: "fn unix_ms() -> i64",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "combining_class",
        signature: "fn combining_class(rune: char) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "fold_case",
        signature: "fn fold_case(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "grapheme_count",
        signature: "fn grapheme_count(text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "graphemes",
        signature: "fn graphemes(text: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_assigned",
        signature: "fn is_assigned(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_control",
        signature: "fn is_control(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_digit",
        signature: "fn is_digit(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_graphic",
        signature: "fn is_graphic(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_letter",
        signature: "fn is_letter(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_lower",
        signature: "fn is_lower(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_mark",
        signature: "fn is_mark(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_nfc",
        signature: "fn is_nfc(text: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_nfd",
        signature: "fn is_nfd(text: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_nfkc",
        signature: "fn is_nfkc(text: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_nfkd",
        signature: "fn is_nfkd(text: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_number",
        signature: "fn is_number(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_print",
        signature: "fn is_print(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_punct",
        signature: "fn is_punct(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_space",
        signature: "fn is_space(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_symbol",
        signature: "fn is_symbol(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_title",
        signature: "fn is_title(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "is_upper",
        signature: "fn is_upper(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "nfc",
        signature: "fn nfc(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "nfd",
        signature: "fn nfd(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "nfkc",
        signature: "fn nfkc(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "nfkd",
        signature: "fn nfkd(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "sentence_count",
        signature: "fn sentence_count(text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "sentences",
        signature: "fn sentences(text: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "simple_fold",
        signature: "fn simple_fold(rune: char) -> char",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "to_lower",
        signature: "fn to_lower(rune: char) -> char",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "to_lower_str",
        signature: "fn to_lower_str(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "to_title",
        signature: "fn to_title(rune: char) -> char",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "to_upper",
        signature: "fn to_upper(rune: char) -> char",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "to_upper_str",
        signature: "fn to_upper_str(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "word_bounds",
        signature: "fn word_bounds(text: String) -> Vec<(i64, i64)>",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "word_count",
        signature: "fn word_count(text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::unicode",
        name: "words",
        signature: "fn words(text: String) -> Vec<String>",
    },
    StdFunctionSignature {
        module_path: "std::utf16",
        name: "decode_surrogate_pair",
        signature: "fn decode_surrogate_pair(high: char, low: char) -> Result<char, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::utf16",
        name: "decode_to_string",
        signature: "fn decode_to_string(units: Vec<u16>) -> Result<String, errors::Error>",
    },
    StdFunctionSignature {
        module_path: "std::utf16",
        name: "encode_string",
        signature: "fn encode_string(text: String) -> Vec<u16>",
    },
    StdFunctionSignature {
        module_path: "std::utf16",
        name: "is_surrogate",
        signature: "fn is_surrogate(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::utf16",
        name: "rune_len",
        signature: "fn rune_len(rune: char) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "append_rune",
        signature: "fn append_rune(bytes: Vec<u8>, rune: char) -> Vec<u8>",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "decode_last_rune",
        signature: "fn decode_last_rune(bytes: Vec<u8>) -> (char, i64)",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "decode_last_rune_in_string",
        signature: "fn decode_last_rune_in_string(text: String) -> (char, i64)",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "decode_rune",
        signature: "fn decode_rune(bytes: Vec<u8>) -> (char, i64)",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "decode_rune_in_string",
        signature: "fn decode_rune_in_string(text: String) -> (char, i64)",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "full_rune",
        signature: "fn full_rune(bytes: Vec<u8>) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "full_rune_in_string",
        signature: "fn full_rune_in_string(text: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "is_valid",
        signature: "fn is_valid(bytes: Vec<u8>) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "rune_count",
        signature: "fn rune_count(bytes: Vec<u8>) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "rune_count_in_string",
        signature: "fn rune_count_in_string(text: String) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "rune_len",
        signature: "fn rune_len(rune: char) -> i64",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "rune_start",
        signature: "fn rune_start(byte: u8) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "valid_rune",
        signature: "fn valid_rune(rune: char) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::utf8",
        name: "valid_string",
        signature: "fn valid_string(text: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::uuid",
        name: "is_valid",
        signature: "fn is_valid(text: String) -> bool",
    },
    StdFunctionSignature {
        module_path: "std::uuid",
        name: "normalize",
        signature: "fn normalize(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::uuid",
        name: "simple",
        signature: "fn simple(text: String) -> String",
    },
    StdFunctionSignature {
        module_path: "std::uuid",
        name: "v4",
        signature: "fn v4() -> String",
    },
    StdFunctionSignature {
        module_path: "std::uuid",
        name: "v7",
        signature: "fn v7() -> String",
    },
];

/// Looks up the source-facing signature for one stdlib function.
#[must_use]
pub fn function_signature(module_path: &str, name: &str) -> Option<&'static str> {
    function(module_path, name).map(|sig| sig.signature)
}

/// Looks up a stdlib function row by canonical module path and name.
#[must_use]
pub fn function(module_path: &str, name: &str) -> Option<&'static StdFunctionSignature> {
    STD_FUNCTION_SIGNATURES
        .iter()
        .find(|sig| sig.module_path == module_path && sig.name == name)
}

/// Looks up a stdlib function row from source path segments.
///
/// The checker sees both canonical paths (`std::encoding::json::parse`) and
/// imported aliases (`json::parse`, `base64::encode`). Exact canonical lookup
/// wins; alias fallback only succeeds when it maps to a single stdlib row.
#[must_use]
pub fn function_for_path(
    module_segments: &[&str],
    name: &str,
) -> Option<&'static StdFunctionSignature> {
    if module_segments.is_empty() {
        return None;
    }
    let module = module_segments.join("::");
    let exact = if module_segments.first().copied() == Some("std") {
        module.clone()
    } else {
        format!("std::{module}")
    };
    if let Some(sig) = function(&exact, name) {
        return Some(sig);
    }
    if module_segments.first().copied() == Some("std") {
        return None;
    }
    let mut matches = STD_FUNCTION_SIGNATURES.iter().filter(|sig| {
        sig.name == name
            && sig
                .module_path
                .strip_prefix("std::")
                .is_some_and(|tail| tail == module || tail.ends_with(&format!("::{module}")))
    });
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some(first)
}

/// Parses the stored source-facing signature into arity, parameter type text,
/// and return type text. Returns `None` for malformed catalogue rows.
#[must_use]
pub fn parse_signature(signature: &'static str) -> Option<StdSignatureShape> {
    let rest = signature.strip_prefix("fn ")?;
    let open = rest.find('(')?;
    let close = matching_delimiter(rest, open, '(', ')')?;
    let params_src = rest.get(open + 1..close)?.trim();
    let after = rest.get(close + 1..)?.trim();
    let return_ty = after.strip_prefix("->")?.trim();
    let params = if params_src.is_empty() {
        Vec::new()
    } else {
        split_top_level(params_src, ',')
            .into_iter()
            .map(|param| {
                let colon = find_top_level(param, ':')?;
                Some(StdSignatureParam {
                    name: param.get(..colon)?.trim(),
                    ty: param.get(colon + 1..)?.trim(),
                })
            })
            .collect::<Option<Vec<_>>>()?
    };
    Some(StdSignatureShape { params, return_ty })
}

/// Returns the parsed signature shape for a canonical stdlib function,
/// in the order a caller writes it.
#[must_use]
pub fn function_shape(module_path: &str, name: &str) -> Option<StdSignatureShape> {
    parse_signature(function_signature(module_path, name)?)
}

/// Returns the parsed signature shape for a source-path stdlib function,
/// in the order a caller writes it.
#[must_use]
pub fn function_shape_for_path(module_segments: &[&str], name: &str) -> Option<StdSignatureShape> {
    parse_signature(function_for_path(module_segments, name)?.signature)
}

/// Whether `name` in `module_segments` is one of the free functions whose
/// data parameter moved to the front for 0.56.0.
///
/// Every std free function now takes its data first, which is what makes
/// `iter::map(xs, f)` read like `xs.map(f)`. Method lowering and both
/// tiers' free-call lowering still pass the data LAST, so the front end
/// rotates a written call into that order once
/// ([`crate::normalize_caller_side_spellings`]) and everything after it
/// sees one shape.
#[must_use]
pub fn takes_data_first(module_segments: &[&str], name: &str) -> bool {
    let module = match module_segments {
        ["std", rest @ ..] => rest,
        rest => rest,
    };
    let names: &[&str] = match module {
        ["iter"] => &DATA_FIRST_ITER,
        ["option"] => &DATA_FIRST_OPTION,
        ["result"] => &DATA_FIRST_RESULT,
        _ => return false,
    };
    names.contains(&name)
}

/// The parameter order every pass after the front end sees: the data
/// parameter last, which is the slot method lowering and the runtime
/// helpers already use.
#[must_use]
pub fn internal_shape_for_path(module_segments: &[&str], name: &str) -> Option<StdSignatureShape> {
    let mut shape = function_shape_for_path(module_segments, name)?;
    if takes_data_first(module_segments, name) && shape.params.len() >= 2 {
        let data = shape.params.remove(0);
        shape.params.push(data);
    }
    Some(shape)
}

const DATA_FIRST_ITER: [&str; 31] = [
    "all",
    "any",
    "chunk_by",
    "chunks",
    "count_by",
    "filter",
    "filter_map",
    "find",
    "find_map",
    "flat_map",
    "fold",
    "for_each",
    "map",
    "max_by",
    "max_by_key",
    "min_by",
    "min_by_key",
    "partition",
    "position",
    "product_by",
    "reduce",
    "scan",
    "skip",
    "skip_while",
    "sort_by",
    "sort_by_key",
    "step_by",
    "sum_by",
    "take",
    "take_while",
    "windows",
];

const DATA_FIRST_OPTION: [&str; 11] = [
    "and_then",
    "expect",
    "filter",
    "map",
    "ok_or",
    "ok_or_else",
    "or",
    "or_else",
    "unwrap_or",
    "unwrap_or_else",
    "zip",
];

const DATA_FIRST_RESULT: [&str; 6] = [
    "and_then",
    "map",
    "map_err",
    "or_else",
    "unwrap_or",
    "unwrap_or_else",
];

fn matching_delimiter(s: &str, open: usize, left: char, right: char) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in s.char_indices().skip_while(|(idx, _)| *idx < open) {
        if ch == left {
            depth = depth.saturating_add(1);
        } else if ch == right {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(idx);
            }
        }
    }
    None
}

fn find_top_level(s: &str, needle: char) -> Option<usize> {
    let mut angle = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    for (idx, ch) in s.char_indices() {
        match ch {
            '<' => angle = angle.saturating_add(1),
            '>' => angle = angle.saturating_sub(1),
            '(' => paren = paren.saturating_add(1),
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket = bracket.saturating_add(1),
            ']' => bracket = bracket.saturating_sub(1),
            _ if ch == needle && angle == 0 && paren == 0 && bracket == 0 => return Some(idx),
            _ => {}
        }
    }
    None
}

/// Splits a comma-like list while preserving nested generic, tuple, and
/// callable-signature groups.
#[must_use]
pub fn split_top_level(s: &str, needle: char) -> Vec<&str> {
    let mut angle = 0usize;
    let mut paren = 0usize;
    let mut bracket = 0usize;
    let mut start = 0usize;
    let mut out = Vec::new();
    for (idx, ch) in s.char_indices() {
        match ch {
            '<' => angle = angle.saturating_add(1),
            '>' => angle = angle.saturating_sub(1),
            '(' => paren = paren.saturating_add(1),
            ')' => paren = paren.saturating_sub(1),
            '[' => bracket = bracket.saturating_add(1),
            ']' => bracket = bracket.saturating_sub(1),
            _ if ch == needle && angle == 0 && paren == 0 && bracket == 0 => {
                if let Some(part) = s.get(start..idx) {
                    out.push(part.trim());
                }
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    if let Some(part) = s.get(start..) {
        out.push(part.trim());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{function_shape_for_path, parse_signature};

    #[test]
    fn parses_nested_catalog_signature_shape() {
        let shape = parse_signature(
            "fn write(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<u8>, errors::Error>",
        )
        .expect("signature parses");
        assert_eq!(shape.params.len(), 1);
        assert_eq!(shape.params[0].ty, "Vec<(String, Vec<u8>)>");
        assert_eq!(shape.return_ty, "Result<Vec<u8>, errors::Error>");
    }

    #[test]
    fn parses_callable_and_generic_signature_shape() {
        let shape = parse_signature("fn map<T, U>(f: Fn(T) -> U, items: Vec<T>) -> Vec<U>")
            .expect("signature parses");
        assert_eq!(shape.params.len(), 2);
        assert_eq!(shape.params[0].ty, "Fn(T) -> U");
        assert_eq!(shape.params[1].ty, "Vec<T>");
    }

    #[test]
    fn alias_path_lookup_is_unambiguous() {
        let shape = function_shape_for_path(&["json"], "parse").expect("json alias resolves");
        assert_eq!(shape.params[0].ty, "String");
        assert_eq!(shape.return_ty, "Result<json::Value, errors::Error>");
    }
}
