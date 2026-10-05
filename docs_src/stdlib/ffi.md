# `std::ffi`

Status: experimental

C type names and C strings for functions declared in `unsafe extern "C"` blocks, and the `errno` a foreign call left.

## Items

| Item | Signature | Description |
|---|---|---|
| `c_char` | `type c_char` | C `char`: `i8`, or `u8` on Linux aarch64 and riscv64. |
| `c_schar` | `type c_schar` | C `signed char`: `i8`. |
| `c_uchar` | `type c_uchar` | C `unsigned char`: `u8`. |
| `c_short` | `type c_short` | C `short`: `i16`. |
| `c_ushort` | `type c_ushort` | C `unsigned short`: `u16`. |
| `c_int` | `type c_int` | C `int`: `i32`. |
| `c_uint` | `type c_uint` | C `unsigned int`: `u32`. |
| `c_long` | `type c_long` | C `long`: `i64`, or `i32` on Windows. |
| `c_ulong` | `type c_ulong` | C `unsigned long`: `u64`, or `u32` on Windows. |
| `c_longlong` | `type c_longlong` | C `long long`: `i64`. |
| `c_ulonglong` | `type c_ulonglong` | C `unsigned long long`: `u64`. |
| `size_t` | `type size_t` | C `size_t`: `usize`. |
| `ssize_t` | `type ssize_t` | C `ssize_t`: `isize`. |
| `c_float` | `type c_float` | C `float`: `f32`. |
| `c_double` | `type c_double` | C `double`: `f64`. |
| `cstring` | `fn cstring(text: String) -> Result<Vec<u8>, errors::Error>` | `cstring(text) -> Result<Vec<u8>, errors::Error>`: the bytes of `text` with a terminating NUL, to pass as `[u8]`; an error when `text` holds a NUL. |
| `from_cstr` | `fn from_cstr(bytes: [u8]) -> Result<String, errors::Error>` | `from_cstr(bytes) -> Result<String, errors::Error>`: the UTF-8 text before the first NUL in `bytes`. |
| `last_errno` | `fn last_errno() -> i64` | `last_errno() -> i64`: the C `errno` the goroutine's most recent foreign call left, cleared just before the call and captured before anything else could change it, so a call that does not set it answers 0. |
| `last_os_error` | `fn last_os_error() -> i64` | `last_os_error() -> i64`: the operating-system error code the goroutine's most recent foreign call left: `GetLastError` on Windows, `errno` elsewhere. Cleared just before the call, so a call that does not set it answers 0. |
| `c_void` | `type c_void` | C `void`, as the pointee of `Ptr<c_void>` (`void *`): an opaque type with no value of its own. |
| `Ptr` | `type Ptr` | `Ptr<T>`: a non-null address of a `T` in memory Gossamer does not manage, as a foreign function takes or answers it; `Option<Ptr<T>>` is the nullable form. Holding, copying, comparing, storing, and sending one is safe; every access through it is `unsafe`. `p.cast::<U>()`, `p.address()`, and `unsafe { Ptr::from_address(n) }` (`None` for 0). |
| `Handle` | `type Handle` | `Handle<T>`: a value handed to native code as `void *` context and read back in a callback. `Handle::new(value)`, `h.as_ptr()`, `unsafe { Handle::<T>::from_ptr(p) }`, `h.get()`, `h.set(value)`, `h.update(|v| ..)`, `h.take()`, `h.release()`; a released or unknown handle is GX0014, never a read of freed memory. |
| `read` | `fn read<T>(p: ffi::Ptr<T>) -> T` | `unsafe { read::<T>(p) }`: a copy of the `T` at `p`, for a scalar, a `Ptr`, or a `#[repr(C)]` plain-data struct. |
| `read_at` | `fn read_at<T>(p: ffi::Ptr<T>, index: i64) -> T` | `unsafe { read_at::<T>(p, i) }`: a copy of element `i` of the array of `T` at `p`. |
| `write` | `fn write<T>(p: ffi::Ptr<T>, value: T)` | `unsafe { write(p, value) }`: stores `value` at `p`. |
| `write_at` | `fn write_at<T>(p: ffi::Ptr<T>, index: i64, value: T)` | `unsafe { write_at(p, i, value) }`: stores `value` as element `i` of the array of `T` at `p`. |
| `alloc` | `fn alloc<T>(count: i64) -> ffi::Ptr<T>` | `unsafe { alloc::<T>(count) }`: zeroed room for `count` values of `T` from the platform C allocator, freed with `free` or by a library that takes it over. |
| `size_of` | `fn size_of<T>() -> i64` | `size_of::<T>() -> i64`: the bytes a `T` occupies in C layout. |
| `free` | `fn free<T>(p: ffi::Ptr<T>)` | `unsafe { free(p) }`: frees memory the platform C allocator handed out (`alloc`, `to_c_bytes`, or a library's `malloc`). |
| `read_bytes` | `fn read_bytes(p: ffi::Ptr<u8>, len: i64) -> Vec<u8>` | `unsafe { read_bytes(p, len) }`: a `Vec<u8>` copy of the `len` bytes at `p`. |
| `read_cstr` | `fn read_cstr(p: ffi::Ptr<u8>) -> Result<String, errors::Error>` | `unsafe { read_cstr(p) } -> Result<String, errors::Error>`: the NUL-terminated text at `p`, copied into a `String`; an error when it is not UTF-8. |
| `write_bytes` | `fn write_bytes(p: ffi::Ptr<u8>, bytes: [u8])` | `unsafe { write_bytes(p, bytes) }`: copies `bytes` to the memory at `p`. |
| `to_c_bytes` | `fn to_c_bytes(bytes: [u8]) -> ffi::Ptr<u8>` | `unsafe { to_c_bytes(bytes) }`: a C-allocated copy of `bytes`, for native code to keep; free it or hand it to a library that takes it over. |
| `fn_from_ptr` | `fn fn_from_ptr<F>(p: ffi::Ptr<ffi::c_void>) -> F` | `unsafe { fn_from_ptr::<Fn(A..) -> R>(p) }`: a callable that calls the native function at `p` (a `dlsym` or `GetProcAddress` result) with that C signature. |
