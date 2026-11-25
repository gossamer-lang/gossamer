//! Emits `cfg(asan)` under `AddressSanitizer`, read from the rustflags the
//! sanitizer job sets, since `cfg(sanitize)` is nightly-only.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(asan)");
    println!("cargo:rerun-if-env-changed=RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    let flags = std::env::var("CARGO_ENCODED_RUSTFLAGS")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("RUSTFLAGS").ok())
        .unwrap_or_default();
    if flags.contains("sanitizer=address") {
        println!("cargo:rustc-cfg=asan");
    }
}
