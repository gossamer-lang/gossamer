// Integration tests for the new stdlib modules added in the P0 gap-fill:
// std::math, std::math::bits, std::unicode, std::utf8 (expanded),
// std::utf16, std::iter, std::encoding::csv, std::encoding::pem,
// std::encoding::binary (full), std::encoding::yaml, std::crypto.

#![allow(missing_docs)]

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);

fn gos_bin() -> PathBuf {
    PathBuf::from(env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"))
}

fn scratch(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "gos-stdlib-{pid}-{n}-{tag}",
        pid = std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Drives a spawned child to completion under [`TIMEOUT`], returning
/// `(stdout, stderr, exit_code)` with CRLF normalised.
fn drive(mut child: std::process::Child) -> (String, String, Option<i32>) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n"),
        out.status.code(),
    )
}

fn run_gos(src: &Path) -> (String, String, Option<i32>) {
    let child = Command::new(gos_bin())
        .arg("run")
        .arg(src)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gos");
    drive(child)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

/// Compiles `main.gos` (already written under `dir`) to a native
/// binary via `gos build` and runs it, returning
/// `(stdout, stderr, exit_code)` or a build/link error message.
fn build_and_run_native(dir: &Path) -> Result<(String, String, Option<i32>), String> {
    let path = dir.join("main.gos");
    let out_dir = dir.join("out");
    fs::create_dir_all(&out_dir).map_err(|e| format!("mkdir out: {e}"))?;
    let build = Command::new(gos_bin())
        .arg("build")
        .arg("--out-dir")
        .arg(&out_dir)
        .arg(&path)
        .output()
        .map_err(|e| format!("spawn gos build: {e}"))?;
    if !build.status.success() {
        return Err(format!(
            "gos build failed:\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        ));
    }
    let bin = fs::read_dir(&out_dir)
        .map_err(|e| format!("read_dir out: {e}"))?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file() && is_executable(p))
        .ok_or_else(|| format!("no executable produced in {}", out_dir.display()))?;
    let child = Command::new(&bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn native binary: {e}"))?;
    Ok(drive(child))
}

/// Runs `src` under the bytecode VM **and** the native LLVM AOT tier,
/// asserting both produce `expected` on stdout (so the compiled tiers
/// can never silently drift from the VM). A `gos build` failure or a
/// compiled-output mismatch fails the test - VM-only gaps surface here
/// rather than going uncaught.
fn assert_vm_output(tag: &str, src: &str, expected: &str) {
    let dir = scratch(tag);
    let path = dir.join("main.gos");
    fs::File::create(&path)
        .unwrap()
        .write_all(src.as_bytes())
        .unwrap();
    let (vm_stdout, vm_stderr, vm_code) = run_gos(&path);
    assert_eq!(
        vm_stdout.trim_end(),
        expected,
        "[{tag}/vm] stdout mismatch\nstderr: {vm_stderr}\ncode: {vm_code:?}"
    );
    match build_and_run_native(&dir) {
        Ok((nat_stdout, nat_stderr, nat_code)) => {
            assert_eq!(
                nat_stdout.trim_end(),
                expected,
                "[{tag}/compiled] stdout mismatch (VM was correct)\n\
                 stderr: {nat_stderr}\ncode: {nat_code:?}"
            );
        }
        Err(e) => panic!("[{tag}/compiled] {e}"),
    }
}

// -----------------------------------------------------------------------
// std::slog - structured fields must survive the FFI on the compiled
// tier. slog writes JSON-line records to stderr (not stdout), so this
// compares stderr across the VM and the native build rather than going
// through `assert_vm_output`.

#[test]
fn image_png_jpeg_round_trip_and_invalid_input_match_native() {
    let src = r#"
use std::image
fn main() {
    let source = image::filled(2, 1, 0xff0000ff)
    println(image::set_pixel(source, 1, 0, 0x00ff00ff))
    let png = image::encode_png_base64(source)
    let decoded = image::decode_base64(png)
    println(image::width(decoded))
    println(image::height(decoded))
    println(image::pixel(decoded, 1, 0))
    println(image::decode_base64("not base64") == 0)
    let jpeg = image::decode_base64(image::encode_jpeg_base64(source, 90))
    println(image::width(jpeg))
    println(image::height(jpeg))
}
"#;
    assert_vm_output("image_png_jpeg", src, "true\n2\n1\n16711935\ntrue\n2\n1");
}

#[test]
fn fs_temp_resources_create_unique_paths_and_reject_unsafe_prefixes_across_tiers() {
    let src = r#"
use std::fs
fn main() {
    match fs::temp_dir("gossamer-fs-tier") {
        Ok(path) => {
            println(fs::is_dir(path))
            let _ = fs::remove_dir_all(path)
        },
        Err(_) => println(false),
    }
    match fs::temp_file("gossamer-fs-tier") {
        Ok((file, path)) => {
            println(fs::is_file(path))
            file.close()
            let _ = fs::remove_file(path)
        },
        Err(_) => println(false),
    }
    match fs::temp_dir("../unsafe") {
        Ok(_) => println(false),
        Err(_) => println(true),
    }
}
"#;
    assert_vm_output("fs_temp_resources", src, "true\ntrue\ntrue");
}

#[test]
fn slog_carries_fields_across_tiers() {
    let src = r#"
use std::slog
fn main() {
    slog::info("served", "status", 200, "path", "/")
    slog::warn("slow", "ms", 1500, "ok", false)
    slog::error("failed", "code", "E42")
    slog::debug("measure", "ratio", 1.5)
}
"#;
    let expected = "{\"level\":\"INFO\",\"msg\":\"served\",\"status\":\"200\",\"path\":\"/\"}\n\
{\"level\":\"WARN\",\"msg\":\"slow\",\"ms\":\"1500\",\"ok\":\"false\"}\n\
{\"level\":\"ERROR\",\"msg\":\"failed\",\"code\":\"E42\"}\n\
{\"level\":\"DEBUG\",\"msg\":\"measure\",\"ratio\":\"1.5\"}";
    let dir = scratch("slog_fields");
    let path = dir.join("main.gos");
    fs::File::create(&path)
        .unwrap()
        .write_all(src.as_bytes())
        .unwrap();
    let (_vm_stdout, vm_stderr, vm_code) = run_gos(&path);
    assert_eq!(
        vm_stderr.trim_end(),
        expected,
        "[slog/vm] stderr mismatch (code {vm_code:?})"
    );
    match build_and_run_native(&dir) {
        Ok((_nat_stdout, nat_stderr, nat_code)) => {
            assert_eq!(
                nat_stderr.trim_end(),
                expected,
                "[slog/compiled] stderr mismatch (VM was correct); code {nat_code:?}"
            );
        }
        Err(e) => panic!("[slog/compiled] {e}"),
    }
}

// -----------------------------------------------------------------------
// std::math

#[test]
fn math_abs_and_sqrt() {
    assert_vm_output(
        "math_abs_sqrt",
        r#"
use std::math
fn main() {
    println("{}", math::abs(-3.0))
    println("{}", math::sqrt(9.0))
}
"#,
        "3\n3",
    );
}

#[test]
fn math_trig_floor_ceil() {
    assert_vm_output(
        "math_trig_floor",
        r#"
use std::math
fn main() {
    let s = math::sin(0.0)
    let c = math::cos(0.0)
    let f = math::floor(2.7)
    let cl = math::ceil(2.1)
    println("{} {} {} {}", s, c, f, cl)
}
"#,
        "0 1 2 3",
    );
}

#[test]
fn math_min_max() {
    assert_vm_output(
        "math_min_max",
        r#"
use std::math
fn main() {
    println("{}", math::min(3.0, 5.0))
    println("{}", math::max(3.0, 5.0))
}
"#,
        "3\n5",
    );
}

// -----------------------------------------------------------------------
// std::math::bits

#[test]
fn math_bits_count_ones_and_len() {
    assert_vm_output(
        "math_bits",
        r#"
use std::math
fn main() {
    println("{}", math::bits::count_ones(255))
    println("{}", math::bits::len(8))
    println("{}", math::bits::leading_zeros(1))
}
"#,
        "8\n4\n63",
    );
}

// -----------------------------------------------------------------------
// std::unicode

#[test]
fn unicode_predicates() {
    assert_vm_output(
        "unicode_predicates",
        r#"
use std::unicode
fn main() {
    println("{}", unicode::is_letter('a'))
    println("{}", unicode::is_digit('5'))
    println("{}", unicode::is_space(' '))
    println("{}", unicode::is_upper('A'))
    println("{}", unicode::is_lower('z'))
    println("{}", unicode::to_upper('a'))
    println("{}", unicode::to_lower('A'))
}
"#,
        "true\ntrue\ntrue\ntrue\ntrue\nA\na",
    );
}

// -----------------------------------------------------------------------
// std::utf8

#[test]
fn utf8_is_valid_and_rune_count() {
    assert_vm_output(
        "utf8_basics",
        r#"
use std::utf8
fn main() {
    println("{}", utf8::valid_string("hello"))
    println("{}", utf8::rune_count_in_string("café"))
    println("{}", utf8::rune_len('€'))
}
"#,
        "true\n4\n3",
    );
}

// -----------------------------------------------------------------------
// std::strings (Unicode-sensitive additions)

#[test]
fn strings_contains_and_split_whitespace() {
    assert_vm_output(
        "strings_unicode",
        r#"
use std::strings
fn main() {
    println("{}", strings::contains("café", "é"))
    let fs = strings::split_whitespace("  hello   world  ")
    println("{}", fs.len())
}
"#,
        "true\n2",
    );
}

#[test]
fn strings_equal_fold() {
    assert_vm_output(
        "strings_equal_fold",
        r#"
use std::strings
fn main() {
    println("{}", strings::equal_fold("Hello", "hello"))
    println("{}", strings::equal_fold("Go", "Python"))
}
"#,
        "true\nfalse",
    );
}

// -----------------------------------------------------------------------
// std::iter

#[test]
fn iter_take_skip_chain() {
    assert_vm_output(
        "iter_basics",
        r#"
use std::iter
fn main() {
    let xs = [1, 2, 3, 4, 5]
    let first = iter::take(xs, 3)
    let rest = iter::skip(xs, 3)
    let merged = iter::chain(first, rest)
    println("{}", merged.len())
}
"#,
        "5",
    );
}

#[test]
fn iter_map_filter_fold() {
    assert_vm_output(
        "iter_closures",
        r#"
use std::iter
fn main() {
    let xs = [1, 2, 3, 4, 5]
    let doubled = iter::map(xs, |x: i64| x * 2)
    let evens = iter::filter(doubled, |x: i64| x % 4 == 0)
    let total = iter::fold(evens, 0, |acc: i64, x: i64| acc + x)
    println("{}", total)
}
"#,
        "12",
    );
}

#[test]
fn iter_any_all() {
    assert_vm_output(
        "iter_any_all",
        r#"
use std::iter
fn main() {
    let xs = [2, 4, 6, 8]
    println("{}", iter::all(xs, |x: i64| x % 2 == 0))
    println("{}", iter::any(xs, |x: i64| x > 5))
}
"#,
        "true\ntrue",
    );
}

#[test]
fn iter_enumerate_zip() {
    assert_vm_output(
        "iter_enum_zip",
        r#"
use std::iter
fn main() {
    let pairs = iter::zip([1, 2, 3], [4, 5, 6])
    println("{}", pairs.len())
    let indexed = iter::enumerate([10, 20, 30])
    println("{}", indexed.count())
}
"#,
        "3\n3",
    );
}

#[test]
fn iter_flatten() {
    assert_vm_output(
        "iter_flatten",
        r#"
use std::iter
fn main() {
    let xs = iter::flatten(Vec::from([
        Vec::from([1, 2]),
        Vec::from([3]),
        Vec::from([4, 5]),
    ]))
    println("{}", xs.len())
}
"#,
        "5",
    );
}

#[test]
fn iter_count_sum_reversed() {
    assert_vm_output(
        "iter_count_sum_rev",
        r#"
use std::iter
fn main() {
    let xs = [10, 20, 30]
    println("{}", iter::count(xs))
    println("{}", iter::sum(xs))
    let r = iter::rev(xs)
    println("{}", r[0])
}
"#,
        "3\n60\n30",
    );
}

// -----------------------------------------------------------------------
// std::encoding::csv

#[test]
fn encoding_csv_read_write() {
    assert_vm_output(
        "csv_roundtrip",
        r#"
use std::encoding
fn main() {
    let rows = encoding::csv::read("a,b,c\n1,2,3\n").unwrap_or(Vec::from([Vec::from([])]))
    println("{}", rows.len())
    println("{}", rows[1][2])
}
"#,
        "2\n3",
    );
}

// -----------------------------------------------------------------------
// std::encoding::binary

#[test]
fn encoding_binary_u64_roundtrip() {
    assert_vm_output(
        "binary_u64",
        r#"
use std::encoding
fn main() {
    let n = 72623859790382856
    let buf = encoding::binary::put_u64_be(Vec::from([0; 8]), n)
    match encoding::binary::get_u64_be(buf) {
        Ok(back) => println("{}", back == n),
        Err(e) => println("err: {}", e),
    }
}
"#,
        "true",
    );
}

// -----------------------------------------------------------------------
// std::encoding::yaml

#[test]
fn encoding_yaml_parse() {
    assert_vm_output(
        "yaml_parse",
        r#"
use std::encoding
fn main() {
    let result = encoding::yaml::parse("hello: world\ncount: 42")
    match result {
        Ok(v) => println("ok"),
        Err(e) => println("err: {}", e),
    }
}
"#,
        "ok",
    );
}

// -----------------------------------------------------------------------
// std::crypto

#[test]
fn crypto_sha256_hex() {
    assert_vm_output(
        "crypto_sha256",
        r#"
use std::crypto
fn main() {
    let h = crypto::sha256::hex("hello".as_bytes())
    println("{}", h.len())
}
"#,
        "64",
    );
}

#[test]
fn crypto_rand_bytes() {
    assert_vm_output(
        "crypto_rand",
        r#"
use std::crypto
fn main() {
    match crypto::rand::bytes(16) {
        Ok(b) => println("{}", b.len()),
        Err(e) => println("err: {}", e),
    }
}
"#,
        "16",
    );
}

// -----------------------------------------------------------------------
// std::compress

#[test]
fn compress_gzip_roundtrip() {
    assert_vm_output(
        "compress_gzip",
        r#"
use std::compress
fn main() {
    let data: Vec<u8> = Vec::from([104, 101, 108, 108, 111])
    match compress::gzip::encode(data, 6) {
        Ok(enc) => {
            match compress::gzip::decode(enc) {
                Ok(dec) => println("{}", dec.len()),
                Err(e) => println("decode err: {}", e),
            }
        }
        Err(e) => println("encode err: {}", e),
    }
}
"#,
        "5",
    );
}

#[test]
fn compress_flate_roundtrip() {
    assert_vm_output(
        "compress_flate",
        r#"
use std::compress
fn main() {
    let data: Vec<u8> = Vec::from([104, 101, 108, 108, 111])
    match compress::flate::compress(data, 6) {
        Ok(enc) => {
            match compress::flate::decompress(enc) {
                Ok(dec) => println("{}", dec.len()),
                Err(e) => println("decompress err: {}", e),
            }
        }
        Err(e) => println("compress err: {}", e),
    }
}
"#,
        "5",
    );
}

#[test]
fn compress_zlib_roundtrip() {
    assert_vm_output(
        "compress_zlib",
        r#"
use std::compress
fn main() {
    let data: Vec<u8> = Vec::from([104, 101, 108, 108, 111])
    match compress::zlib::compress(data, 6) {
        Ok(enc) => {
            match compress::zlib::decompress(enc) {
                Ok(dec) => println("{}", dec.len()),
                Err(e) => println("decompress err: {}", e),
            }
        }
        Err(e) => println("compress err: {}", e),
    }
}
"#,
        "5",
    );
}

// -----------------------------------------------------------------------
// std::hash::fnv

#[test]
fn hash_fnv_hash_string() {
    assert_vm_output(
        "hash_fnv",
        r#"
use std::hash
fn main() {
    let h1 = hash::fnv::hash_string("hello")
    let h2 = hash::fnv::hash_string("hello")
    println("{}", h1 == h2)
    let h3 = hash::fnv::hash_string("world")
    println("{}", h1 == h3)
}
"#,
        "true\nfalse",
    );
}

#[test]
fn hash_fnv_hash64_bytes() {
    assert_vm_output(
        "hash_fnv64",
        r#"
use std::hash
fn main() {
    let h = hash::fnv::hash64(Vec::from([]))
    println("{}", h != 0)
}
"#,
        "true",
    );
}

// -----------------------------------------------------------------------
// std::archive

#[test]
fn archive_zip_roundtrip() {
    assert_vm_output(
        "archive_zip",
        r#"
use std::archive
fn main() {
    let files = Vec::from([("hello.txt", Vec::from([104, 101, 108, 108, 111]))])
    match archive::zip::write(files) {
        Ok(zip_bytes) => {
            match archive::zip::read(zip_bytes) {
                Ok(entries) => {
                    println("{}", entries.len())
                    println("{}", entries[0].data.len())
                }
                Err(e) => println("read err: {}", e),
            }
        }
        Err(e) => println("write err: {}", e),
    }
}
"#,
        "1\n5",
    );
}

#[test]
fn archive_tar_roundtrip() {
    assert_vm_output(
        "archive_tar",
        r#"
use std::archive
fn main() {
    let files = Vec::from([("hello.txt", Vec::from([104, 101, 108, 108, 111]))])
    match archive::tar::write(files) {
        Ok(tar_bytes) => {
            match archive::tar::read(tar_bytes) {
                Ok(entries) => {
                    println("{}", entries.len())
                    println("{}", entries[0].data.len())
                }
                Err(e) => println("read err: {}", e),
            }
        }
        Err(e) => println("write err: {}", e),
    }
}
"#,
        "1\n5",
    );
}

// -----------------------------------------------------------------------
// sync::AtomicU64 and sync::Barrier

#[test]
fn sync_atomic_u64_basic() {
    assert_vm_output(
        "sync_atomic_u64",
        r#"
use std::sync
fn main() {
    let a = sync::AtomicU64::new(0)
    sync::AtomicU64::store(a, 42)
    let v = sync::AtomicU64::load(a)
    println("{}", v)
    let prev = sync::AtomicU64::fetch_add(a, 8)
    println("{}", prev)
    println("{}", sync::AtomicU64::load(a))
}
"#,
        "42\n42\n50",
    );
}

#[test]
fn sync_barrier_releases_all() {
    assert_vm_output(
        "sync_barrier",
        r#"
use std::sync
fn main() {
    let b = sync::Barrier::new(1)
    sync::Barrier::wait(b)
    println("done")
}
"#,
        "done",
    );
}

// -----------------------------------------------------------------------
// P1: crypto breadth

#[test]
fn crypto_sha512_hex() {
    assert_vm_output(
        "crypto_sha512_hex",
        r#"
use std::crypto
fn main() {
    let h = crypto::sha512::hex("abc".as_bytes())
    println("{}", h.len())
    println("{}", h.starts_with("dd"))
}
"#,
        "128\ntrue",
    );
}

#[test]
fn crypto_blake3_hex() {
    assert_vm_output(
        "crypto_blake3_hex",
        r#"
use std::crypto
fn main() {
    let h = crypto::blake3::hex("".as_bytes())
    println("{}", h.len())
    println("{}", h.starts_with("af"))
}
"#,
        "64\ntrue",
    );
}

#[test]
fn crypto_aes_gcm_roundtrip() {
    assert_vm_output(
        "crypto_aes_gcm",
        r#"
use std::crypto
fn main() {
    match crypto::rand::bytes(32) {
        Ok(key) => {
            match crypto::rand::bytes(12) {
                Ok(nonce) => {
                    let pt: Vec<u8> = Vec::from([104, 101, 108, 108, 111, 32, 97, 101, 115])
                    match crypto::aead::aes_256_gcm_seal(key, nonce, pt, Vec::from([])) {
                        Ok(ct) => {
                            match crypto::aead::aes_256_gcm_open(key, nonce, ct, Vec::from([])) {
                                Ok(dec) => println("{}", dec.len()),
                                Err(e) => println("open err: {}", e)
                            }
                        }
                        Err(e) => println("seal err: {}", e)
                    }
                }
                Err(e) => println("nonce err: {}", e)
            }
        }
        Err(e) => println("key err: {}", e)
    }
}
"#,
        "9",
    );
}

#[test]
fn crypto_ed25519_roundtrip() {
    assert_vm_output(
        "crypto_ed25519",
        r#"
use std::crypto
fn main() {
    match crypto::ed25519::keypair() {
        Ok(pair) => {
            let secret = pair.0
            let public = pair.1
            let msg: Vec<u8> = Vec::from([116, 101, 115, 116, 32, 109, 101, 115, 115, 97, 103, 101])
            match crypto::ed25519::sign(secret, msg) {
                Ok(sig) => {
                    match crypto::ed25519::verify(public, msg, sig) {
                        Ok(_) => println("verified"),
                        Err(e) => println("verify err: {}", e)
                    }
                }
                Err(e) => println("sign err: {}", e)
            }
        }
        Err(e) => println("keypair err: {}", e)
    }
}
"#,
        "verified",
    );
}

#[test]
fn crypto_kdf_pbkdf2() {
    assert_vm_output(
        "crypto_pbkdf2",
        r#"
use std::crypto
fn main() {
    let key = crypto::kdf::pbkdf2_sha256(
        Vec::from([112, 97, 115, 115, 119, 111, 114, 100]),
        Vec::from([115, 97, 108, 116]),
        1,
        32,
    )
    println("{}", key.len())
}
"#,
        "32",
    );
}

#[test]
fn crypto_x509_crl_verifier_rejects_missing_crl_on_vm_and_native() {
    assert_vm_output(
        "crypto_x509_crl_verifier",
        r#"
use std::crypto
fn main() {
    match crypto::x509::verify_server_certificate_with_crls("", "", "localhost", "") {
        Ok(_) => println("accepted"),
        Err(_) => println("rejected"),
    }
}
"#,
        "rejected",
    );
}

struct X509CrlFixtures {
    roots: String,
    valid_chain: String,
    revoked_chain: String,
    current_crls: String,
    expired_crls: String,
    root_only_crl: String,
}

/// Generates a private PKI that deliberately covers the public verifier's
/// contract: a root, intermediate, valid server leaf, revoked server leaf,
/// current CRLs for both issuers, and an expired intermediate CRL.
fn generated_x509_crl_fixtures() -> X509CrlFixtures {
    use rcgen::{
        BasicConstraints, CertificateParams, CertificateRevocationListParams, DnType,
        ExtendedKeyUsagePurpose, IsCa, Issuer, KeyIdMethod, KeyPair, KeyUsagePurpose,
        RevocationReason, RevokedCertParams, SerialNumber, date_time_ymd,
    };

    let mut root_params = CertificateParams::new(vec!["crypto-root.invalid".to_owned()]).unwrap();
    root_params
        .distinguished_name
        .push(DnType::CommonName, "crypto test root");
    root_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    root_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let root_key = KeyPair::generate().unwrap();
    let root_cert = root_params.self_signed(&root_key).unwrap();
    let root = Issuer::new(root_params, root_key);

    let mut intermediate_params =
        CertificateParams::new(vec!["crypto-intermediate.invalid".to_owned()]).unwrap();
    intermediate_params
        .distinguished_name
        .push(DnType::CommonName, "crypto test intermediate");
    intermediate_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    intermediate_params.use_authority_key_identifier_extension = true;
    intermediate_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let intermediate_key = KeyPair::generate().unwrap();
    let intermediate_cert = intermediate_params
        .signed_by(&intermediate_key, &root)
        .unwrap();
    let intermediate = Issuer::new(intermediate_params, intermediate_key);

    let mut valid_params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    valid_params.serial_number = Some(SerialNumber::from(100_u64));
    valid_params.use_authority_key_identifier_extension = true;
    valid_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let valid_key = KeyPair::generate().unwrap();
    let valid_leaf = valid_params.signed_by(&valid_key, &intermediate).unwrap();

    let mut revoked_params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    revoked_params.serial_number = Some(SerialNumber::from(200_u64));
    revoked_params.use_authority_key_identifier_extension = true;
    revoked_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let revoked_key = KeyPair::generate().unwrap();
    let revoked_leaf = revoked_params
        .signed_by(&revoked_key, &intermediate)
        .unwrap();

    let root_crl = CertificateRevocationListParams {
        this_update: date_time_ymd(2025, 1, 1),
        next_update: date_time_ymd(2030, 1, 1),
        crl_number: SerialNumber::from(1_u64),
        issuing_distribution_point: None,
        revoked_certs: Vec::new(),
        key_identifier_method: KeyIdMethod::Sha256,
    }
    .signed_by(&root)
    .unwrap();
    let current_intermediate_crl = CertificateRevocationListParams {
        this_update: date_time_ymd(2025, 1, 1),
        next_update: date_time_ymd(2030, 1, 1),
        crl_number: SerialNumber::from(2_u64),
        issuing_distribution_point: None,
        revoked_certs: vec![RevokedCertParams {
            serial_number: SerialNumber::from(200_u64),
            revocation_time: date_time_ymd(2025, 1, 1),
            reason_code: Some(RevocationReason::KeyCompromise),
            invalidity_date: None,
        }],
        key_identifier_method: KeyIdMethod::Sha256,
    }
    .signed_by(&intermediate)
    .unwrap();
    let expired_intermediate_crl = CertificateRevocationListParams {
        this_update: date_time_ymd(2020, 1, 1),
        next_update: date_time_ymd(2021, 1, 1),
        crl_number: SerialNumber::from(3_u64),
        issuing_distribution_point: None,
        revoked_certs: Vec::new(),
        key_identifier_method: KeyIdMethod::Sha256,
    }
    .signed_by(&intermediate)
    .unwrap();

    X509CrlFixtures {
        roots: root_cert.pem(),
        valid_chain: format!("{}{}", valid_leaf.pem(), intermediate_cert.pem()),
        revoked_chain: format!("{}{}", revoked_leaf.pem(), intermediate_cert.pem()),
        current_crls: format!(
            "{}{}",
            root_crl.pem().unwrap(),
            current_intermediate_crl.pem().unwrap()
        ),
        expired_crls: format!(
            "{}{}",
            root_crl.pem().unwrap(),
            expired_intermediate_crl.pem().unwrap()
        ),
        root_only_crl: root_crl.pem().unwrap(),
    }
}

#[test]
fn crypto_x509_crl_contract_matches_vm_forced_jit_and_llvm() {
    let fixtures = generated_x509_crl_fixtures();
    let host_good = gossamer_std::crypto::x509::verify_server_certificate_with_crls(
        fixtures.valid_chain.as_bytes(),
        fixtures.roots.as_bytes(),
        "localhost",
        fixtures.current_crls.as_bytes(),
    );
    assert!(host_good.is_ok(), "generated valid chain: {host_good:?}");
    let source = format!(
        r#"
use std::crypto
fn accepted(chain: String, roots: String, host: String, crls: String) -> bool {{
    match crypto::x509::verify_server_certificate_with_crls(chain, roots, host, crls) {{
        Ok(_) => true,
        Err(_) => false,
    }}
}}
fn main() {{
    let good = accepted({valid_chain:?}, {roots:?}, "localhost", {current_crls:?})
    let revoked = accepted({revoked_chain:?}, {roots:?}, "localhost", {current_crls:?})
    let wrong_host = accepted({valid_chain:?}, {roots:?}, "wrong.invalid", {current_crls:?})
    let expired_crl = accepted({valid_chain:?}, {roots:?}, "localhost", {expired_crls:?})
    let unknown_status = accepted({valid_chain:?}, {roots:?}, "localhost", {root_only_crl:?})
    let malformed = accepted("not pem", {roots:?}, "localhost", {current_crls:?})
    println("{{}} {{}} {{}} {{}} {{}} {{}}", good, revoked, wrong_host, expired_crl, unknown_status, malformed)
}}
"#,
        valid_chain = fixtures.valid_chain,
        revoked_chain = fixtures.revoked_chain,
        roots = fixtures.roots,
        current_crls = fixtures.current_crls,
        expired_crls = fixtures.expired_crls,
        root_only_crl = fixtures.root_only_crl,
    );
    let expected = "true false false false false false";
    assert_vm_output("crypto_x509_crl_contract", &source, expected);

    let dir = scratch("crypto_x509_crl_contract_jit");
    let path = dir.join("main.gos");
    fs::File::create(&path)
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    let output = Command::new(gos_bin())
        .arg("run")
        .arg(&path)
        .env("GOSSAMER_JIT_THRESHOLD", "1")
        .env("GOSSAMER_JIT_MIN_WORK", "1")
        .output()
        .expect("run forced JIT X.509 fixture");
    assert!(
        output.status.success(),
        "forced JIT X.509 fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
}

// -----------------------------------------------------------------------
// P1: hash crc32 and adler32

#[test]
fn hash_crc32_known_value() {
    assert_vm_output(
        "hash_crc32",
        r#"
use std::hash
fn main() {
    // 0x0D4A_1185 = 222957957
    let c = hash::crc32::checksum_string("hello world")
    println("{}", c == 222957957)
}
"#,
        "true",
    );
}

#[test]
fn hash_adler32_known_value() {
    assert_vm_output(
        "hash_adler32",
        r#"
use std::hash
fn main() {
    let c = hash::adler32::checksum_string("Wikipedia")
    println("{}", c == 300286872)
}
"#,
        "true",
    );
}

// -----------------------------------------------------------------------
// P1: json builtins

#[test]
fn json_parse_and_encode() {
    assert_vm_output(
        "json_parse_encode",
        r#"
use std::encoding
fn main() {
    let src = "{\"x\":1,\"y\":2}"
    match encoding::json::parse(src) {
        Ok(v) => {
            let out = encoding::json::encode(v)
            println("{}", encoding::json::valid(out))
        }
        Err(e) => println("err: {}", e)
    }
}
"#,
        "true",
    );
}

#[test]
fn json_valid_rejects_bad() {
    assert_vm_output(
        "json_valid_bad",
        r#"
use std::encoding
fn main() {
    println("{}", encoding::json::valid("not json"))
    println("{}", encoding::json::valid("42"))
}
"#,
        "false\ntrue",
    );
}

// -----------------------------------------------------------------------
// P1: time completeness

#[test]
fn time_now_returns_positive() {
    assert_vm_output(
        "time_now",
        r#"
use std::time
fn main() {
    let ms = time::now()
    println("{}", ms > 0)
}
"#,
        "true",
    );
}

#[test]
fn time_format_parse_rfc3339() {
    assert_vm_output(
        "time_rfc3339",
        r#"
use std::time
fn main() {
    let ms = 0
    match time::format_rfc3339(ms) {
        Ok(s) => {
            println("{}", s.starts_with("1970"))
            match time::parse_rfc3339(s) {
                Ok(back) => println("{}", back == 0),
                Err(e) => println("parse err: {}", e)
            }
        }
        Err(e) => println("format err: {}", e)
    }
}
"#,
        "true\ntrue",
    );
}

// -----------------------------------------------------------------------
// P1: net::ip

#[test]
fn net_ip_parse_and_check() {
    assert_vm_output(
        "net_ip_parse",
        r#"
use std::net
fn main() {
    println("{}", net::ip::is_valid("192.168.1.1"))
    println("{}", net::ip::is_valid("not-an-ip"))
    println("{}", net::ip::is_v4("10.0.0.1"))
    println("{}", net::ip::is_v6("::1"))
    match net::ip::parse("127.0.0.1") {
        Ok(ip) => println("{}", net::ip::is_loopback(ip)),
        Err(e) => println("err: {}", e)
    }
}
"#,
        "true\nfalse\ntrue\ntrue\ntrue",
    );
}

// -----------------------------------------------------------------------
// P1: thread builtins

#[test]
fn thread_num_cpus_positive() {
    assert_vm_output(
        "thread_num_cpus",
        r#"
use std::thread
fn main() {
    let n = thread::num_cpus()
    println("{}", n > 0)
}
"#,
        "true",
    );
}

// -----------------------------------------------------------------------
// P1: html escape / unescape

#[test]
fn html_escape_unescape() {
    assert_vm_output(
        "html_escape",
        r#"
use std::html
fn main() {
    let escaped = html::escape("<b>Hello & 'World'</b>")
    println("{}", escaped)
    let back = html::unescape(escaped)
    println("{}", back)
}
"#,
        "&lt;b&gt;Hello &amp; &#39;World&#39;&lt;&#x2F;b&gt;\n<b>Hello & 'World'</b>",
    );
}

// -----------------------------------------------------------------------
// P2: encoding::base32

#[test]
fn encoding_base32_roundtrip() {
    assert_vm_output(
        "base32_roundtrip",
        r#"
use std::encoding
fn main() {
    let enc = encoding::base32::encode_string("foobar")
    println("{}", enc)
    let dec = encoding::base32::decode_string(enc)
    match dec {
        Ok(s) => println("{}", s),
        Err(e) => println("err: {}", e),
    }
}
"#,
        "MZXW6YTBOI======\nfoobar",
    );
}

// -----------------------------------------------------------------------
// P2: encoding::ascii85

#[test]
fn encoding_ascii85_roundtrip() {
    assert_vm_output(
        "ascii85_roundtrip",
        r#"
use std::encoding
fn main() {
    let enc = encoding::ascii85::encode(Vec::from([104, 101, 108, 108, 111]))
    let dec = encoding::ascii85::decode(enc)
    match dec {
        Ok(bytes) => println("{}", bytes.len()),
        Err(e) => println("err: {}", e),
    }
}
"#,
        "5",
    );
}

// -----------------------------------------------------------------------
// P2: encoding::xml

#[test]
fn encoding_xml_escape() {
    assert_vm_output(
        "xml_escape",
        r#"
use std::encoding
fn main() {
    let s = encoding::xml::escape("<hello & world>")
    println("{}", s)
}
"#,
        "&lt;hello &amp; world&gt;",
    );
}

#[test]
fn encoding_xml_parse_and_roundtrip() {
    assert_vm_output(
        "xml_parse",
        r#"
use std::encoding
use std::strings
fn main() {
    let src = "<root><child>text</child></root>"
    let result = encoding::xml::parse(src)
    match result {
        Ok(node) => {
            let re_encoded = encoding::xml::encode(node)
            let has_root = strings::contains(re_encoded, "root")
            println("{}", has_root)
        }
        Err(e) => println("err: {}", e),
    }
}
"#,
        "true",
    );
}

// -----------------------------------------------------------------------
// P2: crypto::insecure

#[test]
fn crypto_insecure_md5_hex() {
    assert_vm_output(
        "insecure_md5",
        r#"
use std::crypto
fn main() {
    let h = crypto::insecure::md5_hex("".as_bytes())
    println("{}", h)
}
"#,
        "d41d8cd98f00b204e9800998ecf8427e",
    );
}

#[test]
fn crypto_insecure_sha1_hex() {
    assert_vm_output(
        "insecure_sha1",
        r#"
use std::crypto
fn main() {
    let h = crypto::insecure::sha1_hex("abc".as_bytes())
    println("{}", h)
}
"#,
        "a9993e364706816aba3e25717850c26c9cd0d89d",
    );
}

// -----------------------------------------------------------------------
// P2: compress::bzip2

#[test]
fn compress_bzip2_roundtrip() {
    assert_vm_output(
        "bzip2_roundtrip",
        r#"
use std::compress
fn main() {
    let data: Vec<u8> = Vec::from([104, 101, 108, 108, 111, 44, 32, 103, 111, 115, 115, 97, 109, 101, 114, 32, 108, 97, 110, 103, 33])
    let enc = compress::bzip2::compress(data, 6)
    match enc {
        Ok(compressed) => {
            let dec = compress::bzip2::decompress(compressed)
            match dec {
                Ok(bytes) => println("{}", bytes.len()),
                Err(e) => println("dec err: {}", e),
            }
        }
        Err(e) => println("enc err: {}", e),
    }
}
"#,
        "21",
    );
}

// -----------------------------------------------------------------------
// P2: math::big

#[test]
fn math_big_int_arithmetic() {
    assert_vm_output(
        "big_int_arith",
        r#"
use std::math
fn main() {
    let a = math::big::int_from_i64(1000000000)
    let b = math::big::int_from_i64(1000000000)
    let c = math::big::int_mul(a, b)
    println("{}", c)
    let d = math::big::int_add(c, math::big::int_from_i64(1))
    println("{}", d)
}
"#,
        "1000000000000000000\n1000000000000000001",
    );
}

#[test]
fn math_big_factorial() {
    assert_vm_output(
        "big_factorial",
        r#"
use std::math
fn main() {
    let f = math::big::factorial(20)
    println("{}", f)
}
"#,
        "2432902008176640000",
    );
}

#[test]
fn math_big_uint_pow_mod() {
    assert_vm_output(
        "big_uint_pow_mod",
        r#"
use std::math
fn main() {
    let base = math::big::uint_from_u64(2)
    let exp = math::big::uint_from_u64(10)
    let modulus = math::big::uint_from_u64(1000)
    let result = math::big::uint_pow_mod(base, exp, modulus)
    println("{}", result)
}
"#,
        "24",
    );
}
