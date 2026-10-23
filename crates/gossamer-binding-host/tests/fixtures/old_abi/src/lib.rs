//! A library that claims binding ABI 2.0, which the host refuses before it
//! calls anything else.

/// The ABI version this library claims.
#[unsafe(no_mangle)]
pub extern "C" fn gos_binding_abi_version() -> u32 {
    2 << 16
}

/// Never called: the version check refuses the library first.
#[unsafe(no_mangle)]
pub extern "C" fn gos_binding_init(_host: *const u8) -> *const u8 {
    std::process::abort()
}
